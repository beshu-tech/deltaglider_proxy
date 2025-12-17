//! Storage backend trait definitions

use crate::types::FileMetadata;
use async_trait::async_trait;
use std::path::Path;
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

    #[error("Serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("Object too large: {size} bytes (max: {max} bytes)")]
    TooLarge { size: u64, max: u64 },

    #[error("S3 error: {0}")]
    S3(String),

    #[error("Storage error: {0}")]
    Other(String),
}

/// Abstract storage backend for S3-like object storage
/// Uses per-file metadata sidecars following DeltaGlider schema
///
/// This trait is object-safe and can be used with `Box<dyn StorageBackend>`.
#[async_trait]
pub trait StorageBackend: Send + Sync {
    /// Store raw bytes at a path
    async fn put_raw(&self, path: &Path, data: &[u8]) -> Result<(), StorageError>;

    /// Retrieve raw bytes from a path
    async fn get_raw(&self, path: &Path) -> Result<Vec<u8>, StorageError>;

    /// Check if a path exists
    async fn exists(&self, path: &Path) -> bool;

    /// Delete a path
    async fn delete(&self, path: &Path) -> Result<(), StorageError>;

    /// List all files under a prefix
    async fn list_prefix(&self, prefix: &Path) -> Result<Vec<String>, StorageError>;

    // === Reference file operations ===

    /// Get the reference file for a deltaspace
    async fn get_reference(&self, prefix: &str) -> Result<Vec<u8>, StorageError>;

    /// Store a reference file with its metadata
    async fn put_reference(
        &self,
        prefix: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError>;

    /// Store/update reference metadata without rewriting reference data.
    async fn put_reference_metadata(
        &self,
        prefix: &str,
        metadata: &FileMetadata,
    ) -> Result<(), StorageError>;

    /// Get reference file metadata
    async fn get_reference_metadata(&self, prefix: &str) -> Result<FileMetadata, StorageError>;

    /// Check if reference exists
    async fn has_reference(&self, prefix: &str) -> bool;

    /// Delete a reference file and its metadata
    async fn delete_reference(&self, prefix: &str) -> Result<(), StorageError>;

    // === Delta file operations ===

    /// Get a delta file
    async fn get_delta(&self, prefix: &str, filename: &str) -> Result<Vec<u8>, StorageError>;

    /// Store a delta file with its metadata
    async fn put_delta(
        &self,
        prefix: &str,
        filename: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError>;

    /// Get delta file metadata
    async fn get_delta_metadata(
        &self,
        prefix: &str,
        filename: &str,
    ) -> Result<FileMetadata, StorageError>;

    /// Delete a delta file and its metadata
    async fn delete_delta(&self, prefix: &str, filename: &str) -> Result<(), StorageError>;

    // === Direct file operations ===

    /// Get a direct (non-delta) file
    async fn get_direct(&self, prefix: &str, filename: &str) -> Result<Vec<u8>, StorageError>;

    /// Store a direct (non-delta) file with its metadata
    async fn put_direct(
        &self,
        prefix: &str,
        filename: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError>;

    /// Get direct file metadata
    async fn get_direct_metadata(
        &self,
        prefix: &str,
        filename: &str,
    ) -> Result<FileMetadata, StorageError>;

    /// Delete a direct (non-delta) file and its metadata
    async fn delete_direct(&self, prefix: &str, filename: &str) -> Result<(), StorageError>;

    // === Scanning operations ===

    /// Scan a deltaspace directory and return all file metadata
    /// This replaces the centralized index - state is derived from files
    async fn scan_deltaspace(&self, prefix: &str) -> Result<Vec<FileMetadata>, StorageError>;

    /// List all deltaspace prefixes (directories with stored files)
    async fn list_deltaspaces(&self) -> Result<Vec<String>, StorageError>;

    /// Get total storage size used (for metrics)
    async fn total_size(&self) -> Result<u64, StorageError>;
}

/// Blanket implementation for boxed trait objects, enabling dynamic dispatch
#[async_trait]
impl StorageBackend for Box<dyn StorageBackend> {
    async fn put_raw(&self, path: &Path, data: &[u8]) -> Result<(), StorageError> {
        (**self).put_raw(path, data).await
    }

    async fn get_raw(&self, path: &Path) -> Result<Vec<u8>, StorageError> {
        (**self).get_raw(path).await
    }

    async fn exists(&self, path: &Path) -> bool {
        (**self).exists(path).await
    }

    async fn delete(&self, path: &Path) -> Result<(), StorageError> {
        (**self).delete(path).await
    }

    async fn list_prefix(&self, prefix: &Path) -> Result<Vec<String>, StorageError> {
        (**self).list_prefix(prefix).await
    }

    async fn get_reference(&self, prefix: &str) -> Result<Vec<u8>, StorageError> {
        (**self).get_reference(prefix).await
    }

    async fn put_reference(
        &self,
        prefix: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        (**self).put_reference(prefix, data, metadata).await
    }

    async fn put_reference_metadata(
        &self,
        prefix: &str,
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        (**self).put_reference_metadata(prefix, metadata).await
    }

    async fn get_reference_metadata(&self, prefix: &str) -> Result<FileMetadata, StorageError> {
        (**self).get_reference_metadata(prefix).await
    }

    async fn has_reference(&self, prefix: &str) -> bool {
        (**self).has_reference(prefix).await
    }

    async fn delete_reference(&self, prefix: &str) -> Result<(), StorageError> {
        (**self).delete_reference(prefix).await
    }

    async fn get_delta(&self, prefix: &str, filename: &str) -> Result<Vec<u8>, StorageError> {
        (**self).get_delta(prefix, filename).await
    }

    async fn put_delta(
        &self,
        prefix: &str,
        filename: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        (**self).put_delta(prefix, filename, data, metadata).await
    }

    async fn get_delta_metadata(
        &self,
        prefix: &str,
        filename: &str,
    ) -> Result<FileMetadata, StorageError> {
        (**self).get_delta_metadata(prefix, filename).await
    }

    async fn delete_delta(&self, prefix: &str, filename: &str) -> Result<(), StorageError> {
        (**self).delete_delta(prefix, filename).await
    }

    async fn get_direct(&self, prefix: &str, filename: &str) -> Result<Vec<u8>, StorageError> {
        (**self).get_direct(prefix, filename).await
    }

    async fn put_direct(
        &self,
        prefix: &str,
        filename: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        (**self).put_direct(prefix, filename, data, metadata).await
    }

    async fn get_direct_metadata(
        &self,
        prefix: &str,
        filename: &str,
    ) -> Result<FileMetadata, StorageError> {
        (**self).get_direct_metadata(prefix, filename).await
    }

    async fn delete_direct(&self, prefix: &str, filename: &str) -> Result<(), StorageError> {
        (**self).delete_direct(prefix, filename).await
    }

    async fn scan_deltaspace(&self, prefix: &str) -> Result<Vec<FileMetadata>, StorageError> {
        (**self).scan_deltaspace(prefix).await
    }

    async fn list_deltaspaces(&self) -> Result<Vec<String>, StorageError> {
        (**self).list_deltaspaces().await
    }

    async fn total_size(&self) -> Result<u64, StorageError> {
        (**self).total_size().await
    }
}
