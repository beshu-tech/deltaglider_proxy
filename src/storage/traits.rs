//! Storage backend trait definitions

use bytes::Bytes;
use crate::types::FileMetadata;
use async_trait::async_trait;
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
/// Uses per-file metadata sidecars following DeltaGlider schema
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
    async fn get_reference_metadata(&self, bucket: &str, prefix: &str) -> Result<FileMetadata, StorageError>;

    /// Check if reference exists
    async fn has_reference(&self, bucket: &str, prefix: &str) -> bool;

    /// Delete a reference file and its metadata
    async fn delete_reference(&self, bucket: &str, prefix: &str) -> Result<(), StorageError>;

    // === Delta file operations ===

    /// Get a delta file
    async fn get_delta(&self, bucket: &str, prefix: &str, filename: &str) -> Result<Vec<u8>, StorageError>;

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
    async fn delete_delta(&self, bucket: &str, prefix: &str, filename: &str) -> Result<(), StorageError>;

    // === Direct file operations ===

    /// Get a direct (non-delta) file
    async fn get_direct(&self, bucket: &str, prefix: &str, filename: &str) -> Result<Vec<u8>, StorageError>;

    /// Store a direct (non-delta) file with its metadata
    async fn put_direct(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError>;

    /// Get direct file metadata
    async fn get_direct_metadata(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<FileMetadata, StorageError>;

    /// Delete a direct (non-delta) file and its metadata
    async fn delete_direct(&self, bucket: &str, prefix: &str, filename: &str) -> Result<(), StorageError>;

    // === Streaming operations ===

    /// Stream a direct file's contents without buffering the entire file in memory.
    /// Default implementation falls back to `get_direct()` and wraps in a single-chunk stream.
    async fn get_direct_stream(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<BoxStream<'static, Result<Bytes, StorageError>>, StorageError> {
        let data = self.get_direct(bucket, prefix, filename).await?;
        Ok(Box::pin(stream::once(async { Ok(Bytes::from(data)) })))
    }

    // === Scanning operations ===

    /// Scan a deltaspace directory and return all file metadata
    /// This replaces the centralized index - state is derived from files
    async fn scan_deltaspace(&self, bucket: &str, prefix: &str) -> Result<Vec<FileMetadata>, StorageError>;

    /// List all deltaspace prefixes within a bucket
    async fn list_deltaspaces(&self, bucket: &str) -> Result<Vec<String>, StorageError>;

    /// Get total storage size used (for metrics), optionally scoped to a bucket
    async fn total_size(&self, bucket: Option<&str>) -> Result<u64, StorageError>;
}

/// Generate the blanket `impl StorageBackend for Box<dyn StorageBackend>`
/// that forwards every method through dynamic dispatch.
macro_rules! impl_storage_backend_for_box {
    () => {
        #[async_trait]
        impl StorageBackend for Box<dyn StorageBackend> {
            async fn create_bucket(&self, bucket: &str) -> Result<(), StorageError> { (**self).create_bucket(bucket).await }
            async fn delete_bucket(&self, bucket: &str) -> Result<(), StorageError> { (**self).delete_bucket(bucket).await }
            async fn list_buckets(&self) -> Result<Vec<String>, StorageError> { (**self).list_buckets().await }
            async fn head_bucket(&self, bucket: &str) -> Result<bool, StorageError> { (**self).head_bucket(bucket).await }

            async fn get_reference(&self, bucket: &str, prefix: &str) -> Result<Vec<u8>, StorageError> { (**self).get_reference(bucket, prefix).await }
            async fn put_reference(&self, bucket: &str, prefix: &str, data: &[u8], metadata: &FileMetadata) -> Result<(), StorageError> { (**self).put_reference(bucket, prefix, data, metadata).await }
            async fn put_reference_metadata(&self, bucket: &str, prefix: &str, metadata: &FileMetadata) -> Result<(), StorageError> { (**self).put_reference_metadata(bucket, prefix, metadata).await }
            async fn get_reference_metadata(&self, bucket: &str, prefix: &str) -> Result<FileMetadata, StorageError> { (**self).get_reference_metadata(bucket, prefix).await }
            async fn has_reference(&self, bucket: &str, prefix: &str) -> bool { (**self).has_reference(bucket, prefix).await }
            async fn delete_reference(&self, bucket: &str, prefix: &str) -> Result<(), StorageError> { (**self).delete_reference(bucket, prefix).await }

            async fn get_delta(&self, bucket: &str, prefix: &str, filename: &str) -> Result<Vec<u8>, StorageError> { (**self).get_delta(bucket, prefix, filename).await }
            async fn put_delta(&self, bucket: &str, prefix: &str, filename: &str, data: &[u8], metadata: &FileMetadata) -> Result<(), StorageError> { (**self).put_delta(bucket, prefix, filename, data, metadata).await }
            async fn get_delta_metadata(&self, bucket: &str, prefix: &str, filename: &str) -> Result<FileMetadata, StorageError> { (**self).get_delta_metadata(bucket, prefix, filename).await }
            async fn delete_delta(&self, bucket: &str, prefix: &str, filename: &str) -> Result<(), StorageError> { (**self).delete_delta(bucket, prefix, filename).await }

            async fn get_direct(&self, bucket: &str, prefix: &str, filename: &str) -> Result<Vec<u8>, StorageError> { (**self).get_direct(bucket, prefix, filename).await }
            async fn put_direct(&self, bucket: &str, prefix: &str, filename: &str, data: &[u8], metadata: &FileMetadata) -> Result<(), StorageError> { (**self).put_direct(bucket, prefix, filename, data, metadata).await }
            async fn get_direct_metadata(&self, bucket: &str, prefix: &str, filename: &str) -> Result<FileMetadata, StorageError> { (**self).get_direct_metadata(bucket, prefix, filename).await }
            async fn delete_direct(&self, bucket: &str, prefix: &str, filename: &str) -> Result<(), StorageError> { (**self).delete_direct(bucket, prefix, filename).await }

            async fn get_direct_stream(&self, bucket: &str, prefix: &str, filename: &str) -> Result<BoxStream<'static, Result<Bytes, StorageError>>, StorageError> { (**self).get_direct_stream(bucket, prefix, filename).await }

            async fn scan_deltaspace(&self, bucket: &str, prefix: &str) -> Result<Vec<FileMetadata>, StorageError> { (**self).scan_deltaspace(bucket, prefix).await }
            async fn list_deltaspaces(&self, bucket: &str) -> Result<Vec<String>, StorageError> { (**self).list_deltaspaces(bucket).await }
            async fn total_size(&self, bucket: Option<&str>) -> Result<u64, StorageError> { (**self).total_size(bucket).await }
        }
    };
}

impl_storage_backend_for_box!();
