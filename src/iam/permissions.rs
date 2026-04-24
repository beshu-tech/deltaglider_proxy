//! Permission evaluation: pure functions with no I/O, no framework dependencies.
//!
//! The public API is via `AuthenticatedUser::can()` / `can_see_bucket()` / `is_admin()`.
//! These functions are the implementation details.
//!
//! Two evaluation paths:
//! - Legacy: hand-rolled two-pass allow/deny on simple {actions, resources, effect}
//! - IAM: delegates to `iam-rs` crate for full AWS IAM policy evaluation with conditions

use iam_rs::{
    Arn, Context, Decision, IAMAction, IAMEffect, IAMPolicy, IAMRequest, IAMResource, IAMStatement,
    PolicyEvaluator, Principal, PrincipalId,
};

use super::types::{Permission, S3Action};

/// Valid action verbs for permissions.
const VALID_ACTIONS: &[&str] = &["read", "write", "delete", "list", "admin", "*"];

// === IAM-RS Integration ===

/// Map a simple action verb to an AWS IAM S3 action string.
fn action_to_iam(action: &str) -> String {
    match action {
        "read" => "s3:GetObject".to_string(),
        "write" => "s3:PutObject".to_string(),
        "delete" => "s3:DeleteObject".to_string(),
        "list" => "s3:ListBucket".to_string(),
        "admin" => "s3:CreateBucket".to_string(),
        "*" => "s3:*".to_string(),
        other => format!("s3:{}", other),
    }
}

/// Map a simple resource pattern to an S3 ARN string.
fn resource_to_arn(resource: &str) -> String {
    if resource == "*" {
        "*".to_string()
    } else {
        format!("arn:aws:s3:::{}", resource)
    }
}

/// Convert a `Permission` to an `IAMPolicy` for iam-rs evaluation.
pub fn permission_to_iam_policy(perm: &Permission) -> IAMPolicy {
    let effect = if perm.effect == "Deny" {
        IAMEffect::Deny
    } else {
        IAMEffect::Allow
    };

    let actions: Vec<String> = perm.actions.iter().map(|a| action_to_iam(a)).collect();
    let resources: Vec<String> = perm.resources.iter().map(|r| resource_to_arn(r)).collect();

    let mut stmt = IAMStatement::new(effect)
        .with_action(IAMAction::Multiple(actions))
        .with_resource(IAMResource::Multiple(resources));

    // Parse conditions from JSON if present
    if let Some(ref cond_json) = perm.conditions {
        match serde_json::from_value::<iam_rs::ConditionBlock>(cond_json.clone()) {
            Ok(cb) => {
                for (operator, key_values) in &cb.conditions {
                    for (key, value) in key_values {
                        stmt = stmt.with_condition_struct(iam_rs::Condition::new(
                            operator.clone(),
                            key.clone(),
                            value.clone(),
                        ));
                    }
                }
            }
            Err(e) => {
                tracing::warn!("Failed to parse conditions: {} — input: {}", e, cond_json);
                if perm.effect.eq_ignore_ascii_case("Deny") {
                    // For Deny rules: fail safe by applying the Deny WITHOUT conditions.
                    // A Deny with unparseable conditions should still deny (broadest deny).
                    // Dropping it would silently allow access the admin intended to block.
                    tracing::warn!(
                        "  Applying Deny rule WITHOUT conditions (fail-safe for Deny rules)"
                    );
                    // Fall through — stmt is already built without conditions
                } else {
                    // For Allow rules: fail closed by skipping the statement entirely.
                    // An Allow with unparseable conditions should NOT grant access.
                    return IAMPolicy::new();
                }
            }
        }
    }

    IAMPolicy::new().add_statement(stmt)
}

/// Build the S3 resource ARN string from bucket and key.
fn build_resource_arn(bucket: &str, key: &str) -> String {
    if key.is_empty() {
        format!("arn:aws:s3:::{}", bucket)
    } else {
        format!("arn:aws:s3:::{}/{}", bucket, key)
    }
}

/// Build an IAM request and evaluator from common parameters.
/// Returns `None` if the ARN cannot be parsed (caller decides fail-open vs fail-closed).
fn build_iam_evaluator(
    policies: &[IAMPolicy],
    action: S3Action,
    bucket: &str,
    key: &str,
    context: &Context,
) -> Option<(IAMRequest, PolicyEvaluator, String)> {
    let resource_str = build_resource_arn(bucket, key);
    let resource = Arn::parse(&resource_str).ok()?;
    let request = IAMRequest::new_with_context(
        Principal::Aws(PrincipalId::String("000000000000".into())),
        action.to_iam_action(),
        resource,
        context.clone(),
    );
    let evaluator = PolicyEvaluator::with_policies(policies.to_vec());
    Some((request, evaluator, resource_str))
}

