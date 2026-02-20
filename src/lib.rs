//! DeltaGlider Proxy - S3-compatible object storage with DeltaGlider deduplication
//!
//! This library provides the core functionality for the DeltaGlider Proxy S3 server.

pub mod api;
pub mod config;
pub mod deltaglider;
pub mod init;
pub mod multipart;
pub mod session;
pub mod storage;
pub mod tls;
pub mod types;
