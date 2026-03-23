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
    let dir = data_dir.join(bucket).join("deltaspaces").join(prefix);
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
    assert_eq!(
        body.as_ref(),
        content,
        "GET body should match written content"
    );
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
    assert!(
        resp.status().is_success(),
        "GET before DELETE should succeed"
    );

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

/// Regression test for C1/C2: HEAD and GET must return consistent metadata
/// for unmanaged objects. Previously GET used ad-hoc metadata (file_size=0,
/// empty md5) while HEAD used get_passthrough_metadata with correct values.
#[tokio::test]
async fn test_head_and_get_metadata_consistency() {
    let server = TestServer::filesystem().await;
    let data_dir = server.data_dir().expect("filesystem backend has data_dir");
    let content = b"consistency check content with known size";

    write_unmanaged_file(data_dir, server.bucket(), "meta", "consistent.bin", content);

    let client = reqwest::Client::new();
    let url = format!(
        "{}/{}/meta/consistent.bin",
        server.endpoint(),
        server.bucket()
    );

    // HEAD should return correct content-length
    let head_resp = client.head(&url).send().await.unwrap();
    assert_eq!(head_resp.status().as_u16(), 200);
    let head_content_length = head_resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    assert_eq!(
        head_content_length,
        Some(content.len() as u64),
        "HEAD should return correct content-length for unmanaged objects"
    );

    // GET should return same content-length and correct body
    let get_resp = client.get(&url).send().await.unwrap();
    assert_eq!(get_resp.status().as_u16(), 200);
    let get_content_length = get_resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    // Both HEAD and GET should agree on content-length
    assert_eq!(
        get_content_length, head_content_length,
        "HEAD and GET should return the same content-length"
    );
    let body = get_resp.bytes().await.unwrap();
    assert_eq!(body.as_ref(), content);
}

/// Regression test for M6: reserved filenames must be rejected by the proxy.
/// Users must not be able to PUT objects that collide with DeltaGlider internal
/// storage files (reference.bin, *.delta).
#[tokio::test]
async fn test_put_reserved_filename_reference_bin_rejected() {
    let server = TestServer::filesystem().await;
    let client = reqwest::Client::new();

    let url = format!(
        "{}/{}/some-prefix/reference.bin",
        server.endpoint(),
        server.bucket()
    );
    let resp = client
        .put(&url)
        .header("content-type", "application/octet-stream")
        .body(b"should be rejected".to_vec())
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        400,
        "PUT reference.bin should be rejected with 400, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn test_put_reserved_filename_dot_delta_rejected() {
    let server = TestServer::filesystem().await;
    let client = reqwest::Client::new();

    let url = format!(
        "{}/{}/some-prefix/file.zip.delta",
        server.endpoint(),
        server.bucket()
    );
    let resp = client
        .put(&url)
        .header("content-type", "application/octet-stream")
        .body(b"should be rejected".to_vec())
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        400,
        "PUT *.delta should be rejected with 400, got {}",
        resp.status()
    );
}

/// Regression test for M5: copy_object must enforce size limits even when
/// source metadata reports file_size=0 (fallback metadata for unmanaged objects).
#[tokio::test]
async fn test_copy_unmanaged_object_succeeds() {
    let server = TestServer::filesystem().await;
    let data_dir = server.data_dir().expect("filesystem backend has data_dir");
    let content = b"copy me from unmanaged source";

    write_unmanaged_file(
        data_dir,
        server.bucket(),
        "copy-src",
        "original.bin",
        content,
    );

    let client = reqwest::Client::new();
    let dest_url = format!(
        "{}/{}/copy-dest/copied.bin",
        server.endpoint(),
        server.bucket()
    );
    let source = format!("/{}/copy-src/original.bin", server.bucket());
    let resp = client
        .put(&dest_url)
        .header("x-amz-copy-source", &source)
        .send()
        .await
        .unwrap();

    assert!(
        resp.status().is_success(),
        "Copy of unmanaged object should succeed, got {}",
        resp.status()
    );

    // Verify the copy has correct content
    let get_resp = client.get(&dest_url).send().await.unwrap();
    assert!(get_resp.status().is_success());
    let body = get_resp.bytes().await.unwrap();
    assert_eq!(
        body.as_ref(),
        content,
        "Copied content should match original"
    );
}

