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
//! The periodic scheduler is still deferred; interval/next_due state is
//! stored so automatic ticks can be added without changing the rule shape.

pub mod planner;
pub mod state_store;
pub mod worker;

pub use planner::{plan_batch, rewrite_key, should_replicate, BatchPlan, Decision};
pub use state_store::{
    current_unix_seconds, FailureRecord, ReplicationState, RunRecord, RunTotals,
};
pub use worker::{run_rule, RunOutcome};
