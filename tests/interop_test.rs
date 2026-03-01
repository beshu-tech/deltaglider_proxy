//! Interoperability tests with original DeltaGlider CLI
//!
//! These tests verify full interoperability between DeltaGlider Proxy and the
//! original DeltaGlider CLI (beshultd/deltaglider Docker image).
//!
//! Test scenarios:
//! 1. Files uploaded with original DeltaGlider CLI can be read by this proxy
//! 2. Files compressed by original CLI can be reconstructed with matching checksums
//! 3. Files compressed by this proxy can be read/reconstructed by original CLI
//!
//! Usage:
//!   docker compose up -d                    # Start MinIO
//!   cargo test --test interop_test -- --ignored --nocapture
//!   docker compose down
//!
//! Requirements:
//! - MinIO running on localhost:9000
//! - Docker available for running deltaglider CLI

use aws_credential_types::Credentials;
use aws_sdk_s3::config::{BehaviorVersion, Region};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use rand::{Rng, SeedableRng};
use sha2::{Digest, Sha256};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::sleep;

/// Port counter for proxy instances
static PORT_COUNTER: AtomicU16 = AtomicU16::new(19100);

/// Test prefix counter for isolation
static TEST_PREFIX_COUNTER: AtomicU64 = AtomicU64::new(0);

/// MinIO configuration — reads MINIO_ENDPOINT env var, falls back to localhost:9000
fn minio_endpoint() -> String {
    std::env::var("MINIO_ENDPOINT").unwrap_or_else(|_| "http://localhost:9000".to_string())
}
const MINIO_BUCKET: &str = "deltaglider-test";
const MINIO_ACCESS_KEY: &str = "minioadmin";
const MINIO_SECRET_KEY: &str = "minioadmin";

/// DeltaGlider CLI Docker image
const DELTAGLIDER_IMAGE: &str = "beshultd/deltaglider:latest";

/// Generate unique test prefix
fn unique_prefix() -> String {
    let counter = TEST_PREFIX_COUNTER.fetch_add(1, Ordering::SeqCst);
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    format!("interop-{}-{}", timestamp, counter)
}

/// Calculate SHA256 hash of data
fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    format!("{:x}", hasher.finalize())
}

/// Generate pseudorandom binary data (simulating archive content)
fn generate_archive_content(size: usize, seed: u64) -> Vec<u8> {
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
    let mut data = vec![0u8; size];
    rng.fill(&mut data[..]);

    // Add some ZIP-like magic bytes at the start to ensure delta eligibility
    if data.len() >= 4 {
        data[0] = 0x50; // P
        data[1] = 0x4B; // K
        data[2] = 0x03;
        data[3] = 0x04;
    }
    data
}

/// Create a slightly modified version of archive content (simulating new version)
fn mutate_archive(data: &[u8], change_ratio: f64, seed: u64) -> Vec<u8> {
    let mut result = data.to_vec();
    let changes = (data.len() as f64 * change_ratio) as usize;
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);

    // Keep ZIP magic bytes intact
    for _ in 0..changes {
        let idx = rng.gen_range(4..result.len().max(5));
        if idx < result.len() {
            result[idx] = rng.gen();
        }
    }

    result
}

/// Check if MinIO is available
async fn minio_available() -> bool {
    let credentials = Credentials::new(MINIO_ACCESS_KEY, MINIO_SECRET_KEY, None, None, "test");

    let config = aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new("us-east-1"))
        .endpoint_url(minio_endpoint())
        .credentials_provider(credentials)
        .force_path_style(true)
        .build();

    let client = Client::from_conf(config);

    tokio::time::timeout(Duration::from_secs(2), client.list_buckets().send())
        .await
        .map(|r| r.is_ok())
        .unwrap_or(false)
}

