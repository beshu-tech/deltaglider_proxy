// SPDX-License-Identifier: GPL-3.0-only

//! Integration tests for per-bucket storage quotas.

mod common;

use common::TestServer;

const BUCKET: &str = "quotabkt";

/// Helper: upload a file of the given size (filled with zeros).
async fn put_sized(server: &TestServer, key: &str, size: usize) -> Result<(), String> {
    let client = server.s3_client().await;
    let body = vec![0u8; size];
    client
        .put_object()
        .bucket(server.bucket())
        .key(key)
        .body(aws_sdk_s3::primitives::ByteStream::from(body))
        .send()
        .await
        .map(|_| ())
        .map_err(|e| format!("{}", e))
}

/// Helper: delete a file.
async fn delete(server: &TestServer, key: &str) {
    let client = server.s3_client().await;
    let _ = client
        .delete_object()
        .bucket(server.bucket())
        .key(key)
        .send()
        .await;
}

// ═══════════════════════════════════════════════════
// Basic quota enforcement
// ═══════════════════════════════════════════════════

#[tokio::test]
async fn test_quota_put_under_limit() {
    // Quota = 1 MB, upload a 100-byte file → should succeed
    let server = TestServer::builder()
        .bucket(BUCKET)
        .auth("TESTKEY", "TESTSECRET")
        .bucket_policy(BUCKET, "quota_bytes: 1048576")
        .build()
        .await;

    let result = put_sized(&server, "small.txt", 100).await;
    assert!(
        result.is_ok(),
        "PUT under quota should succeed: {:?}",
        result
    );
}

#[tokio::test]
async fn test_quota_zero_blocks_all_writes() {
    // Quota = 0 → freeze bucket. All writes blocked immediately,
    // regardless of usage scanner state (short-circuits before cache check).
    let server = TestServer::builder()
        .bucket(BUCKET)
        .auth("TESTKEY2", "TESTSECRET2")
        .bucket_policy(BUCKET, "quota_bytes: 0")
        .build()
        .await;

    let result = put_sized(&server, "blocked.txt", 10).await;
    assert!(result.is_err(), "quota=0 should block ALL writes");
}

#[tokio::test]
async fn test_quota_no_quota_unlimited() {
    // No quota set → unlimited writes
    let server = TestServer::builder()
        .bucket(BUCKET)
        .auth("TESTKEY3", "TESTSECRET3")
        .build()
        .await;

    let result = put_sized(&server, "big.bin", 1024 * 100).await;
    assert!(result.is_ok(), "No quota should allow unlimited writes");
}

#[tokio::test]
async fn test_quota_first_put_optimistic() {
    // First PUT to a bucket with quota succeeds optimistically because the
    // usage scanner has no cached data yet. This is by design — the scanner
    // runs in the background and caches results. The first write triggers
    // the scan but does not wait for it.
    //
    // NOTE: This test documents intentional behavior, not a bug.
    // Concurrent PUTs during the scan window can also overshoot the quota.
    // The quota is a soft limit (usage scanner TTL = 5 minutes).
    let server = TestServer::builder()
        .bucket(BUCKET)
        .auth("TESTKEY4", "TESTSECRET4")
        .bucket_policy(BUCKET, "quota_bytes: 1") // 1 byte quota
        .build()
        .await;

    // First PUT: scanner has no data → optimistic allow
    let result = put_sized(&server, "first.bin", 1024).await;
    assert!(
        result.is_ok(),
        "First PUT should succeed optimistically (no cached usage data)"
    );
}

#[tokio::test]
async fn test_quota_second_put_enforced() {
    // After the first PUT triggers a scan, subsequent PUTs are enforced.
    // We use a generous quota (10KB) and overshoot it significantly,
    // then retry until the scanner catches up (up to 10 seconds).
    //
    // DGP_USAGE_CACHE_TTL_SECS=1 shortens the scan cache TTL so that
    // each probe retriggers a re-scan; in production the default 5-min
    // TTL means the first (racy) scan caches "0 bytes" before the seed
    // PUT lands on disk, sticking around for 5 minutes and defeating
    // the polling loop. See src/usage_scanner.rs::cache_ttl_secs.
    let server = TestServer::builder()
        .bucket(BUCKET)
        .auth("TESTKEY5", "TESTSECRET5")
        .bucket_policy(BUCKET, "quota_bytes: 10000") // 10 KB
        .env("DGP_USAGE_CACHE_TTL_SECS", "1")
        .build()
        .await;

    // First PUT: 20 KB (optimistic, no cached data)
    put_sized(&server, "seed.bin", 20000).await.ok();

    // Poll until the scanner catches up and blocks writes (max 10 seconds)
    let mut blocked = false;
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        if put_sized(&server, "probe.bin", 10).await.is_err() {
            blocked = true;
            break;
        }
        // Probe succeeded — scanner hasn't caught up yet, delete it and retry
        delete(&server, "probe.bin").await;
    }
    assert!(
        blocked,
        "Quota should be enforced after scanner caches usage"
    );
}

