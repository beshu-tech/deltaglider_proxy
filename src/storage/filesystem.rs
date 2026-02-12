//! Filesystem-based storage backend with per-file metadata sidecars

use super::traits::{StorageBackend, StorageError};
use bytes::Bytes;
use crate::types::FileMetadata;
use async_trait::async_trait;
use futures::stream::BoxStream;
use std::io::Write;
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;
use tokio::fs;
use tokio_util::io::ReaderStream;
use tracing::{debug, instrument, warn};

/// Async-safe path existence check (avoids blocking the Tokio runtime)
async fn path_exists(path: &Path) -> bool {
    fs::try_exists(path).await.unwrap_or(false)
}

/// Async-safe directory check
async fn is_dir(path: &Path) -> bool {
    fs::metadata(path)
        .await
        .map(|m| m.is_dir())
        .unwrap_or(false)
}

/// ENOSPC raw error code on Linux and macOS.
const ENOSPC: i32 = 28;

/// Convert an io::Error into StorageError, detecting disk-full (ENOSPC).
fn io_to_storage_error(e: std::io::Error) -> StorageError {
    if e.raw_os_error() == Some(ENOSPC) {
        StorageError::DiskFull
    } else {
        StorageError::Io(e)
    }
}

/// Atomically write data to a file using write-to-temp + fsync + rename.
async fn atomic_write(path: &Path, data: &[u8]) -> Result<(), StorageError> {
    let parent = path
        .parent()
        .ok_or_else(|| StorageError::Other("Cannot atomic-write to a path with no parent".into()))?
        .to_path_buf();
    let path = path.to_path_buf();
    let data = data.to_vec();

    tokio::task::spawn_blocking(move || {
        let mut tmp = NamedTempFile::new_in(&parent).map_err(io_to_storage_error)?;
        tmp.write_all(&data).map_err(io_to_storage_error)?;
        tmp.as_file().sync_all().map_err(io_to_storage_error)?;
        tmp.persist(&path)
            .map_err(|e| io_to_storage_error(e.error))?;
        Ok(())
    })
    .await
    .map_err(|e| StorageError::Other(format!("spawn_blocking join failed: {}", e)))?
}

