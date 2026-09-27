// SPDX-License-Identifier: BUSL-1.1

//! Where a multipart upload keeps its parts. An upload that will be stored
//! passthrough (a key that is not delta-eligible, the `dg-no-delta` hint)
//! never needs its parts assembled in memory, so its parts go to relay files
//! in the spool from the first part. A delta candidate keeps its parts in
//! memory up to the rebuild limit.

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
async fn an_upload_that_tries_no_delta_relays_its_parts_from_the_start() {
    let spool = tempfile::TempDir::new().unwrap();
    let server = TestServer::builder()
        .env("DGP_SPOOL_DIR", &spool.path().display().to_string())
        .build()
        .await;
    let client = server.s3_client().await;
    let bucket = server.bucket();
    let body = vec![0x5Au8; 64 * 1024];

    for (key, hint, relayed) in [
        ("relay/photo.png", false, true),
        ("relay/app.zip", true, true),
        ("relay/other.zip", false, false),
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
            !relay_parts(spool.path(), &upload_id).is_empty(),
            relayed,
            "{key} (hint={hint}): relay files in the spool"
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
