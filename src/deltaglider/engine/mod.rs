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

mod buckets;
pub(crate) mod conditional;
mod construction;
mod delete;
mod list;
mod locking;
mod metadata;
mod raw;
mod resources;
mod retrieve;
pub(crate) mod store;
mod usage;

use locking::*;

pub(crate) use construction::{derive_key_id, effective_legacy_key_id};
pub(crate) use list::interleave_and_paginate;
pub use locking::RefWriteProof;

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
    /// Keys in `objects` that show only their STORED size: neither this
    /// process nor the durable listing facts knew the logical size, and no
    /// HEAD ran. `DGP_DEBUG_HEADERS` reports the count on the response.
    pub facts_missing_keys: Vec<String>,
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
    /// Env-only settings, copied from the config at build (see
    /// [`crate::config::RuntimeTuning`]).
    tuning: crate::config::RuntimeTuning,
}

/// Type alias for engine with dynamic backend dispatch
pub type DynEngine = DeltaGliderEngine<Box<dyn StorageBackend>>;

impl<S: StorageBackend> DeltaGliderEngine<S> {
    const INTERNAL_REFERENCE_NAME: &'static str = "__reference__";
}

#[cfg(test)]
mod folder_marker_tests;

#[cfg(test)]
mod tests;

/// The store decision, pinned for the buffered and the streaming PUT.
#[cfg(test)]
mod store_plan_tests;

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
