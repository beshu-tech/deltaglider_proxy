//! Self-contained S3 integration tests with ephemeral MinIO
//!
//! Spins up an ephemeral MinIO container via testcontainers, runs comprehensive
//! tests covering bucket CRUD, delta compression, metadata, file integrity,
//! and cross-cutting scenarios, then tears down automatically.
//!
//! All tests share a single MinIO container via OnceCell for efficiency (~3s startup).
//! Test data is isolated via unique prefixes.
//!
//! Requires Docker. Tests skip gracefully if Docker is unavailable.

mod common;

use common::{
    generate_binary, get_bytes, head_headers, list_objects_raw, mutate_binary,
    put_and_get_storage_type, TestServer, MINIO_ACCESS_KEY, MINIO_SECRET_KEY,
};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicU64, Ordering};
use testcontainers::core::IntoContainerPort;
use testcontainers::runners::AsyncRunner;
use testcontainers::ContainerAsync;
use testcontainers_modules::minio::MinIO;
use tokio::sync::OnceCell;

/// Shared MinIO container for all tests in this file.
/// Wrapped in `ContainerGuard` to ensure Docker cleanup even though statics don't drop.
static MINIO_CONTAINER: OnceCell<ContainerGuard> = OnceCell::const_new();

/// Counter for unique test prefixes
static PREFIX_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Wrapper that registers an atexit handler to stop+remove the Docker container.
/// `static` values are never dropped in Rust, so `ContainerAsync::Drop` never fires.
/// This guard captures the container ID at creation and registers a synchronous
/// `docker rm -f` via `std::process::atexit` to ensure cleanup.
struct ContainerGuard {
    inner: ContainerAsync<MinIO>,
}

impl ContainerGuard {
    fn new(container: ContainerAsync<MinIO>) -> Self {
        // Register a process-exit hook that force-removes this container.
        // Uses `docker rm -f` which is synchronous and doesn't need the tokio runtime.
        let id = container.id().to_string();
        // Safety: atexit handler must be safe to call; std::process::Command is fine.
        // We intentionally leak the String so the closure is 'static.
        let id_leaked: &'static str = String::leak(id);
        unsafe {
            libc::atexit(cleanup_container_trampoline);
        }
        CONTAINER_ID_FOR_CLEANUP
            .set(id_leaked)
            .unwrap_or_else(|_| panic!("ContainerGuard created more than once"));

        Self { inner: container }
    }
}

/// The container ID to clean up at exit.
static CONTAINER_ID_FOR_CLEANUP: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();

