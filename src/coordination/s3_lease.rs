// SPDX-License-Identifier: GPL-3.0-only

//! [`S3Lease`] — a cross-instance leader lease stored as a CAS'd object in the
//! coordination bucket. This is what turns the node-local [`super::LocalLease`]
//! into REAL leader failover: every node sees the same lease object, so when a
//! leader dies its lease lapses (TTL) and a peer steals it (edge case E1).
//!
//! The primitive is S3 conditional-write CAS — `If-None-Match:*` to create a free
//! lease, `If-Match:<etag>` to steal an expired one or renew a held one. This is
//! the exact pattern already proven in `config_db_sync.rs`; only a CAS-capable
//! coordination backend reaches here (the boot validation refuses B2/501).
//!
//! The DECISION logic (what action a given lease state warrants) is factored into
//! the pure [`plan_acquire`] / [`plan_renew`] kernels so the full edge-case truth
//! table is unit-testable without a live backend; the async methods do the I/O.

use async_trait::async_trait;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use serde::{Deserialize, Serialize};

use super::lease::{CoordinationLease, LeaseSubsystem};

/// The lease object body. `epoch` is monotonic (bumped on every steal) — a
/// diagnostic + future fence token; it is NOT used to fence customer-bucket
/// writes (impossible — see the HA reassessment).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Lease {
    pub owner: String,
    pub node_id: String,
    pub epoch: u64,
    pub expires_at: i64,
}

/// Bounded retries for a renew PUT against a transient coordination-bucket blip,
/// so one failed round-trip doesn't drop a lease the owner still legitimately
/// holds (edge case E5). Mirrors replication's heartbeat resilience.
const RENEW_MAX_ATTEMPTS: u32 = 3;

/// What `try_acquire` should do given the CURRENT lease object state.
#[derive(Debug, PartialEq, Eq)]
pub enum AcquireAction {
    /// No lease object exists → create with `If-None-Match:*`.
    Create,
    /// A lease exists but is expired/reclaimable → steal with `If-Match(etag)`,
    /// carrying `next_epoch`.
    Steal { etag: String, next_epoch: u64 },
    /// A live lease held by someone else → do not acquire.
    Blocked,
}

/// Pure acquire decision (mirrors `job_store::try_acquire_leader_lease`'s tiling).
///
/// A lease is stealable when it is EXPIRED (`expires_at < now`, STRICT) OR when
/// WE already own it (same `node_id`) — the self-reclaim path (E7) lets a
/// rebooted node take back its own still-live lease immediately instead of
/// waiting a full TTL. A live lease owned by a DIFFERENT node blocks.
pub fn plan_acquire(current: Option<(&Lease, &str)>, now: i64, my_node_id: &str) -> AcquireAction {
    match current {
        None => AcquireAction::Create,
        Some((lease, etag)) => {
            let expired = lease.expires_at < now;
            let mine = lease.node_id == my_node_id;
            if expired || mine {
                AcquireAction::Steal {
                    etag: etag.to_string(),
                    next_epoch: lease.epoch.saturating_add(1),
                }
            } else {
                AcquireAction::Blocked
            }
        }
    }
}

/// What `renew` should do given the current lease object state and the renewer.
#[derive(Debug, PartialEq, Eq)]
pub enum RenewAction {
    /// This owner still holds a live lease → extend with `If-Match(etag)`.
    Renew { etag: String, epoch: u64 },
    /// Not ours anymore (different owner) OR lapsed (`expires_at < now`) OR gone →
    /// stop; never resurrect a lease a peer may have stolen.
    Lost,
}

/// Pure renew decision (mirrors `job_store::renew_leader_lease`'s `>= now` +
/// owner-match guard). A lapsed owner must NOT renew.
pub fn plan_renew(current: Option<(&Lease, &str)>, now: i64, owner: &str) -> RenewAction {
    match current {
        Some((lease, etag)) if lease.owner == owner && lease.expires_at >= now => {
            RenewAction::Renew {
                etag: etag.to_string(),
                epoch: lease.epoch,
            }
        }
        _ => RenewAction::Lost,
    }
}

