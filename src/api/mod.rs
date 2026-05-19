// SPDX-License-Identifier: GPL-3.0-only

//! S3 API implementation

pub mod admin;
pub mod auth;
pub(crate) mod aws_chunked;
pub(crate) mod errors;
mod extractors;
pub mod handlers;

pub use errors::S3Error;
pub use extractors::{ValidatedBucket, ValidatedPath};

/// Marker type: when present as an Extension, the S3 API rejects all requests.
/// Injected when config DB bootstrap password mismatch is detected.
#[derive(Clone)]
pub struct ConfigDbMismatchGuard;