/// C-compatible atexit callback — force-removes the Docker container.
extern "C" fn cleanup_container_trampoline() {
    if let Some(id) = CONTAINER_ID_FOR_CLEANUP.get() {
        let _ = std::process::Command::new("docker")
            .args(["rm", "-f", id])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// The bucket name used for integration tests
const TEST_BUCKET: &str = "integration-test";

/// Generate a unique prefix to isolate each test's data
fn unique_prefix() -> String {
    let counter = PREFIX_COUNTER.fetch_add(1, Ordering::SeqCst);
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    format!("itest-{}-{}", timestamp, counter)
}

/// Get or start the shared MinIO container, returning its S3 endpoint URL.
async fn minio_endpoint() -> String {
    let guard = MINIO_CONTAINER
        .get_or_init(|| async {
            let container = MinIO::default()
                .start()
                .await
                .expect("Failed to start MinIO container");
            ContainerGuard::new(container)
        })
        .await;

    let host = guard.inner.get_host().await.unwrap();
    let port = guard.inner.get_host_port_ipv4(9000.tcp()).await.unwrap();
    format!("http://{}:{}", host, port)
}

/// Create an S3 client pointed directly at the MinIO container (not through proxy)
async fn minio_direct_client(endpoint: &str) -> aws_sdk_s3::Client {
    let credentials = aws_credential_types::Credentials::new(
        MINIO_ACCESS_KEY,
        MINIO_SECRET_KEY,
        None,
        None,
        "test",
    );

    let config = aws_sdk_s3::Config::builder()
        .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
        .region(aws_sdk_s3::config::Region::new("us-east-1"))
        .endpoint_url(endpoint)
        .credentials_provider(credentials)
        .force_path_style(true)
        .build();

    aws_sdk_s3::Client::from_conf(config)
}

/// Ensure the test bucket exists in MinIO (idempotent)
async fn ensure_bucket(endpoint: &str) {
    let client = minio_direct_client(endpoint).await;
    let _ = client.create_bucket().bucket(TEST_BUCKET).send().await;
}

/// Start a proxy server pointed at the ephemeral MinIO, return (TestServer, endpoint)
async fn proxy_server() -> TestServer {
    let endpoint = minio_endpoint().await;
    ensure_bucket(&endpoint).await;
    TestServer::s3_with_endpoint(&endpoint, TEST_BUCKET).await
}

// ============================================================================
// Group 1: Bucket CRUD
// ============================================================================

#[tokio::test]
async fn test_create_and_head_bucket() {
    skip_unless_docker!();
    let endpoint = minio_endpoint().await;
    let client = minio_direct_client(&endpoint).await;
    let bucket_name = format!("test-bucket-{}", unique_prefix());

    client
        .create_bucket()
        .bucket(&bucket_name)
        .send()
        .await
        .expect("CREATE bucket should succeed");

    let head = client.head_bucket().bucket(&bucket_name).send().await;
    assert!(head.is_ok(), "HEAD bucket should succeed after creation");

    // Cleanup
    let _ = client.delete_bucket().bucket(&bucket_name).send().await;
}

#[tokio::test]
async fn test_list_buckets_includes_created() {
    skip_unless_docker!();
    let endpoint = minio_endpoint().await;
    let client = minio_direct_client(&endpoint).await;
    let bucket_name = format!("list-test-{}", unique_prefix());

    client
        .create_bucket()
        .bucket(&bucket_name)
        .send()
        .await
        .expect("CREATE bucket should succeed");

    let result = client
        .list_buckets()
        .send()
        .await
        .expect("LIST buckets should succeed");
    let names: Vec<&str> = result.buckets().iter().filter_map(|b| b.name()).collect();
    assert!(
        names.contains(&bucket_name.as_str()),
        "Created bucket '{}' should appear in list: {:?}",
        bucket_name,
        names
    );

    // Cleanup
    let _ = client.delete_bucket().bucket(&bucket_name).send().await;
}

#[tokio::test]
async fn test_head_bucket_nonexistent() {
    skip_unless_docker!();
    let endpoint = minio_endpoint().await;
    let client = minio_direct_client(&endpoint).await;

    let result = client
        .head_bucket()
        .bucket("nonexistent-bucket-xyz-99999")
        .send()
        .await;
    assert!(result.is_err(), "HEAD nonexistent bucket should fail");
}

#[tokio::test]
async fn test_delete_empty_bucket() {
    skip_unless_docker!();
    let endpoint = minio_endpoint().await;
    let client = minio_direct_client(&endpoint).await;
    let bucket_name = format!("del-empty-{}", unique_prefix());

    client
        .create_bucket()
        .bucket(&bucket_name)
        .send()
        .await
        .unwrap();

    client
        .delete_bucket()
        .bucket(&bucket_name)
        .send()
        .await
        .expect("DELETE empty bucket should succeed");

    let head = client.head_bucket().bucket(&bucket_name).send().await;
    assert!(head.is_err(), "HEAD should fail after bucket deletion");
}

#[tokio::test]
async fn test_delete_nonempty_bucket_fails() {
    skip_unless_docker!();
    let endpoint = minio_endpoint().await;
    let client = minio_direct_client(&endpoint).await;
    let bucket_name = format!("del-nonempty-{}", unique_prefix());

    client
        .create_bucket()
        .bucket(&bucket_name)
        .send()
        .await
        .unwrap();

    client
        .put_object()
        .bucket(&bucket_name)
        .key("blocker.txt")
        .body(aws_sdk_s3::primitives::ByteStream::from(b"x".to_vec()))
        .send()
        .await
        .unwrap();

    let result = client.delete_bucket().bucket(&bucket_name).send().await;
    assert!(result.is_err(), "DELETE non-empty bucket should fail");

    // Cleanup
    let _ = client
        .delete_object()
        .bucket(&bucket_name)
        .key("blocker.txt")
        .send()
        .await;
    let _ = client.delete_bucket().bucket(&bucket_name).send().await;
}

// ============================================================================
// Group 2: Multi-deltaspace + delta compression
// ============================================================================

#[tokio::test]
async fn test_multi_version_delta_compression() {
    skip_unless_docker!();
    let server = proxy_server().await;
    let http = reqwest::Client::new();
    let prefix = unique_prefix();

    let base = generate_binary(100_000, 42);
    let v1 = mutate_binary(&base, 0.01);
    let v2 = mutate_binary(&base, 0.02);
    let v3 = mutate_binary(&base, 0.03);

    // PUT base — should become reference
    let st_base = put_and_get_storage_type(
        &http,
        &server.endpoint(),
        server.bucket(),
        &format!("{}/base.zip", prefix),
        base.clone(),
        "application/zip",
    )
    .await;
    assert!(
        st_base == "reference" || st_base == "delta",
        "First zip should be reference, got: {}",
        st_base
    );

    // PUT variants — should all be delta
    for (i, variant) in [(1, &v1), (2, &v2), (3, &v3)] {
        let st = put_and_get_storage_type(
            &http,
            &server.endpoint(),
            server.bucket(),
            &format!("{}/v{}.zip", prefix, i),
            variant.clone(),
            "application/zip",
        )
        .await;
        assert_eq!(
            st, "delta",
            "Variant v{} should be stored as delta, got: {}",
            i, st
        );
    }

    // GET all back and byte-compare
    assert_eq!(
        get_bytes(
            &http,
            &server.endpoint(),
            server.bucket(),
            &format!("{}/base.zip", prefix)
        )
        .await,
        base
    );
    assert_eq!(
        get_bytes(
            &http,
            &server.endpoint(),
            server.bucket(),
            &format!("{}/v1.zip", prefix)
        )
        .await,
        v1
    );
    assert_eq!(
        get_bytes(
            &http,
            &server.endpoint(),
            server.bucket(),
            &format!("{}/v2.zip", prefix)
        )
        .await,
        v2
    );
    assert_eq!(
        get_bytes(
            &http,
            &server.endpoint(),
            server.bucket(),
            &format!("{}/v3.zip", prefix)
        )
        .await,
        v3
    );
}

#[tokio::test]
async fn test_two_deltaspaces_independent() {
    skip_unless_docker!();
    let server = proxy_server().await;
    let http = reqwest::Client::new();
    let prefix_a = format!("{}/project-a", unique_prefix());
    let prefix_b = format!("{}/project-b", unique_prefix());

    let base_a = generate_binary(80_000, 100);
    let variant_a = mutate_binary(&base_a, 0.01);
    let base_b = generate_binary(80_000, 200);
    let variant_b = mutate_binary(&base_b, 0.01);

    // Upload to project-a
    put_and_get_storage_type(
        &http,
        &server.endpoint(),
        server.bucket(),
        &format!("{}/base.zip", prefix_a),
        base_a.clone(),
        "application/zip",
    )
    .await;
    put_and_get_storage_type(
        &http,
        &server.endpoint(),
        server.bucket(),
        &format!("{}/variant.zip", prefix_a),
        variant_a.clone(),
        "application/zip",
    )
    .await;

    // Upload to project-b
    put_and_get_storage_type(
        &http,
        &server.endpoint(),
        server.bucket(),
        &format!("{}/base.zip", prefix_b),
        base_b.clone(),
        "application/zip",
    )
    .await;
    put_and_get_storage_type(
        &http,
        &server.endpoint(),
        server.bucket(),
        &format!("{}/variant.zip", prefix_b),
        variant_b.clone(),
        "application/zip",
    )
    .await;

    // Verify all 4 files reconstruct correctly
    assert_eq!(
        get_bytes(
            &http,
            &server.endpoint(),
            server.bucket(),
            &format!("{}/base.zip", prefix_a)
        )
        .await,
        base_a
    );
    assert_eq!(
        get_bytes(
            &http,
            &server.endpoint(),
            server.bucket(),
            &format!("{}/variant.zip", prefix_a)
        )
        .await,
        variant_a
    );
    assert_eq!(
        get_bytes(
            &http,
            &server.endpoint(),
            server.bucket(),
            &format!("{}/base.zip", prefix_b)
        )
        .await,
        base_b
    );
    assert_eq!(
        get_bytes(
            &http,
            &server.endpoint(),
            server.bucket(),
            &format!("{}/variant.zip", prefix_b)
        )
        .await,
        variant_b
    );
}

#[tokio::test]
async fn test_delta_reconstruction_sha256() {
    skip_unless_docker!();
    let server = proxy_server().await;
    let http = reqwest::Client::new();
    let prefix = unique_prefix();

    let base = generate_binary(100_000, 777);

    // Upload base
    put_and_get_storage_type(
        &http,
        &server.endpoint(),
        server.bucket(),
        &format!("{}/base.zip", prefix),
        base.clone(),
        "application/zip",
    )
    .await;

    // Upload 5 variants with increasing mutation ratios
    let ratios = [0.01, 0.05, 0.10, 0.25, 0.50];
    let mut variants = Vec::new();
    for (i, ratio) in ratios.iter().enumerate() {
        let variant = mutate_binary(&base, *ratio);
        let expected_hash = hex::encode(Sha256::digest(&variant));
        variants.push((i, variant.clone(), expected_hash));

        put_and_get_storage_type(
            &http,
            &server.endpoint(),
            server.bucket(),
            &format!("{}/v{}.zip", prefix, i),
            variant,
            "application/zip",
        )
        .await;
    }

    // GET each back and SHA256 verify
    for (i, original, expected_hash) in &variants {
        let retrieved = get_bytes(
            &http,
            &server.endpoint(),
            server.bucket(),
            &format!("{}/v{}.zip", prefix, i),
        )
        .await;
        let actual_hash = hex::encode(Sha256::digest(&retrieved));
        assert_eq!(
            actual_hash, *expected_hash,
            "SHA256 mismatch for variant v{} (mutation ratio {})",
            i, ratios[*i]
        );
        assert_eq!(
            retrieved, *original,
            "Byte mismatch for variant v{} (mutation ratio {})",
            i, ratios[*i]
        );
    }
}

// ============================================================================
// Group 3: Metadata verification
// ============================================================================

#[tokio::test]
async fn test_head_returns_dg_metadata_headers() {
    skip_unless_docker!();
    let server = proxy_server().await;
    let http = reqwest::Client::new();
    let prefix = unique_prefix();

    let base = generate_binary(80_000, 42);
    let variant = mutate_binary(&base, 0.01);
    let variant_hash = hex::encode(Sha256::digest(&variant));

    // Upload base + variant
    put_and_get_storage_type(
        &http,
        &server.endpoint(),
        server.bucket(),
        &format!("{}/base.zip", prefix),
        base,
        "application/zip",
    )
    .await;
    put_and_get_storage_type(
        &http,
        &server.endpoint(),
        server.bucket(),
        &format!("{}/delta.zip", prefix),
        variant.clone(),
        "application/zip",
    )
    .await;

    // HEAD the delta file
    let headers = head_headers(
        &http,
        &server.endpoint(),
        server.bucket(),
        &format!("{}/delta.zip", prefix),
    )
    .await;

    // Verify dg-tool header
    let tool = headers
        .get("x-amz-meta-dg-tool")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        tool.starts_with("deltaglider_proxy/"),
        "dg-tool should start with 'deltaglider_proxy/', got: '{}'",
        tool
    );

    // Verify dg-file-sha256
    let file_sha = headers
        .get("x-amz-meta-dg-file-sha256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert_eq!(
        file_sha, variant_hash,
        "dg-file-sha256 should match computed hash"
    );

    // Verify dg-file-size
    let file_size = headers
        .get("x-amz-meta-dg-file-size")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert_eq!(
        file_size,
        variant.len().to_string(),
        "dg-file-size should match original file size"
    );

    // Verify delta-specific headers exist
    assert!(
        headers.get("x-amz-meta-dg-ref-key").is_some(),
        "dg-ref-key should be present for delta files"
    );
    assert!(
        headers.get("x-amz-meta-dg-delta-size").is_some(),
        "dg-delta-size should be present for delta files"
    );
}

#[tokio::test]
async fn test_metadata_for_all_storage_types() {
    skip_unless_docker!();
    let server = proxy_server().await;
    let http = reqwest::Client::new();
    let prefix = unique_prefix();

    let zip_base = generate_binary(80_000, 42);
    let zip_variant = mutate_binary(&zip_base, 0.01);
    let text_data = b"Hello, this is a plain text file for testing.";

    // Upload zip base (reference), zip variant (delta), and text (passthrough)
    let st_ref = put_and_get_storage_type(
        &http,
        &server.endpoint(),
        server.bucket(),
        &format!("{}/base.zip", prefix),
        zip_base,
        "application/zip",
    )
    .await;
    let st_delta = put_and_get_storage_type(
        &http,
        &server.endpoint(),
        server.bucket(),
        &format!("{}/variant.zip", prefix),
        zip_variant,
        "application/zip",
    )
    .await;
    let st_passthrough = put_and_get_storage_type(
        &http,
        &server.endpoint(),
        server.bucket(),
        &format!("{}/readme.txt", prefix),
        text_data.to_vec(),
        "text/plain",
    )
    .await;

    assert!(
        st_ref == "reference" || st_ref == "delta",
        "First zip should be reference, got: {}",
        st_ref
    );
    assert_eq!(st_delta, "delta", "Variant should be delta");
    assert_eq!(st_passthrough, "passthrough", "Text should be passthrough");

    // HEAD the passthrough file — should have dg-tool but lack delta-specific headers
    let passthrough_headers = head_headers(
        &http,
        &server.endpoint(),
        server.bucket(),
        &format!("{}/readme.txt", prefix),
    )
    .await;

    // Passthrough files should NOT have delta-specific metadata
    assert!(
        passthrough_headers.get("x-amz-meta-dg-ref-key").is_none(),
        "Passthrough files should not have dg-ref-key"
    );
    assert!(
        passthrough_headers
            .get("x-amz-meta-dg-delta-size")
            .is_none(),
        "Passthrough files should not have dg-delta-size"
    );
}

// ============================================================================
// Group 4: Non-compressed file integrity
// ============================================================================

#[tokio::test]
async fn test_text_file_passthrough_roundtrip() {
    skip_unless_docker!();
    let server = proxy_server().await;
    let http = reqwest::Client::new();
    let prefix = unique_prefix();

    let text_data = b"Hello, this is a simple text file for roundtrip testing.";

    let st = put_and_get_storage_type(
        &http,
        &server.endpoint(),
        server.bucket(),
        &format!("{}/hello.txt", prefix),
        text_data.to_vec(),
        "text/plain",
    )
    .await;
    assert_eq!(st, "passthrough", ".txt should be stored as passthrough");

    let retrieved = get_bytes(
        &http,
        &server.endpoint(),
        server.bucket(),
        &format!("{}/hello.txt", prefix),
    )
    .await;
    assert_eq!(retrieved, text_data.as_slice());
}

#[tokio::test]
async fn test_multiple_text_files_roundtrip() {
    skip_unless_docker!();
    let server = proxy_server().await;
    let http = reqwest::Client::new();
    let prefix = unique_prefix();

    let files = vec![
        (
            "config.json",
            "application/json",
            b"{\"key\": \"value\"}".as_slice(),
        ),
        (
            "readme.md",
            "text/markdown",
            b"# Hello\n\nThis is a test.".as_slice(),
        ),
        (
            "data.csv",
            "text/csv",
            b"name,age\nAlice,30\nBob,25".as_slice(),
        ),
        (
            "notes.txt",
            "text/plain",
            b"Some plain text notes here.".as_slice(),
        ),
        (
            "script.py",
            "text/x-python",
            b"print('hello world')".as_slice(),
        ),
    ];

    for (filename, content_type, data) in &files {
        let st = put_and_get_storage_type(
            &http,
            &server.endpoint(),
            server.bucket(),
            &format!("{}/{}", prefix, filename),
            data.to_vec(),
            content_type,
        )
        .await;
        assert_eq!(
            st, "passthrough",
            "{} should be stored as passthrough, got: {}",
            filename, st
        );
    }

    // Verify all round-trip correctly
    for (filename, _, data) in &files {
        let retrieved = get_bytes(
            &http,
            &server.endpoint(),
            server.bucket(),
            &format!("{}/{}", prefix, filename),
        )
        .await;
        assert_eq!(
            retrieved,
            data.to_vec(),
            "Round-trip mismatch for {}",
            filename
        );
    }
}

// ============================================================================
// Group 5: Cross-cutting
// ============================================================================

#[tokio::test]
async fn test_mixed_file_types_same_prefix() {
    skip_unless_docker!();
    let server = proxy_server().await;
    let http = reqwest::Client::new();
    let prefix = unique_prefix();

    let zip_data = generate_binary(50_000, 100);
    let text_data = b"README content for the project";

    let st_zip = put_and_get_storage_type(
        &http,
        &server.endpoint(),
        server.bucket(),
        &format!("{}/app.zip", prefix),
        zip_data.clone(),
        "application/zip",
    )
    .await;
    let st_txt = put_and_get_storage_type(
        &http,
        &server.endpoint(),
        server.bucket(),
        &format!("{}/readme.txt", prefix),
        text_data.to_vec(),
        "text/plain",
    )
    .await;

    assert!(
        st_zip == "reference" || st_zip == "delta",
        "zip should be reference or delta, got: {}",
        st_zip
    );
    assert_eq!(st_txt, "passthrough", "txt should be passthrough");

    // Both should be independently retrievable
    assert_eq!(
        get_bytes(
            &http,
            &server.endpoint(),
            server.bucket(),
            &format!("{}/app.zip", prefix)
        )
        .await,
        zip_data
    );
    assert_eq!(
        get_bytes(
            &http,
            &server.endpoint(),
            server.bucket(),
            &format!("{}/readme.txt", prefix)
        )
        .await,
        text_data.as_slice()
    );
}

#[tokio::test]
async fn test_full_lifecycle_with_delete() {
    skip_unless_docker!();
    let server = proxy_server().await;
    let http = reqwest::Client::new();
    let client = server.s3_client().await;
    let prefix = unique_prefix();

    let base = generate_binary(80_000, 42);
    let v1 = mutate_binary(&base, 0.01);
    let v2 = mutate_binary(&base, 0.02);

    // Upload 3 zip versions
    put_and_get_storage_type(
        &http,
        &server.endpoint(),
        server.bucket(),
        &format!("{}/base.zip", prefix),
        base.clone(),
        "application/zip",
    )
    .await;
    put_and_get_storage_type(
        &http,
        &server.endpoint(),
        server.bucket(),
        &format!("{}/v1.zip", prefix),
        v1.clone(),
        "application/zip",
    )
    .await;
    put_and_get_storage_type(
        &http,
        &server.endpoint(),
        server.bucket(),
        &format!("{}/v2.zip", prefix),
        v2.clone(),
        "application/zip",
    )
    .await;

    // Delete v1
    let del_url = format!(
        "{}/{}/{}/v1.zip",
        server.endpoint(),
        server.bucket(),
        prefix
    );
    let del_resp = http.delete(&del_url).send().await.unwrap();
    assert!(
        del_resp.status().is_success() || del_resp.status().as_u16() == 204,
        "DELETE should succeed"
    );

    // Verify base and v2 survive
    assert_eq!(
        get_bytes(
            &http,
            &server.endpoint(),
            server.bucket(),
            &format!("{}/base.zip", prefix)
        )
        .await,
        base
    );
    assert_eq!(
        get_bytes(
            &http,
            &server.endpoint(),
            server.bucket(),
            &format!("{}/v2.zip", prefix)
        )
        .await,
        v2
    );

    // Upload a replacement v1
    let v1_replacement = mutate_binary(&base, 0.015);
    put_and_get_storage_type(
        &http,
        &server.endpoint(),
        server.bucket(),
        &format!("{}/v1.zip", prefix),
        v1_replacement.clone(),
        "application/zip",
    )
    .await;

    // List objects and confirm correct state
    let list = client
        .list_objects_v2()
        .bucket(server.bucket())
        .prefix(format!("{}/", prefix))
        .send()
        .await
        .unwrap();
    let keys: Vec<String> = list
        .contents()
        .iter()
        .filter_map(|o| o.key().map(String::from))
        .collect();
    assert_eq!(keys.len(), 3, "Should have 3 objects: {:?}", keys);

    // Verify the replacement round-trips correctly
    assert_eq!(
        get_bytes(
            &http,
            &server.endpoint(),
            server.bucket(),
            &format!("{}/v1.zip", prefix)
        )
        .await,
        v1_replacement
    );
}

// ============================================================================
// Group 6: Multipart Upload
// ============================================================================

/// Helper: initiate a multipart upload via raw HTTP, return upload_id
async fn create_multipart_upload(
    client: &reqwest::Client,
    endpoint: &str,
    bucket: &str,
    key: &str,
) -> String {
    let url = format!("{}/{}/{}?uploads", endpoint, bucket, key);
    let resp = client
        .post(&url)
        .header("content-type", "application/octet-stream")
        .send()
        .await
        .expect("CreateMultipartUpload failed");
    assert!(
        resp.status().is_success(),
        "CreateMultipartUpload failed: {}",
        resp.status()
    );
    let body = resp.text().await.unwrap();
    // Parse upload_id from XML response
    let start = body.find("<UploadId>").expect("No UploadId in response") + 10;
    let end = body[start..]
        .find("</UploadId>")
        .expect("No closing UploadId")
        + start;
    body[start..end].to_string()
}

/// Helper: upload a part via raw HTTP, return ETag
async fn upload_part(
    client: &reqwest::Client,
    endpoint: &str,
    bucket: &str,
    key: &str,
    upload_id: &str,
    part_number: u32,
    data: Vec<u8>,
) -> String {
    let url = format!(
        "{}/{}/{}?partNumber={}&uploadId={}",
        endpoint, bucket, key, part_number, upload_id
    );
    let resp = client
        .put(&url)
        .body(data)
        .send()
        .await
        .expect("UploadPart failed");
    assert!(
        resp.status().is_success(),
        "UploadPart {} failed: {}",
        part_number,
        resp.status()
    );
    resp.headers()
        .get("etag")
        .expect("No ETag header in UploadPart response")
        .to_str()
        .unwrap()
        .to_string()
}

/// Helper: complete a multipart upload via raw HTTP
async fn complete_multipart_upload(
    client: &reqwest::Client,
    endpoint: &str,
    bucket: &str,
    key: &str,
    upload_id: &str,
    parts: &[(u32, &str)],
) -> reqwest::Response {
    let url = format!("{}/{}/{}?uploadId={}", endpoint, bucket, key, upload_id);
    let mut xml = String::from("<CompleteMultipartUpload>");
    for (num, etag) in parts {
        xml.push_str(&format!(
            "<Part><PartNumber>{}</PartNumber><ETag>{}</ETag></Part>",
            num, etag
        ));
    }
    xml.push_str("</CompleteMultipartUpload>");

    client
        .post(&url)
        .header("content-type", "application/xml")
        .body(xml)
        .send()
        .await
        .expect("CompleteMultipartUpload failed")
}

#[tokio::test]
async fn test_multipart_basic_roundtrip() {
    skip_unless_docker!();
    let server = proxy_server().await;
    let http = reqwest::Client::new();
    let prefix = unique_prefix();
    let key = format!("{}/multipart.bin", prefix);

    let part1_data = generate_binary(1024, 100);
    let part2_data = generate_binary(2048, 200);
    let part3_data = generate_binary(512, 300);

    let upload_id = create_multipart_upload(&http, &server.endpoint(), server.bucket(), &key).await;

    let etag1 = upload_part(
        &http,
        &server.endpoint(),
        server.bucket(),
        &key,
        &upload_id,
        1,
        part1_data.clone(),
    )
    .await;
    let etag2 = upload_part(
        &http,
        &server.endpoint(),
        server.bucket(),
        &key,
        &upload_id,
        2,
        part2_data.clone(),
    )
    .await;
    let etag3 = upload_part(
        &http,
        &server.endpoint(),
        server.bucket(),
        &key,
        &upload_id,
        3,
        part3_data.clone(),
    )
    .await;

    let resp = complete_multipart_upload(
        &http,
        &server.endpoint(),
        server.bucket(),
        &key,
        &upload_id,
        &[(1, &etag1), (2, &etag2), (3, &etag3)],
    )
    .await;
    assert!(
        resp.status().is_success(),
        "CompleteMultipartUpload failed: {}",
        resp.status()
    );

    // GET and verify
    let retrieved = get_bytes(&http, &server.endpoint(), server.bucket(), &key).await;
    let mut expected = Vec::new();
    expected.extend_from_slice(&part1_data);
    expected.extend_from_slice(&part2_data);
    expected.extend_from_slice(&part3_data);
    assert_eq!(
        retrieved, expected,
        "Multipart assembled data should match concatenated parts"
    );
}

#[tokio::test]
async fn test_multipart_single_part() {
    skip_unless_docker!();
    let server = proxy_server().await;
    let http = reqwest::Client::new();
    let prefix = unique_prefix();
    let key = format!("{}/single-part.bin", prefix);

    let data = generate_binary(4096, 42);

    let upload_id = create_multipart_upload(&http, &server.endpoint(), server.bucket(), &key).await;
    let etag = upload_part(
        &http,
        &server.endpoint(),
        server.bucket(),
        &key,
        &upload_id,
        1,
        data.clone(),
    )
    .await;

    let resp = complete_multipart_upload(
        &http,
        &server.endpoint(),
        server.bucket(),
        &key,
        &upload_id,
        &[(1, &etag)],
    )
    .await;
    assert!(resp.status().is_success());

    let retrieved = get_bytes(&http, &server.endpoint(), server.bucket(), &key).await;
    assert_eq!(retrieved, data);
}

#[tokio::test]
async fn test_multipart_abort() {
    skip_unless_docker!();
    let server = proxy_server().await;
    let http = reqwest::Client::new();
    let prefix = unique_prefix();
    let key = format!("{}/abort.bin", prefix);

    let upload_id = create_multipart_upload(&http, &server.endpoint(), server.bucket(), &key).await;
    upload_part(
        &http,
        &server.endpoint(),
        server.bucket(),
        &key,
        &upload_id,
        1,
        vec![0u8; 100],
    )
    .await;

    // Abort
    let abort_url = format!(
        "{}/{}/{}?uploadId={}",
        server.endpoint(),
        server.bucket(),
        key,
        upload_id
    );
    let resp = http.delete(&abort_url).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 204, "Abort should return 204");

    // UploadPart with same ID should fail (NoSuchUpload)
    let url = format!(
        "{}/{}/{}?partNumber=2&uploadId={}",
        server.endpoint(),
        server.bucket(),
        key,
        upload_id
    );
    let resp = http.put(&url).body(vec![0u8; 50]).send().await.unwrap();
    assert_eq!(
        resp.status().as_u16(),
        404,
        "UploadPart after abort should return 404"
    );
}

