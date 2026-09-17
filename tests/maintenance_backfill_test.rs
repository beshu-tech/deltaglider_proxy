// SPDX-License-Identifier: BUSL-1.1

//! Integration tests for the `backfill-metadata` maintenance job
//! (`src/maintenance/backfill.rs`): a foreign passthrough file (planted on
//! disk with no xattr — the shape a tar/rsync restore or a direct backend
//! write leaves behind) gets the canonical DG metadata stamped IN PLACE,
//! bytes untouched, with the served LastModified preserved by default and
//! moved only when `refresh_last_modified: true` is requested.
//!
//! Mostly filesystem-backed (no MinIO needed); the last test exercises the
//! S3 backend's server-side self-copy path against MinIO (skipped without
//! it), including multipart-ETag stability across the copy.

mod common;

use common::{admin_http_client, put_object, TestServer};
use sha2::{Digest, Sha256};

const FOREIGN_BODY: &[u8] = b"FOREIGN_BYTES_planted_without_metadata_0123456789";

/// Plant a foreign passthrough file directly in the filesystem backend's
/// data dir (no xattr — exactly what a non-proxy writer produces).
fn plant_foreign_file(server: &TestServer, filename: &str) -> std::path::PathBuf {
    let dir = server
        .data_dir()
        .expect("filesystem-backed TestServer has a data dir")
        .join(server.bucket())
        .join("deltaspaces");
    std::fs::create_dir_all(&dir).expect("mkdir deltaspaces");
    let path = dir.join(filename);
    std::fs::write(&path, FOREIGN_BODY).expect("plant foreign file");
    path
}

async fn start_backfill(
    admin: &reqwest::Client,
    endpoint: &str,
    bucket: &str,
    refresh_last_modified: bool,
) -> serde_json::Value {
    let resp = admin
        .post(format!("{endpoint}/_/api/admin/jobs/backfill-metadata"))
        .json(&serde_json::json!({
            "buckets": [bucket],
            "refresh_last_modified": refresh_last_modified,
        }))
        .send()
        .await
        .expect("backfill POST failed");
    assert!(
        resp.status().is_success(),
        "backfill POST failed: {}",
        resp.status()
    );
    let v: serde_json::Value = resp.json().await.expect("backfill response not JSON");
    assert!(
        v["errors"].as_array().is_some_and(|e| e.is_empty()),
        "backfill create reported errors: {v}"
    );
    v
}

