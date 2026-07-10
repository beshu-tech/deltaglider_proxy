// SPDX-License-Identifier: GPL-3.0-only

//! The [`CoordinationLease`] seam + its node-local SQLite implementation.

use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::config_db::ConfigDb;

/// Which job subsystem a lease belongs to. Selects the backing table (for the
/// local impl) and namespaces the lease key (for the S3 impl), so a replication
/// rule and a lifecycle rule with the same name never collide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseSubsystem {
    Replication,
    Lifecycle,
}

impl LeaseSubsystem {
    /// Stable slug used in the S3 lease object key (`_dgp/leases/<slug>/<rule>`).
    pub fn slug(&self) -> &'static str {
        match self {
            LeaseSubsystem::Replication => "replication",
            LeaseSubsystem::Lifecycle => "lifecycle",
        }
    }
}

/// A per-rule leader lease with TTL + heartbeat renewal + steal-on-expiry.
///
/// Semantics every impl MUST reproduce (the `config_db/job_store.rs` tiling):
///  - `try_acquire` succeeds when the lease is free/expired (`expires_at < now`,
///    strict) — exactly one racer wins a free lease.
///  - `renew` succeeds only for the same owner AND while `expires_at >= now`
///    (non-strict). A lapsed owner must NOT renew (never resurrect a lease a
///    peer may have stolen).
///  - `release` is owner-scoped (no-op for a different owner).
///
/// The two `<`/`>=` predicates partition the timeline, so the exact expiry
/// instant is never simultaneously renewable by the owner and stealable by a
/// rival. `ttl_secs.max(1)` and saturating expiry math are part of the contract.
#[async_trait]
pub trait CoordinationLease: Send + Sync {
    /// Take the lease for `(subsystem, rule)` if free or expired. `true` = held.
    async fn try_acquire(
        &self,
        subsystem: LeaseSubsystem,
        rule: &str,
        owner: &str,
        now: i64,
        ttl_secs: i64,
    ) -> Result<bool, String>;

    /// Extend a lease this owner still holds. `false` = lost/stolen/lapsed →
    /// the caller must stop before starting more work.
    async fn renew(
        &self,
        subsystem: LeaseSubsystem,
        rule: &str,
        owner: &str,
        now: i64,
        ttl_secs: i64,
    ) -> Result<bool, String>;

    /// Release a lease this owner holds (no-op for a different owner).
    async fn release(
        &self,
        subsystem: LeaseSubsystem,
        rule: &str,
        owner: &str,
    ) -> Result<(), String>;

    /// Read-only: is a (non-expired) lease currently held for `(subsystem,
    /// rule)`? Used by admin handlers (run-now / verify / delete) to gate against
    /// an in-flight run REGARDLESS of which lease backend holds it — the
    /// node-local SQLite check alone is blind to a scheduler holding the S3 lease,
    /// which let run-now double-run and verify/delete race a live run (H14/H29/H48).
    async fn is_held(
        &self,
        subsystem: LeaseSubsystem,
        rule: &str,
        now: i64,
    ) -> Result<bool, String>;
}

/// Node-local lease backed by the SQLite CAS in `config_db/job_store.rs` (via the
/// per-subsystem `state_store` delegations). This is the single-instance default:
/// correct within one node's process restarts, invisible to peers. Selecting this
/// impl is what "HA inactive" means — no coordination bucket, no S3 traffic.
pub struct LocalLease {
    db: Arc<Mutex<ConfigDb>>,
}

impl LocalLease {
    pub fn new(db: Arc<Mutex<ConfigDb>>) -> Self {
        Self { db }
    }
}

#[async_trait]
impl CoordinationLease for LocalLease {
    async fn try_acquire(
        &self,
        subsystem: LeaseSubsystem,
        rule: &str,
        owner: &str,
        now: i64,
        ttl_secs: i64,
    ) -> Result<bool, String> {
        let db = self.db.lock().await;
        match subsystem {
            LeaseSubsystem::Replication => {
                db.replication_try_acquire_lease(rule, owner, now, ttl_secs)
            }
            LeaseSubsystem::Lifecycle => db.lifecycle_try_acquire_lease(rule, owner, now, ttl_secs),
        }
        .map_err(|e| e.to_string())
    }

    async fn renew(
        &self,
        subsystem: LeaseSubsystem,
        rule: &str,
        owner: &str,
        now: i64,
        ttl_secs: i64,
    ) -> Result<bool, String> {
        let db = self.db.lock().await;
        match subsystem {
            LeaseSubsystem::Replication => db.replication_renew_lease(rule, owner, now, ttl_secs),
            LeaseSubsystem::Lifecycle => db.lifecycle_renew_lease(rule, owner, now, ttl_secs),
        }
        .map_err(|e| e.to_string())
    }

