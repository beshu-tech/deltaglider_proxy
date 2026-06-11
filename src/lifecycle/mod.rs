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

pub use planner::{
    compile_rule_globs, plan_object, Decision, PlanError, PlannedLifecycleAction, SkipReason,
};
pub use state_store::{
    LifecycleFailureRecord, LifecycleRunRecord, LifecycleRunTotals, LifecycleState,
};
pub use worker::{
    preview_rule, run_rule, LifecycleFailure, LifecycleRunOutcome, PreviewObject, RunLease,
};

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
