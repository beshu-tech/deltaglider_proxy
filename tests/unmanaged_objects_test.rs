//! Tests for "unmanaged" objects — files that exist on the backend storage
//! but were NOT stored through the proxy (i.e. no DeltaGlider metadata).
//!
//! Regression tests for issues #3 and #4: the proxy previously returned 404
//! for such objects because it required DG metadata to be present.

mod common;

use common::TestServer;
use std::path::Path;

/// Write a file directly to the filesystem backend without DG metadata (xattr).
/// This simulates an object that exists on upstream storage but was never
/// stored through the proxy.
fn write_unmanaged_file(data_dir: &Path, bucket: &str, prefix: &str, filename: &str, data: &[u8]) {
    let dir = data_dir
        .join(bucket)
        .join("deltaspaces")
        .join(prefix);
    std::fs::create_dir_all(&dir).expect("Failed to create deltaspace dir");
    std::fs::write(dir.join(filename), data).expect("Failed to write unmanaged file");
}

#[tokio::test]
async fn test_head_unmanaged_object_returns_200() {
    let server = TestServer::filesystem().await;
    let data_dir = server.data_dir().expect("filesystem backend has data_dir");
    let content = b"hello unmanaged world";

    write_unmanaged_file(data_dir, server.bucket(), "docs", "readme.txt", content);

    let client = reqwest::Client::new();
    let url = format!("{}/{}/docs/readme.txt", server.endpoint(), server.bucket());
    let resp = client.head(&url).send().await.unwrap();

    assert_eq!(
        resp.status().as_u16(),
        200,
        "HEAD on unmanaged object should return 200, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn test_get_unmanaged_object_returns_content() {
    let server = TestServer::filesystem().await;
    let data_dir = server.data_dir().expect("filesystem backend has data_dir");
    let content = b"unmanaged file content for GET";

    write_unmanaged_file(data_dir, server.bucket(), "builds", "artifact.bin", content);

    let client = reqwest::Client::new();
    let url = format!(
        "{}/{}/builds/artifact.bin",
        server.endpoint(),
        server.bucket()
    );
    let resp = client.get(&url).send().await.unwrap();

    assert!(
        resp.status().is_success(),
        "GET on unmanaged object should succeed, got {}",
        resp.status()
    );
    let body = resp.bytes().await.unwrap();
    assert_eq!(body.as_ref(), content, "GET body should match written content");
}

#[tokio::test]
async fn test_list_includes_unmanaged_objects() {
    let server = TestServer::filesystem().await;
    let data_dir = server.data_dir().expect("filesystem backend has data_dir");
    let http = reqwest::Client::new();

    // Store a managed object through the proxy
    let managed_data = vec![0u8; 100];
    let url = format!(
        "{}/{}/mixed/managed.dat",
        server.endpoint(),
        server.bucket()
    );
    let resp = http
        .put(&url)
        .header("content-type", "application/octet-stream")
        .body(managed_data)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "PUT managed object failed");

    // Write an unmanaged file directly
    write_unmanaged_file(
        data_dir,
        server.bucket(),
        "mixed",
        "unmanaged.dat",
        b"direct write",
    );

    // LIST should include both
    let list_url = format!(
        "{}/{}?list-type=2&prefix=mixed/",
        server.endpoint(),
        server.bucket()
    );
    let resp = http.get(&list_url).send().await.unwrap();
    assert!(resp.status().is_success(), "LIST failed: {}", resp.status());
    let body = resp.text().await.unwrap();

    assert!(
        body.contains("managed.dat"),
        "LIST should include managed object, got: {}",
        body
    );
    assert!(
        body.contains("unmanaged.dat"),
        "LIST should include unmanaged object, got: {}",
        body
    );
}

#[tokio::test]
async fn test_head_unmanaged_returns_passthrough_storage_type() {
    let server = TestServer::filesystem().await;
    let data_dir = server.data_dir().expect("filesystem backend has data_dir");

    write_unmanaged_file(
        data_dir,
        server.bucket(),
        "types",
        "plain.txt",
        b"passthrough check",
    );

    let client = reqwest::Client::new();
    let url = format!("{}/{}/types/plain.txt", server.endpoint(), server.bucket());
    let resp = client.head(&url).send().await.unwrap();

    assert_eq!(resp.status().as_u16(), 200);
    let storage_type = resp
        .headers()
        .get("x-amz-storage-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("missing");
    assert_eq!(
        storage_type, "passthrough",
        "Unmanaged objects should report storage-type: passthrough"
    );
}

#[tokio::test]
async fn test_delete_unmanaged_object() {
    let server = TestServer::filesystem().await;
    let data_dir = server.data_dir().expect("filesystem backend has data_dir");

    write_unmanaged_file(
        data_dir,
        server.bucket(),
        "cleanup",
        "removeme.bin",
        b"delete me",
    );

    let client = reqwest::Client::new();
    let url = format!(
        "{}/{}/cleanup/removeme.bin",
        server.endpoint(),
        server.bucket()
    );

    // Verify it exists first
    let resp = client.get(&url).send().await.unwrap();
    assert!(resp.status().is_success(), "GET before DELETE should succeed");

    // DELETE
    let resp = client.delete(&url).send().await.unwrap();
    assert!(
        resp.status().is_success() || resp.status().as_u16() == 204,
        "DELETE should succeed, got {}",
        resp.status()
    );

    // Verify it's gone
    let resp = client.get(&url).send().await.unwrap();
    assert_eq!(
        resp.status().as_u16(),
        404,
        "GET after DELETE should return 404"
    );
}

#[tokio::test]
async fn test_mixed_managed_and_unmanaged_listing_sizes() {
    let server = TestServer::filesystem().await;
    let data_dir = server.data_dir().expect("filesystem backend has data_dir");
    let http = reqwest::Client::new();

    // Store managed objects through the proxy
    for i in 0..3 {
        let data = vec![i as u8; 500 + i * 100];
        let url = format!(
            "{}/{}/batch/managed_{}.bin",
            server.endpoint(),
            server.bucket(),
            i
        );
        let resp = http
            .put(&url)
            .header("content-type", "application/octet-stream")
            .body(data)
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success());
    }

    // Write unmanaged files directly
    for i in 0..2 {
        let data = vec![0xAA; 200 + i * 50];
        write_unmanaged_file(
            data_dir,
            server.bucket(),
            "batch",
            &format!("unmanaged_{}.bin", i),
            &data,
        );
    }

    // LIST should show all 5 objects
    let list_url = format!(
        "{}/{}?list-type=2&prefix=batch/",
        server.endpoint(),
        server.bucket()
    );
    let resp = http.get(&list_url).send().await.unwrap();
    assert!(resp.status().is_success());
    let body = resp.text().await.unwrap();

    // Count <Key> elements
    let key_count = body.matches("<Key>").count();
    assert_eq!(
        key_count, 5,
        "Expected 5 objects in listing (3 managed + 2 unmanaged), got {}: {}",
        key_count, body
    );
}

#[tokio::test]
async fn test_get_nonexistent_still_returns_404() {
    let server = TestServer::filesystem().await;
    let client = reqwest::Client::new();

    let url = format!(
        "{}/{}/nonexistent/file.txt",
        server.endpoint(),
        server.bucket()
    );
    let resp = client.get(&url).send().await.unwrap();

    assert_eq!(
        resp.status().as_u16(),
        404,
        "GET on truly nonexistent object should return 404, got {}",
        resp.status()
    );
}