#[tokio::test]
async fn test_multipart_list_parts() {
    skip_unless_docker!();
    let server = proxy_server().await;
    let http = reqwest::Client::new();
    let prefix = unique_prefix();
    let key = format!("{}/list-parts.bin", prefix);

    let upload_id = create_multipart_upload(&http, &server.endpoint(), server.bucket(), &key).await;

    let mut etags = Vec::new();
    for i in 1..=5 {
        let etag = upload_part(
            &http,
            &server.endpoint(),
            server.bucket(),
            &key,
            &upload_id,
            i,
            generate_binary(256 * i as usize, i as u64),
        )
        .await;
        etags.push(etag);
    }

    // ListParts
    let list_url = format!(
        "{}/{}/{}?uploadId={}",
        server.endpoint(),
        server.bucket(),
        key,
        upload_id
    );
    let resp = http.get(&list_url).send().await.unwrap();
    assert!(resp.status().is_success());
    let body = resp.text().await.unwrap();

    // Verify all 5 parts are listed
    for i in 1..=5 {
        assert!(
            body.contains(&format!("<PartNumber>{}</PartNumber>", i)),
            "ListParts should contain part {}",
            i
        );
    }

    // Verify ETags are present
    for etag in &etags {
        let clean = etag.trim_matches('"');
        assert!(
            body.contains(clean),
            "ListParts should contain ETag {}",
            clean
        );
    }
}

