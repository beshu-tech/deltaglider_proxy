//! DeltaSpace management - reference and delta file coordination
//! Uses per-file metadata sidecars - no centralized index

use crate::storage::{StorageBackend, StorageError};
use crate::types::FileMetadata;
use std::sync::Arc;
use tracing::{debug, instrument};

/// Manages deltaspaces - collections of reference + delta files
/// State is derived from scanning files, not from an index
pub struct DeltaSpaceManager<S: StorageBackend> {
    storage: Arc<S>,
}

impl<S: StorageBackend> DeltaSpaceManager<S> {
    /// Create a new manager with the given storage backend
    pub fn new(storage: Arc<S>) -> Self {
        Self { storage }
    }

    /// Check if a deltaspace has a reference file
    pub async fn has_reference(&self, prefix: &str) -> bool {
        self.storage.has_reference(prefix).await
    }

    /// Get the reference file for a deltaspace
    #[instrument(skip(self))]
    pub async fn get_reference(&self, prefix: &str) -> Result<Vec<u8>, StorageError> {
        self.storage.get_reference(prefix).await
    }

    /// Get the reference file metadata
    #[instrument(skip(self))]
    pub async fn get_reference_metadata(&self, prefix: &str) -> Result<FileMetadata, StorageError> {
        self.storage.get_reference_metadata(prefix).await
    }

    /// Store a reference file with metadata
    #[instrument(skip(self, data, metadata))]
    pub async fn set_reference(
        &self,
        prefix: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        self.storage.put_reference(prefix, data, metadata).await?;
        debug!("Set reference for deltaspace {}", prefix);
        Ok(())
    }

    /// Update reference metadata without rewriting the reference data.
    #[instrument(skip(self, metadata))]
    pub async fn set_reference_metadata(
        &self,
        prefix: &str,
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        self.storage.put_reference_metadata(prefix, metadata).await?;
        debug!("Updated reference metadata for deltaspace {}", prefix);
        Ok(())
    }

    /// Store a delta file with metadata
    #[instrument(skip(self, delta, metadata))]
    pub async fn store_delta(
        &self,
        prefix: &str,
        filename: &str,
        delta: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        self.storage
            .put_delta(prefix, filename, delta, metadata)
            .await?;
        debug!("Stored delta for {}/{}", prefix, filename);
        Ok(())
    }

    /// Store a direct (non-delta) file with metadata
    #[instrument(skip(self, data, metadata))]
    pub async fn store_direct(
        &self,
        prefix: &str,
        filename: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        self.storage
            .put_direct(prefix, filename, data, metadata)
            .await?;
        debug!("Stored direct file for {}/{}", prefix, filename);
        Ok(())
    }

    /// Get a delta file
    pub async fn get_delta(&self, prefix: &str, filename: &str) -> Result<Vec<u8>, StorageError> {
        self.storage.get_delta(prefix, filename).await
    }

    /// Get a direct file
    pub async fn get_direct(&self, prefix: &str, filename: &str) -> Result<Vec<u8>, StorageError> {
        self.storage.get_direct(prefix, filename).await
    }

    /// Get metadata for an object by scanning files
    /// Returns None if object doesn't exist
    #[instrument(skip(self))]
    pub async fn get_metadata(
        &self,
        prefix: &str,
        original_name: &str,
    ) -> Result<Option<FileMetadata>, StorageError> {
        // Extract just the filename from the original_name (which may be a full key like "prefix/file.zip")
        let filename = original_name.rsplit('/').next().unwrap_or(original_name);

        let delta = self.storage.get_delta_metadata(prefix, filename).await.ok();
        let direct = self.storage.get_direct_metadata(prefix, filename).await.ok();

        match (delta, direct) {
            (Some(delta), Some(direct)) => Ok(Some(if delta.created_at >= direct.created_at {
                delta
            } else {
                direct
            })),
            (Some(meta), None) | (None, Some(meta)) => Ok(Some(meta)),
            (None, None) => Ok(None),
        }
    }

    /// List all objects in a deltaspace by scanning files
    #[instrument(skip(self))]
    pub async fn list_objects(&self, prefix: &str) -> Result<Vec<FileMetadata>, StorageError> {
        self.storage.scan_deltaspace(prefix).await
    }

    /// Delete a reference file
    #[instrument(skip(self))]
    pub async fn delete_reference(&self, prefix: &str) -> Result<(), StorageError> {
        self.storage.delete_reference(prefix).await?;
        debug!("Deleted reference for deltaspace {}", prefix);
        Ok(())
    }

