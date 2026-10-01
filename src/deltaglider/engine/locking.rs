// SPDX-License-Identifier: BUSL-1.1

//! The per-deltaspace locks: the in-process prefix mutex and the optional
//! cross-instance reference lock, with the only engine path to reference writes.

use super::*;
use crate::coordination::ReferenceLock;

/// RAII guard for the optional cross-instance reference lock. Held for the
/// duration of a reference read-modify-write, inside the in-process prefix
/// mutex. It holds one lock object, or two while a bucket's lock key changes
/// (`reference_lock_keys`: old key first). While held, a heartbeat task per
/// lock object renews it every `renew_interval`; [`Self::ensure_held`] runs
/// before each commit and checks every object, so a holder whose lock lapsed
/// (a long streaming encode, a coordination-bucket outage) stops before it
/// writes. On drop it stops the heartbeats and spawns a best-effort async
/// release in reverse order (Drop cannot be async); the lock's TTL backstops
/// a release that never completes. Inert (`hold: None`) single-instance.
///
/// Every engine write of reference.bin (bytes or metadata) goes through a
/// method on this guard, so the write cannot happen without the lock: the
/// backend write needs a [`RefWriteProof`], which only the guard's
/// `reference_writes` module makes.
pub(crate) struct ReferenceLockGuard {
    hold: Option<CrossNodeHold>,
}

struct CrossNodeHold {
    lock: Arc<crate::coordination::DynReferenceLock<'static>>,
    owner: String,
    /// The lock objects held, in acquisition order. Released in reverse.
    keys: Vec<HeldKey>,
    /// reference.bin as this hold saw it right after the acquire; every
    /// reference write of the hold is conditional on it (and moves it on).
    /// A writer whose lock lapsed while a peer wrote cannot overwrite the
    /// peer's baseline: its condition fails instead.
    fence: parking_lot::Mutex<crate::storage::RefFence>,
}

/// One held lock object and its heartbeat.
struct HeldKey {
    key: String,
    state: Arc<HoldState>,
    heartbeat: tokio::task::JoinHandle<()>,
}

/// Shared between the guard and its heartbeat task.
struct HoldState {
    lost: std::sync::atomic::AtomicBool,
    /// Monotonic instant of the last confirmed acquire/renew.
    confirmed_at: parking_lot::Mutex<std::time::Instant>,
    /// One renew at a time. The heartbeat and a commit's confirm renew the
    /// same lock as the same owner: both If-Match one etag, and the loser's
    /// 412 read as "lost" although the lock is still ours.
    renewing: tokio::sync::Mutex<()>,
    /// Stops the heartbeat between renews (never mid-renew), so the release
    /// on drop never races a renew in flight.
    stop: tokio::sync::Notify,
}

/// What a commit must do with a hold, given how old its last confirmation
/// is. Pure, so the timing rule is unit-tested without a clock.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum HoldCheck {
    /// Confirmed less than `renew_interval` (ttl / 4) ago: no peer can steal
    /// it yet, so the commit needs no extra round trip.
    Trust,
    /// Old confirmation: renew synchronously before the write.
    Confirm,
    /// The heartbeat saw the lock lost.
    Lost,
}

pub(crate) fn hold_check(lost: bool, age: Duration, renew_interval: Duration) -> HoldCheck {
    if lost {
        HoldCheck::Lost
    } else if age < renew_interval {
        HoldCheck::Trust
    } else {
        HoldCheck::Confirm
    }
}

impl ReferenceLockGuard {
    pub(crate) fn inert() -> Self {
        Self { hold: None }
    }

    /// A cross-instance hold with no lock object yet: [`Self::push_key`]
    /// adds each one when it is acquired, so a drop releases what is held.
    fn holding(lock: Arc<crate::coordination::DynReferenceLock<'static>>, owner: String) -> Self {
        Self {
            hold: Some(CrossNodeHold {
                lock,
                owner,
                keys: Vec::new(),
                fence: parking_lot::Mutex::new(crate::storage::RefFence::Unfenced),
            }),
        }
    }

