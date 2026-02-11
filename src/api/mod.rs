//! S3 API implementation

mod aws_chunked;
mod errors;
mod extractors;
pub mod handlers;
mod xml;

pub use errors::S3Error;
pub use extractors::{ValidatedBucket, ValidatedPath};
