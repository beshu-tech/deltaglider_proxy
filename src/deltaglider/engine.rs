//! DeltaGlider engine - main orchestrator for delta-based storage

use super::cache::ReferenceCache;
use super::codec::{CodecError, DeltaCodec};
use super::deltaspace::DeltaSpaceManager;
use super::file_router::FileRouter;
use crate::config::{BackendConfig, Config};
use crate::storage::{FilesystemBackend, S3Backend, StorageBackend, StorageError};
use crate::types::{FileMetadata, ObjectKey, StorageInfo, StoreResult};
use md5::{Digest as Md5Digest, Md5};
use sha2::Sha256;
use std::sync::Arc;
use thiserror::Error;
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
    deltaspace_mgr: DeltaSpaceManager<S>,
    codec: DeltaCodec,
    file_router: FileRouter,
    cache: ReferenceCache,
    max_delta_ratio: f32,
    max_object_size: u64,
}

impl DeltaGliderEngine<FilesystemBackend> {
    /// Create a new engine with filesystem backend from configuration
    pub async fn new_filesystem(config: &Config) -> Result<Self, StorageError> {
        let path = match &config.backend {
            BackendConfig::Filesystem { path } => path.clone(),
            _ => {
                return Err(StorageError::Other(
                    "Expected Filesystem backend configuration".to_string(),
                ));
            }
        };

        let storage = Arc::new(FilesystemBackend::new(path).await?);
        Ok(Self::new_with_backend(storage, config))
    }
}

