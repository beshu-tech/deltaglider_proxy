//! DeltaGlider engine - main orchestrator for delta-based storage

use super::cache::ReferenceCache;
use super::codec::{CodecError, DeltaCodec};
use super::file_router::FileRouter;
use crate::config::{BackendConfig, Config};
use crate::storage::{FilesystemBackend, S3Backend, StorageBackend, StorageError};
use crate::types::{FileMetadata, ObjectKey, StorageInfo, StoreResult};
use bytes::Bytes;
use futures::stream::BoxStream;
use md5::{Digest as Md5Digest, Md5};
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::Semaphore;
use tracing::{debug, info, instrument, warn};

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
}

#[derive(Debug, Clone)]
pub struct ListObjectsV2Page {
    pub objects: Vec<(String, FileMetadata)>,
    pub is_truncated: bool,
    pub next_continuation_token: Option<String>,
}

/// Response from `retrieve_stream()` — either a streaming or buffered response.
pub enum RetrieveResponse {
    /// Direct file streamed from backend (zero-copy, constant memory).
    Streamed {
        stream: BoxStream<'static, Result<Bytes, StorageError>>,
        metadata: FileMetadata,
    },
    /// Delta-reconstructed file buffered in memory.
    Buffered {
        data: Vec<u8>,
        metadata: FileMetadata,
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
            EngineError::Storage(e) => e.into(),
            other => crate::api::S3Error::InternalError(other.to_string()),
        }
    }
}

/// Main DeltaGlider engine - generic over storage backend
pub struct DeltaGliderEngine<S: StorageBackend> {
    storage: Arc<S>,
    codec: DeltaCodec,
    file_router: FileRouter,
    cache: ReferenceCache,
    max_delta_ratio: f32,
    max_object_size: u64,
    /// Whether to verify SHA256 checksums on read (GET).
    verify_on_read: bool,
    /// Limits concurrent delta encode/decode operations to prevent CPU saturation.
    codec_semaphore: Arc<Semaphore>,
    /// Per-deltaspace locks to prevent concurrent reference overwrites.
    /// Outer parking_lot::Mutex for fast synchronous map access;
    /// inner tokio::sync::Mutex for async-compatible per-prefix locking.
    prefix_locks: parking_lot::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

/// Type alias for engine with dynamic backend dispatch
pub type DynEngine = DeltaGliderEngine<Box<dyn StorageBackend>>;

impl DynEngine {
    /// Create a new engine with the appropriate backend based on configuration
    pub async fn new(config: &Config) -> Result<Self, StorageError> {
        let storage: Box<dyn StorageBackend> = match &config.backend {
            BackendConfig::Filesystem { path } => {
                Box::new(FilesystemBackend::new(path.clone()).await?)
            }
            BackendConfig::S3 { .. } => Box::new(S3Backend::new(&config.backend).await?),
        };

        Ok(Self::new_with_backend(Arc::new(storage), config))
    }
}

impl<S: StorageBackend> DeltaGliderEngine<S> {
    const INTERNAL_REFERENCE_NAME: &'static str = "__reference__";

    /// Create a new engine with a custom storage backend
    pub fn new_with_backend(storage: Arc<S>, config: &Config) -> Self {
        Self {
            storage,
            codec: DeltaCodec::new(config.max_object_size as usize),
            file_router: FileRouter::new(),
            cache: ReferenceCache::new(config.cache_size_mb),
            max_delta_ratio: config.max_delta_ratio,
            max_object_size: config.max_object_size,
            verify_on_read: config.verify_on_read,
            codec_semaphore: Arc::new(Semaphore::new(
                std::thread::available_parallelism()
                    .map(|n| n.get())
                    .unwrap_or(4),
            )),
            prefix_locks: parking_lot::Mutex::new(HashMap::new()),
        }
    }

    /// Returns whether the xdelta3 CLI binary is available for legacy delta decoding.
    pub fn is_cli_available(&self) -> bool {
        self.codec.is_cli_available()
    }

    /// Returns the maximum object size in bytes.
    pub fn max_object_size(&self) -> u64 {
        self.max_object_size
    }