    /// Delete a delta file
    #[instrument(skip(self))]
    pub async fn delete_delta(&self, prefix: &str, filename: &str) -> Result<(), StorageError> {
        self.storage.delete_delta(prefix, filename).await?;
        debug!("Deleted delta {}/{}", prefix, filename);
        Ok(())
    }

    /// Delete a direct file
    #[instrument(skip(self))]
    pub async fn delete_direct(&self, prefix: &str, filename: &str) -> Result<(), StorageError> {
        self.storage.delete_direct(prefix, filename).await?;
        debug!("Deleted direct {}/{}", prefix, filename);
        Ok(())
    }

    /// List all deltaspaces
    pub async fn list_deltaspaces(&self) -> Result<Vec<String>, StorageError> {
        self.storage.list_deltaspaces().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::FilesystemBackend;
    use tempfile::TempDir;

    async fn create_test_manager() -> (DeltaSpaceManager<FilesystemBackend>, TempDir) {
        let tmp = TempDir::new().unwrap();
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .unwrap();
        let manager = DeltaSpaceManager::new(Arc::new(backend));
        (manager, tmp)
    }

    #[tokio::test]
    async fn test_set_and_get_reference() {
        let (manager, _tmp) = create_test_manager().await;

        let data = b"reference content";
        let metadata = FileMetadata::new_reference(
            "file.zip".to_string(),
            "test/file.zip".to_string(),
            "abc123".to_string(),
            "def456".to_string(),
            data.len() as u64,
            None,
        );

        manager
            .set_reference("test", data, &metadata)
            .await
            .unwrap();

        assert!(manager.has_reference("test").await);

        let retrieved = manager.get_reference("test").await.unwrap();
        assert_eq!(retrieved, data);

        let meta = manager.get_reference_metadata("test").await.unwrap();
        assert!(meta.is_reference());
    }

    #[tokio::test]
    async fn test_get_metadata_by_name() {
        let (manager, _tmp) = create_test_manager().await;

        // Store a reference
        let ref_meta = FileMetadata::new_reference(
            "base.zip".to_string(),
            "releases/base.zip".to_string(),
            "sha1".to_string(),
            "md5_1".to_string(),
            100,
            None,
        );
        manager
            .set_reference("releases", b"ref data", &ref_meta)
            .await
            .unwrap();

        // Store a delta
        let delta_meta = FileMetadata::new_delta(
            "v2.zip".to_string(),
            "sha2".to_string(),
            "md5_2".to_string(),
            100,
            "releases/reference.bin".to_string(),
            "sha1".to_string(),
            50,
            None,
        );
        manager
            .store_delta("releases", "v2.zip", b"delta", &delta_meta)
            .await
            .unwrap();

        // Find by name (reference is internal; only delta/direct objects are addressable)
        let found_ref = manager.get_metadata("releases", "base.zip").await.unwrap();
        assert!(found_ref.is_none());

        let found_delta = manager.get_metadata("releases", "v2.zip").await.unwrap();
        assert!(found_delta.is_some());
        assert!(found_delta.unwrap().is_delta());

        let not_found = manager
            .get_metadata("releases", "nonexistent.zip")
            .await
            .unwrap();
        assert!(not_found.is_none());
    }

    #[tokio::test]
    async fn test_list_objects() {
        let (manager, _tmp) = create_test_manager().await;

        // Store multiple files
        let ref_meta = FileMetadata::new_reference(
            "base.zip".to_string(),
            "releases/base.zip".to_string(),
            "sha1".to_string(),
            "md5_1".to_string(),
            100,
            None,
        );
        manager
            .set_reference("releases", b"ref", &ref_meta)
            .await
            .unwrap();

        let delta_meta = FileMetadata::new_delta(
            "v2.zip".to_string(),
            "sha2".to_string(),
            "md5_2".to_string(),
            100,
            "releases/reference.bin".to_string(),
            "sha1".to_string(),
            50,
            None,
        );
        manager
            .store_delta("releases", "v2.zip", b"delta", &delta_meta)
            .await
            .unwrap();

        let direct_meta = FileMetadata::new_direct(
            "readme.txt".to_string(),
            "sha3".to_string(),
            "md5_3".to_string(),
            20,
            None,
        );
        manager
            .store_direct("releases", "readme.txt", b"readme", &direct_meta)
            .await
            .unwrap();

        // List all
        let objects = manager.list_objects("releases").await.unwrap();
        assert_eq!(objects.len(), 3);
    }
}
