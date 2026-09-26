// SPDX-License-Identifier: BUSL-1.1

//! S3 API implementation

pub mod admin;
pub mod auth;
pub(crate) mod aws_chunked;
pub(crate) mod errors;
pub mod handlers;
pub mod request_target;
pub mod s3_router;
pub mod s3s_hooks;

#[cfg(test)]
mod s3s_contract_tests;

pub use errors::S3Error;

/// Marker type: when present as an Extension, the S3 API rejects all requests.
/// Injected when no config DB key opens the config DB (mismatch).
#[derive(Clone)]
pub struct ConfigDbMismatchGuard;
