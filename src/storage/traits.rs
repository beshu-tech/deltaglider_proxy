//! Storage backend trait definitions

use crate::types::FileMetadata;
use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::{self, BoxStream};
use thiserror::Error;

/// Errors that can occur during storage operations
#[derive(Debug, Error)]
pub enum StorageError {
    #[error("Object not found: {0}")]
    NotFound(String),

    #[error("Object already exists: {0}")]
    AlreadyExists(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Disk full: insufficient storage space")]
    DiskFull,

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("Object too large: {size} bytes (max: {max} bytes)")]
    TooLarge { size: u64, max: u64 },

    #[error("S3 error: {0}")]
    S3(String),

    #[error("Bucket not found: {0}")]
    BucketNotFound(String),

    #[error("Bucket not empty: {0}")]
    BucketNotEmpty(String),

    #[error("Storage error: {0}")]
    Other(String),
}

/// Abstract storage backend for S3-like object storage
/// Uses per-file metadata following DeltaGlider schema (xattr on filesystem, sidecars on S3)
///
/// This trait is object-safe and can be used with `Box<dyn StorageBackend>`.
///
/// All methods take a `bucket` parameter which maps to a real storage bucket
/// (S3 bucket or filesystem directory).
#[async_trait]
pub trait StorageBackend: Send + Sync {
    // === Bucket operations ===

    /// Create a new bucket
    async fn create_bucket(&self, bucket: &str) -> Result<(), StorageError>;

    /// Delete a bucket (must be empty)
    async fn delete_bucket(&self, bucket: &str) -> Result<(), StorageError>;

    /// List all buckets
    async fn list_buckets(&self) -> Result<Vec<String>, StorageError>;

    /// Check if a bucket exists
    async fn head_bucket(&self, bucket: &str) -> Result<bool, StorageError>;

    // === Reference file operations ===

    /// Get the reference file for a deltaspace
    async fn get_reference(&self, bucket: &str, prefix: &str) -> Result<Vec<u8>, StorageError>;

