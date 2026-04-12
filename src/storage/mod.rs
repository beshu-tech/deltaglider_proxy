//! Storage backend abstraction

pub mod encrypting;
mod filesystem;
pub(crate) mod routing;
mod s3;
mod traits;
#[cfg(unix)]
pub(crate) mod xattr_meta;

pub use encrypting::{EncryptingBackend, EncryptionConfig, EncryptionKey};
pub use filesystem::FilesystemBackend;
pub use routing::RoutingBackend;
pub use s3::S3Backend;
pub use traits::{DelegatedListResult, StorageBackend, StorageError};

/// ENOSPC raw error code on Linux and macOS.
const ENOSPC: i32 = 28;

/// Convert an io::Error into StorageError, detecting disk-full and not-found.
pub(crate) fn io_to_storage_error(e: std::io::Error) -> StorageError {
    if e.raw_os_error() == Some(ENOSPC) {
        StorageError::DiskFull
    } else if e.kind() == std::io::ErrorKind::NotFound {
        StorageError::NotFound(e.to_string())
    } else {
        StorageError::Io(e)
    }
}

/// Convert a `tokio::task::JoinError` from `spawn_blocking` into `StorageError`.
pub(crate) fn join_error(e: tokio::task::JoinError) -> StorageError {
    StorageError::Other(format!("spawn_blocking join failed: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_io_to_storage_error_not_found() {
        let e = std::io::Error::new(std::io::ErrorKind::NotFound, "file not found");
        let se = io_to_storage_error(e);
        assert!(
            matches!(se, StorageError::NotFound(_)),
            "ENOENT should map to StorageError::NotFound, got: {:?}",
            se
        );
    }

    #[test]
    fn test_io_to_storage_error_permission_denied() {
        let e = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "access denied");
        let se = io_to_storage_error(e);
        assert!(
            matches!(se, StorageError::Io(_)),
            "EACCES should map to StorageError::Io, got: {:?}",
            se
        );
    }

    #[test]
    fn test_io_to_storage_error_disk_full() {
        let e = std::io::Error::from_raw_os_error(ENOSPC);
        let se = io_to_storage_error(e);
        assert!(
            matches!(se, StorageError::DiskFull),
            "ENOSPC should map to StorageError::DiskFull, got: {:?}",
            se
        );
    }

    #[test]
    fn test_io_to_storage_error_generic() {
        let e = std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "refused");
        let se = io_to_storage_error(e);
        assert!(
            matches!(se, StorageError::Io(_)),
            "Other errors should map to StorageError::Io, got: {:?}",
            se
        );
    }
}