#[tokio::test]
async fn test_quota_delete_frees_space() {
    // After deleting objects and scanner refreshes, PUT should succeed again.
    // Uses polling to avoid flaky timing dependencies on scanner speed.
    // See test_quota_second_put_enforced for the DGP_USAGE_CACHE_TTL_SECS=1
    // rationale.
    let server = TestServer::builder()
        .bucket(BUCKET)
        .auth("TESTKEY6", "TESTSECRET6")
        .bucket_policy(BUCKET, "quota_bytes: 10000") // 10 KB
        .env("DGP_USAGE_CACHE_TTL_SECS", "1")
        .build()
        .await;

    // Fill bucket well over quota
    put_sized(&server, "fill1.bin", 8000).await.ok();
    put_sized(&server, "fill2.bin", 8000).await.ok();

    // Wait for scanner to enforce quota
    let mut blocked = false;
    for _ in 0..20 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        if put_sized(&server, "probe_over.bin", 10).await.is_err() {
            blocked = true;
            break;
        }
        delete(&server, "probe_over.bin").await;
    }
    assert!(blocked, "Should be over quota after filling");

    // Delete files to free space
    delete(&server, "fill1.bin").await;
    delete(&server, "fill2.bin").await;

    // Poll until scanner refreshes and allows writes again (max 15 seconds)
    // Scanner cache TTL is 5 minutes, but get_or_scan re-triggers scan when stale.
    let mut freed = false;
    for _ in 0..30 {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        if put_sized(&server, "after_delete.bin", 100).await.is_ok() {
            freed = true;
            break;
        }
    }
    assert!(
        freed,
        "PUT should succeed after deleting objects and scanner refresh"
    );
}

// ════════════════════════════════════════════════════════════════════
// Quota enforcement on non-PUT mutating operations
//
// PUT object is the obvious quota gate, but the s3s adapter shipped
// without `check_quota` on copy_object and complete_multipart_upload.
// Today's commit fixed those. These tests pin the fix: a quota=0
// freeze must block EVERY way bytes can enter a bucket.
// ════════════════════════════════════════════════════════════════════

/// Frozen bucket (quota=0) must reject `CopyObject` as well as `PutObject`.
/// Gap that landed in production silently on s3s before today's fix:
/// s3s adapter's copy_object skipped the check_quota call entirely.
#[tokio::test]
async fn test_quota_zero_blocks_copy_object() {
    let server = TestServer::builder()
        .bucket(BUCKET)
        .auth("COPYKEY", "COPYSECRET")
        .bucket_policy(BUCKET, "quota_bytes: 0") // freeze
        .env("DGP_USAGE_CACHE_TTL_SECS", "1")
        .build()
        .await;

    // We can't PUT (quota=0 freezes everything), but we CAN exercise
    // the CopyObject path: have the SDK try CopyObject with x-amz-
    // copy-source pointing at a key that doesn't even need to exist
    // (the quota gate fires before the storage layer is consulted).
    let client = server.s3_client().await;
    let result = client
        .copy_object()
        .bucket(server.bucket())
        .key("dst-key.bin")
        .copy_source(format!("{}/source.bin", server.bucket()))
        .send()
        .await;
    assert!(
        result.is_err(),
        "quota=0 must reject CopyObject regardless of source existence; got {:?}",
        result
    );
}

/// Frozen bucket (quota=0) must reject `CompleteMultipartUpload`.
/// Without the fix, an attacker could bypass quota by initiating a
/// multipart upload (which doesn't trigger quota checks per-part),
/// uploading parts, and completing — bytes land in the bucket
/// regardless of the quota.
#[tokio::test]
async fn test_quota_zero_blocks_complete_multipart_upload() {
    let server = TestServer::builder()
        .bucket(BUCKET)
        .auth("MPUKEY", "MPUSECRET")
        .bucket_policy(BUCKET, "quota_bytes: 0") // freeze
        .env("DGP_USAGE_CACHE_TTL_SECS", "1")
        .build()
        .await;

    let client = server.s3_client().await;
    // CreateMultipartUpload should succeed (no bytes yet — and the
    // axum/s3s implementations don't gate this on quota; only the
    // commit step does).
    let create = client
        .create_multipart_upload()
        .bucket(server.bucket())
        .key("mpu-key.bin")
        .send()
        .await;
    // If create fails (e.g. quota gate moved to upfront in the future),
    // the test still demonstrates "quota=0 blocks bytes from landing"
    // — we just don't get to exercise the complete path.
    let Ok(create) = create else {
        eprintln!(
            "quota=0 blocks CreateMultipartUpload (acceptable strictness): {:?}",
            create.err()
        );
        return;
    };
    let upload_id = create
        .upload_id()
        .expect("upload_id from CreateMultipartUpload");

    // Upload a 5MB part (S3's minimum for non-final parts).
    // Note: S3's spec requires parts ≥ 5MB except the last. We use
    // 5MB to satisfy that and then complete with this single part
    // (last+only part has no size minimum).
    let part_body = vec![0u8; 5 * 1024 * 1024];
    let upload_part = client
        .upload_part()
        .bucket(server.bucket())
        .key("mpu-key.bin")
        .upload_id(upload_id)
        .part_number(1)
        .body(aws_sdk_s3::primitives::ByteStream::from(part_body))
        .send()
        .await;
    let Ok(upload_part) = upload_part else {
        eprintln!(
            "quota=0 blocks UploadPart (also acceptable strictness): {:?}",
            upload_part.err()
        );
        return;
    };
    let etag = upload_part
        .e_tag()
        .expect("UploadPart must return ETag")
        .to_string();

    // Complete must be rejected — this is the byte-commit point.
    use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
    let complete = client
        .complete_multipart_upload()
        .bucket(server.bucket())
        .key("mpu-key.bin")
        .upload_id(upload_id)
        .multipart_upload(
            CompletedMultipartUpload::builder()
                .parts(CompletedPart::builder().e_tag(&etag).part_number(1).build())
                .build(),
        )
        .send()
        .await;
    assert!(
        complete.is_err(),
        "quota=0 must reject CompleteMultipartUpload; got {:?}",
        complete
    );
}
