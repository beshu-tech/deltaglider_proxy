//! DeltaGlider engine - main orchestrator for delta-based storage

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
use std::time::Instant;
use thiserror::Error;
use tokio::sync::Semaphore;
use tracing::{debug, info, instrument, warn};

mod retrieve;
mod store;

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

#[derive(Debug, Clone)]
pub struct ListObjectsPage {
    /// Direct objects at this level (after delimiter collapsing, if delimiter was provided)
    pub objects: Vec<(String, FileMetadata)>,
    /// CommonPrefixes produced by delimiter collapsing (empty if no delimiter)
    pub common_prefixes: Vec<String>,
    pub is_truncated: bool,
    pub next_continuation_token: Option<String>,
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
            other => crate::api::S3Error::InternalError(other.to_string()),
        }
    }
}

/// Main DeltaGlider engine - generic over storage backend
pub struct DeltaGliderEngine<S: StorageBackend> {
    storage: Arc<S>,
    codec: Arc<DeltaCodec>,
    file_router: FileRouter,
    cache: ReferenceCache,
    max_delta_ratio: f32,
    max_object_size: u64,
    /// Limits concurrent xdelta3 subprocesses (configurable via `codec_concurrency`).
    codec_semaphore: Arc<Semaphore>,
    /// Per-deltaspace locks preventing concurrent reference overwrites.
    /// Uses DashMap for lock-free shard-level lookups (different prefixes never contend).
    prefix_locks: DashMap<String, Arc<tokio::sync::Mutex<()>>>,
    /// Optional Prometheus metrics (None in tests).
    metrics: Option<Arc<Metrics>>,
    /// In-memory cache for object metadata (eliminates HEAD requests).
    metadata_cache: MetadataCache,
}

/// Type alias for engine with dynamic backend dispatch
pub type DynEngine = DeltaGliderEngine<Box<dyn StorageBackend>>;

impl DynEngine {
    /// Create a new engine with the appropriate backend based on configuration.
    /// Pass `metrics` to enable Prometheus instrumentation (None disables it).
    pub async fn new(config: &Config, metrics: Option<Arc<Metrics>>) -> Result<Self, StorageError> {
        let storage: Box<dyn StorageBackend> = match &config.backend {
            BackendConfig::Filesystem { path } => {
                Box::new(FilesystemBackend::new(path.clone()).await?)
            }
            BackendConfig::S3 { .. } => Box::new(S3Backend::new(&config.backend).await?),
        };

        Ok(Self::new_with_backend(Arc::new(storage), config, metrics))
    }
}

impl<S: StorageBackend> DeltaGliderEngine<S> {
    const INTERNAL_REFERENCE_NAME: &'static str = "__reference__";

    /// Access the underlying storage backend (for operations that bypass the delta engine)
    pub fn storage(&self) -> &S {
        &self.storage
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
        Self {
            storage,
            codec: Arc::new(DeltaCodec::new(config.max_object_size as usize)),
            file_router: FileRouter::new(),
            cache: ReferenceCache::new(config.cache_size_mb),
            max_delta_ratio: config.max_delta_ratio,
            max_object_size: config.max_object_size,
            codec_semaphore: Arc::new(Semaphore::new(codec_concurrency)),
            prefix_locks: DashMap::new(),
            metrics,
            metadata_cache: MetadataCache::new((config.metadata_cache_mb as u64) * 1024 * 1024),
        }
    }

    /// Return a reference to the metadata cache (for handler-level access).
    pub fn metadata_cache(&self) -> &MetadataCache {
        &self.metadata_cache
    }

    /// Returns whether the xdelta3 CLI binary is available for legacy delta decoding.
    pub fn is_cli_available(&self) -> bool {
        self.codec.is_cli_available()
    }

