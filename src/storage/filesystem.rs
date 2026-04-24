//! Filesystem-based storage backend with xattr-based metadata

use super::traits::{DelegatedListResult, StorageBackend, StorageError};
use super::xattr_meta;
use crate::types::FileMetadata;
use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;
use tokio::fs;
use tokio_util::io::ReaderStream;
use tracing::{debug, instrument};

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

use super::io_to_storage_error;

/// Atomically write data + metadata to a file using write-to-temp + xattr + fsync + rename.
///
/// The xattr is written to the temp file BEFORE the rename, so a crash can never
/// leave a data file without its metadata. Either both are visible or neither is.
async fn atomic_write_with_metadata(
    path: &Path,
    data: &[u8],
    metadata: Option<&FileMetadata>,
) -> Result<(), StorageError> {
    let parent = path
        .parent()
        .ok_or_else(|| StorageError::Other("Cannot atomic-write to a path with no parent".into()))?
        .to_path_buf();
    let path = path.to_path_buf();
    let data = data.to_vec();
    let meta_json = metadata.map(serde_json::to_vec).transpose()?;

    tokio::task::spawn_blocking(move || {
        let mut tmp = NamedTempFile::new_in(&parent).map_err(io_to_storage_error)?;
        tmp.write_all(&data).map_err(io_to_storage_error)?;
        // Write xattr to temp file BEFORE rename — atomic metadata+data visibility.
        if let Some(json) = &meta_json {
            xattr::set(tmp.path(), xattr_meta::XATTR_NAME, json).map_err(io_to_storage_error)?;
        }
        tmp.as_file().sync_all().map_err(io_to_storage_error)?;
        tmp.persist(&path)
            .map_err(|e| io_to_storage_error(e.error))?;
        Ok(())
    })
    .await
    .map_err(super::join_error)?
}

/// Filesystem storage backend
///
/// Storage layout:
/// ```text
/// {root}/{bucket}/deltaspaces/{prefix}/
///   reference.bin         # Reference file data (metadata in xattr)
///   {name}.delta          # Delta file data (metadata in xattr)
///   {name}                # Passthrough file data with original name (metadata in xattr)
/// ```
///
/// Metadata is stored as a `user.dg.metadata` extended attribute on each
/// data file's inode — no sidecar `.meta` files needed.
///
/// Each bucket is a real subdirectory under the root.
pub struct FilesystemBackend {
    /// Root directory for all data
    root: PathBuf,
}