/// Regression test for M5: copy_object with unmanaged source that exceeds
/// max_object_size should be rejected after actual size check.
#[tokio::test]
async fn test_copy_unmanaged_object_too_large_rejected() {
    let server = TestServer::filesystem_with_max_object_size(100).await;
    let data_dir = server.data_dir().expect("filesystem backend has data_dir");

    // Write a file larger than max_object_size directly (bypassing proxy)
    let large_content = vec![0xBB; 200];
    write_unmanaged_file(
        data_dir,
        server.bucket(),
        "copy-big",
        "large.bin",
        &large_content,
    );

    let client = reqwest::Client::new();
    let dest_url = format!(
        "{}/{}/copy-big/dest.bin",
        server.endpoint(),
        server.bucket()
    );
    let source = format!("/{}/copy-big/large.bin", server.bucket());
    let resp = client
        .put(&dest_url)
        .header("x-amz-copy-source", &source)
        .send()
        .await
        .unwrap();

    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    assert!(
        status == 400 || status == 413,
        "Copy of oversized unmanaged object should be rejected (400 or 413), got {}: {}",
        status,
        body
    );
}

/// Verify that a file written directly (no xattr) and then deleted via the
/// proxy's filesystem returns proper 404 on subsequent access — not 500.
/// Regression for error discrimination: ENOENT must become 404, not 500.
#[tokio::test]
async fn test_deleted_unmanaged_object_returns_404_not_500() {
    let server = TestServer::filesystem().await;
    let data_dir = server.data_dir().expect("filesystem backend has data_dir");

    write_unmanaged_file(data_dir, server.bucket(), "ephemeral", "temp.bin", b"temp");

    let client = reqwest::Client::new();
    let url = format!(
        "{}/{}/ephemeral/temp.bin",
        server.endpoint(),
        server.bucket()
    );

    // Confirm it exists
    let resp = client.head(&url).send().await.unwrap();
    assert_eq!(resp.status().as_u16(), 200);

    // Delete the file directly from the filesystem (simulates external deletion)
    let file_path = data_dir
        .join(server.bucket())
        .join("deltaspaces")
        .join("ephemeral")
        .join("temp.bin");
    std::fs::remove_file(&file_path).expect("Failed to delete file");

    // GET should return 404, not 500
    let resp = client.get(&url).send().await.unwrap();
    assert_eq!(
        resp.status().as_u16(),
        404,
        "Deleted file should return 404, not 500 (error discrimination), got {}",
        resp.status()
    );

    // HEAD should also return 404, not 500
    let resp = client.head(&url).send().await.unwrap();
    assert_eq!(
        resp.status().as_u16(),
        404,
        "HEAD on deleted file should return 404, not 500, got {}",
        resp.status()
    );
}

/// Verify that user metadata (x-amz-meta-*) survives a PUT→GET round-trip.
/// This tests the full metadata preservation path through the engine.
#[tokio::test]
async fn test_user_metadata_round_trip() {
    let server = TestServer::filesystem().await;
    let client = reqwest::Client::new();
    let url = format!(
        "{}/{}/meta-test/doc.txt",
        server.endpoint(),
        server.bucket()
    );

    // PUT with custom metadata
    let resp = client
        .put(&url)
        .header("content-type", "text/plain")
        .header("x-amz-meta-author", "test-user")
        .header("x-amz-meta-version", "42")
        .body(b"hello metadata".to_vec())
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());

    // GET and verify metadata headers
    let resp = client.get(&url).send().await.unwrap();
    assert!(resp.status().is_success());

    let author = resp
        .headers()
        .get("x-amz-meta-author")
        .and_then(|v| v.to_str().ok());
    assert_eq!(
        author,
        Some("test-user"),
        "x-amz-meta-author should survive round-trip"
    );

    let version = resp
        .headers()
        .get("x-amz-meta-version")
        .and_then(|v| v.to_str().ok());
    assert_eq!(
        version,
        Some("42"),
        "x-amz-meta-version should survive round-trip"
    );

    // Verify content-type is preserved
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ct.contains("text/plain"),
        "Content-Type should be preserved, got: {}",
        ct
    );
}

