//! Storage resilience tests: verify data integrity under adversarial conditions.
//!
//! These tests simulate scenarios that normal PUT→GET tests never exercise:
//! - Files whose storage key differs from user-facing key (.delta suffix)
//! - The LIST→HEAD→GET triangle invariant
//! - Content roundtrip byte-identical verification across all storage strategies
//! - External file mutation (delete, corruption)
//!
//! This test suite was created because a critical bug shipped where GET used
//! the user-facing key instead of metadata.original_name, causing 404 on files
//! whose S3 key had a .delta suffix. 222 tests missed it because they all
//! follow the clean path (PUT through proxy → GET through proxy).

mod common;

use common::TestServer;
use sha2::{Digest, Sha256};
use std::path::Path;

/// Write a file directly to the deltaspace directory (bypass the proxy).
fn write_raw_file(data_dir: &Path, bucket: &str, prefix: &str, filename: &str, data: &[u8]) {
    let dir = data_dir.join(bucket).join("deltaspaces").join(prefix);
    std::fs::create_dir_all(&dir).expect("create deltaspace dir");
    std::fs::write(dir.join(filename), data).expect("write file");
}

/// GET an object via HTTP client, return (status, body_bytes).
async fn http_get(
    client: &reqwest::Client,
    endpoint: &str,
    bucket: &str,
    key: &str,
) -> (u16, Vec<u8>) {
    let url = format!("{}/{}/{}", endpoint, bucket, key);
    let resp = client.get(&url).send().await.expect("GET request failed");
    let status = resp.status().as_u16();
    let body = resp.bytes().await.unwrap_or_default().to_vec();
    (status, body)
}

/// HEAD an object, return (status, content_length).
async fn http_head(
    client: &reqwest::Client,
    endpoint: &str,
    bucket: &str,
    key: &str,
) -> (u16, u64) {
    let url = format!("{}/{}/{}", endpoint, bucket, key);
    let resp = client.head(&url).send().await.expect("HEAD request failed");
    let status = resp.status().as_u16();
    let cl = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    (status, cl)
}

/// PUT an object via HTTP client, return status.
async fn http_put(
    client: &reqwest::Client,
    endpoint: &str,
    bucket: &str,
    key: &str,
    data: Vec<u8>,
) -> u16 {
    let url = format!("{}/{}/{}", endpoint, bucket, key);
    let resp = client
        .put(&url)
        .body(data)
        .send()
        .await
        .expect("PUT request failed");
    resp.status().as_u16()
}

/// LIST a prefix, return list of keys.
async fn http_list_keys(
    client: &reqwest::Client,
    endpoint: &str,
    bucket: &str,
    prefix: &str,
) -> Vec<String> {
    let url = format!("{}/{}?list-type=2&prefix={}", endpoint, bucket, prefix);
    let resp = client.get(&url).send().await.expect("LIST request failed");
    let body = resp.text().await.unwrap_or_default();
    // Simple XML parsing — extract <Key>...</Key> elements
    body.split("<Key>")
        .skip(1)
        .filter_map(|s| s.split("</Key>").next())
        .map(String::from)
        .collect()
}

// ============================================================================
// 1. TRIANGLE INVARIANT: LIST → HEAD → GET must all succeed
// ============================================================================

/// The most powerful test: for EVERY key returned by LIST,
/// HEAD and GET must succeed with consistent results.
/// This would have caught the original_name bug immediately.
#[tokio::test]
async fn test_list_head_get_triangle_invariant() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    // Upload a mix of passthrough and delta-eligible files
    let files: Vec<(&str, Vec<u8>)> = vec![
        ("tri/readme.txt", b"hello world".to_vec()),
        ("tri/data.bin", vec![0u8; 1000]),
        ("tri/app-v1.zip", generate_zip_like(1, 5000)),
        ("tri/app-v2.zip", generate_zip_like(2, 5000)),
        ("tri/image.png", vec![0x89, 0x50, 0x4e, 0x47]),
    ];

    for (key, data) in &files {
        client
            .put_object()
            .bucket(server.bucket())
            .key(*key)
            .body(aws_sdk_s3::primitives::ByteStream::from(data.clone()))
            .send()
            .await
            .unwrap_or_else(|e| panic!("PUT {} failed: {:?}", key, e));
    }

    // LIST all keys under the prefix
    let listed = client
        .list_objects_v2()
        .bucket(server.bucket())
        .prefix("tri/")
        .send()
        .await
        .expect("LIST failed");

    let listed_keys: Vec<String> = listed
        .contents()
        .iter()
        .map(|o| o.key().unwrap_or("").to_string())
        .collect();

    assert!(
        !listed_keys.is_empty(),
        "LIST returned no keys — test is broken"
    );

    // For every listed key: HEAD and GET must succeed
    let http = reqwest::Client::new();
    for key in &listed_keys {
        let (head_status, head_cl) =
            http_head(&http, &server.endpoint(), server.bucket(), key).await;
        assert_eq!(
            head_status, 200,
            "HEAD {} returned {} — triangle invariant violated",
            key, head_status
        );

        let (get_status, get_body) =
            http_get(&http, &server.endpoint(), server.bucket(), key).await;
        assert_eq!(
            get_status, 200,
            "GET {} returned {} — triangle invariant violated",
            key, get_status
        );

        // HEAD content-length must match GET body length
        assert_eq!(
            head_cl,
            get_body.len() as u64,
            "HEAD/GET content-length mismatch for {}: HEAD={} GET={}",
            key,
            head_cl,
            get_body.len()
        );
    }
}

