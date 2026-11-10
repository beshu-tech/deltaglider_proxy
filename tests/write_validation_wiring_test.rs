// SPDX-License-Identifier: BUSL-1.1

//! The request-validation checks of the write verbs are wired in: each test
//! goes red when its check is removed from the verb (epic #139, F25).
//!
//! The pure verdicts (`user_metadata_size_check`, `validate_delete_objects_count`)
//! have unit tests. This file proves that the verbs CALL them, which a unit
//! test of the verdict cannot.

use crate::common::TestServer;
use aws_sdk_s3::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{Delete, MetadataDirective, ObjectIdentifier};

/// Over the 2 KiB user-metadata limit (keys and values count).
fn oversized() -> String {
    "v".repeat(3000)
}

fn code<E: ProvideErrorMetadata, R>(err: &SdkError<E, R>) -> String {
    err.code().unwrap_or("<no code>").to_string()
}

/// CreateMultipartUpload refuses oversized metadata before any part exists.
#[tokio::test]
async fn create_multipart_upload_refuses_oversized_metadata() {
    let server = TestServer::builder().build().await;
    let c = server.s3_client().await;
    let err = c
        .create_multipart_upload()
        .bucket(server.bucket())
        .key("mpu-meta.bin")
        .metadata("big", oversized())
        .send()
        .await
        .expect_err("oversized metadata must be refused");
    assert_eq!(code(&err), "MetadataTooLarge", "{err:?}");

    c.create_multipart_upload()
        .bucket(server.bucket())
        .key("mpu-meta.bin")
        .metadata("small", "v".repeat(1000))
        .send()
        .await
        .expect("metadata under the limit is accepted");
}

/// CopyObject with `REPLACE` refuses oversized replacement metadata, and
/// `COPY` (which takes the source's metadata) is not affected.
#[tokio::test]
async fn copy_object_replace_refuses_oversized_metadata() {
    let server = TestServer::builder().build().await;
    let c = server.s3_client().await;
    let bucket = server.bucket();
    c.put_object()
        .bucket(bucket)
        .key("src.txt")
        .body(ByteStream::from_static(b"source"))
        .send()
        .await
        .unwrap();
    let err = c
        .copy_object()
        .bucket(bucket)
        .key("dst.txt")
        .copy_source(format!("{bucket}/src.txt"))
        .metadata_directive(MetadataDirective::Replace)
        .metadata("big", oversized())
        .send()
        .await
        .expect_err("oversized replacement metadata must be refused");
    assert_eq!(code(&err), "MetadataTooLarge", "{err:?}");

    c.copy_object()
        .bucket(bucket)
        .key("dst.txt")
        .copy_source(format!("{bucket}/src.txt"))
        .metadata_directive(MetadataDirective::Replace)
        .metadata("small", "v".repeat(1000))
        .send()
        .await
        .expect("replacement metadata under the limit is accepted");
}

fn delete_of(count: usize) -> Delete {
    let objects: Vec<ObjectIdentifier> = (0..count)
        .map(|i| {
            ObjectIdentifier::builder()
                .key(format!("k{i}"))
                .build()
                .unwrap()
        })
        .collect();
    Delete::builder()
        .set_objects(Some(objects))
        .quiet(true)
        .build()
        .unwrap()
}

/// DeleteObjects takes at most 1000 keys; 1001 is `InvalidArgument`, 1000 passes.
#[tokio::test]
async fn delete_objects_refuses_more_than_1000_keys() {
    let server = TestServer::builder().build().await;
    let c = server.s3_client().await;
    let err = c
        .delete_objects()
        .bucket(server.bucket())
        .delete(delete_of(1001))
        .send()
        .await
        .expect_err("1001 keys must be refused");
    assert_eq!(code(&err), "InvalidArgument", "{err:?}");

    c.delete_objects()
        .bucket(server.bucket())
        .delete(delete_of(1000))
        .send()
        .await
        .expect("1000 keys is the limit and passes");
}
