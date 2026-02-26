//! Shared test infrastructure for integration tests
//!
//! Provides TestServer (filesystem and S3 backends), data generators,
//! and MinIO availability gating.

#![allow(dead_code)]

use aws_credential_types::Credentials;
use aws_sdk_s3::config::{BehaviorVersion, Region};
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
    // ── Factory methods ──

    /// Start a test server with filesystem backend (no Docker needed)
    pub async fn filesystem() -> Self {
        let data_dir = TempDir::new().expect("Failed to create temp dir");
        let config = format!(
            "[backend]\ntype = \"filesystem\"\npath = \"{}\"\n",
            data_dir.path().display()
        );
        Self::spawn_with_config(&config, "bucket", Some(data_dir)).await
    }

    /// Start a test server with filesystem backend and a custom max delta ratio
    pub async fn filesystem_with_max_delta_ratio(max_delta_ratio: f32) -> Self {
        let data_dir = TempDir::new().expect("Failed to create temp dir");
        let config = format!(
            "max_delta_ratio = {}\n\n[backend]\ntype = \"filesystem\"\npath = \"{}\"\n",
            max_delta_ratio,
            data_dir.path().display()
        );
        Self::spawn_with_config(&config, "bucket", Some(data_dir)).await
    }

    /// Start a test server with filesystem backend and a custom max object size
    pub async fn filesystem_with_max_object_size(max_size: u64) -> Self {
        let data_dir = TempDir::new().expect("Failed to create temp dir");
        let config = format!(
            "max_object_size = {}\n\n[backend]\ntype = \"filesystem\"\npath = \"{}\"\n",
            max_size,
            data_dir.path().display()
        );
        Self::spawn_with_config(&config, "bucket", Some(data_dir)).await
    }

    /// Start a test server with filesystem backend and custom codec concurrency
    pub async fn filesystem_with_codec_concurrency(concurrency: usize) -> Self {
        let data_dir = TempDir::new().expect("Failed to create temp dir");
        let config = format!(
            "codec_concurrency = {}\n\n[backend]\ntype = \"filesystem\"\npath = \"{}\"\n",
            concurrency,
            data_dir.path().display()
        );
        Self::spawn_with_config(&config, "bucket", Some(data_dir)).await
    }

    /// Start a test server with S3 backend (needs MinIO running)
    pub async fn s3() -> Self {
        Self::s3_with_endpoint(MINIO_ENDPOINT, MINIO_BUCKET).await
    }

    /// Start a test server with S3 backend pointing at a custom endpoint/bucket.
    pub async fn s3_with_endpoint(endpoint: &str, bucket: &str) -> Self {
        let config = s3_config_block(endpoint, None);
        Self::spawn_with_config(&config, bucket, None).await
    }

    /// Start a test server with S3 backend and a custom max delta ratio.
    pub async fn s3_with_endpoint_and_delta_ratio(
        endpoint: &str,
        bucket: &str,
        max_delta_ratio: f32,
    ) -> Self {
        let config = s3_config_block(endpoint, Some(max_delta_ratio));
        Self::spawn_with_config(&config, bucket, None).await
    }

    // ── Shared spawn logic ──

    /// Allocate a port, write a TOML config, spawn the proxy, wait for readiness,
    /// and create the test bucket. All factory methods delegate here.
    async fn spawn_with_config(config_body: &str, bucket: &str, data_dir: Option<TempDir>) -> Self {
        let port = PORT_COUNTER.fetch_add(2, Ordering::SeqCst);

        // Build full config with listen_addr prepended
        let full_config = format!("listen_addr = \"127.0.0.1:{}\"\n{}", port, config_body);

        // Write config to a temp file (inside data_dir if available, else system temp)
        let config_path = match &data_dir {
            Some(d) => d.path().join("test.toml"),
            None => std::env::temp_dir().join(format!("dgp_test_{}.toml", port)),
        };
        std::fs::write(&config_path, &full_config).expect("Failed to write test config");

        let process = Command::new(env!("CARGO_BIN_EXE_deltaglider_proxy"))
            .env("DGP_CONFIG", &config_path)
            .env("RUST_LOG", "deltaglider_proxy=warn")
            .spawn()
            .expect("Failed to start server");

        let mut server = Self {
            process,
            port,
            _data_dir: data_dir,
            bucket: bucket.to_string(),
        };
        server.wait_ready().await;
        server.ensure_bucket().await;
        server
    }

    // ── Instance methods ──

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

    /// Create the test bucket via the S3 API (replaces the removed DGP_BUCKET auto-create)
    async fn ensure_bucket(&self) {
        let client = self.s3_client().await;
        let _ = client.create_bucket().bucket(&self.bucket).send().await;
    }

    /// Create an S3 client configured for this test server
    pub async fn s3_client(&self) -> Client {
        let credentials = Credentials::new("test", "test", None, None, "test");

        let config = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new("us-east-1"))
            .endpoint_url(self.endpoint())
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
}

