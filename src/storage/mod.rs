//! Storage backend abstraction

mod filesystem;
mod s3;
mod traits;

pub use filesystem::FilesystemBackend;
pub use s3::S3Backend;
pub use traits::{StorageBackend, StorageError};