    /// Record an acquired lock object and start its heartbeat.
    fn push_key(&mut self, key: String) {
        let Some(h) = &mut self.hold else {
            return;
        };
        let state = Arc::new(HoldState {
            lost: std::sync::atomic::AtomicBool::new(false),
            confirmed_at: parking_lot::Mutex::new(std::time::Instant::now()),
            renewing: tokio::sync::Mutex::new(()),
            stop: tokio::sync::Notify::new(),
        });
        let heartbeat = tokio::spawn(Self::heartbeat(
            h.lock.clone(),
            key.clone(),
            h.owner.clone(),
            state.clone(),
        ));
        h.keys.push(HeldKey {
            key,
            state,
            heartbeat,
        });
    }

    /// Whether reference.bin existed when the lock was taken: `Some` for a
    /// cross-instance hold (saves the caller its own HEAD), `None` when the
    /// caller must ask the backend.
    pub(crate) fn observed_reference(&self) -> Option<bool> {
        use crate::storage::RefFence;
        match &*self.hold.as_ref()?.fence.lock() {
            RefFence::Absent => Some(false),
            RefFence::ETag(_) => Some(true),
            RefFence::Unfenced => None,
        }
    }

    /// Renew every `renew_interval` until dropped. A renew that reports the
    /// lock lost, or errors for `ttl / 2` since the last confirmation, marks
    /// the hold lost; the next commit then refuses.
    async fn heartbeat(
        lock: Arc<crate::coordination::DynReferenceLock<'static>>,
        key: String,
        owner: String,
        state: Arc<HoldState>,
    ) {
        use std::sync::atomic::Ordering;
        let every = lock.renew_interval().max(Duration::from_millis(10));
        let give_up = Duration::from_secs((lock.ttl_secs().max(1) as u64).div_ceil(2));
        loop {
            tokio::select! {
                _ = tokio::time::sleep(every) => {}
                _ = state.stop.notified() => return,
            }
            let _one = state.renewing.lock().await;
            let started = std::time::Instant::now();
            match lock
                .renew(&key, &owner, crate::event_outbox::current_unix_seconds())
                .await
            {
                Ok(()) => *state.confirmed_at.lock() = started,
                Err(crate::coordination::LeaseError::Lost) => {
                    warn!("reference lock {key} lost while held; the write will be refused");
                    state.lost.store(true, Ordering::SeqCst);
                    return;
                }
                Err(e) => {
                    if state.confirmed_at.lock().elapsed() >= give_up {
                        warn!("reference lock {key}: renew failing for ttl/2 ({e}); hold lost");
                        state.lost.store(true, Ordering::SeqCst);
                        return;
                    }
                    debug!("reference lock {key}: renew failed, retrying: {e}");
                }
            }
        }
    }

    /// Refuse the caller's next write unless every lock object of the hold
    /// is still ours.
    pub(crate) async fn ensure_held(&self) -> Result<(), EngineError> {
        let Some(h) = &self.hold else {
            return Ok(());
        };
        for k in &h.keys {
            Self::ensure_key_held(h, k).await?;
        }
        Ok(())
    }

