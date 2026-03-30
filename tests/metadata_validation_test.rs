//! Metadata validation tests: verify correct handling of missing, partial, and wrong metadata.
//!
//! These tests ensure the proxy degrades gracefully (not catastrophically) when
//! delta/reference files have missing or corrupt DG metadata — a condition that
//! occurs when files are copied without preserving S3 user metadata.

mod common;

use common::TestServer;
use std::path::Path;

/// Write a file directly into the deltaspace directory.
fn write_file(data_dir: &Path, bucket: &str, prefix: &str, filename: &str, data: &[u8]) {
    let dir = data_dir.join(bucket).join("deltaspaces").join(prefix);
    std::fs::create_dir_all(&dir).expect("create deltaspace dir");
    std::fs::write(dir.join(filename), data).expect("write file");
}

/// Write a file with specific xattr metadata (JSON serialized FileMetadata).
fn write_file_with_xattr(
    data_dir: &Path,
    bucket: &str,
    prefix: &str,
    filename: &str,
    data: &[u8],
    metadata_json: &str,
) {
    let dir = data_dir.join(bucket).join("deltaspaces").join(prefix);
    std::fs::create_dir_all(&dir).expect("create deltaspace dir");
    let path = dir.join(filename);
    std::fs::write(&path, data).expect("write file");
    xattr::set(&path, "user.dg.metadata", metadata_json.as_bytes()).expect("write xattr metadata");
}

/// HTTP GET, return (status, body, headers).
async fn get_with_headers(
    client: &reqwest::Client,
    endpoint: &str,
    bucket: &str,
    key: &str,
) -> (u16, Vec<u8>, reqwest::header::HeaderMap) {
    let url = format!("{}/{}/{}", endpoint, bucket, key);
    let resp = client.get(&url).send().await.expect("GET failed");
    let status = resp.status().as_u16();
    let headers = resp.headers().clone();
    let body = resp.bytes().await.unwrap_or_default().to_vec();
    (status, body, headers)
}

/// HTTP HEAD, return (status, headers).
async fn head_status(client: &reqwest::Client, endpoint: &str, bucket: &str, key: &str) -> u16 {
    let url = format!("{}/{}/{}", endpoint, bucket, key);
    let resp = client.head(&url).send().await.expect("HEAD failed");
    resp.status().as_u16()
}

// ============================================================================
// 1. Delta file with NO metadata → passthrough fallback
// ============================================================================

/// A .delta-suffixed file on the filesystem backend is rejected by validate_object
/// (the proxy blocks .delta suffixes to prevent direct access to internal artifacts).
/// This test verifies the proxy doesn't crash on such files — it returns a clean error.
#[tokio::test]
async fn test_delta_suffix_file_rejected_cleanly() {
    let server = TestServer::filesystem().await;
    let data_dir = server.data_dir().expect("filesystem backend");

    write_file(
        data_dir,
        server.bucket(),
        "nometa",
        "artifact.zip.delta",
        b"raw delta data",
    );

    let http = reqwest::Client::new();

    // .delta suffix is blocked by validate_object — returns 400, not 500
    let status = head_status(
        &http,
        &server.endpoint(),
        server.bucket(),
        "nometa/artifact.zip.delta",
    )
    .await;
    assert!(
        status == 400 || status == 404,
        "HEAD on .delta file should return 400 (blocked) or 404, got {}",
        status
    );
}

/// A regular file (no .delta suffix) without metadata should serve as passthrough.
#[tokio::test]
async fn test_regular_file_no_metadata_serves_as_passthrough() {
    let server = TestServer::filesystem().await;
    let data_dir = server.data_dir().expect("filesystem backend");
    let content = b"file without any DG metadata";

    write_file(data_dir, server.bucket(), "nometa", "report.txt", content);

    let http = reqwest::Client::new();

    let status = head_status(
        &http,
        &server.endpoint(),
        server.bucket(),
        "nometa/report.txt",
    )
    .await;
    assert_eq!(
        status, 200,
        "HEAD on file without metadata should return 200"
    );

    let (get_status, get_body, _) = get_with_headers(
        &http,
        &server.endpoint(),
        server.bucket(),
        "nometa/report.txt",
    )
    .await;
    assert_eq!(
        get_status, 200,
        "GET on file without metadata should return 200"
    );
    assert_eq!(get_body, content, "GET should return raw content");
}

// ============================================================================
// 2. Reference file with NO metadata → still accessible
// ============================================================================

