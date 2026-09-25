// SPDX-License-Identifier: BUSL-1.1

//! xattr-based metadata storage for the filesystem backend.
//!
//! All metadata is stored as a single `user.dg.metadata` extended attribute
//! on each data file's inode, eliminating the need for `.meta` sidecar files.

use super::traits::StorageError;
use crate::types::FileMetadata;
use std::path::Path;

/// The single xattr name used for all DeltaGlider metadata.
pub(crate) const XATTR_NAME: &str = "user.dg.metadata";

use super::io_to_storage_error;

/// Read metadata from the xattr on a data file.
///
/// Returns `StorageError::NotFound` if the xattr is absent.
pub async fn read_metadata(path: &Path) -> Result<FileMetadata, StorageError> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || match xattr::get(&path, XATTR_NAME) {
        Ok(Some(data)) => {
            let metadata: FileMetadata = serde_json::from_slice(&data)?;
            Ok(metadata)
        }
        Ok(None) => Err(StorageError::NotFound(format!(
            "No metadata xattr on {}",
            path.display()
        ))),
        Err(e) => Err(io_to_storage_error(e)),
    })
    .await
    .map_err(super::join_error)?
}

/// Metadata JSON at most this long that fails to store is a real I/O fault
/// (a full disk); a longer one is too large for the xattr (ext4 keeps one
/// value within one block, about 4 KiB).
const XATTR_ALWAYS_FITS: usize = 2048;

/// Pure: the error for a failed metadata xattr write of `json_len` bytes.
pub(crate) fn classify_xattr_set_error(e: std::io::Error, json_len: usize) -> StorageError {
    const E2BIG: i32 = 7;
    const ENOSPC: i32 = 28;
    const ERANGE: i32 = 34;
    let too_big = matches!(e.raw_os_error(), Some(E2BIG) | Some(ERANGE))
        || (e.raw_os_error() == Some(ENOSPC) && json_len > XATTR_ALWAYS_FITS);
    if too_big {
        StorageError::MetadataTooLarge(format!(
            "the object's metadata ({json_len} bytes stored) does not fit this filesystem's \
             extended attribute"
        ))
    } else {
        io_to_storage_error(e)
    }
}

/// THE way the filesystem backend stores an object's metadata xattr.
pub(crate) fn set_metadata_xattr(path: &Path, json: &[u8]) -> Result<(), StorageError> {
    xattr::set(path, XATTR_NAME, json).map_err(|e| classify_xattr_set_error(e, json.len()))
}

/// Write metadata as an xattr on a data file.
///
/// Uses compact JSON serialization to minimize xattr size.
pub async fn write_metadata(path: &Path, metadata: &FileMetadata) -> Result<(), StorageError> {
    let path = path.to_path_buf();
    let json = serde_json::to_vec(metadata)?;
    tokio::task::spawn_blocking(move || set_metadata_xattr(&path, &json))
        .await
        .map_err(super::join_error)?
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
    .map_err(super::join_error)?
}

#[cfg(test)]
mod set_error_tests {
    use super::*;

    #[test]
    fn oversized_metadata_is_not_a_full_disk() {
        let err = |code| std::io::Error::from_raw_os_error(code);
        assert!(matches!(
            classify_xattr_set_error(err(28), 4100),
            StorageError::MetadataTooLarge(_)
        ));
        assert!(matches!(
            classify_xattr_set_error(err(7), 10),
            StorageError::MetadataTooLarge(_)
        ));
        assert!(matches!(
            classify_xattr_set_error(err(28), 500),
            StorageError::DiskFull
        ));
        assert!(matches!(
            classify_xattr_set_error(err(5), 4100),
            StorageError::Io(_)
        ));
    }
}