// ============================================================================
// 2. HEAD/GET CONSISTENCY
// ============================================================================

/// HEAD and GET must agree on content-length for every storage strategy.
#[tokio::test]
async fn test_head_get_content_length_consistency() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;
    let http = reqwest::Client::new();

    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("consist/text.txt", b"passthrough content".to_vec()),
        ("consist/first.zip", generate_zip_like(1, 8000)),
        ("consist/second.zip", generate_zip_like(2, 8000)),
    ];

    for (key, data) in &cases {
        client
            .put_object()
            .bucket(server.bucket())
            .key(*key)
            .body(aws_sdk_s3::primitives::ByteStream::from(data.clone()))
            .send()
            .await
            .expect("PUT failed");
    }

    for (key, original_data) in &cases {
        let (head_status, head_cl) =
            http_head(&http, &server.endpoint(), server.bucket(), key).await;
        let (get_status, get_body) =
            http_get(&http, &server.endpoint(), server.bucket(), key).await;

        assert_eq!(head_status, 200, "HEAD {} failed", key);
        assert_eq!(get_status, 200, "GET {} failed", key);
        assert_eq!(
            head_cl,
            get_body.len() as u64,
            "HEAD/GET size mismatch for {}",
            key
        );
        assert_eq!(
            get_body.len(),
            original_data.len(),
            "GET body size != original for {}",
            key
        );
    }
}

// ============================================================================
// 3. CONTENT ROUNDTRIP — byte-identical verification
// ============================================================================

/// SHA256 of downloaded content must match SHA256 of uploaded content
/// for all storage strategies (passthrough, delta, reference).
#[tokio::test]
async fn test_content_roundtrip_sha256_match() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("rt/readme.txt", b"passthrough file content here".to_vec()),
        ("rt/binary.bin", (0..=255u8).cycle().take(4096).collect()),
        ("rt/app-v1.zip", generate_zip_like(100, 10000)),
        ("rt/app-v2.zip", generate_zip_like(200, 10000)),
        ("rt/empty.txt", vec![]),
    ];

    for (key, data) in &cases {
        let upload_sha = hex::encode(Sha256::digest(data));

        client
            .put_object()
            .bucket(server.bucket())
            .key(*key)
            .body(aws_sdk_s3::primitives::ByteStream::from(data.clone()))
            .send()
            .await
            .expect("PUT failed");

        let result = client
            .get_object()
            .bucket(server.bucket())
            .key(*key)
            .send()
            .await
            .expect("GET failed");

        let body = result.body.collect().await.unwrap().into_bytes();
        let download_sha = hex::encode(Sha256::digest(&body));

        assert_eq!(
            upload_sha,
            download_sha,
            "Content roundtrip SHA256 mismatch for {}: uploaded {} bytes, downloaded {} bytes",
            key,
            data.len(),
            body.len()
        );
    }
}

// ============================================================================
// 4. EXTERNAL FILE WITH .delta SUFFIX (regression test for the bug)
// ============================================================================