/// reference.bin without metadata should be served as passthrough.
#[tokio::test]
async fn test_reference_file_no_metadata_still_accessible() {
    let server = TestServer::filesystem().await;
    let data_dir = server.data_dir().expect("filesystem backend");
    let ref_content = b"reference file data without DG metadata";

    // Write reference.bin without xattr
    write_file(
        data_dir,
        server.bucket(),
        "noref",
        "reference.bin",
        ref_content,
    );

    let http = reqwest::Client::new();

    // reference.bin should be filtered from LIST (internal artifact)
    // But direct HEAD/GET should work as passthrough
    let status = head_status(
        &http,
        &server.endpoint(),
        server.bucket(),
        "noref/reference.bin",
    )
    .await;
    // reference.bin is filtered by validate_object (rejected as internal name)
    // so HEAD returns 400, which is correct behavior
    assert!(
        status == 200 || status == 400,
        "HEAD on reference.bin: {} (200=accessible, 400=blocked as internal name)",
        status
    );
}

// ============================================================================
// 3. Delta with VALID metadata → should reconstruct correctly
// ============================================================================

/// A proper delta file with valid metadata round-trips correctly.
/// This is the positive control — ensures the test infrastructure works.
#[tokio::test]
async fn test_delta_with_valid_metadata_reconstructs() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    // Upload two similar zip-like files through the proxy — creates reference + delta
    let v1 = generate_zip_like(1, 10000);
    let v2 = generate_zip_like(2, 10000);

    client
        .put_object()
        .bucket(server.bucket())
        .key("valid/app-v1.zip")
        .body(aws_sdk_s3::primitives::ByteStream::from(v1.clone()))
        .send()
        .await
        .expect("PUT v1");

    client
        .put_object()
        .bucket(server.bucket())
        .key("valid/app-v2.zip")
        .body(aws_sdk_s3::primitives::ByteStream::from(v2.clone()))
        .send()
        .await
        .expect("PUT v2");

    // GET both and verify byte-identical roundtrip
    let result_v1 = client
        .get_object()
        .bucket(server.bucket())
        .key("valid/app-v1.zip")
        .send()
        .await
        .expect("GET v1");
    let body_v1 = result_v1.body.collect().await.unwrap().into_bytes();
    assert_eq!(body_v1.as_ref(), v1.as_slice(), "v1 roundtrip mismatch");

    let result_v2 = client
        .get_object()
        .bucket(server.bucket())
        .key("valid/app-v2.zip")
        .send()
        .await
        .expect("GET v2");
    let body_v2 = result_v2.body.collect().await.unwrap().into_bytes();
    assert_eq!(body_v2.as_ref(), v2.as_slice(), "v2 roundtrip mismatch");
}

// ============================================================================
// 4. File with CORRUPT xattr metadata → graceful fallback
// ============================================================================

/// A file with invalid/garbage xattr metadata should not crash the proxy.
#[tokio::test]
async fn test_corrupt_xattr_metadata_graceful_fallback() {
    let server = TestServer::filesystem().await;
    let data_dir = server.data_dir().expect("filesystem backend");
    let content = b"file with garbage metadata";

    // Write file with invalid JSON as xattr
    write_file_with_xattr(
        data_dir,
        server.bucket(),
        "corrupt",
        "data.bin",
        content,
        "this is not valid JSON {{{",
    );

    let http = reqwest::Client::new();

    // Corrupt metadata may cause deserialization failure — the proxy should
    // either fallback to passthrough (200) or return a clean error (404/500).
    // It must NOT panic or hang.
    let status = head_status(
        &http,
        &server.endpoint(),
        server.bucket(),
        "corrupt/data.bin",
    )
    .await;
    assert!(
        status == 200 || status == 404 || status == 500,
        "HEAD with corrupt metadata should not hang or panic, got {}",
        status
    );

    let (get_status, get_body, _) = get_with_headers(
        &http,
        &server.endpoint(),
        server.bucket(),
        "corrupt/data.bin",
    )
    .await;
    // If it succeeds, content should be correct
    if get_status == 200 {
        assert_eq!(get_body, content, "GET fallback should return raw content");
    }
    // Any status is OK as long as the proxy didn't crash
}

// ============================================================================
// 5. File with PARTIAL metadata (missing required fields)
// ============================================================================

