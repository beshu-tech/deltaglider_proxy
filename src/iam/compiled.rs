// SPDX-License-Identifier: BUSL-1.1

//! A principal's IAM policies, compiled once when the principal is built.
//!
//! iam-rs rebuilds every statement's matchers on every evaluation: it clones
//! the policies into a `PolicyEvaluator`, interpolates each resource pattern
//! and parses it into an `ArnMatcher`, and matches actions by wildcard. One
//! authorization cost 20-45 µs, and a filtered LIST runs one per listed key.
//!
//! [`CompiledPolicy`] does that work at build time and keeps iam-rs as the
//! one authority for everything it cannot decide statically:
//!
//! - **Actions**: the request action is one of five fixed strings, so the
//!   build asks iam-rs itself which of the five each statement's `Action`
//!   matches and keeps a bit mask.
//! - **Resources**: the request resource is always `arn:aws:s3:::<bucket>[/<key>]`,
//!   so the partition, service, region and account of every pattern are
//!   decided at build time with iam-rs's own `Arn::wildcard_match`. What is
//!   left is the resource part: equality, a literal prefix (`x*`), or iam-rs's
//!   wildcard match for any other glob. A pattern that iam-rs cannot parse
//!   keeps its error, at the same place in the statement order.
//! - **Conditions** and anything unusual (a policy variable in a resource,
//!   `NotAction`, `NotResource`, a principal): the statement is evaluated by
//!   iam-rs, alone, against a request that is built once per evaluation and
//!   only when such a statement is reached.
//!
//! The statements are walked in iam-rs's order with iam-rs's rules: the
//! first explicit Deny wins at once, an error ends the evaluation, and the
//! statement limit errs at the same statement. The differential proptest
//! (`compiled_policy_matches_iam_rs`) holds this evaluator equal to iam-rs.

use iam_rs::{
    Arn, ArnMatcher, Context, Decision, EvaluationError, EvaluationOptions, IAMAction, IAMEffect,
    IAMPolicy, IAMRequest, IAMResource, IAMStatement, PolicyEvaluator, Principal, PrincipalId,
};

use super::types::S3Action;

/// The five request actions, in bit order.
const ACTIONS: [S3Action; 5] = [
    S3Action::Read,
    S3Action::Write,
    S3Action::Delete,
    S3Action::List,
    S3Action::Admin,
];

fn action_bit(action: S3Action) -> u8 {
    match action {
        S3Action::Read => 1,
        S3Action::Write => 2,
        S3Action::Delete => 4,
        S3Action::List => 8,
        S3Action::Admin => 16,
    }
}

/// The principal of every request the proxy evaluates.
fn request_principal() -> Principal {
    Principal::Aws(PrincipalId::String("000000000000".into()))
}

/// The iam-rs request for `action` on the resource `resource` (the ARN text
/// after `arn:aws:s3:::`).
pub(crate) fn iam_request(action: S3Action, resource: &str, context: &Context) -> IAMRequest {
    let arn = Arn {
        partition: "aws".into(),
        service: "s3".into(),
        region: String::new(),
        account_id: String::new(),
        resource: resource.to_string(),
    };
    IAMRequest::new_with_context(
        request_principal(),
        action.to_iam_action(),
        arn,
        context.clone(),
    )
}

/// One pattern of a statement's `Resource`, decided as far as the build can.
#[derive(Debug)]
enum ResourcePattern {
    /// `*`.
    Any,
    /// The partition, service, region or account never matches an S3
    /// request ARN.
    Never,
    /// The resource part, without wildcards.
    Exact(String),
    /// The resource part is `<literal>*`.
    Prefix(String),
    /// Any other glob of `*` and `?` in the resource part.
    Glob(String),
    /// iam-rs cannot parse the pattern: the evaluation errs when it gets here.
    Invalid(String),
}

