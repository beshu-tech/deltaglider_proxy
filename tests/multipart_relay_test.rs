// SPDX-License-Identifier: BUSL-1.1

//! Where a multipart upload keeps its parts: every part, of every upload,
//! goes from the request straight to a relay file in the spool. A delta
//! candidate is joined from its relay files at completion and stored as a
//! delta; the delta cap decides only whether a delta is tried.

use crate::common;

use aws_sdk_s3::primitives::ByteStream;
use common::TestServer;
use std::path::{Path, PathBuf};

/// Relay part files of `upload_id` under `spool` (any process root).
fn relay_parts(spool: &Path, upload_id: &str) -> Vec<PathBuf> {
    let root = spool.join("deltaglider-mpu-relay");
    let Ok(pids) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    pids.flatten()
        .filter_map(|pid| std::fs::read_dir(pid.path().join(upload_id)).ok())
        .flat_map(|dir| dir.flatten().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "bin"))
        .collect()
}

#[tokio::test]
async fn every_upload_relays_its_parts_from_the_first_part() {
    let spool = tempfile::TempDir::new().unwrap();
    let server = TestServer::builder()
        .env("DGP_SPOOL_DIR", &spool.path().display().to_string())
        .build()
        .await;
    let client = server.s3_client().await;
    let bucket = server.bucket();
    let body = vec![0x5Au8; 64 * 1024];

    for (key, hint) in [
        ("relay/photo.png", false),
        ("relay/app.zip", true),
        ("relay/other.zip", false),
    ] {
        let mut create = client.create_multipart_upload().bucket(bucket).key(key);
        if hint {
            create = create.metadata("dg-no-delta", "true");
        }
        let upload_id = create
            .send()
            .await
            .expect("create")
            .upload_id()
            .unwrap()
            .to_string();
        let part = client
            .upload_part()
            .bucket(bucket)
            .key(key)
            .upload_id(&upload_id)
            .part_number(1)
            .body(ByteStream::from(body.clone()))
            .send()
            .await
            .expect("upload part");
        assert_eq!(
            relay_parts(spool.path(), &upload_id).len(),
            1,
            "{key} (hint={hint}): one relay file in the spool"
        );

        let done = aws_sdk_s3::types::CompletedMultipartUpload::builder()
            .parts(
                aws_sdk_s3::types::CompletedPart::builder()
                    .part_number(1)
                    .e_tag(part.e_tag().unwrap())
                    .build(),
            )
            .build();
        client
            .complete_multipart_upload()
            .bucket(bucket)
            .key(key)
            .upload_id(&upload_id)
            .multipart_upload(done)
            .send()
            .await
            .expect("complete");
        let got = client
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
            .into_bytes();
        assert_eq!(got.as_ref(), body.as_slice(), "{key} reads back");
        assert!(
            relay_parts(spool.path(), &upload_id).is_empty(),
            "{key}: completion removes the relay files"
        );
    }
}

fn noise(seed: u64, n: usize) -> Vec<u8> {
    let mut x = seed;
    (0..n)
        .map(|_| {
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 56) as u8
        })
        .collect()
}

/// The delta cap decides only whether a delta is tried, never how much RAM an
/// upload holds: with a cap far above the upload, the parts of a small and a
/// large delta candidate go to relay files, and the completed object is
/// still stored as a delta.
#[tokio::test]
async fn a_delta_candidate_relays_its_parts_and_still_becomes_a_delta() {
    let spool = tempfile::TempDir::new().unwrap();
    let server = TestServer::builder()
        .env("DGP_SPOOL_DIR", &spool.path().display().to_string())
        .env("DGP_MPU_DELTA_RECONSTRUCT_MAX_BYTES", "1073741824")
        .build()
        .await;
    let client = server.s3_client().await;
    let bucket = server.bucket();

    let base = noise(11, 300_000);
    client
        .put_object()
        .bucket(bucket)
        .key("relay/base.zip")
        .body(ByteStream::from(base.clone()))
        .send()
        .await
        .expect("baseline");
    let mut variant = base.clone();
    variant[1000..1100].fill(0xAB);

    // One part of 300 KB, and three of 100 KB.
    for (key, part_size) in [("relay/one.zip", 300_000), ("relay/three.zip", 100_000)] {
        let upload_id = client
            .create_multipart_upload()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .expect("create")
            .upload_id()
            .unwrap()
            .to_string();
        let mut completed = Vec::new();
        for (i, chunk) in variant.chunks(part_size).enumerate() {
            let n = i as i32 + 1;
            let part = client
                .upload_part()
                .bucket(bucket)
                .key(key)
                .upload_id(&upload_id)
                .part_number(n)
                .body(ByteStream::from(chunk.to_vec()))
                .send()
                .await
                .expect("upload part");
            completed.push(
                aws_sdk_s3::types::CompletedPart::builder()
                    .part_number(n)
                    .e_tag(part.e_tag().unwrap())
                    .build(),
            );
        }
        assert_eq!(
            relay_parts(spool.path(), &upload_id).len(),
            completed.len(),
            "{key}: every part is a relay file, none is kept in memory"
        );
        client
            .complete_multipart_upload()
            .bucket(bucket)
            .key(key)
            .upload_id(&upload_id)
            .multipart_upload(
                aws_sdk_s3::types::CompletedMultipartUpload::builder()
                    .set_parts(Some(completed))
                    .build(),
            )
            .send()
            .await
            .expect("complete");

        let head = client
            .head_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .expect("head");
        let meta = head.metadata().cloned().unwrap_or_default();
        assert_eq!(
            meta.get("dg-note").map(String::as_str),
            Some("delta"),
            "{key}: the joined upload goes through the streaming delta store: {meta:?}"
        );
        assert!(!meta.contains_key("dg-passthrough-reason"), "{meta:?}");
        let got = client
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
            .into_bytes();
        assert_eq!(got.as_ref(), variant.as_slice(), "{key}");
        assert!(
            relay_parts(spool.path(), &upload_id).is_empty(),
            "{key}: completion removes the relay files"
        );
    }
}

