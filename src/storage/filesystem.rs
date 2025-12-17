//! Filesystem-based storage backend with per-file metadata sidecars

use super::traits::{StorageBackend, StorageError};
use crate::types::FileMetadata;
use async_trait::async_trait;
use std::path::{Path, PathBuf};
use tokio::fs;
use tracing::{debug, instrument};

/// Filesystem storage backend
///
/// Storage layout:
/// ```text
/// {root}/deltaspaces/{prefix}/
///   reference.bin         # Reference file data
///   reference.bin.meta    # Reference metadata (JSON)
///   {name}.delta          # Delta file data
///   {name}.delta.meta     # Delta metadata (JSON)
///   {name}.direct         # Direct file data
///   {name}.direct.meta    # Direct metadata (JSON)
/// ```
pub struct FilesystemBackend {
    /// Root directory for all data
    root: PathBuf,
}

impl FilesystemBackend {
    /// Create a new filesystem backend with the given root directory
    pub async fn new(root: PathBuf) -> Result<Self, StorageError> {
        // Ensure root directory exists
        fs::create_dir_all(&root).await?;
        Ok(Self { root })
    }

    /// Synchronous constructor for backward compatibility
    pub fn new_sync(root: PathBuf) -> Result<Self, StorageError> {
        std::fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    /// Get the full path for a deltaspace directory
    fn deltaspace_dir(&self, prefix: &str) -> PathBuf {
        let safe_prefix = if prefix.is_empty() { "_root_" } else { prefix };
        self.root.join("deltaspaces").join(safe_prefix)
    }

    /// Get the path for the reference file
    fn reference_path(&self, prefix: &str) -> PathBuf {
        self.deltaspace_dir(prefix).join("reference.bin")
    }

    /// Get the path for reference metadata
    fn reference_meta_path(&self, prefix: &str) -> PathBuf {
        self.deltaspace_dir(prefix).join("reference.bin.meta")
    }

    /// Get the path for a delta file
    fn delta_path(&self, prefix: &str, filename: &str) -> PathBuf {
        self.deltaspace_dir(prefix)
            .join(format!("{}.delta", filename))
    }

    /// Get the path for delta metadata
    fn delta_meta_path(&self, prefix: &str, filename: &str) -> PathBuf {
        self.deltaspace_dir(prefix)
            .join(format!("{}.delta.meta", filename))
    }

    /// Get the path for a direct file
    fn direct_path(&self, prefix: &str, filename: &str) -> PathBuf {
        self.deltaspace_dir(prefix)
            .join(format!("{}.direct", filename))
    }

    /// Get the path for direct metadata
    fn direct_meta_path(&self, prefix: &str, filename: &str) -> PathBuf {
        self.deltaspace_dir(prefix)
            .join(format!("{}.direct.meta", filename))
    }

    /// Ensure a directory exists
    async fn ensure_dir(&self, path: &Path) -> Result<(), StorageError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        Ok(())
    }

    /// Calculate total size of a directory recursively
    async fn dir_size(&self, path: &Path) -> Result<u64, StorageError> {
        let mut total = 0;
        if path.is_dir() {
            let mut entries = fs::read_dir(path).await?;
            while let Some(entry) = entries.next_entry().await? {
                let path = entry.path();
                if path.is_dir() {
                    total += Box::pin(self.dir_size(&path)).await?;
                } else {
                    total += entry.metadata().await?.len();
                }
            }
        }
        Ok(total)
    }

    /// Recursively find all deltaspaces (directories containing deltaglider files)
    fn find_deltaspaces_recursive<'a>(
        base_dir: &'a Path,
        current_dir: &'a Path,
        prefixes: &'a mut Vec<String>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), StorageError>> + Send + 'a>>
    {
        Box::pin(async move {
            let mut entries = fs::read_dir(current_dir).await?;
            let mut has_deltaglider_files = false;

            while let Some(entry) = entries.next_entry().await? {
                let path = entry.path();
                if path.is_dir() {
                    // Recursively check subdirectories
                    Self::find_deltaspaces_recursive(base_dir, &path, prefixes).await?;
                } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    // Check if this is a deltaglider file
                    if name == "reference.bin"
                        || name.ends_with(".delta")
                        || name.ends_with(".direct")
                        || name.ends_with(".meta")
                    {
                        has_deltaglider_files = true;
                    }
                }
            }

            // If this directory has deltaglider files, add it as a deltaspace
            if has_deltaglider_files {
                if let Ok(relative) = current_dir.strip_prefix(base_dir) {
                    let prefix = relative.to_string_lossy().to_string();
                    if !prefixes.contains(&prefix) {
                        prefixes.push(prefix);
                    }
                }
            }

            Ok(())
        })
    }

    /// Read metadata from a .meta file
    async fn read_metadata(&self, meta_path: &Path) -> Result<FileMetadata, StorageError> {
        if !meta_path.exists() {
            return Err(StorageError::NotFound(meta_path.display().to_string()));
        }
        let data = fs::read(meta_path).await?;
        let metadata: FileMetadata = serde_json::from_slice(&data)?;
        Ok(metadata)
    }

    /// Write metadata to a .meta file
    async fn write_metadata(
        &self,
        meta_path: &Path,
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        self.ensure_dir(meta_path).await?;
        let data = serde_json::to_vec_pretty(metadata)?;
        fs::write(meta_path, data).await?;
        debug!("Wrote metadata to {:?}", meta_path);
        Ok(())
    }
}

