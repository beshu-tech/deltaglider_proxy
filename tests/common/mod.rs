//! Shared test infrastructure for integration tests
//!
//! Provides TestServer (filesystem and S3 backends), data generators,
//! and MinIO availability gating.

#![allow(dead_code)]

use aws_config::BehaviorVersion;
use aws_credential_types::Credentials;
use aws_sdk_s3::config::Region;
use aws_sdk_s3::Client;
use rand::{Rng, SeedableRng};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::sleep;

/// Port counter to avoid conflicts between tests.
/// Increments by 2 because each server uses two ports: S3 (N) and demo UI (N+1).
static PORT_COUNTER: AtomicU16 = AtomicU16::new(19000);

/// MinIO configuration constants
pub const MINIO_ENDPOINT: &str = "http://localhost:9000";
pub const MINIO_BUCKET: &str = "deltaglider-test";
pub const MINIO_ACCESS_KEY: &str = "minioadmin";
pub const MINIO_SECRET_KEY: &str = "minioadmin";

/// Test server wrapper that spawns a real deltaglider_proxy binary
pub struct TestServer {
    process: Child,
    port: u16,
    _data_dir: Option<TempDir>,
    bucket: String,
}

impl TestServer {
    /// Start a test server with filesystem backend (no Docker needed)
    pub async fn filesystem() -> Self {
        let port = PORT_COUNTER.fetch_add(2, Ordering::SeqCst);
        let data_dir = TempDir::new().expect("Failed to create temp dir");

        let process = Command::new(env!("CARGO_BIN_EXE_deltaglider_proxy"))
            .env(
                "DELTAGLIDER_PROXY_LISTEN_ADDR",
                format!("127.0.0.1:{}", port),
            )
            .env("DELTAGLIDER_PROXY_DATA_DIR", data_dir.path())
            .env("DELTAGLIDER_PROXY_DEFAULT_BUCKET", "bucket")
            .env("RUST_LOG", "deltaglider_proxy=warn")
            .spawn()
            .expect("Failed to start server");

        let mut server = Self {
            process,
            port,
            _data_dir: Some(data_dir),
            bucket: "bucket".to_string(),
        };
        server.wait_ready().await;
        server
    }

    /// Start a test server with S3 backend (needs MinIO running)
    pub async fn s3() -> Self {
        Self::s3_with_endpoint(MINIO_ENDPOINT, MINIO_BUCKET).await
    }

    /// Start a test server with S3 backend pointing at a custom endpoint/bucket.
    /// Useful for ephemeral MinIO containers with dynamic ports.
    pub async fn s3_with_endpoint(endpoint: &str, bucket: &str) -> Self {
        let port = PORT_COUNTER.fetch_add(2, Ordering::SeqCst);

        let process = Command::new(env!("CARGO_BIN_EXE_deltaglider_proxy"))
            .env(
                "DELTAGLIDER_PROXY_LISTEN_ADDR",
                format!("127.0.0.1:{}", port),
            )
            .env("DELTAGLIDER_PROXY_S3_ENDPOINT", endpoint)
            .env("DELTAGLIDER_PROXY_S3_FORCE_PATH_STYLE", "true")
            .env("AWS_ACCESS_KEY_ID", MINIO_ACCESS_KEY)
            .env("AWS_SECRET_ACCESS_KEY", MINIO_SECRET_KEY)
            .env("DELTAGLIDER_PROXY_DEFAULT_BUCKET", bucket)
            .env("RUST_LOG", "deltaglider_proxy=warn")
            .spawn()
            .expect("Failed to start server");

        let mut server = Self {
            process,
            port,
            _data_dir: None,
            bucket: bucket.to_string(),
        };
        server.wait_ready().await;
        server
    }

    async fn wait_ready(&mut self) {
        let addr = format!("127.0.0.1:{}", self.port);
        for _ in 0..150 {
            if std::net::TcpStream::connect(&addr).is_ok() {
                sleep(Duration::from_millis(100)).await;
                return;
            }

            if let Ok(Some(status)) = self.process.try_wait() {
                panic!("Server exited before becoming ready: {}", status);
            }

            sleep(Duration::from_millis(100)).await;
        }

        let _ = self.process.kill();
        panic!("Timed out waiting for server on {}", addr);
    }

    /// Create an S3 client configured for this test server
    pub async fn s3_client(&self) -> Client {
        let credentials = Credentials::new("test", "test", None, None, "test");

        let config = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new("us-east-1"))
            .endpoint_url(format!("http://127.0.0.1:{}", self.port))
            .credentials_provider(credentials)
            .force_path_style(true)
            .build();

        Client::from_conf(config)
    }

    /// Get the HTTP endpoint URL
    pub fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Get the bucket name
    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    /// Get the child process PID
    pub fn pid(&self) -> u32 {
        self.process.id()
    }

    /// Start a test server with filesystem backend and a custom max object size
    pub async fn filesystem_with_max_object_size(max_size: u64) -> Self {
        let port = PORT_COUNTER.fetch_add(2, Ordering::SeqCst);
        let data_dir = TempDir::new().expect("Failed to create temp dir");

        let process = Command::new(env!("CARGO_BIN_EXE_deltaglider_proxy"))
            .env(
                "DELTAGLIDER_PROXY_LISTEN_ADDR",
                format!("127.0.0.1:{}", port),
            )
            .env("DELTAGLIDER_PROXY_DATA_DIR", data_dir.path())
            .env("DELTAGLIDER_PROXY_DEFAULT_BUCKET", "bucket")
            .env(
                "DELTAGLIDER_PROXY_MAX_OBJECT_SIZE",
                max_size.to_string(),
            )
            .env("RUST_LOG", "deltaglider_proxy=warn")
            .spawn()
            .expect("Failed to start server");

        let mut server = Self {
            process,
            port,
            _data_dir: Some(data_dir),
            bucket: "bucket".to_string(),
        };
        server.wait_ready().await;
        server
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = self.process.kill();
    }
}

// === Data generators ===

/// Generate deterministic binary data
pub fn generate_binary(size: usize, seed: u64) -> Vec<u8> {
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let mut data = vec![0u8; size];
    rng.fill(&mut data[..]);
    data
}

/// Mutate binary data by changing a percentage of bytes
pub fn mutate_binary(data: &[u8], change_ratio: f64) -> Vec<u8> {
    let mut result = data.to_vec();
    let changes = (data.len() as f64 * change_ratio) as usize;
    let mut rng = rand::thread_rng();

    for _ in 0..changes {
        let idx = rng.gen_range(0..result.len());
        result[idx] = rng.gen();
    }

    result
}

// === MinIO gating ===

/// Check if MinIO is available (TCP probe + ListBuckets with 2s timeout)
pub async fn minio_available() -> bool {
    // Quick TCP check first
    if std::net::TcpStream::connect("localhost:9000").is_err() {
        return false;
    }

    let credentials = Credentials::new(MINIO_ACCESS_KEY, MINIO_SECRET_KEY, None, None, "test");

    let config = aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new("us-east-1"))
        .endpoint_url(MINIO_ENDPOINT)
        .credentials_provider(credentials)
        .force_path_style(true)
        .build();

    let client = Client::from_conf(config);

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

/// Check if Docker is available by running `docker version`
pub fn docker_available() -> bool {
    Command::new("docker")
        .arg("version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Macro to skip a test if Docker is not available.
/// Use at the start of any test that requires an ephemeral container.
#[macro_export]
macro_rules! skip_unless_docker {
    () => {
        if !common::docker_available() {
            eprintln!("Docker not available, skipping test");
            return;
        }
    };
}
