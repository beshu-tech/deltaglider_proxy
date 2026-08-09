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
//! object we wrote. `node_id` is the durable node identity, used only for
//! SELF-RECLAIM: a lock left behind by a crashed-then-restarted same node (or a
//! re-entrant acquire on the same node) is reclaimable immediately rather than
//! after a full TTL. A live lock owned by a DIFFERENT node blocks.

use std::time::Duration;

use async_trait::async_trait;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use serde::{Deserialize, Serialize};

/// Default crash-backstop TTL for a held lock. Comfortably longer than the
/// longest reference critical section (an xdelta3 encode of a `max_object_size`
/// object), so a live holder is never mistaken for a dead one, while a genuinely
/// dead holder's lock still frees within a bounded window.
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

/// What `try_acquire` should do given the CURRENT lock object state.
#[derive(Debug, PartialEq, Eq)]
pub enum LockAction {
    /// No object exists → create with `If-None-Match:*`.
    Create,
    /// An object exists but is expired or is OURS (same node) → steal with
    /// `If-Match(etag)`, carrying `next_epoch`.
    Steal { etag: String, next_epoch: u64 },
    /// A live lock held by a different node → cannot acquire this pass.
    Blocked,
}

/// Pure acquire decision (mirrors `s3_lease::plan_acquire`). Stealable when
/// EXPIRED (`expires_at < now`, STRICT — the exact-expiry instant blocks a
/// foreign holder, never simultaneously stealable-and-live) OR when the lock is
/// OURS (`node_id` match, self-reclaim). A live foreign lock blocks.
pub fn plan_lock_acquire(
    current: Option<(&RefLock, &str)>,
    now: i64,
    my_node_id: &str,
) -> LockAction {
    match current {
        None => LockAction::Create,
        Some((lock, etag)) => {
            let expired = lock.expires_at < now;
            let mine = lock.node_id == my_node_id;
            if expired || mine {
                LockAction::Steal {
                    etag: etag.to_string(),
                    next_epoch: lock.epoch.saturating_add(1),
                }
            } else {
                LockAction::Blocked
            }
        }
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

    /// The crash-backstop TTL applied to a freshly acquired lock, in seconds.
    fn ttl_secs(&self) -> i64 {
        DEFAULT_LOCK_TTL_SECS
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

    async fn read_lock(&self, key: &str) -> Result<Option<(RefLock, String)>, String> {
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
                match serde_json::from_slice::<RefLock>(&bytes) {
                    Ok(lock) => Ok(Some((lock, etag))),
                    // A corrupt/foreign object at the key → treat as absent so a
                    // create-if-absent can reclaim it (it 412s if a valid
                    // concurrent writer beat us, which is correct).
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
        match plan_lock_acquire(
            current.as_ref().map(|(l, e)| (l, e.as_str())),
            now,
            &self.node_id,
        ) {
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
        if let Some((lock, etag)) = self.read_lock(key).await? {
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

    #[test]
    fn acquire_free_creates() {
        assert_eq!(plan_lock_acquire(None, 100, "nodeA"), LockAction::Create);
    }

    #[test]
    fn acquire_expired_steals_with_bumped_epoch() {
        let l = obj("old", "nodeB", 5, 90); // expired at now=100
        assert_eq!(
            plan_lock_acquire(Some((&l, "e1")), 100, "nodeA"),
            LockAction::Steal {
                etag: "e1".into(),
                next_epoch: 6
            }
        );
    }

    #[test]
    fn acquire_live_foreign_blocks() {
        let l = obj("held", "nodeB", 5, 160); // live at now=100
        assert_eq!(
            plan_lock_acquire(Some((&l, "e1")), 100, "nodeA"),
            LockAction::Blocked
        );
    }

    #[test]
    fn acquire_own_live_self_reclaims() {
        // Same node_id → reclaimable without waiting for expiry (re-entrancy /
        // crash-restart), so a same-node re-acquire never deadlocks on itself.
        let l = obj("old-token", "nodeA", 5, 160); // live, but ours
        assert_eq!(
            plan_lock_acquire(Some((&l, "e1")), 100, "nodeA"),
            LockAction::Steal {
                etag: "e1".into(),
                next_epoch: 6
            }
        );
    }

    #[test]
    fn acquire_at_exact_expiry_blocks_foreign() {
        // expires_at == now → NOT expired (strict <), so a foreign live lock
        // still blocks — the exact instant is never both live and stealable.
        let l = obj("held", "nodeB", 5, 100);
        assert_eq!(
            plan_lock_acquire(Some((&l, "e")), 100, "nodeA"),
            LockAction::Blocked
        );
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
