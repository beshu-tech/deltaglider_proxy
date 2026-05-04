//! Lazy bucket replication: run-now source→destination copies
//! routed through the engine so encryption / delta compression stay
//! transparent.
//!
//! Module layout:
//! - `planner` — pure functions (rewrite_key, should_replicate,
//!   plan_batch). No I/O; heavily unit-tested.
//! - `state_store` — ConfigDb wrapper for replication_state /
//!   replication_run_history / replication_failures tables (added
//!   later — v6 schema).
//! - `worker` — async copy loop. Calls engine.retrieve on source,
//!   engine.store on destination. Added later.
//!
//! The periodic scheduler wakes from `replication.tick_interval`, checks
//! each rule's persisted `next_due_at`, skips paused/disabled rules, and
//! executes due rules via the same worker used by "Run now".

pub mod planner;
pub mod scheduler;
pub mod state_store;
pub mod worker;

pub use planner::{
    normalize_prefix, plan_batch, rewrite_key, should_replicate, BatchPlan, Decision,
};
pub use state_store::{
    current_unix_seconds, FailureRecord, ReplicationState, RunRecord, RunTotals,
};
pub use worker::{run_rule, RunLease, RunOutcome};
