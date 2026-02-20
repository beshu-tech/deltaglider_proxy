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

/// Convert an io::Error into StorageError, detecting disk-full (ENOSPC).
pub(crate) fn io_to_storage_error(e: std::io::Error) -> StorageError {
    if e.raw_os_error() == Some(ENOSPC) {
        StorageError::DiskFull
    } else {
        StorageError::Io(e)
    }
}
