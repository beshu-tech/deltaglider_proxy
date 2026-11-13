// SPDX-License-Identifier: BUSL-1.1

//! A client multipart object above the single-PUT limit of the backend
//! (5 GiB in production, 5 MiB through `DGP_TEST_PUT_SINGLE_MAX_BYTES`) is
//! stored as a backend multipart upload, and a CompleteMultipartUpload that
//! runs longer than a client's read timeout still succeeds.

use crate::common;

use aws_sdk_s3::error::ProvideErrorMetadata;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
use common::TestServer;
use md5::Digest;

const MIB: usize = 1024 * 1024;

/// The proxy with an S3 backend (MinIO) whose single-PUT limit is 5 MiB and
/// whose backend parts are 6 MiB. The client's parts are 5 MiB, so the
/// parts of the backend upload are not the parts of the client's: a
/// multipart ETag that the proxy took from the backend would differ from
/// the client's.
fn seamed(bucket: &str) -> common::TestServerBuilder {
    TestServer::builder()
        .s3_endpoint(&common::minio_endpoint_url())
        .bucket(bucket)
        .env("DGP_TEST_PUT_SINGLE_MAX_BYTES", "5242880")
        .env("DGP_TEST_PUT_PART_BYTES", "6291456")
}

/// The ETag S3 gives a multipart object of these parts: the MD5 of the
/// parts' MD5s, and the part count.
fn multipart_etag(parts: &[Vec<u8>]) -> String {
    let mut digests = Vec::new();
    for p in parts {
        digests.extend_from_slice(&md5::Md5::digest(p));
    }
    format!(
        "\"{}-{}\"",
        hex::encode(md5::Md5::digest(&digests)),
        parts.len()
    )
}

/// Upload `parts` as one client multipart upload and complete it. Returns
/// the Complete response's ETag.
// The test reads the SDK error's code and status, so it is not boxed.
#[allow(clippy::result_large_err)]
async fn client_multipart(
    client: &aws_sdk_s3::Client,
    bucket: &str,
    key: &str,
    parts: &[Vec<u8>],
) -> Result<String, aws_sdk_s3::Error> {
    let upload_id = client
        .create_multipart_upload()
        .bucket(bucket)
        .key(key)
        .send()
        .await?
        .upload_id()
        .unwrap()
        .to_string();
    let mut done = CompletedMultipartUpload::builder();
    for (i, body) in parts.iter().enumerate() {
        let number = i as i32 + 1;
        let part = client
            .upload_part()
            .bucket(bucket)
            .key(key)
            .upload_id(&upload_id)
            .part_number(number)
            .body(ByteStream::from(body.clone()))
            .send()
            .await?;
        done = done.parts(
            CompletedPart::builder()
                .part_number(number)
                .e_tag(part.e_tag().unwrap())
                .build(),
        );
    }
    let out = client
        .complete_multipart_upload()
        .bucket(bucket)
        .key(key)
        .upload_id(&upload_id)
        .multipart_upload(done.build())
        .send()
        .await?;
    Ok(out.e_tag().unwrap().to_string())
}

async fn get(client: &aws_sdk_s3::Client, bucket: &str, key: &str) -> Vec<u8> {
    client
        .get_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .expect("get")
        .body
        .collect()
        .await
        .unwrap()
        .to_vec()
}

/// The sum of the samples of the backend request counter of one operation.
async fn backend_operations(endpoint: &str, operation: &str) -> u64 {
    let label = format!("operation=\"{operation}\"");
    common::metrics_text(endpoint)
        .await
        .lines()
        .filter(|l| l.starts_with("deltaglider_backend_request_duration_seconds_count"))
        .filter(|l| l.contains(&label))
        .filter_map(|l| l.rsplit_once(' ')?.1.trim().parse::<f64>().ok())
        .sum::<f64>() as u64
}

/// The multipart uploads that MinIO holds open in `bucket` for `key`.
async fn open_backend_uploads(bucket: &str, key: &str) -> Vec<String> {
    common::minio_client()
        .await
        .list_multipart_uploads()
        .bucket(bucket)
        .prefix(key)
        .send()
        .await
        .expect("list multipart uploads")
        .uploads()
        .iter()
        .filter_map(|u| u.upload_id().map(str::to_string))
        .collect()
}

/// 13 MiB in three client parts (5 + 5 + 3 MiB).
fn thirteen_mib() -> Vec<Vec<u8>> {
    let body = common::big_passthrough_body(13 * MIB);
    vec![
        body[..5 * MIB].to_vec(),
        body[5 * MIB..10 * MIB].to_vec(),
        body[10 * MIB..].to_vec(),
    ]
}