/// Cross-instance lease over a CAS-capable coordination bucket.
pub struct S3Lease {
    client: Client,
    bucket: String,
    /// Durable node identity (survives restart) — for self-reclaim provenance.
    node_id: String,
}

impl S3Lease {
    pub fn new(client: Client, bucket: String, node_id: String) -> Self {
        Self {
            client,
            bucket,
            node_id,
        }
    }

    fn object_key(subsystem: LeaseSubsystem, rule: &str) -> String {
        format!("_dgp/leases/{}/{}.json", subsystem.slug(), rule)
    }

    /// Read the lease object + its ETag. `Ok(None)` = absent (404-class).
    async fn read_lease(&self, key: &str) -> Result<Option<(Lease, String)>, String> {
        match self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
        {
            Ok(out) => {
                let etag = out.e_tag().map(str::to_string).unwrap_or_default();
                let bytes = out
                    .body
                    .collect()
                    .await
                    .map_err(|e| format!("lease body read: {e}"))?
                    .into_bytes();
                match serde_json::from_slice::<Lease>(&bytes) {
                    Ok(lease) => Ok(Some((lease, etag))),
                    // A corrupt/foreign object at the key → treat as absent so a
                    // fresh create-if-absent can reclaim it (it will 412 if a
                    // valid concurrent writer beat us, which is correct).
                    Err(_) => Ok(None),
                }
            }
            Err(e) => {
                if crate::config_db_sync::is_object_absent(
                    &crate::config_db_sync::sdk_error_signal(&e),
                ) {
                    Ok(None)
                } else {
                    Err(format!("{e:?}"))
                }
            }
        }
    }

    fn body_for(&self, owner: &str, epoch: u64, expires_at: i64) -> ByteStream {
        let lease = Lease {
            owner: owner.to_string(),
            node_id: self.node_id.clone(),
            epoch,
            expires_at,
        };
        ByteStream::from(serde_json::to_vec(&lease).unwrap_or_default())
    }

    /// PUT the lease object with a precondition. `precondition`:
    /// `None` → `If-None-Match:*` (create-only); `Some(etag)` → `If-Match(etag)`.
    /// Returns `Ok(true)` on success, `Ok(false)` on a 412 (lost the race),
    /// `Err` on any other failure.
    async fn put_lease(
        &self,
        key: &str,
        body: ByteStream,
        precondition: Option<&str>,
    ) -> Result<bool, String> {
        let mut put = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(body)
            .content_type("application/json");
        put = match precondition {
            Some(etag) => put.if_match(etag),
            None => put.if_none_match("*"),
        };
        match put.send().await {
            Ok(_) => Ok(true),
            Err(e) => {
                if crate::config_db_sync::is_precondition_failed(
                    &crate::config_db_sync::sdk_error_signal(&e),
                ) {
                    Ok(false) // a peer won the race — expected, not an error
                } else {
                    Err(format!("{e:?}"))
                }
            }
        }
    }
}

#[async_trait]
impl CoordinationLease for S3Lease {
    async fn try_acquire(
        &self,
        subsystem: LeaseSubsystem,
        rule: &str,
        owner: &str,
        now: i64,
        ttl_secs: i64,
    ) -> Result<bool, String> {
        let key = Self::object_key(subsystem, rule);
        let current = self.read_lease(&key).await?;
        let expires_at = now.saturating_add(ttl_secs.max(1));
        match plan_acquire(
            current.as_ref().map(|(l, e)| (l, e.as_str())),
            now,
            &self.node_id,
        ) {
            AcquireAction::Blocked => Ok(false),
            AcquireAction::Create => {
                self.put_lease(&key, self.body_for(owner, 1, expires_at), None)
                    .await
            }
            AcquireAction::Steal { etag, next_epoch } => {
                self.put_lease(
                    &key,
                    self.body_for(owner, next_epoch, expires_at),
                    Some(&etag),
                )
                .await
            }
        }
    }