/// Build the S3 backend TOML config block with optional extra settings.
fn s3_config_block(endpoint: &str, max_delta_ratio: Option<f32>) -> String {
    let mut config = String::new();
    if let Some(ratio) = max_delta_ratio {
        config.push_str(&format!("max_delta_ratio = {}\n\n", ratio));
    }
    config.push_str(&format!(
        concat!(
            "[backend]\n",
            "type = \"s3\"\n",
            "endpoint = \"{}\"\n",
            "region = \"us-east-1\"\n",
            "force_path_style = true\n",
            "access_key_id = \"{}\"\n",
            "secret_access_key = \"{}\"\n",
        ),
        endpoint, MINIO_ACCESS_KEY, MINIO_SECRET_KEY,
    ));
    config
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = self.process.kill();
    }
}

// === Shared HTTP helpers (reqwest) ===

/// PUT an object via reqwest and return the response.
pub async fn put_object(
    client: &reqwest::Client,
    endpoint: &str,
    bucket: &str,
    key: &str,
    data: Vec<u8>,
    content_type: &str,
) -> reqwest::Response {
    let url = format!("{}/{}/{}", endpoint, bucket, key);
    let resp = client
        .put(&url)
        .header("content-type", content_type)
        .body(data)
        .send()
        .await
        .expect("PUT failed");
    assert!(
        resp.status().is_success(),
        "PUT {} failed: {}",
        key,
        resp.status()
    );
    resp
}

/// PUT an object and return the x-amz-storage-type header value.
pub async fn put_and_get_storage_type(
    client: &reqwest::Client,
    endpoint: &str,
    bucket: &str,
    key: &str,
    data: Vec<u8>,
    content_type: &str,
) -> String {
    let resp = put_object(client, endpoint, bucket, key, data, content_type).await;
    resp.headers()
        .get("x-amz-storage-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string()
}

/// GET an object and return the body bytes.
pub async fn get_bytes(
    client: &reqwest::Client,
    endpoint: &str,
    bucket: &str,
    key: &str,
) -> Vec<u8> {
    let url = format!("{}/{}/{}", endpoint, bucket, key);
    let resp = client.get(&url).send().await.expect("GET failed");
    assert!(
        resp.status().is_success(),
        "GET {} failed: {}",
        key,
        resp.status()
    );
    resp.bytes().await.unwrap().to_vec()
}

/// HEAD an object and return response headers.
pub async fn head_headers(
    client: &reqwest::Client,
    endpoint: &str,
    bucket: &str,
    key: &str,
) -> reqwest::header::HeaderMap {
    let url = format!("{}/{}/{}", endpoint, bucket, key);
    let resp = client.head(&url).send().await.expect("HEAD failed");
    assert!(
        resp.status().is_success(),
        "HEAD {} failed: {}",
        key,
        resp.status()
    );
    resp.headers().clone()
}

/// DELETE an object via reqwest (tolerates 204 and 404).
pub async fn delete_object(client: &reqwest::Client, endpoint: &str, bucket: &str, key: &str) {
    let url = format!("{}/{}/{}", endpoint, bucket, key);
    let resp = client.delete(&url).send().await.expect("DELETE failed");
    assert!(
        resp.status().is_success()
            || resp.status().as_u16() == 204
            || resp.status().as_u16() == 404,
        "DELETE {} failed: {}",
        key,
        resp.status()
    );
}

/// Make a raw ListObjectsV2 request and return the XML body.
pub async fn list_objects_raw(
    client: &reqwest::Client,
    endpoint: &str,
    bucket: &str,
    params: &str,
) -> String {
    let url = format!("{}/{}?list-type=2&{}", endpoint, bucket, params);
    let resp = client.get(&url).send().await.unwrap();
    assert!(
        resp.status().is_success(),
        "ListObjects failed: {}",
        resp.status()
    );
    resp.text().await.unwrap()
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

/// Create an S3 client pointing directly at MinIO (not through the proxy)
pub async fn minio_client() -> Client {
    let credentials = Credentials::new(MINIO_ACCESS_KEY, MINIO_SECRET_KEY, None, None, "test");
    let config = aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new("us-east-1"))
        .endpoint_url(MINIO_ENDPOINT)
        .credentials_provider(credentials)
        .force_path_style(true)
        .build();
    Client::from_conf(config)
}

/// Check if MinIO is available (TCP probe + HeadBucket with 2s timeout)
pub async fn minio_available() -> bool {
    // Quick TCP check first
    if std::net::TcpStream::connect("localhost:9000").is_err() {
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