    async fn ensure_key_held(h: &CrossNodeHold, k: &HeldKey) -> Result<(), EngineError> {
        use std::sync::atomic::Ordering;
        let lost_err = || {
            EngineError::Storage(StorageError::Other(format!(
                "reference lock {} lapsed before the write; refusing to write reference.bin \
                 or its delta (another instance may own the deltaspace now)",
                k.key
            )))
        };
        let check = || {
            hold_check(
                k.state.lost.load(Ordering::SeqCst),
                k.state.confirmed_at.lock().elapsed(),
                h.lock.renew_interval(),
            )
        };
        if check() == HoldCheck::Trust {
            return Ok(());
        }
        // Wait out a heartbeat renew in flight, then decide again: it may
        // have just confirmed the lock, or seen it lost.
        let _one = k.state.renewing.lock().await;
        match check() {
            HoldCheck::Trust => Ok(()),
            HoldCheck::Lost => Err(lost_err()),
            HoldCheck::Confirm => {
                let started = std::time::Instant::now();
                match h
                    .lock
                    .renew(
                        &k.key,
                        &h.owner,
                        crate::event_outbox::current_unix_seconds(),
                    )
                    .await
                {
                    Ok(()) => {
                        *k.state.confirmed_at.lock() = started;
                        Ok(())
                    }
                    Err(crate::coordination::LeaseError::Lost) => {
                        k.state.lost.store(true, Ordering::SeqCst);
                        Err(lost_err())
                    }
                    Err(e) => Err(EngineError::Storage(StorageError::Other(format!(
                        "reference lock {} could not be confirmed before the write: {e}",
                        k.key
                    )))),
                }
            }
        }
    }

    /// True when a cross-instance lock is held (multi-instance only).
    pub(crate) fn is_cross_instance(&self) -> bool {
        self.hold.is_some()
    }
}

/// The only engine path to reference.bin and delta writes. A backend
/// write takes a [`RefWriteProof`], and only this module makes one, so
/// the compiler (not a source scan) keeps every reference and delta write
/// behind the [`ReferenceLockGuard`] and its lock check.
mod reference_writes {
    use super::*;

    /// Proof, for a backend reference or delta write, that the engine holds
    /// the deltaspace's reference lock (and ran its check). Made only here.
    #[derive(Debug)]
    pub struct RefWriteProof {
        _private: (),
    }

    const PROOF: &RefWriteProof = &RefWriteProof { _private: () };

    impl RefWriteProof {
        /// For tests that drive a backend directly.
        #[cfg(test)]
        pub(crate) fn for_tests() -> &'static RefWriteProof {
            PROOF
        }

