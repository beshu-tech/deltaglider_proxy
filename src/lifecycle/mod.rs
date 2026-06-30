// SPDX-License-Identifier: GPL-3.0-only

//! Delete-only object lifecycle rules.
//!
//! v1 keeps lifecycle intentionally narrow: rules are YAML-authored,
//! disabled by default, previewable through the admin API, and execution
//! deletes through the DeltaGlider engine rather than raw storage.

pub mod planner;
pub mod scheduler;
pub mod state_store;
pub mod worker;

use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};

pub use crate::background::RunLease;
pub use planner::{
    compile_rule_globs, plan_object, Decision, PlanError, PlannedLifecycleAction, SkipReason,
};
pub use state_store::{
    LifecycleFailureRecord, LifecycleRunRecord, LifecycleRunTotals, LifecycleState,
};
pub use worker::{preview_rule, run_rule, LifecycleFailure, LifecycleRunOutcome, PreviewObject};

/// HTTP status for a lifecycle run/preview error. Config-attribute problems
/// (missing `expire_after`, unparseable/out-of-range duration, malformed glob,
/// retain-newest `count is 0`) are `BAD_REQUEST` — the rule is invalid, not the
/// server. Everything else (list/rewrite/DB/storage failures) is
/// `INTERNAL_SERVER_ERROR`. Pure so the admin handlers map errors without
/// string-matching inline, and so the truth table is unit-tested. Mirrors the
/// project's `classify_sqlite_error` / `classify_s3_error` convention.
pub fn classify_lifecycle_run_error(err: &str) -> axum::http::StatusCode {
    if is_lifecycle_config_validation_error(err) {
        axum::http::StatusCode::BAD_REQUEST
    } else {
        axum::http::StatusCode::INTERNAL_SERVER_ERROR
    }
}

/// The duration fields a lifecycle rule can carry; a `<field>=<val> invalid:` /
/// `out of range:` error from any of them is a config defect, not a runtime
/// fault. Keep in sync with the `format!("{field}=…")` sites in
/// `planner::lifecycle_rule_errors` + `worker` (the only producers).
const LIFECYCLE_DURATION_FIELDS: &[&str] = &[
    "expire_after=",
    "qualify.min_age=",
    "min_age=",
    "protect_younger_than=",
];

/// True iff `err` is a rule-config validation failure (attacker/operator can
/// fix it by editing the rule), not a runtime/IO failure. Signatures come from
/// `planner::lifecycle_rule_errors` + `worker::run_or_preview` +
/// `run_or_preview_retain_newest` + `planner::PlanError::InvalidGlob`:
/// - "lifecycle rule '{}' {kind} action requires expire_after"
/// - "lifecycle rule '{}' {field}={s} invalid: {err}" / " out of range: {err}"
/// - "lifecycle rule '{}' transition destination bucket is empty"
/// - "lifecycle rule name '{}' is duplicated …"
/// - "invalid glob {pattern:?}: {reason}"
/// - "lifecycle rule '{}' retain-newest count is 0 — refusing to run …"
///
/// The duration matchers are ANCHORED to a known `<field>=` token (not the bare
/// " invalid: " / " out of range: " fragment) so a runtime/IO error that merely
/// happens to contain those English words — e.g. an S3 list failure — is NOT
/// misclassified as a client 400.
fn is_lifecycle_config_validation_error(err: &str) -> bool {
    if err.contains("requires expire_after")
        || err.contains("count is 0")
        || err.contains("transition destination bucket is empty")
        || err.contains("is duplicated")
        || err.starts_with("invalid glob")
    {
        return true;
    }
    // Anchored duration error: a known field token followed (later in the
    // string) by the validation marker. Both producers emit `{field}={val}
    // invalid|out of range: …`, so the field token always precedes the marker.
    let has_marker = err.contains(" invalid: ") || err.contains(" out of range: ");
    has_marker
        && LIFECYCLE_DURATION_FIELDS
            .iter()
            .any(|field| err.contains(field))
}

