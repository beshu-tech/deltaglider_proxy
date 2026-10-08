// SPDX-License-Identifier: BUSL-1.1

//! Phase B: streaming multipart replication copy of a large passthrough
//! object. Exercises the `transfer.rs` streaming branch end-to-end through
//! the replication run-now path on the filesystem backend (the default
//! buffering multipart impl, native=false — still drives create → parts →
//! complete with per-part ranged GETs). Asserts the destination object is
//! byte-identical and correctly sized.
//!
//! The threshold + part size are lowered via env so the test object stays
//! small (~6 MiB) while still routing through `plan_parts` → multipart.

use crate::common;

use aws_sdk_s3::primitives::ByteStream;
use common::{
    admin_http_client, big_passthrough_body, minio_endpoint_url, wait_for_run, TestServer,
};

const STREAM_RULE_YAML: &str = "
replication:
  enabled: true
  tick_interval: \"30s\"
  transfers: 2
  upload_concurrency: 2
  rules:
    - name: stream-a-to-b
      enabled: true
      source:
        bucket: stream-src
        prefix: \"\"
      destination:
        bucket: stream-dst
        prefix: \"\"
      interval: \"1h\"
      batch_size: 100
";

#[tokio::test]
async fn test_streaming_multipart_copy_large_passthrough() {
    skip_unless_minio!();
    // ~6 MiB object, 1 MiB stream threshold, 5 MiB parts → 2 parts.
    let body = big_passthrough_body(6 * 1024 * 1024);

    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .s3_endpoint(&minio_endpoint_url())
        .extra_yaml_storage_section(STREAM_RULE_YAML)
        .env("DGP_STREAM_COPY_THRESHOLD", "1048576") // 1 MiB
        .env("DGP_MULTIPART_PART_SIZE", "5242880") // 5 MiB (S3 min)
        .build()
        .await;

    let client = server.s3_client().await;
    for b in ["stream-src", "stream-dst"] {
        client.create_bucket().bucket(b).send().await.ok();
    }

    // `.bin` is not delta-eligible → stored passthrough → range-able →
    // routes through the streaming multipart copy path.
    client
        .put_object()
        .bucket("stream-src")
        .key("big.bin")
        .body(ByteStream::from(body.clone()))
        .send()
        .await
        .expect("seed large object");

    let admin = admin_http_client(&server.endpoint()).await;
    let resp = admin
        .post(format!(
            "{}/_/api/admin/jobs/replication:stream-a-to-b/run-now",
            server.endpoint()
        ))
        .send()
        .await
        .expect("run-now request");
    // run-now is fire-and-forget (202); poll the run history for the outcome.
    assert_eq!(resp.status().as_u16(), 202, "run-now accepted");
    let run = wait_for_run(&admin, &server.endpoint(), "stream-a-to-b").await;
    assert_eq!(run["status"].as_str(), Some("succeeded"), "run: {run}");
    assert_eq!(
        run["objects_processed"].as_i64().unwrap_or(-1),
        1,
        "exactly one object copied: {run}"
    );

    // Destination object must be byte-identical and correctly sized.
    let got = client
        .get_object()
        .bucket("stream-dst")
        .key("big.bin")
        .send()
        .await
        .expect("dest object present")
        .body
        .collect()
        .await
        .unwrap()
        .into_bytes();
    assert_eq!(got.len(), body.len(), "dest size matches source");
    assert_eq!(&got[..], &body[..], "dest bytes byte-identical to source");
}

/// B002: a streaming copy into a native S3 backend must STORE the object's
/// metadata (Content-Type, user metadata, ETag), not only cache it on the
/// node that copied. A second proxy on the same MinIO has a cold cache, as a
/// peer node or a restart would.
#[tokio::test]
async fn test_streaming_copy_stores_the_object_metadata() {
    skip_unless_minio!();
    const RULE: &str = "
replication:
  enabled: true
  tick_interval: \"30s\"
  rules:
    - name: smeta-a-to-b
      enabled: true
      source:
        bucket: smeta-src
        prefix: \"\"
      destination:
        bucket: smeta-dst
        prefix: \"\"
      interval: \"1h\"
";
    let body = big_passthrough_body(6 * 1024 * 1024);
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .s3_endpoint(&minio_endpoint_url())
        .extra_yaml_storage_section(RULE)
        .env("DGP_STREAM_COPY_THRESHOLD", "1048576")
        .env("DGP_MULTIPART_PART_SIZE", "5242880")
        .build()
        .await;
    let client = server.s3_client().await;
    for b in ["smeta-src", "smeta-dst"] {
        client.create_bucket().bucket(b).send().await.ok();
    }
    client
        .put_object()
        .bucket("smeta-src")
        .key("big.bin")
        .content_type("video/mp4")
        .metadata("owner", "alice")
        .body(ByteStream::from(body))
        .send()
        .await
        .expect("seed large object");
    let src = client
        .head_object()
        .bucket("smeta-src")
        .key("big.bin")
        .send()
        .await
        .unwrap();

    let admin = admin_http_client(&server.endpoint()).await;
    let resp = admin
        .post(format!(
            "{}/_/api/admin/jobs/replication:smeta-a-to-b/run-now",
            server.endpoint()
        ))
        .send()
        .await
        .expect("run-now request");
    assert_eq!(resp.status().as_u16(), 202, "run-now accepted");
    let run = wait_for_run(&admin, &server.endpoint(), "smeta-a-to-b").await;
    assert_eq!(run["status"].as_str(), Some("succeeded"), "run: {run}");

    let peer = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .s3_endpoint(&minio_endpoint_url())
        .build()
        .await;
    let dst = peer
        .s3_client()
        .await
        .head_object()
        .bucket("smeta-dst")
        .key("big.bin")
        .send()
        .await
        .expect("dest object present");
    assert_eq!(dst.content_type(), Some("video/mp4"), "Content-Type kept");
    assert_eq!(
        dst.metadata()
            .and_then(|m| m.get("owner"))
            .map(String::as_str),
        Some("alice"),
        "user metadata kept"
    );
    assert_eq!(dst.e_tag(), src.e_tag(), "ETag kept");
}
