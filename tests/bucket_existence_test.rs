//! C2 security fix regression tests: writes to a non-existent bucket must
//! always return NoSuchBucket, never implicitly create the bucket root.
//!
//! History: pre-fix, the filesystem backend silently created the bucket
//! directory via `ensure_dir` → `create_dir_all` on the first PUT. This
//! bypassed any `s3:CreateBucket` equivalent and produced a contract
//! mismatch with the S3 backend (which always rejects writes to missing
//! buckets). Both defences below are covered here:
//!
//! 1. Handler precheck: `ensure_bucket_exists` in `object_helpers` fails
//!    fast with a clean `NoSuchBucket` HTTP error.
//! 2. Backend guard: `FilesystemBackend::require_bucket_exists` fires if
//!    somehow a caller bypasses the handler layer (defence in depth).

mod common;

use common::TestServer;

/// PUT to a nonexistent bucket on the filesystem backend must return 404
/// NoSuchBucket, NOT create the bucket as a side effect.
#[tokio::test]
async fn test_put_object_to_nonexistent_bucket_returns_nosuchbucket() {
    let server = TestServer::filesystem().await;
    let client = reqwest::Client::new();

    // The test server creates exactly one bucket ("deltaglider-test-<port>").
    // Use a guaranteed-absent name.
    let ghost = "ghost-bucket-does-not-exist";
    let url = format!("{}/{}/anyfile.txt", server.endpoint(), ghost);

    let resp = client
        .put(&url)
        .body(b"hello".to_vec())
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status().as_u16(),
        404,
        "PUT to nonexistent bucket should return 404"
    );

    let body = resp.text().await.unwrap();
    assert!(
        body.contains("<Code>NoSuchBucket</Code>"),
        "Should return NoSuchBucket, got body: {}",
        body
    );
}

/// Verify the filesystem side didn't materialise the bucket directory
/// as a side effect of the failed PUT.
#[tokio::test]
async fn test_put_object_to_nonexistent_bucket_does_not_create_directory() {
    let server = TestServer::filesystem().await;
    let client = reqwest::Client::new();

    let data_dir = server
        .data_dir()
        .expect("filesystem server should expose a data dir");

    let ghost = "never-should-exist";
    let url = format!("{}/{}/attempt.bin", server.endpoint(), ghost);
    let _ = client
        .put(&url)
        .body(b"payload".to_vec())
        .send()
        .await
        .unwrap();

    // The bucket root under `data_dir/<ghost>` must not have been created.
    let ghost_path = data_dir.join(ghost);
    assert!(
        !ghost_path.exists(),
        "Bucket directory {:?} must not be implicitly created by a failed PUT",
        ghost_path
    );
}

/// CompleteMultipartUpload on a bucket that was deleted between initiate
/// and complete must return NoSuchBucket — the subsequent engine.store
/// would otherwise silently recreate the bucket.
///
/// This variant exercises the CreateMultipartUpload precheck directly: a
/// POST?uploads targeting a non-existent bucket must fail fast.
#[tokio::test]
async fn test_create_multipart_upload_to_nonexistent_bucket_returns_nosuchbucket() {
    let server = TestServer::filesystem().await;
    let client = reqwest::Client::new();

    let ghost = "missing-for-multipart";
    let url = format!("{}/{}/file.zip?uploads", server.endpoint(), ghost);

    let resp = client.post(&url).send().await.unwrap();

    assert_eq!(resp.status().as_u16(), 404);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("<Code>NoSuchBucket</Code>"),
        "CreateMultipartUpload on missing bucket should return NoSuchBucket, got: {}",
        body
    );
}