#[tokio::test]
async fn test_multipart_list_uploads() {
    skip_unless_docker!();
    let server = proxy_server().await;
    let http = reqwest::Client::new();
    let prefix = unique_prefix();

    // Create 3 uploads
    let key1 = format!("{}/file1.bin", prefix);
    let key2 = format!("{}/file2.bin", prefix);
    let key3 = format!("{}/file3.bin", prefix);

    let uid1 = create_multipart_upload(&http, &server.endpoint(), server.bucket(), &key1).await;
    let uid2 = create_multipart_upload(&http, &server.endpoint(), server.bucket(), &key2).await;
    let uid3 = create_multipart_upload(&http, &server.endpoint(), server.bucket(), &key3).await;

    // ListMultipartUploads
    let list_url = format!(
        "{}/{}?uploads&prefix={}",
        server.endpoint(),
        server.bucket(),
        prefix
    );
    let resp = http.get(&list_url).send().await.unwrap();
    assert!(resp.status().is_success());
    let body = resp.text().await.unwrap();

    // All 3 uploads should be listed
    assert!(body.contains(&uid1), "Should list upload 1");
    assert!(body.contains(&uid2), "Should list upload 2");
    assert!(body.contains(&uid3), "Should list upload 3");

    // Complete one upload, then list again — should have 2
    let etag = upload_part(
        &http,
        &server.endpoint(),
        server.bucket(),
        &key1,
        &uid1,
        1,
        vec![1u8; 64],
    )
    .await;
    let resp = complete_multipart_upload(
        &http,
        &server.endpoint(),
        server.bucket(),
        &key1,
        &uid1,
        &[(1, &etag)],
    )
    .await;
    assert!(resp.status().is_success());

    let resp = http.get(&list_url).send().await.unwrap();
    let body = resp.text().await.unwrap();
    assert!(
        !body.contains(&uid1),
        "Completed upload should not be listed"
    );
    assert!(body.contains(&uid2), "Upload 2 should still be listed");
    assert!(body.contains(&uid3), "Upload 3 should still be listed");
}