        /// For an integration test that drives a backend's fenced writes
        /// itself (it cannot reach `for_tests`). Production code never
        /// calls it: source test `production_code_never_writes_unguarded`.
        #[doc(hidden)]
        pub fn unguarded() -> &'static RefWriteProof {
            PROOF
        }
    }

    impl ReferenceLockGuard {
        /// One reference write: fenced under a cross-instance hold, plain
        /// otherwise (single instance: the in-process lock is the whole story).
        async fn write_reference<B: StorageBackend + ?Sized>(
            &self,
            storage: &B,
            bucket: &str,
            deltaspace: &str,
            op: crate::storage::RefWrite<'_>,
        ) -> Result<(), EngineError> {
            use crate::storage::RefWrite;
            self.ensure_held().await?;
            if let Some(h) = &self.hold {
                let fence = h.fence.lock().clone();
                let next = storage
                    .write_reference_fenced(bucket, deltaspace, op, &fence, PROOF)
                    .await?;
                *h.fence.lock() = next;
                return Ok(());
            }
            match op {
                RefWrite::Put { data, metadata } => {
                    storage
                        .put_reference(bucket, deltaspace, data, metadata, PROOF)
                        .await?
                }
                RefWrite::PutFile { path, metadata } => {
                    storage
                        .put_reference_from_file(bucket, deltaspace, path, metadata, PROOF)
                        .await?
                }
                RefWrite::Metadata { metadata } => {
                    storage
                        .put_reference_metadata(bucket, deltaspace, metadata, PROOF)
                        .await?
                }
                RefWrite::Delete => storage.delete_reference(bucket, deltaspace, PROOF).await?,
            }
            Ok(())
        }

        // ── Reference writes: the only engine path to them ──

        pub(crate) async fn put_reference<B: StorageBackend + ?Sized>(
            &self,
            storage: &B,
            bucket: &str,
            deltaspace: &str,
            data: &[u8],
            meta: &FileMetadata,
        ) -> Result<(), EngineError> {
            let op = crate::storage::RefWrite::Put {
                data,
                metadata: meta,
            };
            self.write_reference(storage, bucket, deltaspace, op).await
        }

        pub(crate) async fn put_reference_from_file<B: StorageBackend + ?Sized>(
            &self,
            storage: &B,
            bucket: &str,
            deltaspace: &str,
            path: &std::path::Path,
            meta: &FileMetadata,
        ) -> Result<(), EngineError> {
            let op = crate::storage::RefWrite::PutFile {
                path,
                metadata: meta,
            };
            self.write_reference(storage, bucket, deltaspace, op).await
        }

        pub(crate) async fn put_reference_metadata<B: StorageBackend + ?Sized>(
            &self,
            storage: &B,
            bucket: &str,
            deltaspace: &str,
            meta: &FileMetadata,
        ) -> Result<(), EngineError> {
            let op = crate::storage::RefWrite::Metadata { metadata: meta };
            self.write_reference(storage, bucket, deltaspace, op).await
        }

        pub(crate) async fn delete_reference<B: StorageBackend + ?Sized>(
            &self,
            storage: &B,
            bucket: &str,
            deltaspace: &str,
        ) -> Result<(), EngineError> {
            let op = crate::storage::RefWrite::Delete;
            self.write_reference(storage, bucket, deltaspace, op).await
        }

        /// A delta is valid only against the baseline it was encoded from. The
        /// delta write cannot be conditional on reference.bin (another object),
        /// so under a cross-instance hold re-read the reference fence AFTER the
        /// write: a peer that replaced the baseline meanwhile makes our delta
        /// undecodable, so remove it and fail retryably instead of a 200.
        pub(crate) async fn put_delta<B: StorageBackend + ?Sized>(
            &self,
            storage: &B,
            bucket: &str,
            deltaspace: &str,
            filename: &str,
            data: &[u8],
            meta: &FileMetadata,
        ) -> Result<(), EngineError> {
            self.ensure_held().await?;
            storage
                .put_delta(bucket, deltaspace, filename, data, meta, PROOF)
                .await?;
            let Some(h) = &self.hold else {
                return Ok(());
            };
            let expected = h.fence.lock().clone();
            if expected == crate::storage::RefFence::Unfenced {
                return Ok(());
            }
            let verdict = match storage.reference_fence(bucket, deltaspace).await {
                Ok(now) if now == expected => return Ok(()),
                Ok(_) => crate::storage::reference_fence_lost(bucket, deltaspace),
                // Baseline unknown: the delta may be undecodable, so the same
                // answer as a lost fence (the client retries under a fresh lock).
                Err(e) => e,
            };
            if let Err(e) = storage.delete_delta(bucket, deltaspace, filename).await {
                tracing::warn!(
                    "delta {bucket}/{deltaspace}/{filename} is not fenced to its baseline; \
                     its removal failed: {e}"
                );
            }
            Err(verdict.into())
        }
    }
}
pub use reference_writes::RefWriteProof;

impl Drop for ReferenceLockGuard {
    fn drop(&mut self) {
        if let Some(h) = self.hold.take() {
            if h.keys.is_empty() {
                return;
            }
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                // Stop the heartbeats and let a renew in flight finish BEFORE
                // the release: a renew PUT that lands between the release's
                // read and its If-Match DELETE made the DELETE fail, and the
                // lock blocked the deltaspace for a whole TTL.
                for k in &h.keys {
                    k.state.stop.notify_one();
                }
                let (lock, owner, keys) = (h.lock, h.owner, h.keys);
                handle.spawn(async move {
                    // Reverse of the acquire order: the old key goes last.
                    for k in keys.into_iter().rev() {
                        let _ = k.heartbeat.await;
                        if let Err(e) = lock.release(&k.key, &owner).await {
                            tracing::warn!("reference lock release failed for {}: {e}", k.key);
                        }
                    }
                });
            } else {
                // No runtime available (dropped during shutdown) → rely on the TTL.
                for k in &h.keys {
                    k.heartbeat.abort();
                }
            }
        }
    }
}