/// Evaluate permissions using iam-rs policy engine.
/// Supports conditions (s3:prefix, aws:SourceIp, etc.) via the context parameter.
pub(crate) fn evaluate_iam(
    policies: &[IAMPolicy],
    action: S3Action,
    bucket: &str,
    key: &str,
    context: &Context,
) -> bool {
    let (request, evaluator, resource_str) =
        match build_iam_evaluator(policies, action, bucket, key, context) {
            Some(t) => t,
            None => return false, // fail closed
        };

    match evaluator.evaluate(&request) {
        Ok(result) => {
            if result.decision == Decision::Allow {
                return true;
            }
            // Explicit Deny on the exact resource takes priority — don't try the alt path.
            if result.decision == Decision::Deny {
                return false;
            }
            // For bucket-level operations where no rule matched (implicit deny),
            // also try with trailing slash so "bucket/*" can match bucket-level LIST.
            if key.is_empty() {
                let alt_str = format!("arn:aws:s3:::{}/", bucket);
                if let Ok(alt_arn) = Arn::parse(&alt_str) {
                    let alt_request = IAMRequest::new_with_context(
                        Principal::Aws(PrincipalId::String("000000000000".into())),
                        action.to_iam_action(),
                        alt_arn,
                        context.clone(),
                    );
                    if let Ok(alt_result) = evaluator.evaluate(&alt_request) {
                        return alt_result.decision == Decision::Allow;
                    }
                }
            }
            false
        }
        Err(e) => {
            tracing::warn!(
                "IAM policy evaluation error: {} (action={}, resource={})",
                e,
                action.to_iam_action(),
                resource_str
            );
            false // fail closed
        }
    }
}

/// Check if an explicit Deny rule matches (iam-rs path).
/// Returns true if the decision is Deny (not NotApplicable).
pub(crate) fn is_explicitly_denied_iam(
    policies: &[IAMPolicy],
    action: S3Action,
    bucket: &str,
    key: &str,
    context: &Context,
) -> bool {
    let (request, evaluator, _) = match build_iam_evaluator(policies, action, bucket, key, context)
    {
        Some(t) => t,
        None => return true, // fail closed: assume denied if ARN can't be parsed
    };

    matches!(
        evaluator.evaluate(&request),
        Ok(result) if result.decision == Decision::Deny
    )
}

/// Check if any Deny rule matches in legacy permissions (no conditions).
pub(crate) fn has_matching_deny(
    permissions: &[Permission],
    action: S3Action,
    bucket: &str,
    key: &str,
) -> bool {
    let action_str = action.as_str();
    permissions.iter().any(|perm| {
        perm.effect == "Deny" && matches_action_and_resource(perm, action_str, bucket, key)
    })
}

/// Maximum number of permission rules per user or group.
const MAX_PERMISSION_RULES: usize = 100;

/// Normalize permissions in place: fix effect casing.
pub fn normalize_permissions(permissions: &mut [Permission]) {
    for perm in permissions.iter_mut() {
        let lower = perm.effect.to_lowercase();
        perm.effect = match lower.as_str() {
            "allow" => "Allow".to_string(),
            "deny" => "Deny".to_string(),
            _ => perm.effect.clone(), // leave invalid for validate to catch
        };
    }
}