#[async_trait]
impl StorageBackend for FilesystemBackend {
    #[instrument(skip(self, data))]
    async fn put_raw(&self, path: &Path, data: &[u8]) -> Result<(), StorageError> {
        let full_path = self.root.join(path);
        self.ensure_dir(&full_path).await?;
        fs::write(&full_path, data).await?;
        debug!("Wrote {} bytes to {:?}", data.len(), full_path);
        Ok(())
    }

    #[instrument(skip(self))]
    async fn get_raw(&self, path: &Path) -> Result<Vec<u8>, StorageError> {
        let full_path = self.root.join(path);
        if !full_path.exists() {
            return Err(StorageError::NotFound(path.display().to_string()));
        }
        let data = fs::read(&full_path).await?;
        debug!("Read {} bytes from {:?}", data.len(), full_path);
        Ok(data)
    }

    async fn exists(&self, path: &Path) -> bool {
        self.root.join(path).exists()
    }

    #[instrument(skip(self))]
    async fn delete(&self, path: &Path) -> Result<(), StorageError> {
        let full_path = self.root.join(path);
        if !full_path.exists() {
            return Err(StorageError::NotFound(path.display().to_string()));
        }
        fs::remove_file(&full_path).await?;
        debug!("Deleted {:?}", full_path);
        Ok(())
    }

    #[instrument(skip(self))]
    async fn list_prefix(&self, prefix: &Path) -> Result<Vec<String>, StorageError> {
        let dir = self.root.join(prefix);
        if !dir.exists() {
            return Ok(Vec::new());
        }

        let mut files = Vec::new();
        let mut entries = fs::read_dir(&dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.is_file() {
                if let Some(name) = path.file_name() {
                    files.push(name.to_string_lossy().to_string());
                }
            }
        }
        Ok(files)
    }

    // === Reference operations ===

    #[instrument(skip(self))]
    async fn get_reference(&self, prefix: &str) -> Result<Vec<u8>, StorageError> {
        let path = self.reference_path(prefix);
        if !path.exists() {
            return Err(StorageError::NotFound(format!(
                "Reference for prefix: {}",
                prefix
            )));
        }
        let data = fs::read(&path).await?;
        debug!(
            "Read reference ({} bytes) for prefix {:?}",
            data.len(),
            prefix
        );
        Ok(data)
    }

    #[instrument(skip(self, data, metadata))]
    async fn put_reference(
        &self,
        prefix: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        let data_path = self.reference_path(prefix);
        let meta_path = self.reference_meta_path(prefix);

        self.ensure_dir(&data_path).await?;
        fs::write(&data_path, data).await?;
        self.write_metadata(&meta_path, metadata).await?;

        debug!(
            "Wrote reference ({} bytes) for prefix {:?}",
            data.len(),
            prefix
        );
        Ok(())
    }

    #[instrument(skip(self, metadata))]
    async fn put_reference_metadata(
        &self,
        prefix: &str,
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        let meta_path = self.reference_meta_path(prefix);
        self.write_metadata(&meta_path, metadata).await
    }

    #[instrument(skip(self))]
    async fn get_reference_metadata(&self, prefix: &str) -> Result<FileMetadata, StorageError> {
        let meta_path = self.reference_meta_path(prefix);
        self.read_metadata(&meta_path).await
    }

    async fn has_reference(&self, prefix: &str) -> bool {
        self.reference_path(prefix).exists()
    }

    #[instrument(skip(self))]
    async fn delete_reference(&self, prefix: &str) -> Result<(), StorageError> {
        let data_path = self.reference_path(prefix);
        let meta_path = self.reference_meta_path(prefix);

        if !data_path.exists() {
            return Err(StorageError::NotFound(format!(
                "Reference for prefix: {}",
                prefix
            )));
        }

        fs::remove_file(&data_path).await?;
        if meta_path.exists() {
            fs::remove_file(&meta_path).await?;
        }

        debug!("Deleted reference for prefix {:?}", prefix);
        Ok(())
    }