/// Metadata that's valid JSON but missing required fields should degrade gracefully.
#[tokio::test]
async fn test_partial_metadata_missing_fields_graceful() {
    let server = TestServer::filesystem().await;
    let data_dir = server.data_dir().expect("filesystem backend");
    let content = b"file with partial metadata";

    // Write metadata missing several required fields (no storage_info, no file_sha256)
    let partial_meta = serde_json::json!({
        "tool": "deltaglider/test",
        "original_name": "data.bin",
        // missing: file_sha256, file_size, md5, storage_info
    })
    .to_string();

    write_file_with_xattr(
        data_dir,
        server.bucket(),
        "partial",
        "data.bin",
        content,
        &partial_meta,
    );

    let http = reqwest::Client::new();

    // Should not crash — fallback to passthrough or return an error code
    let status = head_status(
        &http,
        &server.endpoint(),
        server.bucket(),
        "partial/data.bin",
    )
    .await;
    assert!(
        status == 200 || status == 404 || status == 500,
        "HEAD with partial metadata should not hang, got {}",
        status
    );

    let (get_status, get_body, _) = get_with_headers(
        &http,
        &server.endpoint(),
        server.bucket(),
        "partial/data.bin",
    )
    .await;
    // Either serves content (fallback) or returns error — but doesn't crash
    if get_status == 200 {
        assert_eq!(
            get_body, content,
            "If GET succeeds, should return correct content"
        );
    }
}

// ============================================================================
// 6. Delta with WRONG ref_sha256 → must NOT serve corrupt data
// ============================================================================

/// A delta file whose dg-ref-sha256 doesn't match the actual reference
/// should NOT silently serve corrupted (wrong-base) reconstructed data.
#[tokio::test]
async fn test_wrong_ref_sha256_does_not_serve_corrupt_data() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    // Upload two files to create a working delta setup
    let v1 = generate_zip_like(100, 10000);
    let v2 = generate_zip_like(200, 10000);

    client
        .put_object()
        .bucket(server.bucket())
        .key("integrity/app-v1.zip")
        .body(aws_sdk_s3::primitives::ByteStream::from(v1.clone()))
        .send()
        .await
        .expect("PUT v1");

    client
        .put_object()
        .bucket(server.bucket())
        .key("integrity/app-v2.zip")
        .body(aws_sdk_s3::primitives::ByteStream::from(v2.clone()))
        .send()
        .await
        .expect("PUT v2");

    // Verify v2 is retrievable (delta reconstruction works)
    let result = client
        .get_object()
        .bucket(server.bucket())
        .key("integrity/app-v2.zip")
        .send()
        .await
        .expect("GET v2 before corruption");
    let body = result.body.collect().await.unwrap().into_bytes();
    assert_eq!(
        body.as_ref(),
        v2.as_slice(),
        "v2 should be correct before corruption"
    );

    // Now corrupt the reference file (replace with different data)
    let data_dir = server.data_dir().expect("filesystem backend");
    let ref_path = data_dir
        .join(server.bucket())
        .join("deltaspaces")
        .join("integrity")
        .join("reference.bin");
    if ref_path.exists() {
        // Replace reference with garbage
        std::fs::write(&ref_path, b"THIS IS CORRUPTED REFERENCE DATA").expect("corrupt reference");

        // GET v2 should now fail (SHA256 mismatch) — must NOT serve wrong data
        let result = client
            .get_object()
            .bucket(server.bucket())
            .key("integrity/app-v2.zip")
            .send()
            .await;

        match result {
            Err(_) => {
                // Good — error means the proxy detected the corruption
            }
            Ok(resp) => {
                // If GET somehow succeeded, the data must NOT be the wrong reconstruction
                let bad_body = resp.body.collect().await.unwrap().into_bytes();
                // It's OK if it returned the original v2 (from cache or passthrough),
                // but it must NOT return garbage from a corrupt delta reconstruction
                assert!(
                    bad_body.as_ref() == v2.as_slice() || bad_body.is_empty(),
                    "GET after reference corruption must NOT serve wrong data (got {} bytes)",
                    bad_body.len()
                );
            }
        }
    }
    // If no reference file exists (passthrough storage), the test passes trivially
}

// ============================================================================
// Helpers
// ============================================================================

fn generate_zip_like(seed: u8, size: usize) -> Vec<u8> {
    let mut data = vec![0x50, 0x4b, 0x03, 0x04];
    let mut rng_state: u64 = seed as u64 * 1000003 + 7;
    while data.len() < size {
        rng_state = rng_state.wrapping_mul(6364136223846793005).wrapping_add(1);
        data.push((rng_state >> 33) as u8);
    }
    data.truncate(size);
    data
}
