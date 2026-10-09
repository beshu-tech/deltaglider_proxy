// SPDX-License-Identifier: BUSL-1.1

//! Filesystem-based storage backend with xattr-based metadata

use super::traits::{
    BulkListing, DelegatedListResult, RefFence, RefWrite, StorageBackend, StorageError,
    StoredObject,
};
use super::xattr_meta;
use crate::types::FileMetadata;
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

#[cfg(test)]
pub(crate) mod fault;
#[cfg(test)]
mod tests;
mod write;

use write::*;

/// Prefix of this backend's own temp files (atomic write-then-rename).
const INTERNAL_TEMP_PREFIX: &str = ".dg-tmp.";

/// File that stores the folder marker `photos/` (review D3): the key's file
/// name is empty, and a directory cannot also be a file, so the marker lives
/// inside the directory under this reserved name.
const DIR_MARKER_FILE: &str = ".dg-folder-marker";

/// The file name part of the key that a data file named `name` stores:
/// empty for a folder marker, the name itself otherwise.
fn key_filename(name: &str) -> &str {
    if name == DIR_MARKER_FILE {
        ""
    } else {
        name
    }
}

/// Is `name` one of this backend's temp files, and so never a user object?
/// `.dg-tmp.*` (current) or tempfile's default `.tmpXXXXXX` (older
/// releases). Every other `.`-name (`.env`, `.gitignore`) is a user object:
/// LIST shows it and DeleteBucket must not erase it.
fn is_internal_temp_name(name: &str) -> bool {
    if name.starts_with(INTERNAL_TEMP_PREFIX) {
        return true;
    }
    name.strip_prefix(".tmp")
        .is_some_and(|r| r.len() == 6 && r.bytes().all(|b| b.is_ascii_alphanumeric()))
}