    /// Returns the maximum object size in bytes.
    pub fn max_object_size(&self) -> u64 {
        self.max_object_size
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

    /// Run a closure with the metrics if enabled (no-op in tests).
    #[inline]
    fn with_metrics(&self, f: impl FnOnce(&Metrics)) {
        if let Some(m) = &self.metrics {
            f(m);
        }
    }

    /// Build the cache key for a deltaspace's reference.
    fn cache_key(bucket: &str, deltaspace_id: &str) -> String {
        format!("{}/{}", bucket, deltaspace_id)
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
    async fn acquire_prefix_lock(&self, prefix: &str) -> tokio::sync::OwnedMutexGuard<()> {
        // Periodic cleanup on every lock acquisition (cheap — just checks len())
        self.cleanup_prefix_locks();
        let mutex = self
            .prefix_locks
            .entry(prefix.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        mutex.lock_owned().await
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

    /// Parse and validate an S3 key, returning the parsed key and deltaspace ID.
    fn validated_key(bucket: &str, key: &str) -> Result<(ObjectKey, String), EngineError> {
        let obj_key = ObjectKey::parse(bucket, key);
        obj_key
            .validate_object()
            .map_err(|e| EngineError::InvalidArgument(e.to_string()))?;
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

    pub async fn head(&self, bucket: &str, key: &str) -> Result<FileMetadata, EngineError> {
        // Note: we do NOT use the metadata cache for HEAD. The cache is used for
        // LIST enrichment and file_size correction, but HEAD must always verify
        // the object exists on storage to handle out-of-band deletions correctly.
        // The cost is one storage call per HEAD, but HEAD is already a storage call.

        let (obj_key, deltaspace_id) = Self::validated_key(bucket, key)?;

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

        // Fast path: delegate delimiter collapsing to the storage backend (S3
        // handles this natively, avoiding the need to fetch every object).
        let mut page = if let Some(delim) = delimiter {
            if let Some(result) = self
                .storage
                .list_objects_delegated(bucket, prefix, delim, max_keys, continuation_token)
                .await?
            {
                ListObjectsPage {
                    objects: result.objects,
                    common_prefixes: result.common_prefixes,
                    is_truncated: result.is_truncated,
                    next_continuation_token: result.next_continuation_token,
                }
            } else {
                // Backend doesn't support delegated listing — fall through to
                // the generic bulk_list + in-memory collapsing path.
                self.list_objects_bulk(bucket, prefix, Some(delim), max_keys, continuation_token)
                    .await?
            }
        } else {
            self.list_objects_bulk(bucket, prefix, None, max_keys, continuation_token)
                .await?
        };

        // Even without metadata=true, use the metadata cache to correct
        // file_size for delta objects. The lite LIST returns delta (stored) size,
        // but if we have the original size cached from a previous HEAD/PUT,
        // use it for a more accurate LIST response. No extra I/O — just cache lookups.
        if !metadata && !page.objects.is_empty() {
            for (key, meta) in &mut page.objects {
                if let Some(cached) = self.metadata_cache.get(bucket, key) {
                    // Replace file_size with the cached original size
                    meta.file_size = cached.file_size;
                }
            }
        }

        // When metadata=true (MinIO extension), enrich objects with full
        // metadata from HEAD calls. Use the metadata cache to avoid HEAD
        // for objects we already know about — the biggest performance win
        // (1000 objects → 1000 cache lookups instead of 1000 HEADs).
        if metadata && !page.objects.is_empty() {
            let mut cache_hits = Vec::new();
            let mut cache_misses = Vec::new();

            for (key, meta) in page.objects {
                if let Some(cached) = self.metadata_cache.get(bucket, &key) {
                    cache_hits.push((key, cached));
                } else {
                    cache_misses.push((key, meta));
                }
            }

            if !cache_misses.is_empty() {
                let enriched = self
                    .storage
                    .enrich_list_metadata(bucket, cache_misses)
                    .await?;
                // Cache the newly enriched metadata
                for (key, meta) in &enriched {
                    self.metadata_cache.insert(bucket, key, meta.clone());
                }
                cache_hits.extend(enriched);
            }

            // Re-sort by key to maintain S3 lexicographic ordering
            cache_hits.sort_by(|a, b| a.0.cmp(&b.0));
            page.objects = cache_hits;
        }

        Ok(page)
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
        // Single-pass listing: replaces list_deltaspaces + scan_deltaspace×N + list_directory_markers
        let bulk = self.storage.bulk_list_objects(bucket, prefix).await?;

        // Dedup by key, keeping latest version
        let mut latest: std::collections::HashMap<String, FileMetadata> =
            std::collections::HashMap::new();
        for (key, meta) in bulk {
            match latest.entry(key) {
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(meta);
                }
                std::collections::hash_map::Entry::Occupied(mut e) => {
                    if meta.created_at > e.get().created_at {
                        e.insert(meta);
                    }
                }
            }
        }

        let mut items: Vec<(String, FileMetadata)> = latest.into_iter().collect();

        if !prefix.is_empty() {
            items.retain(|(key, _meta)| key.starts_with(prefix));
        }

        items.sort_by(|a, b| a.0.cmp(&b.0));

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

    /// Check if a real bucket exists on the storage backend.
    pub async fn head_bucket(&self, bucket: &str) -> Result<bool, EngineError> {
        Ok(self.storage.head_bucket(bucket).await?)
    }

    /// Delete an object
    #[instrument(skip(self))]
    pub async fn delete(&self, bucket: &str, key: &str) -> Result<(), EngineError> {
        let (obj_key, deltaspace_id) = Self::validated_key(bucket, key)?;

        info!("Deleting {}/{}", bucket, key);

        // Acquire per-deltaspace lock to prevent races with concurrent store/delete
        // operations that may create or clean up the reference.
        let _guard = self.acquire_prefix_lock(&deltaspace_id).await;

        // Use resolve_metadata (no migration) — we already hold the prefix lock, and
        // tokio::sync::Mutex is not reentrant, so calling resolve_metadata_with_migration
        // here would deadlock. Legacy objects that haven't been migrated yet will appear
        // as NotFound; a prior GET/HEAD on the key will have triggered migration.
        let metadata = self
            .resolve_metadata(bucket, &deltaspace_id, &obj_key)
            .await?
            .ok_or_else(|| EngineError::NotFound(obj_key.full_key()))?;

        // Delete based on storage type
        match &metadata.storage_info {
            StorageInfo::Passthrough => {
                self.storage
                    .delete_passthrough(bucket, &deltaspace_id, &obj_key.filename)
                    .await?;
            }
            StorageInfo::Delta { .. } => {
                self.storage
                    .delete_delta(bucket, &deltaspace_id, &obj_key.filename)
                    .await?;
            }
            StorageInfo::Reference { .. } => {
                return Err(EngineError::InvalidArgument(
                    "Reference objects are internal and cannot be deleted directly".to_string(),
                ));
            }
        }

        // If this deltaspace no longer has any objects, clean up its reference baseline.
        let remaining = self.storage.scan_deltaspace(bucket, &deltaspace_id).await?;
        let has_objects = remaining
            .iter()
            .any(|m| !matches!(m.storage_info, StorageInfo::Reference { .. }));
        if !has_objects && self.storage.has_reference(bucket, &deltaspace_id).await {
            // Delete storage BEFORE invalidating cache — prevents stale cache entries
            // from a concurrent GET loading between invalidation and deletion.
            self.storage
                .delete_reference(bucket, &deltaspace_id)
                .await?;
            let cache_key = Self::cache_key(bucket, &deltaspace_id);
            self.cache.invalidate(&cache_key);
        }

        // Invalidate metadata cache for the deleted key
        self.metadata_cache.invalidate(bucket, key);

        // Release the per-prefix lock before cleanup so strong_count drops to 1.
        drop(_guard);
        self.cleanup_prefix_locks();

        debug!("Deleted {}/{}", bucket, key);
        Ok(())
    }

    /// Delete all objects under a prefix (server-side recursive delete).
    /// Tries the storage backend's native delete first (filesystem = rm -rf).
    /// Falls back to list + individual delete for S3.
    pub async fn delete_prefix(&self, bucket: &str, prefix: &str) -> Result<u32, EngineError> {
        info!("Recursive delete: {}/{}*", bucket, prefix);

        // Try native backend delete (filesystem = rm -rf, instant)
        if self.storage.delete_prefix(bucket, prefix).await? {
            self.metadata_cache.invalidate_prefix(bucket, prefix);
            info!("Recursive delete (native): {}/{}*", bucket, prefix);
            return Ok(u32::MAX);
        }

        // Fallback: list + delete one by one (S3 backend)
        let page = self
            .list_objects(bucket, prefix, None, u32::MAX, None, false)
            .await?;

        let mut deleted = 0u32;
        for (key, _meta) in &page.objects {
            match self.delete(bucket, key).await {
                Ok(()) => deleted += 1,
                Err(EngineError::NotFound(_)) => deleted += 1,
                Err(e) => {
                    warn!("Failed to delete {}/{}: {}", bucket, key, e);
                }
            }
        }

        info!(
            "Recursive delete complete: {}/{} — {} objects deleted",
            bucket, prefix, deleted
        );
        Ok(deleted)
    }

    /// Get reference with caching. Returns `Bytes` for zero-copy sharing.
    /// Returns `(reference_data, cache_hit)`.
    async fn get_reference_cached(
        &self,
        bucket: &str,
        deltaspace_id: &str,
    ) -> Result<(bytes::Bytes, bool), EngineError> {
        let cache_key = Self::cache_key(bucket, deltaspace_id);

        // Check cache first (Bytes clone is a cheap refcount increment)
        if let Some(data) = self.cache.get(&cache_key) {
            self.with_metrics(|m| m.cache_hits_total.inc());
            return Ok((data, true));
        }

        self.with_metrics(|m| m.cache_misses_total.inc());

        // Load from storage
        let data = self
            .storage
            .get_reference(bucket, deltaspace_id)
            .await
            .map_err(|e| match e {
                StorageError::NotFound(_) => {
                    EngineError::MissingReference(deltaspace_id.to_string())
                }
                other => EngineError::Storage(other),
            })?;

        // PERF: Convert Vec→Bytes once (zero-copy ownership transfer), then
        // clone the Bytes for the cache (refcount increment, no memcpy).
        // The old code did data.clone() (full 80MB memcpy) + Bytes::from — this
        // saves one memcpy per cache miss.
        let bytes = Bytes::from(data);
        self.cache.put(&cache_key, bytes.clone());

        Ok((bytes, false))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_local_prefix_could_match() {
        // Empty prefix matches everything
        assert!(
            DeltaGliderEngine::<FilesystemBackend>::local_prefix_could_match("releases/v1.0", "")
        );
        assert!(DeltaGliderEngine::<FilesystemBackend>::local_prefix_could_match("", ""));

        // Prefix drills into a deltaspace
        assert!(
            DeltaGliderEngine::<FilesystemBackend>::local_prefix_could_match(
                "releases/v1.0",
                "releases/v1.0/"
            )
        );
        assert!(
            DeltaGliderEngine::<FilesystemBackend>::local_prefix_could_match(
                "releases/v1.0",
                "releases/v1.0/app"
            )
        );

        // Prefix is broader than deltaspace
        assert!(
            DeltaGliderEngine::<FilesystemBackend>::local_prefix_could_match(
                "releases/v1.0",
                "releases/"
            )
        );
        assert!(
            DeltaGliderEngine::<FilesystemBackend>::local_prefix_could_match(
                "releases/v1.0",
                "rel"
            )
        );

        // No match — disjoint paths
        assert!(
            !DeltaGliderEngine::<FilesystemBackend>::local_prefix_could_match(
                "releases/v1.0",
                "backups/"
            )
        );
        assert!(
            !DeltaGliderEngine::<FilesystemBackend>::local_prefix_could_match(
                "releases/v1.0",
                "staging/"
            )
        );

        // Root local prefix (empty) — matches only prefixes without '/'
        assert!(DeltaGliderEngine::<FilesystemBackend>::local_prefix_could_match("", "app"));
        assert!(!DeltaGliderEngine::<FilesystemBackend>::local_prefix_could_match("", "releases/"));
    }
}