impl ResourcePattern {
    /// `None`: the pattern holds a policy variable, which depends on the
    /// request context.
    fn compile(pattern: &str) -> Option<Self> {
        if pattern == "*" {
            return Some(Self::Any);
        }
        if pattern.contains("${") {
            return None;
        }
        // iam-rs: `interpolate_variables` (a no-op without `${`), then
        // `ArnMatcher::from_pattern`, whose error ends the evaluation.
        if let Err(e) = ArnMatcher::from_pattern(pattern) {
            return Some(Self::Invalid(e.to_string()));
        }
        let Ok(arn) = Arn::parse(pattern) else {
            return Some(Self::Invalid(format!("unparsable pattern {pattern:?}")));
        };
        let wild = |s: &str| s.contains('*') || s.contains('?');
        // The request ARN's fixed components: partition `aws`, service `s3`,
        // empty region and account.
        let fixed_match = if !wild(pattern) {
            arn.partition == "aws"
                && arn.service == "s3"
                && arn.region.is_empty()
                && arn.account_id.is_empty()
        } else {
            let component = |target: &str, pat: &str| {
                if wild(pat) {
                    Arn::wildcard_match(target, pat)
                } else {
                    target == pat
                }
            };
            !wild(&arn.service)
                && component("aws", &arn.partition)
                && arn.service == "s3"
                && component("", &arn.region)
                && component("", &arn.account_id)
        };
        if !fixed_match {
            return Some(Self::Never);
        }
        let resource = arn.resource;
        Some(if !wild(&resource) {
            Self::Exact(resource)
        } else {
            match resource.strip_suffix('*') {
                Some(head) if !wild(head) => Self::Prefix(head.to_string()),
                _ => Self::Glob(resource),
            }
        })
    }

    /// iam-rs's answer for the request resource `resource`.
    fn matches(&self, resource: &str) -> Result<bool, EvaluationError> {
        Ok(match self {
            Self::Any => true,
            Self::Never => false,
            Self::Exact(s) => resource == s,
            Self::Prefix(head) => resource.starts_with(head.as_str()),
            Self::Glob(glob) => Arn::wildcard_match(resource, glob),
            Self::Invalid(e) => return Err(EvaluationError::InvalidArn(e.clone())),
        })
    }
}

#[derive(Debug)]
enum StatementKind {
    /// Action and resource decided here; the condition, if any, by iam-rs.
    Compiled {
        actions: u8,
        resources: Vec<ResourcePattern>,
        condition: Option<PolicyEvaluator>,
    },
    /// The whole statement is evaluated by iam-rs.
    Delegated(PolicyEvaluator),
}

#[derive(Debug)]
struct CompiledStatement {
    effect: IAMEffect,
    kind: StatementKind,
}

/// iam-rs alone, on one statement.
fn single_statement_evaluator(statement: &IAMStatement) -> PolicyEvaluator {
    PolicyEvaluator::with_policies(vec![IAMPolicy::new().add_statement(statement.clone())])
}

/// The bit mask of the request actions that `action` matches, asked of
/// iam-rs: a statement with only this `Action` and `Resource: "*"`.
/// `None` if iam-rs errs (it does not: the statement has no condition).
fn action_mask(action: &IAMAction) -> Option<u8> {
    let probe = IAMStatement::new(IAMEffect::Allow)
        .with_action(action.clone())
        .with_resource(IAMResource::Single("*".into()));
    let evaluator = single_statement_evaluator(&probe);
    let mut mask = 0;
    for a in ACTIONS {
        let request = iam_request(a, "probe", &Context::new());
        match evaluator.evaluate(&request) {
            Ok(r) if r.decision == Decision::Allow => mask |= action_bit(a),
            Ok(_) => {}
            Err(_) => return None,
        }
    }
    Some(mask)
}

impl CompiledStatement {
    fn compile(statement: &IAMStatement) -> Self {
        let delegated = || Self {
            effect: statement.effect,
            kind: StatementKind::Delegated(single_statement_evaluator(statement)),
        };
        if statement.principal.is_some()
            || statement.not_principal.is_some()
            || statement.not_action.is_some()
            || statement.not_resource.is_some()
        {
            return delegated();
        }
        let (Some(action), Some(resource)) = (&statement.action, &statement.resource) else {
            return delegated();
        };
        let Some(actions) = action_mask(action) else {
            return delegated();
        };
        let patterns: Vec<&str> = match resource {
            IAMResource::Single(r) => vec![r.as_str()],
            IAMResource::Multiple(rs) => rs.iter().map(String::as_str).collect(),
        };
        let Some(resources) = patterns
            .into_iter()
            .map(ResourcePattern::compile)
            .collect::<Option<Vec<_>>>()
        else {
            return delegated();
        };
        Self {
            effect: statement.effect,
            kind: StatementKind::Compiled {
                actions,
                resources,
                condition: statement
                    .condition
                    .as_ref()
                    .map(|_| single_statement_evaluator(statement)),
            },
        }
    }
}

/// A principal's policies, ready to evaluate. See the module docs.
#[derive(Debug)]
pub(crate) struct CompiledPolicy {
    statements: Vec<CompiledStatement>,
    /// iam-rs errs at the statement past this many.
    max_statements: usize,
}