/// An UploadPart larger than the spool threshold, and an UploadPartCopy of
/// a whole source larger than it, each become one relay file, and the
/// completed object reads back whole.
#[tokio::test]
async fn large_parts_and_part_copies_become_relay_files() {
    let spool = tempfile::TempDir::new().unwrap();
    let server = TestServer::builder()
        .env("DGP_SPOOL_DIR", &spool.path().display().to_string())
        .env("DGP_SPOOL_THRESHOLD_BYTES", "65536")
        .build()
        .await;
    let client = server.s3_client().await;
    let bucket = server.bucket();
    let source = noise(5, 3 * 65_536 + 17);
    client
        .put_object()
        .bucket(bucket)
        .key("big/source.png")
        .body(ByteStream::from(source.clone()))
        .send()
        .await
        .expect("source");

    let key = "big/target.png";
    let upload_id = client
        .create_multipart_upload()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .expect("create")
        .upload_id()
        .unwrap()
        .to_string();
    let part = client
        .upload_part()
        .bucket(bucket)
        .key(key)
        .upload_id(&upload_id)
        .part_number(1)
        .body(ByteStream::from(source.clone()))
        .send()
        .await
        .expect("upload part");
    let copied = client
        .upload_part_copy()
        .bucket(bucket)
        .key(key)
        .upload_id(&upload_id)
        .part_number(2)
        .copy_source(format!("{bucket}/big/source.png"))
        .send()
        .await
        .expect("upload part copy");
    assert_eq!(relay_parts(spool.path(), &upload_id).len(), 2);
    let e_tag = |e: Option<&str>| e.unwrap().to_string();
    client
        .complete_multipart_upload()
        .bucket(bucket)
        .key(key)
        .upload_id(&upload_id)
        .multipart_upload(
            aws_sdk_s3::types::CompletedMultipartUpload::builder()
                .parts(
                    aws_sdk_s3::types::CompletedPart::builder()
                        .part_number(1)
                        .e_tag(e_tag(part.e_tag()))
                        .build(),
                )
                .parts(
                    aws_sdk_s3::types::CompletedPart::builder()
                        .part_number(2)
                        .e_tag(e_tag(copied.copy_part_result().and_then(|r| r.e_tag())))
                        .build(),
                )
                .build(),
        )
        .send()
        .await
        .expect("complete");
    let got = client
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
        .into_bytes();
    let mut whole = source.clone();
    whole.extend_from_slice(&source);
    assert_eq!(got.as_ref(), whole.as_slice());
    assert!(relay_parts(spool.path(), &upload_id).is_empty());
}

/// HeadObject names why an object was stored whole.
#[tokio::test]
async fn head_object_says_why_an_object_was_stored_whole() {
    let server = TestServer::builder().build().await;
    let client = server.s3_client().await;
    let bucket = server.bucket();
    for (key, hint, expected) in [
        ("why/photo.png", false, "not-delta-eligible"),
        ("why/app.zip", true, "no-delta-requested"),
    ] {
        let mut put = client
            .put_object()
            .bucket(bucket)
            .key(key)
            .body(ByteStream::from(noise(3, 4096)));
        if hint {
            put = put.metadata("dg-no-delta", "true");
        }
        put.send().await.expect("put");
        let head = client
            .head_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .expect("head");
        let meta = head.metadata().cloned().unwrap_or_default();
        assert_eq!(meta.get("dg-note").map(String::as_str), Some("passthrough"));
        assert_eq!(
            meta.get("dg-passthrough-reason").map(String::as_str),
            Some(expected),
            "{key}: {meta:?}"
        );
    }
}