/// Store a 13 MiB client multipart object through `server` and check it.
/// The backend got ONE multipart upload of three parts, and the client sees
/// its own multipart ETag on Complete, HEAD and GET, with its bytes.
async fn assert_large_multipart_round_trips(server: &TestServer, key: &str) {
    skip_unless_minio!();
    let client = server.s3_client().await;
    let bucket = server.bucket();
    let parts = thirteen_mib();
    let before = (
        backend_operations(&server.endpoint(), "CreateMultipartUpload").await,
        backend_operations(&server.endpoint(), "UploadPart").await,
        backend_operations(&server.endpoint(), "CompleteMultipartUpload").await,
    );
    let etag = client_multipart(&client, bucket, key, &parts)
        .await
        .unwrap_or_else(|e| panic!("{key}: the multipart upload must complete: {e:?}"));
    assert_eq!(etag, multipart_etag(&parts), "{key}: Complete's ETag");
    let after = (
        backend_operations(&server.endpoint(), "CreateMultipartUpload").await,
        backend_operations(&server.endpoint(), "UploadPart").await,
        backend_operations(&server.endpoint(), "CompleteMultipartUpload").await,
    );
    // The client's own upload is not a backend upload (the proxy relays its
    // parts to files), so every backend operation here is the store's.
    assert_eq!(
        (after.0 - before.0, after.1 - before.1, after.2 - before.2),
        (1, 3, 1),
        "{key}: one backend multipart upload of three parts"
    );
    let head = client
        .head_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .unwrap();
    assert_eq!(head.e_tag(), Some(etag.as_str()), "{key}: HEAD's ETag");
    assert_eq!(
        head.content_length(),
        Some(13 * MIB as i64),
        "{key}: HEAD's length"
    );
    assert_eq!(get(&client, bucket, key).await, parts.concat(), "{key}");
}

/// Passthrough (`.dat`) and the joined spool file of a delta candidate
/// (`.zip`) are both file-backed backend writes above the limit.
#[tokio::test]
async fn a_client_multipart_object_above_the_put_limit_is_a_backend_multipart_upload() {
    skip_unless_minio!();
    let server = seamed(common::MINIO_BUCKET).build().await;
    let prefix = common::unique_bucket("mpu-put");
    for name in ["big.dat", "big.zip"] {
        assert_large_multipart_round_trips(&server, &format!("{prefix}/{name}")).await;
    }
}

/// The same in a proxy-encrypted bucket: the backend write is the
/// ciphertext temp file, 13 MiB plus the frames' tags.
#[tokio::test]
async fn a_large_multipart_object_in_a_proxy_encrypted_bucket_is_a_backend_multipart_upload() {
    skip_unless_minio!();
    let bucket = common::unique_bucket("mpu-enc");
    let server = seamed(&bucket)
        .encryption_key("0000000000000000000000000000000000000000000000000000000000000001")
        .build()
        .await;
    let client = server.s3_client().await;
    let parts = thirteen_mib();
    let key = "enc/big.dat";
    let before = backend_operations(&server.endpoint(), "UploadPart").await;
    let etag = client_multipart(&client, &bucket, key, &parts)
        .await
        .expect("the encrypted multipart upload completes");
    assert_eq!(etag, multipart_etag(&parts), "Complete's ETag");
    assert!(
        backend_operations(&server.endpoint(), "UploadPart").await - before >= 3,
        "the ciphertext went to the backend in parts"
    );
    assert_eq!(get(&client, &bucket, key).await, parts.concat());
    // A LIST shows the plaintext size, not the size of the ciphertext.
    let listed = client
        .list_objects_v2()
        .bucket(&bucket)
        .prefix(key)
        .send()
        .await
        .unwrap();
    let sizes: Vec<_> = listed.contents().iter().map(|o| o.size()).collect();
    assert_eq!(sizes, vec![Some(13 * MIB as i64)], "LIST size");
    // The backend holds ciphertext, never the plaintext.
    let raw = common::minio_client()
        .await
        .get_object()
        .bucket(&bucket)
        .key(key)
        .send()
        .await
        .expect("the object is on the backend")
        .body
        .collect()
        .await
        .unwrap()
        .to_vec();
    assert!(
        raw.len() > 13 * MIB,
        "ciphertext is larger than the plaintext"
    );
    assert_ne!(&raw[..1024], &parts[0][..1024], "no plaintext at rest");
}

/// A part that fails after earlier parts were stored aborts the backend
/// upload: MinIO keeps no open multipart upload (and no object).
#[tokio::test]
async fn a_failed_backend_part_leaves_no_backend_multipart_upload() {
    skip_unless_minio!();
    common::expect_server_errors("a backend part fails on purpose (DGP_TEST_PUT_FAIL_PART)");
    let server = seamed(common::MINIO_BUCKET)
        .env("DGP_TEST_PUT_FAIL_PART", "3")
        .build()
        .await;
    let client = server.s3_client().await;
    let key = format!("{}/fails.dat", common::unique_bucket("mpu-fail"));
    let err = client_multipart(&client, server.bucket(), &key, &thirteen_mib())
        .await
        .expect_err("the third backend part fails the Complete");
    assert_eq!(err.code(), Some("ServiceUnavailable"), "{err:?}");
    assert_eq!(
        backend_operations(&server.endpoint(), "UploadPart").await,
        2,
        "the two earlier parts were stored before the failure"
    );
    assert!(
        open_backend_uploads(server.bucket(), &key).await.is_empty(),
        "the backend upload is aborted"
    );
    let missing = client
        .head_object()
        .bucket(server.bucket())
        .key(&key)
        .send()
        .await;
    assert!(missing.is_err(), "no object was stored");
}

