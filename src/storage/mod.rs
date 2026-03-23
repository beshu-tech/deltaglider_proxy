//! Storage backend abstraction

mod filesystem;
mod s3;
mod traits;
#[cfg(unix)]
pub(crate) mod xattr_meta;

pub use filesystem::FilesystemBackend;
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
