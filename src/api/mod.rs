// SPDX-License-Identifier: BUSL-1.1

//! S3 API implementation

pub mod admin;
pub mod auth;
pub(crate) mod aws_chunked;
pub(crate) mod errors;
pub mod handlers;
pub mod request_target;

pub use errors::S3Error;

/// Marker type: when present as an Extension, the S3 API rejects all requests.
/// Injected when no config DB key opens the config DB (mismatch).
#[derive(Clone)]
pub struct ConfigDbMismatchGuard;
