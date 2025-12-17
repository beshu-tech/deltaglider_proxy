//! Full proxy flow integration tests against MinIO
//!
//! These tests verify the complete DeltaGlider Proxy flow:
//! 1. Client -> DeltaGlider Proxy S3 API -> DeltaGlider -> S3 Backend (MinIO)
//!
//! Prerequisites:
//!   docker compose up -d
//!
//! Run manually (tests require running server):
//!   # Terminal 1: Start server with S3 backend
//!   DELTAGLIDER_PROXY_LISTEN_ADDR="127.0.0.1:18888" \
//!   DELTAGLIDER_PROXY_S3_BUCKET="deltaglider-test" \
//!   DELTAGLIDER_PROXY_S3_ENDPOINT="http://localhost:9000" \
//!   AWS_ACCESS_KEY_ID="minioadmin" \
//!   AWS_SECRET_ACCESS_KEY="minioadmin" \
//!   cargo run --release
//!
//!   # Terminal 2: Run tests
//!   cargo test --test proxy_flow_test -- --nocapture --test-threads=1
//!
//! Note: These tests are designed for manual verification with a running DeltaGlider Proxy server.
//! For CI, use the s3_backend_test.rs which tests the S3Backend directly without server.

use std::time::{SystemTime, UNIX_EPOCH};

/// Test configuration
const DELTAGLIDER_PROXY_URL: &str = "http://127.0.0.1:18888";

/// Helper to check if DeltaGlider Proxy server is available
async fn server_available() -> bool {
    let client = reqwest::Client::new();
    // Try a bucket request - server returns 404 for non-existent objects but responds
    match client
        .get(format!("{}/test-bucket/health-check", DELTAGLIDER_PROXY_URL))
        .send()
        .await
    {
        Ok(resp) => {
            // Server is available if it responds (even with 404)
            resp.status().as_u16() == 404 || resp.status().is_success()
        }
        Err(_) => false,
    }
}

/// Helper to create unique test keys
fn test_key(prefix: &str) -> String {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis();
    format!("{}-{}", prefix, ts)
}

#[tokio::test]
async fn test_proxy_basic_crud() {
    if !server_available().await {
        eprintln!("SKIP: DeltaGlider Proxy server not available at {}", DELTAGLIDER_PROXY_URL);
        eprintln!("Start the server with S3 backend to run this test");
        return;
    }

    let client = reqwest::Client::new();
    let bucket = "test-bucket";
    let key = test_key("crud-test.txt");

    // PUT object
    let put_url = format!("{}/{}/{}", DELTAGLIDER_PROXY_URL, bucket, key);
    let test_data = b"Hello from proxy flow test!";

    let put_resp = client
        .put(&put_url)
        .body(test_data.to_vec())
        .send()
        .await
        .expect("PUT request failed");

    assert!(
        put_resp.status().is_success(),
        "PUT failed with status: {}",
        put_resp.status()
    );

    // Check ETag is returned
    assert!(
        put_resp.headers().contains_key("etag"),
        "PUT should return ETag"
    );

    // GET object
    let get_resp = client
        .get(&put_url)
        .send()
        .await
        .expect("GET request failed");

    assert!(
        get_resp.status().is_success(),
        "GET failed with status: {}",
        get_resp.status()
    );

    let body = get_resp.bytes().await.expect("Failed to read body");
    assert_eq!(body.as_ref(), test_data, "Retrieved data doesn't match");

    // HEAD object
    let head_resp = client
        .head(&put_url)
        .send()
        .await
        .expect("HEAD request failed");

    assert!(
        head_resp.status().is_success(),
        "HEAD failed with status: {}",
        head_resp.status()
    );

    let content_length = head_resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok());

    assert_eq!(content_length, Some(test_data.len()));

    // DELETE object
    let delete_resp = client
        .delete(&put_url)
        .send()
        .await
        .expect("DELETE request failed");

    assert!(
        delete_resp.status().is_success() || delete_resp.status().as_u16() == 204,
        "DELETE failed with status: {}",
        delete_resp.status()
    );

    // Verify deletion
    let get_after_delete = client.get(&put_url).send().await.expect("GET failed");
    assert_eq!(
        get_after_delete.status().as_u16(),
        404,
        "Object should be deleted"
    );

    println!("✅ Basic CRUD test passed");
}

