//! S3 API implementation

pub mod admin;
pub mod auth;
mod aws_chunked;
pub(crate) mod errors;
mod extractors;
pub mod handlers;
mod xml;

pub use errors::S3Error;
pub use extractors::{ValidatedBucket, ValidatedPath};
pub use xml::{PartInfo, UploadInfo};

/// Marker type: when present as an Extension, the S3 API rejects all requests.
/// Injected when config DB bootstrap password mismatch is detected.
#[derive(Clone)]
pub struct ConfigDbMismatchGuard;
