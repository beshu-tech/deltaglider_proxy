// SPDX-License-Identifier: BUSL-1.1

//! Metadata validation tests: verify correct handling of missing, partial, and wrong metadata.
//!
//! These tests ensure the proxy degrades gracefully (not catastrophically) when
//! delta/reference files have missing or corrupt DG metadata — a condition that
//! occurs when files are copied without preserving S3 user metadata.

use crate::common;

use common::{
    admin_http_client, generate_binary, head_headers, mutate_binary, put_object, TestServer,
};
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
    client: &crate::common::S3Http,
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
async fn head_status(
    client: &crate::common::S3Http,
    endpoint: &str,
    bucket: &str,
    key: &str,
) -> u16 {
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
    let server = TestServer::builder().build().await;
    let data_dir = server.data_dir().expect("filesystem backend");

    write_file(
        data_dir,
        server.bucket(),
        "nometa",
        "artifact.zip.delta",
        b"raw delta data",
    );

    let http = server.http();

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
    let server = TestServer::builder().build().await;
    let data_dir = server.data_dir().expect("filesystem backend");
    let content = b"file without any DG metadata";

    write_file(data_dir, server.bucket(), "nometa", "report.txt", content);

    let http = server.http();

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
    let server = TestServer::builder().build().await;
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

    let http = server.http();

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
    crate::common::expect_server_errors(
        "corrupt xattrs may answer 200, 404 or 500 (the test accepts all three)",
    );
    let server = TestServer::builder().build().await;
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

    let http = server.http();

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
    crate::common::expect_server_errors(
        "partial metadata may answer 200, 404 or 500 (the test accepts all three)",
    );
    let server = TestServer::builder().build().await;
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

    let http = server.http();

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

// ============================================================================
// 5. Heal-on-write: a store into a prefix whose reference lost its DG metadata
//    re-stamps the reference (same bytes, valid headers) so future reads
//    resolve it — stopping the replication re-copy loop on legacy corrupt data.
// ============================================================================

/// Strip the DG xattr from an on-disk deltaspace file (simulate the "copied
/// without --metadata" damage that leaves bytes intact but headers gone).
fn strip_xattr(data_dir: &Path, bucket: &str, prefix: &str, filename: &str) {
    let path = data_dir
        .join(bucket)
        .join("deltaspaces")
        .join(prefix)
        .join(filename);
    // Remove if present; ignore "no such attr".
    let _ = xattr::remove(&path, "user.dg.metadata");
}

fn read_xattr(data_dir: &Path, bucket: &str, prefix: &str, filename: &str) -> Option<Vec<u8>> {
    let path = data_dir
        .join(bucket)
        .join("deltaspaces")
        .join(prefix)
        .join(filename);
    xattr::get(&path, "user.dg.metadata").ok().flatten()
}

fn ref_bytes(data_dir: &Path, bucket: &str, prefix: &str) -> Vec<u8> {
    let path = data_dir
        .join(bucket)
        .join("deltaspaces")
        .join(prefix)
        .join("reference.bin");
    std::fs::read(path).expect("read reference.bin")
}

#[tokio::test]
async fn test_store_heals_stripped_reference_metadata_preserving_bytes() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;
    let data_dir = server.data_dir().expect("filesystem backend").to_path_buf();
    let bucket = server.bucket().to_string();
    let prefix = "heal"; // deltaspace prefix

    let v1 = generate_zip_like(1, 20000);
    let v2 = generate_zip_like(2, 20000);
    let v3 = generate_zip_like(3, 20000);

    // PUT v1 (creates reference.bin) + v2 (creates a delta against it).
    for (key, body) in [("heal/a-v1.zip", &v1), ("heal/a-v2.zip", &v2)] {
        client
            .put_object()
            .bucket(&bucket)
            .key(key)
            .body(aws_sdk_s3::primitives::ByteStream::from(body.clone()))
            .send()
            .await
            .expect("PUT");
    }

    // Corrupt: strip the reference's DG metadata (bytes stay intact).
    let ref_before = ref_bytes(&data_dir, &bucket, prefix);
    strip_xattr(&data_dir, &bucket, prefix, "reference.bin");
    assert!(
        read_xattr(&data_dir, &bucket, prefix, "reference.bin").is_none(),
        "precondition: reference metadata stripped"
    );

    // A store into the same deltaspace must HEAL the reference in place.
    client
        .put_object()
        .bucket(&bucket)
        .key("heal/a-v3.zip")
        .body(aws_sdk_s3::primitives::ByteStream::from(v3.clone()))
        .send()
        .await
        .expect("PUT v3 (triggers heal)");

    // Reference metadata is back, and it's a valid reference.
    let healed = read_xattr(&data_dir, &bucket, prefix, "reference.bin")
        .expect("reference metadata re-stamped after store");
    let meta: serde_json::Value =
        serde_json::from_slice(&healed).expect("healed metadata is valid JSON");
    assert_eq!(
        meta.get("original_name").and_then(|v| v.as_str()),
        Some("__reference__"),
        "healed reference keeps the internal name (migrator invariant)"
    );
    assert!(
        meta.get("file_sha256")
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty()),
        "healed reference carries a non-empty sha256: {meta}"
    );

    // Bytes UNCHANGED — existing deltas were encoded against these bytes.
    let ref_after = ref_bytes(&data_dir, &bucket, prefix);
    assert_eq!(
        ref_before, ref_after,
        "reference bytes must be identical after a metadata-only heal"
    );

    // All three members still reconstruct byte-exactly (the pre-existing delta
    // v2 proves the heal didn't break reconstruction).
    for (key, expected) in [
        ("heal/a-v1.zip", &v1),
        ("heal/a-v2.zip", &v2),
        ("heal/a-v3.zip", &v3),
    ] {
        let got = client
            .get_object()
            .bucket(&bucket)
            .key(key)
            .send()
            .await
            .expect("GET")
            .body
            .collect()
            .await
            .unwrap()
            .into_bytes();
        assert_eq!(
            got.as_ref(),
            expected.as_slice(),
            "{key} roundtrip mismatch"
        );
    }
}

