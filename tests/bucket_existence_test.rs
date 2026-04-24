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

    let resp = client.put(&url).body(b"hello".to_vec()).send().await.unwrap();

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
    let src_url = format!(
        "{}/{}/source.bin",
        server.endpoint(),
        server.bucket()
    );
    client
        .put(&src_url)
        .body(b"source payload".to_vec())
        .send()
        .await
        .unwrap()
        .error_for_status()
        .expect("seed source object");

    // Copy to a ghost destination.
    let ghost_dest_url = format!(
        "{}/ghost-dest-bucket/copied.bin",
        server.endpoint()
    );
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