    async fn renew(
        &self,
        subsystem: LeaseSubsystem,
        rule: &str,
        owner: &str,
        now: i64,
        ttl_secs: i64,
    ) -> Result<bool, String> {
        let key = Self::object_key(subsystem, rule);
        // Bounded retry loop (E5): a transient blip on read or PUT shouldn't drop
        // a lease we still hold. Only a real 412 (stolen) or a lapsed/foreign
        // lease is terminal `Lost`.
        //
        // The injected `now` is captured ONCE by the caller, but a degraded
        // backend can make the retries span more than a TTL of real time. Using
        // that stale `now` for the freshness check would renew (and re-extend) a
        // lease that has ACTUALLY lapsed — while a peer may already be stealing
        // it, so two nodes hold the same lease. Advance the effective time by the
        // real monotonic elapsed since loop start, so both the plan_renew
        // freshness check and the new expires_at reflect wall-clock progress.
        let started = std::time::Instant::now();
        let mut last_err: Option<String> = None;
        for _ in 0..RENEW_MAX_ATTEMPTS {
            let elapsed_secs = started.elapsed().as_secs() as i64;
            let effective_now = now.saturating_add(elapsed_secs);
            let expires_at = effective_now.saturating_add(ttl_secs.max(1));
            let current = match self.read_lease(&key).await {
                Ok(c) => c,
                Err(e) => {
                    last_err = Some(e);
                    continue;
                }
            };
            match plan_renew(
                current.as_ref().map(|(l, e)| (l, e.as_str())),
                effective_now,
                owner,
            ) {
                RenewAction::Lost => return Ok(false),
                RenewAction::Renew { etag, epoch } => {
                    match self
                        .put_lease(&key, self.body_for(owner, epoch, expires_at), Some(&etag))
                        .await
                    {
                        Ok(true) => return Ok(true),
                        // 412 here = a concurrent writer moved the etag. Re-read
                        // and re-evaluate: either we still own it (rare same-owner
                        // double-renew race) or we were stolen (→ Lost next pass).
                        Ok(false) => continue,
                        Err(e) => {
                            last_err = Some(e);
                            continue;
                        }
                    }
                }
            }
        }
        // Exhausted retries on transient errors: report the error so the caller
        // logs it, but the run should pause (treat like a lost renew) rather than
        // crash — the next tick re-acquires.
        Err(last_err.unwrap_or_else(|| "lease renew exhausted retries".to_string()))
    }