/// Poll the session-light bucket endpoint until no job is active.
async fn wait_job_done(admin: &reqwest::Client, endpoint: &str, bucket: &str) {
    for _ in 0..600 {
        let v: serde_json::Value = admin
            .get(format!("{endpoint}/_/api/admin/jobs/bucket/{bucket}"))
            .send()
            .await
            .expect("status GET failed")
            .json()
            .await
            .expect("status not JSON");
        if v["active"].is_null() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("backfill job on '{bucket}' did not finish within 60s");
}

/// The planted file's xattr metadata, if any.
fn xattr_of(path: &std::path::Path) -> Option<serde_json::Value> {
    let bytes = xattr::get(path, "user.dg.metadata").ok().flatten()?;
    serde_json::from_slice(&bytes).ok()
}

async fn head_last_modified(
    client: &aws_sdk_s3::Client,
    bucket: &str,
    key: &str,
) -> aws_sdk_s3::primitives::DateTime {
    client
        .head_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .expect("HEAD failed")
        .last_modified
        .expect("HEAD carries LastModified")
}

#[tokio::test]
async fn backfill_stamps_canonical_metadata_and_preserves_served_time() {
    let server = TestServer::builder()
        .bucket("backfill-preserve")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;
    let http = reqwest::Client::new();
    let client = server.s3_client().await;

    // One canonical object via the proxy (must be SKIPPED, not rewritten)
    // and one foreign file planted with no metadata (must be stamped).
    put_object(
        &http,
        &server.endpoint(),
        server.bucket(),
        "canonical.json",
        b"written through the proxy".to_vec(),
        "application/json",
    )
    .await;
    let planted = plant_foreign_file(&server, "foreign.bin");
    assert!(
        xattr_of(&planted).is_none(),
        "plant must start metadata-less"
    );

    let before = head_last_modified(&client, server.bucket(), "foreign.bin").await;
    // LastModified has 1s granularity — let the clock move so a wrongly
    // refreshed timestamp would be visible.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

    start_backfill(&admin, &server.endpoint(), server.bucket(), false).await;
    wait_job_done(&admin, &server.endpoint(), server.bucket()).await;

    // The job row: 1 stamped, at least 1 skipped, 0 failed, completed.
    let jobs: serde_json::Value = admin
        .get(format!("{}/_/api/admin/jobs", server.endpoint()))
        .send()
        .await
        .expect("jobs GET")
        .json()
        .await
        .expect("jobs JSON");
    let job = jobs["jobs"]
        .as_array()
        .expect("jobs array")
        .iter()
        .find(|j| j["kind"] == "backfill-metadata")
        .expect("backfill job row")
        .clone();
    assert_eq!(job["status_raw"], "completed", "job: {job}");
    assert_eq!(job["progress"]["processed"], 1, "job: {job}");
    assert_eq!(job["progress"]["failed"], 0, "job: {job}");
    assert!(
        job["progress"]["skipped"].as_i64().unwrap_or(0) >= 1,
        "the canonical object must be skipped, not rewritten: {job}"
    );

    // The planted file now carries the canonical metadata — and the SAME bytes.
    let meta = xattr_of(&planted).expect("backfill must stamp the xattr");
    let expected_sha = hex::encode(Sha256::digest(FOREIGN_BODY));
    assert_eq!(meta["file_sha256"], expected_sha.as_str(), "meta: {meta}");
    assert_eq!(meta["file_size"], FOREIGN_BODY.len() as u64, "meta: {meta}");
    assert_eq!(
        std::fs::read(&planted).expect("read planted"),
        FOREIGN_BODY,
        "backfill must never touch the bytes"
    );

    // Default mode: the served LastModified is unchanged.
    let after = head_last_modified(&client, server.bucket(), "foreign.bin").await;
    assert_eq!(
        before, after,
        "refresh_last_modified=false must not move the served time"
    );

    // The object still reads back byte-identical through the proxy.
    let got = client
        .get_object()
        .bucket(server.bucket())
        .key("foreign.bin")
        .send()
        .await
        .expect("GET failed")
        .body
        .collect()
        .await
        .expect("body")
        .into_bytes();
    assert_eq!(got.as_ref(), FOREIGN_BODY);
}

#[tokio::test]
async fn backfill_refresh_last_modified_moves_the_served_time() {
    let server = TestServer::builder()
        .bucket("backfill-refresh")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;
    let client = server.s3_client().await;

    let planted = plant_foreign_file(&server, "refresh-me.bin");
    let before = head_last_modified(&client, server.bucket(), "refresh-me.bin").await;
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

    start_backfill(&admin, &server.endpoint(), server.bucket(), true).await;
    wait_job_done(&admin, &server.endpoint(), server.bucket()).await;

    let meta = xattr_of(&planted).expect("backfill must stamp the xattr");
    assert_eq!(
        meta["file_sha256"],
        hex::encode(Sha256::digest(FOREIGN_BODY)).as_str()
    );
    let after = head_last_modified(&client, server.bucket(), "refresh-me.bin").await;
    assert!(
        after.secs() > before.secs(),
        "refresh_last_modified=true must move the served time forward \
         (before={before:?}, after={after:?})"
    );
}

/// The S3 backend path: the backfill's metadata write is a SERVER-SIDE
/// self-copy (`MetadataDirective: REPLACE`) — no bytes through the proxy.
/// A self-copy collapses a multipart ETag, so this seeds a real foreign
/// multipart upload and asserts the served ETag survives via the
/// `dg-multipart-etag` override, alongside the preserved LastModified.
#[tokio::test]
async fn backfill_s3_self_copy_preserves_etag_and_served_time() {
    skip_unless_minio!();
    let bucket = "backfill-s3-test";
    let server = TestServer::s3_with_endpoint(&common::minio_endpoint_url(), bucket).await;
    let admin = admin_http_client(&server.endpoint()).await;
    let client = server.s3_client().await;
    let raw = common::minio_client().await;

    // Foreign multipart upload straight into the backend: no dg metadata,
    // and an ETag of the "…-2" multipart shape.
    let key = "foreign-mpu.bin";
    let part1 = vec![7u8; 5 * 1024 * 1024];
    let part2 = vec![9u8; 1024];
    let upload_id = raw
        .create_multipart_upload()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .expect("create mpu")
        .upload_id
        .expect("upload id");
    let mut parts = Vec::new();
    for (n, body) in [(1, part1.clone()), (2, part2.clone())] {
        let etag = raw
            .upload_part()
            .bucket(bucket)
            .key(key)
            .upload_id(&upload_id)
            .part_number(n)
            .body(aws_sdk_s3::primitives::ByteStream::from(body))
            .send()
            .await
            .expect("upload part")
            .e_tag
            .expect("part etag");
        parts.push(
            aws_sdk_s3::types::CompletedPart::builder()
                .part_number(n)
                .e_tag(etag)
                .build(),
        );
    }
    raw.complete_multipart_upload()
        .bucket(bucket)
        .key(key)
        .upload_id(&upload_id)
        .multipart_upload(
            aws_sdk_s3::types::CompletedMultipartUpload::builder()
                .set_parts(Some(parts))
                .build(),
        )
        .send()
        .await
        .expect("complete mpu");

    let before = client
        .head_object()
        .bucket(server.bucket())
        .key(key)
        .send()
        .await
        .expect("HEAD before");
    let etag_before = before.e_tag.clone().expect("etag");
    assert!(
        etag_before.contains('-'),
        "seed must carry a multipart ETag, got {etag_before}"
    );
    let lm_before = before.last_modified.expect("last modified");
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

    start_backfill(&admin, &server.endpoint(), server.bucket(), false).await;
    wait_job_done(&admin, &server.endpoint(), server.bucket()).await;

    // Raw object now carries the canonical hash of the FULL content.
    let raw_head = raw
        .head_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .expect("raw HEAD");
    let md = raw_head.metadata.clone().unwrap_or_default();
    let mut sha = Sha256::new();
    sha.update(&part1);
    sha.update(&part2);
    assert_eq!(
        md.get("dg-file-sha256").map(String::as_str),
        Some(hex::encode(sha.finalize()).as_str()),
        "raw metadata: {md:?}"
    );

    // Served ETag and LastModified are unchanged across the self-copy.
    let after = client
        .head_object()
        .bucket(server.bucket())
        .key(key)
        .send()
        .await
        .expect("HEAD after");
    assert_eq!(
        after.e_tag.as_deref().map(|e| e.trim_matches('"')),
        Some(etag_before.trim_matches('"')),
        "the multipart ETag must survive the self-copy via dg-multipart-etag"
    );
    assert_eq!(
        after.last_modified.expect("last modified").secs(),
        lm_before.secs(),
        "refresh_last_modified=false must not move the served time"
    );

    // Bytes intact through the proxy.
    let got = client
        .get_object()
        .bucket(server.bucket())
        .key(key)
        .send()
        .await
        .expect("GET")
        .body
        .collect()
        .await
        .expect("body")
        .into_bytes();
    assert_eq!(got.len(), part1.len() + part2.len());
    assert_eq!(&got[..part1.len()], part1.as_slice());
    assert_eq!(&got[part1.len()..], part2.as_slice());
}
