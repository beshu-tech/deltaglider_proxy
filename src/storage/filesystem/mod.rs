// SPDX-License-Identifier: BUSL-1.1

//! Filesystem-based storage backend with xattr-based metadata

use super::traits::{
    BulkListing, CopyOutcome, DelegatedListResult, ObjectAt, ObjectVariant, PutBody, RefFence,
    RefWrite, StorageBackend, StorageBackendExt, StorageError, StoredObject, StoredWrite,
};
use super::xattr_meta;
use crate::types::FileMetadata;
use bytes::Bytes;
use std::io::Write;
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;
use tokio::fs;
use tokio_util::io::ReaderStream;
use tracing::{debug, instrument};

use super::io_to_storage_error;

#[cfg(test)]
pub(crate) mod fault;
mod fsio;
#[cfg(test)]
mod list_memory_tests;
mod ref_walk;
mod scan;
mod sweep;
#[cfg(test)]
mod tests;
mod write;

use scan::{join_key, FlatWalk, SortedDir, WalkEntry, WalkStep};
use write::*;

pub use sweep::{sweep_stale_temp_files, STALE_TEMP_MAX_AGE};

/// Prefix of this backend's own temp files (atomic write-then-rename).
const INTERNAL_TEMP_PREFIX: &str = crate::types::TEMP_FILE_PREFIX;

/// File that stores the folder marker `photos/` (review D3): the key's file
/// name is empty, and a directory cannot also be a file, so the marker lives
/// inside the directory under this reserved name.
const DIR_MARKER_FILE: &str = crate::types::FOLDER_MARKER_FILE;

/// The file name part of the key that a data file named `name` stores:
/// empty for a folder marker, the name itself otherwise.
fn key_filename(name: &str, names: Names) -> &str {
    if names.is_marker(name) {
        ""
    } else {
        name
    }
}

/// How this root names the proxy's own files. On a case-SENSITIVE root only
/// the exact names are the proxy's; on a case-INSENSITIVE one a case variant
/// is the same file (`Reference.Bin` opens `reference.bin`), so it is the
/// proxy's too: a listing hides it and never enters a `.Dg` directory, the
/// same rule as [`check_path_segments`] (the one rule `types::reserved_name`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Names {
    case_insensitive: bool,
}

impl Names {
    /// The exact-name rule of a case-sensitive root.
    pub(crate) const EXACT: Self = Self {
        case_insensitive: false,
    };

    pub(crate) fn new(case_insensitive: bool) -> Self {
        Self { case_insensitive }
    }

    fn is(self, name: &str, exact: &str, kind: crate::types::ReservedName) -> bool {
        if self.case_insensitive {
            crate::types::reserved_name(name) == Some(kind)
        } else {
            name == exact
        }
    }

    /// The proxy's own `.dg` directory.
    pub(crate) fn is_internal_dir(self, name: &str) -> bool {
        self.is(
            name,
            crate::storage::listing_facts::INTERNAL_DIR,
            crate::types::ReservedName::InternalDir,
        )
    }

    /// This backend's temp file (`.dg-tmp.*`).
    pub(crate) fn is_temp(self, name: &str) -> bool {
        if self.case_insensitive {
            crate::types::reserved_name(name) == Some(crate::types::ReservedName::TempFile)
        } else {
            name.starts_with(INTERNAL_TEMP_PREFIX)
        }
    }

    fn is_marker(self, name: &str) -> bool {
        self.is(
            name,
            DIR_MARKER_FILE,
            crate::types::ReservedName::FolderMarker,
        )
    }

    fn is_reference(self, name: &str) -> bool {
        self.is(
            name,
            crate::types::REFERENCE_FILE_NAME,
            crate::types::ReservedName::Baseline,
        )
    }

    /// The stem of a stored delta (`a.zip` of `a.zip.delta`).
    fn delta_stem(self, name: &str) -> Option<&str> {
        if !self.case_insensitive {
            return name.strip_suffix(".delta");
        }
        let cut = name.len().checked_sub(crate::types::DELTA_SUFFIX.len())?;
        let (stem, suffix) = (name.get(..cut)?, name.get(cut..)?);
        suffix
            .eq_ignore_ascii_case(crate::types::DELTA_SUFFIX)
            .then_some(stem)
    }
}

/// The file's mtime: the backend's own time of the last write of the
/// stored object ([`FileMetadata::stored_last_modified`]). `None` when the
/// filesystem does not report one.
fn mtime_of(stat: &std::fs::Metadata) -> Option<chrono::DateTime<chrono::Utc>> {
    stat.modified()
        .ok()
        .map(chrono::DateTime::<chrono::Utc>::from)
}

/// The file's version ([`FileMetadata::backend_version`]): its inode, its
/// mtime in nanoseconds and its size. Every write of an object stores a new
/// file and renames it over the old one (`write.rs`), so a write changes
/// the inode and the mtime. `None` when the filesystem names no mtime.
pub(crate) fn version_of(stat: &std::fs::Metadata) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let mtime = stat.modified().ok()?;
    let nanos = mtime.duration_since(std::time::UNIX_EPOCH).ok()?.as_nanos();
    Some(format!("fs:{:x}:{nanos:x}:{:x}", stat.ino(), stat.len()))
}

/// Set the read-side facts of a stored file on `meta`: its mtime
/// ([`FileMetadata::stored_last_modified`]) and its version
/// ([`FileMetadata::backend_version`]).
fn stamp_stored(meta: &mut FileMetadata, stat: Option<&std::fs::Metadata>) {
    meta.stored_last_modified = stat.and_then(mtime_of);
    meta.backend_version = stat.and_then(version_of);
}

/// Metadata read by [`FilesystemBackend::object_metadata`], named for the
/// key: the key's file name wins over what the xattr says (review D4); an
/// unmanaged file's stat metadata is named already.
fn keyed((meta, from_xattr): (FileMetadata, bool), filename: &str) -> FileMetadata {
    if from_xattr {
        FileMetadata {
            original_name: filename.to_string(),
            ..meta
        }
    } else {
        meta
    }
}

/// What a file of a deltaspace directory stores, by its name (review D4:
/// a listing takes the key and the kind from the stored name, never from
/// the metadata inside, which a copy can clone from another file).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StoredName<'a> {
    /// This backend's temp file: no object.
    Internal,
    /// The deltaspace's delta reference.
    Reference,
    /// An object, whose key ends in `key_filename` (empty: the folder
    /// marker).
    Object {
        key_filename: &'a str,
        variant: super::traits::ObjectVariant,
    },
}

fn stored_name(name: &str, names: Names) -> StoredName<'_> {
    use super::traits::ObjectVariant;
    if names.is_temp(name) {
        StoredName::Internal
    } else if names.is_reference(name) {
        StoredName::Reference
    } else if let Some(stem) = names.delta_stem(name) {
        StoredName::Object {
            key_filename: stem,
            variant: ObjectVariant::Delta,
        }
    } else {
        StoredName::Object {
            key_filename: key_filename(name, names),
            variant: ObjectVariant::Passthrough,
        }
    }
}

/// Pure: can a subtree whose keys all start with `below` (`a/b/`) hold a
/// key that starts with `prefix` and sorts after `token`? A key below it
/// sorts before `token` when `token` sorts after `below` and does not start
/// with it: they differ inside `below`, where the key is smaller.
fn flat_subtree_may_match(below: &str, prefix: &str, token: &str) -> bool {
    let in_prefix = below.starts_with(prefix) || prefix.starts_with(below);
    let after_token = token <= below || token.starts_with(below);
    in_prefix && after_token
}