    // === Delta operations ===

    #[instrument(skip(self))]
    async fn get_delta(&self, prefix: &str, filename: &str) -> Result<Vec<u8>, StorageError> {
        let path = self.delta_path(prefix, filename);
        if !path.exists() {
            return Err(StorageError::NotFound(format!(
                "Delta: {}/{}",
                prefix, filename
            )));
        }
        let data = fs::read(&path).await?;
        debug!(
            "Read delta ({} bytes) for {}/{}",
            data.len(),
            prefix,
            filename
        );
        Ok(data)
    }

    #[instrument(skip(self, data, metadata))]
    async fn put_delta(
        &self,
        prefix: &str,
        filename: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        let data_path = self.delta_path(prefix, filename);
        let meta_path = self.delta_meta_path(prefix, filename);

        self.ensure_dir(&data_path).await?;
        fs::write(&data_path, data).await?;
        self.write_metadata(&meta_path, metadata).await?;

        debug!(
            "Wrote delta ({} bytes) for {}/{}",
            data.len(),
            prefix,
            filename
        );
        Ok(())
    }

    #[instrument(skip(self))]
    async fn get_delta_metadata(
        &self,
        prefix: &str,
        filename: &str,
    ) -> Result<FileMetadata, StorageError> {
        let meta_path = self.delta_meta_path(prefix, filename);
        self.read_metadata(&meta_path).await
    }

    #[instrument(skip(self))]
    async fn delete_delta(&self, prefix: &str, filename: &str) -> Result<(), StorageError> {
        let data_path = self.delta_path(prefix, filename);
        let meta_path = self.delta_meta_path(prefix, filename);

        if !data_path.exists() {
            return Err(StorageError::NotFound(format!(
                "Delta: {}/{}",
                prefix, filename
            )));
        }

        fs::remove_file(&data_path).await?;
        if meta_path.exists() {
            fs::remove_file(&meta_path).await?;
        }

        debug!("Deleted delta for {}/{}", prefix, filename);
        Ok(())
    }

    // === Direct operations ===

    #[instrument(skip(self))]
    async fn get_direct(&self, prefix: &str, filename: &str) -> Result<Vec<u8>, StorageError> {
        let path = self.direct_path(prefix, filename);
        if !path.exists() {
            return Err(StorageError::NotFound(format!(
                "Direct: {}/{}",
                prefix, filename
            )));
        }
        let data = fs::read(&path).await?;
        debug!(
            "Read direct ({} bytes) for {}/{}",
            data.len(),
            prefix,
            filename
        );
        Ok(data)
    }

    #[instrument(skip(self, data, metadata))]
    async fn put_direct(
        &self,
        prefix: &str,
        filename: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        let data_path = self.direct_path(prefix, filename);
        let meta_path = self.direct_meta_path(prefix, filename);

        self.ensure_dir(&data_path).await?;
        fs::write(&data_path, data).await?;
        self.write_metadata(&meta_path, metadata).await?;

        debug!(
            "Wrote direct ({} bytes) for {}/{}",
            data.len(),
            prefix,
            filename
        );
        Ok(())
    }

    #[instrument(skip(self))]
    async fn get_direct_metadata(
        &self,
        prefix: &str,
        filename: &str,
    ) -> Result<FileMetadata, StorageError> {
        let meta_path = self.direct_meta_path(prefix, filename);
        self.read_metadata(&meta_path).await
    }

    #[instrument(skip(self))]
    async fn delete_direct(&self, prefix: &str, filename: &str) -> Result<(), StorageError> {
        let data_path = self.direct_path(prefix, filename);
        let meta_path = self.direct_meta_path(prefix, filename);

        if !data_path.exists() {
            return Err(StorageError::NotFound(format!(
                "Direct: {}/{}",
                prefix, filename
            )));
        }

        fs::remove_file(&data_path).await?;
        if meta_path.exists() {
            fs::remove_file(&meta_path).await?;
        }

        debug!("Deleted direct for {}/{}", prefix, filename);
        Ok(())
    }

    // === Scanning operations ===

    #[instrument(skip(self))]
    async fn scan_deltaspace(&self, prefix: &str) -> Result<Vec<FileMetadata>, StorageError> {
        let dir = self.deltaspace_dir(prefix);
        if !dir.exists() {
            return Ok(Vec::new());
        }

        let mut metadata_list = Vec::new();

        let mut entries = fs::read_dir(&dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();

            // Only process .meta files
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.ends_with(".meta") {
                    match self.read_metadata(&path).await {
                        Ok(meta) => metadata_list.push(meta),
                        Err(e) => {
                            debug!("Failed to read metadata {:?}: {}", path, e);
                        }
                    }
                }
            }
        }

