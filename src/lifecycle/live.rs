// SPDX-License-Identifier: BUSL-1.1

//! The lifecycle runs of THIS process while they run: their counters (for
//! `GET /jobs` and the running entry of `GET /jobs/:id/runs`), an
//! operator stop request (pause or kill), and the reason the scheduler
//! last deferred a rule.
//!
//! In memory, like the run itself: a lifecycle lease is node-local, so the
//! node that runs a rule is the node that answers for it. The run history
//! row gets the totals when the run ends.

use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

/// The counters of a run so far. The same names as the run history.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct LiveCounters {
    pub objects_scanned: i64,
    /// Objects the run acted on: deleted, moved or copied.
    pub objects_affected: i64,
    pub objects_skipped: i64,
    /// Skipped because an include/exclude glob (or an internal key or a
    /// folder marker) keeps them out of the rule.
    pub objects_excluded: i64,
    /// Skipped because they are not old enough yet.
    pub objects_not_due: i64,
    pub objects_deleted: i64,
    /// Transitioned, and the source deleted.
    pub objects_moved: i64,
    /// Transitioned, and the source kept.
    pub objects_copied: i64,
    /// Logical (uncompressed) bytes of the affected objects.
    pub bytes_affected: i64,
    pub errors: i64,
}

/// What the operator asked a running run to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopRequest {
    /// Stop at the next object boundary and keep the cursor: a resume
    /// continues where the run stopped.
    Pause,
    /// Stop at the next object boundary and drop the cursor.
    Kill,
}

impl StopRequest {
    /// The run-history status of a run that this request stopped.
    pub fn run_status(self) -> &'static str {
        match self {
            StopRequest::Pause => "paused",
            StopRequest::Kill => "cancelled",
        }
    }
}

/// One running run.
#[derive(Debug)]
pub struct LiveRun {
    pub run_id: Option<i64>,
    pub started_at: i64,
    counters: Mutex<LiveCounters>,
    /// 0 = go on, 1 = pause, 2 = kill.
    stop: AtomicU8,
}

impl LiveRun {
    pub fn counters(&self) -> LiveCounters {
        *self.counters.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn publish(&self, counters: LiveCounters) {
        *self.counters.lock().unwrap_or_else(|e| e.into_inner()) = counters;
    }

    pub fn stop_requested(&self) -> Option<StopRequest> {
        match self.stop.load(Ordering::Relaxed) {
            1 => Some(StopRequest::Pause),
            2 => Some(StopRequest::Kill),
            _ => None,
        }
    }

    fn request(&self, stop: StopRequest) {
        let v = match stop {
            StopRequest::Pause => 1,
            StopRequest::Kill => 2,
        };
        // A kill overrides a pause; a pause never weakens a kill.
        self.stop.fetch_max(v, Ordering::Relaxed);
    }
}

static RUNS: Mutex<Vec<(String, Arc<LiveRun>)>> = Mutex::new(Vec::new());
static DEFERRED: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

/// Registers a run until it drops.
pub struct LiveRunGuard {
    rule: String,
    run: Arc<LiveRun>,
}

impl LiveRunGuard {
    pub fn run(&self) -> &Arc<LiveRun> {
        &self.run
    }
}

impl Drop for LiveRunGuard {
    fn drop(&mut self) {
        let mut runs = RUNS.lock().unwrap_or_else(|e| e.into_inner());
        runs.retain(|(name, run)| !(name == &self.rule && Arc::ptr_eq(run, &self.run)));
    }
}

/// Register the run `run_id` of `rule`. A run that starts clears the
/// rule's deferral.
pub fn register(rule: &str, run_id: Option<i64>) -> LiveRunGuard {
    set_deferred(rule, None);
    let run = Arc::new(LiveRun {
        run_id,
        started_at: super::current_unix_seconds(),
        counters: Mutex::default(),
        stop: AtomicU8::new(0),
    });
    RUNS.lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((rule.to_string(), run.clone()));
    LiveRunGuard {
        rule: rule.to_string(),
        run,
    }
}

/// The running run of `rule` in this process, if any.
pub fn running(rule: &str) -> Option<Arc<LiveRun>> {
    RUNS.lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .rev()
        .find(|(name, _)| name == rule)
        .map(|(_, run)| run.clone())
}

/// Ask the running run of `rule` to stop. `false` when none runs here.
pub fn request_stop(rule: &str, stop: StopRequest) -> bool {
    let runs = RUNS.lock().unwrap_or_else(|e| e.into_inner());
    let mut any = false;
    for (_, run) in runs.iter().filter(|(name, _)| name == rule) {
        run.request(stop);
        any = true;
    }
    any
}

/// Record (or clear, with `None`) why the scheduler did not run a due
/// rule. Returns `true` when the reason changed, so the caller logs a
/// deferral once, not on every check.
pub fn set_deferred(rule: &str, reason: Option<String>) -> bool {
    let mut map = DEFERRED.lock().unwrap_or_else(|e| e.into_inner());
    let map = map.get_or_insert_with(HashMap::new);
    match reason {
        Some(reason) => map.insert(rule.to_string(), reason.clone()).as_ref() != Some(&reason),
        None => map.remove(rule).is_some(),
    }
}

/// Why the scheduler last deferred `rule`, while it still does.
pub fn deferred(rule: &str) -> Option<String> {
    DEFERRED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .and_then(|m| m.get(rule).cloned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_registered_run_is_visible_until_it_drops() {
        let guard = register("live-visible", Some(7));
        let run = running("live-visible").expect("registered");
        assert_eq!(run.run_id, Some(7));
        run.publish(LiveCounters {
            objects_deleted: 3,
            ..Default::default()
        });
        assert_eq!(guard.run().counters().objects_deleted, 3);
        drop(guard);
        assert!(running("live-visible").is_none());
    }

    #[test]
    fn a_kill_overrides_a_pause_and_not_the_reverse() {
        let guard = register("live-stop", None);
        assert_eq!(guard.run().stop_requested(), None);
        assert!(request_stop("live-stop", StopRequest::Pause));
        assert_eq!(guard.run().stop_requested(), Some(StopRequest::Pause));
        assert!(request_stop("live-stop", StopRequest::Kill));
        assert!(request_stop("live-stop", StopRequest::Pause));
        assert_eq!(guard.run().stop_requested(), Some(StopRequest::Kill));
        assert!(!request_stop("live-none", StopRequest::Kill));
    }

    #[test]
    fn a_deferral_is_reported_once_and_cleared_by_a_run() {
        assert!(set_deferred("live-defer", Some("bucket 'a' busy".into())));
        assert!(!set_deferred("live-defer", Some("bucket 'a' busy".into())));
        assert_eq!(deferred("live-defer").as_deref(), Some("bucket 'a' busy"));
        let _run = register("live-defer", None);
        assert_eq!(deferred("live-defer"), None);
    }
}