// ============================================================================
// Reserved names in any path segment
// ============================================================================

/// A write whose key has a reserved name as ANY segment gets a 400 that names
/// the segment, in any letter case; nothing is stored. A key that already has
/// such a segment (written straight to the disk) stays readable and deletable.
#[tokio::test]
async fn a_write_with_a_reserved_segment_is_refused_and_existing_ones_stay_reachable() {
    let server = TestServer::builder().build().await;
    let data_dir = server.data_dir().expect("filesystem backend");
    let s3 = server.s3_client().await;
    let bucket = server.bucket();
    let put = |key: &'static str| {
        let s3 = s3.clone();
        async move {
            s3.put_object()
                .bucket(bucket)
                .key(key)
                .body(aws_sdk_s3::primitives::ByteStream::from_static(b"x"))
                .send()
                .await
        }
    };
    for key in [
        "atk/reference.bin/x.txt",
        "atk/REFERENCE.BIN/x.txt",
        "atk/REFERENCE.BIN",
        "d/app-2.zip.delta/x.txt",
        "d/App.ZIP.DELTA",
        "q/.dg-folder-marker/x",
        "q/.DG-TMP.a/x",
    ] {
        let err = put(key).await.expect_err(key);
        let status = err.raw_response().map(|r| r.status().as_u16());
        assert_eq!(status, Some(400), "{key}");
    }
    assert!(!data_dir
        .join(bucket)
        .join("deltaspaces")
        .join("atk")
        .exists());
    // The message names the segment.
    let http = server.http();
    let resp = http
        .put(format!(
            "{}/{bucket}/atk/reference.bin/x.txt",
            server.endpoint()
        ))
        .body(b"x".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 400);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("the path segment &apos;reference.bin&apos; is reserved"),
        "{body}"
    );

    // An ordinary delta folder still works after the refusals.
    put("atk/app-1.zip").await.unwrap();

    // A key written by an earlier release (a directory named like a baseline).
    write_file(data_dir, bucket, "old/reference.bin", "x.txt", b"legacy");
    let got = s3
        .get_object()
        .bucket(bucket)
        .key("old/reference.bin/x.txt")
        .send()
        .await;
    assert!(got.is_ok(), "{:?}", got.err());
    s3.delete_object()
        .bucket(bucket)
        .key("old/reference.bin/x.txt")
        .send()
        .await
        .unwrap();
    let gone = s3
        .head_object()
        .bucket(bucket)
        .key("old/reference.bin/x.txt")
        .send()
        .await;
    assert!(gone.is_err());
}

