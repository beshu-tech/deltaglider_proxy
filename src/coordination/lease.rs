// SPDX-License-Identifier: BUSL-1.1

//! The [`CoordinationLease`] seam + its node-local SQLite implementation.

use std::future::Future;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::config_db::ConfigDb;

/// Why a lease or lock step did not succeed. `Lost` is a verdict (refused,
/// stolen, lapsed: stop before more work); `Backend` is "could not tell"
/// (the store errored: the holder may still own it until its TTL).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LeaseError {
    #[error("lease lost")]
    Lost,
    #[error("{0}")]
    Backend(String),
}

impl LeaseError {
    /// Map a boolean renewal (`false` = refused) and its store error.
    pub fn from_renewal<E: std::fmt::Display>(r: Result<bool, E>) -> Result<(), LeaseError> {
        match r {
            Ok(true) => Ok(()),
            Ok(false) => Err(LeaseError::Lost),
            Err(e) => Err(LeaseError::Backend(e.to_string())),
        }
    }

    /// A store error (not a verdict).
    pub fn backend<E: std::fmt::Display>(e: E) -> LeaseError {
        LeaseError::Backend(e.to_string())
    }
}

/// Which job subsystem a lease belongs to. Selects the backing table (for the
/// local impl) and namespaces the lease key (for the S3 impl). Only
/// replication runs through this seam: lifecycle and maintenance keep their
/// node-local SQLite leases (a variant without a caller read as HA coverage
/// that did not exist).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseSubsystem {
    Replication,
}

impl LeaseSubsystem {
    /// Stable slug used in the S3 lease object key (`_dgp/leases/<slug>/<rule>`).
    pub fn slug(&self) -> &'static str {
        match self {
            LeaseSubsystem::Replication => "replication",
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
#[dynosaur::dynosaur(pub DynCoordinationLease = dyn(box) CoordinationLease)]
pub trait CoordinationLease: Send + Sync {
    /// Take the lease for `(subsystem, rule)` if free or expired. `true` = held.
    fn try_acquire(
        &self,
        subsystem: LeaseSubsystem,
        rule: &str,
        owner: &str,
        now: i64,
        ttl_secs: i64,
    ) -> impl Future<Output = Result<bool, LeaseError>> + Send;

    /// Extend a lease this owner still holds. `Err(Lost)` = lost/stolen/lapsed
    /// → the caller must stop before starting more work.
    fn renew(
        &self,
        subsystem: LeaseSubsystem,
        rule: &str,
        owner: &str,
        now: i64,
        ttl_secs: i64,
    ) -> impl Future<Output = Result<(), LeaseError>> + Send;

    /// Release a lease this owner holds (no-op for a different owner).
    fn release(
        &self,
        subsystem: LeaseSubsystem,
        rule: &str,
        owner: &str,
    ) -> impl Future<Output = Result<(), LeaseError>> + Send;

    /// Read-only: is a (non-expired) lease currently held for `(subsystem,
    /// rule)`? Used by admin handlers (run-now / verify / delete) to gate against
    /// an in-flight run REGARDLESS of which lease backend holds it — the
    /// node-local SQLite check alone is blind to a scheduler holding the S3 lease,
    /// which let run-now double-run and verify/delete race a live run (H14/H29/H48).
    fn is_held(
        &self,
        subsystem: LeaseSubsystem,
        rule: &str,
        now: i64,
    ) -> impl Future<Output = Result<bool, LeaseError>> + Send;
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

impl CoordinationLease for LocalLease {
    async fn try_acquire(
        &self,
        subsystem: LeaseSubsystem,
        rule: &str,
        owner: &str,
        now: i64,
        ttl_secs: i64,
    ) -> Result<bool, LeaseError> {
        let db = self.db.lock().await;
        match subsystem {
            // The lease lives in the rule's state row: create it first, or
            // the UPDATE matches nothing and a free rule reads as busy.
            LeaseSubsystem::Replication => db
                .replication_ensure_state(rule, now)
                .and_then(|_| db.replication_try_acquire_lease(rule, owner, now, ttl_secs)),
        }
        .map_err(LeaseError::backend)
    }

    async fn renew(
        &self,
        subsystem: LeaseSubsystem,
        rule: &str,
        owner: &str,
        now: i64,
        ttl_secs: i64,
    ) -> Result<(), LeaseError> {
        let db = self.db.lock().await;
        LeaseError::from_renewal(match subsystem {
            LeaseSubsystem::Replication => db.replication_renew_lease(rule, owner, now, ttl_secs),
        })
    }

    async fn release(
        &self,
        subsystem: LeaseSubsystem,
        rule: &str,
        owner: &str,
    ) -> Result<(), LeaseError> {
        let db = self.db.lock().await;
        let _held = match subsystem {
            LeaseSubsystem::Replication => db.replication_release_lease(rule, owner),
        }
        .map_err(LeaseError::backend)?;
        Ok(())
    }

    async fn is_held(
        &self,
        subsystem: LeaseSubsystem,
        rule: &str,
        now: i64,
    ) -> Result<bool, LeaseError> {
        let db = self.db.lock().await;
        match subsystem {
            LeaseSubsystem::Replication => db.replication_lease_is_held(rule, now),
        }
        .map_err(LeaseError::backend)
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
        assert_eq!(lease.renew(r, "rule1", "A", 150, 60).await, Ok(()));

        // At the exact expiry instant the OWNER can renew but a RIVAL can't steal
        // (the >=/< tiling): with expires_at now 210, at now=210 renew succeeds…
        assert_eq!(lease.renew(r, "rule1", "A", 210, 60).await, Ok(())); // → 270
                                                                         // …and a steal at now=270 (== new expiry) is refused (needs < now).
        assert!(!lease.try_acquire(r, "rule1", "B", 270, 60).await.unwrap());
        // Once truly lapsed (now > expiry), a rival steals.
        assert!(lease.try_acquire(r, "rule1", "B", 271, 60).await.unwrap());
        // And the old owner A can no longer renew (lapsed → stop).
        assert_eq!(
            lease.renew(r, "rule1", "A", 271, 60).await,
            Err(LeaseError::Lost)
        );
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

    /// A refused renewal is a verdict; a store error is not.
    #[test]
    fn a_refused_renewal_is_lost_and_a_store_error_is_backend() {
        assert_eq!(LeaseError::from_renewal(Ok::<_, String>(true)), Ok(()));
        assert_eq!(
            LeaseError::from_renewal(Ok::<_, String>(false)),
            Err(LeaseError::Lost)
        );
        assert_eq!(
            LeaseError::from_renewal(Err::<bool, _>("db locked")),
            Err(LeaseError::Backend("db locked".into()))
        );
    }

    #[test]
    fn subsystem_slug_is_stable() {
        // The slug is part of the S3 lease object key: never rename it.
        assert_eq!(LeaseSubsystem::Replication.slug(), "replication");
    }
}
