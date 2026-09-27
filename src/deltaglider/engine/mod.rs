// SPDX-License-Identifier: BUSL-1.1

//! DeltaGlider engine - main orchestrator for delta-based storage

use arc_swap::ArcSwap;

use super::cache::ReferenceCache;
use super::codec::{CodecError, DeltaCodec};
use super::file_router::FileRouter;
use crate::config::{BackendConfig, Config};
use crate::metadata_cache::MetadataCache;
use crate::metrics::Metrics;
use crate::storage::{FilesystemBackend, S3Backend, StorageBackend, StorageError};
use crate::types::{FileMetadata, ObjectKey, StorageInfo, StoreResult};
use bytes::Bytes;
use dashmap::DashMap;
use futures::stream::BoxStream;
use md5::Digest;
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio::sync::Semaphore;
use tracing::{debug, info, instrument, warn};

pub(crate) mod conditional;
mod retrieve;
pub(crate) mod store;

/// Common fields passed through the store pipeline (store → encode_and_store / store_passthrough).
/// Eliminates the 8-parameter signatures that triggered `clippy::too_many_arguments`.
struct StoreContext<'a> {
    bucket: &'a str,
    obj_key: &'a ObjectKey,
    deltaspace_id: &'a str,
    data: &'a [u8],
    sha256: String,
    md5: String,
    content_type: Option<String>,
    user_metadata: HashMap<String, String>,
    /// When `Some`, the persisted `FileMetadata.multipart_etag` is
    /// stamped with this value so subsequent HEAD/GET/LIST return the
    /// same ETag the CompleteMultipartUpload response advertised
    /// (H1 correctness fix). Normal single-PUT writes pass `None` and
    /// get the standard full-body-MD5 ETag.
    multipart_etag: Option<String>,
}

/// Apply continuation-token filtering and max-keys truncation to a sorted list.
/// Returns `(is_truncated, next_continuation_token)`.
fn paginate_sorted<T>(
    items: &mut Vec<T>,
    max_keys: u32,
    continuation_token: Option<&str>,
    sort_key: impl Fn(&T) -> &String,
) -> (bool, Option<String>) {
    if let Some(token) = continuation_token {
        items.retain(|item| sort_key(item).as_str() > token);
    }
    let max = max_keys as usize;
    let is_truncated = items.len() > max;
    if is_truncated {
        items.truncate(max);
    }
    let next_token = if is_truncated {
        items.last().map(|item| sort_key(item).clone())
    } else {
        None
    };
    (is_truncated, next_token)
}

/// Result of interleaving objects and common prefixes with pagination.
pub(crate) struct InterleavedPage<O> {
    pub objects: Vec<(String, O)>,
    pub common_prefixes: Vec<String>,
    pub is_truncated: bool,
    pub next_continuation_token: Option<String>,
}

