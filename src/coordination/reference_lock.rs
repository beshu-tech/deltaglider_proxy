// SPDX-License-Identifier: BUSL-1.1

//! [`ReferenceLock`] — a cross-instance, per-deltaspace MUTEX over the
//! coordination bucket, closing the `B1 (open)` hazard: two proxy instances
//! writing the same deltaspace's `reference.bin` concurrently can corrupt it
//! (each sees `has_reference == false` and both create a baseline), orphaning
//! every delta in the prefix.
//!
//! ## Why a new primitive (not [`super::S3Lease`])
//!
//! `S3Lease` is a TTL LEADER lease: acquired once, heartbeat-renewed for
//! minutes, stolen on death. The reference read-modify-write is the opposite
//! shape — a SHORT critical section (has_reference → set_reference_baseline →
//! put_delta, typically milliseconds to a few seconds for the xdelta3 encode)
//! that wants plain mutual exclusion, acquired and released within one store
//! call, with a TTL only as a crash backstop. So this is a distinct primitive,
//! though it reuses the exact CAS mechanics (`If-None-Match:*` to create,
//! `If-Match:<etag>` to steal an expired one, delete-if-owner to release) proven
//! in `config_db_sync` and `s3_lease`.
//!
//! ## Layering with the in-process lock
//!
//! This sits INSIDE the engine's in-process `prefix_locks` mutex, never replaces
//! it. The in-process mutex serializes same-node threads (so at most one thread
//! per node is ever in the critical section for a deltaspace); this lock
//! serializes across NODES. Single-instance deployments hold no `ReferenceLock`
//! at all (the engine's field is `None`) and pay zero S3 round-trips — the
//! in-process mutex is the whole story, exactly as before.
//!
//! ## Ownership
//!
//! `owner` is a fresh token per acquisition, so `release` deletes precisely the
//! object we wrote. `node_id` is diagnostic provenance only. A live lock is
//! NEVER stolen, not even by the "same" node: node ids come from `HOSTNAME`,
//! which two live replicas can share, and an engine rebuild runs two engines
//! in one process. A lock that a crashed node leaves behind frees at its TTL.
//!
//! ## Holding for a long time
//!
//! A streaming encode can outlast the TTL. The holder renews the lock
//! (`renew`, owner-scoped `If-Match` extend) every `renew_interval`, and checks
//! that it still holds it right before each commit (see the engine's
//! `ReferenceLockGuard::ensure_held`). A holder that cannot renew stops before
//! it writes.
//!
//! ## Clock skew
//!
//! `expires_at` is the writer's wall clock; a peer compares it with its own.
//! The holder trusts its hold for at most `ttl / 2` after its last confirmed
//! renew (a monotonic clock), and renews every `ttl / 4`. So a peer steals a
//! live-held lock only when its clock runs more than `ttl / 2` (60s at the
//! default TTL) ahead of the holder's: the supported skew bound. NTP-synced
//! nodes are far inside it.
//!
//! ## Fencing
//!
//! `epoch` bumps on every steal, but the data writes go to customer buckets,
//! which cannot check it. So the engine fences on reference.bin itself: the
//! acquire observes its ETag (or its absence), and every reference write of
//! the hold is conditional on that observation (`If-Match` /
//! `If-None-Match:*`, see `StorageBackend::write_reference_fenced`). A holder
//! whose lock lapsed while a peer wrote gets a precondition failure, never an
//! overwrite. The renew-and-check above still stops most such writes early.

use std::time::Duration;

use async_trait::async_trait;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use serde::{Deserialize, Serialize};

/// Default crash-backstop TTL for a held lock. A live holder renews it every
/// `ttl / 4`, so it keeps the lock through an encode of any length; a dead
/// holder's lock frees within one TTL.
pub const DEFAULT_LOCK_TTL_SECS: i64 = 120;
/// Default ceiling on how long a writer waits to acquire before failing the PUT.
/// A peer holds the lock only for its own short critical section, so contention
/// normally clears in well under a second; the ceiling exists so a wedged/dead
/// holder surfaces a clean error to the client instead of hanging forever.
pub const DEFAULT_ACQUIRE_TIMEOUT_SECS: u64 = 30;
/// Poll interval while waiting for a contended lock to free.
const ACQUIRE_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// The lock object body. `epoch` is monotonic (bumped on every steal) — purely
/// diagnostic provenance.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RefLock {
    pub owner: String,
    pub node_id: String,
    pub epoch: u64,
    pub expires_at: i64,
}