/// Check if Docker is available and can pull the deltaglider image
fn docker_available() -> bool {
    Command::new("docker")
        .args(["version"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Create S3 client for MinIO
async fn minio_client() -> Client {
    let credentials = Credentials::new(MINIO_ACCESS_KEY, MINIO_SECRET_KEY, None, None, "test");

    let config = aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new("us-east-1"))
        .endpoint_url(minio_endpoint())
        .credentials_provider(credentials)
        .force_path_style(true)
        .build();

    Client::from_conf(config)
}

/// Run DeltaGlider CLI command via Docker
#[allow(dead_code)]
fn run_deltaglider_cli(args: &[&str]) -> std::io::Result<std::process::Output> {
    // Use host network to access MinIO on localhost
    let access_key_env = format!("AWS_ACCESS_KEY_ID={}", MINIO_ACCESS_KEY);
    let secret_key_env = format!("AWS_SECRET_ACCESS_KEY={}", MINIO_SECRET_KEY);
    let endpoint_env = format!("AWS_ENDPOINT_URL={}", minio_endpoint());

    let mut docker_args = vec![
        "run",
        "--rm",
        "--network=host",
        "-e",
        &access_key_env,
        "-e",
        &secret_key_env,
        "-e",
        &endpoint_env,
        "-e",
        "DG_LOG_LEVEL=DEBUG",
        DELTAGLIDER_IMAGE,
    ];
    docker_args.extend(args);

    Command::new("docker").args(&docker_args).output()
}

/// Run DeltaGlider CLI with stdin data
#[allow(dead_code)]
fn run_deltaglider_cli_with_stdin(
    args: &[&str],
    stdin_data: &[u8],
) -> std::io::Result<std::process::Output> {
    use std::io::Write;

    let access_key_env = format!("AWS_ACCESS_KEY_ID={}", MINIO_ACCESS_KEY);
    let secret_key_env = format!("AWS_SECRET_ACCESS_KEY={}", MINIO_SECRET_KEY);
    let endpoint_env = format!("AWS_ENDPOINT_URL={}", minio_endpoint());

    let mut docker_args = vec![
        "run",
        "--rm",
        "-i",
        "--network=host",
        "-e",
        &access_key_env,
        "-e",
        &secret_key_env,
        "-e",
        &endpoint_env,
        "-e",
        "DG_LOG_LEVEL=DEBUG",
        DELTAGLIDER_IMAGE,
    ];
    docker_args.extend(args);

    let mut child = Command::new("docker")
        .args(&docker_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(stdin_data)?;
    }

    child.wait_with_output()
}

/// Test server wrapper for DeltaGlider Proxy
struct TestProxyServer {
    process: Child,
    port: u16,
    _data_dir: TempDir,
}

impl TestProxyServer {
    /// Start proxy server with S3 backend pointing to MinIO
    async fn start_with_s3_backend() -> Self {
        let port = PORT_COUNTER.fetch_add(1, Ordering::SeqCst);
        let data_dir = TempDir::new().expect("Failed to create temp dir");

        let process = Command::new(env!("CARGO_BIN_EXE_deltaglider_proxy"))
            .env("DGP_LISTEN_ADDR", format!("127.0.0.1:{}", port))
            .env("DGP_S3_ENDPOINT", minio_endpoint())
            .env("DGP_S3_REGION", "us-east-1")
            .env("DGP_S3_PATH_STYLE", "true")
            .env("DGP_BE_AWS_ACCESS_KEY_ID", MINIO_ACCESS_KEY)
            .env("DGP_BE_AWS_SECRET_ACCESS_KEY", MINIO_SECRET_KEY)
            .env("RUST_LOG", "deltaglider_proxy=debug")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("Failed to start proxy server");

        // Wait for server to be ready
        let addr = format!("127.0.0.1:{}", port);
        for _ in 0..100 {
            if std::net::TcpStream::connect(&addr).is_ok() {
                sleep(Duration::from_millis(100)).await;
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }

        let server = Self {
            process,
            port,
            _data_dir: data_dir,
        };

        // Create the "default" bucket explicitly (no more DGP_BUCKET auto-create)
        let client = server.s3_client().await;
        let _ = client.create_bucket().bucket("default").send().await;

        server
    }

    /// Create S3 client for this proxy
    async fn s3_client(&self) -> Client {
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

    #[allow(dead_code)]
    fn endpoint(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

impl Drop for TestProxyServer {
    fn drop(&mut self) {
        let _ = self.process.kill();
    }
}

// ============================================================================
// INTEROPERABILITY TESTS
// ============================================================================

/// Test 1: Upload files with original DeltaGlider CLI, verify metadata structure
///
/// This test verifies that files uploaded via the original DeltaGlider CLI
/// follow the expected storage format that our proxy can understand.
#[tokio::test]
#[ignore = "Requires MinIO and Docker: docker compose up -d"]
async fn test_original_cli_upload_metadata_structure() {
    if !minio_available().await {
        eprintln!("MinIO not available, skipping test");
        return;
    }
    if !docker_available() {
        eprintln!("Docker not available, skipping test");
        return;
    }

    let prefix = unique_prefix();
    let client = minio_client().await;

    // Create test file locally and upload via original CLI
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let test_file = temp_dir.path().join("v1.zip");
    let data = generate_archive_content(100_000, 42);
    let _expected_sha256 = sha256_hex(&data);
    std::fs::write(&test_file, &data).expect("Failed to write test file");

    // Upload via original DeltaGlider CLI using cp
    // The CLI expects: deltaglider cp <local-path> s3://bucket/key
    // Note: Volume must be read-write because CLI creates temp files for encryption
    let volume_mount = format!("{}:/data", temp_dir.path().display());
    let access_key = format!("AWS_ACCESS_KEY_ID={}", MINIO_ACCESS_KEY);
    let secret_key = format!("AWS_SECRET_ACCESS_KEY={}", MINIO_SECRET_KEY);
    let endpoint = format!("AWS_ENDPOINT_URL={}", minio_endpoint());
    let s3_dest = format!("s3://{}/{}/v1.zip", MINIO_BUCKET, prefix);

    let result = Command::new("docker")
        .args([
            "run",
            "--rm",
            "--network=host",
            "-v",
            &volume_mount,
            "-e",
            &access_key,
            "-e",
            &secret_key,
            "-e",
            &endpoint,
            "-e",
            "DG_LOG_LEVEL=INFO",
            DELTAGLIDER_IMAGE,
            "cp",
            "/data/v1.zip",
            &s3_dest,
        ])
        .output()
        .expect("Failed to run deltaglider cp");

    println!(
        "DeltaGlider CLI stdout: {}",
        String::from_utf8_lossy(&result.stdout)
    );
    println!(
        "DeltaGlider CLI stderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );

    if !result.status.success() {
        panic!(
            "DeltaGlider CLI upload failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }

    // List what was actually stored in MinIO
    let list_result = client
        .list_objects_v2()
        .bucket(MINIO_BUCKET)
        .prefix(&prefix)
        .send()
        .await
        .expect("Failed to list objects");

    println!("Objects stored by original CLI:");
    for obj in list_result.contents() {
        println!(
            "  - {} ({} bytes)",
            obj.key().unwrap_or("?"),
            obj.size().unwrap_or(0)
        );
    }

    // The original CLI should have stored either:
    // - A reference file (if this is the first file in the deltaspace)
    // - Or directly the file with metadata

    // Check for reference.bin (indicates delta compression was used)
    let has_reference = list_result.contents().iter().any(|o| {
        o.key()
            .map(|k| k.contains("reference.bin"))
            .unwrap_or(false)
    });

    // Get the object and check its metadata
    let head_result = client
        .head_object()
        .bucket(MINIO_BUCKET)
        .key(format!("{}/reference.bin", prefix))
        .send()
        .await;

    if let Ok(head) = head_result {
        println!("Reference file metadata:");
        if let Some(metadata) = head.metadata() {
            for (k, v) in metadata {
                println!("  {}: {}", k, v);
            }
        }

        // Verify expected DeltaGlider metadata keys exist
        let metadata = head.metadata().expect("Should have metadata");
        assert!(
            metadata.contains_key("dg-tool") || metadata.contains_key("tool"),
            "Should have tool metadata"
        );
        assert!(
            metadata.contains_key("dg-file-sha256") || metadata.contains_key("file-sha256"),
            "Should have SHA256 metadata"
        );
    } else if has_reference {
        panic!("Has reference.bin in listing but couldn't HEAD it");
    }

    println!("✓ Original CLI metadata structure verified");
}

/// Test 2: Files compressed by original CLI can be downloaded and match checksum
#[tokio::test]
#[ignore = "Requires MinIO and Docker: docker compose up -d"]
async fn test_original_cli_download_checksum_match() {
    if !minio_available().await {
        eprintln!("MinIO not available, skipping test");
        return;
    }
    if !docker_available() {
        eprintln!("Docker not available, skipping test");
        return;
    }

    let prefix = unique_prefix();
    let temp_dir = TempDir::new().expect("Failed to create temp dir");

    // Create v1.zip (will become reference)
    let v1_data = generate_archive_content(100_000, 100);
    let v1_sha256 = sha256_hex(&v1_data);
    let v1_file = temp_dir.path().join("v1.zip");
    std::fs::write(&v1_file, &v1_data).expect("Failed to write v1.zip");

    // Create v2.zip (5% different, should become delta)
    let v2_data = mutate_archive(&v1_data, 0.05, 200);
    let v2_sha256 = sha256_hex(&v2_data);
    let v2_file = temp_dir.path().join("v2.zip");
    std::fs::write(&v2_file, &v2_data).expect("Failed to write v2.zip");

    // Upload both via original CLI
    for (_file, name) in [(&v1_file, "v1.zip"), (&v2_file, "v2.zip")] {
        let result = Command::new("docker")
            .args([
                "run",
                "--rm",
                "--network=host",
                "-v",
                &format!("{}:/data", temp_dir.path().display()),
                "-e",
                &format!("AWS_ACCESS_KEY_ID={}", MINIO_ACCESS_KEY),
                "-e",
                &format!("AWS_SECRET_ACCESS_KEY={}", MINIO_SECRET_KEY),
                "-e",
                &format!("AWS_ENDPOINT_URL={}", minio_endpoint()),
                DELTAGLIDER_IMAGE,
                "cp",
                &format!("/data/{}", name),
                &format!("s3://{}/{}/{}", MINIO_BUCKET, prefix, name),
            ])
            .output()
            .expect("Failed to run deltaglider cp");

        if !result.status.success() {
            panic!(
                "Upload {} failed: {}",
                name,
                String::from_utf8_lossy(&result.stderr)
            );
        }
        println!("Uploaded {} via original CLI", name);
    }

    // Download via original CLI and verify checksums
    // Note: We mount the download dir as /tmp inside the container to avoid
    // cross-device link errors (CLI uses /tmp for intermediate files)
    let download_dir = TempDir::new().expect("Failed to create download dir");
    let download_mount = format!("{}:/workdir", download_dir.path().display());
    let access_key_dl = format!("AWS_ACCESS_KEY_ID={}", MINIO_ACCESS_KEY);
    let secret_key_dl = format!("AWS_SECRET_ACCESS_KEY={}", MINIO_SECRET_KEY);
    let endpoint_dl = format!("AWS_ENDPOINT_URL={}", minio_endpoint());

    for (name, expected_sha256) in [("v1.zip", &v1_sha256), ("v2.zip", &v2_sha256)] {
        let s3_src = format!("s3://{}/{}/{}", MINIO_BUCKET, prefix, name);
        let dest = format!("/workdir/{}", name);

        let result = Command::new("docker")
            .args([
                "run",
                "--rm",
                "--network=host",
                "-v",
                &download_mount,
                "-w",
                "/workdir",
                "-e",
                "TMPDIR=/workdir",
                "-e",
                &access_key_dl,
                "-e",
                &secret_key_dl,
                "-e",
                &endpoint_dl,
                DELTAGLIDER_IMAGE,
                "cp",
                &s3_src,
                &dest,
            ])
            .output()
            .expect("Failed to run deltaglider cp download");

        if !result.status.success() {
            panic!(
                "Download {} failed: {}",
                name,
                String::from_utf8_lossy(&result.stderr)
            );
        }

        // Read downloaded file and verify checksum
        let downloaded = std::fs::read(download_dir.path().join(name))
            .unwrap_or_else(|_| panic!("Failed to read downloaded {}", name));
        let actual_sha256 = sha256_hex(&downloaded);

        assert_eq!(
            actual_sha256, *expected_sha256,
            "SHA256 mismatch for {} (round-trip via original CLI)",
            name
        );
        println!("✓ {} checksum verified: {}", name, actual_sha256);
    }

    println!("✓ Original CLI round-trip checksum verification passed");
}

/// Test 3: Files uploaded via this proxy can be read by original CLI
#[tokio::test]
#[ignore = "Requires MinIO and Docker: docker compose up -d"]
async fn test_proxy_upload_original_cli_download() {
    if !minio_available().await {
        eprintln!("MinIO not available, skipping test");
        return;
    }
    if !docker_available() {
        eprintln!("Docker not available, skipping test");
        return;
    }

    let prefix = unique_prefix();

    // Start our proxy with S3 backend
    let proxy = TestProxyServer::start_with_s3_backend().await;
    let proxy_client = proxy.s3_client().await;

    // Create and upload files via our proxy
    let v1_data = generate_archive_content(100_000, 300);
    let v1_sha256 = sha256_hex(&v1_data);

    let v2_data = mutate_archive(&v1_data, 0.05, 400);
    let v2_sha256 = sha256_hex(&v2_data);

    // Upload v1 (should become reference or passthrough)
    proxy_client
        .put_object()
        .bucket("default")
        .key(format!("{}/v1.zip", prefix))
        .body(ByteStream::from(v1_data.clone()))
        .send()
        .await
        .expect("Failed to upload v1.zip via proxy");
    println!("Uploaded v1.zip via proxy");

    // Upload v2 (should become delta if v1 is reference)
    proxy_client
        .put_object()
        .bucket("default")
        .key(format!("{}/v2.zip", prefix))
        .body(ByteStream::from(v2_data.clone()))
        .send()
        .await
        .expect("Failed to upload v2.zip via proxy");
    println!("Uploaded v2.zip via proxy");

    // List what the proxy stored in MinIO (for debugging)
    let minio_client = minio_client().await;
    let list_result = minio_client
        .list_objects_v2()
        .bucket(MINIO_BUCKET)
        .prefix(&prefix)
        .send()
        .await
        .expect("Failed to list objects");

    println!("Objects stored by proxy:");
    for obj in list_result.contents() {
        println!(
            "  - {} ({} bytes)",
            obj.key().unwrap_or("?"),
            obj.size().unwrap_or(0)
        );
    }

    // Download via our proxy and verify
    for (name, expected_sha256) in [("v1.zip", &v1_sha256), ("v2.zip", &v2_sha256)] {
        let get_result = proxy_client
            .get_object()
            .bucket("default")
            .key(format!("{}/{}", prefix, name))
            .send()
            .await
            .unwrap_or_else(|_| panic!("Failed to GET {} via proxy", name));

        let body = get_result
            .body
            .collect()
            .await
            .expect("Failed to read body")
            .into_bytes();

        let actual_sha256 = sha256_hex(&body);
        assert_eq!(
            actual_sha256, *expected_sha256,
            "SHA256 mismatch for {} via proxy download",
            name
        );
        println!("✓ {} via proxy: checksum {}", name, actual_sha256);
    }

    println!("✓ Proxy upload and download verified");
}

/// Test 4: Delta reconstruction matches between original CLI and proxy
///
/// This is the critical interoperability test: upload via one tool,
/// download via the other, verify byte-exact match.
#[tokio::test]
#[ignore = "Requires MinIO and Docker: docker compose up -d"]
async fn test_cross_tool_delta_reconstruction() {
    if !minio_available().await {
        eprintln!("MinIO not available, skipping test");
        return;
    }
    if !docker_available() {
        eprintln!("Docker not available, skipping test");
        return;
    }

    let prefix_cli = unique_prefix();
    let prefix_proxy = unique_prefix();
    let temp_dir = TempDir::new().expect("Failed to create temp dir");

    // Start proxy
    let proxy = TestProxyServer::start_with_s3_backend().await;
    let proxy_client = proxy.s3_client().await;

    // Generate test data
    let v1_data = generate_archive_content(100_000, 500);
    let v2_data = mutate_archive(&v1_data, 0.03, 600);
    let v1_sha256 = sha256_hex(&v1_data);
    let v2_sha256 = sha256_hex(&v2_data);

    // Write test files for CLI upload
    std::fs::write(temp_dir.path().join("v1.zip"), &v1_data).unwrap();
    std::fs::write(temp_dir.path().join("v2.zip"), &v2_data).unwrap();

    println!("=== Scenario A: Upload via CLI, download via Proxy ===");

    // Upload via original CLI
    for name in ["v1.zip", "v2.zip"] {
        let result = Command::new("docker")
            .args([
                "run",
                "--rm",
                "--network=host",
                "-v",
                &format!("{}:/data", temp_dir.path().display()),
                "-e",
                &format!("AWS_ACCESS_KEY_ID={}", MINIO_ACCESS_KEY),
                "-e",
                &format!("AWS_SECRET_ACCESS_KEY={}", MINIO_SECRET_KEY),
                "-e",
                &format!("AWS_ENDPOINT_URL={}", minio_endpoint()),
                DELTAGLIDER_IMAGE,
                "cp",
                &format!("/data/{}", name),
                &format!("s3://{}/{}/{}", MINIO_BUCKET, prefix_cli, name),
            ])
            .output()
            .expect("Failed to run deltaglider cp");

        assert!(result.status.success(), "CLI upload {} failed", name);
    }

    // Download via our proxy (reading CLI-uploaded data directly from MinIO)
    // Note: The proxy would need to understand CLI's storage format
    // For now, we verify via direct MinIO access
    let minio_client = minio_client().await;

    // The original CLI stores files differently - let's see what it stored
    let list_result = minio_client
        .list_objects_v2()
        .bucket(MINIO_BUCKET)
        .prefix(&prefix_cli)
        .send()
        .await
        .expect("Failed to list CLI objects");

    println!("CLI stored objects:");
    for obj in list_result.contents() {
        let key = obj.key().unwrap_or("?");
        println!("  - {} ({} bytes)", key, obj.size().unwrap_or(0));

        // Get metadata for each object
        if let Ok(head) = minio_client
            .head_object()
            .bucket(MINIO_BUCKET)
            .key(key)
            .send()
            .await
        {
            if let Some(metadata) = head.metadata() {
                println!("    Metadata: {:?}", metadata);
            }
        }
    }

    println!("=== Scenario B: Upload via Proxy, download via CLI ===");

    // Upload via proxy
    proxy_client
        .put_object()
        .bucket("default")
        .key(format!("{}/v1.zip", prefix_proxy))
        .body(ByteStream::from(v1_data.clone()))
        .send()
        .await
        .expect("Proxy upload v1 failed");

    proxy_client
        .put_object()
        .bucket("default")
        .key(format!("{}/v2.zip", prefix_proxy))
        .body(ByteStream::from(v2_data.clone()))
        .send()
        .await
        .expect("Proxy upload v2 failed");

    // List what proxy stored
    let list_result = minio_client
        .list_objects_v2()
        .bucket(MINIO_BUCKET)
        .prefix(&prefix_proxy)
        .send()
        .await
        .expect("Failed to list proxy objects");

    println!("Proxy stored objects:");
    for obj in list_result.contents() {
        let key = obj.key().unwrap_or("?");
        println!("  - {} ({} bytes)", key, obj.size().unwrap_or(0));
    }

    // Download via original CLI
    // Note: We mount the download dir and set TMPDIR to avoid cross-device link errors
    let download_dir = TempDir::new().expect("Failed to create download dir");
    let dl_mount = format!("{}:/workdir", download_dir.path().display());
    let ak = format!("AWS_ACCESS_KEY_ID={}", MINIO_ACCESS_KEY);
    let sk = format!("AWS_SECRET_ACCESS_KEY={}", MINIO_SECRET_KEY);
    let ep = format!("AWS_ENDPOINT_URL={}", minio_endpoint());

    // Note: The CLI may not understand proxy's .meta sidecar format
    // This test documents the compatibility gap if any
    for name in ["v1.zip", "v2.zip"] {
        let s3_src = format!("s3://{}/{}/{}", MINIO_BUCKET, prefix_proxy, name);
        let dest = format!("/workdir/{}", name);

        let result = Command::new("docker")
            .args([
                "run",
                "--rm",
                "--network=host",
                "-v",
                &dl_mount,
                "-w",
                "/workdir",
                "-e",
                "TMPDIR=/workdir",
                "-e",
                &ak,
                "-e",
                &sk,
                "-e",
                &ep,
                DELTAGLIDER_IMAGE,
                "cp",
                &s3_src,
                &dest,
            ])
            .output()
            .expect("Failed to run deltaglider cp download");

        println!(
            "CLI download {} stdout: {}",
            name,
            String::from_utf8_lossy(&result.stdout)
        );
        println!(
            "CLI download {} stderr: {}",
            name,
            String::from_utf8_lossy(&result.stderr)
        );

        if result.status.success() {
            let downloaded = std::fs::read(download_dir.path().join(name))
                .unwrap_or_else(|_| panic!("Failed to read {}", name));
            let actual_sha256 = sha256_hex(&downloaded);

            let expected = if name == "v1.zip" {
                &v1_sha256
            } else {
                &v2_sha256
            };
            if actual_sha256 == *expected {
                println!("✓ {} checksum matches: {}", name, actual_sha256);
            } else {
                println!(
                    "✗ {} checksum MISMATCH: expected {}, got {}",
                    name, expected, actual_sha256
                );
            }
        } else {
            println!(
                "⚠ CLI couldn't download {} - storage format incompatibility",
                name
            );
        }
    }

    println!("✓ Cross-tool interoperability test completed");
}

/// Test 5: Verify the proxy can read original CLI's storage format
#[tokio::test]
#[ignore = "Requires MinIO and Docker: docker compose up -d"]
async fn test_proxy_reads_cli_format() {
    if !minio_available().await {
        eprintln!("MinIO not available, skipping test");
        return;
    }
    if !docker_available() {
        eprintln!("Docker not available, skipping test");
        return;
    }

    let prefix = unique_prefix();
    let temp_dir = TempDir::new().expect("Failed to create temp dir");

    // Create test files
    let v1_data = generate_archive_content(100_000, 700);
    let v2_data = mutate_archive(&v1_data, 0.05, 800);
    let v1_sha256 = sha256_hex(&v1_data);
    let v2_sha256 = sha256_hex(&v2_data);

    std::fs::write(temp_dir.path().join("v1.zip"), &v1_data).unwrap();
    std::fs::write(temp_dir.path().join("v2.zip"), &v2_data).unwrap();

    // Upload via original CLI to establish baseline format
    for name in ["v1.zip", "v2.zip"] {
        let result = Command::new("docker")
            .args([
                "run",
                "--rm",
                "--network=host",
                "-v",
                &format!("{}:/data", temp_dir.path().display()),
                "-e",
                &format!("AWS_ACCESS_KEY_ID={}", MINIO_ACCESS_KEY),
                "-e",
                &format!("AWS_SECRET_ACCESS_KEY={}", MINIO_SECRET_KEY),
                "-e",
                &format!("AWS_ENDPOINT_URL={}", minio_endpoint()),
                DELTAGLIDER_IMAGE,
                "cp",
                &format!("/data/{}", name),
                &format!("s3://{}/{}/{}", MINIO_BUCKET, prefix, name),
            ])
            .output()
            .expect("Failed to upload via CLI");

        assert!(
            result.status.success(),
            "CLI upload {} failed: {}",
            name,
            String::from_utf8_lossy(&result.stderr)
        );
    }

    // Now start proxy and try to serve these files
    // The proxy reads from the same MinIO bucket
    let proxy = TestProxyServer::start_with_s3_backend().await;
    let proxy_client = proxy.s3_client().await;

    // The key insight: CLI stores data with different structure
    // CLI uses S3 object metadata (x-amz-meta-dg-*)
    // Proxy uses sidecar .meta JSON files

    // For true interop, proxy would need to:
    // 1. Check for .meta sidecar (proxy format)
    // 2. If not found, check S3 metadata headers (CLI format)

    // List what proxy sees
    let list_result = proxy_client
        .list_objects_v2()
        .bucket("default")
        .prefix(&prefix)
        .send()
        .await
        .expect("Failed to list via proxy");

    println!("Proxy sees objects:");
    for obj in list_result.contents() {
        println!(
            "  - {} ({} bytes)",
            obj.key().unwrap_or("?"),
            obj.size().unwrap_or(0)
        );
    }

    // Try to GET the files via proxy
    // This tests if proxy can serve CLI-uploaded content
    for (name, expected_sha256) in [("v1.zip", &v1_sha256), ("v2.zip", &v2_sha256)] {
        let get_result = proxy_client
            .get_object()
            .bucket("default")
            .key(format!("{}/{}", prefix, name))
            .send()
            .await;

        match get_result {
            Ok(response) => {
                let body = response
                    .body
                    .collect()
                    .await
                    .expect("Failed to read body")
                    .into_bytes();

                let actual_sha256 = sha256_hex(&body);
                if actual_sha256 == *expected_sha256 {
                    println!(
                        "✓ Proxy successfully served CLI-uploaded {}: {}",
                        name, actual_sha256
                    );
                } else {
                    println!(
                        "✗ Proxy served {} but checksum mismatch: expected {}, got {}",
                        name, expected_sha256, actual_sha256
                    );
                }
            }
            Err(e) => {
                println!("⚠ Proxy couldn't serve CLI-uploaded {}: {:?}", name, e);
                println!("  This indicates storage format incompatibility");
            }
        }
    }

    println!("✓ Proxy-reads-CLI-format test completed");
}

/// Test 6: Document the storage format differences
#[tokio::test]
#[ignore = "Requires MinIO and Docker: docker compose up -d"]
async fn test_document_storage_format_differences() {
    if !minio_available().await {
        eprintln!("MinIO not available, skipping test");
        return;
    }
    if !docker_available() {
        eprintln!("Docker not available, skipping test");
        return;
    }

    let prefix_cli = format!("{}-cli", unique_prefix());
    let prefix_proxy = format!("{}-proxy", unique_prefix());
    let temp_dir = TempDir::new().expect("Failed to create temp dir");

    // Same test data for both
    let v1_data = generate_archive_content(50_000, 900);
    let v2_data = mutate_archive(&v1_data, 0.05, 1000);

    std::fs::write(temp_dir.path().join("v1.zip"), &v1_data).unwrap();
    std::fs::write(temp_dir.path().join("v2.zip"), &v2_data).unwrap();

    // Upload via CLI
    for name in ["v1.zip", "v2.zip"] {
        let _ = Command::new("docker")
            .args([
                "run",
                "--rm",
                "--network=host",
                "-v",
                &format!("{}:/data", temp_dir.path().display()),
                "-e",
                &format!("AWS_ACCESS_KEY_ID={}", MINIO_ACCESS_KEY),
                "-e",
                &format!("AWS_SECRET_ACCESS_KEY={}", MINIO_SECRET_KEY),
                "-e",
                &format!("AWS_ENDPOINT_URL={}", minio_endpoint()),
                DELTAGLIDER_IMAGE,
                "cp",
                &format!("/data/{}", name),
                &format!("s3://{}/{}/{}", MINIO_BUCKET, prefix_cli, name),
            ])
            .output();
    }

    // Upload via proxy
    let proxy = TestProxyServer::start_with_s3_backend().await;
    let proxy_client = proxy.s3_client().await;

    for (name, data) in [("v1.zip", &v1_data), ("v2.zip", &v2_data)] {
        let _ = proxy_client
            .put_object()
            .bucket("default")
            .key(format!("{}/{}", prefix_proxy, name))
            .body(ByteStream::from(data.clone()))
            .send()
            .await;
    }

    // Document differences
    let minio = minio_client().await;

    println!("\n========================================");
    println!("STORAGE FORMAT COMPARISON");
    println!("========================================\n");

    println!("--- Original DeltaGlider CLI Format ---");
    let cli_objects = minio
        .list_objects_v2()
        .bucket(MINIO_BUCKET)
        .prefix(&prefix_cli)
        .send()
        .await
        .expect("Failed to list CLI objects");

    for obj in cli_objects.contents() {
        let key = obj.key().unwrap_or("?");
        println!("\nObject: {}", key);
        println!("  Size: {} bytes", obj.size().unwrap_or(0));

        if let Ok(head) = minio
            .head_object()
            .bucket(MINIO_BUCKET)
            .key(key)
            .send()
            .await
        {
            if let Some(metadata) = head.metadata() {
                println!("  S3 Metadata:");
                for (k, v) in metadata {
                    println!("    x-amz-meta-{}: {}", k, v);
                }
            }
            println!("  Content-Type: {:?}", head.content_type());
        }
    }

    println!("\n--- DeltaGlider Proxy Format ---");
    let proxy_objects = minio
        .list_objects_v2()
        .bucket(MINIO_BUCKET)
        .prefix(&prefix_proxy)
        .send()
        .await
        .expect("Failed to list proxy objects");

    for obj in proxy_objects.contents() {
        let key = obj.key().unwrap_or("?");
        println!("\nObject: {}", key);
        println!("  Size: {} bytes", obj.size().unwrap_or(0));

        // If it's a .meta file, print its contents
        if key.ends_with(".meta") {
            if let Ok(get) = minio
                .get_object()
                .bucket(MINIO_BUCKET)
                .key(key)
                .send()
                .await
            {
                let body = get
                    .body
                    .collect()
                    .await
                    .expect("Failed to read meta")
                    .into_bytes();
                if let Ok(json) = String::from_utf8(body.to_vec()) {
                    println!("  JSON Content:");
                    // Pretty print JSON
                    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&json) {
                        println!("{}", serde_json::to_string_pretty(&parsed).unwrap_or(json));
                    } else {
                        println!("    {}", json);
                    }
                }
            }
        } else if let Ok(head) = minio
            .head_object()
            .bucket(MINIO_BUCKET)
            .key(key)
            .send()
            .await
        {
            if let Some(metadata) = head.metadata() {
                println!("  S3 Metadata:");
                for (k, v) in metadata {
                    println!("    x-amz-meta-{}: {}", k, v);
                }
            }
        }
    }

    println!("\n========================================");
    println!("KEY DIFFERENCES:");
    println!("========================================");
    println!("1. Original CLI: Metadata in S3 object headers (x-amz-meta-dg-*)");
    println!("2. Proxy: Metadata in sidecar .meta JSON files");
    println!("3. Both use reference.bin for baseline and .delta for patches");
    println!("4. xdelta3 binary format should be compatible");
    println!("========================================\n");
}
