//! H1 correctness fix regression tests: the ETag returned by
//! CompleteMultipartUpload must match the ETag returned by every
//! subsequent HEAD/GET/LIST on that object.
//!
//! Pre-fix, Complete returned `"md5(concat)-N"` but the persisted
//! FileMetadata carried a plain full-body MD5 — two different ETags
//! for the same object. Clients that cache the ETag from Complete and
//! use it as an If-Match precondition would hit 412 Precondition
//! Failed on a later write.

mod common;

use aws_sdk_s3::primitives::ByteStream;
use common::TestServer;

/// Seed a multipart upload with two parts, complete it, then verify
/// HEAD + LIST return the SAME ETag the Complete response gave.
#[tokio::test]
async fn test_multipart_etag_matches_head_and_list() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    let bucket = server.bucket();
    let key = "big-file.bin";

    // 1. CreateMultipartUpload.
    let create = client
        .create_multipart_upload()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .expect("create multipart");
    let upload_id = create.upload_id().expect("upload_id").to_string();

    // 2. UploadPart 1 + 2.
    let part1_body = vec![0xAAu8; 5 * 1024 * 1024]; // 5 MiB (S3 minimum part size)
    let part2_body = vec![0xBBu8; 1024];

    let upload_part1 = client
        .upload_part()
        .bucket(bucket)
        .key(key)
        .upload_id(&upload_id)
        .part_number(1)
        .body(ByteStream::from(part1_body.clone()))
        .send()
        .await
        .expect("upload part 1");
    let part1_etag = upload_part1.e_tag().expect("part 1 etag").to_string();

    let upload_part2 = client
        .upload_part()
        .bucket(bucket)
        .key(key)
        .upload_id(&upload_id)
        .part_number(2)
        .body(ByteStream::from(part2_body.clone()))
        .send()
        .await
        .expect("upload part 2");
    let part2_etag = upload_part2.e_tag().expect("part 2 etag").to_string();

    // 3. CompleteMultipartUpload with both parts.
    use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
    let completed = CompletedMultipartUpload::builder()
        .parts(
            CompletedPart::builder()
                .part_number(1)
                .e_tag(&part1_etag)
                .build(),
        )
        .parts(
            CompletedPart::builder()
                .part_number(2)
                .e_tag(&part2_etag)
                .build(),
        )
        .build();

    let complete = client
        .complete_multipart_upload()
        .bucket(bucket)
        .key(key)
        .upload_id(&upload_id)
        .multipart_upload(completed)
        .send()
        .await
        .expect("complete multipart");
    let complete_etag = complete.e_tag().expect("complete etag").to_string();

    // Sanity: multipart ETag format is "md5-N".
    assert!(
        complete_etag.contains('-'),
        "Complete ETag should be multipart format 'md5-N', got {}",
        complete_etag
    );
    assert!(
        complete_etag.ends_with("-2\""),
        "Complete ETag should end with -2\" for 2 parts, got {}",
        complete_etag
    );

    // 4. HEAD the object — must return the SAME ETag.
    let head = client
        .head_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .expect("head");
    let head_etag = head.e_tag().expect("head etag").to_string();
    assert_eq!(
        head_etag, complete_etag,
        "H1 REGRESSION: HEAD ETag {} does not match Complete ETag {}",
        head_etag, complete_etag
    );

    // 5. LIST the bucket — must also return the SAME ETag for this object.
    let list = client
        .list_objects_v2()
        .bucket(bucket)
        .send()
        .await
        .expect("list");
    let listed = list
        .contents()
        .iter()
        .find(|o| o.key() == Some(key))
        .expect("object in listing");
    let list_etag = listed.e_tag().expect("list etag").to_string();
    assert_eq!(
        list_etag, complete_etag,
        "H1 REGRESSION: LIST ETag {} does not match Complete ETag {}",
        list_etag, complete_etag
    );
}

/// Regression: a single-PUT (non-multipart) object must STILL get the
/// full-body MD5 ETag, not a spurious multipart-style one.
#[tokio::test]
async fn test_single_put_etag_is_md5_not_multipart() {
    let server = TestServer::filesystem().await;
    let client = server.s3_client().await;

    client
        .put_object()
        .bucket(server.bucket())
        .key("plain.txt")
        .body(ByteStream::from(b"hello".to_vec()))
        .send()
        .await
        .expect("put");

    let head = client
        .head_object()
        .bucket(server.bucket())
        .key("plain.txt")
        .send()
        .await
        .expect("head");
    let etag = head.e_tag().expect("etag");
    // MD5 of "hello" is 5d41402abc4b2a76b9719d911017c592 — NO -N suffix.
    assert!(
        !etag.contains('-'),
        "Single-PUT ETag must not look multipart, got {}",
        etag
    );
}
