// SPDX-License-Identifier: GPL-3.0-only

//! Cross-instance coordination primitives for the job plane.
//!
//! The job schedulers (replication / lifecycle / maintenance) elect a single
//! leader per rule via a TTL lease with heartbeat renewal + steal-on-expiry.
//! Today that lease is a node-local SQLite row (`config_db/job_store.rs`), so a
//! peer never sees another node's lease → under multi-instance two nodes can
//! both "acquire" the same rule and double-run it.
//!
//! [`CoordinationLease`] is the seam that lets the SAME scheduler code run
//! against either backing store, selected once at startup:
//!   - [`LocalLease`] — wraps the existing SQLite CAS. Zero cross-node visibility,
//!     zero S3 traffic. The single-instance default (no coordination bucket).
//!   - `S3Lease` (later) — the lease as a CAS'd object in the coordination bucket,
//!     visible to every node → real leader failover. Gated on the boot-validated
//!     coordination bucket supporting conditional writes.
//!
//! The trait deliberately mirrors `job_store`'s two-predicate tiling: acquire/steal
//! on `expires_at < now` (strict), renew while `expires_at >= now` (non-strict), so
//! the exact expiry instant is never simultaneously renewable and stealable.

pub mod lease;
pub mod s3_lease;

pub use lease::{CoordinationLease, LeaseSubsystem, LocalLease};
pub use s3_lease::S3Lease;

/// A durable-per-node identity for lease ownership provenance + self-reclaim.
///
/// Unlike the ephemeral per-task `owner` uuid (regenerated every process/task),
/// this SURVIVES a restart, so a rebooted node recognises its own prior lease
/// object and reclaims it immediately rather than orphaning it for a full TTL
/// (edge case E7). Resolution order:
///  1. `DGP_NODE_ID` env (operator-pinned — best for stable fleets).
///  2. `HOSTNAME` env (container id / host name — stable across restarts of the
///     same container/pod).
///  3. A uuid generated once and persisted to `<dir>/node-id` next to the config
///     DB, re-read on subsequent boots.
///
/// Purely a provenance/self-reclaim label — NEVER a coordination decision (the
/// lease CAS is the only arbiter). `dir` is the config-DB directory.
pub fn durable_node_id(dir: &std::path::Path) -> String {
    if let Ok(id) = std::env::var("DGP_NODE_ID") {
        if !id.trim().is_empty() {
            return id;
        }
    }
    if let Ok(host) = std::env::var("HOSTNAME") {
        if !host.trim().is_empty() {
            return host;
        }
    }
    let path = dir.join("node-id");
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    let generated = format!("node-{}", uuid::Uuid::new_v4());
    // Best-effort persist; if the write fails we still return a valid (if
    // non-durable-this-boot) id rather than block startup.
    let _ = std::fs::write(&path, &generated);
    generated
}
