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
//! - **Pattern conditions** (`StringLike`, `StringNotLike`, `ArnLike`,
//!   `ArnNotLike`, their `IfExists` and set-operator forms) are evaluated here
//!   with the linear matcher ([`crate::linear_glob`]), with iam-rs's rules for
//!   missing keys, multi-valued keys and non-string values. iam-rs matches
//!   them with a recursive search whose cost grows with the text to the power
//!   of the number of `*`.
//! - **Policy variables** in a resource (`${iam:username}`, ...) are
//!   interpolated per request here, with iam-rs's own `interpolate_variables`,
//!   and the result is matched with the linear matcher like any other glob.
//! - **Other conditions** and anything unusual (`NotAction`, `NotResource`, a
//!   principal): the statement, or its other conditions, are evaluated by
//!   iam-rs, alone, against a request that is built once per evaluation and
//!   only when such a statement is reached. Only [`ask_iam_rs`] calls it, and
//!   it refuses a resource text over [`MAX_KEY_BYTES`]; [`CompiledPolicy::evaluate`]
//!   denies such text first, for every principal that is not an unconditional
//!   admin, so no caller needs a length check of its own.
//!
//! The statements are walked in iam-rs's order with iam-rs's rules: the
//! first explicit Deny wins at once, an error ends the evaluation, and the
//! statement limit errs at the same statement. The differential proptest
//! (`compiled_policy_matches_iam_rs`) holds this evaluator equal to iam-rs.

use iam_rs::{
    Arn, ArnMatcher, ConditionBlock, Context, ContextValue, Decision, EvaluationError,
    EvaluationOptions, IAMAction, IAMEffect, IAMOperator, IAMPolicy, IAMRequest, IAMResource,
    IAMStatement, PolicyEvaluator, Principal, PrincipalId,
};
use std::collections::HashMap;

use crate::linear_glob::wildcard_match;

use super::types::S3Action;
use crate::api::input_limits::{MAX_KEY_BYTES, MAX_QUERY_TEXT_BYTES};

/// The five request actions, in bit order.
const ACTIONS: [S3Action; 5] = [
    S3Action::Read,
    S3Action::Write,
    S3Action::Delete,
    S3Action::List,
    S3Action::Admin,
];

/// The mask of all five request actions.
const ALL_ACTIONS: u8 = 1 | 2 | 4 | 8 | 16;

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
    /// A pattern with a policy variable: interpolated from the request
    /// context, then decided as the other shapes are.
    Templated(String),
    /// iam-rs cannot parse the pattern: the evaluation errs when it gets here.
    Invalid(String),
}

impl ResourcePattern {
    /// The shape of a resource pattern of a statement.
    fn compile(pattern: &str) -> Self {
        if pattern != "*" && pattern.contains("${") {
            return Self::Templated(pattern.to_string());
        }
        Self::of_text(pattern)
    }

    /// The shape of a pattern text, as iam-rs reads it (after it interpolated
    /// the variables, for a templated one). A text that still holds `${` is a
    /// variable that iam-rs could not resolve: the evaluation errs.
    fn of_text(pattern: &str) -> Self {
        if pattern == "*" {
            return Self::Any;
        }
        if pattern.contains("${") {
            return Self::Invalid(format!("unresolved variable in {pattern:?}"));
        }
        // iam-rs: `interpolate_variables` (a no-op without `${`), then
        // `ArnMatcher::from_pattern`, whose error ends the evaluation.
        if let Err(e) = ArnMatcher::from_pattern(pattern) {
            return Self::Invalid(e.to_string());
        }
        let Ok(arn) = Arn::parse(pattern) else {
            return Self::Invalid(format!("unparsable pattern {pattern:?}"));
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
                    wildcard_match(pat, target)
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
            return Self::Never;
        }
        let resource = arn.resource;
        if !wild(&resource) {
            Self::Exact(resource)
        } else {
            match resource.strip_suffix('*') {
                Some(head) if !wild(head) => Self::Prefix(head.to_string()),
                _ => Self::Glob(resource),
            }
        }
    }