impl FilesystemBackend {
    /// Create a new filesystem backend with the given root directory.
    ///
    /// Validates xattr support at startup.
    pub async fn new(root: PathBuf) -> Result<Self, StorageError> {
        // Ensure root directory exists
        fs::create_dir_all(&root).await?;

        // Validate that the filesystem supports xattrs
        xattr_meta::validate_xattr_support(&root).await?;

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

    /// Get the path for a delta file
    fn delta_path(&self, bucket: &str, prefix: &str, filename: &str) -> PathBuf {
        self.deltaspace_dir(bucket, prefix)
            .join(format!("{}.delta", filename))
    }

    /// Get the path for a passthrough file (stored with original filename)
    fn passthrough_path(&self, bucket: &str, prefix: &str, filename: &str) -> PathBuf {
        self.deltaspace_dir(bucket, prefix).join(filename)
    }

    /// Build a best-effort FileMetadata from filesystem stats alone (no xattr).
    /// Used when a file exists but has no DeltaGlider metadata (unmanaged file).
    async fn fallback_metadata_from_path(
        path: &Path,
        filename: &str,
    ) -> Result<FileMetadata, StorageError> {
        use crate::types::StorageInfo;
        use chrono::{DateTime, Utc};

        let stat = fs::metadata(path).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                StorageError::NotFound(path.display().to_string())
            } else {
                StorageError::from(e)
            }
        })?;
        let modified: DateTime<Utc> = stat
            .modified()
            .map(DateTime::<Utc>::from)
            .unwrap_or_else(|_| Utc::now());
        Ok(FileMetadata::fallback(
            filename.to_string(),
            stat.len(),
            String::new(),
            modified,
            None,
            StorageInfo::Passthrough,
        ))
    }

    /// Ensure a directory exists
    async fn ensure_dir(&self, path: &Path) -> Result<(), StorageError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        Ok(())
    }

    /// Reject a write if the bucket root does NOT already exist. Prevents
    /// implicit bucket creation via PUT — the classic C2 security bug where
    /// `ensure_dir` + `create_dir_all` silently created `/<root>/<bucket>`
    /// as a side effect of any PUT. Callers: every `put_*` entry point.
    ///
    /// Handler-level `ensure_bucket_exists` (in `api::handlers::object_helpers`)
    /// catches the common case with a clean HTTP error; this guard is belt-
    /// and-braces for any future internal caller that forgets the precheck.
    async fn require_bucket_exists(&self, bucket: &str) -> Result<(), StorageError> {
        if !is_dir(&self.bucket_dir(bucket)).await {
            return Err(StorageError::BucketNotFound(bucket.to_string()));
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
                    // Any data file (reference, delta, or passthrough with original name)
                    // indicates this directory is an active deltaspace.
                    if name == "reference.bin" || name.ends_with(".delta") || !name.starts_with('.')
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

    /// Recursively walk directories, reading xattr metadata for each data file
    /// and producing (user_visible_key, FileMetadata) pairs in a single pass.
    fn bulk_walk_recursive<'a>(
        deltaspaces_dir: &'a Path,
        current_dir: &'a Path,
        results: &'a mut Vec<(String, FileMetadata)>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), StorageError>> + Send + 'a>>
    {
        Box::pin(async move {
            let mut entries = fs::read_dir(current_dir).await?;
            while let Some(entry) = entries.next_entry().await? {
                let path = entry.path();
                let ft = entry.file_type().await?;
                if ft.is_dir() {
                    Self::bulk_walk_recursive(deltaspaces_dir, &path, results).await?;
                    continue;
                }

                let name = match path.file_name().and_then(|n| n.to_str()) {
                    Some(n) => n.to_string(),
                    None => continue,
                };

                // Skip hidden files and internal reference files
                if name.starts_with('.') || name == "reference.bin" {
                    continue;
                }

                // Read xattr metadata, falling back to filesystem stats for unmanaged files
                let meta = match xattr_meta::read_metadata(&path).await {
                    Ok(m) => m,
                    Err(StorageError::NotFound(_)) => {
                        match Self::fallback_metadata_from_path(&path, &name).await {
                            Ok(m) => m,
                            Err(e) => {
                                debug!("Failed to read metadata for {:?}: {}", path, e);
                                continue;
                            }
                        }
                    }
                    Err(e) => {
                        debug!("Error reading xattr for {:?}: {}", path, e);
                        continue;
                    }
                };

                // Skip Reference storage info entries
                if matches!(
                    meta.storage_info,
                    crate::types::StorageInfo::Reference { .. }
                ) {
                    continue;
                }

                // Compute user-visible key from relative path
                let relative_dir = current_dir
                    .strip_prefix(deltaspaces_dir)
                    .unwrap_or(Path::new(""));
                let dir_str = relative_dir.to_string_lossy();

                let user_key = if dir_str.is_empty() {
                    meta.original_name.clone()
                } else {
                    format!("{}/{}", dir_str, meta.original_name)
                };

                results.push((user_key, meta));
            }
            Ok(())
        })
    }

    // === Private helpers to eliminate delta/passthrough duplication ===

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

    async fn put_object_file(
        &self,
        data_path: &Path,
        data: &[u8],
        metadata: &FileMetadata,
        label: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<(), StorageError> {
        self.ensure_dir(data_path).await?;
        atomic_write_with_metadata(data_path, data, Some(metadata)).await?;
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
        fs::remove_file(data_path).await?;
        debug!("Deleted {} for {}/{}", label, prefix, filename);
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
        let dated = self.list_buckets_with_dates().await?;
        Ok(dated.into_iter().map(|(name, _)| name).collect())
    }

    #[instrument(skip(self))]
    async fn list_buckets_with_dates(
        &self,
    ) -> Result<Vec<(String, chrono::DateTime<chrono::Utc>)>, StorageError> {
        let mut buckets = Vec::new();
        if !path_exists(&self.root).await {
            return Ok(buckets);
        }
        let mut entries = fs::read_dir(&self.root).await?;
        while let Some(entry) = entries.next_entry().await? {
            let ft = entry.file_type().await?;
            if ft.is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    let created = entry
                        .metadata()
                        .await
                        .ok()
                        .and_then(|m| m.created().ok().or_else(|| m.modified().ok()))
                        .map(chrono::DateTime::<chrono::Utc>::from)
                        .unwrap_or_else(chrono::Utc::now);
                    buckets.push((name.to_string(), created));
                }
            }
        }
        buckets.sort_by(|a, b| a.0.cmp(&b.0));
        debug!("Listed {} filesystem buckets", buckets.len());
        Ok(buckets)
    }

    #[instrument(skip(self))]
    async fn head_bucket(&self, bucket: &str) -> Result<bool, StorageError> {
        Ok(is_dir(&self.bucket_dir(bucket)).await)
    }

    // === Reference operations ===
    // Delegates to the shared get/put/delete_object_file helpers using
    // the fixed "reference.bin" filename, keeping the same error/debug
    // format as delta and passthrough operations.

    #[instrument(skip(self))]
    async fn get_reference(&self, bucket: &str, prefix: &str) -> Result<Vec<u8>, StorageError> {
        self.get_object_file(
            &self.reference_path(bucket, prefix),
            "reference",
            prefix,
            "reference.bin",
        )
        .await
    }

    #[instrument(skip(self, data, metadata))]
    async fn put_reference(
        &self,
        bucket: &str,
        prefix: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        self.require_bucket_exists(bucket).await?;
        self.put_object_file(
            &self.reference_path(bucket, prefix),
            data,
            metadata,
            "reference",
            prefix,
            "reference.bin",
        )
        .await
    }

    #[instrument(skip(self, metadata))]
    async fn put_reference_metadata(
        &self,
        bucket: &str,
        prefix: &str,
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        self.require_bucket_exists(bucket).await?;
        xattr_meta::write_metadata(&self.reference_path(bucket, prefix), metadata).await
    }

    #[instrument(skip(self))]
    async fn get_reference_metadata(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<FileMetadata, StorageError> {
        let path = self.reference_path(bucket, prefix);
        match xattr_meta::read_metadata(&path).await {
            Ok(meta) => Ok(meta),
            Err(StorageError::NotFound(_)) => {
                // No xattr metadata — fall back to filesystem stats if the file exists.
                Self::fallback_metadata_from_path(&path, "reference.bin").await
            }
            Err(other) => Err(other),
        }
    }

    async fn has_reference(&self, bucket: &str, prefix: &str) -> bool {
        path_exists(&self.reference_path(bucket, prefix)).await
    }

    #[instrument(skip(self))]
    async fn delete_reference(&self, bucket: &str, prefix: &str) -> Result<(), StorageError> {
        self.delete_object_file(
            &self.reference_path(bucket, prefix),
            "reference",
            prefix,
            "reference.bin",
        )
        .await
    }

    // === Delta operations ===

    #[instrument(skip(self))]
    async fn get_delta(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<Vec<u8>, StorageError> {
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
        self.require_bucket_exists(bucket).await?;
        self.put_object_file(
            &self.delta_path(bucket, prefix, filename),
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
        let path = self.delta_path(bucket, prefix, filename);
        match xattr_meta::read_metadata(&path).await {
            Ok(meta) => Ok(meta),
            Err(StorageError::NotFound(_)) => {
                // No xattr metadata — fall back to filesystem stats if the file exists.
                Self::fallback_metadata_from_path(&path, filename).await
            }
            Err(other) => Err(other),
        }
    }

    #[instrument(skip(self))]
    async fn delete_delta(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<(), StorageError> {
        self.delete_object_file(
            &self.delta_path(bucket, prefix, filename),
            "delta",
            prefix,
            filename,
        )
        .await
    }

    // === Passthrough operations (stored with original filename) ===

    #[instrument(skip(self))]
    async fn get_passthrough(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<Vec<u8>, StorageError> {
        self.get_object_file(
            &self.passthrough_path(bucket, prefix, filename),
            "passthrough",
            prefix,
            filename,
        )
        .await
    }

    #[instrument(skip(self, data, metadata))]
    async fn put_passthrough(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        self.require_bucket_exists(bucket).await?;
        self.put_object_file(
            &self.passthrough_path(bucket, prefix, filename),
            data,
            metadata,
            "passthrough",
            prefix,
            filename,
        )
        .await
    }

    #[instrument(skip(self))]
    async fn get_passthrough_metadata(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<FileMetadata, StorageError> {
        let path = self.passthrough_path(bucket, prefix, filename);
        match xattr_meta::read_metadata(&path).await {
            Ok(meta) => Ok(meta),
            Err(StorageError::NotFound(_)) => {
                // No xattr metadata — file may exist without DG metadata (unmanaged).
                // Fall back to filesystem stats if the file exists.
                Self::fallback_metadata_from_path(&path, filename).await
            }
            Err(other) => Err(other),
        }
    }

    #[instrument(skip(self))]
    async fn delete_passthrough(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<(), StorageError> {
        self.delete_object_file(
            &self.passthrough_path(bucket, prefix, filename),
            "passthrough",
            prefix,
            filename,
        )
        .await
    }

    // === Chunked write operations ===

    #[instrument(skip(self, chunks, metadata))]
    async fn put_passthrough_chunked(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        chunks: &[Bytes],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        self.require_bucket_exists(bucket).await?;
        let data_path = self.passthrough_path(bucket, prefix, filename);

        self.ensure_dir(&data_path).await?;

        // Write chunks sequentially to a temp file, then fsync + rename.
        // This avoids allocating a contiguous buffer for the entire object.
        let parent = data_path
            .parent()
            .ok_or_else(|| StorageError::Other("Cannot write to a path with no parent".into()))?
            .to_path_buf();
        let target = data_path.clone();
        let chunks: Vec<Bytes> = chunks.to_vec();
        let num_chunks = chunks.len();
        let meta_json = serde_json::to_vec(metadata)?;

        tokio::task::spawn_blocking(move || -> Result<(), StorageError> {
            let mut tmp = NamedTempFile::new_in(&parent).map_err(io_to_storage_error)?;
            for chunk in &chunks {
                tmp.write_all(chunk).map_err(io_to_storage_error)?;
            }
            // Write xattr before rename — atomic metadata+data visibility.
            xattr::set(tmp.path(), xattr_meta::XATTR_NAME, &meta_json)
                .map_err(io_to_storage_error)?;
            tmp.as_file().sync_all().map_err(io_to_storage_error)?;
            tmp.persist(&target)
                .map_err(|e| io_to_storage_error(e.error))?;
            Ok(())
        })
        .await
        .map_err(super::join_error)??;

        debug!(
            "Wrote passthrough chunked ({} chunks) for {}/{}",
            num_chunks, prefix, filename
        );
        Ok(())
    }

    // === Streaming operations ===

    #[instrument(skip(self))]
    async fn get_passthrough_stream(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<BoxStream<'static, Result<Bytes, StorageError>>, StorageError> {
        use futures::StreamExt;

        let data_path = self.passthrough_path(bucket, prefix, filename);
        if !path_exists(&data_path).await {
            return Err(StorageError::NotFound(format!(
                "passthrough: {}/{}",
                prefix, filename
            )));
        }

        let file = tokio::fs::File::open(&data_path).await?;
        let reader_stream = ReaderStream::new(file);
        let stream = reader_stream.map(|result| result.map_err(StorageError::Io));
        debug!(
            "Opened passthrough file stream for {}/{}/{}",
            bucket, prefix, filename
        );
        Ok(Box::pin(stream))
    }

    #[instrument(skip(self))]
    async fn get_passthrough_stream_range(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        start: u64,
        end: u64,
    ) -> Result<(BoxStream<'static, Result<Bytes, StorageError>>, u64), StorageError> {
        use futures::StreamExt;
        use tokio::io::{AsyncReadExt, AsyncSeekExt};

        let data_path = self.passthrough_path(bucket, prefix, filename);
        if !path_exists(&data_path).await {
            return Err(StorageError::NotFound(format!(
                "passthrough: {}/{}",
                prefix, filename
            )));
        }

        let mut file = tokio::fs::File::open(&data_path).await?;
        file.seek(std::io::SeekFrom::Start(start)).await?;
        let range_len = end - start + 1;
        let limited = file.take(range_len);
        let reader_stream = ReaderStream::new(limited);
        let stream = reader_stream.map(|result| result.map_err(StorageError::Io));
        debug!(
            "Opened passthrough range stream for {}/{}/{} (bytes {}-{})",
            bucket, prefix, filename, start, end
        );
        Ok((Box::pin(stream), range_len))
    }

    // === Scanning operations ===

    #[instrument(skip(self))]
    async fn scan_deltaspace(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<Vec<FileMetadata>, StorageError> {
        let dir = self.deltaspace_dir(bucket, prefix);
        if !path_exists(&dir).await {
            return Ok(Vec::new());
        }

        let mut metadata_list = Vec::new();

        let mut entries = fs::read_dir(&dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();

            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                // Match data files: reference.bin, *.delta, or passthrough files (any other file)
                let is_data_file =
                    name == "reference.bin" || name.ends_with(".delta") || !name.starts_with('.'); // passthrough files have original names

                if is_data_file {
                    match xattr_meta::read_metadata(&path).await {
                        Ok(meta) => metadata_list.push(meta),
                        Err(StorageError::NotFound(_)) => {
                            // No xattr — try filesystem stats for unmanaged files
                            if let Ok(meta) = Self::fallback_metadata_from_path(&path, name).await {
                                metadata_list.push(meta);
                            }
                        }
                        Err(e) => {
                            debug!("Error reading xattr for {:?}: {}", path, e);
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

    #[instrument(skip(self))]
    async fn bulk_list_objects(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<Vec<(String, FileMetadata)>, StorageError> {
        let deltaspaces_dir = self.bucket_dir(bucket).join("deltaspaces");
        let walk_root = if prefix.is_empty() {
            deltaspaces_dir.clone()
        } else {
            deltaspaces_dir.join(prefix)
        };

        if !path_exists(&walk_root).await {
            return Ok(Vec::new());
        }

        let mut results: Vec<(String, FileMetadata)> = Vec::new();
        Self::bulk_walk_recursive(&deltaspaces_dir, &walk_root, &mut results).await?;

        debug!(
            "Bulk listed {} objects in {}/{}",
            results.len(),
            bucket,
            prefix
        );
        Ok(results)
    }

    /// Optimised single-level listing for `delimiter = "/"`.
    ///
    /// Instead of recursively walking every subdirectory and then collapsing
    /// results in-memory, we do a single `read_dir` at the directory implied
    /// by `prefix` and classify entries into objects vs common-prefixes.
    #[instrument(skip(self))]
    async fn list_objects_delegated(
        &self,
        bucket: &str,
        prefix: &str,
        delimiter: &str,
        max_keys: u32,
        continuation_token: Option<&str>,
    ) -> Result<Option<DelegatedListResult>, StorageError> {
        // Only handle the "/" delimiter; fall back for anything else.
        if delimiter != "/" {
            return Ok(None);
        }

        let deltaspaces_dir = self.bucket_dir(bucket).join("deltaspaces");

        // Split prefix into (directory to read, filename filter).
        // e.g. "builds/v" → dir = "builds", filter = "v"
        // e.g. "builds/"  → dir = "builds", filter = ""
        // e.g. ""         → dir = "",        filter = ""
        let (dir_part, name_filter) = if prefix.is_empty() {
            ("", "")
        } else if let Some(idx) = prefix.rfind('/') {
            (&prefix[..idx], &prefix[idx + 1..])
        } else {
            // prefix has no slash → listing root with a name filter
            ("", prefix)
        };

        let read_dir_path = if dir_part.is_empty() {
            deltaspaces_dir.clone()
        } else {
            deltaspaces_dir.join(dir_part)
        };

        // Non-existent directory → empty result (not an error).
        if !path_exists(&read_dir_path).await {
            return Ok(Some(DelegatedListResult {
                objects: Vec::new(),
                common_prefixes: Vec::new(),
                is_truncated: false,
                next_continuation_token: None,
            }));
        }

        // Single-level read_dir.
        let mut entries = fs::read_dir(&read_dir_path).await?;

        // Collect common prefixes and candidate object files.
        // Use BTreeMap for objects keyed by user-visible key so that
        // delta+passthrough duplicates are resolved (delta wins).
        let mut common_prefixes = std::collections::BTreeSet::new();
        let mut object_map: BTreeMap<String, (PathBuf, bool)> = BTreeMap::new(); // key → (path, is_delta)

        while let Some(entry) = entries.next_entry().await? {
            let ft = entry.file_type().await?;
            let os_name = entry.file_name();
            let name = match os_name.to_str() {
                Some(n) => n.to_string(),
                None => continue,
            };

            // Skip hidden/internal entries.
            if name.starts_with('.') {
                continue;
            }

            if ft.is_dir() {
                // Skip the `.dg` internal directory (already caught by dot check
                // above, but be explicit for clarity).
                if name == ".dg" {
                    continue;
                }

                // Build user-visible common-prefix: dir_part + name + "/"
                let cp = if dir_part.is_empty() {
                    format!("{}/", name)
                } else {
                    format!("{}/{}/", dir_part, name)
                };

                // Apply name filter: the directory name must start with name_filter.
                if !name_filter.is_empty() && !name.starts_with(name_filter) {
                    continue;
                }

                common_prefixes.insert(cp);
            } else {
                // File — skip reference.bin.
                if name == "reference.bin" {
                    continue;
                }

                let is_delta = name.ends_with(".delta");
                let user_filename = if is_delta {
                    // Strip ".delta" suffix to get the user-visible name.
                    name[..name.len() - 6].to_string()
                } else {
                    name.clone()
                };

                // Apply name filter.
                if !name_filter.is_empty() && !user_filename.starts_with(name_filter) {
                    continue;
                }

                // Build the full user-visible key.
                let user_key = if dir_part.is_empty() {
                    user_filename
                } else {
                    format!("{}/{}", dir_part, user_filename)
                };

                // Dedup: prefer delta metadata over passthrough when both exist.
                match object_map.get(&user_key) {
                    Some((_, existing_is_delta)) => {
                        if is_delta && !existing_is_delta {
                            // Delta takes precedence over passthrough.
                            object_map.insert(user_key, (entry.path(), true));
                        }
                        // If existing is already delta, or both are passthrough, keep existing.
                    }
                    None => {
                        object_map.insert(user_key, (entry.path(), is_delta));
                    }
                }
            }
        }

        // Interleave objects and common prefixes for unified sort+pagination.
        // S3 ListObjectsV2 counts both objects and common prefixes toward max_keys.
        let obj_entries: Vec<(String, PathBuf)> = object_map
            .into_iter()
            .map(|(key, (path, _))| (key, path))
            .collect();
        let cp_entries: Vec<String> = common_prefixes.into_iter().collect();

        let page = crate::deltaglider::interleave_and_paginate(
            obj_entries,
            cp_entries,
            max_keys,
            continuation_token,
        );

        // Resolve metadata for object entries (after pagination to minimize I/O).
        let mut final_objects: Vec<(String, FileMetadata)> = Vec::new();

        for (key, path) in page.objects {
            let filename = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            let meta = match xattr_meta::read_metadata(&path).await {
                Ok(m) => m,
                Err(StorageError::NotFound(_)) => {
                    match Self::fallback_metadata_from_path(&path, filename).await {
                        Ok(m) => m,
                        Err(e) => {
                            debug!(
                                "Skipping {:?} in delegated list (metadata error): {}",
                                path, e
                            );
                            continue;
                        }
                    }
                }
                Err(e) => {
                    debug!("Skipping {:?} in delegated list (xattr error): {}", path, e);
                    continue;
                }
            };

            // Skip Reference storage info (should not appear as user objects).
            if matches!(
                meta.storage_info,
                crate::types::StorageInfo::Reference { .. }
            ) {
                continue;
            }

            final_objects.push((key, meta));
        }

        debug!(
            "Delegated list (fs): {} objects + {} prefixes in {}/{}",
            final_objects.len(),
            page.common_prefixes.len(),
            bucket,
            prefix
        );

        Ok(Some(DelegatedListResult {
            objects: final_objects,
            common_prefixes: page.common_prefixes,
            is_truncated: page.is_truncated,
            next_continuation_token: page.next_continuation_token,
        }))
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the filesystem backend guards that don't need a
    //! running proxy. Integration tests live in
    //! `tests/bucket_existence_test.rs`.
    use super::*;
    use crate::types::FileMetadata;

    /// Build a minimal FileMetadata for testing the put_* paths. The
    /// content is never read because put_* should fail before touching it.
    fn dummy_metadata(filename: &str) -> FileMetadata {
        FileMetadata::new_passthrough(
            filename.to_string(),
            "0".repeat(64),     // sha256 hex
            "0".repeat(32),     // md5 hex
            0,
            None,
        )
    }

    /// Direct StorageBackend test: put_passthrough to a missing bucket
    /// must fail with BucketNotFound, NOT silently create a bucket root.
    #[tokio::test]
    async fn test_require_bucket_exists_rejects_put_passthrough() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .expect("new backend");

        // Attempt to write without ever calling create_bucket.
        let err = backend
            .put_passthrough(
                "missing-bucket",
                "prefix",
                "file.bin",
                b"payload",
                &dummy_metadata("file.bin"),
            )
            .await
            .expect_err("must refuse");

        match err {
            StorageError::BucketNotFound(b) => assert_eq!(b, "missing-bucket"),
            other => panic!("expected BucketNotFound, got {:?}", other),
        }

        // The bucket directory must NOT have been created.
        assert!(
            !tmp.path().join("missing-bucket").exists(),
            "put_passthrough must not create the bucket root on failure"
        );
    }

    /// Same guard covers put_delta.
    #[tokio::test]
    async fn test_require_bucket_exists_rejects_put_delta() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .expect("new backend");

        let err = backend
            .put_delta(
                "ghost",
                "ns",
                "f.delta",
                b"x",
                &dummy_metadata("f.delta"),
            )
            .await
            .expect_err("must refuse");

        assert!(matches!(err, StorageError::BucketNotFound(_)));
        assert!(!tmp.path().join("ghost").exists());
    }

    /// Same guard covers put_reference.
    #[tokio::test]
    async fn test_require_bucket_exists_rejects_put_reference() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .expect("new backend");

        let err = backend
            .put_reference("ghost", "ns", b"ref", &dummy_metadata("reference.bin"))
            .await
            .expect_err("must refuse");

        assert!(matches!(err, StorageError::BucketNotFound(_)));
        assert!(!tmp.path().join("ghost").exists());
    }

    /// Same guard covers put_passthrough_chunked.
    #[tokio::test]
    async fn test_require_bucket_exists_rejects_put_passthrough_chunked() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .expect("new backend");

        let chunks = vec![Bytes::from_static(b"hello"), Bytes::from_static(b"world")];
        let err = backend
            .put_passthrough_chunked(
                "ghost",
                "ns",
                "chunky.bin",
                &chunks,
                &dummy_metadata("chunky.bin"),
            )
            .await
            .expect_err("must refuse");

        assert!(matches!(err, StorageError::BucketNotFound(_)));
        assert!(!tmp.path().join("ghost").exists());
    }

    /// After create_bucket, put_passthrough should succeed.
    #[tokio::test]
    async fn test_put_after_create_bucket_succeeds() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .expect("new backend");

        backend.create_bucket("real-bucket").await.expect("create");

        backend
            .put_passthrough(
                "real-bucket",
                "",
                "file.bin",
                b"payload",
                &dummy_metadata("file.bin"),
            )
            .await
            .expect("put after create should succeed");

        // File is under deltaspaces/ inside the bucket dir.
        assert!(tmp
            .path()
            .join("real-bucket")
            .join("deltaspaces")
            .join("file.bin")
            .exists());
    }
}
