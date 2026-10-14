// SPDX-License-Identifier: BUSL-1.1

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
//!
//! Clock skew: expiry is judged by the S3 server's clock (the lease object's
//! `Last-Modified` against the GET's `Date`, see `server_clock`), so node
//! clocks do not matter. A body from the previous release (no `ttl_secs`) is
//! still judged by its writer-clock `expires_at`.
//!
//! Cost: the acquire sends the create-only PUT first and reads the lease
//! only when that PUT answers 412. The holder keeps the ETag of its last
//! write, and the release is one `DELETE If-Match` on it. A free lease costs
//! 2 requests per hold (it cost 4). The body format is unchanged.

use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use serde::{Deserialize, Serialize};

use super::lease::{CoordinationLease, LeaseError, LeaseSubsystem};

/// The lease object body. `epoch` is monotonic (bumped on every steal) — a
/// diagnostic + future fence token; it is NOT used to fence customer-bucket
/// writes (impossible — see the HA reassessment).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Lease {
    pub owner: String,
    pub node_id: String,
    pub epoch: u64,
    pub expires_at: i64,
    /// Per-PROCESS id of the writer. Self-reclaim needs it: a lease that THIS
    /// process wrote belongs to a live worker in it (scheduler, event
    /// consumer, run-now) and must block the others. Absent (older writer) →
    /// empty, which never equals a live process's id.
    #[serde(default)]
    pub boot_id: String,
    /// The TTL the writer granted: expiry is `server age > ttl` (see
    /// `server_clock`). Absent in a body from the previous release, which is
    /// judged by `expires_at` (still written, for that release's readers).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_secs: Option<i64>,
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
    /// A live lease of this same owner: a write of ours whose response
    /// was lost (the SDK retry of the PUT answered 412 against it). We
    /// hold it; `etag` is its current ETag.
    Adopt { etag: String },
    /// A live lease held by someone else → do not acquire.
    Blocked,
}

