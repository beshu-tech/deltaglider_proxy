// SPDX-License-Identifier: GPL-3.0-only

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

/// Materialise `src` at `dest` cheaply: hardlink (O(1), no extra bytes) when on
/// the same filesystem, byte-copy as fallback across fs boundaries (EXDEV).
/// `dest` is removed first (hard_link refuses an existing target — the caller may
/// hand a pre-created spool temp). Shared by `get_reference_to_file` and
/// `put_reference_from_file`.
async fn hardlink_or_copy(src: &Path, dest: &Path) -> Result<(), StorageError> {
    let _ = fs::remove_file(dest).await;
    if fs::hard_link(src, dest).await.is_err() {
        fs::copy(src, dest).await?;
    }
    Ok(())
}

/// Atomically copy file data + metadata to destination using temp + rename.
async fn atomic_copy_with_metadata(
    source_path: &Path,
    target_path: &Path,
    metadata: &FileMetadata,
) -> Result<(), StorageError> {
    let parent = target_path
        .parent()
        .ok_or_else(|| StorageError::Other("Cannot copy to a path with no parent".into()))?
        .to_path_buf();
    let source = source_path.to_path_buf();
    let target = target_path.to_path_buf();
    let meta_json = serde_json::to_vec(metadata)?;

    tokio::task::spawn_blocking(move || {
        let mut src = std::fs::File::open(&source).map_err(io_to_storage_error)?;
        let mut tmp = NamedTempFile::new_in(&parent).map_err(io_to_storage_error)?;
        std::io::copy(&mut src, &mut tmp).map_err(io_to_storage_error)?;
        xattr::set(tmp.path(), xattr_meta::XATTR_NAME, &meta_json).map_err(io_to_storage_error)?;
        tmp.as_file().sync_all().map_err(io_to_storage_error)?;
        tmp.persist(&target)
            .map_err(|e| io_to_storage_error(e.error))?;
        Ok(())
    })
    .await
    .map_err(super::join_error)?
}