/// What a read of the lock key found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observed {
    Absent,
    /// An object whose body does not parse. It is not a lock anybody holds.
    Corrupt {
        etag: String,
    },
    Held {
        lock: RefLock,
        etag: String,
    },
}

/// What `try_acquire` should do given the CURRENT lock object state.
#[derive(Debug, PartialEq, Eq)]
pub enum LockAction {
    /// No object exists → create with `If-None-Match:*`.
    Create,
    /// An expired lock or an unparsable body → replace with `If-Match(etag)`,
    /// carrying `next_epoch`. (A create-if-absent on an existing key 412s on
    /// every pass: a corrupt body wedged the deltaspace forever.)
    Steal { etag: String, next_epoch: u64 },
    /// A live lock → cannot acquire this pass, whoever holds it.
    Blocked,
}

/// Pure acquire decision. Stealable only when EXPIRED (`expires_at < now`,
/// STRICT — the exact-expiry instant is never both live and stealable) or
/// corrupt. There is no same-node self-reclaim (see the module doc).
pub fn plan_lock_acquire(current: &Observed, now: i64) -> LockAction {
    match current {
        Observed::Absent => LockAction::Create,
        Observed::Corrupt { etag } => LockAction::Steal {
            etag: etag.clone(),
            next_epoch: 1,
        },
        Observed::Held { lock, etag } if lock.expires_at < now => LockAction::Steal {
            etag: etag.clone(),
            next_epoch: lock.epoch.saturating_add(1),
        },
        Observed::Held { .. } => LockAction::Blocked,
    }
}

/// What `renew` should do given the current lock state.
#[derive(Debug, PartialEq, Eq)]
pub enum RenewLockAction {
    /// Still ours and live → extend with `If-Match(etag)`.
    Renew { etag: String, epoch: u64 },
    /// Gone, corrupt, another owner's, or lapsed → the hold is lost.
    Lost,
}

/// Pure renew decision (`>= now`, non-strict: tiles with the strict steal).
pub fn plan_lock_renew(current: &Observed, now: i64, owner: &str) -> RenewLockAction {
    match current {
        Observed::Held { lock, etag } if lock.owner == owner && lock.expires_at >= now => {
            RenewLockAction::Renew {
                etag: etag.clone(),
                epoch: lock.epoch,
            }
        }
        _ => RenewLockAction::Lost,
    }
}

/// A per-deltaspace cross-instance mutex.
#[async_trait]
pub trait ReferenceLock: Send + Sync {
    /// One acquisition attempt. `Ok(true)` = acquired (we now hold it), `Ok(false)`
    /// = a live foreign holder blocks us (caller should back off and retry),
    /// `Err` = an I/O error (caller treats conservatively — fail the write rather
    /// than risk two baselines).
    async fn try_acquire(&self, key: &str, owner: &str, now: i64) -> Result<bool, String>;

    /// Release the lock, but only if we still own it (owner-scoped delete), so a
    /// release can never clobber a lock a peer legitimately stole after our TTL
    /// lapsed. Best-effort: the TTL backstops a failed release.
    async fn release(&self, key: &str, owner: &str) -> Result<(), String>;

    /// Extend a lock this owner still holds. `Ok(false)` = lost (stolen,
    /// lapsed, gone): the holder must not commit. `Err` = could not tell.
    async fn renew(&self, key: &str, owner: &str, now: i64) -> Result<bool, String>;

    /// The crash-backstop TTL applied to a freshly acquired lock, in seconds.
    fn ttl_secs(&self) -> i64 {
        DEFAULT_LOCK_TTL_SECS
    }

    /// How often a holder renews, and how old a confirmation may be before a
    /// commit re-confirms it. `ttl / 4` (see "Clock skew" in the module doc).
    fn renew_interval(&self) -> Duration {
        Duration::from_millis((self.ttl_secs().max(1) as u64) * 1000 / 4)
    }

    /// How long a writer waits to acquire before failing the write closed.
    fn acquire_timeout(&self) -> Duration {
        Duration::from_secs(DEFAULT_ACQUIRE_TIMEOUT_SECS)
    }
}

/// Stable, filesystem/S3-safe object key for a deltaspace lock. Hashes
/// `bucket \0 deltaspace` so arbitrary prefix characters and lengths can't
/// produce an unsafe or colliding key, and two buckets sharing a prefix name
/// never share a lock.
pub fn lock_object_key(bucket: &str, deltaspace: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bucket.as_bytes());
    h.update([0u8]);
    h.update(deltaspace.as_bytes());
    format!("_dgp/locks/reference/{}.json", hex::encode(h.finalize()))
}