/// Interleave objects and common prefixes into a single sorted list, apply
/// continuation-token filtering and max-keys pagination, then split back.
///
/// S3 ListObjectsV2 counts both objects and common prefixes toward max-keys
/// and requires lexicographic ordering across both sets. This function is the
/// single source of truth for that logic (used by engine, S3 backend, and
/// filesystem backend).
pub(crate) fn interleave_and_paginate<O>(
    objects: Vec<(String, O)>,
    common_prefixes: Vec<String>,
    max_keys: u32,
    continuation_token: Option<&str>,
) -> InterleavedPage<O> {
    enum Entry<T> {
        Obj(String, T),
        Prefix(String),
    }

    let mut entries: Vec<(String, Entry<O>)> =
        Vec::with_capacity(objects.len() + common_prefixes.len());
    for (key, obj) in objects {
        entries.push((key.clone(), Entry::Obj(key, obj)));
    }
    for cp in common_prefixes {
        entries.push((cp.clone(), Entry::Prefix(cp)));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    // Apply continuation_token: skip entries <= token.
    if let Some(token) = continuation_token {
        entries.retain(|e| e.0.as_str() > token);
    }

    let max = max_keys as usize;
    let is_truncated = entries.len() > max;
    if entries.len() > max {
        entries.truncate(max);
    }
    let next_token = if is_truncated {
        entries.last().map(|(key, _)| key.clone())
    } else {
        None
    };

    let mut final_objects = Vec::new();
    let mut final_prefixes = Vec::new();
    for (_, entry) in entries {
        match entry {
            Entry::Obj(key, obj) => final_objects.push((key, obj)),
            Entry::Prefix(p) => final_prefixes.push(p),
        }
    }

    InterleavedPage {
        objects: final_objects,
        common_prefixes: final_prefixes,
        is_truncated,
        next_continuation_token: next_token,
    }
}

/// Errors from the DeltaGlider engine
#[derive(Debug, Error)]
pub enum EngineError {
    #[error("Storage error: {0}")]
    Storage(#[from] StorageError),

    #[error("Codec error: {0}")]
    Codec(#[from] CodecError),

    #[error("Object not found: {0}")]
    NotFound(String),

    #[error("Checksum mismatch for {key}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        key: String,
        expected: String,
        actual: String,
    },

    #[error("Missing reference for deltaspace: {0}")]
    MissingReference(String),

    #[error("Object too large: {size} bytes (max: {max} bytes)")]
    TooLarge { size: u64, max: u64 },

    #[error("InvalidArgument: {0}")]
    InvalidArgument(String),

    #[error("Service overloaded: {0}")]
    Overloaded(String),
}

impl EngineError {
    /// The object does not exist. The engine reports a missing object as
    /// EITHER a top-level `NotFound` OR a storage-level `NotFound` (a stale
    /// cached storage type reads the wrong path), so callers must accept both.
    /// Decide on the variant, never on the Display text.
    pub fn is_not_found(&self) -> bool {
        matches!(
            self,
            EngineError::NotFound(_) | EngineError::Storage(StorageError::NotFound(_))
        )
    }
}

#[derive(Debug, Clone)]
pub struct ListObjectsPage {
    /// Direct objects at this level (after delimiter collapsing, if delimiter was provided)
    pub objects: Vec<(String, FileMetadata)>,
    /// CommonPrefixes produced by delimiter collapsing (empty if no delimiter)
    pub common_prefixes: Vec<String>,
    pub is_truncated: bool,
    pub next_continuation_token: Option<String>,
}

/// Result of [`DeltaGliderEngine::list_deltaspace_references`] — the
/// reference baselines found within a scope, plus a flag telling the
/// caller whether they're seeing the full set or a bounded prefix.
///
/// `truncated == true` means the helper hit [`REFERENCE_SCAN_LIMIT`]
/// before exhausting the scope. Callers folding these into a savings
/// total MUST propagate `truncated` to the wire so the UI can render
/// "scope truncated" rather than implying the number is exact.
#[derive(Debug, Clone, Default)]
pub struct ReferenceScan {
    /// `(deltaspace prefix, reference metadata)`. The prefix has no trailing
    /// slash (`""` is the bucket root), so callers can attribute each baseline
    /// to the folder that holds it.
    pub references: Vec<(String, FileMetadata)>,
    pub truncated: bool,
}

/// Default maximum number of deltaspaces whose `reference.bin`
/// metadata we will fold into a single lightweight savings scan.
///
/// Rationale: each match performs one `get_reference_metadata` —
/// for the S3 backend that's a HEAD; without a cap a chip refresh
/// on a 50k-deltaspace bucket fires 50k HEADs every cache miss.
/// Lightweight callers (the SPA chip's per-prefix endpoint) pass
/// `Some(REFERENCE_SCAN_LIMIT)`; the operator-triggered admin
/// dashboard scan + the CLI `stats` command pass `None` to get
/// the exhaustive answer regardless of cost.
///
/// Module-level (rather than associated const on `DeltaGliderEngine`)
/// so callers can refer to it without specifying the storage backend
/// type parameter `<S>`.
pub const REFERENCE_SCAN_LIMIT: usize = 1000;

/// What [`DeltaGliderEngine::delete_if`] did.
#[derive(Debug)]
pub enum ConditionalDelete {
    /// The object passed the check and is deleted.
    Deleted(Box<FileMetadata>),
    /// The object failed the check (e.g. overwritten): nothing deleted.
    Changed,
    /// No such object: nothing deleted.
    Gone,
}

/// Response from `retrieve_stream()` — either a streaming or buffered response.
pub enum RetrieveResponse {
    /// Passthrough file streamed from backend (zero-copy, constant memory).
    Streamed {
        stream: BoxStream<'static, Result<Bytes, StorageError>>,
        metadata: FileMetadata,
        /// Not applicable for streamed responses (no cache involved).
        cache_hit: Option<bool>,
    },
    /// Delta-reconstructed file buffered in memory.
    Buffered {
        data: Vec<u8>,
        metadata: FileMetadata,
        /// Whether the reference was served from cache (true) or loaded from storage (false).
        cache_hit: Option<bool>,
    },
}

impl From<EngineError> for crate::api::S3Error {
    fn from(err: EngineError) -> Self {
        match err {
            EngineError::NotFound(key) => crate::api::S3Error::NoSuchKey(key),
            EngineError::TooLarge { size, max } => {
                crate::api::S3Error::EntityTooLarge { size, max }
            }
            EngineError::InvalidArgument(msg) => crate::api::S3Error::InvalidArgument(msg),
            EngineError::Overloaded(msg) => crate::api::S3Error::SlowDown(msg),
            EngineError::Storage(e) => e.into(),
            // E4: route opaque engine errors (ChecksumMismatch, codec
            // failures, etc.) through the sanitiser so computed/expected
            // hashes and xdelta3 stderr don't escape to the client.
            other => {
                crate::api::S3Error::InternalError(crate::api::errors::sanitise_for_client(&other))
            }
        }
    }
}

/// Main DeltaGlider engine - generic over storage backend
pub struct DeltaGliderEngine<S: StorageBackend> {
    storage: Arc<S>,
    codec: Arc<DeltaCodec>,
    file_router: FileRouter,
    /// Per engine on purpose (unlike `prefix_locks`): every hit is checked
    /// against the stored reference sha, so an overlap with the old engine
    /// cannot serve stale bytes, and a rebuild may re-route a bucket to
    /// another backend, whose references a shared cache would mask.
    cache: ReferenceCache,
    max_object_size: u64,
    /// Streaming-passthrough size ceiling (Phase B). Separate from
    /// `max_object_size` because multipart copies are O(part_size) memory.
    max_passthrough_object_size: u64,
    /// Limits concurrent xdelta3 subprocesses (configurable via `codec_concurrency`).
    codec_semaphore: Arc<Semaphore>,
    /// Per-deltaspace locks preventing concurrent reference overwrites.
    /// Uses DashMap for lock-free shard-level lookups (different prefixes never contend).
    /// Process-wide ([`shared_prefix_locks`]): a rebuilt engine and the one
    /// it replaces must exclude each other while the old one drains.
    prefix_locks: Arc<PrefixLocks>,
    /// Optional CROSS-INSTANCE per-deltaspace lock (multi-instance only; `None`
    /// single-instance → zero S3 round-trips). Held INSIDE `prefix_locks` around
    /// the reference read-modify-write so two nodes cannot both create a
    /// `reference.bin` baseline for the same deltaspace and corrupt it.
    reference_lock: Option<Arc<dyn crate::coordination::ReferenceLock>>,
    /// Optional Prometheus metrics (None in tests).
    metrics: Option<Arc<Metrics>>,
    /// In-memory cache for object metadata (eliminates HEAD requests).
    metadata_cache: MetadataCache,
    /// Per-bucket compression policy overrides.
    bucket_policies: crate::bucket_policy::BucketPolicyRegistry,
    /// Per-instance running usage counter (None in tests / when unavailable).
    /// Updated best-effort after each successful store/delete.
    bucket_usage: Option<Arc<crate::bucket_usage::BucketUsage>>,
    /// Quota'd temp space for streaming delta reconstruction (Phase 3). Large
    /// delta GETs decode to a spool file here, then stream the file to the
    /// client — bounded memory regardless of object size.
    spool: Arc<crate::deltaglider::spool::SpoolDir>,
    /// Verified reconstructions of large delta objects, kept briefly for
    /// more range reads of the same object (storage-11).
    range_spools: Arc<crate::deltaglider::range_spool::RangeSpoolCache>,
}

/// RAII guard for the optional cross-instance reference lock. Held for the
/// duration of a reference read-modify-write, inside the in-process prefix
/// mutex. While held, a heartbeat task renews the lock every
/// `renew_interval`; [`Self::ensure_held`] runs before each commit so a holder
/// whose lock lapsed (a long streaming encode, a coordination-bucket outage)
/// stops before it writes. On drop it stops the heartbeat and spawns a
/// best-effort async release (Drop cannot be async); the lock's TTL backstops
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
    lock: Arc<dyn crate::coordination::ReferenceLock>,
    key: String,
    owner: String,
    state: Arc<HoldState>,
    heartbeat: tokio::task::JoinHandle<()>,
    /// reference.bin as this hold saw it right after the acquire; every
    /// reference write of the hold is conditional on it (and moves it on).
    /// A writer whose lock lapsed while a peer wrote cannot overwrite the
    /// peer's baseline: its condition fails instead.
    fence: parking_lot::Mutex<crate::storage::RefFence>,
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

    fn held(
        lock: Arc<dyn crate::coordination::ReferenceLock>,
        key: String,
        owner: String,
        fence: crate::storage::RefFence,
    ) -> Self {
        let state = Arc::new(HoldState {
            lost: std::sync::atomic::AtomicBool::new(false),
            confirmed_at: parking_lot::Mutex::new(std::time::Instant::now()),
            renewing: tokio::sync::Mutex::new(()),
            stop: tokio::sync::Notify::new(),
        });
        let heartbeat = tokio::spawn(Self::heartbeat(
            lock.clone(),
            key.clone(),
            owner.clone(),
            state.clone(),
        ));
        Self {
            hold: Some(CrossNodeHold {
                lock,
                key,
                owner,
                state,
                heartbeat,
                fence: parking_lot::Mutex::new(fence),
            }),
        }
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
        lock: Arc<dyn crate::coordination::ReferenceLock>,
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

    /// Refuse the caller's next write unless the lock is still ours.
    pub(crate) async fn ensure_held(&self) -> Result<(), EngineError> {
        use std::sync::atomic::Ordering;
        let Some(h) = &self.hold else {
            return Ok(());
        };
        let lost_err = || {
            EngineError::Storage(StorageError::Other(format!(
                "reference lock {} lapsed before the write; refusing to write reference.bin \
                 or its delta (another instance may own the deltaspace now)",
                h.key
            )))
        };
        let check = || {
            hold_check(
                h.state.lost.load(Ordering::SeqCst),
                h.state.confirmed_at.lock().elapsed(),
                h.lock.renew_interval(),
            )
        };
        if check() == HoldCheck::Trust {
            return Ok(());
        }
        // Wait out a heartbeat renew in flight, then decide again: it may
        // have just confirmed the lock, or seen it lost.
        let _one = h.state.renewing.lock().await;
        match check() {
            HoldCheck::Trust => Ok(()),
            HoldCheck::Lost => Err(lost_err()),
            HoldCheck::Confirm => {
                let started = std::time::Instant::now();
                match h
                    .lock
                    .renew(
                        &h.key,
                        &h.owner,
                        crate::event_outbox::current_unix_seconds(),
                    )
                    .await
                {
                    Ok(()) => {
                        *h.state.confirmed_at.lock() = started;
                        Ok(())
                    }
                    Err(crate::coordination::LeaseError::Lost) => {
                        h.state.lost.store(true, Ordering::SeqCst);
                        Err(lost_err())
                    }
                    Err(e) => Err(EngineError::Storage(StorageError::Other(format!(
                        "reference lock {} could not be confirmed before the write: {e}",
                        h.key
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
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                // Stop the heartbeat and let a renew in flight finish BEFORE
                // the release: a renew PUT that lands between the release's
                // read and its If-Match DELETE made the DELETE fail, and the
                // lock blocked the deltaspace for a whole TTL.
                h.state.stop.notify_one();
                let (lock, key, owner, heartbeat) = (h.lock, h.key, h.owner, h.heartbeat);
                handle.spawn(async move {
                    let _ = heartbeat.await;
                    if let Err(e) = lock.release(&key, &owner).await {
                        tracing::warn!("reference lock release failed for {key}: {e}");
                    }
                });
            } else {
                // No runtime available (dropped during shutdown) → rely on the TTL.
                h.heartbeat.abort();
            }
        }
    }
}

type PrefixLocks = DashMap<String, Arc<tokio::sync::Mutex<()>>>;

/// THE process-wide prefix-lock map, shared by every engine (like the spool).
/// Per-engine maps let a config reload's new engine write a deltaspace while
/// the old engine's in-flight PUT still held its own lock for it.
fn shared_prefix_locks() -> Arc<PrefixLocks> {
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
fn engine_to_storage(e: EngineError) -> StorageError {
    match e {
        EngineError::Storage(s) => s,
        other => StorageError::Other(other.to_string()),
    }
}

/// Type alias for engine with dynamic backend dispatch
pub type DynEngine = DeltaGliderEngine<Box<dyn StorageBackend>>;

impl DynEngine {
    /// Create a new engine with the appropriate backend based on configuration.
    /// Pass `metrics` to enable Prometheus instrumentation (None disables it).
    ///
    /// When `config.backends` is non-empty, constructs a `RoutingBackend` that
    /// routes calls to the correct underlying backend per bucket. Otherwise,
    /// uses the legacy single-backend path from `config.backend`.
    pub async fn new(config: &Config, metrics: Option<Arc<Metrics>>) -> Result<Self, StorageError> {
        // Per-backend encryption wrapping.
        //
        // Every backend ends up wrapped by `EncryptingBackend`, whether
        // or not it has a key configured. The wrapper's read path checks
        // the `dg-encrypted` metadata marker + sniffs for the DGE1 magic
        // on "not-encrypted" responses, so even a mode:none backend gets
        // the xattr-strip defense (if the xattr is lost during a
        // backup/restore round-trip, the wrapper refuses to serve
        // DGE1-prefixed ciphertext as plaintext).
        //
        // Two orthogonal encryption layers live here:
        //   - Proxy-side AES-256-GCM via `EncryptingBackend` when the
        //     mode is Aes256GcmProxy. The wrapper encrypts bytes before
        //     they reach `S3Backend::put_object`.
        //   - S3-native SSE (SseKms / SseS3) when mode is one of
        //     those. The proxy passes `NativeEncryptionConfig` into
        //     `S3Backend::new`, which adds `x-amz-server-side-encryption`
        //     headers to every PutObject; AWS encrypts on write and
        //     decrypts transparently on read for callers with KMS perms.
        //
        // The two layers are mutually exclusive on a given backend: you
        // get ONE of {proxy AES-GCM, SSE-KMS, SSE-S3, none}. The
        // encryption config enum enforces this by construction.
        let storage: Box<dyn StorageBackend> = if config.backends.is_empty() {
            // Singleton backend path. Synthetic name "default" matches
            // what `apply_backend_encryption_env` uses for this entry.
            let raw =
                build_raw_backend("default", &config.backend, &config.backend_encryption).await?;
            wrap_backend_with_encryption(
                "default",
                raw,
                &config.backend_encryption,
                &mut KeyIdCollisionCheck::new(),
            )?
        } else {
            // Multi-backend routing. Each named entry is constructed
            // raw (with native-SSE config already baked in), wrapped
            // with its own proxy-AES config if any, then handed to
            // the router.
            let mut backends = std::collections::HashMap::new();
            let mut kid_collisions = KeyIdCollisionCheck::new();
            for named in &config.backends {
                let raw = build_raw_backend(&named.name, &named.backend, &named.encryption).await?;
                let wrapped = wrap_backend_with_encryption(
                    &named.name,
                    raw,
                    &named.encryption,
                    &mut kid_collisions,
                )?;
                backends.insert(named.name.clone(), Arc::new(wrapped));
            }
            let default_name = config.default_backend_name();

            let registry = crate::bucket_policy::BucketPolicyRegistry::new(
                config.buckets.clone(),
                config.max_delta_ratio,
            );
            let routes = registry.routing_table();

            Box::new(crate::storage::RoutingBackend::new(
                backends,
                routes,
                default_name,
            )?)
        };

        Ok(Self::new_with_backend(Arc::new(storage), config, metrics))
    }
}

/// Translate the on-wire `BackendEncryptionConfig` into the
/// S3-specific `NativeEncryptionConfig` for the raw backend
/// constructor. Returns `None` variant for every non-native mode
/// (proxy-AES or mode:none): those are handled by the
/// `EncryptingBackend` wrapper layer above.
fn native_encryption_for(
    enc: &crate::config::BackendEncryptionConfig,
) -> crate::storage::NativeEncryptionConfig {
    use crate::config::BackendEncryptionConfig as E;
    use crate::storage::NativeEncryptionConfig as N;
    match enc {
        E::None { .. } | E::Aes256GcmProxy { .. } => N::None,
        E::SseS3 { .. } => N::SseS3,
        E::SseKms {
            kms_key_id,
            bucket_key_enabled,
            ..
        } => N::SseKms {
            kms_key_id: kms_key_id.clone(),
            bucket_key_enabled: *bucket_key_enabled,
        },
    }
}

/// Build ONE storage backend from a `BackendConfig` variant + its
/// encryption config. Native SSE modes are baked into the S3 client
/// here; proxy-AES encryption is layered on top by
/// `wrap_backend_with_encryption`. Filesystem backends ignore native
/// modes (rejected at `Config::check` time).
async fn build_raw_backend(
    name: &str,
    cfg: &BackendConfig,
    enc: &crate::config::BackendEncryptionConfig,
) -> Result<Box<dyn StorageBackend>, StorageError> {
    match cfg {
        BackendConfig::Filesystem { path } => {
            Ok(Box::new(FilesystemBackend::new(path.clone()).await?))
        }
        BackendConfig::S3 { .. } => {
            let native = native_encryption_for(enc);
            Ok(Box::new(
                S3Backend::new(cfg, native)
                    .await?
                    .with_health_name(name, cfg),
            ))
        }
    }
}

/// Tracks explicit `key_id` → `key` pairs seen during construction so
/// we can fail-fast on "two backends claim the same key_id but carry
/// different key material" — the same invariant `Config::check`
/// warns about, re-enforced at engine-construction time (the warnings
/// path is advisory; this is load-bearing for the read-side key_id
/// mismatch check in [`crate::storage::encrypting`]).
struct KeyIdCollisionCheck {
    seen: std::collections::BTreeMap<String, Vec<u8>>,
}

impl KeyIdCollisionCheck {
    fn new() -> Self {
        Self {
            seen: std::collections::BTreeMap::new(),
        }
    }
    fn record(
        &mut self,
        backend_name: &str,
        key_id: &str,
        key_bytes: &[u8],
    ) -> Result<(), StorageError> {
        if let Some(prev) = self.seen.get(key_id) {
            if prev != key_bytes {
                return Err(StorageError::Encryption(format!(
                    "backend '{}' declares key_id='{}' but a prior backend uses the SAME \
                     key_id with DIFFERENT key bytes — the read-side key_id mismatch check \
                     would then fire on every cross-backend read. Give each backend a \
                     distinct key_id, or set both to the same key (documented portability \
                     escape hatch).",
                    backend_name, key_id
                )));
            }
        } else {
            self.seen.insert(key_id.to_string(), key_bytes.to_vec());
        }
        Ok(())
    }
}

/// Wrap one raw backend with its encryption config. Always wraps
/// (even for mode:none, which produces a no-op wrapper that still
/// fires the xattr-strip sniffer on reads — see B9 from the earlier
/// audit).
///
/// Resolves:
///   * `Aes256GcmProxy` → proxy key + key_id, write_mode Encrypt.
///   * `SseKms` / `SseS3` → primary key None, write_mode PassThrough.
///     Inner S3Backend does the encryption (Step 4); wrapper stays
///     in the stack for read-side sniffer defense + legacy shim.
///   * `None` → no key, write_mode Encrypt (vacuous; encrypt_if_enabled
///     short-circuits when key is None).
///   * `legacy_key` / `legacy_key_id` (Step 5) → populated on the
///     wrapper config when the YAML carries them. Used by the
///     shim-aware read path to decrypt proxy-AES objects while the
///     backend is running in native or no-key mode.
fn wrap_backend_with_encryption(
    backend_name: &str,
    inner: Box<dyn StorageBackend>,
    enc: &crate::config::BackendEncryptionConfig,
    collisions: &mut KeyIdCollisionCheck,
) -> Result<Box<dyn StorageBackend>, StorageError> {
    use crate::config::BackendEncryptionConfig as E;
    // Resolve primary (key, key_id) + pick the write_mode.
    let (primary_key, primary_kid, write_mode): (
        Option<crate::storage::EncryptionKey>,
        Option<String>,
        crate::storage::WriteMode,
    ) = match enc {
        E::Aes256GcmProxy {
            key: Some(hex),
            key_id,
            ..
        } => {
            let parsed =
                crate::storage::EncryptionKey::from_hex(hex).map_err(StorageError::Encryption)?;
            // Resolve the id: explicit wins over derived. Derivation
            // mixes the backend name in so same-key/different-name
            // backends get distinct ids (see derive_key_id comment).
            let kid = match key_id {
                Some(explicit) => explicit.clone(),
                None => derive_key_id(backend_name, &parsed.0),
            };
            collisions.record(backend_name, &kid, &parsed.0)?;
            tracing::info!(
                "backend '{}' encryption: ENABLED (AES-256-GCM proxy, key_id={})",
                backend_name,
                kid
            );
            let env_name = env_name_for_backend(backend_name);
            if std::env::var(&env_name).is_err() {
                tracing::warn!(
                    "backend '{}' encryption key was loaded from config file (not {}). \
                     Keep an off-box backup of the key; if the config file is lost, all \
                     encrypted objects on this backend become unrecoverable.",
                    backend_name,
                    env_name
                );
            }
            (Some(parsed), Some(kid), crate::storage::WriteMode::Encrypt)
        }
        E::Aes256GcmProxy { key: None, .. } => {
            tracing::warn!(
                "backend '{}' has encryption mode aes256-gcm-proxy but no key is \
                 configured — writes will NOT be encrypted on this backend. Check YAML \
                 or env var.",
                backend_name
            );
            (None, None, crate::storage::WriteMode::Encrypt)
        }
        E::SseKms { .. } | E::SseS3 { .. } => {
            // Native S3-side encryption — the S3Backend constructor
            // already received the matching `NativeEncryptionConfig`
            // via `build_raw_backend`. The wrapper's primary key is
            // None and writes ALWAYS skip encryption (PassThrough).
            // The inner backend handles encryption at its layer.
            tracing::info!(
                "backend '{}' encryption: ENABLED (native {})",
                backend_name,
                enc.mode_tag()
            );
            (None, None, crate::storage::WriteMode::PassThrough)
        }
        // mode: none — no primary key. WriteMode::Encrypt with key=None
        // is passthrough by construction (see WriteMode doc comment).
        // Leaving it Encrypt keeps the degenerate case indistinguishable
        // from "no encryption configured at all".
        E::None { .. } => (None, None, crate::storage::WriteMode::Encrypt),
    };

    // Resolve the decrypt-only shim from the legacy_* fields (Step 5).
    // Both halves must be present; otherwise the shim silently
    // ignores itself (matches the "needs both id + key to fire"
    // invariant in `pick_decrypt_key`).
    let (legacy_key_opt, legacy_kid_opt) = resolve_legacy_shim(backend_name, enc)?;
    if let (Some(_), Some(ref kid)) = (&legacy_key_opt, &legacy_kid_opt) {
        tracing::info!(
            "backend '{}' decrypt-only shim active (legacy key_id='{}') — reads of \
             objects stamped with that id will decrypt with legacy_key; new writes \
             use the current mode. Remove legacy_key / legacy_key_id from the \
             backend's encryption config once all historical objects have been \
             re-written or deleted.",
            backend_name,
            kid
        );
    }

    let enc_config = Arc::new(ArcSwap::new(Arc::new(crate::storage::EncryptionConfig {
        key: primary_key,
        key_id: primary_kid,
        write_mode,
        legacy_key: legacy_key_opt,
        legacy_key_id: legacy_kid_opt,
    })));
    Ok(Box::new(crate::storage::EncryptingBackend::new(
        inner, enc_config,
    )))
}

/// Pull the legacy_key / legacy_key_id pair out of the per-backend
/// encryption config, parse the hex key, and derive the id if the
/// operator left it implicit. Returns a pair of Options — BOTH
/// present means "shim active"; either one alone is silently
/// ignored (matches the wrapper's bilateral check).
///
/// Unlike the primary key path, the legacy key_id uses a reserved
/// backend-name suffix `{backend_name}::legacy` so an operator who
/// derives both from the same key material (rotation-shaped transition)
/// still gets distinct primary and legacy ids.
fn resolve_legacy_shim(
    backend_name: &str,
    enc: &crate::config::BackendEncryptionConfig,
) -> Result<(Option<crate::storage::EncryptionKey>, Option<String>), StorageError> {
    let Some(hex) = enc.legacy_key() else {
        return Ok((None, None));
    };
    let parsed = crate::storage::EncryptionKey::from_hex(hex).map_err(|e| {
        StorageError::Encryption(format!("backend '{}' legacy_key: {}", backend_name, e))
    })?;
    let kid = legacy_key_id_for(backend_name, enc.legacy_key_id(), &parsed);
    Ok((Some(parsed), Some(kid)))
}

/// The id stamped on objects written under a backend's legacy key: the
/// explicit `legacy_key_id`, else derived from `{backend_name}::legacy`.
fn legacy_key_id_for(
    backend_name: &str,
    explicit: Option<&str>,
    key: &crate::storage::EncryptionKey,
) -> String {
    match explicit {
        Some(explicit) => explicit.to_string(),
        None => derive_key_id(&format!("{backend_name}::legacy"), &key.0),
    }
}

/// The legacy (decrypt-only) key id of a backend, as the wrapper resolves
/// it; `None` when no legacy key is configured or it does not parse.
pub(crate) fn effective_legacy_key_id(
    backend_name: &str,
    enc: &crate::config::BackendEncryptionConfig,
) -> Option<String> {
    let parsed = crate::storage::EncryptionKey::from_hex(enc.legacy_key()?).ok()?;
    Some(legacy_key_id_for(
        backend_name,
        enc.legacy_key_id(),
        &parsed,
    ))
}

/// Pure integrity check for a freshly-loaded reference baseline.
///
/// `expected_sha256` is the reference's own recorded checksum (from its
/// stored `FileMetadata.file_sha256`). When it is empty we cannot verify
/// — references uploaded out-of-band (e.g. the Python CLI, or fallback
/// metadata with no DG xattrs) carry no checksum — so we treat that as a
/// pass and let the downstream per-object checksum be the safety net.
///
/// When the checksum IS present and disagrees with the actual data, the
/// reference on disk is corrupt; returning the `(expected, actual)` pair
/// lets the caller fail fast WITHOUT caching the bad bytes. Without this,
/// a corrupted reference would be cached on the first miss and poison
/// every subsequent delta GET in the deltaspace until natural eviction
/// (the downstream checksum-mismatch path in `retrieve.rs` only evicts
/// after a reconstruction has already failed).
fn reference_integrity_ok(actual_sha256: &str, expected_sha256: &str) -> Result<(), String> {
    if expected_sha256.is_empty() || actual_sha256 == expected_sha256 {
        Ok(())
    } else {
        Err(expected_sha256.to_string())
    }
}

/// Derive the per-object `key_id` from the backend name + the 32 key
/// bytes. Name is hashed in first, followed by a 0x00 separator, then
/// the key bytes. Truncated to 16 hex chars of SHA-256.
///
/// Name mixing disambiguates "two backends with the same key material"
/// so objects don't accidentally decrypt across backends — the read
/// path's `check_key_id_match` would reject with a specific error
/// rather than the underlying AEAD having any chance to succeed on
/// ciphertext that "happened to" come from a different backend.
///
/// Operators who WANT cross-backend portability pin an explicit
/// matching `key_id` on both — that's the documented escape hatch,
/// exercised by `test_key_id_collision_allowed_with_same_key`.
///
/// Shared with the admin-API summary path (`field_level::derive_key_id_for_summary`)
/// so the stamped id on disk ALWAYS matches the id the operator sees
/// in the Backends panel; drift between the two surfaces would mean
/// a "rotated key" badge that doesn't correspond to any real object
/// metadata.
pub(crate) fn derive_key_id(backend_name: &str, key_bytes: &[u8; 32]) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    hasher.update(backend_name.as_bytes());
    hasher.update(b"\0"); // separator: "ab"+"c" ≠ "a"+"bc"
    hasher.update(key_bytes);
    hex::encode(&hasher.finalize()[..8])
}

/// Canonical env var name for a backend's encryption key. Matches the
/// `apply_backend_encryption_env` pairing so an operator who sets
/// `DGP_BACKEND_EU_ARCHIVE_ENCRYPTION_KEY` has that key land on
/// backend `eu-archive` and the "key loaded from file" log points
/// back at the SAME env var name.
fn env_name_for_backend(backend_name: &str) -> String {
    if backend_name == "default" {
        "DGP_ENCRYPTION_KEY".to_string()
    } else {
        format!(
            "DGP_BACKEND_{}_ENCRYPTION_KEY",
            crate::config::env_suffix_for_backend_name(backend_name)
        )
    }
}

impl<S: StorageBackend> DeltaGliderEngine<S> {
    const INTERNAL_REFERENCE_NAME: &'static str = "__reference__";

    /// Access the underlying storage backend (for operations that bypass the delta engine)
    pub fn storage(&self) -> &S {
        &self.storage
    }

    /// Access the bucket policy registry (for quota checks, compression settings, etc.)
    pub fn bucket_policy_registry(&self) -> &crate::bucket_policy::BucketPolicyRegistry {
        &self.bucket_policies
    }

    /// Create a new engine with a custom storage backend.
    pub fn new_with_backend(
        storage: Arc<S>,
        config: &Config,
        metrics: Option<Arc<Metrics>>,
    ) -> Self {
        // PERF: codec_concurrency controls how many xdelta3 subprocesses can run
        // in parallel. Defaults to num_cpus * 4 (xdelta3 decode is fast — the bottleneck
        // is network I/O fetching reference+delta from S3, not CPU). Minimum 8.
        // Configurable via DGP_CODEC_CONCURRENCY.
        let codec_concurrency = config.codec_concurrency.unwrap_or_else(|| {
            let cpus = std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(4);
            (cpus * 4).max(16)
        });
        let spool = Arc::new(
            crate::deltaglider::spool::SpoolDir::shared()
                .unwrap_or_else(|e| panic!("failed to init spool dir: {e}")),
        );
        let range_spools = crate::deltaglider::range_spool::RangeSpoolCache::new(
            std::time::Duration::from_secs(config.range_spool_ttl_secs),
            crate::deltaglider::range_spool::MAX_ENTRIES,
        );
        spool.register_evictor(Arc::downgrade(&range_spools) as _);
        Self {
            storage,
            codec: Arc::new(DeltaCodec::new(config.max_object_size as usize)),
            file_router: FileRouter::new(),
            cache: ReferenceCache::new(config.cache_size_mb),
            max_object_size: config.max_object_size,
            max_passthrough_object_size: config.max_passthrough_object_size,
            codec_semaphore: Arc::new(Semaphore::new(codec_concurrency)),
            prefix_locks: shared_prefix_locks(),
            reference_lock: None,
            metrics,
            metadata_cache: MetadataCache::new((config.metadata_cache_mb as u64) * 1024 * 1024),
            bucket_policies: crate::bucket_policy::BucketPolicyRegistry::new(
                config.buckets.clone(),
                config.max_delta_ratio,
            )
            .with_reserved_bucket(config.config_sync_bucket.as_deref()),
            bucket_usage: None,
            spool,
            range_spools,
        }
    }

    /// Attach the per-instance usage counter (builder; called once at startup
    /// after the usage DB is opened). The handle survives engine rebuilds by
    /// being re-attached.
    pub fn with_bucket_usage(
        mut self,
        usage: Option<Arc<crate::bucket_usage::BucketUsage>>,
    ) -> Self {
        self.bucket_usage = usage;
        self
    }

    /// Attach the cross-instance reference lock (builder; re-attached on engine
    /// rebuild, mirroring `with_bucket_usage`). `None` keeps single-instance
    /// behavior — the in-process `prefix_locks` mutex is the only lock and no S3
    /// round-trip is paid.
    pub fn with_reference_lock(
        mut self,
        lock: Option<Arc<dyn crate::coordination::ReferenceLock>>,
    ) -> Self {
        self.reference_lock = lock;
        self
    }

    /// Best-effort: fold a stored object into the bucket counter. Never fails
    /// the S3 path. Applies the NET delta the store path captured:
    /// - new object: +1 / +logical / +stored
    /// - overwrite (`result.replaced` set): subtract the prior version first so
    ///   the count nets to +0 objects (S3 PUT is an upsert — a blind +1 here is
    ///   the over-count bug the review caught)
    /// - a newly-seeded reference.bin: + its bytes into stored_bytes (symmetric
    ///   with `record_delete`'s reclamation subtraction, so inline == scan).
    fn record_store(&self, bucket: &str, result: &StoreResult) {
        let Some(u) = &self.bucket_usage else { return };
        // net: -prior (if overwrite) + new object, + any newly-seeded reference.
        u.apply_net(
            bucket,
            result.replaced.as_deref(),
            Some(&result.metadata),
            result.reference_created_bytes as i64,
        );
    }

    /// Best-effort: fold a deleted object out of the bucket counter (-1), plus
    /// any reclaimed reference bytes (stored-only) so stored_bytes stays exact.
    fn record_delete(&self, bucket: &str, meta: &FileMetadata, reclaimed_ref_bytes: u64) {
        let Some(u) = &self.bucket_usage else { return };
        u.apply_net(bucket, Some(meta), None, -(reclaimed_ref_bytes as i64));
    }

    /// Resolve the prior object at `bucket/key` for overwrite-net accounting —
    /// only when a counter is attached. `None` on miss / no counter.
    async fn prior_for_counter(&self, bucket: &str, key: &str) -> Option<FileMetadata> {
        self.bucket_usage.as_ref()?;
        let (obj_key, deltaspace_id) = self.validated_key(bucket, key).ok()?;
        self.resolve_metadata(bucket, &deltaspace_id, &obj_key)
            .await
            .ok()
            .flatten()
    }

    /// Best-effort counter update for the delta-passthrough FAST PATH
    /// (`transfer.rs`), which ships a `.delta` verbatim via `put_delta_raw` and
    /// thus bypasses the `store()` choke point. Overwrite-aware + adds any
    /// reference the copy seeded. Mirrors [`Self::record_store`].
    /// Snapshot the destination's PRIOR metadata for fast-path accounting.
    /// MUST be called BEFORE the fast-path write — calling `prior_for_counter`
    /// after the write returns the just-written delta, netting an overwrite to
    /// zero (the dest bucket usage counter then never grows).
    pub async fn fast_path_prior(&self, bucket: &str, dest_key: &str) -> Option<FileMetadata> {
        self.prior_for_counter(bucket, dest_key).await
    }

    pub fn record_fast_path_copy(
        &self,
        bucket: &str,
        prior: Option<&FileMetadata>,
        delta_meta: &FileMetadata,
        seeded_reference_bytes: u64,
    ) {
        let Some(u) = &self.bucket_usage else { return };
        u.apply_net(
            bucket,
            prior,
            Some(delta_meta),
            seeded_reference_bytes as i64,
        );
    }

    /// Return a reference to the metadata cache (for handler-level access).
    pub fn metadata_cache(&self) -> &MetadataCache {
        &self.metadata_cache
    }

    /// Returns whether the xdelta3 CLI binary is available for legacy delta decoding.
    pub fn is_cli_available(&self) -> bool {
        self.codec.is_cli_available()
    }

    /// The installed xdelta3 version line (e.g. "Xdelta version 3.0.11..."), if any.
    pub fn cli_version(&self) -> Option<&str> {
        self.codec.cli_version()
    }

    /// Bytes above which a delta-eligible PUT routes through the streaming spool
    /// store (`store_spooled_delta`). Tied to `max_object_size`; overridable via
    /// `DGP_SPOOL_THRESHOLD_BYTES` (shared with the GET-side threshold).
    pub fn spool_store_threshold(&self) -> u64 {
        crate::config::env_parse_with_default("DGP_SPOOL_THRESHOLD_BYTES", self.max_object_size)
    }

    /// Whether `key`'s filename is delta-eligible (used by the adapter to decide
    /// the streaming-store route before constructing a spool).
    pub fn is_delta_eligible_key(&self, key: &str) -> bool {
        let filename = key.rsplit('/').next().unwrap_or(key);
        self.file_router.is_delta_eligible(filename)
    }

    /// Run a spool acquisition under the configured timeout, mapping a timeout to
    /// SlowDown (don't park the request + its budget forever under contention).
    /// The ONE place the timeout/Overloaded policy lives — both PUT/POST
    /// (`spool_acquire`) and GET (`spool_acquire_pair`) go through it.
    async fn with_spool_timeout<T, F>(fut: F) -> Result<T, EngineError>
    where
        F: std::future::Future<Output = std::io::Result<T>>,
    {
        Self::with_spool_timeout_io(fut).await?.map_err(|e| {
            // A holder refused a wait (hold-and-wait guard): retryable.
            if e.kind() == crate::deltaglider::spool::CONTENDED {
                EngineError::Overloaded(e.to_string())
            } else {
                EngineError::Storage(StorageError::from(e))
            }
        })
    }

    /// [`Self::with_spool_timeout`] that hands back the acquisition's own
    /// `io::Result`, for a caller that acts on its error kind.
    async fn with_spool_timeout_io<T, F>(fut: F) -> Result<std::io::Result<T>, EngineError>
    where
        F: std::future::Future<Output = std::io::Result<T>>,
    {
        let secs = crate::config::env_parse_with_default("DGP_SPOOL_ACQUIRE_TIMEOUT_SECS", 120u64);
        tokio::time::timeout(std::time::Duration::from_secs(secs), fut)
            .await
            .map_err(|_| {
                EngineError::Overloaded("spool budget exhausted; retry shortly".to_string())
            })
    }

    /// Acquire a spool file (timed). For the adapter to stage a large PUT/POST
    /// body before `store_spooled_delta`. Both ingest paths share it (B1.1).
    pub async fn spool_acquire(
        &self,
        bytes: u64,
    ) -> Result<crate::deltaglider::spool::Spool, EngineError> {
        Self::with_spool_timeout(self.spool.acquire(bytes)).await
    }

    /// Acquire a deadlock-safe spool PAIR (timed) — the GET reconstruct path.
    pub async fn spool_acquire_pair(
        &self,
        a: u64,
        b: u64,
    ) -> Result<
        (
            crate::deltaglider::spool::Spool,
            crate::deltaglider::spool::Spool,
        ),
        EngineError,
    > {
        Self::with_spool_timeout(self.spool.acquire_pair(a, b)).await
    }

    /// Reserve, BEFORE the deltaspace lock, the spool that a file-streaming
    /// storage write needs for its own temp files (the encrypting wrapper's
    /// ciphertext). Waiting for budget under the lock is hold-and-wait: a
    /// streaming PUT that holds its body spool can wait for the same lock.
    /// `held_mib`: spool budget the op holds already (its body spool, or its
    /// relay parts); a holder never waits. `None`: no spool needed.
    pub(crate) async fn reserve_storage_spool(
        &self,
        bucket: &str,
        bytes: u64,
        parts: bool,
        held_mib: usize,
    ) -> Result<Option<crate::deltaglider::spool::SpoolReservation>, EngineError> {
        let need = self
            .storage
            .file_put_spool_bytes(bucket, bytes, parts)
            .await;
        if need == 0 {
            return Ok(None);
        }
        Self::with_spool_timeout(self.spool.reserve_beside(held_mib, need))
            .await
            .map(Some)
    }

    /// The spool file for the buffered codec's source, taken WITHOUT waiting:
    /// the buffered PUT encodes under the deltaspace lock. A full budget is a
    /// retryable SlowDown.
    pub(crate) fn codec_source_spool_now(
        &self,
        bytes: usize,
    ) -> Result<crate::deltaglider::spool::Spool, EngineError> {
        self.spool.try_acquire(bytes as u64).map_err(|e| {
            if e.kind() == crate::deltaglider::spool::CONTENDED {
                EngineError::Overloaded(e.to_string())
            } else {
                EngineError::Storage(StorageError::from(e))
            }
        })
    }

    /// `spool_acquire` for an op that may already hold a spool (`held`).
    pub(crate) async fn spool_acquire_beside(
        &self,
        held: Option<&crate::deltaglider::spool::Spool>,
        bytes: u64,
    ) -> Result<crate::deltaglider::spool::Spool, EngineError> {
        Self::with_spool_timeout(self.spool.acquire_beside(held, bytes)).await
    }

    /// `spool_acquire_pair` for an op that already holds `held` (the streaming
    /// PUT's body spool): the pair is clamped so the op never waits for budget
    /// it holds itself. `None`: the budget is not free now, and a holder
    /// never waits (hold-and-wait deadlock, see `SpoolDir::reserve_within`);
    /// the caller goes on without the pair.
    pub(crate) async fn spool_acquire_pair_beside(
        &self,
        held: &crate::deltaglider::spool::Spool,
        a: u64,
        b: u64,
    ) -> Result<
        Option<(
            crate::deltaglider::spool::Spool,
            crate::deltaglider::spool::Spool,
        )>,
        EngineError,
    > {
        match Self::with_spool_timeout_io(self.spool.acquire_pair_beside(Some(held), a, b)).await? {
            Ok(pair) => Ok(Some(pair)),
            Err(e) if e.kind() == crate::deltaglider::spool::CONTENDED => Ok(None),
            Err(e) => Err(EngineError::Storage(StorageError::from(e))),
        }
    }

    /// Whether the codec passes `-a` (armor disabled) to xdelta3 (3.1+ only).
    pub fn codec_armor_disabled(&self) -> bool {
        self.codec.armor_disabled()
    }

    /// Returns the maximum object size in bytes.
    pub fn max_object_size(&self) -> u64 {
        self.max_object_size
    }

    /// Streaming-passthrough size ceiling (Phase B).
    pub fn max_passthrough_object_size(&self) -> u64 {
        self.max_passthrough_object_size
    }

    /// Encryption-mode label of the backend serving `bucket`
    /// (`transfer_plan::backend_supports_native_multipart` consumes this).
    pub fn multipart_storage_label(&self, bucket: &str) -> &'static str {
        self.storage.multipart_storage_label(bucket)
    }

    /// True when a streaming multipart copy to `bucket` stays memory-bounded:
    /// the backend must (a) NOT be a whole-object proxy-AES backend (the label
    /// gate) AND (b) write parts durably+incrementally (native multipart).
    /// A filesystem/buffering destination fails (b) and would otherwise retain
    /// the whole object in RAM on the "streaming" path — route it to spool.
    pub fn destination_supports_native_multipart(&self, bucket: &str) -> bool {
        crate::transfer_plan::backend_supports_native_multipart(
            self.storage.multipart_storage_label(bucket),
        ) && self.storage.supports_native_multipart(bucket)
    }

    /// True when a lite LIST of `bucket` carries trustworthy logical facts
    /// (real user_metadata + plaintext size/etag). False → parity must HEAD
    /// every key for ownership + logical size (S3, or an encrypting backend).
    pub fn lite_list_carries_logical_facts(&self, bucket: &str) -> bool {
        self.storage.lite_list_carries_logical_facts(bucket)
    }

    /// Return the number of entries in the reference cache (O(1) atomic read).
    pub fn cache_entry_count(&self) -> u64 {
        self.cache.entry_count()
    }

    /// Return the weighted size of the reference cache in bytes (O(1) atomic read).
    pub fn cache_weighted_size(&self) -> u64 {
        self.cache.weighted_size()
    }

    /// Return the configured maximum cache capacity in bytes.
    pub fn cache_max_capacity(&self) -> u64 {
        self.cache.max_capacity_bytes()
    }

    /// Return available codec semaphore permits.
    pub fn codec_available_permits(&self) -> usize {
        self.codec_semaphore.available_permits()
    }

    /// Borrow the metrics handle (None in tests). Lets transfer/replication
    /// code clone the `Arc<Metrics>` into part/object closures for counters.
    #[inline]
    pub fn metrics(&self) -> Option<&Arc<Metrics>> {
        self.metrics.as_ref()
    }

    /// Run a closure with the metrics if enabled (no-op in tests).
    #[inline]
    fn with_metrics(&self, f: impl FnOnce(&Metrics)) {
        if let Some(m) = &self.metrics {
            f(m);
        }
    }

    /// Build the cache key for a deltaspace's reference.
    /// THE per-deltaspace key: the reference cache and the in-process
    /// deltaspace lock. Keyed by the STORAGE, so two alias names of one real
    /// bucket (one reference.bin) share the entry and the lock.
    fn cache_key(&self, bucket: &str, deltaspace_id: &str) -> String {
        format!(
            "{}/{}",
            self.storage.storage_identity(bucket),
            deltaspace_id
        )
    }

    /// Try to acquire a codec permit, returning `Overloaded` if all slots are busy.
    /// Use for PUT (fail fast — don't queue uploads holding large bodies in memory).
    fn try_acquire_codec(&self) -> Result<tokio::sync::SemaphorePermit<'_>, EngineError> {
        self.codec_semaphore.try_acquire().map_err(|_| {
            EngineError::Overloaded("all delta codec slots busy — try again later".into())
        })
    }

    /// Wait for a codec permit with a timeout. Use for GET (users expect downloads to
    /// work even if they queue briefly behind other reconstructions).
    async fn acquire_codec_timeout(
        &self,
        timeout: std::time::Duration,
    ) -> Result<tokio::sync::SemaphorePermit<'_>, EngineError> {
        match tokio::time::timeout(timeout, self.codec_semaphore.acquire()).await {
            Ok(Ok(permit)) => Ok(permit),
            Ok(Err(_closed)) => Err(EngineError::Overloaded("codec semaphore closed".into())),
            Err(_elapsed) => Err(EngineError::Overloaded(
                "timed out waiting for codec slot — server too busy".into(),
            )),
        }
    }

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
    async fn acquire_reference_lock(
        &self,
        bucket: &str,
        deltaspace: &str,
    ) -> Result<ReferenceLockGuard, EngineError> {
        let Some(lock) = self.reference_lock.clone() else {
            return Ok(ReferenceLockGuard::inert());
        };
        let key = crate::coordination::reference_lock::lock_object_key(
            &self.storage.storage_identity(bucket),
            deltaspace,
        );
        let owner = format!("ref-{}", uuid::Uuid::new_v4());
        let deadline = std::time::Instant::now() + lock.acquire_timeout();
        let now_fn = || crate::event_outbox::current_unix_seconds();
        match crate::coordination::reference_lock::acquire_blocking(
            lock.as_ref(),
            &key,
            &owner,
            deadline,
            &now_fn,
        )
        .await
        {
            Ok(true) => {
                // Observe reference.bin under the lock: the fence of every
                // reference write this hold makes. On an error the guard is
                // built first so that its drop releases the lock.
                let guard =
                    ReferenceLockGuard::held(lock, key, owner, crate::storage::RefFence::Unfenced);
                let fence = self.storage.reference_fence(bucket, deltaspace).await?;
                if let Some(h) = &guard.hold {
                    *h.fence.lock() = fence;
                }
                Ok(guard)
            }
            Ok(false) => Err(EngineError::Storage(StorageError::Other(format!(
                "reference lock for deltaspace '{bucket}/{deltaspace}' held by another instance; \
                 write timed out to avoid corrupting reference.bin"
            )))),
            Err(e) => Err(EngineError::Storage(StorageError::Other(format!(
                "reference lock acquire failed for deltaspace '{bucket}/{deltaspace}': {e}"
            )))),
        }
    }

    /// Prune prefix lock entries that are no longer actively held.
    /// An entry with `Arc::strong_count() == 1` means only the map references it
    /// (no outstanding `OwnedMutexGuard`), so it can be safely removed.
    /// Only runs when the map exceeds a size threshold to avoid overhead.
    fn cleanup_prefix_locks(&self) {
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

    // === Raw deltaspace blob accessors (replication delta-passthrough) ===
    //
    // These read/write the LITERAL stored blob + metadata through the
    // routed+wrapped storage top. For a plaintext object the encrypting
    // wrapper is a no-op so the round-trip is byte-verbatim; markers on
    // the returned metadata reflect AT-REST state (the wrapper encrypts
    // bodies, not metadata). Policy lives in `transfer.rs`; the engine
    // only exposes the routed raw I/O + the per-deltaspace lock.

    /// Read a delta blob verbatim from a deltaspace.
    pub async fn get_delta_raw(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<Vec<u8>, StorageError> {
        self.storage.get_delta(bucket, prefix, filename).await
    }

    /// The cross-instance hold of the enclosing [`Self::with_dest_prefix_lock`]
    /// for a raw `what` write. Outside it: an inert guard single-instance
    /// (the in-process lock is the whole story), a refusal multi-instance
    /// (the write would be unfenced against a reference nobody holds).
    fn held_reference_lock(
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

    /// Write a delta blob + metadata verbatim into a deltaspace. Call it
    /// inside [`Self::with_dest_prefix_lock`]; outside it, multi-instance
    /// refuses.
    pub async fn put_delta_raw(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        // Inside `with_dest_prefix_lock`: the delta is only valid against the
        // reference the held cross-instance lock protects.
        let held = self.held_reference_lock(bucket, prefix, "delta")?;
        held.put_delta(&*self.storage, bucket, prefix, filename, data, metadata)
            .await
            .map_err(engine_to_storage)?;
        // Mirror every engine store path: a delta write supersedes any stale
        // PASSTHROUGH variant of the same key — leaving it behind lets a
        // later delta delete resurrect old content. Cache must drop too.
        if let Err(e) = self
            .delete_passthrough_idempotent(bucket, prefix, filename)
            .await
        {
            tracing::warn!(
                "fast-path delta write {}/{}/{}: stale passthrough cleanup failed: {}",
                bucket,
                prefix,
                filename,
                e
            );
        }
        let full_key = if prefix.is_empty() {
            filename.to_string()
        } else {
            format!("{}/{}", prefix, filename)
        };
        self.metadata_cache.invalidate(bucket, &full_key);
        Ok(())
    }

    /// Read a deltaspace reference blob verbatim.
    pub async fn get_reference_raw(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<Vec<u8>, StorageError> {
        self.storage.get_reference(bucket, prefix).await
    }

    /// Write a deltaspace reference blob + metadata verbatim. Call it inside
    /// [`Self::with_dest_prefix_lock`]: that holds the cross-instance lock
    /// the write goes through. Outside it, multi-instance refuses.
    pub async fn put_reference_raw(
        &self,
        bucket: &str,
        prefix: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        let held = self.held_reference_lock(bucket, prefix, "reference")?;
        held.put_reference(&*self.storage, bucket, prefix, data, metadata)
            .await
            .map_err(engine_to_storage)
    }

    /// Reference metadata as a `Result` (errors propagate) — for callers that
    /// must distinguish "no reference" from a read failure during seeding.
    pub async fn reference_metadata_raw(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<FileMetadata, StorageError> {
        self.storage.get_reference_metadata(bucket, prefix).await
    }

    /// Reference metadata for a deltaspace, or `None` when no reference exists.
    /// A backend error is treated as "no reference" here (read-only reporting
    /// path — the write paths propagate the error instead).
    pub async fn reference_meta(&self, bucket: &str, prefix: &str) -> Option<FileMetadata> {
        if !self
            .storage
            .has_reference(bucket, prefix)
            .await
            .unwrap_or(false)
        {
            return None;
        }
        self.storage
            .get_reference_metadata(bucket, prefix)
            .await
            .ok()
    }

    /// Delta metadata for one object (full Delta info incl. `ref_sha256`).
    pub async fn delta_meta(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<FileMetadata, StorageError> {
        self.storage
            .get_delta_metadata(bucket, prefix, filename)
            .await
    }

    /// Run `f` while holding the per-deltaspace prefix lock AND the
    /// cross-instance reference lock, serialising the fast-path reference
    /// seed against concurrent PUTs to that deltaspace on every node. When
    /// the cross-instance lock cannot be taken, `f` does not run and `R`
    /// reports the failure ([`ReferenceLockFailure`]).
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

    /// Parse and validate an S3 key, returning the parsed key and deltaspace ID.
    fn validated_key(&self, bucket: &str, key: &str) -> Result<(ObjectKey, String), EngineError> {
        let obj_key = ObjectKey::parse(bucket, key);
        obj_key
            .validate_object()
            .map_err(|e| EngineError::InvalidArgument(e.to_string()))?;
        let deltaspace_id = obj_key.deltaspace_id();
        Ok((obj_key, deltaspace_id))
    }

    /// Like `validated_key` but stricter — the INGEST (PUT) gate. Rejects `//`
    /// so a malformed key can't be STORED; reads/deletes keep using
    /// `validated_key` so pre-existing `//` objects stay reachable for cleanup
    /// on S3. (The filesystem backend refuses `.`/empty segments on every
    /// path it builds: `filesystem::check_path_segments`.)
    fn validated_key_ingest(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<(ObjectKey, String), EngineError> {
        let obj_key = ObjectKey::parse(bucket, key);
        obj_key
            .validate_ingest()
            .map_err(|e| EngineError::InvalidArgument(e.to_string()))?;
        // A folder marker is stored only by `store` (zero bytes, through
        // `put_directory_marker`). Every other ingest path would write a data
        // object under the marker's name, so they all refuse it here.
        if obj_key.is_directory_marker() {
            return Err(EngineError::InvalidArgument(
                "A key that ends in '/' is a folder marker and must have an empty body".to_string(),
            ));
        }
        let deltaspace_id = obj_key.deltaspace_id();
        Ok((obj_key, deltaspace_id))
    }

    /// Look up object metadata by checking both delta and passthrough storage,
    /// returning the most recent version if both exist.
    async fn resolve_object_metadata(
        &self,
        bucket: &str,
        prefix: &str,
        original_name: &str,
    ) -> Result<Option<FileMetadata>, StorageError> {
        let filename = original_name.rsplit('/').next().unwrap_or(original_name);

        // Fetch delta and passthrough metadata in parallel — saves one S3 round-trip
        let (delta_result, passthrough_result) = tokio::join!(
            self.storage.get_delta_metadata(bucket, prefix, filename),
            self.storage
                .get_passthrough_metadata(bucket, prefix, filename),
        );

        let delta = match delta_result {
            Ok(meta) => Some(meta),
            Err(StorageError::NotFound(_)) => None,
            Err(StorageError::Io(ref e)) => {
                warn!(
                    "I/O error reading delta metadata for {}/{}: {}",
                    prefix, filename, e
                );
                None
            }
            Err(e) => return Err(e),
        };
        let passthrough = match passthrough_result {
            Ok(meta) => Some(meta),
            Err(StorageError::NotFound(_)) => None,
            Err(StorageError::Io(ref e)) => {
                warn!(
                    "I/O error reading passthrough metadata for {}/{}: {}",
                    prefix, filename, e
                );
                None
            }
            Err(e) => return Err(e),
        };
        match (delta, passthrough) {
            (Some(d), Some(p)) => Ok(Some(if d.created_at >= p.created_at { d } else { p })),
            (Some(meta), None) | (None, Some(meta)) => Ok(Some(meta)),
            (None, None) => Ok(None),
        }
    }

    /// Resolve metadata for an object key, with no migration attempt.
    ///
    /// Use this from callers that **already hold** the per-deltaspace prefix lock
    /// (e.g. `delete()`). Calling `resolve_metadata_with_migration` from such a
    /// caller would deadlock because tokio's async Mutex is not reentrant.
    async fn resolve_metadata(
        &self,
        bucket: &str,
        deltaspace_id: &str,
        obj_key: &ObjectKey,
    ) -> Result<Option<FileMetadata>, EngineError> {
        Ok(self
            .resolve_object_metadata(bucket, deltaspace_id, &obj_key.full_key())
            .await?)
    }

    /// Resolve metadata with legacy migration fallback, acquiring the per-deltaspace
    /// prefix lock before migration to prevent races with concurrent `store()` calls.
    ///
    /// Uses double-checked locking:
    /// 1. Fast path: look up metadata without the lock.
    /// 2. If not found, acquire the prefix lock.
    /// 3. Re-check under the lock (a concurrent writer may have already migrated).
    /// 4. If still not found, attempt migration under the lock.
    ///
    /// **Do not call this from a caller that already holds the prefix lock** — use
    /// `resolve_metadata` instead to avoid a deadlock.
    async fn resolve_metadata_with_migration(
        &self,
        bucket: &str,
        deltaspace_id: &str,
        obj_key: &ObjectKey,
    ) -> Result<Option<FileMetadata>, EngineError> {
        // Fast path: most objects are found immediately without acquiring the lock.
        let metadata = self
            .resolve_object_metadata(bucket, deltaspace_id, &obj_key.full_key())
            .await?;
        if metadata.is_some() {
            return Ok(metadata);
        }

        // Legacy migration removed from GET hot path — it was blocking downloads
        // for 60+ seconds on large reference files. Migration is now batch-only
        // via the /_/api/admin/migrate endpoint.
        //
        // If the object still isn't found, return None and let the caller
        // fall through to the unmanaged passthrough path.
        Ok(None)
    }

    /// Decide whether a LIST entry needs a per-object HEAD to report accurate
    /// metadata, during `metadata=true` enrichment.
    ///
    /// A HEAD is needed only when the stored size could differ from the size the
    /// lite LIST already reports:
    ///   * the entry is already a delta (`meta.is_delta()`) — LIST shows the
    ///     delta (stored) size, HEAD recovers the original; OR
    ///   * the filename is delta-*eligible* by extension — it might be stored as
    ///     a delta even if this LIST entry wasn't flagged, so HEAD to be sure.
    ///
    /// For everything else — a passthrough, non-delta-eligible object (checksum
    /// sidecars, images, …) — the object is stored verbatim, so the LIST entry's
    /// size/etag are authoritative and the HEAD is pure waste. Pure function on
    /// the key + metadata; no I/O. Unit-tested.
    fn list_entry_needs_head(router: &FileRouter, key: &str, meta: &FileMetadata) -> bool {
        if meta.is_delta() {
            return true;
        }
        let filename = key.rsplit('/').next().unwrap_or(key);
        router.is_delta_eligible(filename)
    }

    /// A delta entry built from LIST data alone (no HEAD) — the `delta_stub`
    /// shape with empty `ref_sha256`. Its `file_size` is the STORED (delta)
    /// size, not the original, so it must never be cached as authoritative.
    fn is_unresolved_delta_stub(meta: &FileMetadata) -> bool {
        meta.is_unresolved_delta_stub()
    }

    /// Drop the cached metadata for one key. Used by the metadata-backfill
    /// job after an in-place metadata rewrite — the 10-minute cache would
    /// otherwise keep serving the pre-backfill (fallback) metadata on LIST.
    pub fn invalidate_metadata_cache(&self, bucket: &str, key: &str) {
        self.metadata_cache.invalidate(bucket, key);
    }

    pub async fn head(&self, bucket: &str, key: &str) -> Result<FileMetadata, EngineError> {
        // Note: we do NOT use the metadata cache for HEAD. The cache is used for
        // LIST enrichment and file_size correction, but HEAD must always verify
        // the object exists on storage to handle out-of-band deletions correctly.
        // The cost is one storage call per HEAD, but HEAD is already a storage call.

        let (obj_key, deltaspace_id) = self.validated_key(bucket, key)?;

        let meta = match self
            .resolve_metadata_with_migration(bucket, &deltaspace_id, &obj_key)
            .await?
        {
            Some(meta) => meta,
            None => {
                // No DG metadata — try reading passthrough metadata (lightweight HEAD).
                // If that also fails (unmanaged file with no DG headers), return NotFound.
                // Both S3 and filesystem backends now return fallback metadata for files
                // that exist without DG metadata, so this should succeed for any existing file.
                self.storage
                    .get_passthrough_metadata(bucket, &deltaspace_id, &obj_key.filename)
                    .await
                    .map_err(|e| match e {
                        StorageError::NotFound(_) => EngineError::NotFound(obj_key.full_key()),
                        other => EngineError::Storage(other),
                    })?
            }
        };

        // Populate metadata cache on successful backend lookup
        self.metadata_cache.insert(bucket, key, meta.clone());
        Ok(meta)
    }

    /// Returns `true` if a local prefix (bucket-relative) could contain keys
    /// matching the given user prefix.
    #[cfg(test)]
    fn local_prefix_could_match(local_prefix: &str, prefix: &str) -> bool {
        if prefix.is_empty() {
            return true;
        }
        if local_prefix.is_empty() {
            // Root-level keys are bare filenames (no '/'). They can only match
            // a prefix that doesn't contain '/' (e.g. prefix="app" matches "app.zip").
            return !prefix.contains('/');
        }
        let lp_slash = format!("{}/", local_prefix);
        // Include if: the local prefix starts with the user prefix (prefix is broader),
        // OR the user prefix drills into this local prefix (prefix is narrower/equal).
        lp_slash.starts_with(prefix) || prefix.starts_with(&lp_slash)
    }

    /// S3 ListObjects — the single owner of prefix filtering, delimiter collapsing,
    /// and pagination. All three are coupled (CommonPrefixes count toward max-keys
    /// and must be deduplicated across pages), so they must live in one place.
    #[instrument(skip(self))]
    pub async fn list_objects(
        &self,
        bucket: &str,
        prefix: &str,
        delimiter: Option<&str>,
        max_keys_raw: u32,
        continuation_token: Option<&str>,
        metadata: bool,
    ) -> Result<ListObjectsPage, EngineError> {
        // S3 requires max-keys >= 1; clamp to prevent pagination invariant violations.
        let max_keys = max_keys_raw.max(1);

        ObjectKey::validate_prefix(prefix)
            .map_err(|e| EngineError::InvalidArgument(e.to_string()))?;

        // Fast path: delegate listing to the storage backend (S3 pages
        // natively — with OR without a delimiter — so we never materialise
        // the whole prefix just to cut one page out of it).
        let mut page = if let Some(result) = self
            .storage
            .list_objects_delegated(bucket, prefix, delimiter, max_keys, continuation_token)
            .await?
        {
            ListObjectsPage {
                objects: result.objects,
                common_prefixes: result.common_prefixes,
                is_truncated: result.is_truncated,
                next_continuation_token: result.next_continuation_token,
            }
        } else {
            // Backend doesn't support delegated listing for this shape — fall
            // through to the generic bulk_list + in-memory paging path.
            self.list_objects_bulk(bucket, prefix, delimiter, max_keys, continuation_token)
                .await?
        };

        // Transparency without a HEAD per key: a lite LIST on an S3 backend
        // reports the STORED object (a delta's `.delta`, a ciphertext). Swap
        // in the logical size and ETag of each exact stored object (same key,
        // ETag and size), from this process's listing-size cache or from the
        // durable listing facts (one more LIST per page, any node, after a
        // restart; see `storage::listing_facts`). A miss keeps the stored
        // size: a client LIST never sends a HEAD (issue #82). Never fails.
        let sizes = if page.objects.is_empty() {
            Vec::new()
        } else {
            self.storage
                .resolve_listed_sizes(bucket, &mut page.objects, false)
                .await
        };

        // When metadata=true (MinIO extension), enrich objects with full
        // metadata from HEAD calls. Use the metadata cache to avoid HEAD
        // for objects we already know about — the biggest performance win
        // (1000 objects → 1000 cache lookups instead of 1000 HEADs).
        if metadata && !page.objects.is_empty() {
            let mut cache_hits = Vec::new();
            let mut cache_misses = Vec::new();

            for ((key, meta), size) in
                page.objects
                    .into_iter()
                    .zip(sizes.into_iter().chain(std::iter::repeat(
                        crate::storage::list_size_cache::ListedSize::Listed,
                    )))
            {
                if let Some(cached) = self.metadata_cache.get(bucket, &key) {
                    cache_hits.push((key, cached));
                } else if !size.is_known() {
                    // Only the stored size is known (a ciphertext whose facts
                    // are missing): a HEAD reads the logical one.
                    cache_misses.push((key, meta));
                } else if Self::list_entry_needs_head(&self.file_router, &key, &meta) {
                    // Delta or delta-eligible: the LIST entry carries the stored
                    // (delta) size; a HEAD is required to recover the original
                    // size + storage type.
                    cache_misses.push((key, meta));
                } else {
                    // Passthrough, non-delta-eligible file (e.g. a `.sha1`/`.sha512`
                    // checksum sidecar, an image). It is stored verbatim, so the
                    // LIST entry's size/etag ARE the truth — a per-object HEAD
                    // would return the same size and add nothing. Skipping it
                    // avoids an upstream HEAD per object (the dominant cost on
                    // build-artifact listings full of checksum sidecars, and the
                    // source of the HEAD-burst throttling seen in prod). Use the
                    // lite LIST metadata directly.
                    cache_hits.push((key, meta));
                }
            }

            if !cache_misses.is_empty() {
                let enriched = self
                    .storage
                    .enrich_list_metadata(bucket, cache_misses)
                    .await?;
                // Cache ONLY genuinely HEAD-resolved metadata. When a HEAD
                // sweep aborts under backend throttling, enrich_list_metadata
                // returns unresolved delta STUBS (empty ref_sha256, delta_size
                // = stored size) as a serviceable listing fallback — but those
                // must NOT poison the cache, or a later HEAD/GET would serve
                // the stub's wrong (stored, not original) size. A stub is
                // identifiable by an empty ref_sha256 on a Delta entry.
                for (key, meta) in &enriched {
                    if !Self::is_unresolved_delta_stub(meta) {
                        self.metadata_cache.insert(bucket, key, meta.clone());
                    }
                }
                cache_hits.extend(enriched);
            }

            // Re-sort by key to maintain S3 lexicographic ordering
            cache_hits.sort_by(|a, b| a.0.cmp(&b.0));
            page.objects = cache_hits;
        }

        Ok(page)
    }

    /// Return the `reference.bin` metadata for every deltaspace whose
    /// prefix begins with `scope_prefix` in the given bucket, plus a
    /// `truncated` flag set when the scan hit `limit` matching
    /// deltaspaces.
    ///
    /// `list_objects` deliberately hides references from S3-compatible
    /// callers (a `reference.bin` is an implementation detail, not a
    /// user-visible object). Anything reporting "true storage cost" or
    /// "honest savings" — the admin dashboard, the CLI `stats` command,
    /// the SPA's per-prefix savings chip — must add reference bytes to
    /// the on-disk total. This helper is the supported way to do that
    /// without re-implementing per-backend listing details at the call
    /// sites.
    ///
    /// `scope_prefix == ""` returns every reference in the bucket
    /// (bounded by `limit`).
    /// `limit: None` means "no cap"; `limit: Some(n)` stops after n
    /// matches and sets `truncated: true`. The constant
    /// [`Self::REFERENCE_SCAN_LIMIT`] is the recommended cap for
    /// latency-sensitive paths.
    ///
    /// Errors from `get_reference_metadata` for individual deltaspaces
    /// are logged and skipped — a missing or unreadable reference for
    /// one prefix should not poison the entire scan.
    pub async fn list_deltaspace_references(
        &self,
        bucket: &str,
        scope_prefix: &str,
        limit: Option<usize>,
    ) -> Result<ReferenceScan, EngineError> {
        let all = self.storage.list_deltaspaces(bucket).await?;
        // Normalise `scope_prefix` for the starts_with check below.
        // Storage backends return deltaspace prefixes WITHOUT trailing
        // slashes (e.g. `releases/v1`), but callers using the S3
        // convention pass `releases/v1/` here. Strip the trailing
        // slash so `releases/v1/`-shaped scopes match `releases/v1` and
        // `releases/v1/sub`. An empty scope means "everything".
        let scope_norm = scope_prefix.trim_end_matches('/');
        let mut references = Vec::new();
        let mut truncated = false;
        let prefix_match = |p: &str| -> bool {
            if scope_norm.is_empty() {
                return true;
            }
            p == scope_norm || p.starts_with(&format!("{scope_norm}/"))
        };
        for prefix in all {
            if !prefix_match(&prefix) {
                continue;
            }
            if limit.is_some_and(|n| references.len() >= n) {
                truncated = true;
                tracing::info!(
                    "list_deltaspace_references: hit cap {:?} for bucket={bucket} scope={scope_prefix} \
                     — caller should treat totals as a lower bound and surface `truncated` to the UI.",
                    limit,
                );
                break;
            }
            match self.storage.get_reference_metadata(bucket, &prefix).await {
                Ok(meta) => references.push((prefix.clone(), meta)),
                Err(e) => {
                    tracing::warn!(
                        "list_deltaspace_references: skipping {}/{} ({}). \
                         Savings totals for this scope will undercount the \
                         reference bytes for this deltaspace.",
                        bucket,
                        prefix,
                        e,
                    );
                }
            }
        }
        Ok(ReferenceScan {
            references,
            truncated,
        })
    }

    /// Internal: build a ListObjectsPage from bulk_list_objects + in-memory
    /// delimiter collapsing and pagination.
    async fn list_objects_bulk(
        &self,
        bucket: &str,
        prefix: &str,
        delimiter: Option<&str>,
        max_keys: u32,
        continuation_token: Option<&str>,
    ) -> Result<ListObjectsPage, EngineError> {
        // Single-pass listing: replaces list_deltaspaces + scan_deltaspace×N
        let bulk = self.storage.bulk_list_objects(bucket, prefix).await?;

        // Dedup by key, keeping latest version (shared logic with S3 backend)
        let mut items = crate::types::dedup_keep_latest(bulk);

        if !prefix.is_empty() {
            items.retain(|(key, _meta)| key.starts_with(prefix));
        }

        // --- Delimiter collapsing + pagination as a single operation ---
        //
        // When a delimiter is present, objects whose key (after the prefix)
        // contains the delimiter are collapsed into CommonPrefixes. Each
        // CommonPrefix counts as one entry toward max-keys, and is emitted
        // exactly once across all pages.

        if let Some(delim) = delimiter {
            // Collapse objects into CommonPrefixes where the key contains the delimiter
            let mut collapsed_objects = Vec::new();
            let mut seen_prefixes = std::collections::BTreeSet::new();

            for (key, meta) in items {
                let after = &key[prefix.len()..];
                if let Some(pos) = after.find(delim) {
                    let cp = format!("{}{}{}", prefix, &after[..pos], delim);
                    seen_prefixes.insert(cp);
                } else {
                    collapsed_objects.push((key, meta));
                }
            }

            let collapsed_prefixes: Vec<String> = seen_prefixes.into_iter().collect();
            let page = interleave_and_paginate(
                collapsed_objects,
                collapsed_prefixes,
                max_keys,
                continuation_token,
            );

            Ok(ListObjectsPage {
                objects: page.objects,
                common_prefixes: page.common_prefixes,
                is_truncated: page.is_truncated,
                next_continuation_token: page.next_continuation_token,
            })
        } else {
            // No delimiter — paginate raw objects
            let (is_truncated, next_token) =
                paginate_sorted(&mut items, max_keys, continuation_token, |(k, _)| k);

            Ok(ListObjectsPage {
                objects: items,
                common_prefixes: Vec::new(),
                is_truncated,
                next_continuation_token: next_token,
            })
        }
    }

    // === Bucket operations (delegate to storage) ===

    /// Create a real bucket on the storage backend.
    /// Make durable every object write that the storage deferred
    /// (`storage::with_deferred_fsync`).
    pub async fn flush_pending(&self) -> Result<(), EngineError> {
        Ok(self.storage.flush_pending().await?)
    }

    pub async fn create_bucket(&self, bucket: &str) -> Result<(), EngineError> {
        Ok(self.storage.create_bucket(bucket).await?)
    }

    /// Delete a real bucket on the storage backend (must be empty).
    pub async fn delete_bucket(&self, bucket: &str) -> Result<(), EngineError> {
        Ok(self.storage.delete_bucket(bucket).await?)
    }

    /// List all real buckets from the storage backend.
    pub async fn list_buckets(&self) -> Result<Vec<String>, EngineError> {
        Ok(self.storage.list_buckets().await?)
    }

    /// List all real buckets with their creation dates.
    pub async fn list_buckets_with_dates(
        &self,
    ) -> Result<Vec<(String, chrono::DateTime<chrono::Utc>)>, EngineError> {
        Ok(self.storage.list_buckets_with_dates().await?)
    }

    /// List buckets with optional backend-origin metadata.
    pub async fn list_bucket_origins(
        &self,
    ) -> Result<Vec<crate::storage::BucketListing>, EngineError> {
        Ok(self.storage.list_bucket_origins().await?)
    }

    /// Check if a real bucket exists on the storage backend.
    pub async fn head_bucket(&self, bucket: &str) -> Result<bool, EngineError> {
        Ok(self.storage.head_bucket(bucket).await?)
    }

    /// Best-effort delete of the sibling storage variant (the one NOT matched by
    /// resolve_metadata) so a stale passthrough/delta pair can't resurrect a
    /// deleted key. NotFound (the normal case — only one variant exists) and
    /// transient errors are swallowed with a debug log; the primary delete's
    /// result is authoritative.
    async fn delete_sibling_variant_best_effort(
        &self,
        bucket: &str,
        deltaspace_id: &str,
        filename: &str,
        delete_delta: bool,
    ) {
        let res = if delete_delta {
            self.storage
                .delete_delta(bucket, deltaspace_id, filename)
                .await
        } else {
            self.storage
                .delete_passthrough(bucket, deltaspace_id, filename)
                .await
        };
        if let Err(e) = res {
            if !matches!(e, StorageError::NotFound(_)) {
                debug!(
                    "sibling-variant cleanup for {}/{}/{} (delta={}) failed: {}",
                    bucket, deltaspace_id, filename, delete_delta, e
                );
            }
        }
    }

    /// Delete an object
    #[instrument(skip(self))]
    pub async fn delete(&self, bucket: &str, key: &str) -> Result<FileMetadata, EngineError> {
        Self::deleted(
            key,
            self.delete_inner(bucket, key, /* reclaim_reference = */ true, None)
                .await?,
        )
    }

    /// Delete `key` only if `still_ours` accepts the object as read under
    /// the deltaspace lock. Every PUT holds that lock, so no overwrite can
    /// land between the check and the delete (a HEAD, then a delete by key,
    /// removed an overwrite that landed in between). A peer INSTANCE's PUT
    /// does not take this lock: where the backend has a conditional delete
    /// (S3 `If-Match`), the delete is also pinned to the stored version the
    /// check saw; elsewhere (filesystem, a backend answering 501) the
    /// in-process lock is the only guard.
    #[instrument(skip(self, still_ours))]
    pub async fn delete_if(
        &self,
        bucket: &str,
        key: &str,
        still_ours: &(dyn Fn(&FileMetadata) -> bool + Send + Sync),
    ) -> Result<ConditionalDelete, EngineError> {
        self.delete_inner(bucket, key, true, Some(still_ours)).await
    }

    fn deleted(key: &str, outcome: ConditionalDelete) -> Result<FileMetadata, EngineError> {
        match outcome {
            ConditionalDelete::Deleted(meta) => Ok(*meta),
            // Unconditional deletes report a missing object as NotFound.
            ConditionalDelete::Changed | ConditionalDelete::Gone => {
                Err(EngineError::NotFound(key.to_string()))
            }
        }
    }

    /// Delete one member of a prefix sweep, SKIPPING the per-object
    /// "is the deltaspace empty now?" reference-reclamation scan.
    ///
    /// That scan lists the WHOLE deltaspace, so running it per object makes a
    /// prefix sweep O(N²) in directory reads (1100 objects ≈ 600k entry reads —
    /// enough to blow past the request timeout). A sweep is deleting everything
    /// anyway, so the caller runs [`Self::reclaim_empty_deltaspace`] ONCE when
    /// the sweep finishes. Semantics are otherwise identical to [`Self::delete`].
    pub async fn delete_in_sweep(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<FileMetadata, EngineError> {
        Self::deleted(
            key,
            self.delete_inner(bucket, key, /* reclaim_reference = */ false, None)
                .await?,
        )
    }

    /// Reclaim a deltaspace's `reference.bin` if no non-reference object remains.
    /// The tail half of [`Self::delete`], callable once after a prefix sweep.
    /// Idempotent and safe when the deltaspace still holds objects (no-op).
    pub async fn reclaim_empty_deltaspace(
        &self,
        bucket: &str,
        deltaspace_id: &str,
    ) -> Result<(), EngineError> {
        let _guard = self.acquire_prefix_lock(bucket, deltaspace_id).await;
        let Some((xnode, reclaimed_ref_bytes)) =
            self.reclaimable_reference(bucket, deltaspace_id).await?
        else {
            return Ok(());
        };
        xnode
            .delete_reference(&*self.storage, bucket, deltaspace_id)
            .await?;
        self.cache
            .invalidate(&self.cache_key(bucket, deltaspace_id));
        // Mirror `delete`'s accounting: the reclaimed reference bytes leave
        // stored_bytes (no object count change — the objects were counted as
        // they were individually deleted).
        if let Some(u) = &self.bucket_usage {
            u.apply_net(bucket, None, None, -(reclaimed_ref_bytes as i64));
        }
        Ok(())
    }

    /// `Some((lock, reference bytes))` when the deltaspace holds a reference
    /// and nothing else, so the reference can go. Caller holds the prefix
    /// lock. Multi-instance: the emptiness scan runs again under the
    /// cross-instance lock, because a peer can write a delta against the
    /// reference between the first scan and the delete. The first scan runs
    /// unlocked so a delete in a non-empty deltaspace pays no lock requests.
    async fn reclaimable_reference(
        &self,
        bucket: &str,
        deltaspace_id: &str,
    ) -> Result<Option<(ReferenceLockGuard, u64)>, EngineError> {
        let only_reference = |remaining: &[FileMetadata]| -> Option<u64> {
            let mut ref_bytes = None;
            for m in remaining {
                match m.storage_info {
                    StorageInfo::Reference { .. } => ref_bytes = Some(m.file_size),
                    _ => return None,
                }
            }
            Some(ref_bytes.unwrap_or(0))
        };
        let remaining = self.storage.scan_deltaspace(bucket, deltaspace_id).await?;
        let Some(mut ref_bytes) = only_reference(&remaining) else {
            return Ok(None);
        };
        if !self.storage.has_reference(bucket, deltaspace_id).await? {
            return Ok(None);
        }
        let xnode = self.acquire_reference_lock(bucket, deltaspace_id).await?;
        if xnode.is_cross_instance() {
            let remaining = self.storage.scan_deltaspace(bucket, deltaspace_id).await?;
            match only_reference(&remaining) {
                Some(b) if self.storage.has_reference(bucket, deltaspace_id).await? => {
                    ref_bytes = b
                }
                _ => return Ok(None),
            }
        }
        Ok(Some((xnode, ref_bytes)))
    }

    async fn delete_inner(
        &self,
        bucket: &str,
        key: &str,
        reclaim_reference: bool,
        still_ours: Option<&(dyn Fn(&FileMetadata) -> bool + Send + Sync)>,
    ) -> Result<ConditionalDelete, EngineError> {
        let (obj_key, deltaspace_id) = self.validated_key(bucket, key)?;

        info!("Deleting {}/{}", bucket, key);

        // Acquire per-deltaspace lock to prevent races with concurrent store/delete
        // operations that may create or clean up the reference.
        let _guard = self.acquire_prefix_lock(bucket, &deltaspace_id).await;

        // Use resolve_metadata (no migration) — we already hold the prefix lock, and
        // tokio::sync::Mutex is not reentrant, so calling resolve_metadata_with_migration
        // here would deadlock. Legacy objects that haven't been migrated yet will appear
        // as NotFound; a prior GET/HEAD on the key will have triggered migration.
        let Some(metadata) = self
            .resolve_metadata(bucket, &deltaspace_id, &obj_key)
            .await?
        else {
            return match still_ours {
                Some(_) => Ok(ConditionalDelete::Gone),
                None => Err(EngineError::NotFound(obj_key.full_key())),
            };
        };
        // A conditional delete also pins the stored version where the backend
        // can (S3 If-Match): a peer INSTANCE's PUT does not take our
        // in-process lock. The version is read BEFORE the check's read, so
        // any overwrite after it fails the delete.
        let mut pinned: Option<String> = None;
        let metadata = match still_ours {
            None => metadata,
            Some(ours) => {
                let variant = match metadata.storage_info {
                    StorageInfo::Delta { .. } => crate::storage::ObjectVariant::Delta,
                    _ => crate::storage::ObjectVariant::Passthrough,
                };
                let checked = match self
                    .storage
                    .variant_version(bucket, &deltaspace_id, &obj_key.filename, variant)
                    .await
                {
                    Ok(None) => metadata,
                    Ok(Some(version)) => {
                        pinned = Some(version);
                        match self
                            .resolve_metadata(bucket, &deltaspace_id, &obj_key)
                            .await?
                        {
                            Some(m)
                                if std::mem::discriminant(&m.storage_info)
                                    == std::mem::discriminant(&metadata.storage_info) =>
                            {
                                m
                            }
                            Some(_) => return Ok(ConditionalDelete::Changed),
                            None => return Ok(ConditionalDelete::Gone),
                        }
                    }
                    Err(StorageError::NotFound(_)) => return Ok(ConditionalDelete::Gone),
                    Err(e) => return Err(e.into()),
                };
                if !ours(&checked) {
                    return Ok(ConditionalDelete::Changed);
                }
                checked
            }
        };

        // Delete based on storage type — but ALSO clean up the OTHER variant.
        // A key can transiently have BOTH a passthrough and a delta sibling (e.g.
        // a PUT that stored as delta whose best-effort passthrough cleanup 500'd
        // and was only warned). resolve_metadata picks the newest, and deleting
        // only that variant leaves the stale sibling, which a later GET resolves
        // and serves — a deleted object RESURRECTS (H33). Delete both; the
        // non-resolved one is best-effort (NotFound is the normal case).
        match &metadata.storage_info {
            StorageInfo::Passthrough => {
                if let Some(version) = &pinned {
                    let v = crate::storage::ObjectVariant::Passthrough;
                    if !self
                        .storage
                        .delete_variant_if(bucket, &deltaspace_id, &obj_key.filename, v, version)
                        .await?
                    {
                        return Ok(ConditionalDelete::Changed);
                    }
                } else {
                    self.storage
                        .delete_passthrough(bucket, &deltaspace_id, &obj_key.filename)
                        .await?;
                }
                self.delete_sibling_variant_best_effort(
                    bucket,
                    &deltaspace_id,
                    &obj_key.filename,
                    /* delete_delta = */ true,
                )
                .await;
            }
            StorageInfo::Delta { .. } => {
                if let Some(version) = &pinned {
                    let v = crate::storage::ObjectVariant::Delta;
                    if !self
                        .storage
                        .delete_variant_if(bucket, &deltaspace_id, &obj_key.filename, v, version)
                        .await?
                    {
                        return Ok(ConditionalDelete::Changed);
                    }
                } else {
                    self.storage
                        .delete_delta(bucket, &deltaspace_id, &obj_key.filename)
                        .await?;
                }
                self.delete_sibling_variant_best_effort(
                    bucket,
                    &deltaspace_id,
                    &obj_key.filename,
                    /* delete_delta = */ false,
                )
                .await;
            }
            StorageInfo::Reference { .. } => {
                return Err(EngineError::InvalidArgument(
                    "Reference objects are internal and cannot be deleted directly".to_string(),
                ));
            }
        }

        // If this deltaspace no longer has any objects, clean up its reference
        // baseline. SKIPPED for prefix sweeps (`delete_in_sweep`): this scan
        // lists the entire deltaspace, so running it per object makes a sweep
        // O(N²) in directory reads. The sweep caller reclaims once at the end
        // via `reclaim_empty_deltaspace`.
        // Bytes of a reclaimed reference.bin (stored-only) — subtracted from the
        // counter so stored_bytes stays exact when the last delta is removed.
        let mut reclaimed_ref_bytes = 0u64;
        // The object is already gone: a failed reclaim check (a peer holds
        // the reference lock, a listing error) must not fail the DELETE.
        // The orphan reference is harmless and reclaimed on a later delete.
        let reclaimable = if reclaim_reference {
            match self.reclaimable_reference(bucket, &deltaspace_id).await {
                Ok(r) => r,
                Err(e) => {
                    warn!("reference reclaim skipped for {bucket}/{deltaspace_id}: {e}");
                    None
                }
            }
        } else {
            None
        };
        if let Some((xnode, ref_bytes)) = reclaimable {
            // Delete storage BEFORE invalidating cache — prevents stale cache entries
            // from a concurrent GET loading between invalidation and deletion.
            // Best-effort like the check above: the object is gone, so a lost
            // lock or a transient error must not turn the DELETE into a 500.
            match xnode
                .delete_reference(&*self.storage, bucket, &deltaspace_id)
                .await
            {
                Ok(()) => {
                    reclaimed_ref_bytes = ref_bytes;
                    let cache_key = self.cache_key(bucket, &deltaspace_id);
                    self.cache.invalidate(&cache_key);
                }
                Err(e) => warn!("reference reclaim failed for {bucket}/{deltaspace_id}: {e}"),
            }
        }

        // Invalidate metadata cache for the deleted key
        self.metadata_cache.invalidate(bucket, key);

        // Release the per-prefix lock before cleanup so strong_count drops to 1.
        drop(_guard);
        self.cleanup_prefix_locks();

        // Best-effort counter update: -1 object + reclaimed reference bytes.
        self.record_delete(bucket, &metadata, reclaimed_ref_bytes);

        debug!("Deleted {}/{}", bucket, key);
        Ok(ConditionalDelete::Deleted(Box::new(metadata)))
    }

    /// Get reference with caching. Returns `Bytes` for zero-copy sharing.
    /// Returns `(reference_data, cache_hit)`.
    /// `expected_sha256` is the reference sha the caller is about to rely on
    /// (the stored reference metadata on PUT, the delta's `ref_sha256` on
    /// GET; empty = cannot verify). A cached copy with another sha is stale:
    /// a peer node reseeded the deltaspace. Encoding against it wrote deltas
    /// that no other node could decode.
    async fn get_reference_cached(
        &self,
        bucket: &str,
        deltaspace_id: &str,
        expected_sha256: &str,
    ) -> Result<(bytes::Bytes, bool), EngineError> {
        let cache_key = self.cache_key(bucket, deltaspace_id);

        // Check cache first (Bytes clone is a cheap refcount increment)
        if let Some(data) = self.cache.get_matching(&cache_key, expected_sha256) {
            self.with_metrics(|m| m.cache_hits_total.inc());
            return Ok((data, true));
        }

        self.with_metrics(|m| m.cache_misses_total.inc());

        // Load the reference data and its recorded metadata together. The
        // metadata read is cheap (xattr / S3 HEAD) and runs in parallel so it
        // doesn't add a serial round-trip to the miss path. We use the
        // recorded checksum to verify the bytes BEFORE caching — a reference
        // that's corrupt on disk would otherwise be cached on the first miss
        // and poison every subsequent delta GET in the deltaspace.
        let (data_result, meta_result) = tokio::join!(
            self.storage.get_reference(bucket, deltaspace_id),
            self.storage.get_reference_metadata(bucket, deltaspace_id),
        );
        let data = data_result.map_err(|e| match e {
            StorageError::NotFound(_) => EngineError::MissingReference(deltaspace_id.to_string()),
            other => EngineError::Storage(other),
        })?;

        // Validate against the reference's own recorded checksum when present.
        // A missing metadata read or empty checksum (out-of-band / CLI-uploaded
        // references) is treated as "cannot verify" — we proceed and let the
        // downstream per-object checksum in retrieve.rs catch any corruption.
        let actual = hex::encode(Sha256::digest(&data));
        if let Ok(expected) = meta_result {
            if !expected.file_sha256.is_empty() {
                if let Err(expected_sha256) = reference_integrity_ok(&actual, &expected.file_sha256)
                {
                    // Do NOT cache corrupt bytes — fail fast so a single bad
                    // reference doesn't fan out into repeated reconstruction
                    // failures across the deltaspace.
                    warn!(
                        "Reference integrity check failed for {}/{}: expected {}, got {} — not caching",
                        bucket, deltaspace_id, expected_sha256, actual
                    );
                    return Err(EngineError::ChecksumMismatch {
                        key: format!("{}/.dg/reference.bin", deltaspace_id),
                        expected: expected_sha256,
                        actual,
                    });
                }
            }
        }

        // PERF: Convert Vec→Bytes once (zero-copy ownership transfer), then
        // clone the Bytes for the cache (refcount increment, no memcpy).
        // The old code did data.clone() (full 80MB memcpy) + Bytes::from — this
        // saves one memcpy per cache miss.
        let bytes = Bytes::from(data);
        self.cache.put(&cache_key, bytes.clone(), &actual);

        Ok((bytes, false))
    }
}

#[cfg(test)]
mod folder_marker_tests;

#[cfg(test)]
mod tests;

/// Cross-instance reference lock, engine side: loss detection before a
/// commit, renewal while held, and the reference writes that once skipped
/// the lock (delete-reclaim, sweep-reclaim, fast-path seed).
#[cfg(test)]
mod reference_lock_hold_tests;

/// Model test of the cross-instance reference-lock protocol: the pure
/// planners (`plan_lock_acquire`, `plan_lock_renew`), the CAS store they
/// drive (create-if-absent, If-Match replace/delete) and the guard's trust
/// rule (`hold_check`). Every remote step is split into its read and its
/// conditional write, so peers interleave between them, as they do on S3.
///
/// `loom`/`shuttle` cannot run the guard itself without rebuilding it on
/// their primitives (it uses `tokio::spawn`, `tokio::time`, `tokio::sync`,
/// `parking_lot` and `std::time::Instant`); the guard's in-process
/// interplay (heartbeat vs commit renew, release vs a renew in flight) is
/// covered by the `reference_lock_hold_tests` above.
#[cfg(test)]
mod lock_protocol_model_tests;