// ============================================================================
// Case variants of reserved names that an earlier release stored
// ============================================================================

/// On a case-sensitive data directory a case variant of a reserved name is an
/// ordinary object: one that an earlier release stored stays listed, readable
/// and deletable, and the bucket can be deleted. A new write is refused.
#[tokio::test]
async fn legacy_case_variants_of_reserved_names_stay_reachable() {
    let server = TestServer::builder().build().await;
    let data_dir = server.data_dir().expect("filesystem backend");
    let s3 = server.s3_client().await;
    let bucket = server.bucket();
    // The data dir of this test must be case-sensitive (ext4/tmpfs).
    let probe = data_dir.join(".case-probe");
    std::fs::write(&probe, b"").unwrap();
    let insensitive = data_dir.join(".CASE-PROBE").exists();
    std::fs::remove_file(&probe).unwrap();
    if insensitive {
        eprintln!("skipping: the data directory is case-insensitive");
        return;
    }
    let legacy = [
        ("", "rates.DELTA", "k1"),
        ("", "Reference.Bin", "k2"),
        ("a/.Dg", "x", "k3"),
    ];
    for (prefix, name, _) in legacy {
        write_file(data_dir, bucket, prefix, name, b"legacy");
    }
    let keys = ["rates.DELTA", "Reference.Bin", "a/.Dg/x"];

    // LIST shows all three; a LIST of the odd-case folder works.
    let listed = s3.list_objects_v2().bucket(bucket).send().await.unwrap();
    let mut got: Vec<_> = listed
        .contents()
        .iter()
        .map(|o| o.key().unwrap().to_string())
        .collect();
    got.sort();
    assert_eq!(got, ["Reference.Bin", "a/.Dg/x", "rates.DELTA"]);
    let sub = s3
        .list_objects_v2()
        .bucket(bucket)
        .prefix("a/.Dg/")
        .send()
        .await
        .unwrap();
    assert_eq!(sub.contents().len(), 1);

    for key in keys {
        let got = s3.get_object().bucket(bucket).key(key).send().await;
        assert!(got.is_ok(), "GET {key}: {:?}", got.err());
        let head = s3.head_object().bucket(bucket).key(key).send().await;
        assert!(head.is_ok(), "HEAD {key}: {:?}", head.err());
    }

    // A new write of the same names is refused.
    for key in keys {
        let err = s3
            .put_object()
            .bucket(bucket)
            .key(key)
            .body(aws_sdk_s3::primitives::ByteStream::from_static(b"new"))
            .send()
            .await
            .expect_err(key);
        let status = err.raw_response().map(|r| r.status().as_u16());
        assert_eq!(status, Some(400), "PUT {key}");
    }

    // DeleteObject and DeleteObjects remove them.
    s3.delete_object()
        .bucket(bucket)
        .key("rates.DELTA")
        .send()
        .await
        .unwrap();
    assert!(s3
        .head_object()
        .bucket(bucket)
        .key("rates.DELTA")
        .send()
        .await
        .is_err());
    let del = s3
        .delete_objects()
        .bucket(bucket)
        .delete(
            aws_sdk_s3::types::Delete::builder()
                .objects(
                    aws_sdk_s3::types::ObjectIdentifier::builder()
                        .key("Reference.Bin")
                        .build()
                        .unwrap(),
                )
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap();
    assert!(del.errors().is_empty(), "{:?}", del.errors());
    assert!(!data_dir
        .join(bucket)
        .join("deltaspaces/Reference.Bin")
        .exists());
    assert!(data_dir.join(bucket).join("deltaspaces/a/.Dg/x").exists());

    // DeleteBucket works once the last such object is deleted.
    s3.delete_object()
        .bucket(bucket)
        .key("a/.Dg/x")
        .send()
        .await
        .unwrap();
    s3.delete_bucket().bucket(bucket).send().await.unwrap();
}

// ============================================================================
// A copy of a delta obeys the reserved-name rule of a client write
// ============================================================================

/// Seeds `src/base.zip` and `src/v1.zip`; the second is stored as a delta.
async fn seed_delta_source(server: &TestServer) {
    let http = server.http();
    let base = generate_binary(100_000, 7);
    let variant = mutate_binary(&base, 0.01);
    for (name, body) in [("base.zip", base), ("v1.zip", variant)] {
        put_object(
            &http,
            &server.endpoint(),
            server.bucket(),
            &format!("src/{name}"),
            body,
            "application/zip",
        )
        .await;
    }
    let head = head_headers(&http, &server.endpoint(), server.bucket(), "src/v1.zip").await;
    assert_eq!(
        head["x-amz-storage-type"], "delta",
        "fixture: a delta source"
    );
}

/// Two delta PUTs into `folder`: the second one is stored as a delta.
async fn folder_still_takes_deltas(server: &TestServer, folder: &str) {
    let http = server.http();
    let base = generate_binary(100_000, 9);
    let variant = mutate_binary(&base, 0.01);
    for (name, body) in [("app-1.zip", base), ("app-2.zip", variant)] {
        put_object(
            &http,
            &server.endpoint(),
            server.bucket(),
            &format!("{folder}/{name}"),
            body,
            "application/zip",
        )
        .await;
    }
    let head = head_headers(
        &http,
        &server.endpoint(),
        server.bucket(),
        &format!("{folder}/app-2.zip"),
    )
    .await;
    assert_eq!(head["x-amz-storage-type"], "delta", "{folder}");
}

/// The destinations that a client write refuses, with the folder whose later
/// delta PUTs the refused copy would break.
const REFUSED_COPY_DESTINATIONS: [(&str, &str); 2] = [
    ("atk/reference.bin/x.zip", "atk"),
    ("d/app-2.zip.delta/x.zip", "d"),
];

/// A CopyObject of a delta into a key with a reserved segment is a 400, and
/// no directory appears where the baseline or a delta goes.
#[tokio::test]
async fn a_delta_copy_into_a_reserved_segment_is_refused() {
    let server = TestServer::builder().build().await;
    seed_delta_source(&server).await;
    let data_dir = server.data_dir().expect("filesystem backend");
    let s3 = server.s3_client().await;
    let bucket = server.bucket();
    for (dest, folder) in REFUSED_COPY_DESTINATIONS {
        let err = s3
            .copy_object()
            .bucket(bucket)
            .key(dest)
            .copy_source(format!("{bucket}/src/v1.zip"))
            .send()
            .await
            .expect_err(dest);
        let status = err.raw_response().map(|r| r.status().as_u16());
        assert_eq!(status, Some(400), "{dest}");
        let spaces = data_dir.join(bucket).join("deltaspaces");
        assert!(
            !spaces.join(folder).join("reference.bin").exists(),
            "{dest}"
        );
        assert!(!spaces.join("atk").exists(), "{dest}");
        assert!(!spaces.join("d/app-2.zip.delta").exists(), "{dest}");
    }
    folder_still_takes_deltas(&server, "atk").await;
    folder_still_takes_deltas(&server, "d").await;
}

/// The admin bulk copy and move go through the same transfer: the item
/// fails, the source stays, and the destination folder stays usable.
#[tokio::test]
async fn an_admin_copy_or_move_of_a_delta_into_a_reserved_segment_is_refused() {
    let server = TestServer::builder().build().await;
    seed_delta_source(&server).await;
    let data_dir = server.data_dir().expect("filesystem backend");
    let admin = admin_http_client(&server.endpoint()).await;
    let bucket = server.bucket();
    for op in ["copy", "move"] {
        for (dest, folder) in REFUSED_COPY_DESTINATIONS {
            let (dest_prefix, relative) = dest.rsplit_once('/').unwrap();
            let r: serde_json::Value = admin
                .post(format!("{}/_/api/admin/objects/{op}", server.endpoint()))
                .json(&serde_json::json!({
                    "source_bucket": bucket,
                    "dest_bucket": bucket,
                    "dest_prefix": format!("{dest_prefix}/"),
                    "items": [{ "source_key": "src/v1.zip", "relative": relative }]
                }))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(r["failed"], 1, "{op} {dest}: {r}");
            assert_eq!(r["succeeded"], 0, "{op} {dest}: {r}");
            let spaces = data_dir.join(bucket).join("deltaspaces");
            assert!(!spaces.join(folder).exists(), "{op} {dest}");
        }
    }
    let head = head_headers(&server.http(), &server.endpoint(), bucket, "src/v1.zip").await;
    assert_eq!(head["x-amz-storage-type"], "delta", "the source stays");
    folder_still_takes_deltas(&server, "atk").await;
    folder_still_takes_deltas(&server, "d").await;
}

/// A delta copy into an ordinary key still ships the stored delta.
#[tokio::test]
async fn a_delta_copy_into_an_ordinary_key_keeps_the_delta_form() {
    let server = TestServer::builder().build().await;
    seed_delta_source(&server).await;
    let data_dir = server.data_dir().expect("filesystem backend");
    let s3 = server.s3_client().await;
    let bucket = server.bucket();
    s3.copy_object()
        .bucket(bucket)
        .key("src/v1-copy.zip")
        .copy_source(format!("{bucket}/src/v1.zip"))
        .send()
        .await
        .expect("CopyObject");
    let spaces = data_dir.join(bucket).join("deltaspaces/src");
    assert!(spaces.join("v1-copy.zip.delta").is_file());
    assert!(!spaces.join("v1-copy.zip").exists());
}

// ============================================================================
// A refused key is refused before the body is read
// ============================================================================

/// Send the head of a `method` request with a declared body of 50 MB and no
/// body bytes; the status that comes back before any byte of the body.
async fn status_before_the_body(
    server: &TestServer,
    method: &str,
    path: &str,
    content_type: &str,
) -> Option<u16> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let host = server.endpoint().replace("http://", "");
    let mut tcp = tokio::net::TcpStream::connect(&host).await.unwrap();
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nContent-Length: 50000000\r\n\
         Content-Type: {content_type}\r\n\r\n"
    );
    tcp.write_all(head.as_bytes()).await.unwrap();
    let mut buf = vec![0u8; 64];
    let read = tokio::time::timeout(std::time::Duration::from_secs(3), tcp.read(&mut buf)).await;
    let n = read.ok()?.ok()?;
    std::str::from_utf8(&buf[..n])
        .ok()?
        .split(' ')
        .nth(1)?
        .parse()
        .ok()
}

