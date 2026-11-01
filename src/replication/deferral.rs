// SPDX-License-Identifier: BUSL-1.1

//! Why a replication rule does not write now (node-local, in memory): its
//! destination refuses writes (a read-only encrypted bucket, the atomic-write
//! check still running, the encryption witness) or a backend of the rule is
//! gated. The scheduler and the event consumer record it; the Jobs row shows
//! the rule as `deferred` with the reason (`detail.deferred`).
//!
//! The event consumer does NOT hold a deferred rule's events, except while a
//! maintenance job rewrites the destination (that ends on its own): it lets
//! the rule's cursor move on, so the outbox is pruned, and makes the rule
//! due. The scheduler runs that catch-up walk as soon as the destination
//! accepts writes again (compatibility review C4).

use std::collections::HashMap;
use std::sync::Mutex;

static DEFERRED: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

/// Record (or clear, with `None`) why `rule` does not write now. Returns
/// `true` when the state changed, so the caller logs once per change.
pub fn set_deferred(rule: &str, reason: Option<String>) -> bool {
    let mut map = DEFERRED.lock().unwrap_or_else(|e| e.into_inner());
    let map = map.get_or_insert_with(HashMap::new);
    match reason {
        Some(reason) => map.insert(rule.to_string(), reason.clone()).as_ref() != Some(&reason),
        None => map.remove(rule).is_some(),
    }
}

/// Why `rule` does not write now, while it does not.
pub fn deferred(rule: &str) -> Option<String> {
    DEFERRED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|m| m.get(rule).cloned())
}

/// Pure: the reason shown for a rule whose destination refuses writes.
pub fn destination_refuses(why: &str) -> String {
    format!(
        "the destination refuses writes: {why}. The rule copies what it skipped once the \
         destination accepts writes again"
    )
}

/// Record that `rule`'s destination refuses writes (`why`), with one WARN
/// per change of the reason.
pub fn destination_blocked(rule: &str, why: &str) {
    if set_deferred(rule, Some(destination_refuses(why))) {
        tracing::warn!("replication: rule '{rule}' is deferred: {why}");
    }
}

/// The rule writes again: clear its deferral, with one INFO line.
pub fn writes_again(rule: &str) {
    if set_deferred(rule, None) {
        tracing::info!("replication: rule '{rule}' writes again");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_deferral_changes_once_per_reason() {
        assert!(set_deferred("defer-unit", Some("a".into())));
        assert!(!set_deferred("defer-unit", Some("a".into())), "same reason");
        assert!(set_deferred("defer-unit", Some("b".into())));
        assert_eq!(deferred("defer-unit").as_deref(), Some("b"));
        assert!(set_deferred("defer-unit", None));
        assert!(!set_deferred("defer-unit", None));
        assert_eq!(deferred("defer-unit"), None);
    }
}
