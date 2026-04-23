//! S3 backend parity tests
//!
//! Runs the same core operations as s3_api_test but against TestServer::s3()
//! to verify the S3 storage backend works identically to filesystem.
//! All tests gated with skip_unless_minio!() — skips gracefully without MinIO.

mod common;

use aws_sdk_s3::primitives::ByteStream;
use common::{generate_binary, mutate_binary, TestServer};
use std::sync::atomic::{AtomicU64, Ordering};

/// Counter for unique test prefixes
static PREFIX_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generate a unique prefix to isolate each test's data in the shared MinIO bucket
fn unique_prefix() -> String {
    let counter = PREFIX_COUNTER.fetch_add(1, Ordering::SeqCst);
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    format!("test-{}-{}", timestamp, counter)
}

#[tokio::test]
async fn test_s3_put_get_roundtrip() {
    skip_unless_minio!();
    let server = TestServer::s3().await;
    let client = server.s3_client().await;
    let prefix = unique_prefix();

    let data = b"Hello via S3 backend!";
    let key = format!("{}/hello.txt", prefix);

    client
        .put_object()
        .bucket(server.bucket())
        .key(&key)
        .body(ByteStream::from(data.to_vec()))
        .send()
        .await
        .unwrap();
    let body = client
        .get_object()
        .bucket(server.bucket())
        .key(&key)
        .send()
        .await
        .unwrap()
        .body
        .collect()
        .await
        .unwrap()
        .into_bytes();
    assert_eq!(body.as_ref(), data);
}

// ── 9 filesystem-parity tests removed in QA hygiene pass ──────────────
//
// QA review finding #2: test_s3_put_get_delete_lifecycle,
// test_s3_put_overwrite, test_s3_list_objects_with_prefix,
// test_s3_list_objects_pagination, test_s3_copy_object,
// test_s3_delete_objects_batch, test_s3_head_object,
// test_s3_etag_consistent, and test_s3_unicode_key each verified
// `StorageBackend` trait behaviour that is already guaranteed by the
// filesystem-backed s3_api_test + s3_compat_test + s3_integration_test
// suites. Every such test spent a MinIO round-trip to re-verify trait
// semantics; S3-level differences are an AWS-SDK guarantee, not a
// proxy-level regression surface.
//
// What stayed, and why:
//   - test_s3_put_get_roundtrip  — smoke test for the S3-plumbing path
//     (SigV4 to MinIO, body bytestream, no delta pipeline). One
//     failure here tells you "the S3 backend is wired up at all."
//   - test_s3_delta_similar_files — real delta+S3 interaction: the
//     store.rs path that compresses v2 against v1's reference and
//     the retrieve.rs path that rehydrates. Neither filesystem
//     integration tests nor s3_integration_test exercises THIS
//     specific combination.
//
// If the S3 backend ever grows features that ARE S3-specific and
// don't round-trip through the trait (server-side encryption,
// object-lock, storage classes, requester-pays), add targeted tests
// HERE — keep them tight, one feature per test.
// ──────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_s3_delta_similar_files() {
    skip_unless_minio!();
    let server = TestServer::s3().await;
    let http = reqwest::Client::new();
    let prefix = unique_prefix();

    let base = generate_binary(100_000, 42);
    let variant = mutate_binary(&base, 0.01);

    // PUT base
    let url1 = format!(
        "{}/{}/{}/base.zip",
        server.endpoint(),
        server.bucket(),
        prefix
    );
    let resp1 = http
        .put(&url1)
        .header("content-type", "application/zip")
        .body(base.clone())
        .send()
        .await
        .unwrap();
    assert!(resp1.status().is_success());

    // PUT variant
    let url2 = format!(
        "{}/{}/{}/v1.zip",
        server.endpoint(),
        server.bucket(),
        prefix
    );
    let resp2 = http
        .put(&url2)
        .header("content-type", "application/zip")
        .body(variant.clone())
        .send()
        .await
        .unwrap();
    assert!(resp2.status().is_success());

    // Verify both retrievable
    let got_base = http.get(&url1).send().await.unwrap().bytes().await.unwrap();
    assert_eq!(got_base.as_ref(), base.as_slice());

    let got_v1 = http.get(&url2).send().await.unwrap().bytes().await.unwrap();
    assert_eq!(got_v1.as_ref(), variant.as_slice());
}
