// SPDX-License-Identifier: BUSL-1.1

//! CompleteMultipartUpload must survive a client disconnect and tolerate retries.
//!
//! Found live (killing an HAProxy router mid-Complete on Kubernetes): the broken
//! client connection cancelled the handler future mid-store, the upload rolled
//! back, and the SDK's automatic retry got `InvalidRequest: Upload is already
//! being completed` — the object was lost and the retry poisoned. The fix runs
//! the store pipeline on a detached task (disconnect-proof) and routes retries
//! through a completion registry (join in-flight work / hit the success
//! tombstone). `DGP_TEST_COMPLETE_STALL_MS` holds the store window open so these
//! tests can hit it deterministically.

mod common;

use aws_sdk_s3::primitives::ByteStream;
use common::TestServer;

const STALL_MS: &str = "1500";

async fn seed_multipart(
    client: &aws_sdk_s3::Client,
    bucket: &str,
    key: &str,
) -> (String, aws_sdk_s3::types::CompletedMultipartUpload) {
    let create = client
        .create_multipart_upload()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .expect("create multipart");
    let upload_id = create.upload_id().expect("upload_id").to_string();

    let part = client
        .upload_part()
        .bucket(bucket)
        .key(key)
        .upload_id(&upload_id)
        .part_number(1)
        .body(ByteStream::from(vec![0xC4u8; 5 * 1024 * 1024]))
        .send()
        .await
        .expect("upload part");
    let etag = part.e_tag().expect("part etag").to_string();

    let completed = aws_sdk_s3::types::CompletedMultipartUpload::builder()
        .parts(
            aws_sdk_s3::types::CompletedPart::builder()
                .part_number(1)
                .e_tag(etag)
                .build(),
        )
        .build();
    (upload_id, completed)
}

/// The client vanishes mid-Complete; the store must finish anyway, and a retried
/// Complete must return 200 with the object present and intact.
#[tokio::test]
async fn complete_survives_client_disconnect_and_retry_succeeds() {
    let server = TestServer::builder()
        .env("DGP_TEST_COMPLETE_STALL_MS", STALL_MS)
        .build()
        .await;
    let client = server.s3_client().await;
    let bucket = server.bucket();
    let key = "resilience/disconnect.bin";

    let (upload_id, completed) = seed_multipart(&client, bucket, key).await;

    // Fire Complete and abandon it well inside the stalled store window — the
    // dropped future closes the connection, which is exactly what a dying LB
    // or flaky network does to the proxy.
    let racing = client
        .complete_multipart_upload()
        .bucket(bucket)
        .key(key)
        .upload_id(&upload_id)
        .multipart_upload(completed.clone())
        .send();
    let aborted = tokio::time::timeout(std::time::Duration::from_millis(300), racing).await;
    assert!(
        aborted.is_err(),
        "the first Complete must be abandoned mid-flight"
    );

    // The detached task is still storing; poll until the object lands (10s
    // deadline) instead of guessing a fixed sleep — slow CI runners need slack.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if client
            .head_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .is_ok()
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "detached completion never landed the object"
        );
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }

    // The SDK's retry: must be a clean 200 (tombstone), never
    // "Upload is already being completed" / NoSuchUpload.
    let retry = client
        .complete_multipart_upload()
        .bucket(bucket)
        .key(key)
        .upload_id(&upload_id)
        .multipart_upload(completed)
        .send()
        .await
        .expect("retried Complete must succeed after a disconnect");
    let etag = retry.e_tag().expect("etag").to_string();

    let head = client
        .head_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .expect("object must exist");
    assert_eq!(head.content_length().unwrap_or(0), 5 * 1024 * 1024);
    assert_eq!(head.e_tag().expect("head etag"), &etag);
}

/// Two identical Completes race: the second must join the first's outcome —
/// both return 200 with the same ETag, and the object is stored exactly once.
#[tokio::test]
async fn concurrent_identical_completes_join_one_outcome() {
    let server = TestServer::builder()
        .env("DGP_TEST_COMPLETE_STALL_MS", STALL_MS)
        .build()
        .await;
    let client = server.s3_client().await;
    let bucket = server.bucket();
    let key = "resilience/join.bin";

    let (upload_id, completed) = seed_multipart(&client, bucket, key).await;

    let first = client
        .complete_multipart_upload()
        .bucket(bucket)
        .key(key)
        .upload_id(&upload_id)
        .multipart_upload(completed.clone())
        .send();
    let second = async {
        // Land inside the first request's stalled store window.
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        client
            .complete_multipart_upload()
            .bucket(bucket)
            .key(key)
            .upload_id(&upload_id)
            .multipart_upload(completed.clone())
            .send()
            .await
    };
    let (r1, r2) = tokio::join!(first, second);
    let e1 = r1
        .expect("owner Complete")
        .e_tag()
        .expect("etag1")
        .to_string();
    let e2 = r2
        .expect("joined Complete")
        .e_tag()
        .expect("etag2")
        .to_string();
    assert_eq!(e1, e2, "joiner must return the owner's outcome");

    let head = client
        .head_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .expect("object exists");
    assert_eq!(head.e_tag().expect("head etag"), &e1);
}

/// A Complete retried with a DIFFERENT part list after success must be refused,
/// not silently accepted against the tombstone.
#[tokio::test]
async fn tombstone_rejects_mismatched_part_list() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;
    let bucket = server.bucket();
    let key = "resilience/mismatch.bin";

    let (upload_id, completed) = seed_multipart(&client, bucket, key).await;
    client
        .complete_multipart_upload()
        .bucket(bucket)
        .key(key)
        .upload_id(&upload_id)
        .multipart_upload(completed)
        .send()
        .await
        .expect("first Complete succeeds");

    let wrong = aws_sdk_s3::types::CompletedMultipartUpload::builder()
        .parts(
            aws_sdk_s3::types::CompletedPart::builder()
                .part_number(1)
                .e_tag("\"deadbeefdeadbeefdeadbeefdeadbeef\"")
                .build(),
        )
        .build();
    let err = client
        .complete_multipart_upload()
        .bucket(bucket)
        .key(key)
        .upload_id(&upload_id)
        .multipart_upload(wrong)
        .send()
        .await
        .expect_err("mismatched retry must be refused");
    let msg = format!("{:?}", err.into_service_error());
    assert!(
        msg.contains("InvalidPart") || msg.contains("different part list"),
        "unexpected error: {msg}"
    );
}