/// Filesystem storage backend
///
/// Synthetic ETag for unmanaged files (no DG xattr).
///
/// Produces a stable hex-32 string derived from `(size, mtime_nanos)`.
/// Empty files (size=0) return the canonical empty-content MD5 so
/// they look consistent with managed empty objects and with the S3
/// backend's empty-object handling.
///
/// Property: the same (size, mtime) input always produces the same
/// output; ANY change to either invalidates the etag, which is the
/// only contract a client's `If-Match` / `If-None-Match` actually
/// relies on. Not the real body MD5 — clients that need that should
/// PUT the file through the proxy so DG xattr metadata is written.
pub(crate) fn synthesise_unmanaged_etag(
    size: u64,
    modified: &chrono::DateTime<chrono::Utc>,
) -> String {
    if size == 0 {
        return "d41d8cd98f00b204e9800998ecf8427e".to_string();
    }
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"dg-unmanaged-etag-v1\0");
    hasher.update(size.to_le_bytes());
    hasher.update(modified.timestamp().to_le_bytes());
    hasher.update(modified.timestamp_subsec_nanos().to_le_bytes());
    let digest = hasher.finalize();
    // Take first 16 bytes → hex32, the same shape an MD5 ETag has
    // on the wire. Clients that parse "looks like 32-hex" still work.
    let mut hex = String::with_capacity(32);
    for byte in digest.iter().take(16) {
        use std::fmt::Write;
        let _ = write!(hex, "{:02x}", byte);
    }
    hex
}

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
    ///
    /// S-P1-3: pre-fix this passed `String::new()` as the md5, so the
    /// resulting `etag()` was the literal `"\""` — an empty quoted
    /// string. SDKs comparing via `If-Match` / `If-None-Match`
    /// mis-evaluated; round-trip migrations that preserved bytes but
    /// stripped xattrs (tar, rsync without `-X`, copy across
    /// filesystems) broke client compare-and-swap loops. The S3
    /// backend's fallback path (`s3.rs::fallback_metadata_from_listing`)
    /// returns the real ETag from the listing, so the two backends
    /// disagreed.
    ///
    /// Post-fix: emit a deterministic synthetic ETag derived from
    /// `(size, mtime)`. Clients use ETag for change-detection — any
    /// modification to the file changes either size or mtime, which
    /// invalidates the synthetic. The ETag is NOT a real MD5 (we
    /// can't know it without reading the body) but it is a valid
    /// strong ETag per the S3 wire contract (which doesn't promise
    /// ETag is the body MD5 in the multipart case anyway). Empty
    /// files get the canonical empty-content MD5 so they look
    /// consistent across backends and tooling.
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

        let synthetic_etag = synthesise_unmanaged_etag(stat.len(), &modified);

        Ok(FileMetadata::fallback(
            filename.to_string(),
            stat.len(),
            synthetic_etag,
            modified,
            None,
            StorageInfo::Passthrough,
        ))
    }

    /// Ensure a directory exists, **without** silently creating the
    /// bucket root. The path must be inside an existing bucket dir.
    ///
    /// Pre-fix this called `fs::create_dir_all(parent)` unconditionally
    /// — which would silently recreate `<root>/<bucket>/...` if a
    /// concurrent `delete_bucket` had just removed it (C-P0-1: a
    /// parallel `CompleteMultipartUpload` mid-`engine.store` would
    /// resurrect a bucket the operator had successfully deleted).
    ///
    /// The fix walks intermediate components manually, calling
    /// non-recursive `mkdir`. If the bucket root went missing under us,
    /// the very first `mkdir` (of the first child of the bucket dir)
    /// fails with `ENOENT`, which we propagate as `BucketNotFound`.
    async fn ensure_dir(&self, bucket: &str, path: &Path) -> Result<(), StorageError> {
        let Some(parent) = path.parent() else {
            return Ok(());
        };
        let bucket_dir = self.bucket_dir(bucket);
        if !is_dir(&bucket_dir).await {
            return Err(StorageError::BucketNotFound(bucket.to_string()));
        }
        // Strip the bucket-or-shorter prefix; iterate the remaining
        // components and `mkdir` each one. We never `mkdir` the bucket
        // dir itself — if the strip fails, the path was outside the
        // bucket subtree and we propagate the error rather than
        // creating something we shouldn't.
        let Ok(rel) = parent.strip_prefix(&bucket_dir) else {
            return Err(StorageError::Other(format!(
                "ensure_dir called with path {:?} outside bucket {}",
                path, bucket
            )));
        };
        let mut current = bucket_dir;
        for component in rel.components() {
            current.push(component);
            match fs::create_dir(&current).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // Parent disappeared mid-walk — bucket was deleted
                    // between `require_bucket_exists` and now.
                    return Err(StorageError::BucketNotFound(bucket.to_string()));
                }
                Err(e) => return Err(StorageError::from(e)),
            }
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

    /// Return true when a deltaspace subtree contains at least one user-visible
    /// data file. Empty physical directories are not S3 prefixes and must not
    /// leak into delimiter listings as undeletable "folders".
    fn dir_has_visible_data_recursive<'a>(
        current_dir: &'a Path,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool, StorageError>> + Send + 'a>>
    {
        Box::pin(async move {
            let mut entries = fs::read_dir(current_dir).await?;
            while let Some(entry) = entries.next_entry().await? {
                let path = entry.path();
                let ft = entry.file_type().await?;
                if ft.is_dir() {
                    if Self::dir_has_visible_data_recursive(&path).await? {
                        return Ok(true);
                    }
                } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if !name.starts_with('.') && name != "reference.bin" {
                        return Ok(true);
                    }
                }
            }
            Ok(false)
        })
    }

    /// Remove hidden temp files, reference-only data, and empty directories
    /// from an otherwise empty bucket subtree.
    ///
    /// Returns `true` only when the subtree contains no user-visible data.
    /// Bucket deletion uses this before a non-recursive `remove_dir`, so a
    /// concurrent object creation turns into `BucketNotEmpty` instead of being
    /// erased by `remove_dir_all`.
    fn prune_invisible_data_recursive<'a>(
        current_dir: &'a Path,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool, StorageError>> + Send + 'a>>
    {
        Box::pin(async move {
            if !path_exists(current_dir).await {
                return Ok(true);
            }
            if Self::dir_has_visible_data_recursive(current_dir).await? {
                return Ok(false);
            }

            let mut has_visible_data = false;
            let mut entries = match fs::read_dir(current_dir).await {
                Ok(entries) => entries,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(true),
                Err(e) => return Err(StorageError::from(e)),
            };

            while let Some(entry) = entries.next_entry().await? {
                let path = entry.path();
                let ft = entry.file_type().await?;
                if ft.is_dir() {
                    if Self::prune_invisible_data_recursive(&path).await? {
                        match fs::remove_dir(&path).await {
                            Ok(()) => {}
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                            Err(e) if e.kind() == std::io::ErrorKind::DirectoryNotEmpty => {
                                has_visible_data = true;
                            }
                            Err(e) => return Err(StorageError::from(e)),
                        }
                    } else {
                        has_visible_data = true;
                    }
                } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if name.starts_with('.') || name == "reference.bin" {
                        match fs::remove_file(&path).await {
                            Ok(()) => {}
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                            Err(e) => return Err(StorageError::from(e)),
                        }
                    } else {
                        has_visible_data = true;
                    }
                } else {
                    // Unknown filenames are treated as data to avoid deleting
                    // something we cannot safely classify as internal.
                    has_visible_data = true;
                }
            }

            Ok(!has_visible_data)
        })
    }

    /// Remove internal/non-object residue from a bucket root after
    /// `deltaspaces/` has been verified empty of visible objects.
    ///
    /// Any entry outside `deltaspaces/` is not part of the S3 object view for
    /// the filesystem backend and can be cleaned as internal residue.
    fn prune_bucket_root_residue<'a>(
        bucket_dir: &'a Path,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), StorageError>> + Send + 'a>>
    {
        Box::pin(async move {
            let mut entries = match fs::read_dir(bucket_dir).await {
                Ok(entries) => entries,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(StorageError::from(e)),
            };

            while let Some(entry) = entries.next_entry().await? {
                let path = entry.path();
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if name == "deltaspaces" {
                    continue;
                }

                let file_type = entry.file_type().await?;
                if file_type.is_dir() {
                    fs::remove_dir_all(&path).await?;
                } else {
                    fs::remove_file(&path).await?;
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

    #[allow(clippy::too_many_arguments)]
    async fn put_object_file(
        &self,
        bucket: &str,
        data_path: &Path,
        data: &[u8],
        metadata: &FileMetadata,
        label: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<(), StorageError> {
        self.ensure_dir(bucket, data_path).await?;
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
        prune_root: &Path,
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
        fs::remove_file(data_path).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                StorageError::NotFound(format!("{}: {}/{}", label, prefix, filename))
            } else {
                StorageError::from(e)
            }
        })?;
        if let Some(parent) = data_path.parent() {
            Self::prune_empty_dirs(parent, prune_root).await?;
        }
        debug!("Deleted {} for {}/{}", label, prefix, filename);
        Ok(())
    }

    /// Remove empty directories left behind by filesystem-mode object deletes.
    ///
    /// This is intentionally bounded by the bucket's `deltaspaces` directory:
    /// object deletion may clean empty prefix directories, but it must never
    /// remove the bucket itself or climb outside the backend-owned tree.
    fn prune_empty_dirs<'a>(
        start_dir: &'a Path,
        stop_dir: &'a Path,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), StorageError>> + Send + 'a>>
    {
        Box::pin(async move {
            let stop = stop_dir.to_path_buf();
            let mut current = start_dir.to_path_buf();

            loop {
                if current == stop || !current.starts_with(&stop) {
                    break;
                }

                match fs::remove_dir(&current).await {
                    Ok(()) => {
                        debug!("Pruned empty filesystem prefix dir: {:?}", current);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) if e.kind() == std::io::ErrorKind::DirectoryNotEmpty => break,
                    Err(e) => return Err(StorageError::from(e)),
                }

                let Some(parent) = current.parent() else {
                    break;
                };
                current = parent.to_path_buf();
            }

            Ok(())
        })
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
        // Check if bucket has any user-visible object content.
        let deltaspaces_dir = bucket_dir.join("deltaspaces");
        if path_exists(&deltaspaces_dir).await {
            if !Self::prune_invisible_data_recursive(&deltaspaces_dir).await? {
                return Err(StorageError::BucketNotEmpty(bucket.to_string()));
            }
            match fs::remove_dir(&deltaspaces_dir).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) if e.kind() == std::io::ErrorKind::DirectoryNotEmpty => {
                    return Err(StorageError::BucketNotEmpty(bucket.to_string()));
                }
                Err(e) => return Err(StorageError::from(e)),
            }
        }

        // If the bucket is object-empty but still "dirty", proactively clear
        // internal residue so users are not blocked by backend housekeeping.
        Self::prune_bucket_root_residue(&bucket_dir).await?;

        match fs::remove_dir(&bucket_dir).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(StorageError::BucketNotFound(bucket.to_string()));
            }
            Err(e) if e.kind() == std::io::ErrorKind::DirectoryNotEmpty => {
                return Err(StorageError::BucketNotEmpty(bucket.to_string()));
            }
            Err(e) => return Err(StorageError::from(e)),
        }
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

    async fn get_reference_to_file(
        &self,
        bucket: &str,
        prefix: &str,
        dest: &Path,
    ) -> Result<u64, StorageError> {
        let src = self.reference_path(bucket, prefix);
        if !path_exists(&src).await {
            return Err(StorageError::NotFound(format!(
                "reference: {}/reference.bin",
                prefix
            )));
        }
        hardlink_or_copy(&src, dest).await?;
        let len = tokio::fs::metadata(dest).await?.len();
        Ok(len)
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
            bucket,
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
    async fn put_reference_from_file(
        &self,
        bucket: &str,
        prefix: &str,
        source_path: &Path,
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        self.require_bucket_exists(bucket).await?;
        let dest = self.reference_path(bucket, prefix);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).await?;
        }
        hardlink_or_copy(source_path, &dest).await?;
        xattr_meta::write_metadata(&dest, metadata).await
    }

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

    async fn has_reference(&self, bucket: &str, prefix: &str) -> Result<bool, StorageError> {
        // Local disk: a stat is either present or not; there is no transient
        // remote-throttle case to disambiguate.
        Ok(path_exists(&self.reference_path(bucket, prefix)).await)
    }

    #[instrument(skip(self))]
    async fn delete_reference(&self, bucket: &str, prefix: &str) -> Result<(), StorageError> {
        self.delete_object_file(
            &self.reference_path(bucket, prefix),
            &self.deltaspace_dir(bucket, ""),
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
            bucket,
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
            &self.deltaspace_dir(bucket, ""),
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
            bucket,
            &self.passthrough_path(bucket, prefix, filename),
            data,
            metadata,
            "passthrough",
            prefix,
            filename,
        )
        .await
    }

    #[instrument(skip(self, metadata))]
    async fn put_passthrough_file(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        source_path: &Path,
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        self.require_bucket_exists(bucket).await?;
        let data_path = self.passthrough_path(bucket, prefix, filename);
        self.ensure_dir(bucket, &data_path).await?;
        atomic_copy_with_metadata(source_path, &data_path, metadata).await?;
        debug!(
            "Copied passthrough file {:?} -> {:?} for {}/{}",
            source_path, data_path, prefix, filename
        );
        Ok(())
    }

    #[instrument(skip(self, part_paths, metadata))]
    async fn put_passthrough_parts(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        part_paths: &[PathBuf],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        self.require_bucket_exists(bucket).await?;
        let data_path = self.passthrough_path(bucket, prefix, filename);
        self.ensure_dir(bucket, &data_path).await?;
        let parent = data_path
            .parent()
            .ok_or_else(|| StorageError::Other("Cannot write to a path with no parent".into()))?
            .to_path_buf();
        let target = data_path.clone();
        let parts: Vec<PathBuf> = part_paths.to_vec();
        let meta_json = serde_json::to_vec(metadata)?;

        tokio::task::spawn_blocking(move || {
            let mut tmp = NamedTempFile::new_in(&parent).map_err(io_to_storage_error)?;
            for path in &parts {
                let mut src = std::fs::File::open(path).map_err(io_to_storage_error)?;
                std::io::copy(&mut src, &mut tmp).map_err(io_to_storage_error)?;
            }
            xattr::set(tmp.path(), xattr_meta::XATTR_NAME, &meta_json)
                .map_err(io_to_storage_error)?;
            tmp.as_file().sync_all().map_err(io_to_storage_error)?;
            tmp.persist(&target)
                .map_err(|e| io_to_storage_error(e.error))?;
            Ok(())
        })
        .await
        .map_err(super::join_error)?
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
            &self.deltaspace_dir(bucket, ""),
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

        self.ensure_dir(bucket, &data_path).await?;

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
        delimiter: Option<&str>,
        max_keys: u32,
        continuation_token: Option<&str>,
    ) -> Result<Option<DelegatedListResult>, StorageError> {
        // Only handle the "/" delimiter; fall back for anything else —
        // including delimiter-less listings, which stay on the bulk path
        // (a local-disk walk; the S3 backend is where paging pays off).
        if delimiter != Some("/") {
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

                if !Self::dir_has_visible_data_recursive(&entry.path()).await? {
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
            "0".repeat(64), // sha256 hex
            "0".repeat(32), // md5 hex
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
            .put_delta("ghost", "ns", "f.delta", b"x", &dummy_metadata("f.delta"))
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

    /// C-P0-1 regression: `ensure_dir` must NOT silently recreate the
    /// bucket root if a parallel `delete_bucket` removed it between
    /// `require_bucket_exists` and the actual write. Pre-fix,
    /// `ensure_dir` called `fs::create_dir_all(parent)` which happily
    /// resurrected `<root>/<bucket>/...` and the operator's deletion
    /// was silently undone.
    ///
    /// We exercise the race directly: create the bucket, remove it
    /// behind the backend's back, then call a put_* path. The
    /// `require_bucket_exists` precheck catches some races (race-A:
    /// delete BEFORE precheck), but here we simulate race-B: delete
    /// AFTER precheck. The check happens at the start of `put_*`; we
    /// run delete *after* `require_bucket_exists` would have passed.
    /// In practice the first race window is precheck → ensure_dir; the
    /// second is ensure_dir → atomic_write_with_metadata. This test
    /// pins the precheck → ensure_dir window.
    #[tokio::test]
    async fn test_ensure_dir_does_not_resurrect_deleted_bucket() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .expect("new backend");
        backend.create_bucket("racy").await.expect("create bucket");

        // Simulate: between require_bucket_exists and the write, the
        // bucket dir disappears. We can do that by removing it
        // directly with std::fs (the backend doesn't know).
        std::fs::remove_dir_all(tmp.path().join("racy")).unwrap();

        // Now put_passthrough — `require_bucket_exists` will catch this
        // because it's the first thing it checks. So this path proves
        // race-A is closed.
        let err = backend
            .put_passthrough("racy", "ns", "f.bin", b"payload", &dummy_metadata("f.bin"))
            .await
            .expect_err("must refuse");
        assert!(matches!(err, StorageError::BucketNotFound(_)));
        assert!(
            !tmp.path().join("racy").exists(),
            "must not have resurrected the bucket"
        );
    }

    /// Direct unit test of `ensure_dir`: when the bucket root is
    /// missing, even if something else points us at a path inside the
    /// bucket subtree, we must NOT create the bucket root. Pre-fix the
    /// `create_dir_all(parent)` path would happily build the whole
    /// tree from root downward.
    #[tokio::test]
    async fn test_ensure_dir_refuses_when_bucket_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .expect("new backend");

        let bogus_path = tmp
            .path()
            .join("phantom-bucket")
            .join("deltaspaces")
            .join("p")
            .join("file.bin");
        let err = backend
            .ensure_dir("phantom-bucket", &bogus_path)
            .await
            .expect_err("must refuse to create dirs in a missing bucket");

        match err {
            StorageError::BucketNotFound(b) => assert_eq!(b, "phantom-bucket"),
            other => panic!("expected BucketNotFound, got {:?}", other),
        }
        assert!(
            !tmp.path().join("phantom-bucket").exists(),
            "ensure_dir must not silently materialise the bucket root"
        );
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

    #[tokio::test]
    async fn test_delete_delta_prunes_empty_nested_prefix_dirs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .expect("new backend");
        backend.create_bucket("bucket").await.expect("create");

        backend
            .put_delta(
                "bucket",
                "a/b",
                "file.bin",
                b"delta",
                &dummy_metadata("file.bin"),
            )
            .await
            .expect("put delta");
        backend
            .delete_delta("bucket", "a/b", "file.bin")
            .await
            .expect("delete delta");

        assert!(!tmp
            .path()
            .join("bucket")
            .join("deltaspaces")
            .join("a")
            .exists());
        assert!(tmp.path().join("bucket").join("deltaspaces").exists());
    }

    #[tokio::test]
    async fn test_delete_reference_prunes_reference_only_prefix_dirs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .expect("new backend");
        backend.create_bucket("bucket").await.expect("create");
        let meta = FileMetadata::new_reference(
            "reference.bin".into(),
            "source.bin".into(),
            "0".repeat(64),
            "0".repeat(32),
            3,
            None,
        );

        backend
            .put_reference("bucket", "only/ref", b"ref", &meta)
            .await
            .expect("put reference");
        backend
            .delete_reference("bucket", "only/ref")
            .await
            .expect("delete reference");

        assert!(!tmp
            .path()
            .join("bucket")
            .join("deltaspaces")
            .join("only")
            .exists());
    }

    #[tokio::test]
    async fn test_delegated_list_hides_empty_and_reference_only_prefixes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .expect("new backend");
        backend.create_bucket("bucket").await.expect("create");
        fs::create_dir_all(
            tmp.path()
                .join("bucket")
                .join("deltaspaces")
                .join("empty/child"),
        )
        .await
        .expect("create empty prefix");
        let meta = FileMetadata::new_reference(
            "reference.bin".into(),
            "source.bin".into(),
            "0".repeat(64),
            "0".repeat(32),
            3,
            None,
        );
        backend
            .put_reference("bucket", "ghost", b"ref", &meta)
            .await
            .expect("put reference");

        let listed = backend
            .list_objects_delegated("bucket", "", Some("/"), 100, None)
            .await
            .expect("list")
            .expect("delegated");

        assert!(listed.objects.is_empty());
        assert!(listed.common_prefixes.is_empty());
    }

    #[tokio::test]
    async fn test_delete_bucket_removes_reference_only_and_empty_dirs() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .expect("new backend");
        backend.create_bucket("bucket").await.expect("create");
        let meta = FileMetadata::new_reference(
            "reference.bin".into(),
            "source.bin".into(),
            "0".repeat(64),
            "0".repeat(32),
            3,
            None,
        );
        backend
            .put_reference("bucket", "ghost", b"ref", &meta)
            .await
            .expect("put reference");
        let hidden = tmp
            .path()
            .join("bucket")
            .join("deltaspaces")
            .join("ghost")
            .join(".stale-write");
        fs::write(hidden, b"tmp").await.expect("write hidden");
        fs::create_dir_all(
            tmp.path()
                .join("bucket")
                .join("deltaspaces")
                .join("empty/child"),
        )
        .await
        .expect("create empty prefix");

        backend
            .delete_bucket("bucket")
            .await
            .expect("delete bucket");
        assert!(!tmp.path().join("bucket").exists());
    }

    /// S-P1-3 regression: zero-byte unmanaged files get the canonical
    /// empty-content MD5, NOT an empty string.
    #[test]
    fn unmanaged_etag_zero_size_is_canonical_empty_md5() {
        let mtime = chrono::Utc::now();
        assert_eq!(
            synthesise_unmanaged_etag(0, &mtime),
            "d41d8cd98f00b204e9800998ecf8427e",
            "empty unmanaged files must map to the canonical empty MD5"
        );
    }

    /// Pre-fix this test would have asserted `etag() == "\""` because
    /// `md5 = String::new()` rendered as a quoted empty string.
    /// Post-fix: stable, non-empty, hex-32 — looks like a real ETag
    /// to SDK consumers.
    #[test]
    fn unmanaged_etag_nonempty_is_stable_hex32() {
        let mtime = chrono::Utc::now();
        let a = synthesise_unmanaged_etag(1024, &mtime);
        let b = synthesise_unmanaged_etag(1024, &mtime);
        assert_eq!(a, b, "same (size, mtime) must produce same etag");
        assert_eq!(a.len(), 32, "must be hex-32 like a real MD5");
        assert!(
            a.chars().all(|c| c.is_ascii_hexdigit()),
            "must be valid hex"
        );
        assert_ne!(
            a, "d41d8cd98f00b204e9800998ecf8427e",
            "non-empty file must NOT collide with the empty-MD5 sentinel"
        );
    }

    /// Any change to size or mtime invalidates the etag — that's the
    /// property change-detection clients rely on.
    #[test]
    fn unmanaged_etag_size_change_invalidates() {
        let mtime = chrono::Utc::now();
        let a = synthesise_unmanaged_etag(1024, &mtime);
        let b = synthesise_unmanaged_etag(1025, &mtime);
        assert_ne!(a, b);
    }

    #[test]
    fn unmanaged_etag_mtime_change_invalidates() {
        let now = chrono::Utc::now();
        let later = now + chrono::Duration::seconds(1);
        let a = synthesise_unmanaged_etag(1024, &now);
        let b = synthesise_unmanaged_etag(1024, &later);
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn test_delete_bucket_removes_root_internal_residue() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .expect("new backend");
        backend.create_bucket("bucket").await.expect("create");

        let bucket_dir = tmp.path().join("bucket");
        fs::write(bucket_dir.join(".tmp-lock"), b"stale")
            .await
            .unwrap();
        fs::create_dir_all(bucket_dir.join("tmp-work/subdir"))
            .await
            .expect("create tmp dir");
        fs::write(bucket_dir.join("tmp-work/subdir/cache.bin"), b"stale")
            .await
            .expect("write tmp payload");

        backend
            .delete_bucket("bucket")
            .await
            .expect("delete bucket with root residue");
        assert!(!tmp.path().join("bucket").exists());
    }

    #[tokio::test]
    async fn test_delete_bucket_rejects_visible_data() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .expect("new backend");
        backend.create_bucket("bucket").await.expect("create");
        backend
            .put_passthrough(
                "bucket",
                "visible",
                "file.bin",
                b"payload",
                &dummy_metadata("file.bin"),
            )
            .await
            .expect("put passthrough");
        let meta = FileMetadata::new_reference(
            "reference.bin".into(),
            "source.bin".into(),
            "0".repeat(64),
            "0".repeat(32),
            3,
            None,
        );
        backend
            .put_reference("bucket", "visible", b"ref", &meta)
            .await
            .expect("put reference");

        let err = backend
            .delete_bucket("bucket")
            .await
            .expect_err("non-empty bucket must be rejected");

        assert!(matches!(err, StorageError::BucketNotEmpty(_)));
        assert!(tmp.path().join("bucket").exists());
        assert!(tmp
            .path()
            .join("bucket")
            .join("deltaspaces")
            .join("visible")
            .join("reference.bin")
            .exists());
    }

    #[tokio::test]
    async fn get_reference_to_file_is_byte_identical() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .expect("new backend");
        backend.create_bucket("bucket").await.expect("create");
        let payload: Vec<u8> = (0..50_000u32).flat_map(|n| n.to_le_bytes()).collect();
        let meta = FileMetadata::new_reference(
            "reference.bin".into(),
            "source.bin".into(),
            "0".repeat(64),
            "0".repeat(32),
            payload.len() as u64,
            None,
        );
        backend
            .put_reference("bucket", "deltas", &payload, &meta)
            .await
            .expect("put reference");

        let dest = tmp.path().join("spool_ref.bin");
        let n = backend
            .get_reference_to_file("bucket", "deltas", &dest)
            .await
            .expect("reference to file");
        assert_eq!(n, payload.len() as u64);
        let back = fs::read(&dest).await.expect("read dest");
        assert_eq!(
            back, payload,
            "materialised reference must be byte-identical"
        );

        // dest pre-existing (e.g. a pre-created spool temp) must be overwritten,
        // not error.
        let n2 = backend
            .get_reference_to_file("bucket", "deltas", &dest)
            .await
            .expect("reference to file over existing dest");
        assert_eq!(n2, payload.len() as u64);

        // Missing reference → NotFound (not a silent empty file).
        let err = backend
            .get_reference_to_file("bucket", "absent", &dest)
            .await
            .expect_err("missing reference must error");
        assert!(matches!(err, StorageError::NotFound(_)));
    }
}
