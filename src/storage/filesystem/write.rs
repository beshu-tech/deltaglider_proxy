// SPDX-License-Identifier: BUSL-1.1

//! Atomic, durable file writes: the temp file in the target directory
//! (re-created if a prune removed it), `durable_rename` (file fsync,
//! rename, directory fsync) and the deferred-fsync pending set.

use super::*;

/// Temp file for an atomic write in `dir`, named so listings and bucket
/// deletion recognise it (`is_internal_temp_name`).
pub(super) fn internal_temp_in(dir: &Path) -> std::io::Result<NamedTempFile> {
    tempfile::Builder::new()
        .prefix(INTERNAL_TEMP_PREFIX)
        .tempfile_in(dir)
}

/// How often a write re-creates a directory that a concurrent DELETE pruned.
pub(super) const MKDIR_ATTEMPTS: usize = 8;

/// Create `dir` and its missing ancestors inside `bucket_dir`, never the
/// bucket dir itself (a deleted bucket stays deleted: C-P0-1). A DELETE in
/// another deltaspace prunes empty ancestors under its own lock, so a
/// component can vanish mid-walk: the walk starts again while the bucket
/// exists (storage-1). Blocking.
pub(super) fn mkdir_within(
    bucket: &str,
    bucket_dir: &Path,
    dir: &Path,
) -> Result<(), StorageError> {
    let Ok(rel) = dir.strip_prefix(bucket_dir) else {
        return Err(StorageError::Other(format!(
            "ensure_dir called with path {:?} outside bucket {}",
            dir, bucket
        )));
    };
    'walk: for _ in 0..MKDIR_ATTEMPTS {
        let mut current = bucket_dir.to_path_buf();
        for component in rel.components() {
            current.push(component);
            match std::fs::create_dir(&current) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    if !bucket_dir.is_dir() {
                        return Err(StorageError::BucketNotFound(bucket.to_string()));
                    }
                    continue 'walk;
                }
                Err(e) => return Err(StorageError::from(e)),
            }
        }
        return Ok(());
    }
    Err(StorageError::Throttled(format!(
        "directory {dir:?} was removed {MKDIR_ATTEMPTS} times while it was created"
    )))
}

/// The directory an atomic write puts its temp file in. Made only by
/// `FilesystemBackend::ensure_dir`, so every write can re-create it.
#[derive(Debug)]
pub(super) struct WriteDir {
    pub(super) bucket: String,
    pub(super) bucket_dir: PathBuf,
    pub(super) dir: PathBuf,
}

impl WriteDir {
    /// A temp file in the directory. A DELETE in another deltaspace can
    /// prune the directory between `ensure_dir` and here (the PUT then got
    /// ENOENT, which reads as 404 NoSuchKey): create it again and retry.
    /// Once the temp file exists, the directory is not empty, so the
    /// rename that follows cannot lose it. Blocking.
    pub(super) fn temp(&self) -> Result<NamedTempFile, StorageError> {
        for _ in 0..MKDIR_ATTEMPTS {
            match internal_temp_in(&self.dir) {
                Ok(tmp) => return Ok(tmp),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    mkdir_within(&self.bucket, &self.bucket_dir, &self.dir)?;
                }
                Err(e) => return Err(io_to_storage_error(e)),
            }
        }
        internal_temp_in(&self.dir).map_err(io_to_storage_error)
    }
}

/// Whether a write fsyncs its file before the rename (the default) or
/// leaves that to [`StorageBackend::flush_pending`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Durability {
    Sync,
    Deferred,
}

impl Durability {
    /// An object (delta / passthrough) write: deferred only inside
    /// [`crate::storage::with_deferred_fsync`]. Reference writes and directory
    /// markers use `Durability::Sync` directly.
    pub(super) fn for_object() -> Self {
        if crate::storage::fsync_deferred() {
            Durability::Deferred
        } else {
            Durability::Sync
        }
    }
}

/// Deferred writes not yet made durable. Process-wide, not per backend: an
/// engine rebuild (config apply) replaces the backend between a write and
/// its flush, and the flush must still cover the write. Bounded: a write
/// that fills it flushes it.
pub(super) static PENDING_FSYNC: parking_lot::Mutex<Vec<PathBuf>> =
    parking_lot::Mutex::new(Vec::new());

pub(super) const PENDING_FSYNC_MAX: usize = 256;