    async fn release(
        &self,
        subsystem: LeaseSubsystem,
        rule: &str,
        owner: &str,
    ) -> Result<(), String> {
        let db = self.db.lock().await;
        let _held = match subsystem {
            LeaseSubsystem::Replication => db.replication_release_lease(rule, owner),
            LeaseSubsystem::Lifecycle => db.lifecycle_release_lease(rule, owner),
        }
        .map_err(|e| e.to_string())?;
        Ok(())
    }

    async fn is_held(
        &self,
        subsystem: LeaseSubsystem,
        rule: &str,
        now: i64,
    ) -> Result<bool, String> {
        let db = self.db.lock().await;
        match subsystem {
            LeaseSubsystem::Replication => db.replication_lease_is_held(rule, now),
            LeaseSubsystem::Lifecycle => db.lifecycle_lease_is_held(rule, now),
        }
        .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_db() -> Arc<Mutex<ConfigDb>> {
        // In-memory config DB (schema created), for lease-tiling parity tests.
        Arc::new(Mutex::new(ConfigDb::in_memory("testpass").unwrap()))
    }

    async fn ensure_rule(lease: &LocalLease, rule: &str) {
        // A lease row must exist for the UPDATE to target — mirror the scheduler's
        // `replication_ensure_state` precondition.
        let db = lease.db.lock().await;
        db.replication_ensure_state(rule, 0).unwrap();
    }

    #[tokio::test]
    async fn local_lease_acquire_renew_steal_tiling() {
        let lease = LocalLease::new(mem_db());
        let r = LeaseSubsystem::Replication;
        ensure_rule(&lease, "rule1").await;

        // Acquire a free lease (owner A), TTL 60 → expires at now+60=160.
        assert!(lease.try_acquire(r, "rule1", "A", 100, 60).await.unwrap());
        // A rival B cannot steal a LIVE lease (expires_at 160 > now 150).
        assert!(!lease.try_acquire(r, "rule1", "B", 150, 60).await.unwrap());
        // Owner A CAN renew while live (expires_at 160 >= now 150 → new 210).
        assert!(lease.renew(r, "rule1", "A", 150, 60).await.unwrap());

        // At the exact expiry instant the OWNER can renew but a RIVAL can't steal
        // (the >=/< tiling): with expires_at now 210, at now=210 renew succeeds…
        assert!(lease.renew(r, "rule1", "A", 210, 60).await.unwrap()); // → 270
                                                                       // …and a steal at now=270 (== new expiry) is refused (needs < now).
        assert!(!lease.try_acquire(r, "rule1", "B", 270, 60).await.unwrap());
        // Once truly lapsed (now > expiry), a rival steals.
        assert!(lease.try_acquire(r, "rule1", "B", 271, 60).await.unwrap());
        // And the old owner A can no longer renew (lapsed → stop).
        assert!(!lease.renew(r, "rule1", "A", 271, 60).await.unwrap());
    }

    #[tokio::test]
    async fn local_lease_is_held_reflects_liveness() {
        // is_held backs the admin run-now/verify/delete cross-backend gate.
        let lease = LocalLease::new(mem_db());
        let r = LeaseSubsystem::Replication;
        ensure_rule(&lease, "rule1").await;

        // Nothing held yet.
        assert!(!lease.is_held(r, "rule1", 100).await.unwrap());
        // Acquire (TTL 60 → expires 160): held at now=150, not at now=161.
        assert!(lease.try_acquire(r, "rule1", "A", 100, 60).await.unwrap());
        assert!(lease.is_held(r, "rule1", 150).await.unwrap());
        assert!(!lease.is_held(r, "rule1", 161).await.unwrap());
    }

    #[tokio::test]
    async fn local_lease_release_is_owner_scoped() {
        let lease = LocalLease::new(mem_db());
        let r = LeaseSubsystem::Replication;
        ensure_rule(&lease, "rule1").await;

        assert!(lease.try_acquire(r, "rule1", "A", 100, 60).await.unwrap());
        // A different owner's release is a no-op — A still holds it.
        lease.release(r, "rule1", "B").await.unwrap();
        assert!(!lease.try_acquire(r, "rule1", "B", 120, 60).await.unwrap());
        // The real owner releasing frees it immediately (before expiry).
        lease.release(r, "rule1", "A").await.unwrap();
        assert!(lease.try_acquire(r, "rule1", "B", 120, 60).await.unwrap());
    }

    #[test]
    fn subsystem_slugs_are_distinct() {
        assert_eq!(LeaseSubsystem::Replication.slug(), "replication");
        assert_eq!(LeaseSubsystem::Lifecycle.slug(), "lifecycle");
        assert_ne!(
            LeaseSubsystem::Replication.slug(),
            LeaseSubsystem::Lifecycle.slug()
        );
    }
}
