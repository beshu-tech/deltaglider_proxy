//! DeltaGlider Proxy - S3-compatible object storage with DeltaGlider deduplication
//!
//! This library provides the core functionality for the DeltaGlider Proxy S3 server.

pub mod admission;
pub mod api;
pub mod audit;
pub mod bucket_policy;
pub mod cli;
pub mod config;
pub mod config_db;
pub mod config_db_sync;
pub mod config_sections;
pub mod deltaglider;
pub mod iam;
pub mod init;
pub mod metadata_cache;
pub mod metrics;
pub mod multipart;
pub mod rate_limiter;
pub mod session;
pub mod storage;
pub mod tls;
pub mod types;
pub mod usage_scanner;