impl CompiledPolicy {
    pub(crate) fn new(policies: &[IAMPolicy]) -> Self {
        Self {
            statements: policies
                .iter()
                .flat_map(|p| p.statement.iter())
                .map(CompiledStatement::compile)
                .collect(),
            max_statements: EvaluationOptions::default().max_statements,
        }
    }

    /// iam-rs's decision for `action` on `resource` (the ARN text after
    /// `arn:aws:s3:::`), with iam-rs's errors.
    pub(crate) fn evaluate(
        &self,
        action: S3Action,
        resource: &str,
        context: &Context,
    ) -> Result<Decision, EvaluationError> {
        // iam-rs refuses a request ARN without a resource before any statement.
        if resource.is_empty() {
            return Err(EvaluationError::InvalidRequest(
                "Invalid resource ARN".to_string(),
            ));
        }
        let bit = action_bit(action);
        let mut request: Option<IAMRequest> = None;
        let mut allowed = false;
        for (i, statement) in self.statements.iter().enumerate() {
            if i >= self.max_statements {
                return Err(EvaluationError::InternalError(
                    "Maximum statement evaluation limit exceeded".to_string(),
                ));
            }
            let evaluator = match &statement.kind {
                StatementKind::Delegated(evaluator) => Some(evaluator),
                StatementKind::Compiled {
                    actions,
                    resources,
                    condition,
                } => {
                    if actions & bit == 0 {
                        continue;
                    }
                    let mut matched = false;
                    for pattern in resources {
                        if pattern.matches(resource)? {
                            matched = true;
                            break;
                        }
                    }
                    if !matched {
                        continue;
                    }
                    condition.as_ref()
                }
            };
            let decision = match evaluator {
                None => match statement.effect {
                    IAMEffect::Allow => Decision::Allow,
                    IAMEffect::Deny => Decision::Deny,
                },
                Some(evaluator) => {
                    let request =
                        request.get_or_insert_with(|| iam_request(action, resource, context));
                    evaluator.evaluate(request)?.decision
                }
            };
            match decision {
                Decision::Deny => return Ok(Decision::Deny),
                Decision::Allow => allowed = true,
                Decision::NotApplicable => {}
            }
        }
        Ok(if allowed {
            Decision::Allow
        } else {
            Decision::NotApplicable
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// iam-rs on the whole policy set: the reference. The request ARN is
    /// parsed from its text, as the evaluator did before compilation.
    fn reference(
        policies: &[IAMPolicy],
        action: S3Action,
        resource: &str,
        context: &Context,
    ) -> Result<Decision, EvaluationError> {
        let arn = Arn::parse(&format!("arn:aws:s3:::{resource}")).unwrap();
        let request = IAMRequest::new_with_context(
            request_principal(),
            action.to_iam_action(),
            arn,
            context.clone(),
        );
        PolicyEvaluator::with_policies(policies.to_vec())
            .evaluate(&request)
            .map(|r| r.decision)
    }

    /// Same decision, or both an error (the text of an error only reaches
    /// a log line).
    fn same(a: &Result<Decision, EvaluationError>, b: &Result<Decision, EvaluationError>) -> bool {
        match (a, b) {
            (Ok(x), Ok(y)) => x == y,
            (Err(_), Err(_)) => true,
            _ => false,
        }
    }

    const ACTION_PATTERNS: &[&str] = &[
        "s3:GetObject",
        "s3:PutObject",
        "s3:DeleteObject",
        "s3:ListBucket",
        "s3:CreateBucket",
        "s3:*",
        "*",
        "s3:Get*",
        "s3:*Object",
        "s3:?etObject",
        "S3:GETOBJECT",
        "iam:*",
        "*:*",
        "s3:",
        "",
        "s3:read",
    ];

    const RESOURCE_PATTERNS: &[&str] = &[
        "*",
        "arn:aws:s3:::*",
        "arn:aws:s3:::b",
        "arn:aws:s3:::b/",
        "arn:aws:s3:::b/*",
        "arn:aws:s3:::b*",
        "arn:aws:s3:::b/k",
        "arn:aws:s3:::b/a*",
        "arn:aws:s3:::b/*/x",
        "arn:aws:s3:::b/?",
        "arn:aws:s3:::b/k:x*",
        "arn:aws:s3:::é/*",
        "arn:aws:s3:::b/**",
        "arn:aws:s3:::?/*",
        "arn:*:s3:::b/*",
        "arn:aws*:s3:::b/k",
        "arn:aws:s*:::b/*",
        "arn:aws:s3:*:*:b/*",
        "arn:aws:s3:?:*:b/*",
        "arn:aws:s3:us-east-1::b/*",
        "arn:aws:s3::123456789012:b/*",
        "arn:aws:s3:::b/${aws:SourceIp}",
        "arn:aws:s3:::b/${s3:prefix, 'k'}*",
        "arn:aws:s3:::b/${unclosed",
        "arn:aws:s3:::b/$x*",
        "notanarn",
        "arn:aws:s3",
        "",
        "arn:aws:ec2:::b/*",
        "ARN:aws:s3:::b/*",
    ];

    fn conditions() -> Vec<serde_json::Value> {
        use serde_json::json;
        vec![
            json!({"IpAddress": {"aws:SourceIp": "10.0.0.0/8"}}),
            json!({"IpAddress": {"aws:SourceIp": ["10.0.0.0/8", "192.168.0.0/16"]}}),
            json!({"NotIpAddress": {"aws:SourceIp": "10.0.0.0/8"}}),
            json!({"StringLike": {"s3:prefix": ["a*", ""]}}),
            json!({"StringEquals": {"s3:prefix": ""}}),
            json!({"StringNotLike": {"s3:prefix": "a*"}}),
            json!({"NumericLessThan": {"s3:max-keys": "10"}}),
            json!({"IpAddress": {"s3:prefix": "10.0.0.0/8"}}),
            json!({"Bool": {"aws:SecureTransport": "true"}}),
            json!({}),
        ]
    }

    fn arb_condition() -> impl Strategy<Value = Option<iam_rs::ConditionBlock>> {
        prop_oneof![
            3 => Just(None),
            1 => (0..conditions().len()).prop_map(|i| {
                Some(serde_json::from_value(conditions()[i].clone()).unwrap())
            }),
        ]
    }

    fn arb_resource_pattern() -> impl Strategy<Value = String> {
        prop_oneof![
            4 => prop::sample::select(RESOURCE_PATTERNS).prop_map(str::to_string),
            1 => "[bk/*?:é]{0,6}".prop_map(|r| format!("arn:aws:s3:::{r}")),
        ]
    }

    fn arb_actions() -> impl Strategy<Value = IAMAction> {
        prop_oneof![
            prop::sample::select(ACTION_PATTERNS).prop_map(|a| IAMAction::Single(a.to_string())),
            prop::collection::vec(prop::sample::select(ACTION_PATTERNS), 0..4)
                .prop_map(|v| { IAMAction::Multiple(v.into_iter().map(str::to_string).collect()) }),
        ]
    }

    fn arb_resources() -> impl Strategy<Value = IAMResource> {
        prop_oneof![
            arb_resource_pattern().prop_map(IAMResource::Single),
            prop::collection::vec(arb_resource_pattern(), 0..4).prop_map(IAMResource::Multiple),
        ]
    }

    /// Shapes: 0 plain, 1 NotAction, 2 NotResource, 3 a Principal,
    /// 4 no Action, 5 no Resource.
    fn arb_statement() -> impl Strategy<Value = IAMStatement> {
        (
            any::<bool>(),
            arb_actions(),
            arb_resources(),
            arb_condition(),
            prop_oneof![10 => Just(0u8), 1 => 1u8..6],
        )
            .prop_map(|(deny, action, resource, condition, shape)| {
                let effect = if deny {
                    IAMEffect::Deny
                } else {
                    IAMEffect::Allow
                };
                let mut st = IAMStatement::new(effect);
                match shape {
                    1 => st.not_action = Some(action),
                    4 => {}
                    _ => st.action = Some(action),
                }
                match shape {
                    2 => st.not_resource = Some(resource),
                    5 => {}
                    _ => st.resource = Some(resource),
                }
                if shape == 3 {
                    st.principal = Some(Principal::Wildcard);
                }
                st.condition = condition;
                st
            })
    }

    /// A policy as the proxy builds it from a stored permission, or a raw
    /// one with any statement shape.
    fn arb_policy() -> impl Strategy<Value = IAMPolicy> {
        let verbs = prop::sample::subsequence(
            vec!["read", "write", "delete", "list", "admin", "*", "bogus"],
            0..4,
        );
        let resources = prop::collection::vec(
            prop_oneof![
                prop::sample::select(vec![
                    "*", "b/*", "b", "b/a*", "b/k", "bb/*", "é/*", "b/*/x", "b?/*", "*/k",
                ])
                .prop_map(str::to_string),
                "[bk/*?:]{0,5}",
            ],
            1..3,
        );
        let permission = (
            any::<bool>(),
            verbs,
            resources,
            prop::option::of(0..10usize),
        )
            .prop_map(|(deny, actions, resources, cond)| {
                super::super::permissions::permission_to_iam_policy(&super::super::Permission {
                    id: 0,
                    effect: if deny { "Deny" } else { "Allow" }.into(),
                    actions: actions.into_iter().map(str::to_string).collect(),
                    resources,
                    conditions: cond.map(|i| conditions()[i].clone()),
                })
            });
        let raw = prop::collection::vec(arb_statement(), 0..3).prop_map(|statements| {
            statements
                .into_iter()
                .fold(IAMPolicy::new(), |p, s| p.add_statement(s))
        });
        prop_oneof![permission, raw]
    }

    fn arb_action() -> impl Strategy<Value = S3Action> {
        prop::sample::select(ACTIONS.to_vec())
    }

    fn arb_bucket_key() -> impl Strategy<Value = (String, String)> {
        let bucket = prop_oneof![
            prop::sample::select(vec!["b", "bb", "é", "", "b?"]).prop_map(str::to_string),
            "[bk*?]{0,3}",
        ];
        let key = prop_oneof![
            prop::sample::select(vec![
                "", "k", "a/b", "x:y", "*", "?", "a*b", "dir/", "k:x", "a", "ab/x"
            ])
            .prop_map(str::to_string),
            "[bk/*?:aé]{0,6}",
        ];
        (bucket, key)
    }

    fn arb_context() -> impl Strategy<Value = Context> {
        (
            prop::option::of(prop::sample::select(vec!["10.1.2.3", "192.168.1.1", "::1"])),
            prop::option::of(prop::sample::select(vec!["", "a/", "b", "ab"])),
            prop::option::of(0.0f64..20.0),
        )
            .prop_map(|(ip, prefix, max_keys)| {
                let mut ctx = Context::new();
                super::super::permissions::insert_source_ip(
                    &mut ctx,
                    ip.map(|i| i.parse().unwrap()),
                );
                if let Some(p) = prefix {
                    ctx.insert("s3:prefix".into(), iam_rs::ContextValue::String(p.into()));
                }
                if let Some(n) = max_keys {
                    ctx.insert("s3:max-keys".into(), iam_rs::ContextValue::Number(n));
                }
                ctx
            })
    }

    proptest! {
        /// P7.2 MERGE BLOCKER: the compiled evaluator equals iam-rs, for
        /// random policies (the proxy's own shapes and raw iam-rs ones)
        /// and random requests. Also through the allow and explicit-deny
        /// rules that the request path uses (`evaluate_compiled`,
        /// `is_explicitly_denied_compiled`).
        #[test]
        fn compiled_policy_matches_iam_rs(
            policies in prop::collection::vec(arb_policy(), 0..5),
            action in arb_action(),
            (bucket, key) in arb_bucket_key(),
            context in arb_context(),
        ) {
            use super::super::permissions as p;
            let compiled = CompiledPolicy::new(&policies);
            for resource in [
                if key.is_empty() { bucket.clone() } else { format!("{bucket}/{key}") },
                format!("{bucket}/"),
            ] {
                let want = reference(&policies, action, &resource, &context);
                let got = compiled.evaluate(action, &resource, &context);
                prop_assert!(same(&want, &got), "{resource:?}: iam-rs {want:?}, compiled {got:?}");
            }
            prop_assert_eq!(
                p::evaluate_iam(&policies, action, &bucket, &key, &context),
                p::evaluate_compiled(&compiled, action, &bucket, &key, &context)
            );
            prop_assert_eq!(
                p::is_explicitly_denied_iam(&policies, action, &bucket, &key, &context),
                p::is_explicitly_denied_compiled(&compiled, action, &bucket, &key, &context)
            );
        }
    }

    fn statement(effect: IAMEffect, action: &str, resource: &str) -> IAMStatement {
        IAMStatement::new(effect)
            .with_action(IAMAction::Single(action.into()))
            .with_resource(IAMResource::Single(resource.into()))
    }

    /// iam-rs errs past its statement limit unless a Deny returned first;
    /// the compiled walk errs at the same statement.
    #[test]
    fn the_statement_limit_errs_where_iam_rs_errs() {
        let limit = EvaluationOptions::default().max_statements;
        let filler = statement(IAMEffect::Allow, "s3:GetObject", "arn:aws:s3:::other/*");
        let mut policy = IAMPolicy::new();
        for _ in 0..limit {
            policy = policy.add_statement(filler.clone());
        }
        let at_limit = vec![policy.clone()];
        let over = vec![policy
            .clone()
            .add_statement(statement(IAMEffect::Allow, "*", "*"))];
        let deny_first = vec![
            IAMPolicy::new().add_statement(statement(IAMEffect::Deny, "*", "*")),
            policy,
        ];
        let ctx = Context::new();
        for policies in [&at_limit, &over, &deny_first] {
            let want = reference(policies, S3Action::Read, "b/k", &ctx);
            let got = CompiledPolicy::new(policies).evaluate(S3Action::Read, "b/k", &ctx);
            assert!(same(&want, &got), "{want:?} vs {got:?}");
        }
        assert!(CompiledPolicy::new(&over)
            .evaluate(S3Action::Read, "b/k", &ctx)
            .is_err());
        assert_eq!(
            CompiledPolicy::new(&deny_first).evaluate(S3Action::Read, "b/k", &ctx),
            Ok(Decision::Deny)
        );
    }

    /// An unparsable pattern errs only when the walk reaches it: an
    /// earlier pattern of the same statement that matches, or a Deny
    /// before it, decides first.
    #[test]
    fn an_invalid_pattern_errs_in_iam_rs_order() {
        let ctx = Context::new();
        let multi = |patterns: &[&str]| {
            vec![IAMPolicy::new().add_statement(
                IAMStatement::new(IAMEffect::Allow)
                    .with_action(IAMAction::Single("*".into()))
                    .with_resource(IAMResource::Multiple(
                        patterns.iter().map(|p| p.to_string()).collect(),
                    )),
            )]
        };
        for policies in [
            multi(&["arn:aws:s3:::b/*", "notanarn"]),
            multi(&["notanarn", "arn:aws:s3:::b/*"]),
            vec![
                IAMPolicy::new().add_statement(statement(IAMEffect::Deny, "*", "*")),
                multi(&["notanarn"]).remove(0),
            ],
        ] {
            let want = reference(&policies, S3Action::Read, "b/k", &ctx);
            let got = CompiledPolicy::new(&policies).evaluate(S3Action::Read, "b/k", &ctx);
            assert!(same(&want, &got), "{want:?} vs {got:?}");
        }
        assert!(
            CompiledPolicy::new(&multi(&["notanarn", "arn:aws:s3:::b/*"]))
                .evaluate(S3Action::Read, "b/k", &ctx)
                .is_err()
        );
    }

    /// The compiled walk asks iam-rs nothing for a statement without a
    /// condition, and builds the iam-rs request only when it reaches a
    /// conditioned statement whose action and resource match.
    #[test]
    fn only_conditioned_statements_reach_iam_rs() {
        let plain = CompiledPolicy::new(&[IAMPolicy::new().add_statement(statement(
            IAMEffect::Allow,
            "s3:GetObject",
            "arn:aws:s3:::b/*",
        ))]);
        assert!(plain.statements.iter().all(|s| matches!(
            s.kind,
            StatementKind::Compiled {
                condition: None,
                ..
            }
        )));
        let conditioned = IAMPolicy::new().add_statement(
            statement(IAMEffect::Allow, "s3:GetObject", "arn:aws:s3:::b/*").with_condition_struct(
                iam_rs::Condition::new(
                    iam_rs::IAMOperator::IpAddress,
                    "aws:SourceIp".to_string(),
                    iam_rs::ConditionValue::String("10.0.0.0/8".into()),
                ),
            ),
        );
        let compiled = CompiledPolicy::new(&[conditioned]);
        assert!(matches!(
            compiled.statements[0].kind,
            StatementKind::Compiled {
                condition: Some(_),
                ..
            }
        ));
        let mut ctx = Context::new();
        super::super::permissions::insert_source_ip(&mut ctx, Some("10.1.2.3".parse().unwrap()));
        assert_eq!(
            compiled.evaluate(S3Action::Read, "b/k", &ctx),
            Ok(Decision::Allow)
        );
        let mut far = Context::new();
        super::super::permissions::insert_source_ip(&mut far, Some("8.8.8.8".parse().unwrap()));
        assert_eq!(
            compiled.evaluate(S3Action::Read, "b/k", &far),
            Ok(Decision::NotApplicable)
        );
    }
}