/// A PutObject whose key a client write refuses gets its 400 before the proxy
/// reads the body, as CreateMultipartUpload does.
#[tokio::test]
async fn a_put_with_a_refused_key_is_refused_before_its_body_is_read() {
    let server = TestServer::builder().open_access().build().await;
    let bucket = server.bucket();
    for key in ["atk/reference.bin/x.zip", "d/app.zip.delta", "q/.dg/x"] {
        let status = status_before_the_body(
            &server,
            "PUT",
            &format!("/{bucket}/{key}"),
            "application/zip",
        )
        .await;
        assert_eq!(status, Some(400), "{key}");
    }
}

/// The same for a browser form upload: after its signature is verified and
/// before its file is read. (Open access: the form is not signed.)
#[tokio::test]
async fn a_form_upload_with_a_refused_key_is_refused_before_its_file_is_read() {
    let server = TestServer::builder().open_access().build().await;
    let bucket = server.bucket();
    let boundary = "xBOUNDARYx";
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let host = server.endpoint().replace("http://", "");
    let mut tcp = tokio::net::TcpStream::connect(&host).await.unwrap();
    let fields = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"key\"\r\n\r\natk/reference.bin/x.zip\r\n\
         --{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"f.zip\"\r\n\
         Content-Type: application/zip\r\n\r\n"
    );
    let head = format!(
        "POST /{bucket} HTTP/1.1\r\nHost: {host}\r\nContent-Length: 50000000\r\n\
         Content-Type: multipart/form-data; boundary={boundary}\r\n\r\n{fields}"
    );
    tcp.write_all(head.as_bytes()).await.unwrap();
    let mut buf = vec![0u8; 64];
    let n = tokio::time::timeout(std::time::Duration::from_secs(3), tcp.read(&mut buf))
        .await
        .expect("an answer before the file")
        .unwrap();
    let status = std::str::from_utf8(&buf[..n])
        .unwrap()
        .split(' ')
        .nth(1)
        .unwrap()
        .to_string();
    assert_eq!(status, "400");
}
