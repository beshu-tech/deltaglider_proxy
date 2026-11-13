// SPDX-License-Identifier: BUSL-1.1

//! Integration tests for per-bucket storage quotas.

use crate::common;

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
        .map_err(|e| format!("{}", aws_sdk_s3::error::DisplayErrorContext(&e)))
}

/// Helper: delete a file.
async fn delete(server: &TestServer, key: &str) {
    let client = server.s3_client().await;
    client
        .delete_object()
        .bucket(server.bucket())
        .key(key)
        .send()
        .await
        .unwrap_or_else(|e| {
            panic!(
                "DELETE {key}: {}",
                aws_sdk_s3::error::DisplayErrorContext(&e)
            )
        });
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

    // First PUT: 20 KB, over the quota, but optimistic (no usage known
    // yet): its quota check, on the declared length before the body,
    // triggers a background scan via check_quota → get_or_scan (cache
    // empty → enqueue). That scan can finish while the body uploads; the
    // write stays optimistic all the same (`exact_quota_check_after_body`).
    put_sized(&server, "seed.bin", 20000)
        .await
        .expect("the first PUT to an unscanned bucket is optimistic");

    // Signal-driven poll: wait for a scan to complete (bounded, non-panicking),
    // then probe. Replaces the old blind sleep(500ms)×N loop while preserving
    // its retry-until-cache-reflects-reality structure (an early scan could
    // cache a pre-seed snapshot). See common::wait_usage_scan_refresh_bounded.
    let http = reqwest::Client::new();
    let endpoint = server.endpoint();
    let mut blocked = false;
    for _ in 0..20 {
        // Wait for the next scan to settle (signal-driven, bounded to 500ms so
        // the first iteration — no scan triggered yet — falls through like the
        // old sleep did). Fresh baseline per iteration: no cross-iter mutation.
        let baseline = common::get_usage_scan_version(&http, &endpoint).await;
        let _ = common::wait_usage_scan_refresh_bounded(
            &http,
            &endpoint,
            baseline,
            std::time::Duration::from_millis(500),
        )
        .await;
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

    // Go over quota with ONE object larger than the quota. The first PUT is
    // allowed optimistically (see test_quota_first_put_optimistic). Two
    // smaller fills are not reliable: once the usage cache knows the first,
    // the quota check rejects the second up front (403), which leaves the
    // bucket UNDER quota and the probe below is never blocked.
    let http = reqwest::Client::new();
    let endpoint = server.endpoint();
    put_sized(&server, "fill1.bin", 12000)
        .await
        .expect("first PUT is allowed optimistically");

    // Signal-driven poll for the scanner to catch up and enforce quota.
    let mut blocked = false;
    for _ in 0..20 {
        let baseline = common::get_usage_scan_version(&http, &endpoint).await;
        let _ = common::wait_usage_scan_refresh_bounded(
            &http,
            &endpoint,
            baseline,
            std::time::Duration::from_millis(500),
        )
        .await;
        if put_sized(&server, "probe_over.bin", 10).await.is_err() {
            blocked = true;
            break;
        }
        delete(&server, "probe_over.bin").await;
    }
    assert!(blocked, "Should be over quota after filling");

    // Delete files to free space
    delete(&server, "fill1.bin").await;

    // Signal-driven poll until scanner refreshes and allows writes again.
    // Scanner cache TTL is 5 minutes, but get_or_scan re-triggers scan when stale.
    let mut freed = false;
    let mut last_err = String::new();
    for _ in 0..30 {
        let baseline = common::get_usage_scan_version(&http, &endpoint).await;
        let _ = common::wait_usage_scan_refresh_bounded(
            &http,
            &endpoint,
            baseline,
            std::time::Duration::from_millis(500),
        )
        .await;
        match put_sized(&server, "after_delete.bin", 100).await {
            Ok(()) => {
                freed = true;
                break;
            }
            Err(e) => last_err = e,
        }
    }
    assert!(
        freed,
        "PUT should succeed after deleting objects and scanner refresh; last error: {last_err}"
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

/// The S3 error code of a failed SDK call.
fn error_code<E, R>(err: &aws_sdk_s3::error::SdkError<E, R>) -> String
where
    E: aws_sdk_s3::error::ProvideErrorMetadata,
    R: std::fmt::Debug,
{
    use aws_sdk_s3::error::ProvideErrorMetadata;
    err.code().unwrap_or("<no code>").to_string()
}

/// A frozen bucket (quota=0) refuses `CopyObject` with `AccessDenied`, for
/// every copy path: a passthrough source (server-side copy), a delta source,
/// and a passthrough source copied to a delta-eligible key (the streaming
/// store, above the 1 KiB spool threshold set below). The sources EXIST, in an unfrozen bucket: a missing source
/// answers `NoSuchKey` before the quota gate, so such a test passes when the
/// quota call is deleted. Each copy site calls its own gate; the table
/// names them so a removed call turns one row red.
#[tokio::test]
async fn test_quota_zero_blocks_copy_object() {
    let server = TestServer::builder()
        .bucket(BUCKET)
        .auth("COPYKEY", "COPYSECRET")
        .bucket_policy(BUCKET, "quota_bytes: 0") // freeze
        .env("DGP_USAGE_CACHE_TTL_SECS", "1")
        // A 4 KiB object is already "large": the copy takes the spool path.
        .env("DGP_SPOOL_THRESHOLD_BYTES", "1024")
        .build()
        .await;
    let client = server.s3_client().await;
    client
        .create_bucket()
        .bucket("srcbkt")
        .send()
        .await
        .unwrap();

    // (source key, destination key, body). A passthrough source copied to a
    // delta-eligible destination key takes the spooled store (the copy
    // re-encodes it), the third gate.
    let body_zip: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
    let sources: [(&str, &str, Vec<u8>); 3] = [
        ("photo.jpg", "copy-of-photo.jpg", vec![7u8; 4096]),
        ("build-1.zip", "copy-of-build-1.zip", body_zip),
        ("other.jpg", "copy-of-other.zip", vec![9u8; 4096]),
    ];
    for (key, _, body) in &sources {
        client
            .put_object()
            .bucket("srcbkt")
            .key(*key)
            .body(aws_sdk_s3::primitives::ByteStream::from(body.clone()))
            .send()
            .await
            .unwrap_or_else(|e| panic!("source {key} must store: {e:?}"));
    }
    for (key, dst, _) in &sources {
        let err = client
            .copy_object()
            .bucket(server.bucket())
            .key(*dst)
            .copy_source(format!("srcbkt/{key}"))
            .send()
            .await
            .expect_err("quota=0 must reject CopyObject");
        assert_eq!(error_code(&err), "AccessDenied", "{key} -> {dst}: {err:?}");
        let head = client
            .head_object()
            .bucket(server.bucket())
            .key(*dst)
            .send()
            .await;
        assert!(
            head.is_err(),
            "{key} -> {dst}: the refused copy wrote nothing"
        );
    }
}

/// The bodies the proxy read into the spool so far (one scrape).
async fn spooled_bodies(server: &TestServer) -> u64 {
    common::metrics_text(&server.endpoint())
        .await
        .lines()
        .filter(|l| l.starts_with("deltaglider_ingest_bodies_total{medium=\"spool\""))
        .filter_map(|l| l.rsplit_once(' ')?.1.trim().parse::<f64>().ok())
        .sum::<f64>() as u64
}

/// PUT 4 KiB of `big.zip` into the server's bucket; `Err`: the S3 error code.
async fn put_4k(server: &TestServer) -> Result<(), String> {
    server
        .s3_client()
        .await
        .put_object()
        .bucket(server.bucket())
        .key("big.zip")
        .body(aws_sdk_s3::primitives::ByteStream::from(vec![3u8; 4096]))
        .send()
        .await
        .map(|_| ())
        .map_err(|e| error_code(&e))
}

/// A frozen bucket (quota=0) refuses a PutObject above the spool threshold
/// with `AccessDenied` BEFORE its body is read: the gate before the body
/// (`ingest_client_body`) judges the declared length, so the 4 KiB body
/// never reaches the spool. The exact gate after the body
/// (`store_client_write`) would refuse it too, but only after the client
/// sent every byte, so the answer alone cannot tell the two gates apart;
/// the ingest counter can. An untouched counter emits no series, so the
/// zero would hold for a renamed metric too: the control, the same PUT into
/// a bucket without a quota, must count exactly one spooled body.
#[tokio::test]
async fn test_quota_zero_refuses_a_spooled_put_before_its_body() {
    let control = TestServer::builder()
        .bucket(BUCKET)
        .env("DGP_SPOOL_THRESHOLD_BYTES", "1024")
        .build()
        .await;
    put_4k(&control).await.expect("the control PUT must store");
    assert_eq!(
        spooled_bodies(&control).await,
        1,
        "the control: the 4 KiB body is read into the spool (the series exists)"
    );

    let server = TestServer::builder()
        .bucket(BUCKET)
        .bucket_policy(BUCKET, "quota_bytes: 0")
        .env("DGP_SPOOL_THRESHOLD_BYTES", "1024")
        .build()
        .await;
    let err = put_4k(&server)
        .await
        .expect_err("quota=0 must reject the PutObject");
    assert_eq!(err, "AccessDenied");
    assert_eq!(
        spooled_bodies(&server).await,
        0,
        "the refused body was read into the spool"
    );
    let client = server.s3_client().await;
    assert!(
        client
            .head_object()
            .bucket(server.bucket())
            .key("big.zip")
            .send()
            .await
            .is_err(),
        "the refused PUT wrote nothing"
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

// ═══════════════════════════════════════════════════
// Quota counts the stored footprint INCLUDING delta baselines
// ═══════════════════════════════════════════════════

/// One build per folder is the common release layout: every folder is its own
/// deltaspace, so each upload stores a full `reference.bin` baseline plus a
/// tiny delta. The quota used to sum only the per-object stored sizes (the
/// deltas, tens of bytes) and never saw the baselines, so a 5 MB quota let
/// through any number of 3 MB builds. It must also answer 403 (documented),
/// not a 500 that SDKs retry as a server fault.
#[tokio::test]
async fn test_quota_counts_delta_baselines_and_answers_403() {
    let server = TestServer::builder()
        .bucket(BUCKET)
        .auth("TESTKEY", "TESTSECRET")
        .bucket_policy(BUCKET, "quota_bytes: 5242880") // 5 MiB
        .build()
        .await;

    let three_mib = 3 * 1024 * 1024;
    put_sized(&server, "builds/v1/firmware.tar", three_mib)
        .await
        .expect("first 3 MiB build fits a 5 MiB quota");

    let client = server.s3_client().await;
    // The proxy refuses a write over the quota before it has read the whole
    // body, and then it may close the connection while the client still
    // sends: the client sees a broken pipe, not the 403 (the test client
    // never retries). A transport failure is sent again; the answer to the
    // second try is the 403.
    let mut attempts = 0;
    let err = loop {
        attempts += 1;
        let err = client
            .put_object()
            .bucket(server.bucket())
            .key("builds/v2/firmware.tar")
            .body(aws_sdk_s3::primitives::ByteStream::from(vec![
                1u8;
                three_mib
            ]))
            .send()
            .await
            .expect_err("a second 3 MiB baseline must exceed a 5 MiB quota");
        if err.raw_response().is_some() || attempts >= 3 {
            break err;
        }
    };
    let status = err.raw_response().map(|r| r.status().as_u16());
    assert_eq!(
        status,
        Some(403),
        "quota rejection must be 403, got {status:?}: {err:?}"
    );
}

/// A bucket with a quota and one writer after the other gets a TRUSTED
/// counter from the scan that its first PUT starts. The PUT's quota check
/// spawns the scan inside the PUT, so the scan often ends while the PUT is
/// between its storage write and the record of its size; such a scan is
/// not trusted, and it is repeated once the PUT is done. The trusted
/// counter is exact (every object once) and stays exact for the next
/// PUTs, which start no more scans. A bucket whose every scan was refused
/// would leave the quota to the scanner's snapshot, up to the cache TTL old.
#[tokio::test]
async fn a_serial_writer_gets_a_trusted_exact_counter() {
    let server = TestServer::builder()
        .bucket(BUCKET)
        .bucket_policy(BUCKET, "quota_bytes: 1000000000")
        .env("DGP_USAGE_CACHE_TTL_SECS", "1")
        .build()
        .await;
    let http = reqwest::Client::new();
    let endpoint = server.endpoint();
    let admin = common::admin_http_client(&endpoint).await;
    let usage_url = format!("{endpoint}/_/api/admin/usage/bucket/{}", server.bucket());
    let counter = || async {
        let body: serde_json::Value = admin
            .get(&usage_url)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        body
    };

    // The first PUT starts the scan; wait for it to end (the scan repeats
    // inside the same task, so the version moves once).
    let before = common::get_usage_scan_version(&http, &endpoint).await;
    put_sized(&server, "serial-0.dat", 3000).await.expect("PUT");
    common::wait_for_usage_scan_refresh(&http, &endpoint, before).await;
    let body = counter().await;
    assert_eq!(
        body["never_scanned"], false,
        "the scan of a serial writer was not trusted: {body}"
    );
    assert_eq!(
        (body["object_count"].as_u64(), body["stored_bytes"].as_u64()),
        (Some(1), Some(3000)),
        "a trusted counter must be exact: {body}"
    );

    // And it stays trusted and exact for the next writes.
    for n in 2..=5u64 {
        put_sized(&server, &format!("serial-{n}.dat"), 3000)
            .await
            .expect("PUT");
        let body = counter().await;
        assert_eq!(body["never_scanned"], false, "{body}");
        assert_eq!(
            (body["object_count"].as_u64(), body["stored_bytes"].as_u64()),
            (Some(n), Some(3000 * n)),
            "{body}"
        );
    }
}
