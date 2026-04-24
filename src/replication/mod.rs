//! Lazy bucket replication: scheduled source→destination copies
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
//! This file today just re-exports the planner; the worker + state
//! store will follow in subsequent commits per the rollout plan.

pub mod planner;

pub use planner::{plan_batch, rewrite_key, should_replicate, BatchPlan, Decision};
