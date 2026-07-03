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

pub use lease::{CoordinationLease, LeaseSubsystem, LocalLease};