impl DeltaGliderEngine<S3Backend> {
    /// Create a new engine with S3 backend from configuration
    pub async fn new_s3(config: &Config) -> Result<Self, StorageError> {
        let storage = Arc::new(S3Backend::new(&config.backend).await?);
        Ok(Self::new_with_backend(storage, config))
    }
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
            deltaspace_mgr: DeltaSpaceManager::new(storage),
            codec: DeltaCodec::new(config.max_object_size as usize),
            file_router: FileRouter::new(),
            cache: ReferenceCache::new(config.cache_size_mb),
            max_delta_ratio: config.max_delta_ratio,
            max_object_size: config.max_object_size,
        }
    }

    /// Store an object with automatic delta compression
    #[instrument(skip(self, data))]
    pub async fn store(
        &self,
        bucket: &str,
        key: &str,
        data: &[u8],
        content_type: Option<String>,
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
            self.delete_delta_if_exists(&deltaspace_id, &obj_key.filename)
                .await?;
            return self
                .store_direct(&obj_key, &deltaspace_id, data, sha256, md5, content_type)
                .await;
        }

        // Ensure deltaspace has an internal reference baseline.
        let ref_meta = if self.deltaspace_mgr.has_reference(&deltaspace_id).await {
            self.deltaspace_mgr
                .get_reference_metadata(&deltaspace_id)
                .await?
        } else {
            debug!("No reference in deltaspace, creating baseline");
            self.set_reference_baseline(
                &obj_key,
                &deltaspace_id,
                data,
                &sha256,
                &md5,
                content_type.clone(),
            )
            .await?
        };

        // Try to compute delta
        let reference = self.get_reference_cached(&deltaspace_id).await?;
        let delta = self.codec.encode(&reference, data)?;
        let ratio = DeltaCodec::compression_ratio(data.len(), delta.len());

        info!(
            "Delta computed: {} bytes -> {} bytes (ratio: {:.2}%)",
            data.len(),
            delta.len(),
            ratio * 100.0
        );

        // Check if delta is worth storing
        if ratio >= self.max_delta_ratio {
            debug!(
                "Delta ratio {:.2} >= {:.2}, storing directly",
                ratio, self.max_delta_ratio
            );
            self.delete_delta_if_exists(&deltaspace_id, &obj_key.filename)
                .await?;
            return self
                .store_direct(&obj_key, &deltaspace_id, data, sha256, md5, content_type)
                .await;
        }

        // Store as delta
        let metadata = FileMetadata::new_delta(
            obj_key.filename.clone(),
            sha256,
            md5,
            data.len() as u64,
            format!("{}/reference.bin", deltaspace_id),
            ref_meta.file_sha256,
            delta.len() as u64,
            content_type,
        );

        self.delete_direct_if_exists(&deltaspace_id, &obj_key.filename)
            .await?;
        self.deltaspace_mgr
            .store_delta(&deltaspace_id, &obj_key.filename, &delta, &metadata)
            .await?;

        Ok(StoreResult {
            metadata,
            stored_size: delta.len() as u64,
        })
    }

    /// Store the internal deltaspace reference baseline.
    async fn set_reference_baseline(
        &self,
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

        self.deltaspace_mgr
            .set_reference(deltaspace_id, data, &metadata)
            .await?;

        // Cache the reference
        self.cache.put(deltaspace_id, data.to_vec());

        Ok(metadata)
    }

    /// Store directly without delta compression
    async fn store_direct(
        &self,
        obj_key: &ObjectKey,
        deltaspace_id: &str,
        data: &[u8],
        sha256: String,
        md5: String,
        content_type: Option<String>,
    ) -> Result<StoreResult, EngineError> {
        let metadata = FileMetadata::new_direct(
            obj_key.filename.clone(),
            sha256,
            md5,
            data.len() as u64,
            content_type,
        );

        self.deltaspace_mgr
            .store_direct(deltaspace_id, &obj_key.filename, data, &metadata)
            .await?;

        Ok(StoreResult {
            metadata,
            stored_size: data.len() as u64,
        })
    }

    async fn delete_delta_if_exists(
        &self,
        deltaspace_id: &str,
        filename: &str,
    ) -> Result<(), EngineError> {
        match self.deltaspace_mgr.delete_delta(deltaspace_id, filename).await {
            Ok(()) => Ok(()),
            Err(StorageError::NotFound(_)) => Ok(()),
            Err(other) => Err(other.into()),
        }
    }

    async fn delete_direct_if_exists(
        &self,
        deltaspace_id: &str,
        filename: &str,
    ) -> Result<(), EngineError> {
        match self.deltaspace_mgr.delete_direct(deltaspace_id, filename).await {
            Ok(()) => Ok(()),
            Err(StorageError::NotFound(_)) => Ok(()),
            Err(other) => Err(other.into()),
        }
    }

    async fn migrate_legacy_reference_object_if_needed(
        &self,
        deltaspace_id: &str,
        filename: &str,
    ) -> Result<bool, EngineError> {
        if !self.deltaspace_mgr.has_reference(deltaspace_id).await {
            return Ok(false);
        }

        let mut ref_meta = self.deltaspace_mgr.get_reference_metadata(deltaspace_id).await?;
        if ref_meta.original_name == Self::INTERNAL_REFERENCE_NAME {
            return Ok(false);
        }
        if ref_meta.original_name != filename {
            return Ok(false);
        }

        let reference = self.get_reference_cached(deltaspace_id).await?;
        let delta = self.codec.encode(&reference, &reference)?;

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

        self.delete_direct_if_exists(deltaspace_id, filename).await?;
        self.deltaspace_mgr
            .store_delta(deltaspace_id, filename, &delta, &delta_meta)
            .await?;

        ref_meta.original_name = Self::INTERNAL_REFERENCE_NAME.to_string();
        self.deltaspace_mgr
            .set_reference_metadata(deltaspace_id, &ref_meta)
            .await?;

        Ok(true)
    }

    /// Retrieve an object, reconstructing from delta if necessary
    #[instrument(skip(self))]
    pub async fn retrieve(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<(Vec<u8>, FileMetadata), EngineError> {
        let obj_key = ObjectKey::parse(bucket, key);
        obj_key
            .validate_object()
            .map_err(|e| EngineError::InvalidArgument(e.to_string()))?;
        let deltaspace_id = obj_key.deltaspace_id();

        // Get metadata
        let mut metadata = self
            .deltaspace_mgr
            .get_metadata(&deltaspace_id, &obj_key.full_key())
            .await?;
        if metadata.is_none()
            && self
                .migrate_legacy_reference_object_if_needed(&deltaspace_id, &obj_key.filename)
                .await?
        {
            metadata = self
                .deltaspace_mgr
                .get_metadata(&deltaspace_id, &obj_key.full_key())
                .await?;
        }

        let metadata = metadata.ok_or_else(|| EngineError::NotFound(obj_key.full_key()))?;

        info!(
            "Retrieving {}/{} (stored as {:?})",
            bucket,
            key,
            match &metadata.storage_info {
                StorageInfo::Reference { .. } => "reference",
                StorageInfo::Delta { .. } => "delta",
                StorageInfo::Direct => "direct",
            }
        );

        // Get data based on storage type
        let data = match &metadata.storage_info {
            StorageInfo::Reference { .. } => {
                self.deltaspace_mgr.get_reference(&deltaspace_id).await?
            }
            StorageInfo::Direct => {
                self.deltaspace_mgr
                    .get_direct(&deltaspace_id, &obj_key.filename)
                    .await?
            }
            StorageInfo::Delta { .. } => {
                // Reconstruct from reference + delta
                let reference = self.get_reference_cached(&deltaspace_id).await?;
                let delta = self
                    .deltaspace_mgr
                    .get_delta(&deltaspace_id, &obj_key.filename)
                    .await?;
                self.codec.decode(&reference, &delta)?
            }
        };

        // Verify checksum
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

        debug!("Retrieved {} bytes for {}", data.len(), obj_key.full_key());

        Ok((data, metadata))
    }

    /// Retrieve object metadata without reading object bodies.
    #[instrument(skip(self))]
    pub async fn head(&self, bucket: &str, key: &str) -> Result<FileMetadata, EngineError> {
        let obj_key = ObjectKey::parse(bucket, key);
        obj_key
            .validate_object()
            .map_err(|e| EngineError::InvalidArgument(e.to_string()))?;
        let deltaspace_id = obj_key.deltaspace_id();

        let mut metadata = self
            .deltaspace_mgr
            .get_metadata(&deltaspace_id, &obj_key.full_key())
            .await?;
        if metadata.is_none()
            && self
                .migrate_legacy_reference_object_if_needed(&deltaspace_id, &obj_key.filename)
                .await?
        {
            metadata = self
                .deltaspace_mgr
                .get_metadata(&deltaspace_id, &obj_key.full_key())
                .await?;
        }

        metadata.ok_or_else(|| EngineError::NotFound(obj_key.full_key()))
    }

    /// List objects matching a prefix
    #[instrument(skip(self))]
    pub async fn list(&self, bucket: &str, prefix: &str) -> Result<Vec<FileMetadata>, EngineError> {
        let page = self.list_objects_v2(bucket, prefix, u32::MAX, None).await?;
        Ok(page
            .objects
            .into_iter()
            .map(|(_key, meta)| meta)
            .collect())
    }

    /// ListObjectsV2-style listing across all deltaspaces, with simple pagination.
    #[instrument(skip(self))]
    pub async fn list_objects_v2(
        &self,
        _bucket: &str,
        prefix: &str,
        max_keys: u32,
        continuation_token: Option<&str>,
    ) -> Result<ListObjectsV2Page, EngineError> {
        ObjectKey::validate_prefix(prefix)
            .map_err(|e| EngineError::InvalidArgument(e.to_string()))?;
        let deltaspace_ids = self.deltaspace_mgr.list_deltaspaces().await?;
        let mut latest: std::collections::HashMap<String, FileMetadata> =
            std::collections::HashMap::new();

        for deltaspace_id in deltaspace_ids {
            let metas = self.deltaspace_mgr.list_objects(&deltaspace_id).await?;

            let key_prefix = if deltaspace_id == "_root_" {
                ""
            } else {
                deltaspace_id.as_str()
            };

            for meta in metas {
                if matches!(meta.storage_info, StorageInfo::Reference { .. }) {
                    continue;
                }
                let full_key = if key_prefix.is_empty() {
                    meta.original_name.clone()
                } else {
                    format!("{}/{}", key_prefix, meta.original_name)
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

    /// Delete an object
    #[instrument(skip(self))]
    pub async fn delete(&self, bucket: &str, key: &str) -> Result<(), EngineError> {
        let obj_key = ObjectKey::parse(bucket, key);
        obj_key
            .validate_object()
            .map_err(|e| EngineError::InvalidArgument(e.to_string()))?;
        let deltaspace_id = obj_key.deltaspace_id();

        info!("Deleting {}/{}", bucket, key);

        // Check if object exists
        let mut metadata = self
            .deltaspace_mgr
            .get_metadata(&deltaspace_id, &obj_key.full_key())
            .await?;
        if metadata.is_none()
            && self
                .migrate_legacy_reference_object_if_needed(&deltaspace_id, &obj_key.filename)
                .await?
        {
            metadata = self
                .deltaspace_mgr
                .get_metadata(&deltaspace_id, &obj_key.full_key())
                .await?;
        }

        let metadata = metadata.ok_or_else(|| EngineError::NotFound(obj_key.full_key()))?;

        // Delete based on storage type
        match &metadata.storage_info {
            StorageInfo::Direct => {
                self.deltaspace_mgr
                    .delete_direct(&deltaspace_id, &obj_key.filename)
                    .await?;
            }
            StorageInfo::Delta { .. } => {
                self.deltaspace_mgr
                    .delete_delta(&deltaspace_id, &obj_key.filename)
                    .await?;
            }
            StorageInfo::Reference { .. } => {
                return Err(EngineError::InvalidArgument(
                    "Reference objects are internal and cannot be deleted directly".to_string(),
                ));
            }
        }

        // If this deltaspace no longer has any objects, clean up its reference baseline.
        let remaining = self.deltaspace_mgr.list_objects(&deltaspace_id).await?;
        let has_objects = remaining
            .iter()
            .any(|m| !matches!(m.storage_info, StorageInfo::Reference { .. }));
        if !has_objects && self.deltaspace_mgr.has_reference(&deltaspace_id).await {
            self.cache.invalidate(&deltaspace_id);
            self.deltaspace_mgr.delete_reference(&deltaspace_id).await?;
        }

        debug!("Deleted {}/{}", bucket, key);
        Ok(())
    }

    /// Get reference with caching
    async fn get_reference_cached(&self, deltaspace_id: &str) -> Result<Vec<u8>, EngineError> {
        // Check cache first
        if let Some(data) = self.cache.get(deltaspace_id) {
            return Ok(data);
        }

        // Load from storage
        let data = self
            .deltaspace_mgr
            .get_reference(deltaspace_id)
            .await
            .map_err(|_| EngineError::MissingReference(deltaspace_id.to_string()))?;

        // Cache it
        self.cache.put(deltaspace_id, data.clone());

        Ok(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn create_test_engine() -> (DeltaGliderEngine<FilesystemBackend>, TempDir) {
        let tmp = TempDir::new().unwrap();
        let config = Config {
            backend: BackendConfig::Filesystem {
                path: tmp.path().to_path_buf(),
            },
            max_delta_ratio: 0.5,
            max_object_size: 10 * 1024 * 1024,
            cache_size_mb: 10,
            ..Default::default()
        };
        let engine = DeltaGliderEngine::new_filesystem(&config).await.unwrap();
        (engine, tmp)
    }

    #[tokio::test]
    async fn test_store_retrieve_direct() {
        let (engine, _tmp) = create_test_engine().await;

        // Store a non-delta-eligible file
        let data = b"Hello, World!";
        let result = engine
            .store("bucket", "file.txt", data, None)
            .await
            .unwrap();
        assert!(matches!(result.metadata.storage_info, StorageInfo::Direct));

        // Retrieve it
        let (retrieved, _meta) = engine.retrieve("bucket", "file.txt").await.unwrap();
        assert_eq!(retrieved, data);
    }

    #[tokio::test]
    async fn test_store_retrieve_first_delta_eligible() {
        let (engine, _tmp) = create_test_engine().await;

        // First delta-eligible file initializes the reference baseline but is still stored as an object.
        let data = vec![0u8; 10_000];
        let result = engine
            .store("bucket", "releases/v1.zip", &data, None)
            .await
            .unwrap();
        assert!(matches!(result.metadata.storage_info, StorageInfo::Delta { .. }));

        // Retrieve it
        let (retrieved, _meta) = engine.retrieve("bucket", "releases/v1.zip").await.unwrap();
        assert_eq!(retrieved, data);
    }

    #[tokio::test]
    async fn test_store_retrieve_delta() {
        let (engine, _tmp) = create_test_engine().await;

        // First file becomes reference
        let base_data = vec![0u8; 10000];
        engine
            .store("bucket", "releases/v1.zip", &base_data, None)
            .await
            .unwrap();

        // Second similar file should be stored as delta
        let mut modified = base_data.clone();
        modified[0] = 1; // Small change
        modified[100] = 2;
        let result = engine
            .store("bucket", "releases/v2.zip", &modified, None)
            .await
            .unwrap();

        // Should be stored as delta (ratio should be < 0.5)
        assert!(matches!(
            result.metadata.storage_info,
            StorageInfo::Delta { .. }
        ));
        assert!(result.stored_size < modified.len() as u64);

        // Retrieve and verify
        let (retrieved, _meta) = engine.retrieve("bucket", "releases/v2.zip").await.unwrap();
        assert_eq!(retrieved, modified);
    }

    #[tokio::test]
    async fn test_list_objects() {
        let (engine, _tmp) = create_test_engine().await;

        engine
            .store("bucket", "prefix/a.zip", b"data a", None)
            .await
            .unwrap();
        engine
            .store("bucket", "prefix/b.zip", b"data b", None)
            .await
            .unwrap();

        let objects = engine.list("bucket", "prefix/").await.unwrap();
        assert_eq!(objects.len(), 2);
    }

    #[tokio::test]
    async fn test_list_objects_v2_pagination() {
        let (engine, _tmp) = create_test_engine().await;

        engine
            .store("bucket", "prefix/a.zip", b"data a", None)
            .await
            .unwrap();
        engine
            .store("bucket", "prefix/b.zip", b"data b", None)
            .await
            .unwrap();
        engine
            .store("bucket", "prefix/c.zip", b"data c", None)
            .await
            .unwrap();

        let page1 = engine
            .list_objects_v2("bucket", "prefix/", 2, None)
            .await
            .unwrap();
        assert_eq!(
            page1
                .objects
                .iter()
                .map(|(k, _)| k.as_str())
                .collect::<Vec<_>>(),
            vec!["prefix/a.zip", "prefix/b.zip"]
        );
        assert!(page1.is_truncated);
        assert_eq!(page1.next_continuation_token.as_deref(), Some("prefix/b.zip"));

        let page2 = engine
            .list_objects_v2("bucket", "prefix/", 2, page1.next_continuation_token.as_deref())
            .await
            .unwrap();
        assert_eq!(
            page2
                .objects
                .iter()
                .map(|(k, _)| k.as_str())
                .collect::<Vec<_>>(),
            vec!["prefix/c.zip"]
        );
        assert!(!page2.is_truncated);
        assert!(page2.next_continuation_token.is_none());
    }

    #[tokio::test]
    async fn test_size_limit() {
        let tmp = TempDir::new().unwrap();
        let config = Config {
            backend: BackendConfig::Filesystem {
                path: tmp.path().to_path_buf(),
            },
            max_object_size: 100, // Very small limit
            ..Default::default()
        };
        let engine = DeltaGliderEngine::new_filesystem(&config).await.unwrap();

        let large_data = vec![0u8; 200];
        let result = engine.store("bucket", "large.zip", &large_data, None).await;

        assert!(matches!(result, Err(EngineError::TooLarge { .. })));
    }
}