#[tokio::test]
async fn test_multipart_delta_compression() {
    skip_unless_docker!();
    let server = proxy_server().await;
    let http = reqwest::Client::new();
    let prefix = unique_prefix();

    // Upload base via single PUT
    let base = generate_binary(100_000, 42);
    let st_base = put_and_get_storage_type(
        &http,
        &server.endpoint(),
        server.bucket(),
        &format!("{}/base.zip", prefix),
        base.clone(),
        "application/zip",
    )
    .await;
    assert!(
        st_base == "reference" || st_base == "delta",
        "Base should be reference, got: {}",
        st_base
    );

    // Upload variant via multipart
    let variant = mutate_binary(&base, 0.01);
    let variant_key = format!("{}/variant.zip", prefix);

    let upload_id =
        create_multipart_upload(&http, &server.endpoint(), server.bucket(), &variant_key).await;

    // Split variant into 2 parts
    let mid = variant.len() / 2;
    let etag1 = upload_part(
        &http,
        &server.endpoint(),
        server.bucket(),
        &variant_key,
        &upload_id,
        1,
        variant[..mid].to_vec(),
    )
    .await;
    let etag2 = upload_part(
        &http,
        &server.endpoint(),
        server.bucket(),
        &variant_key,
        &upload_id,
        2,
        variant[mid..].to_vec(),
    )
    .await;

    let resp = complete_multipart_upload(
        &http,
        &server.endpoint(),
        server.bucket(),
        &variant_key,
        &upload_id,
        &[(1, &etag1), (2, &etag2)],
    )
    .await;
    assert!(resp.status().is_success());

    // Check storage type header
    let st = resp
        .headers()
        .get("x-amz-storage-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown");
    assert_eq!(
        st, "delta",
        "Multipart variant should be stored as delta, got: {}",
        st
    );

    // Verify data integrity
    let retrieved = get_bytes(&http, &server.endpoint(), server.bucket(), &variant_key).await;
    assert_eq!(retrieved, variant);
}

