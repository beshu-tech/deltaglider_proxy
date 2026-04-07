//! Group mapping evaluation: maps external identity attributes to local group IDs.

use crate::config_db::auth_providers::GroupMappingRule;

use super::types::ExternalIdentityInfo;

/// Evaluate all matching group mapping rules for an external identity.
/// Returns a deduplicated list of group IDs the identity should be assigned to.
///
/// Rules are evaluated in priority order (already sorted by ConfigDb).
/// All matching rules contribute (union of groups, not first-match-wins).
pub fn evaluate_mappings(
    rules: &[GroupMappingRule],
    identity: &ExternalIdentityInfo,
    provider_id: i64,
) -> Vec<i64> {
    collect_matching_groups(rules, identity, Some(provider_id))
}

/// Preview which groups an email would match against.
/// Used by the admin UI preview feature. Does not require a full identity —
/// constructs a minimal one from just the email.
pub fn preview_email_mappings(rules: &[GroupMappingRule], email: &str) -> Vec<i64> {
    let identity = ExternalIdentityInfo {
        subject: String::new(),
        email: Some(email.to_string()),
        email_verified: true,
        name: None,
        groups: Vec::new(),
        raw_claims: serde_json::json!({"email": email}),
    };

    // Preview evaluates all rules (provider_id filter skipped)
    collect_matching_groups(rules, &identity, None)
}

/// Core accumulation: iterate rules, apply optional provider_id filter,
/// check match, and collect deduplicated group IDs.
fn collect_matching_groups(
    rules: &[GroupMappingRule],
    identity: &ExternalIdentityInfo,
    provider_id_filter: Option<i64>,
) -> Vec<i64> {
    let mut group_ids: Vec<i64> = Vec::new();

    for rule in rules {
        // Skip rules scoped to a different provider (when filtering)
        if let (Some(filter_id), Some(rule_provider_id)) = (provider_id_filter, rule.provider_id) {
            if rule_provider_id != filter_id {
                continue;
            }
        }

        if matches_rule(rule, identity) && !group_ids.contains(&rule.group_id) {
            group_ids.push(rule.group_id);
        }
    }

    group_ids
}

fn matches_rule(rule: &GroupMappingRule, identity: &ExternalIdentityInfo) -> bool {
    match rule.match_type.as_str() {
        "email_exact" => identity
            .email
            .as_deref()
            .map(|e| e.eq_ignore_ascii_case(&rule.match_value))
            .unwrap_or(false),
        "email_domain" => identity
            .email
            .as_deref()
            .map(|e| {
                let suffix = format!("@{}", rule.match_value);
                e.to_ascii_lowercase()
                    .ends_with(&suffix.to_ascii_lowercase())
            })
            .unwrap_or(false),
        "email_glob" => {
            // Simple wildcard matching: * matches any sequence of characters.
            // e.g., "*@acme.com", "*.engineering@acme.com", "alice@*"
            identity
                .email
                .as_deref()
                .map(|e| {
                    glob_match(
                        &rule.match_value.to_ascii_lowercase(),
                        &e.to_ascii_lowercase(),
                    )
                })
                .unwrap_or(false)
        }
        "email_regex" => {
            // Regex matching — compiled per evaluation. For high-volume use cases
            // consider caching, but mapping evaluation happens once per login.
            identity
                .email
                .as_deref()
                .and_then(|e| {
                    regex::Regex::new(&rule.match_value)
                        .ok()
                        .map(|re| re.is_match(e))
                })
                .unwrap_or(false)
        }
        "claim_value" => {
            // Look up a specific claim field in raw_claims and check if it
            // contains the match_value (supports both string and array claims).
            match identity.raw_claims.get(&rule.match_field) {
                Some(serde_json::Value::String(s)) => s.eq_ignore_ascii_case(&rule.match_value),
                Some(serde_json::Value::Array(arr)) => arr.iter().any(|v| {
                    v.as_str()
                        .map(|s| s.eq_ignore_ascii_case(&rule.match_value))
                        .unwrap_or(false)
                }),
                _ => false,
            }
        }
        _ => {
            tracing::warn!(
                "Unknown match_type '{}' in mapping rule {}",
                rule.match_type,
                rule.id
            );
            false
        }
    }
}