#[cfg(test)]
thread_local! {
    /// Metadata reads of listed files ([`FilesystemBackend::listed_entry`])
    /// on this thread, for the listing cost tests (a `#[tokio::test]` runs
    /// on one thread).
    static LISTED_ENTRY_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Directory reads of [`FilesystemBackend::dir_has_visible_data_recursive`]
    /// on this thread.
    static VISIBILITY_DIR_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// The most directory entries (or listed objects) that one listing
    /// request held in memory at once on this thread
    /// ([`note_held`]); the memory-bound tests read it.
    static HELD_PEAK: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Test seam: a listing holds `n` entries now. Keeps the peak.
#[cfg(test)]
fn note_held(n: usize) {
    HELD_PEAK.with(|p| p.set(p.get().max(n)));
}
#[cfg(not(test))]
fn note_held(_n: usize) {}

/// Is `name` one of this backend's temp files (`.dg-tmp.*`), and so never a
/// user object? Every other `.`-name (`.env`, `.gitignore`, `.tmpAb12Cd`) is
/// a user object: LIST shows it and DeleteBucket must not erase it.
pub(crate) fn is_internal_temp_name(name: &str) -> bool {
    Names::EXACT.is_temp(name)
}

/// Is `name` the proxy's own `.dg` entry (`names`: the root's case mode)? A
/// walk never reads it: no client key lives below it, and DeleteBucket removes it.
pub(crate) fn is_internal_dir_name(name: &str, names: Names) -> bool {
    names.is_internal_dir(name)
}

/// Does a listing prefix name a place below a `.dg` directory? Such a
/// prefix lists nothing.
fn prefix_is_below_internal_dir(dir_part: &str) -> bool {
    crate::storage::listing_facts::has_internal_segment(dir_part)
}

/// Refuse a prefix or filename that the OS path join would resolve to a
/// DIFFERENT key's file: a `.`, `..` or empty (`//`) segment. IAM authorizes
/// the literal key text, so `a/./secret` must not open `a/secret` (and no
/// such key can be stored distinctly on a filesystem anyway). S3 keeps keys
/// literal, so only this backend needs the rule.
///
/// The proxy's own names: on a case-SENSITIVE disk only the exact names are
/// the proxy's (`.dg-folder-marker`, `.dg-tmp.*`), and `Reference.Bin` or
/// `a/.Dg/x`, which an earlier release stored, are ordinary objects. On a
/// case-INSENSITIVE disk a case variant IS the proxy's file, so it is refused
/// by the one rule of key validation (`types::reserved_name`).
fn check_path_segments(
    prefix: &str,
    filename: &str,
    case_insensitive: bool,
) -> Result<(), StorageError> {
    let aliases = |segment: &str| segment.is_empty() || segment == "." || segment == "..";
    let bad_prefix = !prefix.is_empty() && prefix.split('/').any(aliases);
    let bad_filename = !filename.is_empty() && (aliases(filename) || filename.contains('/'));
    if bad_prefix || bad_filename {
        return Err(StorageError::InvalidKey(
            "Key must not contain '.', '..' or empty path segments on the filesystem backend"
                .to_string(),
        ));
    }
    if case_insensitive {
        if let Some(kind) = crate::types::reserved_name(filename) {
            return Err(StorageError::InvalidKey(format!(
                "Key file name '{filename}' is reserved on the filesystem backend: {}",
                kind.why()
            )));
        }
        // The exact `.dg` has its own handling (a listing of it is empty).
        if let Some(segment) = prefix.split('/').find(|s| {
            *s != crate::storage::listing_facts::INTERNAL_DIR
                && crate::types::reserved_name(s) == Some(crate::types::ReservedName::InternalDir)
        }) {
            return Err(StorageError::InvalidKey(format!(
                "Key folder name '{segment}' is reserved on the filesystem backend: {}",
                crate::types::ReservedName::InternalDir.why()
            )));
        }
    } else if filename == DIR_MARKER_FILE || is_internal_temp_name(filename) {
        // A user file with such a name would read back as the marker or be pruned as a temp file.
        return Err(StorageError::InvalidKey(format!(
            "Key file name '{filename}' is reserved on the filesystem backend"
        )));
    }
    Ok(())
}

/// Pure: the first folder of `prefix` whose name is a file that the proxy
/// keeps in its parent folder: a baseline, a delta, a temp file or a folder
/// marker, in the exact case of a case-sensitive root and in any case of a
/// case-insensitive one (`names`). Such a folder would sit where that file
/// goes, or be swept as a temp file.
fn colliding_folder(prefix: &str, names: Names) -> Option<&str> {
    prefix.split('/').find(|segment| {
        names.is_reference(segment)
            || names.delta_stem(segment).is_some()
            || names.is_temp(segment)
            || names.is_marker(segment)
    })
}

/// Does the filesystem at `dir` treat names that differ only in ASCII case
/// as one name? Creates a temp file and looks for it under the other case.
fn probe_case_insensitive(dir: &Path) -> std::io::Result<bool> {
    let name = format!(".dg-case-probe-{}", uuid::Uuid::new_v4().simple());
    let lower = dir.join(&name);
    std::fs::File::create_new(&lower)?;
    // fsio-exempt: a start-up probe of a temp file that this function made
    let aliased = dir.join(name.to_ascii_uppercase()).exists();
    let _ = std::fs::remove_file(&lower); // fsio-exempt: same probe file
    Ok(aliased)
}

/// Pure: a failed probe counts as case-insensitive, the safe side (a case
/// variant of a reserved name stays refused).
fn treat_as_case_insensitive(probe: &std::io::Result<bool>) -> bool {
    probe.as_ref().map_or(true, |insensitive| *insensitive)
}

/// Pure: the start-up warning for a data directory on a case-insensitive
/// filesystem (`probe` is its probe result), or `None`. Never fatal: the
/// proxy refuses reserved names in any case, but `A.zip` and `a.zip` are one
/// object there while S3 clients see two keys.
fn case_insensitive_warning(root: &Path, probe: &std::io::Result<bool>) -> Option<String> {
    matches!(probe, Ok(true)).then(|| {
        format!(
            "[storage] the data directory '{}' is on a case-insensitive filesystem: keys that \
             differ only in case (`A.zip`, `a.zip`) share one file, which S3 clients do not expect. \
             Use a case-sensitive filesystem for production data.",
            root.display()
        )
    })
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
    /// The data directory ignores letter case: a case variant of a reserved
    /// name is then the proxy's own file ([`check_path_segments`]).
    case_insensitive: bool,
}

impl FilesystemBackend {
    /// Create a new filesystem backend with the given root directory.
    ///
    /// Validates xattr support at startup.
    pub async fn new(root: PathBuf) -> Result<Self, StorageError> {
        // Ensure root directory exists
        fs::create_dir_all(&root).await.map_err(|e| {
            std::io::Error::new(
                e.kind(),
                format!("cannot create the data directory '{}': {e}", root.display()),
            )
        })?;

        // Validate that the filesystem supports xattrs
        xattr_meta::validate_xattr_support(&root).await?;

        let probe = probe_case_insensitive(&root);
        if let Some(warning) = case_insensitive_warning(&root, &probe) {
            tracing::warn!("{warning}");
        }
        let case_insensitive = treat_as_case_insensitive(&probe);

        Ok(Self {
            root,
            case_insensitive,
        })
    }

    /// How this root names the proxy's own files (its case mode).
    fn names(&self) -> Names {
        Names::new(self.case_insensitive)
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
        check_path_segments(prefix, "", self.case_insensitive)?;
        Ok(if prefix.is_empty() {
            self.bucket_dir(bucket).join("deltaspaces")
        } else {
            self.bucket_dir(bucket).join("deltaspaces").join(prefix)
        })
    }

    /// Store `object` from a local file of any size: copied to a temp file
    /// in the destination folder with its xattr, then renamed into place
    /// (`durability` says when it is synced). Never loaded into memory, and
    /// never a hardlink: the stored file must not share an inode (and so
    /// its xattr) with the caller's file.
    #[instrument(skip(self, metadata))]
    async fn put_object_from_file(
        &self,
        bucket: &str,
        prefix: &str,
        object: StoredObject<'_>,
        source_path: &Path,
        metadata: &FileMetadata,
        durability: Durability,
    ) -> Result<(), StorageError> {
        self.require_bucket_exists(bucket).await?;
        let (data_path, label, _) = self.object_path(bucket, prefix, object)?;
        // `ensure_dir`, never `create_dir_all`: that re-created a bucket
        // deleted after `require_bucket_exists` (C-P0-1, storage-12).
        let dir = self.ensure_dir(bucket, &data_path)?;
        atomic_copy_with_metadata(dir, source_path, &data_path, metadata, durability).await?;
        debug!("Copied {label} file {source_path:?} -> {data_path:?}");
        Ok(())
    }

    /// `put_object` of `object` from ordered part files.
    #[instrument(skip(self, part_paths, metadata))]
    async fn put_object_from_parts(
        &self,
        bucket: &str,
        prefix: &str,
        object: StoredObject<'_>,
        part_paths: &[PathBuf],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        self.require_bucket_exists(bucket).await?;
        let (data_path, _, _) = self.object_path(bucket, prefix, object)?;
        let dir = self.ensure_dir(bucket, &data_path)?;
        let target = data_path.clone();
        let parts: Vec<PathBuf> = part_paths.to_vec();
        let meta_json = serde_json::to_vec(metadata)?;
        let durability = Durability::for_object();

        let (_gate, ticket) = write::commit_gate();
        tokio::task::spawn_blocking(move || {
            let mut tmp = dir.temp()?;
            for path in &parts {
                // fsio-exempt: a write source, not a probe of the store.
                let mut src = std::fs::File::open(path).map_err(io_to_storage_error)?;
                std::io::copy(&mut src, &mut tmp).map_err(io_to_storage_error)?;
            }
            xattr_meta::set_metadata_xattr(tmp.path(), &meta_json)?;
            durable_rename(tmp, &target, durability, ticket)
        })
        .await
        .map_err(super::join_error)?
    }

    /// `put_object` of `object` from chunks, without a contiguous buffer.
    #[instrument(skip(self, chunks, metadata))]
    async fn put_object_from_chunks(
        &self,
        bucket: &str,
        prefix: &str,
        object: StoredObject<'_>,
        chunks: &[Bytes],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        self.require_bucket_exists(bucket).await?;
        let (data_path, label, filename) = self.object_path(bucket, prefix, object)?;

        let dir = self.ensure_dir(bucket, &data_path)?;

        // Write chunks sequentially to a temp file, then fsync + rename.
        // This avoids allocating a contiguous buffer for the entire object.
        let target = data_path.clone();
        let chunks: Vec<Bytes> = chunks.to_vec();
        let num_chunks = chunks.len();
        let meta_json = serde_json::to_vec(metadata)?;
        let durability = Durability::for_object();

        let (_gate, ticket) = write::commit_gate();
        tokio::task::spawn_blocking(move || -> Result<(), StorageError> {
            let mut tmp = dir.temp()?;
            for chunk in &chunks {
                tmp.write_all(chunk).map_err(io_to_storage_error)?;
            }
            // Write xattr before rename — atomic metadata+data visibility.
            xattr_meta::set_metadata_xattr(tmp.path(), &meta_json)?;
            durable_rename(tmp, &target, durability, ticket)
        })
        .await
        .map_err(super::join_error)??;

        debug!(
            "Wrote {} chunked ({} chunks) for {}/{}",
            label, num_chunks, prefix, filename
        );
        Ok(())
    }

    /// The file of one stored object, its label, and its name (as the
    /// error and debug messages print them).
    fn object_path<'o>(
        &self,
        bucket: &str,
        prefix: &str,
        object: StoredObject<'o>,
    ) -> Result<(PathBuf, &'static str, &'o str), StorageError> {
        Ok(match object {
            StoredObject::Reference => (
                self.reference_path(bucket, prefix)?,
                "reference",
                "reference.bin",
            ),
            StoredObject::Delta(f) => (self.delta_path(bucket, prefix, f)?, "delta", f),
            StoredObject::Passthrough(f) => {
                (self.passthrough_path(bucket, prefix, f)?, "passthrough", f)
            }
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
        check_path_segments("", filename, self.case_insensitive)?;
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
        check_path_segments("", filename, self.case_insensitive)?;
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
        names: Names,
    ) -> Result<FileMetadata, StorageError> {
        Self::fallback_metadata_from_stat(path, fsio::stat(path).await?, filename, names)
    }

    /// [`Self::fallback_metadata_from_path`] from a stat already taken
    /// (`None`: no file), of the path or of an open file.
    fn fallback_metadata_from_stat(
        path: &Path,
        stat: Option<std::fs::Metadata>,
        filename: &str,
        names: Names,
    ) -> Result<FileMetadata, StorageError> {
        use crate::types::StorageInfo;
        use chrono::{DateTime, Utc};

        // A directory (a key prefix) or a path under a file is no object
        // (s3surface-2): without this a GET of `dir` answered 200 with the
        // directory's size and then failed mid-body.
        let stat = match stat {
            Some(stat) if stat.is_file() => stat,
            _ => return Err(StorageError::NotFound(path.display().to_string())),
        };
        let modified: DateTime<Utc> = stat
            .modified()
            .map(DateTime::<Utc>::from)
            .unwrap_or_else(|_| Utc::now());

        let synthetic_etag = synthesise_unmanaged_etag(stat.len(), &modified);

        let mut meta = FileMetadata::fallback(
            key_filename(filename, names).to_string(),
            stat.len(),
            synthetic_etag,
            modified,
            None,
            StorageInfo::Passthrough,
        );
        stamp_stored(&mut meta, Some(&stat));
        Ok(meta)
    }

    /// The metadata of reference.bin for a read. An xattr that does not
    /// parse is described from the file's stat, with no checksum: the
    /// reference check is skipped, and the SHA-256 of each object still
    /// checks its reconstruction. Refusing it failed every delta GET of
    /// the deltaspace. Other errors pass through.
    /// Open the object file at `path` and read the metadata of the OPEN
    /// file (fgetxattr; fstat for an unmanaged file, described as `name`),
    /// in one blocking task: the metadata and the bytes are one file's,
    /// even if a write renames another file over the path between the two.
    /// Returns the file and whether the metadata came from the xattr.
    async fn open_with_metadata(
        path: PathBuf,
        what: String,
        name: &str,
        is_reference: bool,
    ) -> Result<(std::fs::File, (FileMetadata, bool)), StorageError> {
        let name = name.to_string();
        tokio::task::spawn_blocking(move || {
            use xattr::FileExt;
            let file = fsio::open_blocking(&path).map_err(|e| fsio::storage_error(e, || what))?;
            let found = file
                .get_xattr(xattr_meta::XATTR_NAME)
                .map_err(io_to_storage_error)?;
            let mut meta = match found {
                Some(json) if is_reference => (
                    Self::reference_metadata_or_stat(
                        serde_json::from_slice(&json).map_err(StorageError::from),
                        &path,
                        file.metadata().ok(),
                    )?,
                    true,
                ),
                Some(json) => (serde_json::from_slice::<FileMetadata>(&json)?, true),
                None => (
                    Self::fallback_metadata_from_stat(
                        &path,
                        file.metadata().ok(),
                        &name,
                        Names::EXACT,
                    )?,
                    false,
                ),
            };
            // The open file's mtime: the same version as the metadata.
            stamp_stored(&mut meta.0, file.metadata().ok().as_ref());
            Ok::<_, StorageError>((file, meta))
        })
        .await
        .map_err(super::join_error)?
    }

    fn reference_metadata_or_stat(
        parsed: Result<FileMetadata, StorageError>,
        path: &Path,
        stat: Option<std::fs::Metadata>,
    ) -> Result<FileMetadata, StorageError> {
        match parsed {
            Err(StorageError::Serialization(e)) => {
                tracing::warn!(
                    "Corrupt metadata on {}: {e}; the reference is read without its checksum",
                    path.display()
                );
                Self::fallback_metadata_from_stat(path, stat, "reference.bin", Names::EXACT)
            }
            other => other,
        }
    }

    /// The metadata of the object file at `path`, and whether it came from
    /// the xattr (`false`: an unmanaged file, described from its stat as
    /// `filename`). One blocking task; a missing file is known from the
    /// xattr read's ENOENT and costs no stat (P7.3).
    async fn object_metadata(
        path: &Path,
        filename: &str,
    ) -> Result<(FileMetadata, bool), StorageError> {
        let (path, filename) = (path.to_path_buf(), filename.to_string());
        tokio::task::spawn_blocking(move || match fsio::metadata_xattr_blocking(&path)? {
            fsio::StoredXattr::Value(json) => {
                let mut meta: FileMetadata = serde_json::from_slice(&json)?;
                stamp_stored(&mut meta, fsio::stat_blocking(&path)?.as_ref());
                Ok((meta, true))
            }
            fsio::StoredXattr::NoFile => Err(StorageError::NotFound(path.display().to_string())),
            fsio::StoredXattr::Missing => Ok((
                Self::fallback_metadata_from_stat(
                    &path,
                    fsio::stat_blocking(&path)?,
                    &filename,
                    Names::EXACT,
                )?,
                false,
            )),
        })
        .await
        .map_err(super::join_error)?
    }

    /// One object file of a listing or a scan (review D3, D4): the key and
    /// the kind come from the file name; the metadata is the xattr. A file
    /// without one, or with one that cannot be read or that names the other
    /// kind, is listed from its stat (a delta as an unresolved stub): a
    /// listing that dropped it let a mirror tool delete the copies, and a
    /// scan that dropped it let a delete reclaim the reference. `None`: not
    /// an object file, or deleted during the walk. A stat error fails.
    async fn listed_entry(
        path: &Path,
        name: &str,
        names: Names,
    ) -> Result<Option<(String, FileMetadata)>, StorageError> {
        use super::traits::ObjectVariant;
        use crate::types::StorageInfo;
        #[cfg(test)]
        LISTED_ENTRY_READS.with(|n| n.set(n.get() + 1));
        let StoredName::Object {
            key_filename,
            variant,
        } = stored_name(name, names)
        else {
            return Ok(None);
        };
        let kind_matches = |meta: &FileMetadata| match variant {
            ObjectVariant::Delta => matches!(meta.storage_info, StorageInfo::Delta { .. }),
            ObjectVariant::Passthrough => matches!(meta.storage_info, StorageInfo::Passthrough),
        };
        match xattr_meta::read_metadata(path).await {
            Ok(mut meta) if kind_matches(&meta) => {
                meta.original_name = key_filename.to_string();
                return Ok(Some((key_filename.to_string(), meta)));
            }
            Ok(meta) => tracing::warn!(
                "{path:?}: the metadata names another kind ({:?}); listed from its stat",
                meta.storage_info
            ),
            Err(StorageError::NotFound(_)) => {} // an unmanaged file
            Err(e @ (StorageError::Io(_) | StorageError::Serialization(_))) => {
                tracing::warn!("{path:?}: unreadable metadata ({e}); listed from its stat")
            }
            Err(e) => return Err(e),
        }
        let Some(stat) = fsio::stat(path).await? else {
            return Ok(None);
        };
        if !stat.is_file() {
            return Ok(None);
        }
        let modified = stat
            .modified()
            .map(chrono::DateTime::<chrono::Utc>::from)
            .unwrap_or_else(|_| chrono::Utc::now());
        let kind = match variant {
            ObjectVariant::Delta => StorageInfo::delta_stub(stat.len()),
            ObjectVariant::Passthrough => StorageInfo::Passthrough,
        };
        let mut meta = FileMetadata::fallback(
            key_filename.to_string(),
            stat.len(),
            synthesise_unmanaged_etag(stat.len(), &modified),
            modified,
            None,
            kind,
        );
        stamp_stored(&mut meta, Some(&stat));
        Ok(Some((key_filename.to_string(), meta)))
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
    ///
    /// No I/O here (P7.3): the write's own blocking task makes the
    /// directory ([`WriteDir::temp`]), unless the process knows it already.
    /// Every caller ran `require_bucket_exists` first, so the bucket was
    /// stat'ed once; a bucket deleted since then is found by `mkdir_within`.
    fn ensure_dir(&self, bucket: &str, path: &Path) -> Result<WriteDir, StorageError> {
        let bucket_dir = self.bucket_dir(bucket);
        let dir = path.parent().unwrap_or(&bucket_dir).to_path_buf();
        Ok(WriteDir {
            bucket: bucket.to_string(),
            bucket_dir,
            dir,
        })
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
        if !fsio::dir_exists(&self.bucket_dir(bucket)).await? {
            return Err(StorageError::BucketNotFound(bucket.to_string()));
        }
        Ok(())
    }

    /// Calculate total size of a directory recursively
    async fn dir_size(&self, path: &Path) -> Result<u64, StorageError> {
        let mut total = 0;
        if fsio::dir_exists(path).await? {
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
    /// leak into delimiter listings as undeletable "folders". Every file
    /// counts; a `.dg` entry, a `reference.bin` and a temp file do not. This
    /// is the one check of the listing's folders and of the bucket delete,
    /// which must never erase what it cannot name.
    fn dir_has_visible_data_recursive<'a>(
        current_dir: &'a Path,
        names: Names,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool, StorageError>> + Send + 'a>>
    {
        Box::pin(async move {
            #[cfg(test)]
            VISIBILITY_DIR_READS.with(|n| n.set(n.get() + 1));
            let mut entries = fs::read_dir(current_dir).await?;
            while let Some(entry) = entries.next_entry().await? {
                let path = entry.path();
                if path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| is_internal_dir_name(n, names))
                {
                    continue;
                }
                let ft = entry.file_type().await?;
                if ft.is_dir() {
                    if Self::dir_has_visible_data_recursive(&path, names).await? {
                        return Ok(true);
                    }
                } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    if !is_internal_temp_name(name) && name != crate::types::REFERENCE_FILE_NAME {
                        return Ok(true);
                    }
                }
            }
            Ok(false)
        })
    }

    /// Remove hidden temp files and empty directories from an otherwise
    /// empty bucket subtree. A reference.bin stays and counts as data: only
    /// the engine deletes it, under its deltaspace lock.
    ///
    /// Returns `true` only when the subtree contains no data.
    /// Bucket deletion uses this before a non-recursive `remove_dir`, so a
    /// concurrent object creation turns into `BucketNotEmpty` instead of being
    /// erased by `remove_dir_all`.
    fn prune_invisible_data_recursive<'a>(
        current_dir: &'a Path,
        names: Names,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<bool, StorageError>> + Send + 'a>>
    {
        Box::pin(async move {
            if !fsio::exists(current_dir).await? {
                return Ok(true);
            }
            if Self::dir_has_visible_data_recursive(current_dir, names).await? {
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
                // The proxy's own `.dg` entry holds no client key: remove it.
                if path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| is_internal_dir_name(n, names))
                {
                    let removed = if ft.is_dir() {
                        fs::remove_dir_all(&path).await
                    } else {
                        fsio::remove_file(&path).await
                    };
                    match removed {
                        Ok(()) => {}
                        Err(e) if fsio::absent(&e) => {}
                        Err(e) => return Err(io_to_storage_error(e)),
                    }
                    continue;
                }
                if ft.is_dir() {
                    if Self::prune_invisible_data_recursive(&path, names).await? {
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
                    // reference.bin stays: only the engine deletes it, under
                    // the deltaspace lock (a PUT in flight may encode
                    // against it). An orphan one is reclaimed by
                    // `DeltaGliderEngine::delete_bucket` before this runs.
                    if is_internal_temp_name(name) {
                        match fsio::remove_file(&path).await {
                            Ok(()) => {}
                            Err(e) if fsio::absent(&e) => {}
                            Err(e) => return Err(io_to_storage_error(e)),
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
                    fsio::remove_file(&path).await?;
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
        names: Names,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), StorageError>> + Send + 'a>>
    {
        Box::pin(async move {
            let mut entries = fs::read_dir(current_dir).await?;
            let mut has_deltaglider_files = false;

            while let Some(entry) = entries.next_entry().await? {
                let path = entry.path();
                if entry
                    .file_name()
                    .to_str()
                    .is_some_and(|n| is_internal_dir_name(n, names))
                {
                    continue;
                }
                let ft = entry.file_type().await?;
                if ft.is_dir() {
                    Self::find_deltaspaces_recursive(base_dir, &path, prefixes, names).await?;
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

    /// Recursively find the directories that hold a `reference.bin`, as
    /// prefixes relative to `base_dir`. Reads no metadata. A directory that
    /// is gone (a concurrent delete) or is a file holds none.
    fn find_references_recursive<'a>(
        base_dir: &'a Path,
        current_dir: &'a Path,
        prefixes: &'a mut Vec<String>,
        names: Names,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), StorageError>> + Send + 'a>>
    {
        Box::pin(async move {
            let mut entries = match fs::read_dir(current_dir).await {
                Ok(entries) => entries,
                Err(e) if fsio::absent(&e) => return Ok(()),
                Err(e) => return Err(io_to_storage_error(e)),
            };
            while let Some(entry) = entries.next_entry().await? {
                if entry
                    .file_name()
                    .to_str()
                    .is_some_and(|n| is_internal_dir_name(n, names))
                {
                    continue;
                }
                if entry.file_type().await?.is_dir() {
                    Self::find_references_recursive(base_dir, &entry.path(), prefixes, names)
                        .await?;
                } else if entry.file_name() == crate::types::REFERENCE_FILE_NAME {
                    if let Ok(relative) = current_dir.strip_prefix(base_dir) {
                        prefixes.push(relative.to_string_lossy().into_owned());
                    }
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
        names: Names,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), StorageError>> + Send + 'a>>
    {
        Box::pin(async move {
            let mut entries = fs::read_dir(current_dir).await?;
            while let Some(entry) = entries.next_entry().await? {
                let path = entry.path();
                if entry
                    .file_name()
                    .to_str()
                    .is_some_and(|n| is_internal_dir_name(n, names))
                {
                    continue;
                }
                let ft = entry.file_type().await?;
                if ft.is_dir() {
                    Self::bulk_walk_recursive(deltaspaces_dir, &path, results, baselines, names)
                        .await?;
                    continue;
                }

                let name = match path.file_name().and_then(|n| n.to_str()) {
                    Some(n) => n.to_string(),
                    None => continue,
                };

                // A baseline is never a user object; report its stored size
                // (one local stat, the walk already reads each file's xattrs).
                if name == crate::types::REFERENCE_FILE_NAME {
                    let md = match entry.metadata().await {
                        Ok(md) => md,
                        Err(e) if fsio::absent(&e) => continue,
                        Err(e) => return Err(io_to_storage_error(e)),
                    };
                    let rel = path
                        .strip_prefix(deltaspaces_dir)
                        .unwrap_or(Path::new(&name))
                        .to_string_lossy()
                        .into_owned();
                    baselines.push((rel, md.len()));
                    continue;
                }
                let Some((key_name, meta)) = Self::listed_entry(&path, &name, names).await? else {
                    continue;
                };

                // Compute user-visible key from relative path
                let relative_dir = current_dir
                    .strip_prefix(deltaspaces_dir)
                    .unwrap_or(Path::new(""));
                let dir_str = relative_dir.to_string_lossy();
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

    /// One page of a delimiter-less listing, from a walk of the subtree in
    /// key order that stops after the page: it reads the metadata of the
    /// page's files and of one more key (which proves `is_truncated`), not of
    /// every file under the prefix. The same keys, metadata and pages as
    /// `bulk_list_objects` + `dedup_keep_latest` + paging.
    ///
    /// Key order: inside one directory every key starts with the same
    /// `dir/`, so the entries sort by their name in the key, a file by its
    /// key name (`x` for both `x` and `x.delta`; empty for the folder
    /// marker) and a directory by `name/`, because every key below it starts
    /// with that. The walk visits the entries in that order and a directory's
    /// whole subtree at its place.
    async fn list_flat_page(
        &self,
        bucket: &str,
        prefix: &str,
        max_keys: u32,
        continuation_token: Option<&str>,
    ) -> Result<DelegatedListResult, StorageError> {
        let empty = DelegatedListResult {
            objects: Vec::new(),
            common_prefixes: Vec::new(),
            is_truncated: false,
            next_continuation_token: None,
            upstream_requests: 0,
            forms: super::traits::ListedForms::Reported {
                ambiguous: Vec::new(),
            },
            reference_prefixes: None,
        };
        // As `bulk_list_objects_with_baselines`: walk the deepest directory
        // the prefix names completely, keep the keys that start with it.
        let dir_part = prefix.rfind('/').map_or("", |i| &prefix[..i]);
        check_path_segments(dir_part, "", self.case_insensitive)?;
        if prefix_is_below_internal_dir(dir_part) {
            return Ok(empty);
        }
        let deltaspaces_dir = self.bucket_dir(bucket).join("deltaspaces");
        let walk_root = if dir_part.is_empty() {
            deltaspaces_dir
        } else {
            deltaspaces_dir.join(dir_part)
        };
        let token = continuation_token.unwrap_or("");
        let max = max_keys as usize;
        let Some(mut walk) = FlatWalk::open(
            &walk_root,
            dir_part.to_string(),
            prefix,
            token,
            None,
            self.names(),
        )
        .await?
        else {
            return Ok(empty);
        };
        // Distinct keys in order; one more than the page proves truncation.
        let mut found: Vec<(String, FileMetadata)> = Vec::new();
        // Keys seen in both forms (sorted: the walk is in key order).
        let mut ambiguous: Vec<String> = Vec::new();
        while let Some(step) = walk.next().await? {
            let WalkStep::File { key, path, entry } = step else {
                continue;
            };
            let Some((_, meta)) = Self::listed_entry(&path, &entry.name, self.names()).await?
            else {
                continue; // not an object file, or deleted during the walk
            };
            // The forms of one key (`x`, `x.delta`) are neighbours: the
            // newest wins, as in `dedup_keep_latest`.
            match found.last_mut() {
                Some((k, m)) if *k == key => {
                    if ambiguous.last() != Some(k) {
                        ambiguous.push(k.clone());
                    }
                    if meta.created_at > m.created_at {
                        *m = meta;
                    }
                }
                _ => {
                    found.push((key, meta));
                    if found.len() > max {
                        break;
                    }
                }
            }
        }
        let is_truncated = found.len() > max;
        found.truncate(max);
        let next_continuation_token = if is_truncated {
            found.last().map(|(k, _)| k.clone())
        } else {
            None
        };
        debug!(
            "Flat page (fs): {} objects in {}/{}, truncated={}",
            found.len(),
            bucket,
            prefix,
            is_truncated
        );
        Ok(DelegatedListResult {
            objects: found,
            common_prefixes: Vec::new(),
            is_truncated,
            next_continuation_token,
            upstream_requests: 0,
            forms: super::traits::ListedForms::Reported { ambiguous },
            reference_prefixes: None,
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
        let data = fsio::read(data_path)
            .await
            .map_err(|e| fsio::storage_error(e, || format!("{label}: {prefix}/{filename}")))?;
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
        data: Bytes,
        metadata: &FileMetadata,
        label: &str,
        prefix: &str,
        filename: &str,
        durability: Durability,
    ) -> Result<(), StorageError> {
        let dir = self.ensure_dir(bucket, data_path)?;
        let len = data.len();
        atomic_write_with_metadata(dir, data_path, data, Some(metadata), durability).await?;
        debug!(
            "Wrote {} ({} bytes) for {}/{}",
            label, len, prefix, filename
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
        fsio::remove_file(data_path)
            .await
            .map_err(|e| fsio::storage_error(e, || format!("{label}: {prefix}/{filename}")))?;
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
                        write::forget_dirs_under(&current);
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

/// The file of a bucket's key claim, in the bucket's directory.
const KEY_CLAIM_FILE: &str = ".dg-key-claim";

/// The file of a bucket's conversion marker, in the bucket's directory.
const CONVERSION_MARKER_FILE: &str = ".dg-conversion";

/// A file of a bucket-level object read longer than this is read as empty
/// (which no reader accepts).
const MAX_BUCKET_FILE: u64 = crate::storage::MAX_BUCKET_OBJECT_BYTES;

/// The version of a key claim file: its label, as the claim reads it.
fn claim_version(body: &[u8]) -> String {
    crate::storage::claim_label(body)
}

/// The version of a conversion marker file: its content.
fn marker_version(body: &[u8]) -> String {
    String::from_utf8_lossy(body).into_owned()
}

/// A synced private temp file holding `body`, for a write of the
/// bucket-level file `name`: unique per process and call.
async fn bucket_file_temp(dir: &Path, name: &str, body: &[u8]) -> std::io::Result<PathBuf> {
    use tokio::io::AsyncWriteExt;
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!("{name}.{}.{n}.tmp", std::process::id()));
    let mut f = fs::File::create(&tmp).await?;
    f.write_all(body).await?;
    f.sync_all().await?;
    Ok(tmp)
}

impl FilesystemBackend {
    /// The bucket-level file `name` (key claim, conversion marker) in the
    /// bucket's directory, beside `deltaspaces/`: listings do not see it,
    /// and DeleteBucket prunes it with the other residue. `None`: absent.
    async fn read_bucket_file(
        &self,
        bucket: &str,
        name: &str,
    ) -> Result<Option<Vec<u8>>, StorageError> {
        let path = self.bucket_dir(bucket).join(name);
        match fsio::stat(&path).await? {
            None => return Ok(None),
            Some(stat) if stat.len() > MAX_BUCKET_FILE => return Ok(Some(Vec::new())),
            Some(_) => {}
        }
        match fsio::read(&path).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Write the bucket-level file `name` with the condition of `op`: a
    /// replace or remove holds when the file's `version_of` is the named
    /// version.
    async fn write_bucket_file(
        &self,
        bucket: &str,
        name: &str,
        version_of: fn(&[u8]) -> String,
        op: crate::storage::MarkerWrite<'_>,
    ) -> Result<crate::storage::ClaimOutcome, StorageError> {
        use crate::storage::{ClaimOutcome, MarkerWrite};
        let dir = self.bucket_dir(bucket);
        if !fsio::exists(&dir).await? {
            return Err(StorageError::BucketNotFound(bucket.to_string()));
        }
        let path = dir.join(name);
        // A Replace or Remove compares the version and then writes: one at
        // a time in this process. (Two processes on one directory can still
        // interleave them; a Create cannot.)
        static WRITES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        let _one = WRITES.lock().await;
        let current = self.read_bucket_file(bucket, name).await?;
        let version_is = |v: &str| current.as_deref().map(version_of).as_deref() == Some(v);
        let outcome = match op {
            MarkerWrite::Create(body) => {
                // The file appears whole or not at all: write a synced
                // private file, then hard-link it into place. `link` fails
                // when the file exists, which is the condition (one
                // creator wins).
                let tmp = bucket_file_temp(&dir, name, body).await?;
                let linked = fs::hard_link(&tmp, &path).await;
                let _ = fsio::remove_file(&tmp).await;
                match linked {
                    Ok(()) => ClaimOutcome::Written,
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        return Ok(ClaimOutcome::Lost)
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            MarkerWrite::Replace { body, version } => {
                if !version_is(version) {
                    return Ok(ClaimOutcome::Lost);
                }
                let tmp = bucket_file_temp(&dir, name, body).await?;
                fs::rename(&tmp, &path).await?;
                ClaimOutcome::Written
            }
            MarkerWrite::Remove { version } => {
                if !version_is(version) {
                    return Ok(ClaimOutcome::Lost);
                }
                match fsio::remove_file(&path).await {
                    Ok(()) => ClaimOutcome::Written,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        return Ok(ClaimOutcome::Lost)
                    }
                    Err(e) => return Err(e.into()),
                }
            }
        };
        // The new directory entry survives a crash.
        fsio::open(&dir).await?.sync_all().await?;
        Ok(outcome)
    }
}

impl StorageBackend for FilesystemBackend {
    fn new_key_refusal(&self, _bucket: &str, prefix: &str, _filename: &str) -> Option<String> {
        let segment = colliding_folder(prefix, Names::new(self.case_insensitive))?;
        Some(format!(
            "Key folder name '{segment}' collides with a file that the filesystem backend keeps in \
             its parent folder, so no key can be stored below it"
        ))
    }

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
        if !fsio::exists(&bucket_dir).await? {
            return Err(StorageError::BucketNotFound(bucket.to_string()));
        }
        // Check if bucket has any user-visible object content.
        let deltaspaces_dir = bucket_dir.join("deltaspaces");
        // The writes' known folders: the prune below may remove them.
        write::forget_dirs_under(&bucket_dir);
        if fsio::exists(&deltaspaces_dir).await? {
            if !Self::prune_invisible_data_recursive(&deltaspaces_dir, self.names()).await? {
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
        if !fsio::exists(&self.root).await? {
            return Ok(buckets);
        }
        let mut entries = fs::read_dir(&self.root).await?;
        while let Some(entry) = entries.next_entry().await? {
            let ft = entry.file_type().await?;
            if ft.is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    // fsio-exempt: a date only.
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
        fsio::dir_exists(&self.bucket_dir(bucket)).await
    }

    // === Reference operations ===
    // Delegates to the shared get/put/delete_object_file helpers using
    // the fixed "reference.bin" filename, keeping the same error/debug
    // format as delta and passthrough operations.

    #[instrument(skip(self))]
    async fn get_object(
        &self,
        bucket: &str,
        prefix: &str,
        object: StoredObject<'_>,
    ) -> Result<Vec<u8>, StorageError> {
        let (path, label, name) = self.object_path(bucket, prefix, object)?;
        self.get_object_file(&path, label, prefix, name).await
    }

    async fn get_reference_to_file(
        &self,
        bucket: &str,
        prefix: &str,
        dest: &Path,
    ) -> Result<u64, StorageError> {
        let src = self.reference_path(bucket, prefix)?;
        if !fsio::exists(&src).await? {
            return Err(StorageError::NotFound(format!(
                "reference: {}/reference.bin",
                prefix
            )));
        }
        hardlink_or_copy(&src, dest).await?;
        let len = tokio::fs::metadata(dest).await?.len(); // fsio-exempt: the caller's copy
        Ok(len)
    }

    /// The reference file itself, opened (the caller hardlinks it) and the
    /// xattr of the open file.
    async fn open_reference_source(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<super::ReferenceSource, StorageError> {
        let path = self.reference_path(bucket, prefix)?;
        let what = format!("reference: {prefix}/reference.bin");
        // The file opened once: its size and metadata from the open file,
        // which the source keeps (see `ReferenceSource::write_to`).
        let (file, (meta, _)) =
            Self::open_with_metadata(path.clone(), what, "reference.bin", true).await?;
        let size = file.metadata()?.len();
        Ok(super::ReferenceSource::local(path, file, size, meta))
    }

    /// One metadata read; the filesystem has no response headers to keep.
    async fn get_passthrough_metadata_for_rewrite(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<(FileMetadata, crate::storage::RewriteBasis), StorageError> {
        let meta = self
            .get_passthrough_metadata(bucket, prefix, filename)
            .await?;
        Ok((meta.clone(), crate::storage::RewriteBasis::stored(meta)))
    }

    async fn put_passthrough_metadata(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        metadata: &FileMetadata,
        _basis: &crate::storage::RewriteBasis,
    ) -> Result<(), StorageError> {
        self.require_bucket_exists(bucket).await?;
        let path = self.passthrough_path(bucket, prefix, filename)?;
        if !fsio::exists(&path).await? {
            return Err(StorageError::NotFound(format!(
                "{bucket}/{prefix}/{filename}"
            )));
        }
        // xattr write only — bytes and mtime untouched (setxattr changes
        // ctime, not mtime), so the served LastModified is stable.
        xattr_meta::write_metadata(&path, metadata).await
    }

    #[instrument(skip(self))]
    async fn head_object(
        &self,
        bucket: &str,
        prefix: &str,
        object: StoredObject<'_>,
    ) -> Result<FileMetadata, StorageError> {
        match object {
            StoredObject::Reference => {
                let path = self.reference_path(bucket, prefix)?;
                // No xattr metadata: described from its stat.
                Ok(Self::object_metadata(&path, "reference.bin").await?.0)
            }
            StoredObject::Delta(filename) => {
                let path = self.delta_path(bucket, prefix, filename)?;
                Ok(keyed(
                    Self::object_metadata(&path, filename).await?,
                    filename,
                ))
            }
            StoredObject::Passthrough(filename) => {
                let path = self.passthrough_path(bucket, prefix, filename)?;
                // No xattr: an unmanaged file, described from its stat.
                Ok(keyed(
                    Self::object_metadata(&path, filename).await?,
                    filename,
                ))
            }
        }
    }

    /// Existence only: the filesystem backend is single-node, so it never
    /// fences (a cross-instance reference lock needs a shared S3 backend).
    async fn reference_fence(&self, bucket: &str, prefix: &str) -> Result<RefFence, StorageError> {
        super::traits::unfenced_reference_fence(self, bucket, prefix).await
    }

    /// The metadata read, and the existence-only fence (see above).
    async fn reference_fence_with_metadata(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<(RefFence, Option<FileMetadata>), StorageError> {
        super::traits::unfenced_reference_fence_with_metadata(self, bucket, prefix).await
    }

    #[instrument(skip(self, op, _proof))]
    async fn write_reference(
        &self,
        bucket: &str,
        prefix: &str,
        op: RefWrite<'_>,
        _proof: &crate::deltaglider::RefWriteProof,
    ) -> Result<(), StorageError> {
        // Each arm in the order of the old per-op method: the bucket
        // check (not for a delete), then the path.
        match op {
            RefWrite::Put { data, metadata } => {
                self.require_bucket_exists(bucket).await?;
                self.put_object_file(
                    bucket,
                    &self.reference_path(bucket, prefix)?,
                    data.clone(),
                    metadata,
                    "reference",
                    prefix,
                    "reference.bin",
                    Durability::Sync,
                )
                .await
            }
            RefWrite::PutFile { path, metadata, .. } => {
                // Copy to a temp file + xattr + fsync + rename. Delete-then-copy lost
                // the baseline on a failed or short copy, and a hardlink shared the
                // inode (and so the xattr) with the caller's source file.
                let reference = StoredObject::Reference;
                self.put_object_from_file(
                    bucket,
                    prefix,
                    reference,
                    path,
                    metadata,
                    Durability::Sync,
                )
                .await
            }
            RefWrite::Metadata { metadata } => {
                self.require_bucket_exists(bucket).await?;
                xattr_meta::write_metadata(&self.reference_path(bucket, prefix)?, metadata).await
            }
            RefWrite::Delete => {
                self.delete_object_file(
                    &self.reference_path(bucket, prefix)?,
                    &self.deltaspace_dir(bucket, "")?,
                    "reference",
                    prefix,
                    "reference.bin",
                )
                .await
            }
        }
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
        // A stat error is an error (B11): read as "no reference", a PUT
        // wrote a new baseline over the live one.
        fsio::exists(&self.reference_path(bucket, prefix)?).await
    }

    async fn copy_within(
        &self,
        variant: ObjectVariant,
        src: ObjectAt<'_>,
        dst: ObjectAt<'_>,
        source: &FileMetadata,
        metadata: &FileMetadata,
    ) -> Result<CopyOutcome, StorageError> {
        if !crate::storage::copy_keeps_encryption(source, metadata) {
            return Ok(CopyOutcome::Unsupported);
        }
        self.require_bucket_exists(dst.bucket).await?;
        let path_of = |at: ObjectAt<'_>| match variant {
            ObjectVariant::Delta => self.delta_path(at.bucket, at.prefix, at.filename),
            ObjectVariant::Passthrough => self.passthrough_path(at.bucket, at.prefix, at.filename),
        };
        let (src_path, dst_path) = (path_of(src)?, path_of(dst)?);
        let dir = self.ensure_dir(dst.bucket, &dst_path)?;
        let copied = atomic_server_copy(
            dir,
            &src_path,
            &dst_path,
            source,
            metadata,
            Durability::for_object(),
        )
        .await?;
        Ok(match copied {
            FileCopy::Copied => {
                debug!("Copied {src_path:?} -> {dst_path:?} inside the filesystem");
                CopyOutcome::Copied
            }
            FileCopy::NoMetadata => CopyOutcome::Unsupported,
        })
    }

    // === Delta operations ===

    // === Passthrough operations (stored with original filename) ===

    // === Chunked write operations ===

    // === Streaming operations ===

    #[instrument(skip(self, target, body, metadata), fields(name = target.name()))]
    async fn put_object(
        &self,
        bucket: &str,
        prefix: &str,
        target: StoredWrite<'_>,
        body: PutBody<'_>,
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        let object = target.object();
        match body {
            PutBody::Bytes(data) => {
                self.require_bucket_exists(bucket).await?;
                let (path, label, filename) = self.object_path(bucket, prefix, object)?;
                self.put_object_file(
                    bucket,
                    &path,
                    data,
                    metadata,
                    label,
                    prefix,
                    filename,
                    Durability::for_object(),
                )
                .await
            }
            PutBody::File { path, .. } => {
                let durability = Durability::for_object();
                self.put_object_from_file(bucket, prefix, object, path, metadata, durability)
                    .await
            }
            PutBody::Parts { paths, .. } => {
                self.put_object_from_parts(bucket, prefix, object, paths, metadata)
                    .await
            }
            PutBody::Chunks(chunks) => {
                self.put_object_from_chunks(bucket, prefix, object, chunks, metadata)
                    .await
            }
        }
    }

    /// The file's version (inode, mtime and size) is checked, then the file is
    /// written as `put_object` / `write_reference` write it. The caller
    /// holds the engine's in-process lock of the object (and of the
    /// deltaspace for a delta or the reference), which every write of it
    /// takes: on a single-node backend nothing writes between the check
    /// and the write.
    #[instrument(skip(self, target, body, metadata))]
    async fn put_if_unchanged(
        &self,
        bucket: &str,
        prefix: &str,
        target: crate::storage::RewriteTarget<'_>,
        body: PutBody<'_>,
        metadata: &FileMetadata,
        version: &str,
    ) -> Result<crate::storage::WriteIf, StorageError> {
        use crate::storage::{RewriteTarget, WriteIf};
        let (path, _, _) = self.object_path(bucket, prefix, target.object())?;
        let now = fsio::stat(&path).await?;
        if now.as_ref().and_then(version_of).as_deref() != Some(version) {
            debug!("conditional rewrite of {path:?}: the file changed; not written");
            return Ok(WriteIf::Changed);
        }
        match target {
            RewriteTarget::Delta { name, proof } => {
                let target = StoredWrite::Delta { name, proof };
                self.put_object(bucket, prefix, target, body, metadata)
                    .await?
            }
            RewriteTarget::Passthrough { name } => {
                let target = StoredWrite::Passthrough { name };
                self.put_object(bucket, prefix, target, body, metadata)
                    .await?
            }
            RewriteTarget::Reference { proof } => match body {
                PutBody::File { path, spool } => {
                    let op = RefWrite::PutFile {
                        path,
                        metadata,
                        spool,
                    };
                    self.write_reference(bucket, prefix, op, proof).await?
                }
                body => {
                    let data = body.into_small_bytes()?;
                    let op = RefWrite::Put {
                        data: &data,
                        metadata,
                    };
                    self.write_reference(bucket, prefix, op, proof).await?
                }
            },
        }
        Ok(WriteIf::Written)
    }

    #[instrument(skip(self))]
    async fn delete_object(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        variant: ObjectVariant,
    ) -> Result<(), StorageError> {
        let (path, label) = match variant {
            ObjectVariant::Delta => (self.delta_path(bucket, prefix, filename)?, "delta"),
            ObjectVariant::Passthrough => (
                self.passthrough_path(bucket, prefix, filename)?,
                "passthrough",
            ),
        };
        self.delete_object_file(
            &path,
            &self.deltaspace_dir(bucket, "")?,
            label,
            prefix,
            filename,
        )
        .await
    }

    /// The metadata read (xattr, or the fallback of a file without it) and
    /// the file, opened once.
    async fn open_object(
        &self,
        bucket: &str,
        prefix: &str,
        object: StoredObject<'_>,
    ) -> Result<super::ObjectRead, StorageError> {
        use futures::StreamExt;
        let (path, label, name) = self.object_path(bucket, prefix, object)?;
        let what = format!("{label}: {prefix}/{name}");
        let is_reference = matches!(object, StoredObject::Reference);
        let (file, meta) = Self::open_with_metadata(path, what, name, is_reference).await?;
        let meta = match object {
            StoredObject::Reference => meta.0,
            StoredObject::Delta(f) | StoredObject::Passthrough(f) => keyed(meta, f),
        };
        let file = tokio::fs::File::from_std(file);
        let stored_total = file.metadata().await?.len();
        let stream = ReaderStream::new(file).map(|r| r.map_err(StorageError::Io));
        Ok(super::ObjectRead {
            stream: Box::pin(stream),
            metadata: meta,
            stored_total,
        })
    }

    /// The file opened once: its metadata (fgetxattr) and the range read
    /// from the same open file, so both are one version's.
    async fn open_passthrough_range(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        start: u64,
        end: u64,
    ) -> Result<super::RangeRead, StorageError> {
        use futures::StreamExt;
        use tokio::io::{AsyncReadExt, AsyncSeekExt};
        let path = self.passthrough_path(bucket, prefix, filename)?;
        let what = format!("passthrough: {prefix}/{filename}");
        let (file, meta) = Self::open_with_metadata(path, what, filename, false).await?;
        let meta = keyed(meta, filename);
        let mut file = tokio::fs::File::from_std(file);
        let stored_total = file.metadata().await?.len();
        let (start, end) = super::clamp_range(start, end, stored_total)?;
        file.seek(std::io::SeekFrom::Start(start)).await?;
        let range_len = end - start + 1;
        let stream = ReaderStream::new(file.take(range_len)).map(|r| r.map_err(StorageError::Io));
        Ok(super::RangeRead {
            stream: Box::pin(stream),
            len: range_len,
            metadata: meta,
            stored_total,
        })
    }

    async fn listing_tokens(
        &self,
        _bucket: &str,
        objects: &[(String, FileMetadata)],
    ) -> Vec<Option<crate::storage::ListedToken>> {
        vec![None; objects.len()]
    }

    /// The listing carries the logical facts: no token to keep.
    async fn keep_listing_token(&self, _bucket: &str, _token: &crate::storage::ListedToken) {}

    // === Scanning operations ===

    #[instrument(skip(self))]
    async fn scan_deltaspace(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<Vec<FileMetadata>, StorageError> {
        let dir = self.deltaspace_dir(bucket, prefix)?;
        // A stat error is an error (B11): an empty scan let a delete
        // reclaim reference.bin under live deltas.
        if !fsio::exists(&dir).await? {
            return Ok(Vec::new());
        }

        let mut metadata_list = Vec::new();

        let mut entries = fs::read_dir(&dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if entry.file_type().await?.is_dir() {
                continue; // a child deltaspace
            }
            match stored_name(name, self.names()) {
                StoredName::Internal => {}
                StoredName::Reference => match xattr_meta::read_metadata(&path).await {
                    Ok(meta) => metadata_list.push(meta),
                    // Listed as a passthrough, so a delete never reclaims a
                    // reference that it cannot describe (fail closed).
                    Err(_) => {
                        match Self::fallback_metadata_from_path(&path, name, self.names()).await {
                            Ok(meta) => metadata_list.push(meta),
                            Err(StorageError::NotFound(_)) => {}
                            Err(e) => return Err(e),
                        }
                    }
                },
                StoredName::Object { .. } => {
                    if let Some((_, meta)) = Self::listed_entry(&path, name, self.names()).await? {
                        metadata_list.push(meta);
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

    /// Reads the directory names only, up to the first object; a child
    /// directory is another deltaspace.
    #[instrument(skip(self))]
    async fn holds_only_reference(&self, bucket: &str, prefix: &str) -> Result<bool, StorageError> {
        let dir = self.deltaspace_dir(bucket, prefix)?;
        if !fsio::exists(&dir).await? {
            return Ok(false);
        }
        let mut reference = false;
        let mut entries = fs::read_dir(&dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            if entry.file_type().await?.is_dir() {
                continue;
            }
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            match stored_name(name, self.names()) {
                StoredName::Internal => {}
                StoredName::Reference => reference = true,
                StoredName::Object { .. } => return Ok(false),
            }
        }
        // A reference whose metadata cannot be read stays (fail closed, as
        // in `scan_deltaspace`).
        Ok(reference
            && xattr_meta::read_metadata(&self.reference_path(bucket, prefix)?)
                .await
                .is_ok())
    }

    #[instrument(skip(self))]
    async fn list_deltaspaces(&self, bucket: &str) -> Result<Vec<String>, StorageError> {
        let deltaspaces_dir = self.bucket_dir(bucket).join("deltaspaces");
        if !fsio::exists(&deltaspaces_dir).await? {
            return Ok(Vec::new());
        }

        let mut prefixes = std::collections::HashSet::new();
        Self::find_deltaspaces_recursive(
            &deltaspaces_dir,
            &deltaspaces_dir,
            &mut prefixes,
            self.names(),
        )
        .await?;

        Ok(prefixes.into_iter().collect())
    }

    #[instrument(skip(self))]
    async fn list_reference_prefixes(
        &self,
        bucket: &str,
        scope: &str,
    ) -> Result<Vec<String>, StorageError> {
        // A `.`, `..` or empty segment names no directory of this backend:
        // no key is stored under one.
        if check_path_segments(scope, "", self.case_insensitive).is_err() {
            return Ok(Vec::new());
        }
        let deltaspaces_dir = self.bucket_dir(bucket).join("deltaspaces");
        let scope_dir = self.deltaspace_dir(bucket, scope)?;
        // A stat error is an error (B11), never "no references".
        if !fsio::dir_exists(&scope_dir).await? {
            return Ok(Vec::new());
        }
        let mut prefixes = Vec::new();
        Self::find_references_recursive(&deltaspaces_dir, &scope_dir, &mut prefixes, self.names())
            .await?;
        Ok(prefixes)
    }

    async fn list_reference_prefix_stream(
        &self,
        bucket: &str,
        scope: &str,
    ) -> Result<futures::stream::BoxStream<'static, Result<String, StorageError>>, StorageError>
    {
        ref_walk::stream(self, bucket, scope).await
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
            Bytes::new(),
            &meta,
            "folder marker",
            &obj.prefix,
            "",
            Durability::Sync,
        )
        .await
    }

    /// The key claim is a bucket-level file (`read_bucket_file`).
    /// Its version is its label.
    async fn read_key_claim(
        &self,
        bucket: &str,
    ) -> Result<Option<crate::storage::KeyClaim>, StorageError> {
        Ok(self
            .read_bucket_file(bucket, KEY_CLAIM_FILE)
            .await?
            .map(|bytes| {
                // A claim is a short label; anything longer is not one.
                let fingerprint = claim_version(&bytes);
                crate::storage::KeyClaim {
                    version: fingerprint.clone(),
                    fingerprint,
                }
            }))
    }

    async fn write_key_claim(
        &self,
        bucket: &str,
        op: crate::storage::ClaimWrite<'_>,
    ) -> Result<crate::storage::ClaimOutcome, StorageError> {
        self.write_bucket_file(bucket, KEY_CLAIM_FILE, claim_version, op.into())
            .await
    }

    /// The conversion marker is a bucket-level file; its version is its
    /// content, and the clock is this host's (a filesystem backend is one
    /// node).
    async fn read_conversion_marker(
        &self,
        bucket: &str,
    ) -> Result<crate::storage::MarkerRead, StorageError> {
        let marker = self
            .read_bucket_file(bucket, CONVERSION_MARKER_FILE)
            .await?
            .map(|body| crate::storage::ConversionMarker {
                version: marker_version(&body),
                content: body,
            });
        Ok(crate::storage::MarkerRead {
            marker,
            server_now: Some(chrono::Utc::now().timestamp()),
        })
    }

    async fn write_conversion_marker(
        &self,
        bucket: &str,
        op: crate::storage::MarkerWrite<'_>,
    ) -> Result<crate::storage::ClaimOutcome, StorageError> {
        self.write_bucket_file(bucket, CONVERSION_MARKER_FILE, marker_version, op)
            .await
    }
    async fn conversion(
        &self,
        _bucket: &str,
        _op: crate::storage::encrypting::conversion::ConversionOp,
    ) -> Result<crate::storage::encrypting::conversion::ConversionReply, crate::storage::StorageError>
    {
        Ok(crate::storage::encrypting::conversion::ConversionReply::NotEncrypted)
    }
    async fn purge_plaintext_versions(
        &self,
        _bucket: &str,
        _cursor: Option<&str>,
    ) -> Result<crate::storage::VersionPurgePage, crate::storage::StorageError> {
        Ok(crate::storage::VersionPurgePage::default())
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
            .bulk_list_objects_with_baselines(bucket, prefix, None, None)
            .await?
            .objects)
    }

    /// One local walk of the whole prefix: `start_after` and `max_listed`
    /// are ignored (no request to bound), so the answer is complete.
    async fn bulk_list_objects_with_baselines(
        &self,
        bucket: &str,
        prefix: &str,
        _start_after: Option<&str>,
        _max_listed: Option<usize>,
    ) -> Result<BulkListing, StorageError> {
        let deltaspaces_dir = self.bucket_dir(bucket).join("deltaspaces");
        // An S3 prefix is a string, not a directory: `nightly/pg` must match
        // `nightly/pg-01.sql`. Walk the deepest directory the prefix names
        // completely, then keep the keys that start with the whole prefix.
        let dir_part = prefix.rfind('/').map_or("", |i| &prefix[..i]);
        // A `.`/empty segment would walk another directory and report its
        // keys under the alias.
        check_path_segments(dir_part, "", self.case_insensitive)?;
        if prefix_is_below_internal_dir(dir_part) {
            return Ok(BulkListing::default());
        }
        let walk_root = if dir_part.is_empty() {
            deltaspaces_dir.clone()
        } else {
            deltaspaces_dir.join(dir_part)
        };

        if !fsio::exists(&walk_root).await? {
            return Ok(BulkListing::default());
        }

        let mut results: Vec<(String, FileMetadata)> = Vec::new();
        let mut baselines: Vec<(String, u64)> = Vec::new();
        Self::bulk_walk_recursive(
            &deltaspaces_dir,
            &walk_root,
            &mut results,
            &mut baselines,
            self.names(),
        )
        .await?;
        note_held(results.len());
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
            next_start_after: None,
        })
    }

    /// Paged listing without a delimiter (`list_flat_page`), and the
    /// optimised single-level listing for `delimiter = "/"`.
    ///
    /// With `/`, instead of recursively walking every subdirectory and then
    /// collapsing results in-memory, we do a single `read_dir` at the
    /// directory implied by `prefix` and classify entries into objects vs
    /// common-prefixes.
    #[instrument(skip(self))]
    async fn list_objects_delegated(
        &self,
        bucket: &str,
        prefix: &str,
        delimiter: Option<&str>,
        max_keys: u32,
        continuation_token: Option<&str>,
    ) -> Result<Option<DelegatedListResult>, StorageError> {
        // No delimiter: a sorted walk that stops after the page. A
        // delimiter other than "/" is a sorted walk too (`list_collapsed_page`).
        if delimiter.is_none() {
            return self
                .list_flat_page(bucket, prefix, max_keys, continuation_token)
                .await
                .map(Some);
        }
        let delimiter = delimiter.unwrap_or("/");
        if delimiter != "/" {
            return self
                .list_collapsed_page(bucket, prefix, delimiter, max_keys, continuation_token)
                .await
                .map(Some);
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
        check_path_segments(dir_part, "", self.case_insensitive)?;
        let read_dir_path = if dir_part.is_empty() {
            deltaspaces_dir.clone()
        } else {
            deltaspaces_dir.join(dir_part)
        };

        let no_keys = || DelegatedListResult {
            objects: Vec::new(),
            common_prefixes: Vec::new(),
            is_truncated: false,
            next_continuation_token: None,
            upstream_requests: 0,
            forms: super::traits::ListedForms::Unreported,
            reference_prefixes: None,
        };
        // A directory that does not exist is an empty result, not an error.
        // A stat error is an error, never "no keys" (review B11).
        if prefix_is_below_internal_dir(dir_part) || !fsio::exists(&read_dir_path).await? {
            return Ok(Some(no_keys()));
        }
        let token = continuation_token.unwrap_or("");
        let keep = |dir_key: &str, e: &WalkEntry| level_keep(name_filter, token, dir_key, e);
        let Some(mut level) =
            SortedDir::open(&read_dir_path, dir_part.to_string(), &keep, 0, self.names()).await?
        else {
            return Ok(Some(no_keys()));
        };

        // Objects and folders after the token, in key order, one chunk of
        // the directory at a time. A folder counts only when it holds a
        // visible key, and that check walks the folder: run it in key order,
        // only until the page holds max_keys entries plus the one that
        // proves truncation.
        let want = max_keys as usize + 1;
        let mut obj_entries: Vec<(String, PathBuf)> = Vec::new();
        let mut cp_entries: Vec<String> = Vec::new();
        // The newest-seen file of the key being read: `x` and `x.delta` are
        // neighbours, and the delta wins.
        let mut pending: Option<(String, PathBuf, bool)> = None;
        'level: loop {
            let entry = level.next_entry(&keep).await?;
            if let Some(e) = &entry {
                let key = join_key(dir_part, &e.key_name);
                if !e.is_dir {
                    let path = level.path.join(&e.name);
                    match &mut pending {
                        Some((k, p, is_delta)) if *k == key => {
                            if e.is_delta && !*is_delta {
                                *p = path;
                                *is_delta = true;
                            }
                        }
                        _ => {
                            if let Some((k, p, _)) = pending.replace((key, path, e.is_delta)) {
                                obj_entries.push((k, p));
                            }
                        }
                    }
                    if obj_entries.len() + cp_entries.len() >= want {
                        break 'level;
                    }
                    continue;
                }
            }
            if let Some((k, p, _)) = pending.take() {
                obj_entries.push((k, p));
                if obj_entries.len() + cp_entries.len() >= want {
                    break 'level;
                }
            }
            let Some(e) = entry else { break };
            // A folder (`dir/name/`).
            let cp = format!("{}/", join_key(dir_part, &e.key_name));
            if Self::dir_has_visible_data_recursive(&level.path.join(&e.name), self.names()).await?
            {
                cp_entries.push(cp);
                if obj_entries.len() + cp_entries.len() >= want {
                    break;
                }
            }
        }

        // S3 ListObjectsV2 counts both objects and common prefixes toward max_keys.
        let page =
            crate::deltaglider::interleave_and_paginate(obj_entries, cp_entries, max_keys, None);

        // Resolve metadata for object entries (after pagination to minimize I/O).
        let mut final_objects: Vec<(String, FileMetadata)> = Vec::new();

        for (key, path) in page.objects {
            let filename = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            let Some((_, meta)) = Self::listed_entry(&path, filename, self.names()).await? else {
                continue; // deleted during the listing
            };
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
            upstream_requests: 0,
            forms: super::traits::ListedForms::Unreported,
            reference_prefixes: None,
        }))
    }
}

/// Pure: does the single-level listing (`delimiter = /`) keep this entry of
/// the directory `dir_key`? A name must start with the filter and its entry
/// (`dir/name` or `dir/name/`) sort after the token. The walk never yields a `.dg` entry. A marker file
/// at the bucket root names no key.
fn level_keep(name_filter: &str, token: &str, dir_key: &str, e: &WalkEntry) -> bool {
    if !e.key_name.starts_with(name_filter) || (dir_key.is_empty() && e.key_name.is_empty()) {
        return false;
    }
    let key = join_key(dir_key, &e.key_name);
    let entry = if e.is_dir { format!("{key}/") } else { key };
    entry.as_str() > token
}

impl FilesystemBackend {
    /// A listing with a delimiter other than `/`: the sorted walk of the
    /// prefix, each key collapsed into its common prefix as it passes. It
    /// holds the page (plus one entry) and one chunk per open directory,
    /// never the prefix. A folder whose keys all collapse to one common
    /// prefix is not read, only checked for a key.
    async fn list_collapsed_page(
        &self,
        bucket: &str,
        prefix: &str,
        delimiter: &str,
        max_keys: u32,
        continuation_token: Option<&str>,
    ) -> Result<DelegatedListResult, StorageError> {
        let dir_part = prefix.rfind('/').map_or("", |i| &prefix[..i]);
        check_path_segments(dir_part, "", self.case_insensitive)?;
        let deltaspaces_dir = self.bucket_dir(bucket).join("deltaspaces");
        let walk_root = if dir_part.is_empty() {
            deltaspaces_dir
        } else {
            deltaspaces_dir.join(dir_part)
        };
        let token = continuation_token.unwrap_or("");
        let want = max_keys as usize + 1;
        let mut objects: Vec<(String, FileMetadata)> = Vec::new();
        let mut prefixes: Vec<String> = Vec::new();
        let walk = if prefix_is_below_internal_dir(dir_part) || !fsio::exists(&walk_root).await? {
            None
        } else {
            FlatWalk::open(
                &walk_root,
                dir_part.to_string(),
                prefix,
                token,
                Some(delimiter),
                self.names(),
            )
            .await?
        };
        if let Some(mut walk) = walk {
            while let Some(step) = walk.next().await? {
                if objects.len() + prefixes.len() >= want {
                    break;
                }
                let cp = match step {
                    WalkStep::Collapsed(cp) => cp,
                    WalkStep::File { key, path, entry } => {
                        let Some((_, meta)) =
                            Self::listed_entry(&path, &entry.name, self.names()).await?
                        else {
                            continue; // not an object file, or deleted during the walk
                        };
                        // `x` and `x.delta` are neighbours: the newest wins.
                        match objects.last_mut() {
                            Some((k, m)) if *k == key => {
                                if meta.created_at > m.created_at {
                                    *m = meta;
                                }
                            }
                            _ => objects.push((key, meta)),
                        }
                        continue;
                    }
                };
                // Keys of one common prefix are neighbours; one that the
                // token already ended is not shown again.
                if cp.as_str() > token && prefixes.last() != Some(&cp) {
                    prefixes.push(cp);
                }
            }
        }
        let page = crate::deltaglider::interleave_and_paginate(objects, prefixes, max_keys, None);
        Ok(DelegatedListResult {
            objects: page.objects,
            common_prefixes: page.common_prefixes,
            is_truncated: page.is_truncated,
            next_continuation_token: page.next_continuation_token,
            upstream_requests: 0,
            forms: super::traits::ListedForms::Unreported,
            reference_prefixes: None,
        })
    }
}

/// A delimiter-less listing of the filesystem backend pages through a
/// sorted walk that stops after the page. It walked the whole subtree and
/// read the metadata of every file for every page: paging through N keys
/// read N²/1000 metadata entries.
#[cfg(test)]
mod reserved_name_tests {
    use super::*;

    /// On a case-insensitive disk a case variant of a reserved name IS the
    /// proxy's file: refused for every request.
    #[test]
    fn a_case_insensitive_root_refuses_reserved_names_in_any_case() {
        for name in [
            "REFERENCE.BIN",
            "Reference.bin",
            ".DG-FOLDER-MARKER",
            ".dg-folder-marker",
            ".DG-TMP.x",
            ".dg-tmp.x",
            "a.DELTA",
            // KELVIN SIGN folds to `k` on APFS.
            ".dg-folder-mar\u{212A}er",
        ] {
            assert!(check_path_segments("", name, true).is_err(), "{name}");
        }
        assert!(check_path_segments("a/.DG/b", "", true).is_err());
        // The exact `.dg` has its own handling: key validation refuses it, a listing of it is empty.
        assert!(check_path_segments("a/.dg", "", true).is_ok());
        // A reserved folder name in the middle keeps reads and deletes working.
        assert!(check_path_segments("a/reference.bin/b", "x.txt", true).is_ok());
        assert!(check_path_segments("a/x.zip.delta", "y", true).is_ok());
        assert!(check_path_segments("a/.dgx/b", "x.txt", true).is_ok());
    }

    /// On a case-sensitive disk only the exact names are the proxy's: a case
    /// variant that an earlier release stored is an ordinary object.
    #[test]
    fn a_case_sensitive_root_keeps_the_exact_name_rule() {
        for name in [".dg-folder-marker", ".dg-tmp.x", ".dg-tmp."] {
            assert!(check_path_segments("", name, false).is_err(), "{name}");
        }
        for name in [
            "REFERENCE.BIN",
            "Reference.Bin",
            "rates.DELTA",
            ".DG-FOLDER-MARKER",
            ".Dg-Tmp.x",
            ".dg-folder-mar\u{212A}er",
        ] {
            assert!(check_path_segments("", name, false).is_ok(), "{name}");
        }
        assert!(check_path_segments("a/.Dg/b", "x", false).is_ok());
        assert!(check_path_segments("a/.DG/b", "", false).is_ok());
        // Alias segments are refused on every disk.
        for insensitive in [false, true] {
            assert!(check_path_segments("a/./b", "", insensitive).is_err());
            assert!(check_path_segments("", "..", insensitive).is_err());
        }
    }

    /// The trait method has a default (`None`), so the compiler does not make
    /// a wrapper forward it: every backend that wraps or routes does it here.
    #[test]
    fn wrappers_forward_the_new_key_refusal() {
        for (name, src) in [
            ("routing", include_str!("../routing.rs")),
            ("encrypting", include_str!("../encrypting/mod.rs")),
            ("fault", include_str!("fault.rs")),
            ("filesystem", include_str!("mod.rs")),
        ] {
            assert!(src.contains("fn new_key_refusal("), "{name}");
        }
    }

    /// A new key may not create a folder named like a file of the proxy in
    /// its parent: exact names on a case-sensitive root, any case on a
    /// case-insensitive one.
    #[test]
    fn a_new_key_may_not_create_a_folder_that_collides_with_a_proxy_file() {
        let exact = Names::new(false);
        let folded = Names::new(true);
        for folder in [
            "reference.bin",
            "b.delta",
            ".dg-tmp.x",
            ".dg-tmp.",
            ".dg-folder-marker",
        ] {
            for prefix in [
                folder.to_string(),
                format!("a/{folder}"),
                format!("{folder}/z"),
                format!("a/{folder}/z"),
            ] {
                assert_eq!(
                    colliding_folder(&prefix, exact),
                    Some(folder),
                    "exact {prefix}"
                );
                assert_eq!(
                    colliding_folder(&prefix, folded),
                    Some(folder),
                    "folded {prefix}"
                );
            }
        }
        // A case variant is an ordinary folder on a case-sensitive root only.
        for folder in ["Reference.Bin", "b.DELTA", ".DG-TMP.x", ".Dg-Folder-Marker"] {
            assert_eq!(colliding_folder(&format!("a/{folder}"), exact), None);
            assert_eq!(
                colliding_folder(&format!("a/{folder}"), folded),
                Some(folder)
            );
        }
        for ordinary in [
            "",
            "a",
            "a/b",
            "my-reference.bin",
            "x.deltas",
            ".dg-tmp",
            "a/.dgx",
        ] {
            assert_eq!(colliding_folder(ordinary, exact), None, "{ordinary}");
            assert_eq!(colliding_folder(ordinary, folded), None, "{ordinary}");
        }
    }

    /// The listing follows the root's case mode. On a case-insensitive root a
    /// case variant of an internal name is the proxy's own file: no listing
    /// shows it and none enters a `.Dg` directory, so a LIST never shows a key
    /// that GET refuses. On a case-sensitive root the same names are ordinary keys.
    #[tokio::test]
    async fn the_listing_follows_the_case_mode_of_the_root() {
        let tmp = tempfile::tempdir().unwrap();
        let mut backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .unwrap();
        let space = tmp.path().join("bucket/deltaspaces");
        for (dir, name) in [
            ("", "ok.txt"),
            ("", "Reference.Bin"),
            ("", ".DG-TMP.z"),
            (".Dg", "x"),
            ("a/.DG", "y"),
            ("a", "fine.txt"),
        ] {
            std::fs::create_dir_all(space.join(dir)).unwrap();
            std::fs::write(space.join(dir).join(name), b"data").unwrap();
        }
        for (insensitive, expected) in [
            (
                false,
                vec![
                    ".DG-TMP.z",
                    ".Dg/x",
                    "Reference.Bin",
                    "a/.DG/y",
                    "a/fine.txt",
                    "ok.txt",
                ],
            ),
            (true, vec!["a/fine.txt", "ok.txt"]),
        ] {
            backend.case_insensitive = insensitive;
            let flat = backend
                .list_objects_delegated("bucket", "", None, 100, None)
                .await
                .unwrap()
                .unwrap();
            let mut keys: Vec<String> = flat.objects.into_iter().map(|(k, _)| k).collect();
            keys.sort();
            assert_eq!(keys, expected, "insensitive={insensitive}");
            let folders = backend
                .list_objects_delegated("bucket", "", Some("/"), 100, None)
                .await
                .unwrap()
                .unwrap();
            let mut names: Vec<String> = folders.common_prefixes;
            names.sort();
            let want_folders: Vec<&str> = if insensitive {
                vec!["a/"]
            } else {
                vec![".Dg/", "a/"]
            };
            assert_eq!(names, want_folders, "folders, insensitive={insensitive}");
            // A listing of the variant directory enters nothing on a case-insensitive root.
            let inside = backend
                .list_objects_delegated("bucket", "a/.DG/", None, 100, None)
                .await;
            let inside = inside.ok().flatten().map_or(0, |r| r.objects.len());
            assert_eq!(inside, usize::from(!insensitive), "a/.DG/");
        }
    }

    #[test]
    fn a_failed_probe_counts_as_case_insensitive() {
        assert!(treat_as_case_insensitive(&Ok(true)));
        assert!(!treat_as_case_insensitive(&Ok(false)));
        assert!(treat_as_case_insensitive(&Err(std::io::Error::other("x"))));
    }

    #[test]
    fn a_case_insensitive_data_directory_is_a_warning_never_an_error() {
        let root = Path::new("/data/hetzner-fsn1");
        let warning = case_insensitive_warning(root, &Ok(true)).expect("a warning");
        assert!(warning.contains("/data/hetzner-fsn1") && warning.contains("case-insensitive"));
        assert_eq!(case_insensitive_warning(root, &Ok(false)), None);
        let err: std::io::Result<bool> = Err(std::io::Error::other("read-only"));
        assert_eq!(case_insensitive_warning(root, &err), None);
    }

    #[test]
    fn the_probe_leaves_no_file_and_answers() {
        let tmp = tempfile::tempdir().unwrap();
        probe_case_insensitive(tmp.path()).expect("the probe answers");
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
        assert!(probe_case_insensitive(&tmp.path().join("missing")).is_err());
    }
}

#[cfg(test)]
mod paged_listing_tests {
    use super::*;
    use crate::config::Config;
    use crate::deltaglider::{DeltaGliderEngine, DynEngine as Engine, ListObjectsPage};
    use std::collections::BTreeMap;
    use std::sync::Arc;

    async fn engine() -> (tempfile::TempDir, Engine) {
        let tmp = tempfile::tempdir().unwrap();
        let fs = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .unwrap();
        let engine: Engine = DeltaGliderEngine::new_with_backend(
            Arc::new(super::super::DynStorageBackend::new_box(fs)),
            &Config::default(),
            None,
        );
        engine.create_bucket("b").await.unwrap();
        (tmp, engine)
    }

    async fn page(
        engine: &Engine,
        prefix: &str,
        max_keys: u32,
        token: Option<&str>,
    ) -> ListObjectsPage {
        engine
            .list_objects("b", prefix, None, max_keys, token, false)
            .await
            .unwrap()
    }

    /// 5,000 keys in 5 pages read about 5,000 entries' metadata (it was
    /// 25,000: every page walked all of them).
    #[tokio::test]
    async fn a_paged_flat_listing_reads_each_entry_about_once() {
        let (tmp, engine) = engine().await;
        // Files without metadata (foreign objects, listed from their stat):
        // 5,000 engine PUTs take a minute.
        let ds = tmp.path().join("b").join("deltaspaces");
        let mut want = Vec::new();
        for i in 0..5000 {
            let key = format!("d{}/k-{i:05}.jpg", i % 7);
            let path = ds.join(&key);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"x").unwrap();
            want.push(key);
        }
        want.sort();
        LISTED_ENTRY_READS.with(|n| n.set(0));
        let mut got = Vec::new();
        let mut token: Option<String> = None;
        let mut pages = 0;
        loop {
            let p = page(&engine, "", 1000, token.as_deref()).await;
            pages += 1;
            got.extend(p.objects.into_iter().map(|(k, _)| k));
            if !p.is_truncated {
                break;
            }
            token = p.next_continuation_token;
        }
        assert_eq!(got, want);
        assert_eq!(pages, 5);
        let reads = LISTED_ENTRY_READS.with(|n| n.get());
        assert!(reads <= 5_100, "{reads} metadata reads for 5,000 keys");
    }

    #[test]
    fn a_subtree_is_walked_only_when_it_can_hold_a_key_of_the_page() {
        // (below, prefix, token)
        assert!(flat_subtree_may_match("a/", "", ""));
        assert!(flat_subtree_may_match("a/", "a", ""));
        assert!(
            flat_subtree_may_match("a/b/", "a/b/c", ""),
            "narrower prefix"
        );
        assert!(!flat_subtree_may_match("x/", "a", ""));
        assert!(flat_subtree_may_match("a/", "a/b/", ""), "prefix inside");
        assert!(
            !flat_subtree_may_match("a/c/", "a/b/", ""),
            "prefix in a sibling"
        );
        assert!(flat_subtree_may_match("a/", "", "a/"), "token == below");
        assert!(flat_subtree_may_match("a/", "", "a/k"), "token inside");
        assert!(flat_subtree_may_match("b/", "", "a/z"), "token before");
        assert!(!flat_subtree_may_match("a/", "", "a0"), "token after");
        assert!(!flat_subtree_may_match("a/", "", "b"));
    }

    /// The paged walk lists exactly what the full walk did, page by page:
    /// key order across directories (`a-b` < `a.b` < `a/` < `a0`), name
    /// prefixes, folder markers, and a key stored in both forms.
    #[tokio::test]
    async fn the_paged_walk_matches_the_full_walk() {
        let (tmp, engine) = engine().await;
        let no_delta = std::collections::HashMap::from([(
            crate::types::meta_keys::NO_DELTA.to_string(),
            "true".to_string(),
        )]);
        for key in [
            "a-b",
            "a.b",
            "a/b-c",
            "a/b/c",
            "a/b.c",
            "a/bb",
            "a0",
            "ab",
            "dir/",
            "dir/x",
            "x/y-1.txt",
            "x/y.txt",
            "x/y/z.txt",
            "x/y0",
            ".env",
            "é.txt",
            "z",
            "dup.zip",
        ] {
            let body: &[u8] = if key.ends_with('/') { b"" } else { b"body" };
            engine
                .store("b", key, body, None, no_delta.clone())
                .await
                .unwrap();
        }
        // Two deltas of one deltaspace (each with its `.delta` file).
        crate::deltaglider::store_deltas(&engine, "rel", 3).await;
        // `dup.zip` in both forms: a `.delta` file without metadata beside
        // the passthrough one (a stub, newer: it wins in both walks).
        let ds = tmp.path().join("b").join("deltaspaces");
        std::fs::copy(ds.join("dup.zip"), ds.join("dup.zip.delta")).unwrap();

        let bulk = engine.storage().bulk_list_objects("b", "").await.unwrap();
        let all = crate::types::dedup_keep_latest(bulk);
        for prefix in [
            "", "a", "a/", "a/b", "x/y", "x/y/", "dir/", "rel/", "é", "none",
        ] {
            let want: Vec<(String, FileMetadata)> = all
                .iter()
                .filter(|(k, _)| k.starts_with(prefix))
                .cloned()
                .collect();
            for max_keys in [1, 2, 3, 1000] {
                let mut got: Vec<(String, FileMetadata)> = Vec::new();
                let mut token: Option<String> = None;
                loop {
                    let p = page(&engine, prefix, max_keys, token.as_deref()).await;
                    assert!(p.common_prefixes.is_empty());
                    assert!(p.objects.len() <= max_keys as usize);
                    let full = p.objects.len() == max_keys as usize;
                    got.extend(p.objects);
                    if !p.is_truncated {
                        break;
                    }
                    assert!(full, "{prefix:?}/{max_keys}: a short truncated page");
                    assert_eq!(
                        p.next_continuation_token.as_ref(),
                        got.last().map(|(k, _)| k)
                    );
                    token = p.next_continuation_token;
                }
                let names = |v: &[(String, FileMetadata)]| {
                    v.iter()
                        .map(|(k, m)| (k.clone(), m.file_size, m.created_at, m.md5.clone()))
                        .collect::<Vec<_>>()
                };
                assert_eq!(names(&got), names(&want), "prefix {prefix:?}, {max_keys}");
            }
        }
    }

    /// A root listing with a delimiter over 3,000 folders: the visibility
    /// check walked every folder for every page (3 pages = 18,000 directory
    /// reads), before the name filter and the page cut.
    #[tokio::test]
    async fn a_delimiter_listing_checks_only_the_folders_it_shows() {
        let (tmp, engine) = engine().await;
        let ds = tmp.path().join("b").join("deltaspaces");
        for i in 0..3000 {
            let p = ds.join(format!("f{i:05}/sub/a.txt"));
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, b"x").unwrap();
        }
        for (prefix, folders, max_reads) in [("", 3000, 6_100), ("f029", 100, 210)] {
            VISIBILITY_DIR_READS.with(|n| n.set(0));
            let mut token: Option<String> = None;
            let mut got = 0;
            loop {
                let p = engine
                    .list_objects("b", prefix, Some("/"), 1000, token.as_deref(), false)
                    .await
                    .unwrap();
                got += p.common_prefixes.len();
                if !p.is_truncated {
                    break;
                }
                token = p.next_continuation_token;
            }
            assert_eq!(got, folders, "{prefix:?}");
            let reads = VISIBILITY_DIR_READS.with(|n| n.get());
            assert!(
                reads <= max_reads,
                "{reads} directory reads to list {folders} folders under {prefix:?}"
            );
        }
    }

    /// The delimiter listing over random trees equals a sorted model built
    /// from the visible keys: folders with only `reference.bin`, temp files
    /// or nothing stay hidden; paging with every page size and a start-after
    /// token anywhere (inside a folder name too) matches.
    mod delimiter_model {
        use super::*;
        use proptest::prelude::*;
        use std::collections::BTreeSet;

        const DIRS: [&str; 5] = ["a", "b", "a-b", "a.b", "a0"];
        /// A file name and the key name it stores (`None`: not a key).
        const FILES: [(&str, Option<&str>); 7] = [
            ("x", Some("x")),
            ("y.txt", Some("y.txt")),
            ("x-1", Some("x-1")),
            ("q.delta", Some("q")),
            ("reference.bin", None),
            (".dg-tmp.0", None),
            ("", None), // an empty directory
        ];
        const PREFIXES: [&str; 10] = [
            "", "a", "a/", "a-", "a/b", "a/b/", "a.b/", "a0", "b/a/", "zz",
        ];

        /// The entries a delimiter (`/`) listing of `prefix` shows: keys
        /// directly under it, and one common prefix per folder below it.
        fn model(keys: &BTreeSet<String>, prefix: &str) -> Vec<(String, bool)> {
            let mut out = BTreeMap::new();
            for k in keys.iter().filter(|k| k.starts_with(prefix)) {
                let rest = &k[prefix.len()..];
                match rest.find('/') {
                    Some(i) => out.insert(format!("{prefix}{}", &rest[..=i]), true),
                    None => out.insert(k.clone(), false),
                };
            }
            out.into_iter().collect()
        }

        fn page_of(
            model: &[(String, bool)],
            max_keys: usize,
            token: Option<&str>,
        ) -> (Vec<(String, bool)>, bool) {
            let after: Vec<_> = model
                .iter()
                .filter(|(k, _)| token.is_none_or(|t| k.as_str() > t))
                .cloned()
                .collect();
            let truncated = after.len() > max_keys;
            (after.into_iter().take(max_keys).collect(), truncated)
        }

        fn listed(r: &DelegatedListResult) -> Vec<(String, bool)> {
            let mut v: Vec<(String, bool)> = r
                .objects
                .iter()
                .map(|(k, _)| (k.clone(), false))
                .chain(r.common_prefixes.iter().map(|p| (p.clone(), true)))
                .collect();
            v.sort();
            v
        }

        fn tree() -> impl Strategy<Value = Vec<(Vec<usize>, usize)>> {
            prop::collection::vec(
                (prop::collection::vec(0..DIRS.len(), 0..3), 0..FILES.len()),
                0..14,
            )
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(96))]
            #[test]
            fn the_delimiter_listing_matches_the_sorted_model(
                files in tree(),
                prefix in 0..PREFIXES.len(),
                token in prop::option::of(0..PREFIXES.len()),
            ) {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(async {
                    let tmp = tempfile::tempdir().unwrap();
                    let fs = FilesystemBackend::new(tmp.path().to_path_buf()).await.unwrap();
                    fs.create_bucket("b").await.unwrap();
                    let ds = tmp.path().join("b").join("deltaspaces");
                    let mut keys = BTreeSet::new();
                    for (dirs, file) in &files {
                        let dir: Vec<&str> = dirs.iter().map(|&d| DIRS[d]).collect();
                        let dir_path = ds.join(dir.join("/"));
                        std::fs::create_dir_all(&dir_path).unwrap();
                        let (name, key_name) = FILES[*file];
                        if name.is_empty() {
                            continue;
                        }
                        std::fs::write(dir_path.join(name), b"x").unwrap();
                        if let Some(key_name) = key_name {
                            let mut parts = dir.clone();
                            parts.push(key_name);
                            keys.insert(parts.join("/"));
                        }
                    }
                    let prefix = PREFIXES[prefix];
                    let model = model(&keys, prefix);
                    // Start-after anywhere: one page from that token.
                    let start = token.map(|t| format!("{}x", PREFIXES[t]));
                    let r = fs
                        .list_objects_delegated("b", prefix, Some("/"), 2, start.as_deref())
                        .await
                        .unwrap()
                        .unwrap();
                    let (want, truncated) = page_of(&model, 2, start.as_deref());
                    assert_eq!(listed(&r), want, "start-after {start:?}");
                    assert_eq!(r.is_truncated, truncated);
                    // Every page size, walked to the end.
                    for max_keys in [1usize, 2, 3, 1000] {
                        let mut got = Vec::new();
                        let mut token: Option<String> = None;
                        loop {
                            let r = fs
                                .list_objects_delegated(
                                    "b",
                                    prefix,
                                    Some("/"),
                                    max_keys as u32,
                                    token.as_deref(),
                                )
                                .await
                                .unwrap()
                                .unwrap();
                            let (want, truncated) = page_of(&model, max_keys, token.as_deref());
                            assert_eq!(listed(&r), want, "{prefix:?} {max_keys} {token:?}");
                            assert_eq!(r.is_truncated, truncated);
                            got.extend(listed(&r));
                            if !r.is_truncated {
                                break;
                            }
                            token = r.next_continuation_token.clone();
                            assert_eq!(token.as_ref(), got.last().map(|(k, _)| k));
                        }
                        assert_eq!(got, model, "{prefix:?} {max_keys}");
                    }
                });
            }
        }
    }
}