/// Verify delta round-trip: PUT two similar files, confirm both GET back
/// the exact original bytes (not corrupted by delta encode/decode).
#[tokio::test]
async fn test_delta_round_trip_byte_integrity() {
    let server = TestServer::filesystem().await;
    let client = reqwest::Client::new();

    // Generate two similar binary files (delta-eligible: .zip extension)
    let base_data: Vec<u8> = (0..10000).map(|i| (i % 256) as u8).collect();
    let mut variant_data = base_data.clone();
    // Mutate ~1% of bytes
    for i in (0..variant_data.len()).step_by(100) {
        variant_data[i] = variant_data[i].wrapping_add(1);
    }

    // PUT base
    let url1 = format!(
        "{}/{}/integrity/base.zip",
        server.endpoint(),
        server.bucket()
    );
    let resp = client
        .put(&url1)
        .header("content-type", "application/zip")
        .body(base_data.clone())
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "PUT base failed: {}",
        resp.status()
    );

    // PUT variant (should be stored as delta)
    let url2 = format!(
        "{}/{}/integrity/variant.zip",
        server.endpoint(),
        server.bucket()
    );
    let resp = client
        .put(&url2)
        .header("content-type", "application/zip")
        .body(variant_data.clone())
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "PUT variant failed: {}",
        resp.status()
    );

    // GET base and verify exact bytes
    let resp = client.get(&url1).send().await.unwrap();
    assert!(resp.status().is_success());
    let got_base = resp.bytes().await.unwrap();
    assert_eq!(
        got_base.as_ref(),
        base_data.as_slice(),
        "Base file round-trip: GET bytes must match PUT bytes exactly"
    );

    // GET variant and verify exact bytes
    let resp = client.get(&url2).send().await.unwrap();
    assert!(resp.status().is_success());
    let got_variant = resp.bytes().await.unwrap();
    assert_eq!(
        got_variant.as_ref(),
        variant_data.as_slice(),
        "Variant file round-trip: GET bytes must match PUT bytes exactly"
    );
}

/// Verify multipart upload completion returns proper S3 ETag format (MD5-N).
#[tokio::test]
async fn test_multipart_etag_format() {
    let server = TestServer::filesystem().await;
    let s3 = server.s3_client().await;

    // Create multipart upload
    let create = s3
        .create_multipart_upload()
        .bucket(server.bucket())
        .key("multi/test.bin")
        .send()
        .await
        .unwrap();
    let upload_id = create.upload_id().unwrap();

    // Upload 2 parts
    let part1 = s3
        .upload_part()
        .bucket(server.bucket())
        .key("multi/test.bin")
        .upload_id(upload_id)
        .part_number(1)
        .body(aws_sdk_s3::primitives::ByteStream::from(vec![
            0xAA;
            5 * 1024
                * 1024
        ]))
        .send()
        .await
        .unwrap();

    let part2 = s3
        .upload_part()
        .bucket(server.bucket())
        .key("multi/test.bin")
        .upload_id(upload_id)
        .part_number(2)
        .body(aws_sdk_s3::primitives::ByteStream::from(vec![0xBB; 1024]))
        .send()
        .await
        .unwrap();

    // Complete
    use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
    let complete = s3
        .complete_multipart_upload()
        .bucket(server.bucket())
        .key("multi/test.bin")
        .upload_id(upload_id)
        .multipart_upload(
            CompletedMultipartUpload::builder()
                .parts(
                    CompletedPart::builder()
                        .part_number(1)
                        .e_tag(part1.e_tag().unwrap_or_default())
                        .build(),
                )
                .parts(
                    CompletedPart::builder()
                        .part_number(2)
                        .e_tag(part2.e_tag().unwrap_or_default())
                        .build(),
                )
                .build(),
        )
        .send()
        .await
        .unwrap();

    let etag = complete.e_tag().unwrap_or_default();
    // S3 multipart ETag format: "hex-N" where N is part count
    assert!(
        etag.contains("-2"),
        "Multipart ETag should contain part count suffix '-2', got: {}",
        etag
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