/// Simple glob matching: `*` matches any sequence of characters.
/// Used for email patterns like `*@acme.com` or `*.eng@acme.com`.
fn glob_match(pattern: &str, text: &str) -> bool {
    glob_match_recursive(
        &pattern.chars().collect::<Vec<_>>(),
        &text.chars().collect::<Vec<_>>(),
        0,
        0,
    )
}

fn glob_match_recursive(pattern: &[char], text: &[char], pi: usize, ti: usize) -> bool {
    if pi == pattern.len() {
        return ti == text.len();
    }
    if pattern[pi] == '*' {
        // * matches zero or more characters
        // Try matching rest of pattern at every position from ti to end
        for i in ti..=text.len() {
            if glob_match_recursive(pattern, text, pi + 1, i) {
                return true;
            }
        }
        false
    } else if ti < text.len() && pattern[pi] == text[ti] {
        glob_match_recursive(pattern, text, pi + 1, ti + 1)
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_identity(email: &str, claims: serde_json::Value) -> ExternalIdentityInfo {
        ExternalIdentityInfo {
            subject: "sub-123".into(),
            email: Some(email.into()),
            email_verified: true,
            name: Some("Test User".into()),
            groups: vec![],
            raw_claims: claims,
        }
    }

    fn make_rule(
        id: i64,
        provider_id: Option<i64>,
        match_type: &str,
        match_field: &str,
        match_value: &str,
        group_id: i64,
    ) -> GroupMappingRule {
        GroupMappingRule {
            id,
            provider_id,
            priority: 0,
            match_type: match_type.into(),
            match_field: match_field.into(),
            match_value: match_value.into(),
            group_id,
            created_at: String::new(),
        }
    }

    #[test]
    fn test_email_domain_match() {
        let rules = vec![make_rule(
            1,
            None,
            "email_domain",
            "email",
            "company.com",
            10,
        )];
        let identity = make_identity("alice@company.com", serde_json::json!({}));
        let groups = evaluate_mappings(&rules, &identity, 1);
        assert_eq!(groups, vec![10]);
    }

    #[test]
    fn test_email_domain_no_match() {
        let rules = vec![make_rule(
            1,
            None,
            "email_domain",
            "email",
            "company.com",
            10,
        )];
        let identity = make_identity("alice@other.com", serde_json::json!({}));
        let groups = evaluate_mappings(&rules, &identity, 1);
        assert!(groups.is_empty());
    }

    #[test]
    fn test_email_exact_match() {
        let rules = vec![make_rule(
            1,
            None,
            "email_exact",
            "email",
            "admin@co.com",
            20,
        )];
        let identity = make_identity("admin@co.com", serde_json::json!({}));
        let groups = evaluate_mappings(&rules, &identity, 1);
        assert_eq!(groups, vec![20]);
    }

    #[test]
    fn test_email_exact_case_insensitive() {
        let rules = vec![make_rule(
            1,
            None,
            "email_exact",
            "email",
            "Admin@Co.Com",
            20,
        )];
        let identity = make_identity("admin@co.com", serde_json::json!({}));
        let groups = evaluate_mappings(&rules, &identity, 1);
        assert_eq!(groups, vec![20]);
    }

    #[test]
    fn test_email_regex_match() {
        let rules = vec![make_rule(
            1,
            None,
            "email_regex",
            "email",
            r"^.*@(eng|dev)\.company\.com$",
            30,
        )];
        let identity = make_identity("alice@eng.company.com", serde_json::json!({}));
        let groups = evaluate_mappings(&rules, &identity, 1);
        assert_eq!(groups, vec![30]);
    }

    #[test]
    fn test_claim_value_string() {
        let rules = vec![make_rule(1, None, "claim_value", "hd", "company.com", 40)];
        let identity = make_identity(
            "alice@company.com",
            serde_json::json!({"hd": "company.com"}),
        );
        let groups = evaluate_mappings(&rules, &identity, 1);
        assert_eq!(groups, vec![40]);
    }

    #[test]
    fn test_claim_value_array() {
        let rules = vec![make_rule(
            1,
            None,
            "claim_value",
            "groups",
            "engineering",
            50,
        )];
        let identity = make_identity(
            "alice@company.com",
            serde_json::json!({"groups": ["engineering", "frontend"]}),
        );
        let groups = evaluate_mappings(&rules, &identity, 1);
        assert_eq!(groups, vec![50]);
    }

    #[test]
    fn test_multiple_rules_union() {
        let rules = vec![
            make_rule(1, None, "email_domain", "email", "company.com", 10),
            make_rule(2, None, "email_exact", "email", "alice@company.com", 20),
        ];
        let identity = make_identity("alice@company.com", serde_json::json!({}));
        let groups = evaluate_mappings(&rules, &identity, 1);
        assert_eq!(groups, vec![10, 20]);
    }

    #[test]
    fn test_no_duplicate_groups() {
        let rules = vec![
            make_rule(1, None, "email_domain", "email", "company.com", 10),
            make_rule(2, None, "email_exact", "email", "alice@company.com", 10),
        ];
        let identity = make_identity("alice@company.com", serde_json::json!({}));
        let groups = evaluate_mappings(&rules, &identity, 1);
        assert_eq!(groups, vec![10]);
    }

    #[test]
    fn test_provider_scoping() {
        let rules = vec![
            make_rule(1, Some(100), "email_domain", "email", "company.com", 10),
            make_rule(2, Some(200), "email_domain", "email", "company.com", 20),
        ];
        let identity = make_identity("alice@company.com", serde_json::json!({}));

        // Only provider 100's rule matches
        let groups = evaluate_mappings(&rules, &identity, 100);
        assert_eq!(groups, vec![10]);

        // Only provider 200's rule matches
        let groups = evaluate_mappings(&rules, &identity, 200);
        assert_eq!(groups, vec![20]);
    }

    #[test]
    fn test_preview_email() {
        let rules = vec![
            make_rule(1, Some(100), "email_domain", "email", "company.com", 10),
            make_rule(2, None, "email_domain", "email", "company.com", 20),
        ];
        // Preview ignores provider scoping
        let groups = preview_email_mappings(&rules, "bob@company.com");
        assert_eq!(groups, vec![10, 20]);
    }

    // ── Corner case tests ──

    #[test]
    fn test_email_domain_subdomain_no_false_positive() {
        // "alice@notcompany.com" must NOT match domain "company.com"
        let rules = vec![make_rule(
            1,
            None,
            "email_domain",
            "email",
            "company.com",
            10,
        )];
        let identity = make_identity("alice@notcompany.com", serde_json::json!({}));
        let groups = evaluate_mappings(&rules, &identity, 1);
        assert!(
            groups.is_empty(),
            "notcompany.com should not match domain company.com"
        );
    }

    #[test]
    fn test_email_domain_case_insensitive() {
        let rules = vec![make_rule(
            1,
            None,
            "email_domain",
            "email",
            "COMPANY.COM",
            10,
        )];
        let identity = make_identity("alice@company.com", serde_json::json!({}));
        let groups = evaluate_mappings(&rules, &identity, 1);
        assert_eq!(groups, vec![10]);
    }

    #[test]
    fn test_email_exact_no_partial_match() {
        let rules = vec![make_rule(
            1,
            None,
            "email_exact",
            "email",
            "alice@company.com",
            10,
        )];
        // Suffix attack: "alice@company.com.evil.com"
        let identity = make_identity("alice@company.com.evil.com", serde_json::json!({}));
        let groups = evaluate_mappings(&rules, &identity, 1);
        assert!(groups.is_empty(), "Partial email match should not succeed");
    }

    #[test]
    fn test_email_regex_invalid_pattern_no_panic() {
        let rules = vec![make_rule(
            1,
            None,
            "email_regex",
            "email",
            "[invalid regex",
            10,
        )];
        let identity = make_identity("alice@company.com", serde_json::json!({}));
        // Should not panic, just returns no match
        let groups = evaluate_mappings(&rules, &identity, 1);
        assert!(groups.is_empty());
    }

    #[test]
    fn test_email_regex_anchored_correctly() {
        let rules = vec![make_rule(1, None, "email_regex", "email", r"^admin@", 10)];
        let admin = make_identity("admin@co.com", serde_json::json!({}));
        let not_admin = make_identity("notadmin@co.com", serde_json::json!({}));
        assert_eq!(evaluate_mappings(&rules, &admin, 1), vec![10]);
        assert!(evaluate_mappings(&rules, &not_admin, 1).is_empty());
    }

    #[test]
    fn test_claim_value_missing_field() {
        let rules = vec![make_rule(
            1,
            None,
            "claim_value",
            "department",
            "engineering",
            10,
        )];
        // Claims have no "department" key
        let identity = make_identity("alice@co.com", serde_json::json!({"email": "alice@co.com"}));
        let groups = evaluate_mappings(&rules, &identity, 1);
        assert!(groups.is_empty());
    }

    #[test]
    fn test_claim_value_nested_not_matched() {
        // Nested objects are not string/array, so should not match
        let rules = vec![make_rule(1, None, "claim_value", "org", "Acme", 10)];
        let identity = make_identity("a@co.com", serde_json::json!({"org": {"name": "Acme"}}));
        let groups = evaluate_mappings(&rules, &identity, 1);
        assert!(
            groups.is_empty(),
            "Nested object should not match as string"
        );
    }

    #[test]
    fn test_no_email_in_identity() {
        let identity = ExternalIdentityInfo {
            subject: "sub-123".into(),
            email: None,
            email_verified: false,
            name: None,
            groups: vec![],
            raw_claims: serde_json::json!({}),
        };
        let rules = vec![
            make_rule(1, None, "email_domain", "email", "company.com", 10),
            make_rule(2, None, "email_exact", "email", "alice@co.com", 20),
            make_rule(3, None, "email_regex", "email", ".*", 30),
        ];
        let groups = evaluate_mappings(&rules, &identity, 1);
        assert!(groups.is_empty(), "No email should match no email rules");
    }

    #[test]
    fn test_empty_rules_returns_empty() {
        let identity = make_identity("alice@co.com", serde_json::json!({}));
        let groups = evaluate_mappings(&[], &identity, 1);
        assert!(groups.is_empty());
    }

    // ── email_glob tests ──

    #[test]
    fn test_email_glob_star_at_domain() {
        let rules = vec![make_rule(1, None, "email_glob", "email", "*@acme.com", 10)];
        assert_eq!(
            evaluate_mappings(
                &rules,
                &make_identity("alice@acme.com", serde_json::json!({})),
                1
            ),
            vec![10]
        );
        assert_eq!(
            evaluate_mappings(
                &rules,
                &make_identity("bob@acme.com", serde_json::json!({})),
                1
            ),
            vec![10]
        );
        assert!(evaluate_mappings(
            &rules,
            &make_identity("alice@other.com", serde_json::json!({})),
            1
        )
        .is_empty());
    }

    #[test]
    fn test_email_glob_subdomain_wildcard() {
        let rules = vec![make_rule(
            1,
            None,
            "email_glob",
            "email",
            "*.engineering@acme.com",
            10,
        )];
        assert_eq!(
            evaluate_mappings(
                &rules,
                &make_identity("alice.engineering@acme.com", serde_json::json!({})),
                1,
            ),
            vec![10]
        );
        assert!(evaluate_mappings(
            &rules,
            &make_identity("alice.sales@acme.com", serde_json::json!({})),
            1,
        )
        .is_empty());
    }

    #[test]
    fn test_email_glob_case_insensitive() {
        let rules = vec![make_rule(1, None, "email_glob", "email", "*@ACME.COM", 10)];
        assert_eq!(
            evaluate_mappings(
                &rules,
                &make_identity("Alice@acme.com", serde_json::json!({})),
                1
            ),
            vec![10]
        );
    }

    #[test]
    fn test_email_glob_exact_match_no_star() {
        let rules = vec![make_rule(
            1,
            None,
            "email_glob",
            "email",
            "alice@acme.com",
            10,
        )];
        assert_eq!(
            evaluate_mappings(
                &rules,
                &make_identity("alice@acme.com", serde_json::json!({})),
                1
            ),
            vec![10]
        );
        assert!(evaluate_mappings(
            &rules,
            &make_identity("bob@acme.com", serde_json::json!({})),
            1,
        )
        .is_empty());
    }

    #[test]
    fn test_unknown_match_type_skipped() {
        let rules = vec![
            make_rule(1, None, "magic_match", "email", "anything", 10),
            make_rule(2, None, "email_domain", "email", "company.com", 20),
        ];
        let identity = make_identity("alice@company.com", serde_json::json!({}));
        let groups = evaluate_mappings(&rules, &identity, 1);
        // "magic_match" skipped, domain rule still evaluates
        assert_eq!(groups, vec![20]);
    }
}
