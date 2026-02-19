//! Storage backend abstraction

mod filesystem;
mod s3;
mod traits;
#[cfg(unix)]
pub(crate) mod xattr_meta;

pub use filesystem::FilesystemBackend;
pub use s3::S3Backend;
pub use traits::{DelegatedListResult, StorageBackend, StorageError};