/// Pure acquire decision (mirrors `job_store::try_acquire_leader_lease`'s tiling).
///
/// A lease is stealable when it is EXPIRED (`expires_at < now`, STRICT) OR when
/// the PREVIOUS PROCESS of this node wrote it (same `node_id`, and its
/// `boot_id` is `previous_boot_id`, the id that this node's data directory
/// recorded before this process started) — the self-reclaim path (E7) lets a
/// rebooted node take back its own still-live lease immediately instead of
/// waiting a full TTL. Any other live lease blocks: one written by this
/// process (another worker in it), by a DIFFERENT node, or by a LIVE twin
/// that shares the node id (docker `--network host`, a shared `DGP_NODE_ID`):
/// a twin's boot id is not this node's previous one.
///
/// A live lease whose `owner` is the caller's is ours already: a write of
/// this owner whose response was lost. Owners carry a per-task uuid
/// (`scheduler:<uuid>`, `run-now:<uuid>`, ...), and each task takes a rule's
/// lease one hold at a time, so no other worker or process matches it.
pub fn plan_acquire(
    current: Option<(&Lease, &str)>,
    now: i64,
    owner: &str,
    my_node_id: &str,
    previous_boot_id: Option<&str>,
) -> AcquireAction {
    match current {
        None => AcquireAction::Create,
        Some((lease, etag)) if lease.owner == owner && lease.expires_at >= now => {
            AcquireAction::Adopt {
                etag: etag.to_string(),
            }
        }
        Some((lease, etag)) => {
            let expired = lease.expires_at < now;
            let from_my_previous_boot = lease.node_id == my_node_id
                && !lease.boot_id.is_empty()
                && previous_boot_id == Some(lease.boot_id.as_str());
            if expired || from_my_previous_boot {
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

/// What a read of the lease key found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseRead {
    Absent,
    /// An object whose body does not parse: not a lease anybody holds.
    Corrupt {
        etag: String,
    },
    Valid {
        lease: Lease,
        etag: String,
    },
}

impl LeaseRead {
    fn valid(&self) -> Option<(&Lease, &str)> {
        match self {
            LeaseRead::Valid { lease, etag } => Some((lease, etag.as_str())),
            _ => None,
        }
    }
}

/// [`plan_acquire`] over a raw read. A corrupt body is replaced with
/// `If-Match(etag)`: reading it as absent sent a create-if-absent, which 412s
/// on the existing key on every pass, so the rule never ran again.
pub fn plan_acquire_read(
    current: &LeaseRead,
    now: i64,
    owner: &str,
    my_node_id: &str,
    previous_boot_id: Option<&str>,
) -> AcquireAction {
    match current {
        LeaseRead::Corrupt { etag } => AcquireAction::Steal {
            etag: etag.clone(),
            next_epoch: 1,
        },
        other => plan_acquire(other.valid(), now, owner, my_node_id, previous_boot_id),
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
    /// This process's id, written into every lease body.
    boot_id: String,
    /// The id of this node's previous process: self-reclaim applies only to
    /// a lease it wrote. `None` = no self-reclaim (a lease frees at its TTL).
    previous_boot_id: Option<String>,
    /// The ETag of each lease this process wrote last: the release deletes
    /// on it without reading the lease first.
    kept: super::cas::KeptEtags,
    /// The bucket enforces `If-Match` on DELETE (the probe proved it). Only
    /// then may a release delete by its kept ETag with no read: on a bucket
    /// that ignores the condition, that DELETE removes a lease a peer stole.
    conditional_delete: bool,
}

impl S3Lease {
    /// A lease with no self-reclaim (a fresh boot id, no previous one).
    pub fn new(client: Client, bucket: String, node_id: String) -> Self {
        Self::with_boot(
            client,
            bucket,
            node_id,
            super::BootIds {
                current: uuid::Uuid::new_v4().to_string(),
                previous: None,
            },
        )
    }

    /// A lease that reclaims the live leases of this node's previous
    /// process (`boot.previous`, see [`super::process_boot_ids`]).
    pub fn with_boot(
        client: Client,
        bucket: String,
        node_id: String,
        boot: super::BootIds,
    ) -> Self {
        Self {
            client,
            bucket,
            node_id,
            boot_id: boot.current,
            previous_boot_id: boot.previous,
            kept: Default::default(),
            conditional_delete: false,
        }
    }

    /// Set what the CAS probe found about `If-Match` on DELETE. The default
    /// (`false`) reads the lease before every release.
    pub fn with_conditional_delete(mut self, enforced: bool) -> Self {
        self.conditional_delete = enforced;
        self
    }

    fn object_key(subsystem: LeaseSubsystem, rule: &str) -> String {
        format!("_dgp/leases/{}/{}.json", subsystem.slug(), rule)
    }

    /// Read the lease object + its ETag. The lease's `expires_at` comes back
    /// re-based onto the caller's clock `now` from the server-clock age
    /// (`server_clock::effective_expires_at`).
    async fn read_lease(&self, key: &str, now: i64) -> Result<LeaseRead, LeaseError> {
        match super::server_clock::get_with_server_age(&self.client, &self.bucket, key).await {
            Ok((out, age)) => {
                let etag = out.e_tag().map(str::to_string).unwrap_or_default();
                let bytes = out
                    .body
                    .collect()
                    .await
                    .map_err(|e| LeaseError::Backend(format!("lease body read: {e}")))?
                    .into_bytes();
                Ok(match serde_json::from_slice::<Lease>(&bytes) {
                    Ok(mut lease) => {
                        lease.expires_at = super::server_clock::effective_expires_at(
                            lease.expires_at,
                            lease.ttl_secs,
                            age,
                            now,
                        );
                        LeaseRead::Valid { lease, etag }
                    }
                    Err(_) => LeaseRead::Corrupt { etag },
                })
            }
            Err(e) => {
                if super::cas::is_object_absent(&super::cas::sdk_error_signal(&*e)) {
                    Ok(LeaseRead::Absent)
                } else {
                    Err(LeaseError::Backend(format!("{e:?}")))
                }
            }
        }
    }

    fn body_for(&self, owner: &str, epoch: u64, expires_at: i64, ttl_secs: i64) -> ByteStream {
        let lease = Lease {
            owner: owner.to_string(),
            node_id: self.node_id.clone(),
            epoch,
            expires_at,
            boot_id: self.boot_id.clone(),
            ttl_secs: Some(ttl_secs.max(1)),
        };
        ByteStream::from(serde_json::to_vec(&lease).unwrap_or_default())
    }

    /// PUT the lease object with a precondition. `precondition`:
    /// `None` → `If-None-Match:*` (create-only); `Some(etag)` → `If-Match(etag)`.
    /// Returns `Ok(true)` on success, `Ok(false)` on a 412 (lost the race),
    /// `Err` on any other failure. Keeps the written ETag for the release.
    async fn put_lease(
        &self,
        key: &str,
        owner: &str,
        body: ByteStream,
        precondition: Option<&str>,
    ) -> Result<bool, LeaseError> {
        let outcome =
            super::cas::conditional_put_etag(&self.client, &self.bucket, key, body, precondition)
                .await;
        self.kept.note(key, owner, &outcome);
        outcome
            .map(|written| written.is_some())
            .map_err(LeaseError::Backend)
    }
}

impl CoordinationLease for S3Lease {
    async fn try_acquire(
        &self,
        subsystem: LeaseSubsystem,
        rule: &str,
        owner: &str,
        now: i64,
        ttl_secs: i64,
    ) -> Result<bool, LeaseError> {
        let key = Self::object_key(subsystem, rule);
        let expires_at = now.saturating_add(ttl_secs.max(1));
        // Write first: a free lease costs this one create-only PUT. Its 412
        // says that a lease object exists; only then read it.
        if self
            .put_lease(
                &key,
                owner,
                self.body_for(owner, 1, expires_at, ttl_secs),
                None,
            )
            .await?
        {
            return Ok(true);
        }
        let current = self.read_lease(&key, now).await?;
        match plan_acquire_read(
            &current,
            now,
            owner,
            &self.node_id,
            self.previous_boot_id.as_deref(),
        ) {
            AcquireAction::Blocked => Ok(false),
            AcquireAction::Adopt { etag } => {
                self.kept.note(&key, owner, &Ok::<_, ()>(Some(etag)));
                Ok(true)
            }
            AcquireAction::Create => {
                self.put_lease(
                    &key,
                    owner,
                    self.body_for(owner, 1, expires_at, ttl_secs),
                    None,
                )
                .await
            }
            AcquireAction::Steal { etag, next_epoch } => {
                self.put_lease(
                    &key,
                    owner,
                    self.body_for(owner, next_epoch, expires_at, ttl_secs),
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
    ) -> Result<(), LeaseError> {
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
        let mut last_err: Option<LeaseError> = None;
        for _ in 0..RENEW_MAX_ATTEMPTS {
            let elapsed_secs = started.elapsed().as_secs() as i64;
            let effective_now = now.saturating_add(elapsed_secs);
            let expires_at = effective_now.saturating_add(ttl_secs.max(1));
            let current = match self.read_lease(&key, effective_now).await {
                Ok(c) => c,
                Err(e) => {
                    last_err = Some(e);
                    continue;
                }
            };
            match plan_renew(current.valid(), effective_now, owner) {
                RenewAction::Lost => return Err(LeaseError::Lost),
                RenewAction::Renew { etag, epoch } => {
                    match self
                        .put_lease(
                            &key,
                            owner,
                            self.body_for(owner, epoch, expires_at, ttl_secs),
                            Some(&etag),
                        )
                        .await
                    {
                        Ok(true) => return Ok(()),
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
        Err(last_err
            .unwrap_or_else(|| LeaseError::Backend("lease renew exhausted retries".to_string())))
    }

    async fn release(
        &self,
        subsystem: LeaseSubsystem,
        rule: &str,
        owner: &str,
    ) -> Result<(), LeaseError> {
        let key = Self::object_key(subsystem, rule);
        // Owner-scoped release: only delete the object if WE still own it, so a
        // release can't clobber a lease a peer legitimately stole. Our last
        // write of it: delete it only while it is still that write (a 412 =
        // stolen). No read, but only on a bucket that enforces the condition
        // on DELETE.
        let kept = self.kept.take(&key, owner);
        if let Some(etag) = kept.filter(|_| self.conditional_delete) {
            super::cas::delete_if_match(&self.client, &self.bucket, &key, &etag).await;
            return Ok(());
        }
        // No kept ETag (our last write of it lost or failed), or a bucket
        // that may ignore `If-Match` on DELETE: read first, delete only our
        // own LIVE lease (see `cas::read_first_release_may_delete`).
        let now = crate::event_outbox::current_unix_seconds();
        if let LeaseRead::Valid { lease, etag } = self.read_lease(&key, now).await? {
            if super::cas::read_first_release_may_delete(&lease.owner, lease.expires_at, owner, now)
            {
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

    async fn is_held(
        &self,
        subsystem: LeaseSubsystem,
        rule: &str,
        now: i64,
    ) -> Result<bool, LeaseError> {
        let key = Self::object_key(subsystem, rule);
        // Held = the lease object exists AND hasn't lapsed. A transient read
        // error is surfaced (the caller treats an Err conservatively).
        Ok(self
            .read_lease(&key, now)
            .await?
            .valid()
            .is_some_and(|(lease, _etag)| lease.expires_at >= now))
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
            boot_id: "boot-old".into(),
            ttl_secs: None,
        }
    }

    /// A lease on a fake bucket that proved conditional DELETE.
    async fn fake_lease_proven() -> (S3Lease, std::sync::Arc<crate::storage::FakeS3>) {
        let (lease, fake) = fake_lease().await;
        (lease.with_conditional_delete(true), fake)
    }

    async fn fake_lease() -> (S3Lease, std::sync::Arc<crate::storage::FakeS3>) {
        let (endpoint, fake) = crate::storage::fake_s3::start().await;
        let client =
            crate::config_db_sync::ConfigDbSync::build_client(&crate::config::BackendConfig::S3 {
                endpoint: Some(endpoint),
                region: "us-east-1".into(),
                force_path_style: true,
                access_key_id: Some("k".into()),
                secret_access_key: Some("s".into()),
                allow_local: true,
                session_token: None,
            })
            .await
            .unwrap();
        (S3Lease::new(client, "coord".into(), "node".into()), fake)
    }

    fn methods(fake: &crate::storage::FakeS3) -> Vec<String> {
        fake.requests()
            .iter()
            .map(|r| r.split(' ').next().unwrap_or("").to_string())
            .collect()
    }

    /// A free lease cost 4 requests per hold (GET + PUT, GET + DELETE);
    /// write-first + the kept ETag make it 2. A held lease is read only
    /// after the create answers 412; a renew keeps the new ETag.
    #[tokio::test]
    async fn a_free_lease_costs_one_put_and_one_delete() {
        let (lease, fake) = fake_lease_proven().await;
        let sub = LeaseSubsystem::Replication;
        let now = crate::event_outbox::current_unix_seconds();
        assert!(lease.try_acquire(sub, "r", "o1", now, 300).await.unwrap());
        lease.release(sub, "r", "o1").await.unwrap();
        assert_eq!(methods(&fake), ["PUT", "DELETE"]);

        fake.clear();
        assert!(lease.try_acquire(sub, "r", "o2", now, 300).await.unwrap());
        assert!(!lease.try_acquire(sub, "r", "o3", now, 300).await.unwrap());
        lease.renew(sub, "r", "o2", now, 300).await.unwrap();
        lease.release(sub, "r", "o2").await.unwrap();
        assert_eq!(
            methods(&fake),
            ["PUT", "PUT", "GET", "GET", "PUT", "DELETE"]
        );
        assert!(!lease.is_held(sub, "r", now).await.unwrap(), "released");
    }

    /// A backend that ignores `If-Match` on DELETE: the stale leader's
    /// release by its kept ETag deleted the peer's live lease. A lease
    /// whose bucket did not prove conditional DELETE reads first.
    #[tokio::test]
    async fn a_stale_release_keeps_the_peer_lease_when_delete_ignores_if_match() {
        let (lease, fake) = fake_lease().await;
        fake.set_delete_if_match_ignored(true);
        let sub = LeaseSubsystem::Replication;
        let now = crate::event_outbox::current_unix_seconds();
        assert!(lease.try_acquire(sub, "r", "old", now, 300).await.unwrap());
        let lapsed = serde_json::json!({
            "owner": "old", "node_id": "node", "epoch": 1, "expires_at": now - 1
        });
        lease
            .client
            .put_object()
            .bucket("coord")
            .key(S3Lease::object_key(sub, "r"))
            .body(aws_sdk_s3::primitives::ByteStream::from(
                serde_json::to_vec(&lapsed).unwrap(),
            ))
            .send()
            .await
            .unwrap();
        assert!(lease.try_acquire(sub, "r", "new", now, 300).await.unwrap());
        lease.release(sub, "r", "old").await.unwrap();
        assert!(
            lease.is_held(sub, "r", now).await.unwrap(),
            "the stale release deleted the peer's lease"
        );
    }

    fn lossy_lease() -> (S3Lease, crate::coordination::test_s3::Lossy) {
        let lossy = crate::coordination::test_s3::Lossy::default();
        let client = crate::coordination::test_s3::lossy_client(&lossy);
        let lease =
            S3Lease::new(client, "coord".into(), "node".into()).with_conditional_delete(true);
        (lease, lossy)
    }

    /// The create PUT lands, its response is lost, the SDK retry gets 412
    /// against our own write. The read finds a live lease of this owner:
    /// ours. It answered "blocked", and the rule stalled for the lease TTL
    /// (300 s) on every node.
    #[tokio::test]
    async fn a_create_whose_response_was_lost_is_held() {
        let (lease, lossy) = lossy_lease();
        let sub = LeaseSubsystem::Replication;
        let key = S3Lease::object_key(sub, "r");
        *lossy.lose_puts.lock().unwrap() = 1;
        let now = crate::event_outbox::current_unix_seconds();
        assert!(
            lease
                .try_acquire(sub, "r", "scheduler:1", now, 300)
                .await
                .unwrap(),
            "our own lease blocked us: {:?}",
            lossy.seen()
        );
        // Another worker of this process is still blocked.
        assert!(!lease
            .try_acquire(sub, "r", "event-consumer:1", now, 300)
            .await
            .unwrap());
        lossy.seen.lock().unwrap().clear();
        lease.release(sub, "r", "scheduler:1").await.unwrap();
        assert_eq!(lossy.seen(), ["DELETE"], "the kept ETag was not kept");
        assert!(lossy.body(&key).is_none());
    }

    /// Twin of the reference lock's renew: a renew whose response was lost
    /// re-reads, finds the lease still ours, and is confirmed.
    #[tokio::test]
    async fn a_renew_whose_response_was_lost_is_confirmed() {
        let (lease, lossy) = lossy_lease();
        let sub = LeaseSubsystem::Replication;
        let now = crate::event_outbox::current_unix_seconds();
        assert!(lease.try_acquire(sub, "r", "o", now, 300).await.unwrap());
        *lossy.lose_puts.lock().unwrap() = 1;
        lease.renew(sub, "r", "o", now + 10, 300).await.unwrap();
        lossy.seen.lock().unwrap().clear();
        lease.release(sub, "r", "o").await.unwrap();
        assert_eq!(lossy.seen(), ["DELETE"]);
    }

    /// A release that reads first deletes only a LIVE lease of ours. An
    /// expired one is free to every peer, which may steal it between the
    /// read and the delete; on a bucket that ignores `If-Match` on DELETE,
    /// the delete then removed the peer's lease.
    #[tokio::test]
    async fn a_read_first_release_leaves_an_expired_lease() {
        let (lease, fake) = fake_lease().await;
        fake.set_delete_if_match_ignored(true);
        let sub = LeaseSubsystem::Replication;
        let now = crate::event_outbox::current_unix_seconds();
        let lapsed = serde_json::json!({
            "owner": "old", "node_id": "node", "epoch": 1, "expires_at": now - 1
        });
        lease
            .client
            .put_object()
            .bucket("coord")
            .key(S3Lease::object_key(sub, "r"))
            .body(aws_sdk_s3::primitives::ByteStream::from(
                serde_json::to_vec(&lapsed).unwrap(),
            ))
            .send()
            .await
            .unwrap();
        fake.clear();
        lease.release(sub, "r", "old").await.unwrap();
        assert_eq!(methods(&fake), ["GET"], "an expired lease was deleted");
    }

    fn canned_lease(body: serde_json::Value, age: i64) -> crate::coordination::test_s3::Canned {
        let now = crate::event_outbox::current_unix_seconds();
        crate::coordination::test_s3::Canned {
            body: serde_json::to_vec(&body).unwrap(),
            date: now,
            last_modified: now - age,
            seen: Default::default(),
        }
    }

    /// Expiry by the server clock: a leader whose clock runs 1000 s behind
    /// renewed 20 s ago (TTL 300). Its lease is live; a peer must not steal.
    #[tokio::test]
    async fn a_lease_from_a_slow_clock_is_live_by_server_time() {
        let now = crate::event_outbox::current_unix_seconds();
        let canned = canned_lease(
            serde_json::json!({"owner": "leader", "node_id": "nodeB", "epoch": 2,
                "expires_at": now - 1000 + 300, "ttl_secs": 300}),
            20,
        );
        let l = S3Lease::new(
            crate::coordination::test_s3::client(&canned),
            "coord".into(),
            "nodeA".into(),
        );
        let got = l
            .try_acquire(LeaseSubsystem::Replication, "r", "peer", now, 300)
            .await
            .unwrap();
        assert!(!got, "a live lease was stolen");
        assert_eq!(canned.puts(), 0);
    }

    /// A dead leader with a clock far ahead: the server says 400 s since
    /// its last renew (TTL 300), so the lease is free for a peer.
    #[tokio::test]
    async fn a_lease_from_a_fast_clock_expires_by_server_time() {
        let now = crate::event_outbox::current_unix_seconds();
        let canned = canned_lease(
            serde_json::json!({"owner": "leader", "node_id": "nodeB", "epoch": 2,
                "expires_at": now + 50_000, "ttl_secs": 300}),
            400,
        );
        let l = S3Lease::new(
            crate::coordination::test_s3::client(&canned),
            "coord".into(),
            "nodeA".into(),
        );
        assert!(l
            .try_acquire(LeaseSubsystem::Replication, "r", "peer", now, 300)
            .await
            .unwrap());
        assert_eq!(canned.puts(), 1);
    }

    #[test]
    fn acquire_free_lease_creates() {
        assert_eq!(
            plan_acquire(None, 100, "me", "nodeA", None),
            AcquireAction::Create
        );
    }

    #[test]
    fn acquire_expired_lease_steals_with_bumped_epoch() {
        let l = lease("old", "nodeB", 5, 90); // expired at now=100
        assert_eq!(
            plan_acquire(Some((&l, "etag1")), 100, "me", "nodeA", None),
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
            plan_acquire(Some((&l, "etag1")), 100, "me", "nodeA", None),
            AcquireAction::Blocked
        );
    }

    #[test]
    fn acquire_own_live_lease_self_reclaims() {
        // E7: the still-live lease of this node's PREVIOUS process is
        // reclaimable without waiting for expiry.
        let l = lease("old-task", "nodeA", 5, 160); // live, written by boot-old
        assert_eq!(
            plan_acquire(Some((&l, "etag1")), 100, "me", "nodeA", Some("boot-old")),
            AcquireAction::Steal {
                etag: "etag1".into(),
                next_epoch: 6
            }
        );
    }

    /// Another worker of THIS process (scheduler vs event consumer vs
    /// run-now) holds the live lease: blocked, or they would take it from
    /// each other.
    #[test]
    fn acquire_live_lease_of_this_process_blocks() {
        let l = lease("scheduler", "nodeA", 5, 160);
        assert_eq!(
            plan_acquire(Some((&l, "etag1")), 100, "me", "nodeA", Some("boot-before")),
            AcquireAction::Blocked
        );
        // A lease body from an older release has no boot id: never ours.
        let old = Lease {
            boot_id: String::new(),
            ..l.clone()
        };
        assert_eq!(
            plan_acquire(Some((&old, "etag1")), 100, "me", "nodeA", Some("")),
            AcquireAction::Blocked
        );
    }

    /// Review 4 coordination-2: two LIVE processes with one node id (docker
    /// `--network host` twins, a shared DGP_NODE_ID). Neither may take the
    /// other's live lease: each one's previous boot id is its own.
    #[test]
    fn live_twins_with_one_node_id_block_each_other() {
        let p1 = Lease {
            owner: "p1-worker".into(),
            node_id: "shared-host".into(),
            epoch: 1,
            expires_at: 400,
            boot_id: "boot-p1".into(),
            ttl_secs: Some(300),
        };
        assert_eq!(
            plan_acquire(
                Some((&p1, "e1")),
                100,
                "me",
                "shared-host",
                Some("boot-p2-prev")
            ),
            AcquireAction::Blocked
        );
        assert_eq!(
            plan_acquire(Some((&p1, "e1")), 100, "me", "shared-host", None),
            AcquireAction::Blocked
        );
        // It expires as usual.
        assert!(matches!(
            plan_acquire(Some((&p1, "e1")), 401, "me", "shared-host", None),
            AcquireAction::Steal { .. }
        ));
    }

    #[test]
    fn acquire_boundary_at_exact_expiry_blocks_foreign() {
        // expires_at == now → NOT expired (strict <), so a foreign live lease
        // still blocks (mirrors the job_store `< now` steal predicate).
        let l = lease("held", "nodeB", 5, 100);
        assert_eq!(
            plan_acquire(Some((&l, "e")), 100, "me", "nodeA", None),
            AcquireAction::Blocked
        );
    }

    #[test]
    fn acquire_corrupt_lease_body_is_replaced_by_etag() {
        let c = LeaseRead::Corrupt {
            etag: "junk".into(),
        };
        assert_eq!(
            plan_acquire_read(&c, 100, "me", "nodeA", None),
            AcquireAction::Steal {
                etag: "junk".into(),
                next_epoch: 1
            }
        );
        // A valid read still goes through the normal truth table.
        let live = LeaseRead::Valid {
            lease: lease("held", "nodeB", 5, 160),
            etag: "e".into(),
        };
        assert_eq!(
            plan_acquire_read(&live, 100, "me", "nodeA", None),
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
    }
}