/// Filesystem storage backend
///
/// Storage layout:
/// ```text
/// {root}/{bucket}/deltaspaces/{prefix}/
///   reference.bin         # Reference file data
///   reference.bin.meta    # Reference metadata (JSON)
///   {name}.delta          # Delta file data
///   {name}.delta.meta     # Delta metadata (JSON)
///   {name}.direct         # Direct file data
///   {name}.direct.meta    # Direct metadata (JSON)
/// ```
///
/// Each bucket is a real subdirectory under the root.
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

    /// Get the bucket directory
    fn bucket_dir(&self, bucket: &str) -> PathBuf {
        self.root.join(bucket)
    }

    /// Get the full path for a deltaspace directory within a bucket
    fn deltaspace_dir(&self, bucket: &str, prefix: &str) -> PathBuf {
        if prefix.is_empty() {
            self.bucket_dir(bucket).join("deltaspaces")
        } else {
            self.bucket_dir(bucket).join("deltaspaces").join(prefix)
        }
    }

    /// Get the path for the reference file
    fn reference_path(&self, bucket: &str, prefix: &str) -> PathBuf {
        self.deltaspace_dir(bucket, prefix).join("reference.bin")
    }

    /// Get the path for reference metadata
    fn reference_meta_path(&self, bucket: &str, prefix: &str) -> PathBuf {
        self.deltaspace_dir(bucket, prefix).join("reference.bin.meta")
    }

    /// Get the path for a delta file
    fn delta_path(&self, bucket: &str, prefix: &str, filename: &str) -> PathBuf {
        self.deltaspace_dir(bucket, prefix)
            .join(format!("{}.delta", filename))
    }

    /// Get the path for delta metadata
    fn delta_meta_path(&self, bucket: &str, prefix: &str, filename: &str) -> PathBuf {
        self.deltaspace_dir(bucket, prefix)
            .join(format!("{}.delta.meta", filename))
    }

    /// Get the path for a direct file
    fn direct_path(&self, bucket: &str, prefix: &str, filename: &str) -> PathBuf {
        self.deltaspace_dir(bucket, prefix)
            .join(format!("{}.direct", filename))
    }

    /// Get the path for direct metadata
    fn direct_meta_path(&self, bucket: &str, prefix: &str, filename: &str) -> PathBuf {
        self.deltaspace_dir(bucket, prefix)
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
        if is_dir(path).await {
            let mut entries = fs::read_dir(path).await?;
            while let Some(entry) = entries.next_entry().await? {
                let path = entry.path();
                let ft = entry.file_type().await?;
                if ft.is_dir() {
                    total += Box::pin(self.dir_size(&path)).await?;
                } else {
                    total += entry.metadata().await?.len();
                }
            }
        }
        Ok(total)
    }

    /// Log warnings for orphaned data files (data without corresponding metadata).
    pub async fn warn_orphaned_files(&self) {
        // Scan all bucket directories
        let mut entries = match fs::read_dir(&self.root).await {
            Ok(e) => e,
            Err(_) => return,
        };
        while let Some(entry) = entries.next_entry().await.unwrap_or(None) {
            let path = entry.path();
            if !is_dir(&path).await {
                continue;
            }
            let deltaspaces_dir = path.join("deltaspaces");
            if !path_exists(&deltaspaces_dir).await {
                continue;
            }
            let mut orphans = Vec::new();
            if let Err(e) = Self::find_orphans_recursive(&deltaspaces_dir, &mut orphans).await {
                warn!("Failed to scan for orphaned files: {}", e);
                continue;
            }
            for orphan in &orphans {
                warn!(
                    "Orphaned data file (no metadata): {} — likely from an interrupted write",
                    orphan.display()
                );
            }
            if !orphans.is_empty() {
                warn!(
                    "Found {} orphaned data file(s). These consume disk but are not served. \
                     They can be safely deleted.",
                    orphans.len()
                );
            }
        }
    }

    /// Recursively find data files that have no corresponding .meta sidecar
    fn find_orphans_recursive<'a>(
        dir: &'a Path,
        orphans: &'a mut Vec<PathBuf>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), StorageError>> + Send + 'a>>
    {
        Box::pin(async move {
            let mut entries = match fs::read_dir(dir).await {
                Ok(e) => e,
                Err(_) => return Ok(()),
            };

            while let Some(entry) = entries.next_entry().await? {
                let path = entry.path();
                let ft = entry.file_type().await?;
                if ft.is_dir() {
                    Self::find_orphans_recursive(&path, orphans).await?;
                } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    let is_data_file = name == "reference.bin"
                        || (name.ends_with(".delta") && !name.ends_with(".delta.meta"))
                        || (name.ends_with(".direct") && !name.ends_with(".direct.meta"));

                    if is_data_file {
                        let meta_path = path.with_extension(
                            path.extension()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .to_string()
                                + ".meta",
                        );
                        let meta_path = if name == "reference.bin" {
                            dir.join("reference.bin.meta")
                        } else {
                            meta_path
                        };
                        if !path_exists(&meta_path).await {
                            orphans.push(path);
                        }
                    }
                }
            }
            Ok(())
        })
    }

    /// Recursively find all deltaspaces (directories containing deltaglider files)
    fn find_deltaspaces_recursive<'a>(
        base_dir: &'a Path,
        current_dir: &'a Path,
        prefixes: &'a mut std::collections::HashSet<String>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), StorageError>> + Send + 'a>>
    {
        Box::pin(async move {
            let mut entries = fs::read_dir(current_dir).await?;
            let mut has_deltaglider_files = false;

            while let Some(entry) = entries.next_entry().await? {
                let path = entry.path();
                let ft = entry.file_type().await?;
                if ft.is_dir() {
                    Self::find_deltaspaces_recursive(base_dir, &path, prefixes).await?;
                } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if name == "reference.bin"
                        || name.ends_with(".delta")
                        || name.ends_with(".direct")
                        || name.ends_with(".meta")
                    {
                        has_deltaglider_files = true;
                    }
                }
            }

            if has_deltaglider_files {
                if let Ok(relative) = current_dir.strip_prefix(base_dir) {
                    prefixes.insert(relative.to_string_lossy().to_string());
                }
            }

            Ok(())
        })
    }

    // === Private helpers to eliminate delta/direct duplication ===

    async fn get_object_file(
        &self,
        data_path: &Path,
        label: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<Vec<u8>, StorageError> {
        if !path_exists(data_path).await {
            return Err(StorageError::NotFound(format!(
                "{}: {}/{}",
                label, prefix, filename
            )));
        }
        let data = fs::read(data_path).await?;
        debug!(
            "Read {} ({} bytes) for {}/{}",
            label,
            data.len(),
            prefix,
            filename
        );
        Ok(data)
    }

    #[allow(clippy::too_many_arguments)]
    async fn put_object_file(
        &self,
        data_path: &Path,
        meta_path: &Path,
        data: &[u8],
        metadata: &FileMetadata,
        label: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<(), StorageError> {
        self.ensure_dir(data_path).await?;
        atomic_write(data_path, data).await?;
        self.write_metadata(meta_path, metadata).await?;
        debug!(
            "Wrote {} ({} bytes) for {}/{}",
            label,
            data.len(),
            prefix,
            filename
        );
        Ok(())
    }

    async fn delete_object_file(
        &self,
        data_path: &Path,
        meta_path: &Path,
        label: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<(), StorageError> {
        if !path_exists(data_path).await {
            return Err(StorageError::NotFound(format!(
                "{}: {}/{}",
                label, prefix, filename
            )));
        }
        if path_exists(meta_path).await {
            fs::remove_file(meta_path).await?;
        }
        fs::remove_file(data_path).await?;
        debug!("Deleted {} for {}/{}", label, prefix, filename);
        Ok(())
    }

    /// Read metadata from a .meta file
    async fn read_metadata(&self, meta_path: &Path) -> Result<FileMetadata, StorageError> {
        if !path_exists(meta_path).await {
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
        atomic_write(meta_path, &data).await?;
        debug!("Wrote metadata to {:?}", meta_path);
        Ok(())
    }
}

#[async_trait]
impl StorageBackend for FilesystemBackend {
    // === Bucket operations ===

    #[instrument(skip(self))]
    async fn create_bucket(&self, bucket: &str) -> Result<(), StorageError> {
        let bucket_dir = self.bucket_dir(bucket);
        fs::create_dir_all(&bucket_dir).await?;
        debug!("Created bucket directory: {:?}", bucket_dir);
        Ok(())
    }

    #[instrument(skip(self))]
    async fn delete_bucket(&self, bucket: &str) -> Result<(), StorageError> {
        let bucket_dir = self.bucket_dir(bucket);
        if !path_exists(&bucket_dir).await {
            return Err(StorageError::BucketNotFound(bucket.to_string()));
        }
        // Check if bucket has any content
        let deltaspaces_dir = bucket_dir.join("deltaspaces");
        if path_exists(&deltaspaces_dir).await {
            let mut entries = fs::read_dir(&deltaspaces_dir).await?;
            if entries.next_entry().await?.is_some() {
                return Err(StorageError::BucketNotEmpty(bucket.to_string()));
            }
        }
        // Remove the bucket directory
        fs::remove_dir_all(&bucket_dir).await?;
        debug!("Deleted bucket directory: {:?}", bucket_dir);
        Ok(())
    }

    #[instrument(skip(self))]
    async fn list_buckets(&self) -> Result<Vec<String>, StorageError> {
        let mut buckets = Vec::new();
        if !path_exists(&self.root).await {
            return Ok(buckets);
        }
        let mut entries = fs::read_dir(&self.root).await?;
        while let Some(entry) = entries.next_entry().await? {
            let ft = entry.file_type().await?;
            if ft.is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    buckets.push(name.to_string());
                }
            }
        }
        buckets.sort();
        debug!("Listed {} filesystem buckets", buckets.len());
        Ok(buckets)
    }

    #[instrument(skip(self))]
    async fn head_bucket(&self, bucket: &str) -> Result<bool, StorageError> {
        Ok(is_dir(&self.bucket_dir(bucket)).await)
    }

    // === Reference operations ===

    #[instrument(skip(self))]
    async fn get_reference(&self, bucket: &str, prefix: &str) -> Result<Vec<u8>, StorageError> {
        let path = self.reference_path(bucket, prefix);
        if !path_exists(&path).await {
            return Err(StorageError::NotFound(format!(
                "Reference for {}/{}",
                bucket, prefix
            )));
        }
        let data = fs::read(&path).await?;
        debug!(
            "Read reference ({} bytes) for {}/{}",
            data.len(),
            bucket,
            prefix
        );
        Ok(data)
    }

    #[instrument(skip(self, data, metadata))]
    async fn put_reference(
        &self,
        bucket: &str,
        prefix: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        let data_path = self.reference_path(bucket, prefix);
        let meta_path = self.reference_meta_path(bucket, prefix);

        self.ensure_dir(&data_path).await?;
        atomic_write(&data_path, data).await?;
        self.write_metadata(&meta_path, metadata).await?;

        debug!(
            "Wrote reference ({} bytes) for {}/{}",
            data.len(),
            bucket,
            prefix
        );
        Ok(())
    }

    #[instrument(skip(self, metadata))]
    async fn put_reference_metadata(
        &self,
        bucket: &str,
        prefix: &str,
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        let meta_path = self.reference_meta_path(bucket, prefix);
        self.write_metadata(&meta_path, metadata).await
    }

    #[instrument(skip(self))]
    async fn get_reference_metadata(&self, bucket: &str, prefix: &str) -> Result<FileMetadata, StorageError> {
        let meta_path = self.reference_meta_path(bucket, prefix);
        self.read_metadata(&meta_path).await
    }

    async fn has_reference(&self, bucket: &str, prefix: &str) -> bool {
        path_exists(&self.reference_path(bucket, prefix)).await
    }

    #[instrument(skip(self))]
    async fn delete_reference(&self, bucket: &str, prefix: &str) -> Result<(), StorageError> {
        let data_path = self.reference_path(bucket, prefix);
        let meta_path = self.reference_meta_path(bucket, prefix);

        if !path_exists(&data_path).await {
            return Err(StorageError::NotFound(format!(
                "Reference for {}/{}",
                bucket, prefix
            )));
        }

        if path_exists(&meta_path).await {
            fs::remove_file(&meta_path).await?;
        }
        fs::remove_file(&data_path).await?;

        debug!("Deleted reference for {}/{}", bucket, prefix);
        Ok(())
    }

    // === Delta operations ===

    #[instrument(skip(self))]
    async fn get_delta(&self, bucket: &str, prefix: &str, filename: &str) -> Result<Vec<u8>, StorageError> {
        self.get_object_file(
            &self.delta_path(bucket, prefix, filename),
            "delta",
            prefix,
            filename,
        )
        .await
    }

    #[instrument(skip(self, data, metadata))]
    async fn put_delta(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        self.put_object_file(
            &self.delta_path(bucket, prefix, filename),
            &self.delta_meta_path(bucket, prefix, filename),
            data,
            metadata,
            "delta",
            prefix,
            filename,
        )
        .await
    }

    #[instrument(skip(self))]
    async fn get_delta_metadata(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<FileMetadata, StorageError> {
        self.read_metadata(&self.delta_meta_path(bucket, prefix, filename))
            .await
    }

    #[instrument(skip(self))]
    async fn delete_delta(&self, bucket: &str, prefix: &str, filename: &str) -> Result<(), StorageError> {
        self.delete_object_file(
            &self.delta_path(bucket, prefix, filename),
            &self.delta_meta_path(bucket, prefix, filename),
            "delta",
            prefix,
            filename,
        )
        .await
    }

    // === Direct operations ===

    #[instrument(skip(self))]
    async fn get_direct(&self, bucket: &str, prefix: &str, filename: &str) -> Result<Vec<u8>, StorageError> {
        self.get_object_file(
            &self.direct_path(bucket, prefix, filename),
            "direct",
            prefix,
            filename,
        )
        .await
    }

    #[instrument(skip(self, data, metadata))]
    async fn put_direct(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        self.put_object_file(
            &self.direct_path(bucket, prefix, filename),
            &self.direct_meta_path(bucket, prefix, filename),
            data,
            metadata,
            "direct",
            prefix,
            filename,
        )
        .await
    }

    #[instrument(skip(self))]
    async fn get_direct_metadata(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<FileMetadata, StorageError> {
        self.read_metadata(&self.direct_meta_path(bucket, prefix, filename))
            .await
    }

    #[instrument(skip(self))]
    async fn delete_direct(&self, bucket: &str, prefix: &str, filename: &str) -> Result<(), StorageError> {
        self.delete_object_file(
            &self.direct_path(bucket, prefix, filename),
            &self.direct_meta_path(bucket, prefix, filename),
            "direct",
            prefix,
            filename,
        )
        .await
    }

    // === Streaming operations ===

    #[instrument(skip(self))]
    async fn get_direct_stream(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<BoxStream<'static, Result<Bytes, StorageError>>, StorageError> {
        use futures::StreamExt;

        let data_path = self.direct_path(bucket, prefix, filename);
        if !path_exists(&data_path).await {
            return Err(StorageError::NotFound(format!(
                "direct: {}/{}",
                prefix, filename
            )));
        }

        let file = tokio::fs::File::open(&data_path).await?;
        let reader_stream = ReaderStream::new(file);
        let stream = reader_stream.map(|result| result.map_err(StorageError::Io));
        debug!(
            "Opened direct file stream for {}/{}/{}",
            bucket, prefix, filename
        );
        Ok(Box::pin(stream))
    }

    // === Scanning operations ===

    #[instrument(skip(self))]
    async fn scan_deltaspace(&self, bucket: &str, prefix: &str) -> Result<Vec<FileMetadata>, StorageError> {
        let dir = self.deltaspace_dir(bucket, prefix);
        if !path_exists(&dir).await {
            return Ok(Vec::new());
        }

        let mut metadata_list = Vec::new();

        let mut entries = fs::read_dir(&dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();

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
            "Scanned {} objects in deltaspace {}/{}",
            metadata_list.len(),
            bucket,
            prefix
        );
        Ok(metadata_list)
    }

    #[instrument(skip(self))]
    async fn list_deltaspaces(&self, bucket: &str) -> Result<Vec<String>, StorageError> {
        let deltaspaces_dir = self.bucket_dir(bucket).join("deltaspaces");
        if !path_exists(&deltaspaces_dir).await {
            return Ok(Vec::new());
        }

        let mut prefixes = std::collections::HashSet::new();
        Self::find_deltaspaces_recursive(&deltaspaces_dir, &deltaspaces_dir, &mut prefixes).await?;

        Ok(prefixes.into_iter().collect())
    }

    async fn total_size(&self, bucket: Option<&str>) -> Result<u64, StorageError> {
        if let Some(b) = bucket {
            self.dir_size(&self.bucket_dir(b)).await
        } else {
            self.dir_size(&self.root).await
        }
    }
}