#[tokio::test]
async fn test_multipart_invalid_upload_id() {
    skip_unless_docker!();
    let server = proxy_server().await;
    let http = reqwest::Client::new();
    let prefix = unique_prefix();
    let key = format!("{}/invalid.bin", prefix);

    let fake_id = "nonexistent_upload_id_12345678";

    // UploadPart with nonexistent ID should fail
    let url = format!(
        "{}/{}/{}?partNumber=1&uploadId={}",
        server.endpoint(),
        server.bucket(),
        key,
        fake_id
    );
    let resp = http.put(&url).body(vec![0u8; 50]).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 404);

    // CompleteMultipartUpload with nonexistent ID should fail
    let resp = complete_multipart_upload(
        &http,
        &server.endpoint(),
        server.bucket(),
        &key,
        fake_id,
        &[(1, "\"abc\"")],
    )
    .await;
    assert_eq!(resp.status().as_u16(), 404);

    // AbortMultipartUpload with nonexistent ID should fail
    let abort_url = format!(
        "{}/{}/{}?uploadId={}",
        server.endpoint(),
        server.bucket(),
        key,
        fake_id
    );
    let resp = http.delete(&abort_url).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 404);
}

#[tokio::test]
async fn test_multipart_aws_sdk_compat() {
    skip_unless_docker!();
    let server = proxy_server().await;
    let prefix = unique_prefix();
    let key = format!("{}/sdk-multipart.bin", prefix);

    // Use AWS SDK S3 client pointed at the proxy
    let client = server.s3_client().await;

    // CreateMultipartUpload
    let create_resp = client
        .create_multipart_upload()
        .bucket(server.bucket())
        .key(&key)
        .content_type("application/octet-stream")
        .send()
        .await
        .expect("SDK CreateMultipartUpload should succeed");
    let upload_id = create_resp.upload_id().expect("Should have upload_id");

    // Upload 3 parts
    let part1 = generate_binary(1024, 10);
    let part2 = generate_binary(2048, 20);
    let part3 = generate_binary(512, 30);

    let up1 = client
        .upload_part()
        .bucket(server.bucket())
        .key(&key)
        .upload_id(upload_id)
        .part_number(1)
        .body(aws_sdk_s3::primitives::ByteStream::from(part1.clone()))
        .send()
        .await
        .expect("SDK UploadPart 1 should succeed");

    let up2 = client
        .upload_part()
        .bucket(server.bucket())
        .key(&key)
        .upload_id(upload_id)
        .part_number(2)
        .body(aws_sdk_s3::primitives::ByteStream::from(part2.clone()))
        .send()
        .await
        .expect("SDK UploadPart 2 should succeed");

    let up3 = client
        .upload_part()
        .bucket(server.bucket())
        .key(&key)
        .upload_id(upload_id)
        .part_number(3)
        .body(aws_sdk_s3::primitives::ByteStream::from(part3.clone()))
        .send()
        .await
        .expect("SDK UploadPart 3 should succeed");

    // CompleteMultipartUpload
    use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
    let completed_parts = CompletedMultipartUpload::builder()
        .parts(
            CompletedPart::builder()
                .part_number(1)
                .e_tag(up1.e_tag().unwrap_or_default())
                .build(),
        )
        .parts(
            CompletedPart::builder()
                .part_number(2)
                .e_tag(up2.e_tag().unwrap_or_default())
                .build(),
        )
        .parts(
            CompletedPart::builder()
                .part_number(3)
                .e_tag(up3.e_tag().unwrap_or_default())
                .build(),
        )
        .build();

    client
        .complete_multipart_upload()
        .bucket(server.bucket())
        .key(&key)
        .upload_id(upload_id)
        .multipart_upload(completed_parts)
        .send()
        .await
        .expect("SDK CompleteMultipartUpload should succeed");

    // Verify round-trip via SDK GetObject
    let get_resp = client
        .get_object()
        .bucket(server.bucket())
        .key(&key)
        .send()
        .await
        .expect("SDK GetObject should succeed");

    let retrieved = get_resp
        .body
        .collect()
        .await
        .expect("Body collect should succeed")
        .into_bytes()
        .to_vec();

    let mut expected = Vec::new();
    expected.extend_from_slice(&part1);
    expected.extend_from_slice(&part2);
    expected.extend_from_slice(&part3);

    assert_eq!(
        retrieved, expected,
        "SDK multipart upload round-trip data should match"
    );
}

// ============================================================================
// Group 7: Multi-Bucket
// ============================================================================

/// Create a bucket through the proxy endpoint (not directly on MinIO)
async fn ensure_bucket_via_proxy(client: &reqwest::Client, endpoint: &str, bucket: &str) {
    let url = format!("{}/{}", endpoint, bucket);
    let resp = client.put(&url).send().await.expect("CREATE bucket failed");
    let status = resp.status().as_u16();
    assert!(
        status == 200 || status == 409,
        "CREATE bucket {} should return 200 or 409, got: {}",
        bucket,
        status
    );
}

#[tokio::test]
async fn test_multi_bucket_create_and_list() {
    skip_unless_docker!();
    let server = proxy_server().await;
    let http = reqwest::Client::new();
    let prefix = unique_prefix();

    let bucket_a = format!("mb-a-{}", prefix);
    let bucket_b = format!("mb-b-{}", prefix);

    // Create two buckets through the proxy
    ensure_bucket_via_proxy(&http, &server.endpoint(), &bucket_a).await;
    ensure_bucket_via_proxy(&http, &server.endpoint(), &bucket_b).await;

    // ListBuckets and verify both appear
    let resp = http
        .get(format!("{}/", server.endpoint()))
        .send()
        .await
        .expect("ListBuckets failed");
    assert!(resp.status().is_success());
    let body = resp.text().await.unwrap();

    assert!(
        body.contains(&format!("<Name>{}</Name>", bucket_a)),
        "Bucket '{}' should appear in ListBuckets response",
        bucket_a
    );
    assert!(
        body.contains(&format!("<Name>{}</Name>", bucket_b)),
        "Bucket '{}' should appear in ListBuckets response",
        bucket_b
    );
}