    /// iam-rs's answer for the request resource `resource`.
    fn matches(&self, resource: &str, context: &Context) -> Result<bool, EvaluationError> {
        Ok(match self {
            Self::Templated(template) => {
                let text = iam_rs::interpolate_variables(template, context)?;
                return Self::of_text(&text).matches(resource, context);
            }
            Self::Any => true,
            Self::Never => false,
            Self::Exact(s) => resource == s,
            Self::Prefix(head) => resource.starts_with(head.as_str()),
            Self::Glob(glob) => wildcard_match(glob, resource),
            Self::Invalid(e) => return Err(EvaluationError::InvalidArn(e.clone())),
        })
    }
}

/// How a pattern condition reads a multi-valued context key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SetOperator {
    None,
    ForAnyValue,
    ForAllValues,
}

/// One `key: value` of a `StringLike`-family condition (see the module docs),
/// decided as iam-rs decides it.
#[derive(Debug)]
struct PatternCondition {
    /// `StringNotLike` and `ArnNotLike`.
    negated: bool,
    if_exists: bool,
    set: SetOperator,
    key: String,
    /// The values in order; `None` is a value that is not a string, which
    /// iam-rs refuses with an error when it gets there.
    patterns: Vec<Option<String>>,
}

/// `Some(negated)` for the operators that match with a pattern.
fn pattern_operator(operator: &IAMOperator) -> Option<bool> {
    use IAMOperator as O;
    match operator {
        O::StringLike
        | O::ForAllValuesStringLike
        | O::ForAnyValueStringLike
        | O::StringLikeIfExists
        | O::ArnLike
        | O::ForAllValuesArnLike
        | O::ForAnyValueArnLike
        | O::ArnLikeIfExists => Some(false),
        O::StringNotLike
        | O::ForAllValuesStringNotLike
        | O::ForAnyValueStringNotLike
        | O::StringNotLikeIfExists
        | O::ArnNotLike
        | O::ForAllValuesArnNotLike
        | O::ForAnyValueArnNotLike
        | O::ArnNotLikeIfExists => Some(true),
        _ => None,
    }
}

impl PatternCondition {
    fn new(
        operator: &IAMOperator,
        negated: bool,
        key: &str,
        value: &iam_rs::ConditionValue,
    ) -> Self {
        let text = operator.to_string();
        let set = if text.starts_with("ForAnyValue:") {
            SetOperator::ForAnyValue
        } else if text.starts_with("ForAllValues:") {
            SetOperator::ForAllValues
        } else {
            SetOperator::None
        };
        let patterns = match value.to_json_value() {
            serde_json::Value::Array(items) => items,
            single => vec![single],
        }
        .into_iter()
        .map(|v| v.as_str().map(str::to_string))
        .collect();
        Self {
            negated,
            if_exists: operator.is_if_exists_operator(),
            set,
            key: key.to_string(),
            patterns,
        }
    }

    fn test(&self, pattern: &str, text: &str) -> bool {
        wildcard_match(pattern, text) != self.negated
    }

