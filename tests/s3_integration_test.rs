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

use common::{generate_binary, mutate_binary, TestServer, MINIO_ACCESS_KEY, MINIO_SECRET_KEY};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicU64, Ordering};
use testcontainers::core::IntoContainerPort;
use testcontainers::runners::AsyncRunner;
use testcontainers::ContainerAsync;
use testcontainers_modules::minio::MinIO;
use tokio::sync::OnceCell;

/// Shared MinIO container for all tests in this file
static MINIO_CONTAINER: OnceCell<ContainerAsync<MinIO>> = OnceCell::const_new();

/// Counter for unique test prefixes
static PREFIX_COUNTER: AtomicU64 = AtomicU64::new(0);

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
    let container = MINIO_CONTAINER
        .get_or_init(|| async {
            MinIO::default()
                .start()
                .await
                .expect("Failed to start MinIO container")
        })
        .await;

    let host = container.get_host().await.unwrap();
    let port = container.get_host_port_ipv4(9000.tcp()).await.unwrap();
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
        .behavior_version(aws_config::BehaviorVersion::latest())
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

/// Helper to PUT via reqwest and return the x-amz-storage-type header
async fn put_and_get_storage_type(
    client: &reqwest::Client,
    endpoint: &str,
    bucket: &str,
    key: &str,
    data: Vec<u8>,
    content_type: &str,
) -> String {
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
    resp.headers()
        .get("x-amz-storage-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string()
}

/// Helper to GET via reqwest and return bytes
async fn get_bytes(client: &reqwest::Client, endpoint: &str, bucket: &str, key: &str) -> Vec<u8> {
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

/// Helper to HEAD via reqwest and return all response headers
async fn head_headers(
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

    // Upload zip base (reference), zip variant (delta), and text (direct)
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
    let st_direct = put_and_get_storage_type(
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
    assert_eq!(st_direct, "direct", "Text should be direct");

    // HEAD the direct file — should have dg-tool but lack delta-specific headers
    let direct_headers = head_headers(
        &http,
        &server.endpoint(),
        server.bucket(),
        &format!("{}/readme.txt", prefix),
    )
    .await;

    // Direct files should NOT have delta-specific metadata
    assert!(
        direct_headers.get("x-amz-meta-dg-ref-key").is_none(),
        "Direct files should not have dg-ref-key"
    );
    assert!(
        direct_headers.get("x-amz-meta-dg-delta-size").is_none(),
        "Direct files should not have dg-delta-size"
    );
}

// ============================================================================
// Group 4: Non-compressed file integrity
// ============================================================================

#[tokio::test]
async fn test_text_file_direct_roundtrip() {
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
    assert_eq!(st, "direct", ".txt should be stored as direct");

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
            st, "direct",
            "{} should be stored as direct, got: {}",
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
    assert_eq!(st_txt, "direct", "txt should be direct");

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