/// fsync every file in `paths`, up to `FSYNC_PARALLEL` at a time: fsyncs
/// that run together share one journal commit (ext4, XFS), which makes a
/// flush of 20 files about 6x cheaper than 20 fsyncs in a row. A file that
/// is gone (deleted, or replaced by a later write, which fsyncs or defers
/// on its own) is skipped. The paths that fail go back to the pending set.
pub(super) fn fsync_paths(paths: Vec<PathBuf>) -> Result<(), StorageError> {
    const FSYNC_PARALLEL: usize = 32;
    let mut first_err = None;
    for batch in paths.chunks(FSYNC_PARALLEL) {
        let results: Vec<std::io::Result<()>> = std::thread::scope(|s| {
            let handles: Vec<_> = batch
                .iter()
                .map(|path| {
                    s.spawn(move || match sync_path(path) {
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                        other => other,
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| {
                    h.join()
                        .unwrap_or_else(|_| Err(std::io::Error::other("fsync thread panicked")))
                })
                .collect()
        });
        for (path, res) in batch.iter().zip(results) {
            if let Err(e) = res {
                PENDING_FSYNC.lock().push(path.clone());
                first_err.get_or_insert(e);
            }
        }
    }
    match first_err {
        Some(e) => Err(io_to_storage_error(e)),
        None => Ok(()),
    }
}

/// Paths that `sync_path` made durable, for the tests.
#[cfg(test)]
pub(super) static SYNCED: parking_lot::Mutex<Vec<PathBuf>> = parking_lot::Mutex::new(Vec::new());

/// fsync a file or a directory. A directory fsync makes its entries
/// durable: without it, a rename can be lost on power loss (ext4, XFS)
/// although the file's own data is on disk.
pub(super) fn sync_path(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()?;
    #[cfg(test)]
    SYNCED.lock().push(path.to_path_buf());
    Ok(())
}

/// Make the pending deferred writes durable (blocking).
pub(super) fn flush_pending_fsync() -> Result<(), StorageError> {
    let paths = std::mem::take(&mut *PENDING_FSYNC.lock());
    fsync_paths(paths)
}

/// The end of every atomic write, and the one owner of its durability:
/// fsync the file, rename it over `target`, fsync the parent directory
/// (the rename is an entry of it); or, `Deferred`, rename and record the
/// file and its directory in the pending set for `flush_pending`.
/// Blocking.
pub(super) fn durable_rename(
    tmp: NamedTempFile,
    target: &Path,
    durability: Durability,
) -> Result<(), StorageError> {
    if durability == Durability::Sync {
        tmp.as_file().sync_all().map_err(io_to_storage_error)?;
    }
    tmp.persist(target)
        .map_err(|e| io_to_storage_error(e.error))?;
    // The rename is an entry of the parent directory: durable only once
    // the directory is fsynced too (storage-5).
    let parent = target.parent().map(Path::to_path_buf);
    if durability == Durability::Sync {
        if let Some(dir) = &parent {
            sync_path(dir).map_err(io_to_storage_error)?;
        }
    }
    if durability == Durability::Deferred {
        let full = {
            let mut pending = PENDING_FSYNC.lock();
            pending.push(target.to_path_buf());
            // Many copies share one directory: fsync it once per flush.
            if let Some(dir) = parent {
                if !pending.contains(&dir) {
                    pending.push(dir);
                }
            }
            pending.len() >= PENDING_FSYNC_MAX
        };
        if full {
            flush_pending_fsync()?;
        }
    }
    Ok(())
}

/// Atomically write data + metadata to a file using write-to-temp + xattr + fsync + rename.
///
/// The xattr is written to the temp file BEFORE the rename, so a crash can never
/// leave a data file without its metadata. Either both are visible or neither is.
pub(super) async fn atomic_write_with_metadata(
    dir: WriteDir,
    path: &Path,
    data: &[u8],
    metadata: Option<&FileMetadata>,
    durability: Durability,
) -> Result<(), StorageError> {
    use tokio::io::AsyncWriteExt;
    let path = path.to_path_buf();
    let meta_json = metadata.map(serde_json::to_vec).transpose()?;

    let tmp = tokio::task::spawn_blocking(move || dir.temp())
        .await
        .map_err(crate::storage::join_error)??;
    // The body goes through an async file handle, which copies it in small
    // chunks: a blocking task needs an owned buffer, and a copy of the whole
    // body doubled the RAM of every buffered write (storage-15).
    let handle = tmp.as_file().try_clone().map_err(io_to_storage_error)?;
    let mut file = tokio::fs::File::from_std(handle);
    file.write_all(data).await.map_err(io_to_storage_error)?;
    file.flush().await.map_err(io_to_storage_error)?;
    drop(file);
    tokio::task::spawn_blocking(move || {
        // Write xattr to temp file BEFORE rename — atomic metadata+data visibility.
        if let Some(json) = &meta_json {
            xattr_meta::set_metadata_xattr(tmp.path(), json)?;
        }
        durable_rename(tmp, &path, durability)
    })
    .await
    .map_err(crate::storage::join_error)?
}

/// Materialise `src` at `dest` cheaply: hardlink (O(1), no extra bytes) when on
/// the same filesystem, byte-copy as fallback across fs boundaries (EXDEV).
/// `dest` is removed first (hard_link refuses an existing target — the caller may
/// hand a pre-created spool temp). READ side only (`get_reference_to_file`):
/// the spool gets a snapshot inode that a later reference rename never
/// touches. Never use it to WRITE a stored object — see
/// `put_reference_from_file`.
pub(super) async fn hardlink_or_copy(src: &Path, dest: &Path) -> Result<(), StorageError> {
    let _ = fs::remove_file(dest).await;
    if fs::hard_link(src, dest).await.is_err() {
        fs::copy(src, dest).await?;
    }
    Ok(())
}

/// Atomically copy file data + metadata to destination using temp + rename.
pub(super) async fn atomic_copy_with_metadata(
    dir: WriteDir,
    source_path: &Path,
    target_path: &Path,
    metadata: &FileMetadata,
    durability: Durability,
) -> Result<(), StorageError> {
    let source = source_path.to_path_buf();
    let target = target_path.to_path_buf();
    let meta_json = serde_json::to_vec(metadata)?;

    tokio::task::spawn_blocking(move || {
        let mut src = std::fs::File::open(&source).map_err(io_to_storage_error)?;
        let mut tmp = dir.temp()?;
        std::io::copy(&mut src, &mut tmp).map_err(io_to_storage_error)?;
        xattr_meta::set_metadata_xattr(tmp.path(), &meta_json)?;
        durable_rename(tmp, &target, durability)
    })
    .await
    .map_err(crate::storage::join_error)?
}