pub(super) type PrefixLocks = DashMap<String, Arc<tokio::sync::Mutex<()>>>;

/// THE process-wide prefix-lock map, shared by every engine (like the spool).
/// Per-engine maps let a config reload's new engine write a deltaspace while
/// the old engine's in-flight PUT still held its own lock for it.
pub(super) fn shared_prefix_locks() -> Arc<PrefixLocks> {
    static SHARED: std::sync::OnceLock<Arc<PrefixLocks>> = std::sync::OnceLock::new();
    SHARED.get_or_init(|| Arc::new(DashMap::new())).clone()
}

tokio::task_local! {
    /// The cross-instance lock `with_dest_prefix_lock` holds while its closure
    /// runs, so the raw writers it calls can check it before they write.
    static HELD_REFERENCE_LOCK: Arc<ReferenceLockGuard>;
}

/// How a [`DeltaGliderEngine::with_dest_prefix_lock`] result reports that
/// the cross-instance reference lock could not be taken (the closure did
/// not run).
pub trait ReferenceLockFailure {
    fn reference_lock_failed(err: EngineError) -> Self;
}

/// Best-effort callers (no result to carry): log and skip.
impl ReferenceLockFailure for () {
    fn reference_lock_failed(err: EngineError) -> Self {
        warn!("deltaspace work skipped: {err}");
    }
}

impl<T, E: From<EngineError>> ReferenceLockFailure for Result<T, E> {
    fn reference_lock_failed(err: EngineError) -> Self {
        Err(E::from(err))
    }
}

/// The raw accessors speak `StorageError`; keep a storage error as-is.
pub(super) fn engine_to_storage(e: EngineError) -> StorageError {
    match e {
        EngineError::Storage(s) => s,
        other => StorageError::Other(other.to_string()),
    }
}