#[tokio::test]
async fn test_proxy_delta_compression() {
    if !server_available().await {
        eprintln!("SKIP: DeltaGlider Proxy server not available");
        return;
    }

    let client = reqwest::Client::new();
    let bucket = "delta-test";
    let deltaspace = test_key("releases");

    // Create base file (50KB deterministic content)
    let base_content: Vec<u8> = (0..50000).map(|i| ((i * 7 + 13) % 256) as u8).collect();

    // Upload v1.zip (should become reference)
    let v1_key = format!("{}/v1.zip", deltaspace);
    let v1_url = format!("{}/{}/{}", DELTAGLIDER_PROXY_URL, bucket, v1_key);

    let v1_resp = client
        .put(&v1_url)
        .body(base_content.clone())
        .header("content-type", "application/zip")
        .send()
        .await
        .expect("PUT v1 failed");

    assert!(v1_resp.status().is_success(), "v1 upload failed");

    // Check storage type is Reference
    let v1_storage = v1_resp
        .headers()
        .get("x-amz-storage-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    println!("v1.zip storage type: {}", v1_storage);
    assert_eq!(
        v1_storage, "Reference",
        "First .zip file should be stored as reference"
    );

    // Create v2 with small changes (5% modifications)
    let mut v2_content = base_content.clone();
    for i in (0..v2_content.len()).step_by(20) {
        v2_content[i] = v2_content[i].wrapping_add(1);
    }

    let v2_key = format!("{}/v2.zip", deltaspace);
    let v2_url = format!("{}/{}/{}", DELTAGLIDER_PROXY_URL, bucket, v2_key);

    let v2_resp = client
        .put(&v2_url)
        .body(v2_content.clone())
        .header("content-type", "application/zip")
        .send()
        .await
        .expect("PUT v2 failed");

    assert!(v2_resp.status().is_success(), "v2 upload failed");

    // Check storage type is Delta
    let v2_storage = v2_resp
        .headers()
        .get("x-amz-storage-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    println!("v2.zip storage type: {}", v2_storage);
    assert_eq!(
        v2_storage, "Delta",
        "Second .zip file with small changes should be stored as delta"
    );

    // Retrieve v2 and verify reconstruction
    let get_v2 = client.get(&v2_url).send().await.expect("GET v2 failed");

    assert!(get_v2.status().is_success(), "GET v2 failed");

    let retrieved = get_v2.bytes().await.expect("Failed to read v2 body");
    assert_eq!(
        retrieved.len(),
        v2_content.len(),
        "v2 content length mismatch"
    );
    assert_eq!(
        retrieved.as_ref(),
        v2_content.as_slice(),
        "v2 content mismatch - delta reconstruction failed"
    );

    // Also verify v1 still works
    let get_v1 = client.get(&v1_url).send().await.expect("GET v1 failed");

    assert!(get_v1.status().is_success(), "GET v1 failed");

    let v1_retrieved = get_v1.bytes().await.expect("Failed to read v1 body");
    assert_eq!(
        v1_retrieved.as_ref(),
        base_content.as_slice(),
        "v1 content mismatch"
    );

    println!("✅ Delta compression test passed");
    println!(
        "  Storage savings: {} bytes original -> delta compressed",
        v2_content.len()
    );
}

#[tokio::test]
async fn test_proxy_non_delta_files() {
    if !server_available().await {
        eprintln!("SKIP: DeltaGlider Proxy server not available");
        return;
    }

    let client = reqwest::Client::new();
    let bucket = "direct-test";
    let key = test_key("document.txt");

    // Upload a .txt file (not delta-eligible)
    let url = format!("{}/{}/{}", DELTAGLIDER_PROXY_URL, bucket, key);
    let content = b"This is a text file, not delta-eligible";

    let resp = client
        .put(&url)
        .body(content.to_vec())
        .send()
        .await
        .expect("PUT failed");

    assert!(resp.status().is_success(), "PUT failed");

    // Check storage type is Direct
    let storage_type = resp
        .headers()
        .get("x-amz-storage-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    println!(".txt file storage type: {}", storage_type);
    assert_eq!(
        storage_type, "Direct",
        "Non-archive files should be stored directly"
    );

    // Verify retrieval
    let get_resp = client.get(&url).send().await.expect("GET failed");
    let body = get_resp.bytes().await.expect("Failed to read body");
    assert_eq!(body.as_ref(), content);

    println!("✅ Non-delta files test passed");
}

#[tokio::test]
async fn test_proxy_large_file() {
    if !server_available().await {
        eprintln!("SKIP: DeltaGlider Proxy server not available");
        return;
    }

    let client = reqwest::Client::new();
    let bucket = "large-file-test";
    let key = test_key("large.zip");

    // Create a 1MB file
    let large_content: Vec<u8> = (0..1_000_000)
        .map(|i| ((i * 31 + 17) % 256) as u8)
        .collect();

    let url = format!("{}/{}/{}", DELTAGLIDER_PROXY_URL, bucket, key);

    // Upload
    let put_resp = client
        .put(&url)
        .body(large_content.clone())
        .header("content-type", "application/zip")
        .send()
        .await
        .expect("PUT large file failed");

    assert!(put_resp.status().is_success(), "Large file upload failed");

    // Download and verify
    let get_resp = client
        .get(&url)
        .send()
        .await
        .expect("GET large file failed");

    assert!(get_resp.status().is_success(), "Large file download failed");

    let retrieved = get_resp.bytes().await.expect("Failed to read large file");
    assert_eq!(
        retrieved.len(),
        large_content.len(),
        "Large file size mismatch"
    );
    assert_eq!(
        retrieved.as_ref(),
        large_content.as_slice(),
        "Large file content mismatch"
    );

    println!("✅ Large file test passed (1MB)");
}

#[tokio::test]
async fn test_proxy_multiple_versions() {
    if !server_available().await {
        eprintln!("SKIP: DeltaGlider Proxy server not available");
        return;
    }

    let client = reqwest::Client::new();
    let bucket = "versions-test";
    let deltaspace = test_key("app");

    // Base content
    let base: Vec<u8> = (0..20000).map(|i| ((i * 11 + 7) % 256) as u8).collect();

    // Upload multiple versions
    let versions = vec![
        ("v1.0.zip", base.clone()),
        // v1.1 - small patch (1% change)
        ("v1.1.zip", {
            let mut v = base.clone();
            for i in (0..v.len()).step_by(100) {
                v[i] = v[i].wrapping_add(1);
            }
            v
        }),
        // v1.2 - another small patch
        ("v1.2.zip", {
            let mut v = base.clone();
            for i in (0..v.len()).step_by(50) {
                v[i] = v[i].wrapping_add(2);
            }
            v
        }),
        // v2.0 - bigger change (10%)
        ("v2.0.zip", {
            let mut v = base.clone();
            for i in (0..v.len()).step_by(10) {
                v[i] = v[i].wrapping_add(10);
            }
            v
        }),
    ];

    for (name, content) in &versions {
        let url = format!("{}/{}/{}/{}", DELTAGLIDER_PROXY_URL, bucket, deltaspace, name);
        let resp = client
            .put(&url)
            .body(content.clone())
            .header("content-type", "application/zip")
            .send()
            .await
            .unwrap_or_else(|err| panic!("PUT {name} failed: {err}"));

        let storage_type = resp
            .headers()
            .get("x-amz-storage-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("unknown");

        println!("{}: {}", name, storage_type);
        assert!(resp.status().is_success());
    }

    // Verify all versions can be retrieved correctly
    for (name, expected) in &versions {
        let url = format!("{}/{}/{}/{}", DELTAGLIDER_PROXY_URL, bucket, deltaspace, name);
        let resp = client.get(&url).send().await.expect("GET failed");
        let body = resp.bytes().await.expect("Read failed");
        assert_eq!(
            body.as_ref(),
            expected.as_slice(),
            "{} content mismatch",
            name
        );
    }

    println!("✅ Multiple versions test passed");
}