    /// iam-rs's answer for this condition: true when any value holds.
    fn holds(&self, context: &Context) -> Result<bool, EvaluationError> {
        for pattern in &self.patterns {
            let pattern = pattern.as_deref().ok_or_else(|| {
                EvaluationError::ConditionError(
                    "String condition value must be a string".to_string(),
                )
            })?;
            let held = match context.get(&self.key) {
                Some(ContextValue::String(text)) => self.test(pattern, text),
                Some(ContextValue::StringList(list)) => match self.set {
                    SetOperator::ForAnyValue => list.iter().any(|v| self.test(pattern, v)),
                    SetOperator::ForAllValues => list.iter().all(|v| self.test(pattern, v)),
                    SetOperator::None => {
                        return Err(EvaluationError::ConditionError(
                            "Multivalued context keys require a condition set operator."
                                .to_string(),
                        ))
                    }
                },
                Some(_) => false,
                None => self.if_exists,
            };
            if held {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// The condition block of a compiled statement: the pattern conditions, here,
/// and the rest, for iam-rs.
#[derive(Debug)]
struct CompiledCondition {
    patterns: Vec<PatternCondition>,
    /// The other conditions, as a statement that matches every action and
    /// resource (the walk checked them already).
    rest: Option<PolicyEvaluator>,
}

impl CompiledCondition {
    fn compile(effect: IAMEffect, block: &ConditionBlock) -> Self {
        let mut patterns = Vec::new();
        let mut rest: HashMap<_, _> = HashMap::new();
        for (operator, keys) in &block.conditions {
            match pattern_operator(operator) {
                Some(negated) => patterns.extend(
                    keys.iter()
                        .map(|(key, value)| PatternCondition::new(operator, negated, key, value)),
                ),
                None => {
                    rest.insert(operator.clone(), keys.clone());
                }
            }
        }
        let rest = (!rest.is_empty()).then(|| {
            let mut statement = IAMStatement::new(effect)
                .with_action(IAMAction::Single("*".into()))
                .with_resource(IAMResource::Single("*".into()));
            statement.condition = Some(ConditionBlock { conditions: rest });
            single_statement_evaluator(&statement)
        });
        Self { patterns, rest }
    }

    /// Do the pattern conditions all hold?
    fn patterns_hold(&self, context: &Context) -> Result<bool, EvaluationError> {
        for pattern in &self.patterns {
            if !pattern.holds(context)? {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

#[derive(Debug)]
enum StatementKind {
    /// Action and resource decided here; the condition, if any, here and by iam-rs.
    Compiled {
        actions: u8,
        resources: Vec<ResourcePattern>,
        condition: Option<CompiledCondition>,
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
        let resources = patterns.into_iter().map(ResourcePattern::compile).collect();
        Self {
            effect: statement.effect,
            kind: StatementKind::Compiled {
                actions,
                resources,
                condition: statement
                    .condition
                    .as_ref()
                    .map(|block| CompiledCondition::compile(statement.effect, block)),
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
    /// An unconditional Allow of every action on every resource, and no
    /// statement that iam-rs evaluates on the resource text: the one
    /// principal that [`Self::evaluate`] does not refuse long text for.
    unconditional_admin: bool,
}

/// iam-rs's answer for one statement, asked of it ALONE. THE call into iam-rs
/// evaluation (source guard `iam_rs_evaluates_only_through_ask_iam_rs`).
/// `reads_resource_text`: the statement matches the request resource (a
/// delegated one); iam-rs matches it with a search that costs the text
/// squared, so text over [`MAX_KEY_BYTES`] is refused here, whatever the caller did.
fn ask_iam_rs(
    evaluator: &PolicyEvaluator,
    request: &IAMRequest,
    resource: &str,
    reads_resource_text: bool,
) -> Result<Decision, EvaluationError> {
    if reads_resource_text && resource.len() > MAX_KEY_BYTES + MAX_BUCKET_PART {
        return Err(EvaluationError::InvalidRequest(
            "resource text over the key limit".to_string(),
        ));
    }
    evaluator.evaluate(request).map(|r| r.decision)
}

/// The most that a bucket name and its `/` add to a resource text. A bucket
/// name is at most 63 bytes; the rest is slack for a name that no request
/// validated yet.
const MAX_BUCKET_PART: usize = 256;

/// Pure: does the request carry client text over the limit: a key (the
/// resource after its bucket) or a bucket name, or the text of `s3:prefix` or
/// `s3:delimiter`?
fn text_over_limit(resource: &str, context: &Context) -> bool {
    let (bucket, key) = resource.split_once('/').unwrap_or((resource, ""));
    if bucket.len() > MAX_BUCKET_PART || key.len() > MAX_KEY_BYTES {
        return true;
    }
    ["s3:prefix", "s3:delimiter"].iter().any(|name| {
        matches!(context.get(name), Some(ContextValue::String(text)) if text.len() > MAX_QUERY_TEXT_BYTES)
    })
}

impl CompiledPolicy {
    pub(crate) fn new(policies: &[IAMPolicy]) -> Self {
        let statements: Vec<CompiledStatement> = policies
            .iter()
            .flat_map(|p| p.statement.iter())
            .map(CompiledStatement::compile)
            .collect();
        let unconditional_admin = !statements
            .iter()
            .any(|s| matches!(s.kind, StatementKind::Delegated(_)))
            && statements.iter().any(|s| {
                s.effect == IAMEffect::Allow
                    && matches!(&s.kind, StatementKind::Compiled { actions, resources, condition: None }
                        if *actions == ALL_ACTIONS
                            && resources.iter().any(|r| matches!(r, ResourcePattern::Any)))
            });
        Self {
            statements,
            max_statements: EvaluationOptions::default().max_statements,
            unconditional_admin,
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
        // THE bound of client text: no statement below sees a key or prefix
        // over the limit, except an unconditional admin's (nothing of it
        // reaches iam-rs).
        if !self.unconditional_admin && text_over_limit(resource, context) {
            return Ok(Decision::Deny);
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
            let mut delegated = false;
            let evaluator = match &statement.kind {
                StatementKind::Delegated(evaluator) => {
                    delegated = true;
                    Some(evaluator)
                }
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
                        if pattern.matches(resource, context)? {
                            matched = true;
                            break;
                        }
                    }
                    if !matched {
                        continue;
                    }
                    match condition {
                        Some(condition) => {
                            if !condition.patterns_hold(context)? {
                                continue;
                            }
                            condition.rest.as_ref()
                        }
                        None => None,
                    }
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
                    ask_iam_rs(evaluator, request, resource, delegated)?
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
        "arn:aws:s3:::b/${iam:username}/*",
        "arn:aws:s3:::b/${iam:username}",
        "arn:aws:s3:::${iam:username}/*",
        "arn:aws:s3:::b/k${iam:access_key_id}*${iam:username}",
        "arn:aws:s3:::b/${iam:username}${iam:username}?",
        "arn:aws:${iam:username}:::b/*",
        "${iam:username}",
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
            json!({"StringLike": {"s3:prefix": "a*b*"}}),
            json!({"ArnLike": {"s3:prefix": ["a?", "*"]}}),
            json!({"ArnNotLike": {"s3:prefix": "?*"}}),
            json!({"StringLikeIfExists": {"s3:delimiter": "/*"}}),
            json!({"StringNotLikeIfExists": {"s3:delimiter": "/*"}}),
            json!({"StringLike": {"s3:prefix": 5}}),
            json!({"StringLike": {"s3:prefix": []}}),
            json!({"StringLike": {"aws:Tags": "a*"}}),
            json!({"ForAnyValue:StringLike": {"aws:Tags": ["a*", "b"]}}),
            json!({"ForAllValues:StringLike": {"aws:Tags": "a*"}}),
            json!({"ForAnyValue:StringNotLike": {"aws:Tags": "a*"}}),
            json!({"ForAllValues:ArnNotLike": {"aws:Tags": ["a*", "?"]}}),
            json!({"ForAnyValue:StringLike": {"s3:prefix": "a*"}}),
            json!({"StringLike": {"s3:prefix": "a*"}, "IpAddress": {"aws:SourceIp": "10.0.0.0/8"}}),
            json!({"StringNotLike": {"s3:prefix": ["a*", "b"], "s3:delimiter": "?"}}),
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
                    "*",
                    "b/*",
                    "b",
                    "b/a*",
                    "b/k",
                    "bb/*",
                    "é/*",
                    "b/*/x",
                    "b?/*",
                    "*/k",
                    "b/${iam:username}/*",
                    "${iam:username}/*",
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
            prop::option::of(0..conditions().len()),
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
            prop::option::of(prop::sample::select(vec![
                vec![],
                vec!["a"],
                vec!["ab", "b"],
                vec!["b", "c"],
            ])),
            prop::option::of(prop::sample::select(vec!["/", "x"])),
            prop::option::of(prop::sample::select(vec!["a", "k", "*", "?", "", "ab"])),
            prop::option::of(prop::sample::select(vec!["k", "a*", "x"])),
        )
            .prop_map(
                |(ip, prefix, max_keys, tags, delimiter, user, access_key)| {
                    let mut ctx = Context::new();
                    if let Some(u) = user {
                        ctx.insert("iam:username".into(), ContextValue::String(u.into()));
                    }
                    if let Some(k) = access_key {
                        ctx.insert("iam:access_key_id".into(), ContextValue::String(k.into()));
                    }
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
                    if let Some(tags) = tags {
                        ctx.insert(
                            "aws:Tags".into(),
                            iam_rs::ContextValue::StringList(
                                tags.into_iter().map(str::to_string).collect(),
                            ),
                        );
                    }
                    if let Some(d) = delimiter {
                        ctx.insert(
                            "s3:delimiter".into(),
                            iam_rs::ContextValue::String(d.into()),
                        );
                    }
                    ctx
                },
            )
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

    /// The pattern that made iam-rs's recursive search try every split of the
    /// text: many `*`, then a letter that the text never has.
    fn hostile(stars: usize) -> String {
        format!("{}b", "a*".repeat(stars))
    }

    fn steps_for(policy: &CompiledPolicy, len: usize) -> (Result<Decision, EvaluationError>, u64) {
        let text = "a".repeat(len);
        let mut ctx = Context::new();
        ctx.insert("s3:prefix".into(), ContextValue::String(text.clone()));
        let before = crate::linear_glob::STEPS.with(std::cell::Cell::get);
        let decision = policy.evaluate(S3Action::Read, &format!("b/{text}"), &ctx);
        (
            decision,
            crate::linear_glob::STEPS.with(std::cell::Cell::get) - before,
        )
    }

    /// DETERMINISTIC cost test (steps, never wall-clock): a resource glob and
    /// a `StringLike` value with eight `*` cost steps in proportion to the
    /// text, and at most pattern x text.
    #[test]
    fn a_hostile_pattern_costs_steps_in_proportion_to_the_text() {
        let condition = iam_rs::Condition::new(
            iam_rs::IAMOperator::StringLike,
            "s3:prefix".to_string(),
            iam_rs::ConditionValue::String(hostile(8)),
        );
        let glob = format!("arn:aws:s3:::b/{}", hostile(8));
        let policy = CompiledPolicy::new(&[IAMPolicy::new()
            .add_statement(statement(IAMEffect::Allow, "s3:GetObject", &glob))
            .add_statement(
                statement(IAMEffect::Allow, "s3:GetObject", "arn:aws:s3:::b/*")
                    .with_condition_struct(condition),
            )]);
        // The linear matcher ran at all (the steps are counted only there).
        assert!(
            steps_for(&policy, 8).1 > 0,
            "the linear matcher did not run"
        );
        let (small_decision, small) = steps_for(&policy, 128);
        let (large_decision, large) = steps_for(&policy, 1024);
        // The text has no `b`: the glob does not match, the condition does not hold.
        assert_eq!(small_decision, Ok(Decision::NotApplicable));
        assert_eq!(large_decision, Ok(Decision::NotApplicable));
        let bound = 2 * (hostile(8).len() * 1024) as u64;
        assert!(
            large <= bound,
            "{large} steps for 1024 characters, bound {bound}"
        );
        assert!(
            large <= small * 9,
            "8 times the text cost {large} steps, over 9 times the {small} steps"
        );
    }

    /// A policy of an unconditional admin plus one more statement.
    fn admin_with(extra: IAMStatement) -> CompiledPolicy {
        CompiledPolicy::new(&[IAMPolicy::new()
            .add_statement(statement(IAMEffect::Allow, "*", "*"))
            .add_statement(extra)])
    }

    /// The steps of one evaluation of `resource` with the context `username`.
    fn steps_of(
        policy: &CompiledPolicy,
        resource: &str,
    ) -> (Result<Decision, EvaluationError>, u64) {
        let mut ctx = Context::new();
        ctx.insert("iam:username".into(), ContextValue::String("dana".into()));
        let before = crate::linear_glob::STEPS.with(std::cell::Cell::get);
        let decision = policy.evaluate(S3Action::Read, resource, &ctx);
        (
            decision,
            crate::linear_glob::STEPS.with(std::cell::Cell::get) - before,
        )
    }

    /// DETERMINISTIC cost test (steps, never wall-clock): a resource with a
    /// `${iam:username}` variable and eight `*` costs steps in proportion to a
    /// key of 1 MiB. iam-rs matched it with a search that costs the text
    /// squared (its recursion; a 1 MiB key kept it busy for minutes).
    #[test]
    fn a_templated_resource_costs_steps_in_proportion_to_the_key() {
        let glob = format!("arn:aws:s3:::b/${{iam:username}}/{}", hostile(8));
        let policy = admin_with(statement(IAMEffect::Deny, "s3:GetObject", &glob));
        let key = |len: usize| format!("b/dana/{}", "a".repeat(len));
        let (small_decision, small) = steps_of(&policy, &key(128 * 1024));
        let (large_decision, large) = steps_of(&policy, &key(1024 * 1024));
        // The admin is not refused for the length, and the Deny never matches.
        assert_eq!(small_decision, Ok(Decision::Allow));
        assert_eq!(large_decision, Ok(Decision::Allow));
        assert!(small > 0, "the linear matcher did not run");
        let bound = 2 * (hostile(8).len() as u64 + 16) * (1024 * 1024 + 16);
        assert!(large <= bound, "{large} steps, bound {bound}");
        assert!(
            large <= small * 9,
            "8 times the key cost {large} steps, over 9 times the {small} steps"
        );
    }

    /// A templated resource decides as iam-rs decides, on a user name and on
    /// a name with no variable value (iam-rs reads it as empty).
    #[test]
    fn a_templated_resource_matches_the_interpolated_text() {
        let policy = CompiledPolicy::new(&[IAMPolicy::new().add_statement(statement(
            IAMEffect::Allow,
            "s3:GetObject",
            "arn:aws:s3:::b/home/${iam:username}/*",
        ))]);
        let mut dana = Context::new();
        dana.insert("iam:username".into(), ContextValue::String("dana".into()));
        let got = |resource: &str, ctx: &Context| policy.evaluate(S3Action::Read, resource, ctx);
        assert_eq!(got("b/home/dana/x", &dana), Ok(Decision::Allow));
        assert_eq!(got("b/home/bob/x", &dana), Ok(Decision::NotApplicable));
        assert_eq!(got("b/home//x", &Context::new()), Ok(Decision::Allow));
    }

    /// THE bound of client text: a principal that is not an unconditional
    /// admin is denied a key or a prefix over 1024 bytes, before any statement
    /// sees it; an unconditional admin is not.
    #[test]
    fn text_over_the_limit_is_denied_for_everyone_but_an_unconditional_admin() {
        let long = "a".repeat(MAX_KEY_BYTES + 1);
        let limit = "a".repeat(MAX_KEY_BYTES);
        let user = CompiledPolicy::new(&[IAMPolicy::new().add_statement(statement(
            IAMEffect::Allow,
            "*",
            "arn:aws:s3:::b/*",
        ))]);
        let ctx = Context::new();
        assert_eq!(
            user.evaluate(S3Action::Read, &format!("b/{limit}"), &ctx),
            Ok(Decision::Allow)
        );
        assert_eq!(
            user.evaluate(S3Action::Read, &format!("b/{long}"), &ctx),
            Ok(Decision::Deny)
        );
        // A prefix and a delimiter of a LIST are text too.
        for name in ["s3:prefix", "s3:delimiter"] {
            let mut list = Context::new();
            list.insert(name.into(), ContextValue::String(long.clone()));
            assert_eq!(
                user.evaluate(S3Action::List, "b", &list),
                Ok(Decision::Deny)
            );
        }
        // An admin: wildcard actions and resources, no condition.
        let admin = CompiledPolicy::new(&[IAMPolicy::new().add_statement(statement(
            IAMEffect::Allow,
            "*",
            "*",
        ))]);
        assert_eq!(
            admin.evaluate(S3Action::Delete, &format!("b/{long}"), &ctx),
            Ok(Decision::Allow)
        );
        // Not an admin: a condition, one action, one bucket, or a statement
        // that iam-rs evaluates on the text.
        let conditioned =
            statement(IAMEffect::Allow, "*", "*").with_condition_struct(iam_rs::Condition::new(
                iam_rs::IAMOperator::IpAddress,
                "aws:SourceIp".to_string(),
                iam_rs::ConditionValue::String("10.0.0.0/8".into()),
            ));
        let mut not_admin = vec![
            statement(IAMEffect::Allow, "s3:GetObject", "*"),
            statement(IAMEffect::Allow, "*", "arn:aws:s3:::b/*"),
            conditioned,
        ];
        let mut not_resource = statement(IAMEffect::Allow, "*", "*");
        not_resource.not_resource = Some(IAMResource::Single("arn:aws:s3:::other/*".into()));
        not_resource.resource = None;
        let with_not_resource = admin_with(not_resource);
        assert!(!with_not_resource.unconditional_admin);
        assert_eq!(
            with_not_resource.evaluate(S3Action::Read, &format!("b/{long}"), &ctx),
            Ok(Decision::Deny)
        );
        for st in not_admin.drain(..) {
            let p = CompiledPolicy::new(&[IAMPolicy::new()
                .add_statement(statement(IAMEffect::Allow, "*", "*").with_condition_struct(
                    iam_rs::Condition::new(
                        iam_rs::IAMOperator::IpAddress,
                        "aws:SourceIp".to_string(),
                        iam_rs::ConditionValue::String("0.0.0.0/0".into()),
                    ),
                ))
                .add_statement(st)]);
            assert!(!p.unconditional_admin);
            assert_eq!(
                p.evaluate(S3Action::Read, &format!("b/{long}"), &ctx),
                Ok(Decision::Deny)
            );
        }
    }

    /// The one call into iam-rs refuses resource text over the key limit for a
    /// statement that it matches on the text, whoever asked.
    #[test]
    fn the_one_iam_rs_call_refuses_long_text() {
        let long = format!("b/{}", "a".repeat(MAX_KEY_BYTES * 2));
        let evaluator = single_statement_evaluator(&statement(IAMEffect::Allow, "*", "*"));
        let request = iam_request(S3Action::Read, &long, &Context::new());
        assert!(ask_iam_rs(&evaluator, &request, &long, true).is_err());
        assert_eq!(
            ask_iam_rs(&evaluator, &request, &long, false),
            Ok(Decision::Allow),
            "a statement that reads no resource text is not refused"
        );
    }

    /// Source guard: iam-rs evaluates only through `ask_iam_rs`, with a
    /// resource text that is bounded. The other calls named `evaluate` are
    /// `CompiledPolicy::evaluate` (the bound) and the two that judge a text
    /// that the proxy fixed itself (the admin rule: `"admin"`; the probe of a
    /// condition at save time and the action masks of a build: `"probe"`).
    #[test]
    fn iam_rs_evaluates_only_through_ask_iam_rs() {
        let mut found: Vec<(String, String)> = Vec::new();
        for source in crate::source_scan::ast::src() {
            if !source.prod.mentions("iam_rs") {
                continue;
            }
            for call in source.prod.calls_named("evaluate").filter(|c| c.method) {
                found.push((source.rel.clone(), source.fn_name(call.func).to_string()));
            }
        }
        found.sort();
        found.dedup();
        let allowed = [
            ("src/iam/compiled.rs", "ask_iam_rs"),
            ("src/iam/compiled.rs", "action_mask"),
            ("src/iam/permissions.rs", "evaluate_compiled"),
            ("src/iam/permissions.rs", "is_explicitly_denied_compiled"),
            ("src/iam/permissions.rs", "iam_rs_eval"),
            ("src/iam/permissions.rs", "probe_one_condition_value"),
        ];
        for (file, function) in &found {
            assert!(
                allowed.contains(&(file.as_str(), function.as_str())),
                "{file}::{function} calls `.evaluate(`: only ask_iam_rs may call iam-rs with request text"
            );
        }
        assert!(found.contains(&("src/iam/compiled.rs".into(), "ask_iam_rs".into())));
    }

    /// Source guard: the compiled evaluator never calls iam-rs's recursive
    /// matcher on text that a request carries.
    #[test]
    fn the_compiled_evaluator_never_calls_the_recursive_matcher() {
        let source = include_str!("compiled.rs");
        let production: String = source
            .split("#[cfg(test)]")
            .next()
            .unwrap()
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect();
        assert!(
            !production.contains("Arn::wildcard_match"),
            "use crate::linear_glob::wildcard_match"
        );
    }

    /// A pattern condition decides in the compiled evaluator, and only the
    /// other conditions reach iam-rs.
    #[test]
    fn pattern_conditions_do_not_reach_iam_rs() {
        let block: iam_rs::ConditionBlock = serde_json::from_value(serde_json::json!({
            "StringLike": {"s3:prefix": "a*"},
            "IpAddress": {"aws:SourceIp": "10.0.0.0/8"},
        }))
        .unwrap();
        let compiled = CompiledCondition::compile(IAMEffect::Allow, &block);
        assert_eq!(compiled.patterns.len(), 1);
        assert!(compiled.rest.is_some());
        let only_patterns: iam_rs::ConditionBlock =
            serde_json::from_value(serde_json::json!({"StringNotLike": {"s3:prefix": "a*"}}))
                .unwrap();
        assert!(CompiledCondition::compile(IAMEffect::Allow, &only_patterns)
            .rest
            .is_none());
    }
}