/// Copy between buckets where the destination doesn't exist must
/// return NoSuchBucket — the destination must not be implicitly created.
#[tokio::test]
async fn test_copy_to_nonexistent_destination_bucket_returns_nosuchbucket() {
    let server = TestServer::filesystem().await;
    let client = reqwest::Client::new();

    // First PUT a source object into the real bucket.
    let src_url = format!("{}/{}/source.bin", server.endpoint(), server.bucket());
    client
        .put(&src_url)
        .body(b"source payload".to_vec())
        .send()
        .await
        .unwrap()
        .error_for_status()
        .expect("seed source object");

    // Copy to a ghost destination.
    let ghost_dest_url = format!("{}/ghost-dest-bucket/copied.bin", server.endpoint());
    let resp = client
        .put(&ghost_dest_url)
        .header(
            "x-amz-copy-source",
            format!("/{}/{}", server.bucket(), "source.bin"),
        )
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status().as_u16(), 404);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("<Code>NoSuchBucket</Code>"),
        "Copy to nonexistent bucket should return NoSuchBucket, got: {}",
        body
    );

    // And the ghost dest directory must not exist.
    let data_dir = server.data_dir().expect("fs data dir");
    assert!(
        !data_dir.join("ghost-dest-bucket").exists(),
        "Copy must not implicitly create destination bucket"
    );
}

/// Copy with a SOURCE bucket that doesn't exist must return
/// NoSuchBucket (the source-bucket precheck in `copy_object_inner`).
/// The destination bucket is the real one. Pre-fix the source-side
/// `ensure_bucket_exists` was omitted in some COPY arms.
#[tokio::test]
async fn test_copy_from_nonexistent_source_bucket_returns_nosuchbucket() {
    let server = TestServer::filesystem().await;
    let client = reqwest::Client::new();

    // PUT into the real bucket — destination is fine.
    let dst_url = format!("{}/{}/dst.bin", server.endpoint(), server.bucket());
    let resp = client
        .put(&dst_url)
        .header(
            "x-amz-copy-source",
            "/ghost-source-bucket/anything.bin".to_string(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("<Code>NoSuchBucket</Code>"),
        "Copy from missing source bucket should return NoSuchBucket, got: {}",
        body
    );
}

/// CompleteMultipartUpload when the bucket got deleted between
/// Initiate and Complete must return NoSuchBucket. Pre-fix the
/// engine.store path could silently re-create the filesystem dir
/// (C2 root-cause); the handler-level `ensure_bucket_exists` should
/// short-circuit cleanly.
#[tokio::test]
async fn test_complete_multipart_after_bucket_deletion_returns_nosuchbucket() {
    let server = TestServer::filesystem().await;
    let s3 = server.s3_client().await;
    let bucket = server.bucket().to_string();

    // Initiate on a real bucket.
    let create = s3
        .create_multipart_upload()
        .bucket(&bucket)
        .key("late.bin")
        .send()
        .await
        .expect("initiate");
    let upload_id = create.upload_id().unwrap().to_string();

    // Upload one valid 5 MiB part so we can attempt complete.
    use aws_sdk_s3::primitives::ByteStream;
    let part = s3
        .upload_part()
        .bucket(&bucket)
        .key("late.bin")
        .upload_id(&upload_id)
        .part_number(1)
        .body(ByteStream::from(vec![0u8; 5 * 1024 * 1024]))
        .send()
        .await
        .expect("upload part");
    let etag = part.e_tag().unwrap().to_string();

    // Race condition simulation: remove the bucket directory under
    // the proxy's feet.
    let data_dir = server.data_dir().expect("fs data dir");
    let _ = std::fs::remove_dir_all(data_dir.join(&bucket));

    use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
    let completed = CompletedMultipartUpload::builder()
        .parts(CompletedPart::builder().part_number(1).e_tag(&etag).build())
        .build();

    let res = s3
        .complete_multipart_upload()
        .bucket(&bucket)
        .key("late.bin")
        .upload_id(&upload_id)
        .multipart_upload(completed)
        .send()
        .await;

    assert!(
        res.is_err(),
        "CompleteMultipartUpload on missing bucket must fail (no implicit recreate)"
    );
    let err_msg = format!("{:?}", res.unwrap_err());
    assert!(
        err_msg.contains("NoSuchBucket") || err_msg.contains("404"),
        "C2 REGRESSION: CompleteMultipartUpload silently recreated bucket; got {}",
        err_msg
    );

    // The bucket directory must not have been recreated as a side
    // effect of the complete attempt.
    assert!(
        !data_dir.join(&bucket).exists(),
        "C2 REGRESSION: bucket directory recreated by failed CompleteMultipartUpload"
    );
}
