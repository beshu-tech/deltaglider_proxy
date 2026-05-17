// SPDX-License-Identifier: GPL-3.0-only

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
pub mod ls;
pub mod migrate;
pub mod purge;
pub mod rm;
pub mod s3_url;
pub mod stats;
pub mod sync;
pub mod verify;