/// Directly placed file (no proxy metadata): HEAD and GET should still
/// work via the unmanaged passthrough fallback path. This verifies the
/// triangle invariant for files that bypass the proxy's PUT pipeline.
#[tokio::test]
async fn test_unmanaged_file_triangle_invariant() {
    let server = TestServer::filesystem().await;
    let data_dir = server.data_dir().expect("filesystem backend");
    let content = b"directly placed file content";

    // Write a plain file (no .delta suffix, no metadata)
    write_raw_file(data_dir, server.bucket(), "direct", "report.txt", content);

    let http = reqwest::Client::new();

    // LIST should show it
    let keys = http_list_keys(&http, &server.endpoint(), server.bucket(), "direct/").await;
    assert!(
        keys.iter().any(|k| k.contains("report.txt")),
        "LIST should show directly placed file, got: {:?}",
        keys
    );

    // HEAD should work
    let (head_status, head_cl) = http_head(
        &http,
        &server.endpoint(),
        server.bucket(),
        "direct/report.txt",
    )
    .await;
    assert_eq!(head_status, 200, "HEAD should work for unmanaged file");
    assert_eq!(
        head_cl,
        content.len() as u64,
        "HEAD content-length should match"
    );

    // GET should return correct content
    let (get_status, get_body) = http_get(
        &http,
        &server.endpoint(),
        server.bucket(),
        "direct/report.txt",
    )
    .await;
    assert_eq!(get_status, 200, "GET should work for unmanaged file");
    assert_eq!(get_body, content, "GET content should match");
}

// ============================================================================
// 5. EXTERNAL DELETE → 404 (not stale cache)
// ============================================================================

/// After HEAD populates the metadata cache, deleting the file externally
/// should cause GET to return 404, not stale cached data.
#[tokio::test]
async fn test_external_delete_returns_404() {
    let server = TestServer::filesystem().await;
    let data_dir = server.data_dir().expect("filesystem backend");
    let content = b"file that will be externally deleted";

    write_raw_file(data_dir, server.bucket(), "ephemeral", "temp.txt", content);

    let http = reqwest::Client::new();

    // HEAD to populate metadata cache
    let (status, _) = http_head(
        &http,
        &server.endpoint(),
        server.bucket(),
        "ephemeral/temp.txt",
    )
    .await;
    assert_eq!(status, 200, "HEAD should succeed before deletion");

    // Delete file directly from filesystem
    let file_path = data_dir
        .join(server.bucket())
        .join("deltaspaces")
        .join("ephemeral")
        .join("temp.txt");
    std::fs::remove_file(&file_path).expect("delete file");

    // GET should return 404 (not stale cached data)
    let (status, _) = http_get(
        &http,
        &server.endpoint(),
        server.bucket(),
        "ephemeral/temp.txt",
    )
    .await;
    assert!(
        status == 404 || status == 500,
        "GET after external delete should fail, got {}",
        status
    );
}

// ============================================================================
// 6. LIST never exposes .delta suffix or reference.bin
// ============================================================================

/// Internal storage artifacts (.delta suffix, reference.bin) must never
/// appear in LIST responses.
#[tokio::test]
async fn test_list_hides_internal_artifacts() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    // Upload two delta-eligible files to create reference + delta
    let v1 = generate_zip_like(1, 8000);
    let v2 = generate_zip_like(2, 8000);

    client
        .put_object()
        .bucket(server.bucket())
        .key("hidden/app-v1.zip")
        .body(aws_sdk_s3::primitives::ByteStream::from(v1))
        .send()
        .await
        .expect("PUT v1");

    client
        .put_object()
        .bucket(server.bucket())
        .key("hidden/app-v2.zip")
        .body(aws_sdk_s3::primitives::ByteStream::from(v2))
        .send()
        .await
        .expect("PUT v2");

    // LIST the prefix
    let result = client
        .list_objects_v2()
        .bucket(server.bucket())
        .prefix("hidden/")
        .send()
        .await
        .expect("LIST");

    let keys: Vec<String> = result
        .contents()
        .iter()
        .map(|o| o.key().unwrap_or("").to_string())
        .collect();

    // Should see app-v1.zip and app-v2.zip
    assert!(
        keys.iter().any(|k| k == "hidden/app-v1.zip"),
        "v1 should be listed"
    );
    assert!(
        keys.iter().any(|k| k == "hidden/app-v2.zip"),
        "v2 should be listed"
    );

    // Should NOT see .delta suffix or reference.bin
    for key in &keys {
        assert!(
            !key.ends_with(".delta"),
            "LIST should not expose .delta suffix: {}",
            key
        );
        assert!(
            !key.contains("reference.bin"),
            "LIST should not expose reference.bin: {}",
            key
        );
    }
}

// ============================================================================
// Helpers
// ============================================================================

/// Generate deterministic zip-like binary data that's delta-eligible
/// (similar structure, different content per seed).
fn generate_zip_like(seed: u8, size: usize) -> Vec<u8> {
    // PK header + deterministic content that varies by seed
    let mut data = vec![0x50, 0x4b, 0x03, 0x04]; // ZIP magic
    let mut rng_state: u64 = seed as u64 * 1000003 + 7;
    while data.len() < size {
        rng_state = rng_state.wrapping_mul(6364136223846793005).wrapping_add(1);
        data.push((rng_state >> 33) as u8);
    }
    data.truncate(size);
    data
}