    /// Acquire a per-deltaspace async lock. Different prefixes do not contend.
    async fn acquire_prefix_lock(&self, prefix: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let mutex = {
            let mut locks = self.prefix_locks.lock();
            locks
                .entry(prefix.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
        };
        mutex.lock_owned().await
    }

    /// Prune prefix lock entries that are no longer actively held.
    /// An entry with `Arc::strong_count() == 1` means only the map references it
    /// (no outstanding `OwnedMutexGuard`), so it can be safely removed.
    /// Only runs when the map exceeds a size threshold to avoid overhead.
    fn cleanup_prefix_locks(&self) {
        const CLEANUP_THRESHOLD: usize = 1024;
        let mut locks = self.prefix_locks.lock();
        if locks.len() <= CLEANUP_THRESHOLD {
            return;
        }
        let before = locks.len();
        locks.retain(|_, arc| Arc::strong_count(arc) > 1);
        let removed = before - locks.len();
        if removed > 0 {
            debug!(
                "Pruned {} idle prefix locks ({} remaining)",
                removed,
                locks.len()
            );
        }
    }

    /// Look up object metadata by checking both delta and direct storage,
    /// returning the most recent version if both exist.
    async fn resolve_object_metadata(
        &self,
        bucket: &str,
        prefix: &str,
        original_name: &str,
    ) -> Result<Option<FileMetadata>, StorageError> {
        let filename = original_name.rsplit('/').next().unwrap_or(original_name);
        let delta = self
            .storage
            .get_delta_metadata(bucket, prefix, filename)
            .await
            .ok();
        let direct = self
            .storage
            .get_direct_metadata(bucket, prefix, filename)
            .await
            .ok();
        match (delta, direct) {
            (Some(d), Some(di)) => Ok(Some(if d.created_at >= di.created_at { d } else { di })),
            (Some(meta), None) | (None, Some(meta)) => Ok(Some(meta)),
            (None, None) => Ok(None),
        }
    }

    /// Resolve metadata with legacy migration fallback.
    /// Tries a direct lookup first; if not found, attempts to migrate a legacy
    /// reference object, then retries the lookup.
    async fn resolve_metadata_with_migration(
        &self,
        bucket: &str,
        deltaspace_id: &str,
        obj_key: &ObjectKey,
    ) -> Result<Option<FileMetadata>, EngineError> {
        let mut metadata = self
            .resolve_object_metadata(bucket, deltaspace_id, &obj_key.full_key())
            .await?;
        if metadata.is_none()
            && self
                .migrate_legacy_reference_object_if_needed(bucket, deltaspace_id, &obj_key.filename)
                .await?
        {
            metadata = self
                .resolve_object_metadata(bucket, deltaspace_id, &obj_key.full_key())
                .await?;
        }
        Ok(metadata)
    }

    /// Store an object with automatic delta compression
    #[instrument(skip(self, data, user_metadata))]
    pub async fn store(
        &self,
        bucket: &str,
        key: &str,
        data: &[u8],
        content_type: Option<String>,
        user_metadata: std::collections::HashMap<String, String>,
    ) -> Result<StoreResult, EngineError> {
        // Check size limit
        if data.len() as u64 > self.max_object_size {
            return Err(EngineError::TooLarge {
                size: data.len() as u64,
                max: self.max_object_size,
            });
        }

        let obj_key = ObjectKey::parse(bucket, key);
        obj_key
            .validate_object()
            .map_err(|e| EngineError::InvalidArgument(e.to_string()))?;
        let deltaspace_id = obj_key.deltaspace_id();

        // Calculate hashes
        let sha256 = hex::encode(Sha256::digest(data));
        let md5 = hex::encode(Md5::digest(data));

        info!(
            "Storing {}/{} ({} bytes, sha256={})",
            bucket,
            key,
            data.len(),
            &sha256[..8]
        );

        // Check if file type is eligible for delta compression
        if !self.file_router.is_delta_eligible(&obj_key.filename) {
            debug!("File type not delta-eligible, storing directly");
            let _lock = self.acquire_prefix_lock(&deltaspace_id).await;
            self.delete_delta_if_exists(bucket, &deltaspace_id, &obj_key.filename)
                .await?;
            return self
                .store_direct(
                    bucket,
                    &obj_key,
                    &deltaspace_id,
                    data,
                    sha256,
                    md5,
                    content_type,
                    user_metadata,
                )
                .await;
        }

        // Acquire per-deltaspace lock to prevent concurrent reference overwrites.
        // The critical section: has_reference check → set_reference → store_delta
        // must be atomic per-prefix to avoid two writers both creating a reference.
        let _lock = self.acquire_prefix_lock(&deltaspace_id).await;

        // Check if deltaspace already has a reference (existing deltaspace)
        let has_existing_reference = self.storage.has_reference(bucket, &deltaspace_id).await;

        // Ensure deltaspace has an internal reference baseline.
        let ref_meta = if has_existing_reference {
            self.storage
                .get_reference_metadata(bucket, &deltaspace_id)
                .await?
        } else {
            debug!("No reference in deltaspace, creating baseline");
            self.set_reference_baseline(
                bucket,
                &obj_key,
                &deltaspace_id,
                data,
                &sha256,
                &md5,
                content_type.clone(),
            )
            .await?
        };

        // Try to compute delta (bounded by semaphore to prevent CPU saturation)
        let reference = self.get_reference_cached(bucket, &deltaspace_id).await?;
        let _codec_permit = self.codec_semaphore.acquire().await.map_err(|_| {
            EngineError::Storage(StorageError::Other("codec semaphore closed".into()))
        })?;
        let delta = self.codec.encode(&reference, data)?;
        drop(_codec_permit);
        let ratio = DeltaCodec::compression_ratio(data.len(), delta.len());

        info!(
            "Delta computed: {} bytes -> {} bytes (ratio: {:.2}%)",
            data.len(),
            delta.len(),
            ratio * 100.0
        );

        // Only apply the threshold when NO reference exists yet (first file in deltaspace).
        // Once a reference exists, ALWAYS store as delta - the deltaspace is committed to
        // delta storage and we want all related files to benefit from the shared reference.
        if !has_existing_reference && ratio >= self.max_delta_ratio {
            debug!(
                "First file in deltaspace with poor delta ratio {:.2} >= {:.2}, storing directly",
                ratio, self.max_delta_ratio
            );
            // Clean up the reference we just created since we're not using it
            let cache_key = format!("{}/{}", bucket, deltaspace_id);
            self.cache.invalidate(&cache_key);
            self.storage
                .delete_reference(bucket, &deltaspace_id)
                .await?;
            self.delete_delta_if_exists(bucket, &deltaspace_id, &obj_key.filename)
                .await?;
            return self
                .store_direct(
                    bucket,
                    &obj_key,
                    &deltaspace_id,
                    data,
                    sha256,
                    md5,
                    content_type,
                    user_metadata,
                )
                .await;
        }

        // Store as delta
        let mut metadata = FileMetadata::new_delta(
            obj_key.filename.clone(),
            sha256,
            md5,
            data.len() as u64,
            format!("{}/reference.bin", deltaspace_id),
            ref_meta.file_sha256,
            delta.len() as u64,
            content_type,
        );
        metadata.user_metadata = user_metadata;

        self.delete_direct_if_exists(bucket, &deltaspace_id, &obj_key.filename)
            .await?;
        self.storage
            .put_delta(bucket, &deltaspace_id, &obj_key.filename, &delta, &metadata)
            .await?;

        Ok(StoreResult {
            metadata,
            stored_size: delta.len() as u64,
        })
    }

    /// Store the internal deltaspace reference baseline.
    #[allow(clippy::too_many_arguments)]
    async fn set_reference_baseline(
        &self,
        bucket: &str,
        obj_key: &ObjectKey,
        deltaspace_id: &str,
        data: &[u8],
        sha256: &str,
        md5: &str,
        content_type: Option<String>,
    ) -> Result<FileMetadata, EngineError> {
        let metadata = FileMetadata::new_reference(
            Self::INTERNAL_REFERENCE_NAME.to_string(),
            obj_key.full_key(),
            sha256.to_string(),
            md5.to_string(),
            data.len() as u64,
            content_type,
        );

        self.storage
            .put_reference(bucket, deltaspace_id, data, &metadata)
            .await?;

        // Cache the reference (use bucket/prefix as cache key for uniqueness)
        let cache_key = format!("{}/{}", bucket, deltaspace_id);
        self.cache.put(&cache_key, data.to_vec());

        Ok(metadata)
    }

    /// Check if a key's filename is eligible for delta compression.
    pub fn is_delta_eligible(&self, key: &str) -> bool {
        let obj_key = ObjectKey::parse("_", key);
        self.file_router.is_delta_eligible(&obj_key.filename)
    }

    /// Store a non-delta-eligible object from pre-split chunks without assembling
    /// into a contiguous buffer. Computes SHA256 and MD5 incrementally.
    #[instrument(skip(self, chunks, user_metadata))]
    pub async fn store_direct_chunked(
        &self,
        bucket: &str,
        key: &str,
        chunks: &[Bytes],
        total_size: u64,
        content_type: Option<String>,
        user_metadata: HashMap<String, String>,
    ) -> Result<StoreResult, EngineError> {
        if total_size > self.max_object_size {
            return Err(EngineError::TooLarge {
                size: total_size,
                max: self.max_object_size,
            });
        }

        let obj_key = ObjectKey::parse(bucket, key);
        obj_key
            .validate_object()
            .map_err(|e| EngineError::InvalidArgument(e.to_string()))?;
        let deltaspace_id = obj_key.deltaspace_id();

        // Compute SHA256 + MD5 incrementally across chunks
        let mut sha256_hasher = Sha256::new();
        let mut md5_hasher = Md5::new();
        for chunk in chunks {
            sha256_hasher.update(chunk);
            md5_hasher.update(chunk);
        }
        let sha256 = hex::encode(sha256_hasher.finalize());
        let md5 = hex::encode(md5_hasher.finalize());

        info!(
            "Storing chunked {}/{} ({} bytes, {} chunks, sha256={})",
            bucket,
            key,
            total_size,
            chunks.len(),
            &sha256[..8]
        );

        let _lock = self.acquire_prefix_lock(&deltaspace_id).await;
        self.delete_delta_if_exists(bucket, &deltaspace_id, &obj_key.filename)
            .await?;

        let mut metadata = FileMetadata::new_direct(
            obj_key.filename.clone(),
            sha256,
            md5,
            total_size,
            content_type,
        );
        metadata.user_metadata = user_metadata;

        self.storage
            .put_direct_chunked(bucket, &deltaspace_id, &obj_key.filename, chunks, &metadata)
            .await?;

        Ok(StoreResult {
            metadata,
            stored_size: total_size,
        })
    }

    /// Store directly without delta compression
    #[allow(clippy::too_many_arguments)]
    async fn store_direct(
        &self,
        bucket: &str,
        obj_key: &ObjectKey,
        deltaspace_id: &str,
        data: &[u8],
        sha256: String,
        md5: String,
        content_type: Option<String>,
        user_metadata: HashMap<String, String>,
    ) -> Result<StoreResult, EngineError> {
        let mut metadata = FileMetadata::new_direct(
            obj_key.filename.clone(),
            sha256,
            md5,
            data.len() as u64,
            content_type,
        );
        metadata.user_metadata = user_metadata;

        self.storage
            .put_direct(bucket, deltaspace_id, &obj_key.filename, data, &metadata)
            .await?;

        Ok(StoreResult {
            metadata,
            stored_size: data.len() as u64,
        })
    }

    /// Delete a storage object, ignoring NotFound errors (idempotent delete).
    async fn delete_ignoring_not_found(
        result: Result<(), StorageError>,
    ) -> Result<(), EngineError> {
        match result {
            Ok(()) | Err(StorageError::NotFound(_)) => Ok(()),
            Err(other) => Err(other.into()),
        }
    }

    async fn delete_delta_if_exists(
        &self,
        bucket: &str,
        deltaspace_id: &str,
        filename: &str,
    ) -> Result<(), EngineError> {
        Self::delete_ignoring_not_found(
            self.storage
                .delete_delta(bucket, deltaspace_id, filename)
                .await,
        )
        .await
    }

    async fn delete_direct_if_exists(
        &self,
        bucket: &str,
        deltaspace_id: &str,
        filename: &str,
    ) -> Result<(), EngineError> {
        Self::delete_ignoring_not_found(
            self.storage
                .delete_direct(bucket, deltaspace_id, filename)
                .await,
        )
        .await
    }

    async fn migrate_legacy_reference_object_if_needed(
        &self,
        bucket: &str,
        deltaspace_id: &str,
        filename: &str,
    ) -> Result<bool, EngineError> {
        if !self.storage.has_reference(bucket, deltaspace_id).await {
            return Ok(false);
        }

        let mut ref_meta = self
            .storage
            .get_reference_metadata(bucket, deltaspace_id)
            .await?;
        if ref_meta.original_name == Self::INTERNAL_REFERENCE_NAME {
            return Ok(false);
        }
        if ref_meta.original_name != filename {
            return Ok(false);
        }

        let reference = self.get_reference_cached(bucket, deltaspace_id).await?;
        let _codec_permit = self.codec_semaphore.acquire().await.map_err(|_| {
            EngineError::Storage(StorageError::Other("codec semaphore closed".into()))
        })?;
        let delta = self.codec.encode(&reference, &reference)?;
        drop(_codec_permit);

        let delta_meta = FileMetadata::new_delta(
            filename.to_string(),
            ref_meta.file_sha256.clone(),
            ref_meta.md5.clone(),
            ref_meta.file_size,
            format!("{}/reference.bin", deltaspace_id),
            ref_meta.file_sha256.clone(),
            delta.len() as u64,
            ref_meta.content_type.clone(),
        );

        self.delete_direct_if_exists(bucket, deltaspace_id, filename)
            .await?;
        self.storage
            .put_delta(bucket, deltaspace_id, filename, &delta, &delta_meta)
            .await?;

        ref_meta.original_name = Self::INTERNAL_REFERENCE_NAME.to_string();
        self.storage
            .put_reference_metadata(bucket, deltaspace_id, &ref_meta)
            .await?;

        Ok(true)
    }

    /// Retrieve an object fully buffered, reconstructing from delta if necessary.
    ///
    /// For callers that need the full data in memory (e.g. copy_object).
    /// Delegates to `retrieve_stream()` and collects any streamed response.
    #[instrument(skip(self))]
    pub async fn retrieve(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<(Vec<u8>, FileMetadata), EngineError> {
        use futures::TryStreamExt;

        match self.retrieve_stream(bucket, key).await? {
            RetrieveResponse::Buffered { data, metadata } => Ok((data, metadata)),
            RetrieveResponse::Streamed { stream, metadata } => {
                let chunks: Vec<Bytes> = stream.map_err(EngineError::Storage).try_collect().await?;
                let data: Vec<u8> = chunks.into_iter().flat_map(|b| b.to_vec()).collect();
                Ok((data, metadata))
            }
        }
    }

    /// Retrieve an object with streaming support for direct files.
    ///
    /// Direct files are streamed from the backend without buffering (constant memory).
    /// Delta/reference files are reconstructed in memory (buffering required by xdelta3).
    #[instrument(skip(self))]
    pub async fn retrieve_stream(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<RetrieveResponse, EngineError> {
        let obj_key = ObjectKey::parse(bucket, key);
        obj_key
            .validate_object()
            .map_err(|e| EngineError::InvalidArgument(e.to_string()))?;
        let deltaspace_id = obj_key.deltaspace_id();

        let metadata = self
            .resolve_metadata_with_migration(bucket, &deltaspace_id, &obj_key)
            .await?
            .ok_or_else(|| EngineError::NotFound(obj_key.full_key()))?;

        info!(
            "Retrieving {}/{} (stored as {})",
            bucket,
            key,
            metadata.storage_info.label()
        );

        match &metadata.storage_info {
            StorageInfo::Direct => {
                // Stream directly from backend — no buffering needed
                let stream = self
                    .storage
                    .get_direct_stream(bucket, &deltaspace_id, &obj_key.filename)
                    .await?;
                debug!("Streaming direct file for {}", obj_key.full_key());
                Ok(RetrieveResponse::Streamed { stream, metadata })
            }
            StorageInfo::Reference { .. } | StorageInfo::Delta { .. } => {
                let data = self
                    .retrieve_buffered(bucket, &deltaspace_id, &obj_key, &metadata)
                    .await?;
                debug!(
                    "Retrieved (buffered) {} bytes for {}",
                    data.len(),
                    obj_key.full_key()
                );
                Ok(RetrieveResponse::Buffered { data, metadata })
            }
        }
    }

    /// Fetch and reconstruct a reference or delta object, with checksum verification.
    async fn retrieve_buffered(
        &self,
        bucket: &str,
        deltaspace_id: &str,
        obj_key: &ObjectKey,
        metadata: &FileMetadata,
    ) -> Result<Vec<u8>, EngineError> {
        let data = match &metadata.storage_info {
            StorageInfo::Reference { .. } => {
                self.storage.get_reference(bucket, deltaspace_id).await?
            }
            StorageInfo::Delta { .. } => {
                let reference = self.get_reference_cached(bucket, deltaspace_id).await?;
                let delta = self
                    .storage
                    .get_delta(bucket, deltaspace_id, &obj_key.filename)
                    .await?;
                let _codec_permit = self.codec_semaphore.acquire().await.map_err(|_| {
                    EngineError::Storage(StorageError::Other("codec semaphore closed".into()))
                })?;
                let result = self.codec.decode(&reference, &delta)?;
                drop(_codec_permit);
                result
            }
            StorageInfo::Direct => {
                // Should not reach here — callers route Direct to streaming path
                self.storage
                    .get_direct(bucket, deltaspace_id, &obj_key.filename)
                    .await?
            }
        };

        // Verify checksum (configurable — disable for throughput when storage is trusted)
        if self.verify_on_read {
            let actual_sha256 = hex::encode(Sha256::digest(&data));
            if actual_sha256 != metadata.file_sha256 {
                warn!(
                    "Checksum mismatch for {}: expected {}, got {}",
                    obj_key.full_key(),
                    metadata.file_sha256,
                    actual_sha256
                );
                return Err(EngineError::ChecksumMismatch {
                    key: obj_key.full_key(),
                    expected: metadata.file_sha256.clone(),
                    actual: actual_sha256,
                });
            }
        }

        Ok(data)
    }

    /// Retrieve object metadata without reading object bodies.
    #[instrument(skip(self))]
    pub async fn head(&self, bucket: &str, key: &str) -> Result<FileMetadata, EngineError> {
        let obj_key = ObjectKey::parse(bucket, key);
        obj_key
            .validate_object()
            .map_err(|e| EngineError::InvalidArgument(e.to_string()))?;
        let deltaspace_id = obj_key.deltaspace_id();

        self.resolve_metadata_with_migration(bucket, &deltaspace_id, &obj_key)
            .await?
            .ok_or_else(|| EngineError::NotFound(obj_key.full_key()))
    }

    /// Returns `true` if a local prefix (bucket-relative) could contain keys
    /// matching the given user prefix.
    /// Used to skip entire deltaspaces during listing, avoiding unnecessary I/O.
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

    /// ListObjectsV2-style listing scoped to a real bucket.
    #[instrument(skip(self))]
    pub async fn list_objects_v2(
        &self,
        bucket: &str,
        prefix: &str,
        max_keys: u32,
        continuation_token: Option<&str>,
    ) -> Result<ListObjectsV2Page, EngineError> {
        ObjectKey::validate_prefix(prefix)
            .map_err(|e| EngineError::InvalidArgument(e.to_string()))?;
        let deltaspace_ids = self.storage.list_deltaspaces(bucket).await?;
        let mut latest: std::collections::HashMap<String, FileMetadata> =
            std::collections::HashMap::new();

        for local_prefix in deltaspace_ids {
            // Skip deltaspaces that cannot produce keys matching the requested prefix.
            if !Self::local_prefix_could_match(&local_prefix, prefix) {
                continue;
            }

            let metas = self.storage.scan_deltaspace(bucket, &local_prefix).await?;

            for meta in metas {
                if matches!(meta.storage_info, StorageInfo::Reference { .. }) {
                    continue;
                }
                let full_key = if local_prefix.is_empty() {
                    meta.original_name.clone()
                } else {
                    format!("{}/{}", local_prefix, meta.original_name)
                };
                match latest.entry(full_key) {
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        entry.insert(meta);
                    }
                    std::collections::hash_map::Entry::Occupied(mut entry) => {
                        if meta.created_at > entry.get().created_at {
                            entry.insert(meta);
                        }
                    }
                }
            }
        }

        let mut items: Vec<(String, FileMetadata)> = latest.into_iter().collect();

        if !prefix.is_empty() {
            items.retain(|(key, _meta)| key.starts_with(prefix));
        }

        items.sort_by(|a, b| a.0.cmp(&b.0));

        if let Some(token) = continuation_token {
            items.retain(|(key, _meta)| key.as_str() > token);
        }

        let max_keys = max_keys as usize;
        let is_truncated = max_keys < items.len();
        let page = if is_truncated {
            items.truncate(max_keys);
            items
        } else {
            items
        };

        let next_token = if is_truncated {
            page.last().map(|(key, _)| key.clone())
        } else {
            None
        };

        Ok(ListObjectsV2Page {
            objects: page,
            is_truncated,
            next_continuation_token: next_token,
        })
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

    /// Check if a real bucket exists on the storage backend.
    pub async fn head_bucket(&self, bucket: &str) -> Result<bool, EngineError> {
        Ok(self.storage.head_bucket(bucket).await?)
    }

    /// Delete an object
    #[instrument(skip(self))]
    pub async fn delete(&self, bucket: &str, key: &str) -> Result<(), EngineError> {
        let obj_key = ObjectKey::parse(bucket, key);
        obj_key
            .validate_object()
            .map_err(|e| EngineError::InvalidArgument(e.to_string()))?;
        let deltaspace_id = obj_key.deltaspace_id();

        info!("Deleting {}/{}", bucket, key);

        // Acquire per-deltaspace lock to prevent races with concurrent store/delete
        // operations that may create or clean up the reference.
        let _lock = self.acquire_prefix_lock(&deltaspace_id).await;

        let metadata = self
            .resolve_metadata_with_migration(bucket, &deltaspace_id, &obj_key)
            .await?
            .ok_or_else(|| EngineError::NotFound(obj_key.full_key()))?;

        // Delete based on storage type
        match &metadata.storage_info {
            StorageInfo::Direct => {
                self.storage
                    .delete_direct(bucket, &deltaspace_id, &obj_key.filename)
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
            let cache_key = format!("{}/{}", bucket, deltaspace_id);
            self.cache.invalidate(&cache_key);
            self.storage
                .delete_reference(bucket, &deltaspace_id)
                .await?;
        }

        // Release the per-prefix lock before cleanup so strong_count drops to 1.
        drop(_lock);
        self.cleanup_prefix_locks();

        debug!("Deleted {}/{}", bucket, key);
        Ok(())
    }

    /// Get reference with caching. Returns `Bytes` for zero-copy sharing.
    async fn get_reference_cached(
        &self,
        bucket: &str,
        deltaspace_id: &str,
    ) -> Result<bytes::Bytes, EngineError> {
        let cache_key = format!("{}/{}", bucket, deltaspace_id);

        // Check cache first (Bytes clone is a cheap refcount increment)
        if let Some(data) = self.cache.get(&cache_key) {
            return Ok(data);
        }

        // Load from storage
        let data = self
            .storage
            .get_reference(bucket, deltaspace_id)
            .await
            .map_err(|_| EngineError::MissingReference(deltaspace_id.to_string()))?;

        // Cache it (Vec<u8> is moved into Bytes inside the cache)
        self.cache.put(&cache_key, data.clone());

        Ok(data.into())
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