pub fn current_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

static RUNNING_RULES: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

pub(crate) struct RuleRunGuard {
    name: String,
}

impl Drop for RuleRunGuard {
    fn drop(&mut self) {
        if let Some(lock) = RUNNING_RULES.get() {
            lock.lock()
                .expect("lifecycle run lock poisoned")
                .remove(&self.name);
        }
    }
}

/// Process-local single-flight for lifecycle rule execution. This is not a
/// distributed lease; v1 avoids DB state. It still prevents admin run-now and
/// the local scheduler from racing the same rule inside one process.
pub(crate) fn try_acquire_rule(name: &str) -> Option<RuleRunGuard> {
    let lock = RUNNING_RULES.get_or_init(|| Mutex::new(HashSet::new()));
    let mut running = lock.lock().expect("lifecycle run lock poisoned");
    if running.insert(name.to_string()) {
        Some(RuleRunGuard {
            name: name.to_string(),
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    #[test]
    fn validation_errors_map_to_bad_request() {
        // Each signature the run/preview path emits for a malformed RULE CONFIG
        // must classify as 400 (operator fixable), not 500.
        // These are the ACTUAL strings the producers emit (planner +
        // worker) — field names match the real config keys, not generic
        // placeholders, so the anchored classifier recognises them.
        let cases = [
            "lifecycle rule 'expire-old' delete action requires expire_after",
            "lifecycle rule 'r' expire_after=5x invalid: character 'x' not expected",
            "lifecycle rule 'r' expire_after=999999999d out of range: value out of range",
            "lifecycle rule 'r' qualify.min_age=abc invalid: expected number",
            "lifecycle rule 'r' protect_younger_than=1y out of range: value out of bounds",
            "invalid glob \"[unclosed\": error building glob set",
            "lifecycle rule 'keep-top' retain-newest count is 0 — refusing to run (would delete the whole prefix)",
        ];
        for err in cases {
            assert_eq!(
                classify_lifecycle_run_error(err),
                StatusCode::BAD_REQUEST,
                "should be 400 (validation): {err}"
            );
        }
    }

    #[test]
    fn runtime_errors_map_to_internal_server_error() {
        // Runtime/IO failures (list, rewrite, DB, storage) must stay 500 — the
        // rule config is fine; the server/backend failed.
        let cases = [
            "list lifecycle page 3 failed: Connection refused",
            "could not rewrite lifecycle destination for \"a/b\": empty key",
            "storage error: NoSuchBucket",
            "config db error: SqliteFailure",
            // Adversarial: runtime errors that DO contain the bare validation
            // markers but are NOT lifecycle-config defects. The old unanchored
            // matcher misclassified these as 400.
            "list page failed: parameter invalid: bad continuation token",
            "storage HEAD failed: byte offset out of range: 9999",
            "S3 InvalidArgument: timestamp invalid: malformed",
        ];
        for err in cases {
            assert_eq!(
                classify_lifecycle_run_error(err),
                StatusCode::INTERNAL_SERVER_ERROR,
                "should be 500 (runtime): {err}"
            );
        }
    }

    #[test]
    fn anchored_duration_errors_map_to_bad_request() {
        // Every lifecycle duration-field validation error (field-anchored) is a
        // config defect → 400, across all four duration fields.
        let cases = [
            "lifecycle rule 'r' expire_after=5x invalid: unknown unit 'x'",
            "lifecycle rule 'r' expire_after=999999d out of range: overflow",
            "lifecycle rule 'r' qualify.min_age=2z invalid: unknown unit",
            "lifecycle rule 'r' protect_younger_than=-1 invalid: negative",
        ];
        for err in cases {
            assert_eq!(
                classify_lifecycle_run_error(err),
                StatusCode::BAD_REQUEST,
                "should be 400 (config): {err}"
            );
        }
    }
}
