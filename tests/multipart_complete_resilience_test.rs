// SPDX-License-Identifier: BUSL-1.1

//! CompleteMultipartUpload stops on a client disconnect and tolerates retries.
//!
//! Requests are stateless: a dropped connection drops the store, the upload
//! rolls back to Open, and the retry completes it (found live: killing an
//! HAProxy router mid-Complete on Kubernetes left the upload in `Completing`
//! and the retry got `InvalidRequest`). Retries go through a completion
//! registry (join in-flight work / hit the success tombstone). `DGP_TEST_COMPLETE_STALL_MS` holds the store window open so these
//! tests can hit it deterministically.

use crate::common;

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

/// The client vanishes mid-Complete; the store stops, the object is absent, the
/// upload is Open, and a retried Complete returns 200 with the object intact.
#[tokio::test]
async fn a_dropped_complete_stops_and_the_retry_succeeds() {
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

    // Wait past the stall window: a store that ran on would have landed.
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
    assert!(
        client
            .head_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .is_err(),
        "a dropped Complete stored the object"
    );
    let open = client
        .list_parts()
        .bucket(bucket)
        .key(key)
        .upload_id(&upload_id)
        .send()
        .await
        .expect("the upload is Open again");
    assert_eq!(open.parts().len(), 1);

    // The SDK's retry completes the upload, never
    // "Upload is already being completed" / NoSuchUpload.
    let retry = client
        .complete_multipart_upload()
        .bucket(bucket)
        .key(key)
        .upload_id(&upload_id)
        .multipart_upload(completed)
        .send()
        .await
        .expect("a retried Complete must succeed after a disconnect");
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
    // The completed upload no longer exists: NoSuchUpload, as on S3.
    let msg = format!("{:?}", err.into_service_error());
    assert!(msg.contains("NoSuchUpload"), "unexpected error: {msg}");
}

/// A Complete whose `200` is out (keepalive) still holds its request slot until
/// its body ends: with one slot, a GET sent after the `200` answers after the
/// Complete ends. Tower gave the slot back when the headers went out.
#[tokio::test]
async fn a_late_complete_body_holds_the_request_slot() {
    let server = TestServer::builder()
        .env("DGP_MAX_CONCURRENT_REQUESTS", "1")
        .env("DGP_TEST_COMPLETE_STALL_MS", "3000")
        .env("DGP_TEST_COMPLETE_KEEPALIVE_AFTER_MS", "300")
        .build()
        .await;
    let client = server.s3_client().await;
    let bucket = server.bucket().to_string();
    client
        .put_object()
        .bucket(&bucket)
        .key("other.txt")
        .body(ByteStream::from(b"x".to_vec()))
        .send()
        .await
        .unwrap();
    let key = "resilience/slot.bin";
    let (upload_id, completed) = seed_multipart(&client, &bucket, key).await;

    let complete = {
        let (client, bucket) = (client.clone(), bucket.clone());
        tokio::spawn(async move {
            client
                .complete_multipart_upload()
                .bucket(&bucket)
                .key(key)
                .upload_id(&upload_id)
                .multipart_upload(completed)
                .send()
                .await
                .expect("complete");
            std::time::Instant::now()
        })
    };
    // Past the keepalive: the 200 is out, the store still runs.
    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
    client
        .get_object()
        .bucket(&bucket)
        .key("other.txt")
        .send()
        .await
        .expect("get");
    let got = std::time::Instant::now();
    let complete_ended = complete.await.unwrap();
    assert!(
        got >= complete_ended,
        "the GET was answered while the Complete body still held the only slot"
    );
}

/// The client closes the socket AFTER the `200` headers of a long Complete:
/// the store stops, the object is absent, the upload is Open, the retry
/// completes, and the next request gets the only slot at once.
#[tokio::test]
async fn a_cut_after_the_200_headers_stops_the_store_and_frees_the_slot() {
    let server = TestServer::builder()
        .env("DGP_MAX_CONCURRENT_REQUESTS", "1")
        .env("DGP_TEST_COMPLETE_STALL_MS", "4000")
        .env("DGP_TEST_COMPLETE_KEEPALIVE_AFTER_MS", "200")
        .build()
        .await;
    let client = server.s3_client().await;
    let http = server.http();
    let bucket = server.bucket().to_string();
    client
        .put_object()
        .bucket(&bucket)
        .key("other.txt")
        .body(ByteStream::from(b"x".to_vec()))
        .send()
        .await
        .unwrap();
    let key = "resilience/latecut.bin";
    let (upload_id, completed) = seed_multipart(&client, &bucket, key).await;
    let etag = completed.parts()[0].e_tag().unwrap().to_string();
    let body = format!(
        "<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>{etag}</ETag></Part></CompleteMultipartUpload>"
    );
    let resp = http
        .post(format!(
            "{}/{bucket}/{key}?uploadId={upload_id}",
            server.endpoint()
        ))
        .body(body)
        .send()
        .await
        .expect("the 200 headers arrive");
    assert_eq!(resp.status().as_u16(), 200);
    // Headers are out, the store still stalls: close the socket.
    drop(resp);

    // The slot is free as soon as the proxy sees the close, long before the stall ends.
    let started = std::time::Instant::now();
    client
        .get_object()
        .bucket(&bucket)
        .key("other.txt")
        .send()
        .await
        .expect("the next GET gets the slot");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(3),
        "the GET waited for the stalled store: {:?}",
        started.elapsed()
    );

    // Past the stall window: a store that ran on would have landed.
    tokio::time::sleep(std::time::Duration::from_millis(4500)).await;
    assert!(client
        .head_object()
        .bucket(&bucket)
        .key(key)
        .send()
        .await
        .is_err());
    let open = client
        .list_parts()
        .bucket(&bucket)
        .key(key)
        .upload_id(&upload_id)
        .send()
        .await
        .expect("the upload is Open");
    assert_eq!(open.parts().len(), 1);
    client
        .complete_multipart_upload()
        .bucket(&bucket)
        .key(key)
        .upload_id(&upload_id)
        .multipart_upload(completed)
        .send()
        .await
        .expect("the retry completes");
    assert!(client
        .head_object()
        .bucket(&bucket)
        .key(key)
        .send()
        .await
        .is_ok());
}