/// The backend completes the upload and its answer is lost (a dropped
/// connection, a timeout; the debug seam drops it). The proxy finds its own
/// object by its ETag (the MD5 of the parts' MD5s) and answers the client
/// with the success: it does not report a failure and does not send the
/// parts again.
#[tokio::test]
async fn a_lost_backend_complete_answer_is_a_success_not_a_second_upload() {
    skip_unless_minio!();
    let server = seamed(common::MINIO_BUCKET)
        .env("DGP_TEST_PUT_DROP_COMPLETE_ANSWER", "true")
        .build()
        .await;
    let client = server.s3_client().await;
    let key = format!("{}/lost.dat", common::unique_bucket("mpu-lost"));
    let parts = thirteen_mib();
    let etag = client_multipart(&client, server.bucket(), &key, &parts)
        .await
        .unwrap_or_else(|e| panic!("the Complete must succeed: {e:?}"));
    assert_eq!(etag, multipart_etag(&parts));
    let ops = |op: &'static str| {
        let endpoint = server.endpoint();
        async move { backend_operations(&endpoint, op).await }
    };
    assert_eq!(ops("CompleteMultipartUpload").await, 1, "one Complete");
    assert_eq!(ops("UploadPart").await, 3, "the parts were sent once");
    assert_eq!(get(&client, server.bucket(), &key).await, parts.concat());
    assert!(open_backend_uploads(server.bucket(), &key).await.is_empty());
}

/// A client with a short read timeout (the AWS CLI waits 60 s) completes an
/// upload whose store takes longer: the proxy answers 200 at once and sends
/// whitespace until the store ends, then the result document.
#[tokio::test]
async fn a_complete_that_outlasts_the_client_read_timeout_succeeds() {
    let server = TestServer::builder()
        .env("DGP_TEST_COMPLETE_STALL_MS", "3000")
        .env("DGP_TEST_COMPLETE_KEEPALIVE_AFTER_MS", "300")
        .build()
        .await;
    let client = impatient_client(&server);
    let parts = vec![common::big_passthrough_body(6 * MIB)];
    let started = std::time::Instant::now();
    let etag = client_multipart(&client, server.bucket(), "slow/obj.dat", &parts)
        .await
        .unwrap_or_else(|e| panic!("the slow Complete must succeed: {e:?}"));
    assert!(
        started.elapsed() > std::time::Duration::from_millis(2500),
        "the store was slow ({:?})",
        started.elapsed()
    );
    assert_eq!(etag, multipart_etag(&parts));
    assert_eq!(
        get(&client, server.bucket(), "slow/obj.dat").await,
        parts.concat()
    );
}

/// A store that fails after the `200` went out is an error document in the
/// `200` body, as on AWS; the client reads it as the error.
#[tokio::test]
async fn a_failure_after_the_keepalive_is_an_error_document() {
    skip_unless_minio!();
    let server = seamed(common::MINIO_BUCKET)
        .env("DGP_TEST_COMPLETE_STALL_MS", "1500")
        .env("DGP_TEST_COMPLETE_KEEPALIVE_AFTER_MS", "300")
        .env("DGP_TEST_PUT_FAIL_PART", "2")
        .build()
        .await;
    let client = impatient_client(&server);
    let key = format!("{}/late.dat", common::unique_bucket("mpu-late"));
    let err = client_multipart(&client, server.bucket(), &key, &thirteen_mib())
        .await
        .expect_err("the store fails");
    assert_eq!(err.code(), Some("ServiceUnavailable"), "{err:?}");
    assert!(open_backend_uploads(server.bucket(), &key).await.is_empty());
}

/// A client that gives up on a response after 1.5 s without a byte.
fn impatient_client(server: &TestServer) -> aws_sdk_s3::Client {
    let config = aws_sdk_s3::Config::builder()
        .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
        .region(aws_sdk_s3::config::Region::new("us-east-1"))
        .endpoint_url(server.endpoint())
        .credentials_provider(aws_sdk_s3::config::Credentials::new(
            common::TEST_ACCESS_KEY,
            common::TEST_SECRET_KEY,
            None,
            None,
            "test",
        ))
        .force_path_style(true)
        .retry_config(aws_sdk_s3::config::retry::RetryConfig::disabled())
        .timeout_config(
            aws_sdk_s3::config::timeout::TimeoutConfig::builder()
                .read_timeout(std::time::Duration::from_millis(1500))
                .build(),
        )
        .build();
    aws_sdk_s3::Client::from_conf(config)
}