/// Validate and normalize a list of permissions, returning an error message if any are invalid.
///
/// Normalization:
/// - Effect is case-insensitive ("allow" → "Allow", "DENY" → "Deny")
///
/// Checks:
/// - Effect must be "Allow" or "Deny" (case-insensitive)
/// - Actions must be valid verbs (read, write, delete, list, admin, *)
/// - Actions list must not be empty
/// - Resources must not be empty
/// - Resource patterns: only trailing `*` is supported (no mid-pattern wildcards)
/// - Resource must not contain whitespace or control characters
/// - Maximum 100 rules per user/group
pub fn validate_permissions(permissions: &[Permission]) -> Result<(), String> {
    if permissions.len() > MAX_PERMISSION_RULES {
        return Err(format!(
            "too many permission rules ({}, max {})",
            permissions.len(),
            MAX_PERMISSION_RULES
        ));
    }

    for (i, perm) in permissions.iter().enumerate() {
        let ctx = format!("rule {}", i + 1);

        // Effect (case-insensitive)
        let effect_lower = perm.effect.to_lowercase();
        if effect_lower != "allow" && effect_lower != "deny" {
            return Err(format!(
                "{}: effect must be 'Allow' or 'Deny', got '{}'",
                ctx, perm.effect
            ));
        }

        // Actions
        if perm.actions.is_empty() {
            return Err(format!("{}: actions must not be empty", ctx));
        }
        for action in &perm.actions {
            if !VALID_ACTIONS.contains(&action.as_str()) {
                return Err(format!(
                    "{}: invalid action '{}' (valid: {})",
                    ctx,
                    action,
                    VALID_ACTIONS.join(", ")
                ));
            }
        }

        // Resources
        if perm.resources.is_empty() {
            return Err(format!("{}: resources must not be empty", ctx));
        }
        for res in &perm.resources {
            if res.is_empty() {
                return Err(format!("{}: resource pattern must not be empty", ctx));
            }
            if res.contains(char::is_whitespace) || res.chars().any(|c| c.is_control()) {
                return Err(format!(
                    "{}: resource '{}' contains whitespace or control characters",
                    ctx, res
                ));
            }
            // Only trailing * is valid. Check for * anywhere except the last character.
            if let Some(pos) = res.find('*') {
                if pos != res.len() - 1 {
                    return Err(format!(
                        "{}: resource '{}' has '*' in an invalid position — only trailing '*' is supported (e.g. 'bucket/*', 'bucket/prefix/*')",
                        ctx, res
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Check if a user's permissions allow the given action on the given resource.
/// Two-pass evaluation: explicit Deny overrides Allow. No match = implicit deny.
pub(crate) fn evaluate(
    permissions: &[Permission],
    action: S3Action,
    bucket: &str,
    key: &str,
) -> bool {
    let action_str = action.as_str();

    // Pass 1: Any explicit Deny? Reject immediately.
    for perm in permissions {
        if perm.effect == "Deny" && matches_action_and_resource(perm, action_str, bucket, key) {
            return false;
        }
    }

    // Pass 2: Any Allow? Permit.
    for perm in permissions {
        if perm.effect == "Allow" && matches_action_and_resource(perm, action_str, bucket, key) {
            return true;
        }
    }

    false // implicit deny
}

/// Check if a permission set has ANY Allow rule that touches the given bucket.
/// Used to filter ListBuckets — a user with "my-bucket/prefix/*" should see "my-bucket".
/// Ignores Deny rules for visibility (deny only blocks actions, not bucket discovery).
pub(crate) fn has_any_on_bucket(permissions: &[Permission], bucket: &str) -> bool {
    for perm in permissions {
        if perm.effect != "Allow" {
            continue;
        }
        for res in &perm.resources {
            if res == "*" {
                return true;
            }
            if res == bucket || res.starts_with(&format!("{}/", bucket)) {
                return true;
            }
        }
    }
    false
}

/// Check if the user has an Allow that grants *unrestricted* access to the
/// full bucket OR to the exact requested prefix — i.e. the listed resource
/// space is fully covered. Used by the authorization middleware to decide
/// between `ListScope::Unrestricted` and `ListScope::Filtered`:
///
/// - Returns `true` when at least one Allow covers `bucket` / `bucket/*`
///   OR covers `bucket/<prefix>*` with `prefix` being a non-empty prefix
///   that's narrower than or equal to the user's policy pattern.
/// - Returns `false` when only narrower Allows match (e.g. policy grants
///   `bucket/alice/*` but the request has no prefix or a wider one).
///
/// Deny rules are IGNORED at this stage — this predicate is about
/// "how broad is the Allow space," not "what's denied inside it."
/// The per-key filter on the handler side still evaluates denies per key.
pub(crate) fn has_unrestricted_allow_for_bucket_prefix(
    permissions: &[Permission],
    bucket: &str,
    prefix: &str,
) -> bool {
    let requested_path = if prefix.is_empty() {
        bucket.to_string()
    } else {
        format!("{}/{}", bucket, prefix)
    };

    for perm in permissions {
        if perm.effect != "Allow" {
            continue;
        }
        for res in &perm.resources {
            // Full wildcard Allow.
            if res == "*" {
                return true;
            }
            // Policy grants the entire bucket (or bucket-level).
            if res == bucket {
                return true;
            }
            if res == &format!("{}/*", bucket) {
                return true;
            }
            // Policy exactly matches the requested bucket+prefix OR is a
            // wider pattern that covers it (e.g. "bucket/alice*" covers
            // "bucket/alice/file.txt").
            if let Some(stripped) = res.strip_suffix('*') {
                if requested_path.starts_with(stripped) {
                    return true;
                }
            }
            if res == &requested_path {
                return true;
            }
        }
    }
    false
}

/// Pure predicate: can this user see this key in a LIST response?
///
/// Used by the bucket handler to post-filter the engine's list output
/// when the middleware set `ListScope::Filtered`. The rule: a user may
/// see a key if they have Read OR List permission on it — List alone
/// is enough because it's the same verb that gated the request itself,
/// and Read is a strict superset in intent (if you can read it, you
/// can also see it in the listing).
///
/// Kept as a free function (not a method) because:
/// - It's a lookup used on a hot loop over potentially thousands of
///   keys; `AuthenticatedUser::can` re-evaluates the full policy graph
///   each call, and inlining here lets LLVM consolidate the two checks.
/// - It's unit-testable as a pure function without an HTTP stack.
pub fn user_can_see_listed_key(
    user: &super::AuthenticatedUser,
    bucket: &str,
    key: &str,
) -> bool {
    user.can(S3Action::Read, bucket, key) || user.can(S3Action::List, bucket, key)
}

/// Check if a permission set grants full admin access:
/// actions must contain "*" or "admin", AND resources must contain "*".
/// Respects Deny overrides.
pub(crate) fn is_admin(permissions: &[Permission]) -> bool {
    let has_deny = permissions.iter().any(|p| {
        p.effect == "Deny"
            && p.actions.iter().any(|a| a == "*" || a == "admin")
            && p.resources.iter().any(|r| r == "*")
    });
    if has_deny {
        return false;
    }

    permissions.iter().any(|p| {
        p.effect == "Allow"
            && p.actions.iter().any(|a| a == "*" || a == "admin")
            && p.resources.iter().any(|r| r == "*")
    })
}

/// Check whether a permission rule matches the given action and resource.
fn matches_action_and_resource(
    perm: &Permission,
    action_str: &str,
    bucket: &str,
    key: &str,
) -> bool {
    let action_matches = perm.actions.iter().any(|a| a == "*" || a == action_str);
    if !action_matches {
        return false;
    }

    let resource = if key.is_empty() {
        bucket.to_string()
    } else {
        format!("{}/{}", bucket, key)
    };

    perm.resources
        .iter()
        .any(|pattern| matches_resource(pattern, &resource))
}

/// Match a resource string against a pattern.
/// Patterns: "bucket/*" (prefix + bucket-level), "bucket/exact" (exact), "*" (everything).
/// "bucket/*" also matches the bucket itself (for list operations).
fn matches_resource(pattern: &str, resource: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        if resource.starts_with(prefix) {
            return true;
        }
        if let Some(bucket_prefix) = prefix.strip_suffix('/') {
            return resource == bucket_prefix;
        }
        false
    } else {
        resource == pattern
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_iam_rs_basic_evaluation() {
        let perm = Permission {
            id: 0,
            effect: "Allow".into(),
            actions: vec!["read".into()],
            resources: vec!["*".into()],
            conditions: None,
        };
        let policy = permission_to_iam_policy(&perm);

        let result = evaluate_iam(
            &[policy],
            S3Action::Read,
            "bucket",
            "key",
            &Default::default(),
        );
        assert!(result, "iam-rs should allow read on * for bucket/key");
    }

    #[test]
    fn test_evaluate_iam_deny_with_prefix_condition() {
        // Deny list on beshu/* when s3:prefix starts with "."
        let allow = Permission {
            id: 0,
            effect: "Allow".into(),
            actions: vec!["read".into(), "list".into()],
            resources: vec!["beshu/ror/builds/*".into()],
            conditions: None,
        };
        // Resource is "beshu" (bucket-level) — s3:ListBucket operates on the bucket ARN,
        // not on objects. The s3:prefix condition restricts what can be listed.
        let deny = Permission {
            id: 1,
            effect: "Deny".into(),
            actions: vec!["list".into()],
            resources: vec!["beshu".into()],
            conditions: Some(serde_json::json!({"StringLike": {"s3:prefix": ".*"}})),
        };

        let policies: Vec<IAMPolicy> = [allow, deny].iter().map(permission_to_iam_policy).collect();

        // The middleware uses three checks for LIST:
        // 1. can_with_context() — evaluate_iam with context
        // 2. is_explicitly_denied() — check if a Deny rule matches
        // 3. can_see_bucket() — fallback for prefix-scoped allows
        //
        // For dotfile prefix: Deny matches (condition satisfied) → is_explicitly_denied = true → blocked
        // For ror/builds/ prefix: Deny doesn't match (condition not satisfied) → not explicitly denied → can_see_bucket allows

        let dotfile_ctx = Context::new().with_string("s3:prefix", ".deltaglider/");
        let builds_ctx = Context::new().with_string("s3:prefix", "ror/builds/");

        // Dotfile prefix: explicitly denied (Deny condition matches)
        assert!(
            is_explicitly_denied_iam(&policies, S3Action::List, "beshu", "", &dotfile_ctx),
            "dotfile prefix should trigger explicit Deny"
        );

        // Normal prefix: NOT explicitly denied (condition doesn't match)
        assert!(
            !is_explicitly_denied_iam(&policies, S3Action::List, "beshu", "", &builds_ctx),
            "ror/builds/ prefix should NOT trigger Deny"
        );

        // No prefix: NOT explicitly denied
        assert!(
            !is_explicitly_denied_iam(&policies, S3Action::List, "beshu", "", &Default::default()),
            "empty prefix should NOT trigger Deny"
        );
    }

    #[test]
    fn test_evaluate_iam_condition_not_matched_skips_rule() {
        // Deny with condition that doesn't match — rule should be skipped
        let allow = Permission {
            id: 0,
            effect: "Allow".into(),
            actions: vec!["*".into()],
            resources: vec!["*".into()],
            conditions: None,
        };
        let deny_with_condition = Permission {
            id: 1,
            effect: "Deny".into(),
            actions: vec!["*".into()],
            resources: vec!["*".into()],
            conditions: Some(serde_json::json!({"IpAddress": {"aws:SourceIp": "10.0.0.0/8"}})),
        };

        let policies: Vec<IAMPolicy> = [allow, deny_with_condition]
            .iter()
            .map(permission_to_iam_policy)
            .collect();

        // Request from 192.168.1.1 — doesn't match 10.0.0.0/8, so deny is skipped
        let ctx = Context::new().with_string("aws:SourceIp", "192.168.1.1");
        assert!(
            evaluate_iam(&policies, S3Action::Read, "bucket", "key", &ctx),
            "should be allowed — deny condition doesn't match"
        );

        // Request from 10.1.2.3 — matches 10.0.0.0/8, deny applies
        let ctx_deny = Context::new().with_string("aws:SourceIp", "10.1.2.3");
        assert!(
            !evaluate_iam(&policies, S3Action::Read, "bucket", "key", &ctx_deny),
            "should be denied — source IP matches deny condition"
        );
    }

    #[test]
    fn test_evaluate_allow_read() {
        let perms = vec![Permission {
            id: 0,
            effect: "Allow".into(),
            actions: vec!["read".into()],
            resources: vec!["releases/*".into()],
            conditions: None,
        }];

        assert!(evaluate(&perms, S3Action::Read, "releases", "v1.zip"));
        assert!(!evaluate(&perms, S3Action::Write, "releases", "v1.zip"));
        assert!(!evaluate(
            &perms,
            S3Action::Read,
            "other-bucket",
            "file.txt"
        ));
    }

    #[test]
    fn test_evaluate_wildcard_action() {
        let perms = vec![Permission {
            id: 0,
            effect: "Allow".into(),
            actions: vec!["*".into()],
            resources: vec!["*".into()],
            conditions: None,
        }];

        assert!(evaluate(&perms, S3Action::Read, "any", "key"));
        assert!(evaluate(&perms, S3Action::Delete, "any", "key"));
        assert!(evaluate(&perms, S3Action::Admin, "any", ""));
    }

    #[test]
    fn test_evaluate_no_permissions_denies() {
        let perms: Vec<Permission> = vec![];
        assert!(!evaluate(&perms, S3Action::Read, "bucket", "key"));
    }

    #[test]
    fn test_evaluate_multiple_rules() {
        let perms = vec![
            Permission {
                id: 0,
                effect: "Allow".into(),
                actions: vec!["read".into(), "list".into()],
                resources: vec!["releases/*".into()],
                conditions: None,
            },
            Permission {
                id: 0,
                effect: "Allow".into(),
                actions: vec!["write".into()],
                resources: vec!["uploads/*".into()],
                conditions: None,
            },
        ];

        assert!(evaluate(&perms, S3Action::Read, "releases", "v1.zip"));
        assert!(evaluate(&perms, S3Action::List, "releases", ""));
        assert!(evaluate(&perms, S3Action::Write, "uploads", "file.bin"));
        assert!(!evaluate(&perms, S3Action::Write, "releases", "v1.zip"));
        assert!(!evaluate(&perms, S3Action::Delete, "releases", "v1.zip"));
    }

    #[test]
    fn test_evaluate_exact_resource() {
        let perms = vec![Permission {
            id: 0,
            effect: "Allow".into(),
            actions: vec!["read".into()],
            resources: vec!["specific-bucket/exact-key.txt".into()],
            conditions: None,
        }];

        assert!(evaluate(
            &perms,
            S3Action::Read,
            "specific-bucket",
            "exact-key.txt"
        ));
        assert!(!evaluate(
            &perms,
            S3Action::Read,
            "specific-bucket",
            "other-key.txt"
        ));
    }

    #[test]
    fn test_evaluate_bucket_level() {
        let perms = vec![Permission {
            id: 0,
            effect: "Allow".into(),
            actions: vec!["list".into()],
            resources: vec!["my-bucket".into()],
            conditions: None,
        }];

        assert!(evaluate(&perms, S3Action::List, "my-bucket", ""));
        assert!(!evaluate(&perms, S3Action::List, "my-bucket", "prefix/"));
    }

    #[test]
    fn test_evaluate_bucket_wildcard() {
        let perms = vec![Permission {
            id: 0,
            effect: "Allow".into(),
            actions: vec!["list".into(), "read".into()],
            resources: vec!["my-bucket/*".into()],
            conditions: None,
        }];

        assert!(evaluate(&perms, S3Action::Read, "my-bucket", "file.txt"));
        assert!(evaluate(&perms, S3Action::List, "my-bucket", ""));
    }

    #[test]
    fn test_matches_resource_patterns() {
        assert!(matches_resource("*", "anything/at/all"));
        assert!(matches_resource("releases/*", "releases/v1.zip"));
        assert!(matches_resource("releases/*", "releases/sub/dir/file"));
        assert!(!matches_resource("releases/*", "other/file"));
        assert!(matches_resource("exact", "exact"));
        assert!(!matches_resource("exact", "not-exact"));
    }

    #[test]
    fn test_deny_overrides_allow() {
        let perms = vec![
            Permission {
                id: 0,
                effect: "Allow".into(),
                actions: vec!["*".into()],
                resources: vec!["*".into()],
                conditions: None,
            },
            Permission {
                id: 1,
                effect: "Deny".into(),
                actions: vec!["delete".into()],
                resources: vec!["releases/*".into()],
                conditions: None,
            },
        ];

        assert!(evaluate(&perms, S3Action::Read, "releases", "v1.zip"));
        assert!(!evaluate(&perms, S3Action::Delete, "releases", "v1.zip"));
        assert!(evaluate(&perms, S3Action::Delete, "uploads", "file.bin"));
    }

    #[test]
    fn test_deny_all_blocks_everything() {
        let perms = vec![
            Permission {
                id: 0,
                effect: "Allow".into(),
                actions: vec!["*".into()],
                resources: vec!["*".into()],
                conditions: None,
            },
            Permission {
                id: 1,
                effect: "Deny".into(),
                actions: vec!["*".into()],
                resources: vec!["*".into()],
                conditions: None,
            },
        ];

        assert!(!evaluate(&perms, S3Action::Read, "any", "key"));
        assert!(!evaluate(&perms, S3Action::Write, "any", "key"));
        assert!(!evaluate(&perms, S3Action::Delete, "any", "key"));
        assert!(!evaluate(&perms, S3Action::List, "any", ""));
        assert!(!evaluate(&perms, S3Action::Admin, "any", ""));
    }

    #[test]
    fn test_allow_without_deny() {
        let perms = vec![Permission {
            id: 0,
            effect: "Allow".into(),
            actions: vec!["read".into(), "list".into()],
            resources: vec!["*".into()],
            conditions: None,
        }];

        assert!(evaluate(&perms, S3Action::Read, "bucket", "key"));
        assert!(evaluate(&perms, S3Action::List, "bucket", ""));
        assert!(!evaluate(&perms, S3Action::Write, "bucket", "key"));
        assert!(!evaluate(&perms, S3Action::Delete, "bucket", "key"));
    }

    #[test]
    fn test_mixed_deny_allow() {
        let perms = vec![
            Permission {
                id: 0,
                effect: "Allow".into(),
                actions: vec!["read".into(), "write".into(), "list".into()],
                resources: vec!["*".into()],
                conditions: None,
            },
            Permission {
                id: 1,
                effect: "Deny".into(),
                actions: vec!["write".into()],
                resources: vec!["releases/*".into()],
                conditions: None,
            },
        ];

        assert!(evaluate(&perms, S3Action::Read, "releases", "v1.zip"));
        assert!(!evaluate(&perms, S3Action::Write, "releases", "v1.zip"));
        assert!(evaluate(&perms, S3Action::Write, "uploads", "file.bin"));
        assert!(evaluate(&perms, S3Action::List, "releases", ""));
    }

    #[test]
    fn test_is_admin_with_allow() {
        let perms = vec![Permission {
            id: 0,
            effect: "Allow".into(),
            actions: vec!["*".into()],
            resources: vec!["*".into()],
            conditions: None,
        }];
        assert!(is_admin(&perms));
    }

    #[test]
    fn test_is_admin_denied_by_deny_rule() {
        let perms = vec![
            Permission {
                id: 0,
                effect: "Allow".into(),
                actions: vec!["*".into()],
                resources: vec!["*".into()],
                conditions: None,
            },
            Permission {
                id: 1,
                effect: "Deny".into(),
                actions: vec!["admin".into()],
                resources: vec!["*".into()],
                conditions: None,
            },
        ];
        assert!(!is_admin(&perms));
    }

    #[test]
    fn test_is_admin_not_admin_without_wildcard_resource() {
        let perms = vec![Permission {
            id: 0,
            effect: "Allow".into(),
            actions: vec!["*".into()],
            resources: vec!["my-bucket/*".into()],
            conditions: None,
        }];
        assert!(!is_admin(&perms));
    }

    #[test]
    fn test_has_any_on_bucket() {
        let perms = vec![Permission {
            id: 0,
            effect: "Allow".into(),
            actions: vec!["read".into()],
            resources: vec!["my-bucket/prefix/*".into()],
            conditions: None,
        }];
        assert!(has_any_on_bucket(&perms, "my-bucket"));
        assert!(!has_any_on_bucket(&perms, "other-bucket"));
    }

    #[test]
    fn test_has_any_on_bucket_wildcard() {
        let perms = vec![Permission {
            id: 0,
            effect: "Allow".into(),
            actions: vec!["read".into()],
            resources: vec!["*".into()],
            conditions: None,
        }];
        assert!(has_any_on_bucket(&perms, "any-bucket"));
    }

    // === has_unrestricted_allow_for_bucket_prefix — C1 security fix ===

    #[test]
    fn test_unrestricted_wildcard_is_unrestricted() {
        let perms = vec![Permission {
            id: 0,
            effect: "Allow".into(),
            actions: vec!["read".into()],
            resources: vec!["*".into()],
            conditions: None,
        }];
        assert!(has_unrestricted_allow_for_bucket_prefix(&perms, "anything", ""));
        assert!(has_unrestricted_allow_for_bucket_prefix(&perms, "anything", "pfx"));
    }

    #[test]
    fn test_unrestricted_full_bucket_slash_star() {
        let perms = vec![Permission {
            id: 0,
            effect: "Allow".into(),
            actions: vec!["read".into()],
            resources: vec!["prod/*".into()],
            conditions: None,
        }];
        assert!(has_unrestricted_allow_for_bucket_prefix(&perms, "prod", ""));
        assert!(has_unrestricted_allow_for_bucket_prefix(&perms, "prod", "alice/"));
        assert!(!has_unrestricted_allow_for_bucket_prefix(&perms, "other", ""));
    }

    #[test]
    fn test_prefix_scoped_is_not_unrestricted_for_empty_prefix() {
        // Classic C1 attack: policy grants "prod/alice/*", user calls LIST
        // with empty prefix. Must NOT be treated as unrestricted.
        let perms = vec![Permission {
            id: 0,
            effect: "Allow".into(),
            actions: vec!["read".into()],
            resources: vec!["prod/alice/*".into()],
            conditions: None,
        }];
        assert!(!has_unrestricted_allow_for_bucket_prefix(&perms, "prod", ""));
    }

    #[test]
    fn test_prefix_scoped_is_unrestricted_for_matching_prefix() {
        // Same policy but narrower LIST — requesting the exact prefix
        // the policy allows. User SHOULD get unrestricted within that scope.
        let perms = vec![Permission {
            id: 0,
            effect: "Allow".into(),
            actions: vec!["read".into()],
            resources: vec!["prod/alice/*".into()],
            conditions: None,
        }];
        assert!(has_unrestricted_allow_for_bucket_prefix(
            &perms,
            "prod",
            "alice/"
        ));
        assert!(has_unrestricted_allow_for_bucket_prefix(
            &perms,
            "prod",
            "alice/sub"
        ));
    }

    #[test]
    fn test_prefix_scoped_is_not_unrestricted_for_sibling_prefix() {
        // Policy grants alice's tree, user asks about bob's — must NOT
        // short-circuit as unrestricted.
        let perms = vec![Permission {
            id: 0,
            effect: "Allow".into(),
            actions: vec!["read".into()],
            resources: vec!["prod/alice/*".into()],
            conditions: None,
        }];
        assert!(!has_unrestricted_allow_for_bucket_prefix(
            &perms,
            "prod",
            "bob/"
        ));
    }

    #[test]
    fn test_deny_rule_does_not_claim_unrestricted() {
        // Bare Deny by itself never grants unrestricted access.
        let perms = vec![Permission {
            id: 0,
            effect: "Deny".into(),
            actions: vec!["read".into()],
            resources: vec!["*".into()],
            conditions: None,
        }];
        assert!(!has_unrestricted_allow_for_bucket_prefix(&perms, "x", ""));
    }

    // === user_can_see_listed_key integration with AuthenticatedUser ===

    fn make_user(name: &str, resources: Vec<&str>, actions: Vec<&str>) -> super::super::AuthenticatedUser {
        let permissions = vec![Permission {
            id: 0,
            effect: "Allow".into(),
            actions: actions.iter().map(|s| s.to_string()).collect(),
            resources: resources.iter().map(|s| s.to_string()).collect(),
            conditions: None,
        }];
        let iam_policies = permissions.iter().map(permission_to_iam_policy).collect();
        super::super::AuthenticatedUser {
            name: name.to_string(),
            access_key_id: "AKIA".into(),
            permissions,
            iam_policies,
        }
    }

    #[test]
    fn test_user_can_see_listed_key_unrestricted_user() {
        let user = make_user("alice", vec!["prod/*"], vec!["read", "list"]);
        assert!(user_can_see_listed_key(&user, "prod", "alice/file.txt"));
        assert!(user_can_see_listed_key(&user, "prod", "anything/else.bin"));
    }

    #[test]
    fn test_user_can_see_listed_key_prefix_scoped() {
        let user = make_user("alice", vec!["prod/alice/*"], vec!["read"]);
        assert!(user_can_see_listed_key(&user, "prod", "alice/file.txt"));
        assert!(!user_can_see_listed_key(&user, "prod", "bob/file.txt"));
        assert!(!user_can_see_listed_key(&user, "prod", "secret.bin"));
    }

    #[test]
    fn test_user_can_see_listed_key_list_only() {
        // List-only permission should let the user see keys in listings
        // (they can't Read them, but they can discover them).
        let user = make_user("alice", vec!["prod/public/*"], vec!["list"]);
        assert!(user_can_see_listed_key(&user, "prod", "public/x.txt"));
        assert!(!user_can_see_listed_key(&user, "prod", "private/x.txt"));
    }

    #[test]
    fn test_user_cannot_see_keys_in_different_bucket() {
        let user = make_user("alice", vec!["prod/*"], vec!["read", "list"]);
        assert!(!user_can_see_listed_key(&user, "staging", "anything"));
    }

    // === Property-based tests ===

    fn random_action(i: usize) -> S3Action {
        match i % 5 {
            0 => S3Action::Read,
            1 => S3Action::Write,
            2 => S3Action::Delete,
            3 => S3Action::List,
            _ => S3Action::Admin,
        }
    }

    fn random_bucket(i: usize) -> &'static str {
        match i % 4 {
            0 => "alpha",
            1 => "beta",
            2 => "gamma",
            _ => "delta",
        }
    }

    fn random_key(i: usize) -> &'static str {
        match i % 5 {
            0 => "file.zip",
            1 => "builds/v1.zip",
            2 => "releases/latest.tar.gz",
            3 => "",
            _ => "deep/nested/path/file.bin",
        }
    }

    #[test]
    fn prop_deny_always_overrides_allow() {
        for i in 0..100 {
            let action = random_action(i);
            let bucket = random_bucket(i);
            let key = random_key(i);
            let action_str = action.as_str().to_string();

            let perms = vec![
                Permission {
                    id: 0,
                    effect: "Allow".into(),
                    actions: vec!["*".into()],
                    resources: vec!["*".into()],
                    conditions: None,
                },
                Permission {
                    id: 1,
                    effect: "Allow".into(),
                    actions: vec![action_str.clone()],
                    resources: vec![format!("{}/*", bucket)],
                    conditions: None,
                },
                Permission {
                    id: 2,
                    effect: "Deny".into(),
                    actions: vec!["*".into()],
                    resources: vec!["*".into()],
                    conditions: None,
                },
            ];

            assert!(
                !evaluate(&perms, action, bucket, key),
                "Deny * should override Allow * for action={:?} bucket={} key={}",
                action_str,
                bucket,
                key
            );
        }
    }

    #[test]
    fn prop_no_matching_rule_means_deny() {
        for i in 0..100 {
            let action = random_action(i);
            let bucket = random_bucket(i);
            let key = random_key(i);

            let perms = vec![Permission {
                id: 0,
                effect: "Allow".into(),
                actions: vec!["read".into()],
                resources: vec!["nonexistent-bucket/*".into()],
                conditions: None,
            }];

            if action.as_str() != "read" || bucket != "nonexistent-bucket" {
                assert!(
                    !evaluate(&perms, action, bucket, key),
                    "Non-matching rule should deny: action={} bucket={} key={}",
                    action.as_str(),
                    bucket,
                    key
                );
            }
        }
    }

    #[test]
    fn prop_wildcard_allow_permits_everything_without_deny() {
        let perms = vec![Permission {
            id: 0,
            effect: "Allow".into(),
            actions: vec!["*".into()],
            resources: vec!["*".into()],
            conditions: None,
        }];

        for i in 0..100 {
            let action = random_action(i);
            let bucket = random_bucket(i);
            let key = random_key(i);

            assert!(
                evaluate(&perms, action, bucket, key),
                "Allow * should permit action={} bucket={} key={}",
                action.as_str(),
                bucket,
                key
            );
        }
    }

    #[test]
    fn prop_specific_deny_only_blocks_matching_action() {
        for i in 0..100 {
            let action = random_action(i);
            let bucket = random_bucket(i);
            let key = random_key(i);

            let perms = vec![
                Permission {
                    id: 0,
                    effect: "Allow".into(),
                    actions: vec!["*".into()],
                    resources: vec!["*".into()],
                    conditions: None,
                },
                Permission {
                    id: 1,
                    effect: "Deny".into(),
                    actions: vec!["delete".into()],
                    resources: vec!["*".into()],
                    conditions: None,
                },
            ];

            let result = evaluate(&perms, action, bucket, key);

            if action.as_str() == "delete" {
                assert!(
                    !result,
                    "Deny delete should block delete on {}/{}",
                    bucket, key
                );
            } else {
                assert!(
                    result,
                    "Deny delete should NOT block {} on {}/{}",
                    action.as_str(),
                    bucket,
                    key
                );
            }
        }
    }

    #[test]
    fn prop_resource_scoping_respected() {
        let perms = vec![Permission {
            id: 0,
            effect: "Allow".into(),
            actions: vec!["*".into()],
            resources: vec!["alpha/*".into()],
            conditions: None,
        }];

        for i in 0..100 {
            let action = random_action(i);
            let key = random_key(i);

            assert!(
                evaluate(&perms, action, "alpha", key),
                "Should allow {} on alpha/{}",
                action.as_str(),
                key
            );

            for other_bucket in &["beta", "gamma", "delta"] {
                if !key.is_empty() {
                    assert!(
                        !evaluate(&perms, action, other_bucket, key),
                        "Should deny {} on {}/{}",
                        action.as_str(),
                        other_bucket,
                        key
                    );
                }
            }
        }
    }

    // === Validation tests ===

    #[test]
    fn test_validate_valid_permissions() {
        let perms = vec![
            Permission {
                id: 0,
                effect: "Allow".into(),
                actions: vec!["read".into(), "list".into()],
                resources: vec!["bucket/*".into()],
                conditions: None,
            },
            Permission {
                id: 0,
                effect: "Deny".into(),
                actions: vec!["delete".into()],
                resources: vec!["*".into()],
                conditions: None,
            },
        ];
        assert!(validate_permissions(&perms).is_ok());
    }

    #[test]
    fn test_validate_empty_permissions_ok() {
        assert!(validate_permissions(&[]).is_ok());
    }

    #[test]
    fn test_validate_rejects_invalid_effect() {
        let perms = vec![Permission {
            id: 0,
            effect: "Maybe".into(),
            actions: vec!["read".into()],
            resources: vec!["*".into()],
            conditions: None,
        }];
        let err = validate_permissions(&perms).unwrap_err();
        assert!(err.contains("effect"), "error: {}", err);
    }

    #[test]
    fn test_validate_rejects_invalid_action() {
        let perms = vec![Permission {
            id: 0,
            effect: "Allow".into(),
            actions: vec!["readwrite".into()],
            resources: vec!["*".into()],
            conditions: None,
        }];
        let err = validate_permissions(&perms).unwrap_err();
        assert!(err.contains("invalid action"), "error: {}", err);
    }

    #[test]
    fn test_validate_rejects_empty_actions() {
        let perms = vec![Permission {
            id: 0,
            effect: "Allow".into(),
            actions: vec![],
            resources: vec!["*".into()],
            conditions: None,
        }];
        let err = validate_permissions(&perms).unwrap_err();
        assert!(err.contains("actions must not be empty"), "error: {}", err);
    }

    #[test]
    fn test_validate_rejects_empty_resources() {
        let perms = vec![Permission {
            id: 0,
            effect: "Allow".into(),
            actions: vec!["read".into()],
            resources: vec![],
            conditions: None,
        }];
        let err = validate_permissions(&perms).unwrap_err();
        assert!(
            err.contains("resources must not be empty"),
            "error: {}",
            err
        );
    }

    #[test]
    fn test_validate_rejects_mid_wildcard() {
        let perms = vec![Permission {
            id: 0,
            effect: "Allow".into(),
            actions: vec!["read".into()],
            resources: vec!["bucket/*/files".into()],
            conditions: None,
        }];
        let err = validate_permissions(&perms).unwrap_err();
        assert!(err.contains("invalid position"), "error: {}", err);
    }

    #[test]
    fn test_validate_rejects_double_wildcard() {
        let perms = vec![Permission {
            id: 0,
            effect: "Allow".into(),
            actions: vec!["read".into()],
            resources: vec!["bucket/*.*".into()],
            conditions: None,
        }];
        let err = validate_permissions(&perms).unwrap_err();
        assert!(err.contains("invalid position"), "error: {}", err);
    }

    #[test]
    fn test_validate_rejects_whitespace_in_resource() {
        let perms = vec![Permission {
            id: 0,
            effect: "Allow".into(),
            actions: vec!["read".into()],
            resources: vec!["bucket /prefix/*".into()],
            conditions: None,
        }];
        let err = validate_permissions(&perms).unwrap_err();
        assert!(err.contains("whitespace"), "error: {}", err);
    }

    #[test]
    fn test_validate_accepts_valid_patterns() {
        for pattern in &["*", "bucket/*", "bucket/prefix/*", "bucket/exact.txt"] {
            let perms = vec![Permission {
                id: 0,
                effect: "Allow".into(),
                actions: vec!["read".into()],
                resources: vec![pattern.to_string()],
                conditions: None,
            }];
            assert!(
                validate_permissions(&perms).is_ok(),
                "pattern '{}' should be valid",
                pattern
            );
        }
    }
}
