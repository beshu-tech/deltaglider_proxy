// SPDX-License-Identifier: BUSL-1.1

//! MinIO: the test bucket, a direct client, and the `skip_unless_minio!` gate.

use aws_credential_types::Credentials;
use aws_sdk_s3::config::{BehaviorVersion, Region};
use aws_sdk_s3::Client;
use std::time::Duration;

/// MinIO configuration constants
pub const MINIO_BUCKET: &str = "deltaglider-test";

/// A bucket name that is unique per call within this process AND across
/// processes: nanosecond timestamp plus a process-wide counter, so two calls
/// in the same clock tick (parallel tests start within microseconds of each
/// other) still get different names.
pub fn unique_bucket(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{prefix}-{ts}-{n}")
}

/// MinIO endpoint — reads MINIO_ENDPOINT env var, falls back to localhost:9000
pub fn minio_endpoint_url() -> String {
    std::env::var("MINIO_ENDPOINT").unwrap_or_else(|_| "http://localhost:9000".to_string())
}
pub const MINIO_ACCESS_KEY: &str = "minioadmin";
pub const MINIO_SECRET_KEY: &str = "minioadmin";

// === MinIO gating ===

/// Create an S3 client pointing directly at MinIO (not through the proxy)
pub async fn minio_client() -> Client {
    let credentials = Credentials::new(MINIO_ACCESS_KEY, MINIO_SECRET_KEY, None, None, "test");
    let config = aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new("us-east-1"))
        .endpoint_url(minio_endpoint_url())
        .credentials_provider(credentials)
        .force_path_style(true)
        .build();
    Client::from_conf(config)
}

/// Check if MinIO is available (TCP probe + HeadBucket with 2s timeout)
pub async fn minio_available() -> bool {
    // Quick TCP check first — parse host:port from endpoint URL
    let endpoint = minio_endpoint_url();
    let addr = endpoint
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    if std::net::TcpStream::connect(addr).is_err() {
        return false;
    }

    let client = minio_client().await;

    // Verify the specific test bucket exists (not just any S3-compatible service)
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        client.head_bucket().bucket(MINIO_BUCKET).send(),
    )
    .await;
    matches!(result, Ok(Ok(_)))
}

/// Macro to skip a test if MinIO is not available.
/// Use at the start of any test that requires MinIO.
#[macro_export]
macro_rules! skip_unless_minio {
    () => {
        if !common::minio_available().await {
            eprintln!("MinIO not available, skipping test");
            return;
        }
    };
}