    async fn release(
        &self,
        subsystem: LeaseSubsystem,
        rule: &str,
        owner: &str,
    ) -> Result<(), String> {
        let key = Self::object_key(subsystem, rule);
        // Owner-scoped release: only delete the object if WE still own it, so a
        // release can't clobber a lease a peer legitimately stole.
        if let Some((lease, etag)) = self.read_lease(&key).await? {
            if lease.owner == owner {
                let _ = self
                    .client
                    .delete_object()
                    .bucket(&self.bucket)
                    .key(&key)
                    .if_match(&etag)
                    .send()
                    .await;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lease(owner: &str, node: &str, epoch: u64, expires: i64) -> Lease {
        Lease {
            owner: owner.into(),
            node_id: node.into(),
            epoch,
            expires_at: expires,
        }
    }

    #[test]
    fn acquire_free_lease_creates() {
        assert_eq!(plan_acquire(None, 100, "nodeA"), AcquireAction::Create);
    }

    #[test]
    fn acquire_expired_lease_steals_with_bumped_epoch() {
        let l = lease("old", "nodeB", 5, 90); // expired at now=100
        assert_eq!(
            plan_acquire(Some((&l, "etag1")), 100, "nodeA"),
            AcquireAction::Steal {
                etag: "etag1".into(),
                next_epoch: 6
            }
        );
    }

    #[test]
    fn acquire_live_foreign_lease_blocks() {
        let l = lease("held", "nodeB", 5, 160); // live at now=100
        assert_eq!(
            plan_acquire(Some((&l, "etag1")), 100, "nodeA"),
            AcquireAction::Blocked
        );
    }

    #[test]
    fn acquire_own_live_lease_self_reclaims() {
        // E7: our OWN still-live lease (same node_id) is reclaimable without
        // waiting for expiry — a rebooted node takes it straight back.
        let l = lease("old-task", "nodeA", 5, 160); // live, but ours
        assert_eq!(
            plan_acquire(Some((&l, "etag1")), 100, "nodeA"),
            AcquireAction::Steal {
                etag: "etag1".into(),
                next_epoch: 6
            }
        );
    }

    #[test]
    fn acquire_boundary_at_exact_expiry_blocks_foreign() {
        // expires_at == now → NOT expired (strict <), so a foreign live lease
        // still blocks (mirrors the job_store `< now` steal predicate).
        let l = lease("held", "nodeB", 5, 100);
        assert_eq!(
            plan_acquire(Some((&l, "e")), 100, "nodeA"),
            AcquireAction::Blocked
        );
    }

    #[test]
    fn renew_own_live_lease_extends() {
        let l = lease("me", "nodeA", 7, 160);
        assert_eq!(
            plan_renew(Some((&l, "etag2")), 100, "me"),
            RenewAction::Renew {
                etag: "etag2".into(),
                epoch: 7
            }
        );
    }

    #[test]
    fn renew_at_exact_expiry_still_ours() {
        // expires_at == now → renewable (non-strict >=), mirrors job_store.
        let l = lease("me", "nodeA", 7, 100);
        assert!(matches!(
            plan_renew(Some((&l, "e")), 100, "me"),
            RenewAction::Renew { .. }
        ));
    }

    #[test]
    fn renew_lapsed_lease_is_lost() {
        // expires_at < now → lapsed → must NOT renew (never resurrect).
        let l = lease("me", "nodeA", 7, 99);
        assert_eq!(plan_renew(Some((&l, "e")), 100, "me"), RenewAction::Lost);
    }

    #[test]
    fn renew_effective_now_advances_a_boundary_lease_to_lapsed() {
        // The renew loop advances the injected `now` by real monotonic elapsed
        // (M critic-gap): a lease that is renewable at the captured `now` but
        // whose expiry falls WITHIN the elapsed retry window must flip to Lost
        // when evaluated at effective_now = now + elapsed. Model the arithmetic
        // the loop performs: captured now=100, lease expires_at=102, and 3s of
        // real time elapsed on a degraded backend → effective_now=103 > 102.
        let l = lease("me", "nodeA", 7, 102);
        assert!(
            matches!(
                plan_renew(Some((&l, "e")), 100, "me"),
                RenewAction::Renew { .. }
            ),
            "renewable at the stale captured now"
        );
        let effective_now = 100i64.saturating_add(3); // now + elapsed_secs
        assert_eq!(
            plan_renew(Some((&l, "e")), effective_now, "me"),
            RenewAction::Lost,
            "a lease that lapsed DURING the retry window must not be renewed"
        );
    }

    #[test]
    fn renew_foreign_owner_is_lost() {
        let l = lease("someone-else", "nodeB", 7, 160);
        assert_eq!(plan_renew(Some((&l, "e")), 100, "me"), RenewAction::Lost);
    }

    #[test]
    fn renew_absent_lease_is_lost() {
        assert_eq!(plan_renew(None, 100, "me"), RenewAction::Lost);
    }

    #[test]
    fn lease_json_round_trips() {
        let l = lease("o", "n", 3, 1783000000);
        let bytes = serde_json::to_vec(&l).unwrap();
        assert_eq!(serde_json::from_slice::<Lease>(&bytes).unwrap(), l);
    }

    #[test]
    fn object_key_namespaced_by_subsystem() {
        assert_eq!(
            S3Lease::object_key(LeaseSubsystem::Replication, "r1"),
            "_dgp/leases/replication/r1.json"
        );
        assert_eq!(
            S3Lease::object_key(LeaseSubsystem::Lifecycle, "r1"),
            "_dgp/leases/lifecycle/r1.json"
        );
        assert_ne!(
            S3Lease::object_key(LeaseSubsystem::Replication, "r1"),
            S3Lease::object_key(LeaseSubsystem::Lifecycle, "r1")
        );
    }
}