/// Refuse a prefix or filename that the OS path join would resolve to a
/// DIFFERENT key's file: a `.`, `..` or empty (`//`) segment. IAM authorizes
/// the literal key text, so `a/./secret` must not open `a/secret` (and no
/// such key can be stored distinctly on a filesystem anyway). S3 keeps keys
/// literal, so only this backend needs the rule.
fn check_path_segments(prefix: &str, filename: &str) -> Result<(), StorageError> {
    let aliases = |segment: &str| segment.is_empty() || segment == "." || segment == "..";
    let bad_prefix = !prefix.is_empty() && prefix.split('/').any(aliases);
    let bad_filename = !filename.is_empty() && (aliases(filename) || filename.contains('/'));
    if bad_prefix || bad_filename {
        return Err(StorageError::InvalidKey(
            "Key must not contain '.', '..' or empty path segments on the filesystem backend"
                .to_string(),
        ));
    }
    // A user file with the marker name would read back as the folder marker.
    if filename == DIR_MARKER_FILE {
        return Err(StorageError::InvalidKey(format!(
            "Key file name '{filename}' is reserved for folder markers on the filesystem backend"
        )));
    }
    // A user file with a temp-file name would be hidden and pruned as one.
    if is_internal_temp_name(filename) {
        return Err(StorageError::InvalidKey(format!(
            "Key file name '{filename}' is reserved for temp files on the filesystem backend"
        )));
    }
    Ok(())
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

    /// Get the full path for a deltaspace directory within a bucket.
    ///
    /// THE gate for key aliasing: every object path is built here (or by a
    /// listing join that calls [`check_path_segments`]), so no caller can
    /// reach a file through a key that names another key.
    fn deltaspace_dir(&self, bucket: &str, prefix: &str) -> Result<PathBuf, StorageError> {
        check_path_segments(prefix, "")?;
        Ok(if prefix.is_empty() {
            self.bucket_dir(bucket).join("deltaspaces")
        } else {
            self.bucket_dir(bucket).join("deltaspaces").join(prefix)
        })
    }

    /// Get the path for the reference file
    fn reference_path(&self, bucket: &str, prefix: &str) -> Result<PathBuf, StorageError> {
        Ok(self.deltaspace_dir(bucket, prefix)?.join("reference.bin"))
    }

    /// Get the path for a delta file
    fn delta_path(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<PathBuf, StorageError> {
        check_path_segments("", filename)?;
        Ok(self
            .deltaspace_dir(bucket, prefix)?
            .join(format!("{}.delta", filename)))
    }

    /// Get the path for a passthrough file (stored with original filename).
    /// An empty filename under a prefix is the folder marker `prefix/`.
    fn passthrough_path(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<PathBuf, StorageError> {
        check_path_segments("", filename)?;
        if filename.is_empty() {
            if prefix.is_empty() {
                return Err(StorageError::InvalidKey(
                    "Object key must not be empty".to_string(),
                ));
            }
            return Ok(self.deltaspace_dir(bucket, prefix)?.join(DIR_MARKER_FILE));
        }
        Ok(self.deltaspace_dir(bucket, prefix)?.join(filename))
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

        // A directory (a key prefix) or a path under a file is no object
        // (s3surface-2): without this a GET of `dir` answered 200 with the
        // directory's size and then failed mid-body.
        let stat = fs::metadata(path).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound || super::io_error_is_path_type_conflict(&e)
            {
                StorageError::NotFound(path.display().to_string())
            } else {
                StorageError::from(e)
            }
        })?;
        if !stat.is_file() {
            return Err(StorageError::NotFound(path.display().to_string()));
        }
        let modified: DateTime<Utc> = stat
            .modified()
            .map(DateTime::<Utc>::from)
            .unwrap_or_else(|_| Utc::now());

        let synthetic_etag = synthesise_unmanaged_etag(stat.len(), &modified);

        Ok(FileMetadata::fallback(
            key_filename(filename).to_string(),
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
    async fn ensure_dir(&self, bucket: &str, path: &Path) -> Result<WriteDir, StorageError> {
        let bucket_dir = self.bucket_dir(bucket);
        let dir = path.parent().unwrap_or(&bucket_dir).to_path_buf();
        if !is_dir(&bucket_dir).await {
            return Err(StorageError::BucketNotFound(bucket.to_string()));
        }
        let wd = WriteDir {
            bucket: bucket.to_string(),
            bucket_dir,
            dir,
        };
        tokio::task::spawn_blocking(move || {
            mkdir_within(&wd.bucket, &wd.bucket_dir, &wd.dir)?;
            Ok(wd)
        })
        .await
        .map_err(super::join_error)?
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
                    if !is_internal_temp_name(name) && name != "reference.bin" {
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
                    if is_internal_temp_name(name) || name == "reference.bin" {
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
                    if !is_internal_temp_name(name) {
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
        baselines: &'a mut Vec<(String, u64)>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), StorageError>> + Send + 'a>>
    {
        Box::pin(async move {
            let mut entries = fs::read_dir(current_dir).await?;
            while let Some(entry) = entries.next_entry().await? {
                let path = entry.path();
                let ft = entry.file_type().await?;
                if ft.is_dir() {
                    Self::bulk_walk_recursive(deltaspaces_dir, &path, results, baselines).await?;
                    continue;
                }

                let name = match path.file_name().and_then(|n| n.to_str()) {
                    Some(n) => n.to_string(),
                    None => continue,
                };

                // A baseline is never a user object; report its stored size
                // (one local stat, the walk already reads each file's xattrs).
                if name == "reference.bin" {
                    if let Ok(md) = entry.metadata().await {
                        let rel = path
                            .strip_prefix(deltaspaces_dir)
                            .unwrap_or(Path::new(&name))
                            .to_string_lossy()
                            .into_owned();
                        baselines.push((rel, md.len()));
                    }
                    continue;
                }
                // Skip this backend's temp files (not user objects)
                if is_internal_temp_name(&name) {
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

                // The marker file is the key `dir/`, whatever its xattr says.
                let key_name = if name == DIR_MARKER_FILE {
                    ""
                } else {
                    meta.original_name.as_str()
                };
                if dir_str.is_empty() && key_name.is_empty() {
                    continue;
                }
                let user_key = if dir_str.is_empty() {
                    key_name.to_string()
                } else {
                    format!("{}/{}", dir_str, key_name)
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
        durability: Durability,
    ) -> Result<(), StorageError> {
        let dir = self.ensure_dir(bucket, data_path).await?;
        atomic_write_with_metadata(dir, data_path, data, Some(metadata), durability).await?;
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

impl StorageBackend for FilesystemBackend {
    async fn flush_pending(&self) -> Result<(), StorageError> {
        tokio::task::spawn_blocking(flush_pending_fsync)
            .await
            .map_err(super::join_error)?
    }

    // === Bucket operations ===

    #[instrument(skip(self))]
    async fn create_bucket(&self, bucket: &str) -> Result<(), StorageError> {
        let bucket_dir = self.bucket_dir(bucket);
        fs::create_dir_all(&bucket_dir).await?;
        debug!("Created bucket directory: {:?}", bucket_dir);
        Ok(())
    }

    /// A declared filesystem bucket is just a directory — create it so its
    /// first write doesn't 404 (#63). Idempotent (`create_dir_all`).
    async fn ensure_declared_bucket(&self, bucket: &str) -> Result<(), StorageError> {
        self.create_bucket(bucket).await
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
            &self.reference_path(bucket, prefix)?,
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
        let src = self.reference_path(bucket, prefix)?;
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
        proof: &crate::deltaglider::RefWriteProof,
    ) -> Result<(), StorageError> {
        self.require_bucket_exists(bucket).await?;
        self.put_object_file(
            bucket,
            &self.reference_path(bucket, prefix)?,
            data,
            metadata,
            "reference",
            prefix,
            "reference.bin",
            Durability::Sync,
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
        proof: &crate::deltaglider::RefWriteProof,
    ) -> Result<(), StorageError> {
        self.require_bucket_exists(bucket).await?;
        let dest = self.reference_path(bucket, prefix)?;
        // `ensure_dir`, never `create_dir_all`: that re-created a bucket
        // deleted after `require_bucket_exists` (C-P0-1, storage-12).
        let dir = self.ensure_dir(bucket, &dest).await?;
        // Copy to a temp file + xattr + fsync + rename. Delete-then-copy lost
        // the baseline on a failed or short copy, and a hardlink shared the
        // inode (and so the xattr) with the caller's source file.
        atomic_copy_with_metadata(dir, source_path, &dest, metadata, Durability::Sync).await
    }

    async fn put_reference_metadata(
        &self,
        bucket: &str,
        prefix: &str,
        metadata: &FileMetadata,
        _proof: &crate::deltaglider::RefWriteProof,
    ) -> Result<(), StorageError> {
        self.require_bucket_exists(bucket).await?;
        xattr_meta::write_metadata(&self.reference_path(bucket, prefix)?, metadata).await
    }

    async fn put_passthrough_metadata(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        self.require_bucket_exists(bucket).await?;
        let path = self.passthrough_path(bucket, prefix, filename)?;
        if !path_exists(&path).await {
            return Err(StorageError::NotFound(format!(
                "{bucket}/{prefix}/{filename}"
            )));
        }
        // xattr write only — bytes and mtime untouched (setxattr changes
        // ctime, not mtime), so the served LastModified is stable.
        xattr_meta::write_metadata(&path, metadata).await
    }

    #[instrument(skip(self))]
    async fn get_reference_metadata(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<FileMetadata, StorageError> {
        let path = self.reference_path(bucket, prefix)?;
        match xattr_meta::read_metadata(&path).await {
            Ok(meta) => Ok(meta),
            Err(StorageError::NotFound(_)) => {
                // No xattr metadata — fall back to filesystem stats if the file exists.
                Self::fallback_metadata_from_path(&path, "reference.bin").await
            }
            Err(other) => Err(other),
        }
    }

    /// Existence only: the filesystem backend is single-node, so it never
    /// fences (a cross-instance reference lock needs a shared S3 backend).
    async fn reference_fence(&self, bucket: &str, prefix: &str) -> Result<RefFence, StorageError> {
        super::traits::unfenced_reference_fence(self, bucket, prefix).await
    }

    /// Ignores the fence and returns `Unfenced` (single node, see above).
    async fn write_reference_fenced(
        &self,
        bucket: &str,
        prefix: &str,
        op: RefWrite<'_>,
        _fence: &RefFence,
        proof: &crate::deltaglider::RefWriteProof,
    ) -> Result<RefFence, StorageError> {
        super::traits::unfenced_reference_write(self, bucket, prefix, op, proof).await
    }

    async fn has_reference(&self, bucket: &str, prefix: &str) -> Result<bool, StorageError> {
        // Local disk: a stat is either present or not; there is no transient
        // remote-throttle case to disambiguate.
        Ok(path_exists(&self.reference_path(bucket, prefix)?).await)
    }

    #[instrument(skip(self))]
    async fn delete_reference(
        &self,
        bucket: &str,
        prefix: &str,
        proof: &crate::deltaglider::RefWriteProof,
    ) -> Result<(), StorageError> {
        self.delete_object_file(
            &self.reference_path(bucket, prefix)?,
            &self.deltaspace_dir(bucket, "")?,
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
            &self.delta_path(bucket, prefix, filename)?,
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
        proof: &crate::deltaglider::RefWriteProof,
    ) -> Result<(), StorageError> {
        self.require_bucket_exists(bucket).await?;
        self.put_object_file(
            bucket,
            &self.delta_path(bucket, prefix, filename)?,
            data,
            metadata,
            "delta",
            prefix,
            filename,
            Durability::for_object(),
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
        let path = self.delta_path(bucket, prefix, filename)?;
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
            &self.delta_path(bucket, prefix, filename)?,
            &self.deltaspace_dir(bucket, "")?,
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
            &self.passthrough_path(bucket, prefix, filename)?,
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
            &self.passthrough_path(bucket, prefix, filename)?,
            data,
            metadata,
            "passthrough",
            prefix,
            filename,
            Durability::for_object(),
        )
        .await
    }

    #[instrument(skip(self, metadata, _spool))]
    async fn put_passthrough_file(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        source_path: &Path,
        metadata: &FileMetadata,
        _spool: crate::deltaglider::spool::SpoolBudget<'_>,
    ) -> Result<(), StorageError> {
        self.require_bucket_exists(bucket).await?;
        let data_path = self.passthrough_path(bucket, prefix, filename)?;
        let dir = self.ensure_dir(bucket, &data_path).await?;
        atomic_copy_with_metadata(
            dir,
            source_path,
            &data_path,
            metadata,
            Durability::for_object(),
        )
        .await?;
        debug!(
            "Copied passthrough file {:?} -> {:?} for {}/{}",
            source_path, data_path, prefix, filename
        );
        Ok(())
    }

    #[instrument(skip(self, part_paths, metadata, _spool))]
    async fn put_passthrough_parts(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        part_paths: &[PathBuf],
        metadata: &FileMetadata,
        _spool: crate::deltaglider::spool::SpoolBudget<'_>,
    ) -> Result<(), StorageError> {
        self.require_bucket_exists(bucket).await?;
        let data_path = self.passthrough_path(bucket, prefix, filename)?;
        let dir = self.ensure_dir(bucket, &data_path).await?;
        let target = data_path.clone();
        let parts: Vec<PathBuf> = part_paths.to_vec();
        let meta_json = serde_json::to_vec(metadata)?;
        let durability = Durability::for_object();

        tokio::task::spawn_blocking(move || {
            let mut tmp = dir.temp()?;
            for path in &parts {
                let mut src = std::fs::File::open(path).map_err(io_to_storage_error)?;
                std::io::copy(&mut src, &mut tmp).map_err(io_to_storage_error)?;
            }
            xattr_meta::set_metadata_xattr(tmp.path(), &meta_json)?;
            durable_rename(tmp, &target, durability)
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
        let path = self.passthrough_path(bucket, prefix, filename)?;
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
            &self.passthrough_path(bucket, prefix, filename)?,
            &self.deltaspace_dir(bucket, "")?,
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
        let data_path = self.passthrough_path(bucket, prefix, filename)?;

        let dir = self.ensure_dir(bucket, &data_path).await?;

        // Write chunks sequentially to a temp file, then fsync + rename.
        // This avoids allocating a contiguous buffer for the entire object.
        let target = data_path.clone();
        let chunks: Vec<Bytes> = chunks.to_vec();
        let num_chunks = chunks.len();
        let meta_json = serde_json::to_vec(metadata)?;
        let durability = Durability::for_object();

        tokio::task::spawn_blocking(move || -> Result<(), StorageError> {
            let mut tmp = dir.temp()?;
            for chunk in &chunks {
                tmp.write_all(chunk).map_err(io_to_storage_error)?;
            }
            // Write xattr before rename — atomic metadata+data visibility.
            xattr_meta::set_metadata_xattr(tmp.path(), &meta_json)?;
            durable_rename(tmp, &target, durability)
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

        let data_path = self.passthrough_path(bucket, prefix, filename)?;
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

    /// The metadata read (xattr, or the fallback of a file without it) and
    /// the file, opened once.
    async fn open_object(
        &self,
        bucket: &str,
        prefix: &str,
        object: StoredObject<'_>,
    ) -> Result<(super::ByteStream, FileMetadata), StorageError> {
        use futures::StreamExt;
        let (path, meta, label, name) = match object {
            StoredObject::Reference => (
                self.reference_path(bucket, prefix)?,
                self.get_reference_metadata(bucket, prefix).await?,
                "reference",
                "reference.bin",
            ),
            StoredObject::Delta(f) => (
                self.delta_path(bucket, prefix, f)?,
                self.get_delta_metadata(bucket, prefix, f).await?,
                "delta",
                f,
            ),
            StoredObject::Passthrough(f) => (
                self.passthrough_path(bucket, prefix, f)?,
                self.get_passthrough_metadata(bucket, prefix, f).await?,
                "passthrough",
                f,
            ),
        };
        let file = match tokio::fs::File::open(&path).await {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(StorageError::NotFound(format!("{label}: {prefix}/{name}")));
            }
            Err(e) => return Err(e.into()),
        };
        let stream = ReaderStream::new(file).map(|r| r.map_err(StorageError::Io));
        Ok((Box::pin(stream), meta))
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

        let data_path = self.passthrough_path(bucket, prefix, filename)?;
        if !path_exists(&data_path).await {
            return Err(StorageError::NotFound(format!(
                "passthrough: {}/{}",
                prefix, filename
            )));
        }

        let mut file = tokio::fs::File::open(&data_path).await?;
        let (start, end) = super::clamp_range(start, end, file.metadata().await?.len())?;
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
        let dir = self.deltaspace_dir(bucket, prefix)?;
        if !path_exists(&dir).await {
            return Ok(Vec::new());
        }

        let mut metadata_list = Vec::new();

        let mut entries = fs::read_dir(&dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();

            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                // Match data files: reference.bin, *.delta, or passthrough files (any other file)
                let is_data_file = !is_internal_temp_name(name);

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
                            // An object whose metadata cannot be read is
                            // still an object: listing it as one keeps
                            // reference reclaim from deleting reference.bin
                            // under a delta (fail closed).
                            tracing::warn!("Error reading xattr for {:?}: {}", path, e);
                            match Self::fallback_metadata_from_path(&path, name).await {
                                Ok(meta) => metadata_list.push(meta),
                                Err(_) => metadata_list.push(FileMetadata::fallback(
                                    name.to_string(),
                                    0,
                                    String::new(),
                                    chrono::Utc::now(),
                                    None,
                                    crate::types::StorageInfo::Passthrough,
                                )),
                            }
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

    async fn put_directory_marker(&self, bucket: &str, key: &str) -> Result<(), StorageError> {
        let obj = crate::types::ObjectKey::parse(bucket, key);
        if !obj.is_directory_marker() {
            return Err(StorageError::InvalidKey(format!(
                "not a folder marker key: {key}"
            )));
        }
        self.require_bucket_exists(bucket).await?;
        let path = self.passthrough_path(bucket, &obj.prefix, "")?;
        let mut meta = FileMetadata::directory_marker(key);
        // The key's file name, as for every other object on this backend.
        meta.original_name = String::new();
        self.put_object_file(
            bucket,
            &path,
            &[],
            &meta,
            "folder marker",
            &obj.prefix,
            "",
            Durability::Sync,
        )
        .await
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
        Ok(self
            .bulk_list_objects_with_baselines(bucket, prefix)
            .await?
            .objects)
    }

    async fn bulk_list_objects_with_baselines(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<BulkListing, StorageError> {
        let deltaspaces_dir = self.bucket_dir(bucket).join("deltaspaces");
        // An S3 prefix is a string, not a directory: `nightly/pg` must match
        // `nightly/pg-01.sql`. Walk the deepest directory the prefix names
        // completely, then keep the keys that start with the whole prefix.
        let dir_part = prefix.rfind('/').map_or("", |i| &prefix[..i]);
        // A `.`/empty segment would walk another directory and report its
        // keys under the alias.
        check_path_segments(dir_part, "")?;
        let walk_root = if dir_part.is_empty() {
            deltaspaces_dir.clone()
        } else {
            deltaspaces_dir.join(dir_part)
        };

        if !path_exists(&walk_root).await {
            return Ok(BulkListing::default());
        }

        let mut results: Vec<(String, FileMetadata)> = Vec::new();
        let mut baselines: Vec<(String, u64)> = Vec::new();
        Self::bulk_walk_recursive(&deltaspaces_dir, &walk_root, &mut results, &mut baselines)
            .await?;
        // One prefix rule for objects and baselines alike.
        results.retain(|(key, _)| key.starts_with(prefix));
        baselines.retain(|(key, _)| key.starts_with(prefix));

        debug!(
            "Bulk listed {} objects + {} baselines in {}/{}",
            results.len(),
            baselines.len(),
            bucket,
            prefix
        );
        Ok(BulkListing {
            objects: results,
            baselines,
        })
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

        // A `.`/empty segment would read another directory and list its keys
        // under the alias text, past a policy on the real prefix.
        check_path_segments(dir_part, "")?;
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

            if ft.is_dir() {
                // Hide only the `.dg` internal directory. Other dot-dirs
                // (`.well-known/`…) are legitimate user prefixes — parity with
                // the S3 backend and with this backend's own flat listing.
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
                // Temp files and reference.bin are internal, not objects.
                if is_internal_temp_name(&name) || name == "reference.bin" {
                    continue;
                }

                let is_delta = name.ends_with(".delta");
                let user_filename = if is_delta {
                    // Strip ".delta" suffix to get the user-visible name.
                    name[..name.len() - 6].to_string()
                } else {
                    key_filename(&name).to_string()
                };

                // Apply name filter. A marker file at the bucket root names
                // no key.
                if (!name_filter.is_empty() && !user_filename.starts_with(name_filter))
                    || (dir_part.is_empty() && user_filename.is_empty())
                {
                    continue;
                }

                // Build the full user-visible key (`dir/` for the marker).
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