    /// Store a reference file with its metadata
    async fn put_reference(
        &self,
        bucket: &str,
        prefix: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError>;

    /// Store/update reference metadata without rewriting reference data.
    async fn put_reference_metadata(
        &self,
        bucket: &str,
        prefix: &str,
        metadata: &FileMetadata,
    ) -> Result<(), StorageError>;

    /// Get reference file metadata
    async fn get_reference_metadata(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<FileMetadata, StorageError>;

    /// Check if reference exists
    async fn has_reference(&self, bucket: &str, prefix: &str) -> bool;

    /// Delete a reference file and its metadata
    async fn delete_reference(&self, bucket: &str, prefix: &str) -> Result<(), StorageError>;

    // === Delta file operations ===

    /// Get a delta file
    async fn get_delta(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<Vec<u8>, StorageError>;

    /// Store a delta file with its metadata
    async fn put_delta(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError>;

    /// Get delta file metadata
    async fn get_delta_metadata(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<FileMetadata, StorageError>;

    /// Delete a delta file and its metadata
    async fn delete_delta(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<(), StorageError>;

    // === Passthrough file operations (stored as-is with original filename) ===

    /// Get a passthrough (non-delta) file
    async fn get_passthrough(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<Vec<u8>, StorageError>;

    /// Store a passthrough (non-delta) file with its metadata
    async fn put_passthrough(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError>;

    /// Get passthrough file metadata
    async fn get_passthrough_metadata(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<FileMetadata, StorageError>;

    /// Delete a passthrough (non-delta) file and its metadata
    async fn delete_passthrough(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<(), StorageError>;

    // === Streaming operations ===

    /// Stream a passthrough file's contents without buffering the entire file in memory.
    /// Default implementation falls back to `get_passthrough()` and wraps in a single-chunk stream.
    async fn get_passthrough_stream(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<BoxStream<'static, Result<Bytes, StorageError>>, StorageError> {
        let data = self.get_passthrough(bucket, prefix, filename).await?;
        Ok(Box::pin(stream::once(async { Ok(Bytes::from(data)) })))
    }

    /// Store a passthrough file from pre-split chunks without assembling into a contiguous buffer.
    /// Default implementation collects chunks and delegates to `put_passthrough()`.
    async fn put_passthrough_chunked(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        chunks: &[Bytes],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        let total_len: usize = chunks.iter().map(|c| c.len()).sum();
        let mut buf = Vec::with_capacity(total_len);
        for chunk in chunks {
            buf.extend_from_slice(chunk);
        }
        self.put_passthrough(bucket, prefix, filename, &buf, metadata)
            .await
    }

    // === Scanning operations ===

    /// Scan a deltaspace directory and return all file metadata
    /// This replaces the centralized index - state is derived from files
    async fn scan_deltaspace(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<Vec<FileMetadata>, StorageError>;

    /// List all deltaspace prefixes within a bucket
    async fn list_deltaspaces(&self, bucket: &str) -> Result<Vec<String>, StorageError>;

    /// Get total storage size used (for metrics), optionally scoped to a bucket
    async fn total_size(&self, bucket: Option<&str>) -> Result<u64, StorageError>;

    /// Store a zero-byte S3 directory marker (key ending with '/').
    /// Used by Cyberduck, AWS Console, etc. to create "folders".
    /// Default: no-op (directories are implicit in S3).
    async fn put_directory_marker(&self, _bucket: &str, _key: &str) -> Result<(), StorageError> {
        Ok(())
    }

    /// List directory markers (zero-byte objects ending with '/') in a bucket.
    /// Returns keys like "folder/" that represent S3 directory markers.
    /// Default: empty (filesystem backend doesn't use directory markers).
    async fn list_directory_markers(
        &self,
        _bucket: &str,
        _prefix: &str,
    ) -> Result<Vec<String>, StorageError> {
        Ok(vec![])
    }

    /// List all objects in a bucket matching a prefix, in a single pass.
    /// Returns `(user_visible_key, FileMetadata)` pairs — references are excluded,
    /// directory markers are included. This replaces the three-step
    /// list_deltaspaces → scan_deltaspace × N → list_directory_markers dance.
    async fn bulk_list_objects(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<Vec<(String, FileMetadata)>, StorageError>;

    /// Optimised listing with delimiter support.
    ///
    /// Backends that can delegate delimiter collapsing to the underlying store
    /// (e.g. S3) override this to avoid fetching every object just to collapse
    /// them into CommonPrefixes.  Returns `None` by default so the engine
    /// falls back to `bulk_list_objects` + in-memory collapsing.
    async fn list_objects_delegated(
        &self,
        _bucket: &str,
        _prefix: &str,
        _delimiter: &str,
        _max_keys: u32,
        _continuation_token: Option<&str>,
    ) -> Result<Option<DelegatedListResult>, StorageError> {
        Ok(None)
    }
}

/// Result from `list_objects_delegated` when the backend handles delimiter
/// collapsing natively.
pub struct DelegatedListResult {
    pub objects: Vec<(String, FileMetadata)>,
    pub common_prefixes: Vec<String>,
    pub is_truncated: bool,
    pub next_continuation_token: Option<String>,
}

/// Generate the blanket `impl StorageBackend for Box<dyn StorageBackend>`
/// that forwards every method through dynamic dispatch.
macro_rules! impl_storage_backend_for_box {
    () => {
        #[async_trait]
        impl StorageBackend for Box<dyn StorageBackend> {
            async fn create_bucket(&self, bucket: &str) -> Result<(), StorageError> {
                (**self).create_bucket(bucket).await
            }
            async fn delete_bucket(&self, bucket: &str) -> Result<(), StorageError> {
                (**self).delete_bucket(bucket).await
            }
            async fn list_buckets(&self) -> Result<Vec<String>, StorageError> {
                (**self).list_buckets().await
            }
            async fn head_bucket(&self, bucket: &str) -> Result<bool, StorageError> {
                (**self).head_bucket(bucket).await
            }

            async fn get_reference(
                &self,
                bucket: &str,
                prefix: &str,
            ) -> Result<Vec<u8>, StorageError> {
                (**self).get_reference(bucket, prefix).await
            }
            async fn put_reference(
                &self,
                bucket: &str,
                prefix: &str,
                data: &[u8],
                metadata: &FileMetadata,
            ) -> Result<(), StorageError> {
                (**self).put_reference(bucket, prefix, data, metadata).await
            }
            async fn put_reference_metadata(
                &self,
                bucket: &str,
                prefix: &str,
                metadata: &FileMetadata,
            ) -> Result<(), StorageError> {
                (**self)
                    .put_reference_metadata(bucket, prefix, metadata)
                    .await
            }
            async fn get_reference_metadata(
                &self,
                bucket: &str,
                prefix: &str,
            ) -> Result<FileMetadata, StorageError> {
                (**self).get_reference_metadata(bucket, prefix).await
            }
            async fn has_reference(&self, bucket: &str, prefix: &str) -> bool {
                (**self).has_reference(bucket, prefix).await
            }
            async fn delete_reference(
                &self,
                bucket: &str,
                prefix: &str,
            ) -> Result<(), StorageError> {
                (**self).delete_reference(bucket, prefix).await
            }

            async fn get_delta(
                &self,
                bucket: &str,
                prefix: &str,
                filename: &str,
            ) -> Result<Vec<u8>, StorageError> {
                (**self).get_delta(bucket, prefix, filename).await
            }
            async fn put_delta(
                &self,
                bucket: &str,
                prefix: &str,
                filename: &str,
                data: &[u8],
                metadata: &FileMetadata,
            ) -> Result<(), StorageError> {
                (**self)
                    .put_delta(bucket, prefix, filename, data, metadata)
                    .await
            }
            async fn get_delta_metadata(
                &self,
                bucket: &str,
                prefix: &str,
                filename: &str,
            ) -> Result<FileMetadata, StorageError> {
                (**self).get_delta_metadata(bucket, prefix, filename).await
            }
            async fn delete_delta(
                &self,
                bucket: &str,
                prefix: &str,
                filename: &str,
            ) -> Result<(), StorageError> {
                (**self).delete_delta(bucket, prefix, filename).await
            }

            async fn get_passthrough(
                &self,
                bucket: &str,
                prefix: &str,
                filename: &str,
            ) -> Result<Vec<u8>, StorageError> {
                (**self).get_passthrough(bucket, prefix, filename).await
            }
            async fn put_passthrough(
                &self,
                bucket: &str,
                prefix: &str,
                filename: &str,
                data: &[u8],
                metadata: &FileMetadata,
            ) -> Result<(), StorageError> {
                (**self)
                    .put_passthrough(bucket, prefix, filename, data, metadata)
                    .await
            }
            async fn get_passthrough_metadata(
                &self,
                bucket: &str,
                prefix: &str,
                filename: &str,
            ) -> Result<FileMetadata, StorageError> {
                (**self)
                    .get_passthrough_metadata(bucket, prefix, filename)
                    .await
            }
            async fn delete_passthrough(
                &self,
                bucket: &str,
                prefix: &str,
                filename: &str,
            ) -> Result<(), StorageError> {
                (**self).delete_passthrough(bucket, prefix, filename).await
            }

            async fn get_passthrough_stream(
                &self,
                bucket: &str,
                prefix: &str,
                filename: &str,
            ) -> Result<BoxStream<'static, Result<Bytes, StorageError>>, StorageError> {
                (**self)
                    .get_passthrough_stream(bucket, prefix, filename)
                    .await
            }

            async fn put_passthrough_chunked(
                &self,
                bucket: &str,
                prefix: &str,
                filename: &str,
                chunks: &[Bytes],
                metadata: &FileMetadata,
            ) -> Result<(), StorageError> {
                (**self)
                    .put_passthrough_chunked(bucket, prefix, filename, chunks, metadata)
                    .await
            }

            async fn scan_deltaspace(
                &self,
                bucket: &str,
                prefix: &str,
            ) -> Result<Vec<FileMetadata>, StorageError> {
                (**self).scan_deltaspace(bucket, prefix).await
            }
            async fn list_deltaspaces(&self, bucket: &str) -> Result<Vec<String>, StorageError> {
                (**self).list_deltaspaces(bucket).await
            }
            async fn total_size(&self, bucket: Option<&str>) -> Result<u64, StorageError> {
                (**self).total_size(bucket).await
            }
            async fn put_directory_marker(
                &self,
                bucket: &str,
                key: &str,
            ) -> Result<(), StorageError> {
                (**self).put_directory_marker(bucket, key).await
            }
            async fn list_directory_markers(
                &self,
                bucket: &str,
                prefix: &str,
            ) -> Result<Vec<String>, StorageError> {
                (**self).list_directory_markers(bucket, prefix).await
            }
            async fn bulk_list_objects(
                &self,
                bucket: &str,
                prefix: &str,
            ) -> Result<Vec<(String, FileMetadata)>, StorageError> {
                (**self).bulk_list_objects(bucket, prefix).await
            }
            async fn list_objects_delegated(
                &self,
                bucket: &str,
                prefix: &str,
                delimiter: &str,
                max_keys: u32,
                continuation_token: Option<&str>,
            ) -> Result<Option<DelegatedListResult>, StorageError> {
                (**self)
                    .list_objects_delegated(bucket, prefix, delimiter, max_keys, continuation_token)
                    .await
            }
        }
    };
}

impl_storage_backend_for_box!();