impl<S: StorageBackend> DeltaGliderEngine<S> {
    /// Acquire a per-deltaspace async lock. Different prefixes do not contend.
    pub(super) async fn acquire_prefix_lock(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> tokio::sync::OwnedMutexGuard<()> {
        // Periodic cleanup on every lock acquisition (cheap — just checks len())
        self.cleanup_prefix_locks();
        // A deltaspace is (storage, prefix): keyed by the prefix alone, the
        // same prefix in two buckets shared one mutex (see `cache_key`).
        let mutex = self
            .prefix_locks
            .entry(self.cache_key(bucket, prefix))
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        mutex.lock_owned().await
    }

    /// Acquire the CROSS-INSTANCE reference lock for a deltaspace, INSIDE the
    /// in-process `prefix_locks` mutex (which the caller must already hold), so
    /// two nodes cannot both create/overwrite a deltaspace's `reference.bin`.
    ///
    /// Single-instance (`reference_lock == None`) → an inert guard, zero S3
    /// round-trips. Multi-instance → block up to the lock's acquire timeout; if a
    /// peer holds it that long (or the coordination bucket errors), FAIL the
    /// write closed (`EngineError`) rather than risk a second baseline. The
    /// returned guard releases the lock (best-effort) on drop; the lock's TTL
    /// backstops a release that never runs (crash / shutdown).
    pub(super) async fn acquire_reference_lock(
        &self,
        bucket: &str,
        deltaspace: &str,
    ) -> Result<ReferenceLockGuard, EngineError> {
        let Some(lock) = self.reference_lock.clone() else {
            return Ok(ReferenceLockGuard::inert());
        };
        // Old key first while it differs (see `reference_lock_keys`).
        let keys = crate::coordination::reference_lock::reference_lock_keys(
            &self.storage.storage_identity(bucket),
            self.storage.previous_storage_identity(bucket).as_deref(),
            deltaspace,
        );
        let owner = format!("ref-{}", uuid::Uuid::new_v4());
        let deadline = std::time::Instant::now() + lock.acquire_timeout();
        let now_fn = || crate::event_outbox::current_unix_seconds();
        // Built before the first acquire: on any error below, its drop
        // releases the lock objects that it holds so far.
        let mut guard = ReferenceLockGuard::holding(lock.clone(), owner.clone());
        for key in keys {
            match crate::coordination::reference_lock::acquire_blocking(
                lock.as_ref(),
                &key,
                &owner,
                deadline,
                &now_fn,
            )
            .await
            {
                Ok(true) => guard.push_key(key),
                Ok(false) => {
                    return Err(EngineError::Storage(StorageError::Contended(format!(
                        "reference lock for deltaspace '{bucket}/{deltaspace}' held by another \
                         instance; write timed out to avoid corrupting reference.bin"
                    ))))
                }
                Err(e) => {
                    return Err(EngineError::Storage(StorageError::Other(format!(
                        "reference lock acquire failed for deltaspace '{bucket}/{deltaspace}': {e}"
                    ))))
                }
            }
        }
        // Observe reference.bin under the lock: the fence of every reference
        // write this hold makes.
        let fence = self.storage.reference_fence(bucket, deltaspace).await?;
        if let Some(h) = &guard.hold {
            *h.fence.lock() = fence;
        }
        Ok(guard)
    }

    /// Prune prefix lock entries that are no longer actively held.
    /// An entry with `Arc::strong_count() == 1` means only the map references it
    /// (no outstanding `OwnedMutexGuard`), so it can be safely removed.
    /// Only runs when the map exceeds a size threshold to avoid overhead.
    pub(super) fn cleanup_prefix_locks(&self) {
        const CLEANUP_THRESHOLD: usize = 1024;
        if self.prefix_locks.len() <= CLEANUP_THRESHOLD {
            return;
        }
        let before = self.prefix_locks.len();
        self.prefix_locks
            .retain(|_, arc| Arc::strong_count(arc) > 1);
        let removed = before - self.prefix_locks.len();
        if removed > 0 {
            debug!(
                "Pruned {} idle prefix locks ({} remaining)",
                removed,
                self.prefix_locks.len()
            );
        }
    }

    /// The cross-instance hold of the enclosing [`Self::with_dest_prefix_lock`]
    /// for a raw `what` write. Outside it: an inert guard single-instance
    /// (the in-process lock is the whole story), a refusal multi-instance
    /// (the write would be unfenced against a reference nobody holds).
    pub(super) fn held_reference_lock(
        &self,
        bucket: &str,
        prefix: &str,
        what: &str,
    ) -> Result<Arc<ReferenceLockGuard>, StorageError> {
        match HELD_REFERENCE_LOCK.try_with(Arc::clone) {
            Ok(held) => Ok(held),
            Err(_) if self.reference_lock.is_none() => Ok(Arc::new(ReferenceLockGuard::inert())),
            Err(_) => Err(StorageError::Other(format!(
                "{what} write to {bucket}/{prefix} outside the deltaspace lock"
            ))),
        }
    }

    /// Run `f` while holding the per-deltaspace prefix lock AND the
    /// cross-instance reference lock, serialising the fast-path reference
    /// seed against concurrent PUTs to that deltaspace on every node. When
    /// the cross-instance lock cannot be taken, `f` does not run and `R`
    /// reports the failure (`ReferenceLockFailure`).
    pub async fn with_dest_prefix_lock<F, Fut, R>(&self, bucket: &str, prefix: &str, f: F) -> R
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = R>,
        R: ReferenceLockFailure,
    {
        let _guard = self.acquire_prefix_lock(bucket, prefix).await;
        let xnode = match self.acquire_reference_lock(bucket, prefix).await {
            Ok(g) => Arc::new(g),
            Err(e) => return R::reference_lock_failed(e),
        };
        HELD_REFERENCE_LOCK.scope(xnode, f()).await
    }
}
