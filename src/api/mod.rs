//! S3 API implementation

pub mod auth;
mod aws_chunked;
mod errors;
mod extractors;
pub mod handlers;
mod xml;

pub use errors::S3Error;
pub use extractors::{ValidatedBucket, ValidatedPath};
pub use xml::{PartInfo, UploadInfo};