/// The concrete lock over a CAS-capable coordination bucket.
pub struct S3ReferenceLock {
    client: Client,
    bucket: String,
    node_id: String,
    ttl_secs: i64,
    acquire_timeout: Duration,
}

impl S3ReferenceLock {
    pub fn new(client: Client, bucket: String, node_id: String) -> Self {
        Self {
            client,
            bucket,
            node_id,
            ttl_secs: DEFAULT_LOCK_TTL_SECS,
            acquire_timeout: Duration::from_secs(DEFAULT_ACQUIRE_TIMEOUT_SECS),
        }
    }

    pub fn with_tunables(mut self, ttl_secs: i64, acquire_timeout_secs: u64) -> Self {
        self.ttl_secs = ttl_secs.max(1);
        self.acquire_timeout = Duration::from_secs(acquire_timeout_secs.max(1));
        self
    }

    async fn read_lock(&self, key: &str) -> Result<Observed, String> {
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
                    .map_err(|e| format!("lock body read: {e}"))?
                    .into_bytes();
                Ok(match serde_json::from_slice::<RefLock>(&bytes) {
                    Ok(lock) => Observed::Held { lock, etag },
                    Err(_) => Observed::Corrupt { etag },
                })
            }
            Err(e) => {
                if crate::config_db_sync::is_object_absent(
                    &crate::config_db_sync::sdk_error_signal(&e),
                ) {
                    Ok(Observed::Absent)
                } else {
                    Err(format!("{e:?}"))
                }
            }
        }
    }

    fn body_for(&self, owner: &str, epoch: u64, expires_at: i64) -> ByteStream {
        let lock = RefLock {
            owner: owner.to_string(),
            node_id: self.node_id.clone(),
            epoch,
            expires_at,
        };
        ByteStream::from(serde_json::to_vec(&lock).unwrap_or_default())
    }

    async fn put_lock(
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
impl ReferenceLock for S3ReferenceLock {
    async fn try_acquire(&self, key: &str, owner: &str, now: i64) -> Result<bool, String> {
        let current = self.read_lock(key).await?;
        let expires_at = now.saturating_add(self.ttl_secs.max(1));
        match plan_lock_acquire(&current, now) {
            LockAction::Blocked => Ok(false),
            LockAction::Create => {
                self.put_lock(key, self.body_for(owner, 1, expires_at), None)
                    .await
            }
            LockAction::Steal { etag, next_epoch } => {
                self.put_lock(
                    key,
                    self.body_for(owner, next_epoch, expires_at),
                    Some(&etag),
                )
                .await
            }
        }
    }

    async fn release(&self, key: &str, owner: &str) -> Result<(), String> {
        if let Observed::Held { lock, etag } = self.read_lock(key).await? {
            if lock.owner == owner {
                let _ = self
                    .client
                    .delete_object()
                    .bucket(&self.bucket)
                    .key(key)
                    .if_match(&etag)
                    .send()
                    .await;
            }
        }
        Ok(())
    }

    async fn renew(&self, key: &str, owner: &str, now: i64) -> Result<bool, String> {
        let current = self.read_lock(key).await?;
        match plan_lock_renew(&current, now, owner) {
            RenewLockAction::Lost => Ok(false),
            // A 412 here means the object moved under us: lost.
            RenewLockAction::Renew { etag, epoch } => {
                let expires_at = now.saturating_add(self.ttl_secs.max(1));
                self.put_lock(key, self.body_for(owner, epoch, expires_at), Some(&etag))
                    .await
            }
        }
    }

    fn ttl_secs(&self) -> i64 {
        self.ttl_secs
    }

    fn acquire_timeout(&self) -> Duration {
        self.acquire_timeout
    }
}

/// Block until the per-deltaspace lock is acquired or `deadline` passes.
///
/// Returns `Ok(true)` once held, `Ok(false)` if the acquire timeout elapsed
/// while a peer kept the lock (the caller fails the write with a clear "busy"
/// error rather than risk a second baseline), and `Err` on a hard I/O error
/// (also fail-closed). `now_fn` supplies the clock so the whole loop is
/// unit-testable against a mock lock without real time.
pub async fn acquire_blocking(
    lock: &dyn ReferenceLock,
    key: &str,
    owner: &str,
    deadline: std::time::Instant,
    now_fn: &(dyn Fn() -> i64 + Send + Sync),
) -> Result<bool, String> {
    loop {
        if lock.try_acquire(key, owner, now_fn()).await? {
            return Ok(true);
        }
        if std::time::Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(ACQUIRE_POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::sync::Arc;
    use tokio::sync::Mutex as TokioMutex;

    fn obj(owner: &str, node: &str, epoch: u64, expires: i64) -> RefLock {
        RefLock {
            owner: owner.into(),
            node_id: node.into(),
            epoch,
            expires_at: expires,
        }
    }

    fn held(l: RefLock) -> Observed {
        Observed::Held {
            lock: l,
            etag: "e1".into(),
        }
    }

    #[test]
    fn acquire_free_creates() {
        assert_eq!(
            plan_lock_acquire(&Observed::Absent, 100),
            LockAction::Create
        );
    }

    #[test]
    fn acquire_expired_steals_with_bumped_epoch() {
        let l = obj("old", "nodeB", 5, 90); // expired at now=100
        assert_eq!(
            plan_lock_acquire(&held(l), 100),
            LockAction::Steal {
                etag: "e1".into(),
                next_epoch: 6
            }
        );
    }

    #[test]
    fn acquire_live_foreign_blocks() {
        let l = obj("held", "nodeB", 5, 160); // live at now=100
        assert_eq!(plan_lock_acquire(&held(l), 100), LockAction::Blocked);
    }

    /// No same-node self-reclaim: two replicas can share a HOSTNAME-derived
    /// node id, and an engine rebuild runs two engines in one process. A
    /// live lock blocks whoever asks; a crashed holder's lock frees at TTL.
    #[test]
    fn acquire_live_lock_blocks_even_the_same_node() {
        let l = obj("old-token", "nodeA", 5, 160);
        assert_eq!(plan_lock_acquire(&held(l), 100), LockAction::Blocked);
    }

    /// An unparsable body at the key is not a lock. Create-if-absent 412s on
    /// it forever, so it must be replaced with `If-Match`.
    #[test]
    fn acquire_corrupt_body_is_replaced_by_etag() {
        let c = Observed::Corrupt {
            etag: "junk".into(),
        };
        assert_eq!(
            plan_lock_acquire(&c, 100),
            LockAction::Steal {
                etag: "junk".into(),
                next_epoch: 1
            }
        );
    }

    #[test]
    fn acquire_at_exact_expiry_blocks_foreign() {
        // expires_at == now → NOT expired (strict <), so a foreign live lock
        // still blocks — the exact instant is never both live and stealable.
        let l = obj("held", "nodeB", 5, 100);
        assert_eq!(plan_lock_acquire(&held(l), 100), LockAction::Blocked);
    }

    #[test]
    fn renew_truth_table() {
        let mine = obj("me", "nodeA", 7, 100);
        // Live (and the exact expiry instant, non-strict) → renew.
        assert_eq!(
            plan_lock_renew(&held(mine.clone()), 100, "me"),
            RenewLockAction::Renew {
                etag: "e1".into(),
                epoch: 7
            }
        );
        // Lapsed → lost (a peer may already hold it).
        assert_eq!(
            plan_lock_renew(&held(mine), 101, "me"),
            RenewLockAction::Lost
        );
        // Stolen by another owner, gone, or corrupt → lost.
        assert_eq!(
            plan_lock_renew(&held(obj("peer", "nodeB", 8, 500)), 100, "me"),
            RenewLockAction::Lost
        );
        assert_eq!(
            plan_lock_renew(&Observed::Absent, 100, "me"),
            RenewLockAction::Lost
        );
        assert_eq!(
            plan_lock_renew(&Observed::Corrupt { etag: "x".into() }, 100, "me"),
            RenewLockAction::Lost
        );
    }

    #[test]
    fn renew_interval_is_a_quarter_ttl() {
        struct T;
        #[async_trait]
        impl ReferenceLock for T {
            async fn try_acquire(&self, _: &str, _: &str, _: i64) -> Result<bool, String> {
                Ok(true)
            }
            async fn release(&self, _: &str, _: &str) -> Result<(), String> {
                Ok(())
            }
            async fn renew(&self, _: &str, _: &str, _: i64) -> Result<bool, String> {
                Ok(true)
            }
        }
        assert_eq!(T.renew_interval(), Duration::from_secs(30));
    }

    #[test]
    fn lock_key_is_stable_bucket_scoped_and_safe() {
        let a = lock_object_key("bucket-a", "ror/builds");
        let b = lock_object_key("bucket-b", "ror/builds");
        // Same prefix in different buckets must NOT collide.
        assert_ne!(a, b);
        // Stable and charset-safe (no raw prefix slashes/chars leak in).
        assert_eq!(a, lock_object_key("bucket-a", "ror/builds"));
        assert!(a.starts_with("_dgp/locks/reference/") && a.ends_with(".json"));
        assert!(!a.contains("ror/builds"));
    }

    #[test]
    fn lock_json_round_trips() {
        let l = obj("o", "n", 3, 1783000000);
        let bytes = serde_json::to_vec(&l).unwrap();
        assert_eq!(serde_json::from_slice::<RefLock>(&bytes).unwrap(), l);
    }

    // --- acquire_blocking loop, driven against an in-memory mock lock ---

    /// A mock that a test can pre-load to block N attempts then succeed, and
    /// that records how many acquire attempts it saw.
    struct MockLock {
        // Number of times try_acquire should return Ok(false) before Ok(true).
        block_until: AtomicI64,
        attempts: AtomicI64,
        held_by: TokioMutex<Option<String>>,
    }

    #[async_trait]
    impl ReferenceLock for MockLock {
        async fn try_acquire(&self, _key: &str, owner: &str, _now: i64) -> Result<bool, String> {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            if self.block_until.fetch_sub(1, Ordering::SeqCst) > 0 {
                return Ok(false);
            }
            *self.held_by.lock().await = Some(owner.to_string());
            Ok(true)
        }
        async fn release(&self, _key: &str, owner: &str) -> Result<(), String> {
            let mut h = self.held_by.lock().await;
            if h.as_deref() == Some(owner) {
                *h = None;
            }
            Ok(())
        }
        async fn renew(&self, _key: &str, owner: &str, _now: i64) -> Result<bool, String> {
            Ok(self.held_by.lock().await.as_deref() == Some(owner))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn acquire_blocking_succeeds_after_contention() {
        let lock = MockLock {
            block_until: AtomicI64::new(3), // blocked 3 times, then free
            attempts: AtomicI64::new(0),
            held_by: TokioMutex::new(None),
        };
        let now = Arc::new(AtomicI64::new(1000));
        let now2 = now.clone();
        let now_fn = move || now2.load(Ordering::SeqCst);
        let deadline = tokio::time::Instant::now().into_std() + Duration::from_secs(30);
        let got = acquire_blocking(&lock, "k", "owner-1", deadline, &now_fn)
            .await
            .unwrap();
        assert!(got, "must eventually acquire once the peer frees it");
        assert_eq!(lock.attempts.load(Ordering::SeqCst), 4, "3 blocked + 1 win");
        assert_eq!(*lock.held_by.lock().await, Some("owner-1".to_string()));
    }

    #[tokio::test(start_paused = true)]
    async fn acquire_blocking_times_out_when_never_free() {
        let lock = MockLock {
            block_until: AtomicI64::new(i64::MAX), // never frees
            attempts: AtomicI64::new(0),
            held_by: TokioMutex::new(None),
        };
        let now_fn = || 1000i64;
        // Deadline already in the past → exactly one attempt, then give up.
        let deadline = tokio::time::Instant::now().into_std();
        let got = acquire_blocking(&lock, "k", "owner-1", deadline, &now_fn)
            .await
            .unwrap();
        assert!(!got, "must report not-acquired rather than hang forever");
    }

    #[tokio::test]
    async fn acquire_blocking_propagates_io_error_fail_closed() {
        struct ErrLock;
        #[async_trait]
        impl ReferenceLock for ErrLock {
            async fn try_acquire(&self, _k: &str, _o: &str, _n: i64) -> Result<bool, String> {
                Err("coordination bucket unreachable".into())
            }
            async fn release(&self, _k: &str, _o: &str) -> Result<(), String> {
                Ok(())
            }
            async fn renew(&self, _k: &str, _o: &str, _n: i64) -> Result<bool, String> {
                Err("coordination bucket unreachable".into())
            }
        }
        let now_fn = || 1000i64;
        let deadline = tokio::time::Instant::now().into_std() + Duration::from_secs(30);
        let res = acquire_blocking(&ErrLock, "k", "o", deadline, &now_fn).await;
        assert!(
            res.is_err(),
            "an I/O error must surface so the caller fails closed"
        );
    }
}