        debug!(
            "Scanned {} objects in deltaspace {}",
            metadata_list.len(),
            prefix
        );
        Ok(metadata_list)
    }

    #[instrument(skip(self))]
    async fn list_deltaspaces(&self) -> Result<Vec<String>, StorageError> {
        let deltaspaces_dir = self.root.join("deltaspaces");
        if !deltaspaces_dir.exists() {
            return Ok(Vec::new());
        }

        let mut prefixes = Vec::new();
        // Recursively find all directories containing deltaglider files
        Self::find_deltaspaces_recursive(&deltaspaces_dir, &deltaspaces_dir, &mut prefixes).await?;

        Ok(prefixes)
    }

    async fn total_size(&self) -> Result<u64, StorageError> {
        self.dir_size(&self.root).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_put_get_raw() {
        let tmp = TempDir::new().unwrap();
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .unwrap();

        let data = b"hello world";
        backend
            .put_raw(Path::new("test/file.txt"), data)
            .await
            .unwrap();

        let retrieved = backend.get_raw(Path::new("test/file.txt")).await.unwrap();
        assert_eq!(retrieved, data);
    }

    #[tokio::test]
    async fn test_reference_with_metadata() {
        let tmp = TempDir::new().unwrap();
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .unwrap();

        let data = b"reference content";
        let metadata = FileMetadata::new_reference(
            "app.zip".to_string(),
            "releases/v1.0/app.zip".to_string(),
            "sha256hash".to_string(),
            "md5hash".to_string(),
            data.len() as u64,
            Some("application/zip".to_string()),
        );

        backend
            .put_reference("myprefix", data, &metadata)
            .await
            .unwrap();

        assert!(backend.has_reference("myprefix").await);

        let retrieved_data = backend.get_reference("myprefix").await.unwrap();
        assert_eq!(retrieved_data, data);

        let retrieved_meta = backend.get_reference_metadata("myprefix").await.unwrap();
        assert!(retrieved_meta.is_reference());
        assert_eq!(retrieved_meta.original_name, "app.zip");
    }

    #[tokio::test]
    async fn test_delta_with_metadata() {
        let tmp = TempDir::new().unwrap();
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .unwrap();

        let data = b"delta content";
        let metadata = FileMetadata::new_delta(
            "app.zip".to_string(),
            "sha256hash".to_string(),
            "md5hash".to_string(),
            1024,
            "releases/reference.bin".to_string(),
            "ref_sha256".to_string(),
            data.len() as u64,
            None,
        );

        backend
            .put_delta("myprefix", "app.zip", data, &metadata)
            .await
            .unwrap();

        let retrieved_data = backend.get_delta("myprefix", "app.zip").await.unwrap();
        assert_eq!(retrieved_data, data);

        let retrieved_meta = backend
            .get_delta_metadata("myprefix", "app.zip")
            .await
            .unwrap();
        assert!(retrieved_meta.is_delta());
    }

    #[tokio::test]
    async fn test_scan_deltaspace() {
        let tmp = TempDir::new().unwrap();
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .unwrap();

        // Add a reference
        let ref_meta = FileMetadata::new_reference(
            "base.zip".to_string(),
            "releases/base.zip".to_string(),
            "sha1".to_string(),
            "md5_1".to_string(),
            100,
            None,
        );
        backend
            .put_reference("releases", b"ref data", &ref_meta)
            .await
            .unwrap();

        // Add a delta
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
        backend
            .put_delta("releases", "v2.zip", b"delta data", &delta_meta)
            .await
            .unwrap();

        // Scan
        let all_meta = backend.scan_deltaspace("releases").await.unwrap();
        assert_eq!(all_meta.len(), 2);
    }

    #[tokio::test]
    async fn test_list_deltaspaces() {
        let tmp = TempDir::new().unwrap();
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .unwrap();

        // Create deltaspaces
        let meta = FileMetadata::new_direct(
            "file.txt".to_string(),
            "sha".to_string(),
            "md5".to_string(),
            10,
            None,
        );
        backend
            .put_direct("releases/v1", "file.txt", b"data", &meta)
            .await
            .unwrap();
        backend
            .put_direct("docs", "readme.txt", b"readme", &meta)
            .await
            .unwrap();

        let deltaspaces = backend.list_deltaspaces().await.unwrap();
        assert!(deltaspaces.contains(&"releases/v1".to_string()));
        assert!(deltaspaces.contains(&"docs".to_string()));
    }
}