#[tokio::test]
async fn test_multi_bucket_put_get_isolation() {
    skip_unless_docker!();
    let server = proxy_server().await;
    let http = reqwest::Client::new();
    let prefix = unique_prefix();

    let bucket_a = format!("iso-a-{}", prefix);
    let bucket_b = format!("iso-b-{}", prefix);

    ensure_bucket_via_proxy(&http, &server.endpoint(), &bucket_a).await;
    ensure_bucket_via_proxy(&http, &server.endpoint(), &bucket_b).await;

    let data_a = b"data-for-bucket-a";
    let data_b = b"data-for-bucket-b";

    // PUT object into bucket-a
    put_and_get_storage_type(
        &http,
        &server.endpoint(),
        &bucket_a,
        "shared-key.txt",
        data_a.to_vec(),
        "text/plain",
    )
    .await;

    // PUT different object into bucket-b with the same key
    put_and_get_storage_type(
        &http,
        &server.endpoint(),
        &bucket_b,
        "shared-key.txt",
        data_b.to_vec(),
        "text/plain",
    )
    .await;

    // GET from each bucket — verify data isolation
    let retrieved_a = get_bytes(&http, &server.endpoint(), &bucket_a, "shared-key.txt").await;
    let retrieved_b = get_bytes(&http, &server.endpoint(), &bucket_b, "shared-key.txt").await;

    assert_eq!(
        retrieved_a,
        data_a.as_slice(),
        "bucket-a should return data-a"
    );
    assert_eq!(
        retrieved_b,
        data_b.as_slice(),
        "bucket-b should return data-b"
    );
}

#[tokio::test]
async fn test_multi_bucket_list_objects_isolation() {
    skip_unless_docker!();
    let server = proxy_server().await;
    let http = reqwest::Client::new();
    let prefix = unique_prefix();

    let bucket_a = format!("lst-a-{}", prefix);
    let bucket_b = format!("lst-b-{}", prefix);

    ensure_bucket_via_proxy(&http, &server.endpoint(), &bucket_a).await;
    ensure_bucket_via_proxy(&http, &server.endpoint(), &bucket_b).await;

    // PUT objects into bucket-a
    put_and_get_storage_type(
        &http,
        &server.endpoint(),
        &bucket_a,
        "only-in-a.txt",
        b"aaa".to_vec(),
        "text/plain",
    )
    .await;

    // PUT objects into bucket-b
    put_and_get_storage_type(
        &http,
        &server.endpoint(),
        &bucket_b,
        "only-in-b.txt",
        b"bbb".to_vec(),
        "text/plain",
    )
    .await;

    // ListObjectsV2 on bucket-a
    let resp_a = http
        .get(format!("{}/{}?list-type=2", server.endpoint(), bucket_a))
        .send()
        .await
        .unwrap();
    assert!(resp_a.status().is_success());
    let body_a = resp_a.text().await.unwrap();

    assert!(
        body_a.contains("<Key>only-in-a.txt</Key>"),
        "bucket-a should list only-in-a.txt"
    );
    assert!(
        !body_a.contains("<Key>only-in-b.txt</Key>"),
        "bucket-a should NOT list only-in-b.txt"
    );

    // ListObjectsV2 on bucket-b
    let resp_b = http
        .get(format!("{}/{}?list-type=2", server.endpoint(), bucket_b))
        .send()
        .await
        .unwrap();
    assert!(resp_b.status().is_success());
    let body_b = resp_b.text().await.unwrap();

    assert!(
        body_b.contains("<Key>only-in-b.txt</Key>"),
        "bucket-b should list only-in-b.txt"
    );
    assert!(
        !body_b.contains("<Key>only-in-a.txt</Key>"),
        "bucket-b should NOT list only-in-a.txt"
    );
}

#[tokio::test]
async fn test_multi_bucket_cross_bucket_copy() {
    skip_unless_docker!();
    let server = proxy_server().await;
    let http = reqwest::Client::new();
    let prefix = unique_prefix();

    let bucket_src = format!("cpy-s-{}", prefix);
    let bucket_dst = format!("cpy-d-{}", prefix);

    ensure_bucket_via_proxy(&http, &server.endpoint(), &bucket_src).await;
    ensure_bucket_via_proxy(&http, &server.endpoint(), &bucket_dst).await;

    let original_data = b"cross-bucket-copy-payload";

    // PUT object into source bucket
    put_and_get_storage_type(
        &http,
        &server.endpoint(),
        &bucket_src,
        "source.txt",
        original_data.to_vec(),
        "text/plain",
    )
    .await;

    // COPY to destination bucket via x-amz-copy-source
    let copy_url = format!("{}/{}/copied.txt", server.endpoint(), bucket_dst);
    let copy_source = format!("/{}/source.txt", bucket_src);
    let resp = http
        .put(&copy_url)
        .header("x-amz-copy-source", &copy_source)
        .send()
        .await
        .expect("COPY failed");
    assert!(
        resp.status().is_success(),
        "COPY should succeed, got: {}",
        resp.status()
    );

    // GET from destination bucket and byte-compare
    let retrieved = get_bytes(&http, &server.endpoint(), &bucket_dst, "copied.txt").await;
    assert_eq!(
        retrieved,
        original_data.as_slice(),
        "Cross-bucket copy should preserve data"
    );
}

#[tokio::test]
async fn test_multi_bucket_delete_bucket() {
    skip_unless_docker!();
    let server = proxy_server().await;
    let http = reqwest::Client::new();
    let prefix = unique_prefix();

    let bucket = format!("del-mb-{}", prefix);

    // Create bucket through proxy
    ensure_bucket_via_proxy(&http, &server.endpoint(), &bucket).await;

    // PUT an object
    put_and_get_storage_type(
        &http,
        &server.endpoint(),
        &bucket,
        "temp.txt",
        b"temporary".to_vec(),
        "text/plain",
    )
    .await;

    // DELETE object
    let del_obj_url = format!("{}/{}/temp.txt", server.endpoint(), bucket);
    let resp = http.delete(&del_obj_url).send().await.unwrap();
    assert!(
        resp.status().as_u16() == 204 || resp.status().is_success(),
        "DELETE object should succeed"
    );

    // DELETE bucket (now empty)
    let del_bucket_url = format!("{}/{}", server.endpoint(), bucket);
    let resp = http.delete(&del_bucket_url).send().await.unwrap();
    assert_eq!(
        resp.status().as_u16(),
        204,
        "DELETE empty bucket should return 204"
    );

    // HEAD bucket should return 404
    let head_url = format!("{}/{}", server.endpoint(), bucket);
    let resp = http.head(&head_url).send().await.unwrap();
    assert_eq!(
        resp.status().as_u16(),
        404,
        "HEAD deleted bucket should return 404"
    );
}

// ============================================================================
// Group 8: Listing & Pagination
// ============================================================================

