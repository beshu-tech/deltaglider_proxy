//! xattr-based metadata storage for the filesystem backend.
//!
//! All metadata is stored as a single `user.dg.metadata` extended attribute
//! on each data file's inode, eliminating the need for `.meta` sidecar files.

use super::traits::StorageError;
use crate::types::FileMetadata;
use std::path::Path;

/// The single xattr name used for all DeltaGlider metadata.
const XATTR_NAME: &str = "user.dg.metadata";

/// Convert an io::Error into StorageError, detecting disk-full (ENOSPC).
fn io_to_storage_error(e: std::io::Error) -> StorageError {
    const ENOSPC: i32 = 28;
    if e.raw_os_error() == Some(ENOSPC) {
        StorageError::DiskFull
    } else {
        StorageError::Io(e)
    }
}

/// Read metadata from the xattr on a data file.
///
/// Returns `StorageError::NotFound` if the xattr is absent.
pub async fn read_metadata(path: &Path) -> Result<FileMetadata, StorageError> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        match xattr::get(&path, XATTR_NAME) {
            Ok(Some(data)) => {
                let metadata: FileMetadata = serde_json::from_slice(&data)?;
                Ok(metadata)
            }
            Ok(None) => Err(StorageError::NotFound(format!(
                "No metadata xattr on {}",
                path.display()
            ))),
            Err(e) => Err(io_to_storage_error(e)),
        }
    })
    .await
    .map_err(|e| StorageError::Other(format!("spawn_blocking join failed: {}", e)))?
}

/// Write metadata as an xattr on a data file.
///
/// Uses compact JSON serialization to minimize xattr size.
pub async fn write_metadata(path: &Path, metadata: &FileMetadata) -> Result<(), StorageError> {
    let path = path.to_path_buf();
    let json = serde_json::to_vec(metadata)?;
    tokio::task::spawn_blocking(move || {
        xattr::set(&path, XATTR_NAME, &json).map_err(io_to_storage_error)
    })
    .await
    .map_err(|e| StorageError::Other(format!("spawn_blocking join failed: {}", e)))?
}

/// Remove metadata xattr from a data file.
///
/// Silently succeeds if the xattr is already absent.
#[allow(dead_code)]
pub async fn remove_metadata(path: &Path) -> Result<(), StorageError> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        match xattr::remove(&path, XATTR_NAME) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            // ENODATA (61 on Linux, 93 on macOS) means the attribute doesn't exist
            Err(e) if e.raw_os_error() == Some(93) || e.raw_os_error() == Some(61) => Ok(()),
            Err(e) => Err(io_to_storage_error(e)),
        }
    })
    .await
    .map_err(|e| StorageError::Other(format!("spawn_blocking join failed: {}", e)))?
}

/// Validate that the filesystem at `root` supports extended attributes.
///
/// Creates a probe file, writes a test xattr, reads it back, then cleans up.
/// On failure, returns a descriptive error listing compatible filesystems.
pub async fn validate_xattr_support(root: &Path) -> Result<(), StorageError> {
    let probe_path = root.join(".dg_xattr_probe");
    let probe = probe_path.clone();

    tokio::task::spawn_blocking(move || {
        // Create probe file
        std::fs::write(&probe, b"xattr_probe").map_err(io_to_storage_error)?;

        let test_value = b"xattr_test_ok";
        let result = (|| -> Result<(), StorageError> {
            xattr::set(&probe, XATTR_NAME, test_value).map_err(io_to_storage_error)?;

            let readback = xattr::get(&probe, XATTR_NAME).map_err(io_to_storage_error)?;
            match readback {
                Some(v) if v == test_value => Ok(()),
                Some(_) => Err(StorageError::Other(
                    "xattr readback mismatch — filesystem may not support xattrs reliably".into(),
                )),
                None => Err(StorageError::Other(
                    "xattr readback returned None — filesystem may not support xattrs".into(),
                )),
            }
        })();

        // Always clean up probe file
        let _ = std::fs::remove_file(&probe);

        result.map_err(|_| {
            StorageError::Other(
                "Filesystem at data directory does not support extended attributes (xattr). \
                 DeltaGlider requires xattr support — use ext4, XFS, Btrfs, ZFS, or APFS."
                    .into(),
            )
        })
    })
    .await
    .map_err(|e| StorageError::Other(format!("spawn_blocking join failed: {}", e)))?
}

