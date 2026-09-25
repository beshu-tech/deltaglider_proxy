// SPDX-License-Identifier: BUSL-1.1

//! CLI subcommands for the `deltaglider_proxy` binary.
//!
//! Top-level shape: `deltaglider_proxy <subcommand> [args...]`.
//!
//! Each subcommand is a small dispatcher that borrows logic from the library
//! crate. The `config` and `admission` families live in `config.rs`; the
//! AWS-CLI-shaped S3 commands (`cp`, `ls`, `rm`, `stats`, `verify`) each get
//! their own module so help-text and argument shapes don't collide.

pub mod aws_creds;
pub mod bucket_acl;
pub mod config;
pub mod cp;
pub mod engine_factory;
pub mod filter;
pub mod keys;
pub mod ls;
pub mod migrate;
pub mod purge;
pub mod rm;
pub mod s3_url;
pub mod stats;
pub mod sync;
pub mod verify;

/// What an engine error says is missing, if anything: `Some("object")` or
/// `Some("bucket")`. Decided on the error variant, never on its text.
pub(crate) fn missing(e: &crate::deltaglider::EngineError) -> Option<&'static str> {
    use crate::deltaglider::EngineError;
    use crate::storage::StorageError;
    if e.is_not_found() {
        Some("object")
    } else if matches!(e, EngineError::Storage(StorageError::BucketNotFound(_))) {
        Some("bucket")
    } else {
        None
    }
}