#[tokio::test]
async fn test_list_objects_reports_original_sizes() {
    skip_unless_docker!();
    let server = proxy_server().await;
    let http = reqwest::Client::new();
    let prefix = unique_prefix();

    // Upload a base zip (reference)
    let base = generate_binary(1024, 42);
    put_and_get_storage_type(
        &http,
        &server.endpoint(),
        server.bucket(),
        &format!("{}/base.zip", prefix),
        base.clone(),
        "application/zip",
    )
    .await;

    // Upload a similar variant (should be stored as delta, much smaller on disk)
    let variant = mutate_binary(&base, 0.01);
    let variant_len = variant.len();
    let st = put_and_get_storage_type(
        &http,
        &server.endpoint(),
        server.bucket(),
        &format!("{}/v1.zip", prefix),
        variant,
        "application/zip",
    )
    .await;
    assert_eq!(st, "delta", "Variant should be stored as delta");

    // List and check sizes
    let xml = list_objects_raw(
        &http,
        &server.endpoint(),
        server.bucket(),
        &format!("prefix={}/", prefix),
    )
    .await;

    // Extract all <Size> values
    let sizes: Vec<u64> = xml
        .match_indices("<Size>")
        .map(|(start, _)| {
            let rest = &xml[start + 6..];
            let end = rest.find("</Size>").unwrap();
            rest[..end].parse::<u64>().unwrap()
        })
        .collect();

    assert_eq!(sizes.len(), 2, "Should list 2 objects, got: {:?}", sizes);
    // Both sizes should be the original file sizes, not delta sizes
    for size in &sizes {
        assert!(
            *size >= 1000,
            "Listed size {} should be original size (~1024), not delta size",
            size
        );
    }
    // The variant's listed size should match its original length
    let v1_key = format!("{}/v1.zip", prefix);
    let v1_tag = format!("<Key>{}</Key>", v1_key);
    let v1_pos = xml.find(&v1_tag).unwrap();
    let size_after_v1 = &xml[v1_pos..];
    let size_start = size_after_v1.find("<Size>").unwrap() + 6;
    let size_end = size_after_v1[size_start..].find("</Size>").unwrap() + size_start;
    let v1_listed_size: u64 = size_after_v1[size_start..size_end].parse().unwrap();
    assert_eq!(
        v1_listed_size, variant_len as u64,
        "v1.zip listed size should be original size {}, got {}",
        variant_len, v1_listed_size
    );
}

#[tokio::test]
async fn test_list_objects_delimiter_common_prefixes() {
    skip_unless_docker!();
    let server = proxy_server().await;
    let http = reqwest::Client::new();
    let prefix = unique_prefix();

    // Upload objects under different sub-prefixes
    for suffix in &["a/file1.zip", "a/file2.zip", "b/file1.zip"] {
        put_and_get_storage_type(
            &http,
            &server.endpoint(),
            server.bucket(),
            &format!("{}/{}", prefix, suffix),
            generate_binary(1024, 42),
            "application/zip",
        )
        .await;
    }

    // List with delimiter — should collapse into CommonPrefixes
    let xml = list_objects_raw(
        &http,
        &server.endpoint(),
        server.bucket(),
        &format!("prefix={}/&delimiter=/", prefix),
    )
    .await;

    // Should have CommonPrefixes for {prefix}/a/ and {prefix}/b/
    let expected_a = format!("<Prefix>{}/a/</Prefix>", prefix);
    let expected_b = format!("<Prefix>{}/b/</Prefix>", prefix);
    assert!(
        xml.contains(&expected_a),
        "Should contain CommonPrefix {}/a/, got:\n{}",
        prefix,
        xml
    );
    assert!(
        xml.contains(&expected_b),
        "Should contain CommonPrefix {}/b/, got:\n{}",
        prefix,
        xml
    );

    // Should have no <Contents> since all objects are behind sub-prefixes
    assert!(
        !xml.contains("<Key>"),
        "Should have no direct <Key> entries with delimiter, got:\n{}",
        xml
    );
}

#[tokio::test]
async fn test_list_objects_pagination() {
    skip_unless_docker!();
    let server = proxy_server().await;
    let http = reqwest::Client::new();
    let prefix = unique_prefix();

    // Upload 4 files
    for i in 1..=4 {
        put_and_get_storage_type(
            &http,
            &server.endpoint(),
            server.bucket(),
            &format!("{}/file{}.zip", prefix, i),
            generate_binary(1024, i as u64),
            "application/zip",
        )
        .await;
    }

    // First page: max-keys=2
    let xml1 = list_objects_raw(
        &http,
        &server.endpoint(),
        server.bucket(),
        &format!("prefix={}/&max-keys=2", prefix),
    )
    .await;

    assert!(
        xml1.contains("<IsTruncated>true</IsTruncated>"),
        "First page should be truncated, got:\n{}",
        xml1
    );
    assert!(
        xml1.contains("<KeyCount>2</KeyCount>"),
        "First page should have KeyCount=2, got:\n{}",
        xml1
    );

    // Extract NextContinuationToken
    let token_start = xml1.find("<NextContinuationToken>").unwrap() + 23;
    let token_end = xml1[token_start..]
        .find("</NextContinuationToken>")
        .unwrap()
        + token_start;
    let token = &xml1[token_start..token_end];

    // Second page with continuation token
    let xml2 = list_objects_raw(
        &http,
        &server.endpoint(),
        server.bucket(),
        &format!("prefix={}/&max-keys=2&continuation-token={}", prefix, token),
    )
    .await;

    assert!(
        xml2.contains("<IsTruncated>false</IsTruncated>"),
        "Second page should not be truncated, got:\n{}",
        xml2
    );
    assert!(
        xml2.contains("<KeyCount>2</KeyCount>"),
        "Second page should have KeyCount=2, got:\n{}",
        xml2
    );

    // Collect all keys across both pages
    let all_xml = format!("{}{}", xml1, xml2);
    let mut keys: Vec<&str> = Vec::new();
    let mut search_from = 0;
    while let Some(pos) = all_xml[search_from..].find("<Key>") {
        let abs_pos = search_from + pos + 5;
        let end = all_xml[abs_pos..].find("</Key>").unwrap() + abs_pos;
        keys.push(&all_xml[abs_pos..end]);
        search_from = end;
    }
    assert_eq!(
        keys.len(),
        4,
        "Should have 4 keys total across both pages: {:?}",
        keys
    );
}

#[tokio::test]
async fn test_first_file_bad_delta_ratio_passthrough() {
    skip_unless_docker!();
    // Use a very low max_delta_ratio so the identity delta (first file against itself)
    // exceeds the threshold and triggers the passthrough fallback
    let endpoint = minio_endpoint().await;
    ensure_bucket(&endpoint).await;
    let server = TestServer::s3_with_endpoint_and_delta_ratio(&endpoint, TEST_BUCKET, 0.001).await;
    let http = reqwest::Client::new();
    let prefix = unique_prefix();

    let data = generate_binary(1024, 99999);

    let st = put_and_get_storage_type(
        &http,
        &server.endpoint(),
        server.bucket(),
        &format!("{}/random.zip", prefix),
        data.clone(),
        "application/zip",
    )
    .await;
    assert_eq!(
        st, "passthrough",
        "First file with delta ratio exceeding threshold should be passthrough, got: {}",
        st
    );

    // Verify the data round-trips correctly
    let retrieved = get_bytes(
        &http,
        &server.endpoint(),
        server.bucket(),
        &format!("{}/random.zip", prefix),
    )
    .await;
    assert_eq!(
        retrieved, data,
        "Passthrough file should round-trip correctly"
    );
}
