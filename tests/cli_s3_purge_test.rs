// SPDX-License-Identifier: GPL-3.0-only

//! Integration test for `deltaglider_proxy purge`. Simulates a
//! Python-toolchain rehydration-cache layout by writing two raw S3
//! objects under `.deltaglider/tmp/` — one with a past
//! `dg-expires-at`, one with a future one — and asserts that only the
//! past one is removed.

mod common;

use aws_sdk_s3::primitives::ByteStream;
use common::{minio_endpoint_url, MINIO_ACCESS_KEY, MINIO_SECRET_KEY};
use deltaglider_proxy::cli::purge::{run as purge_run, PurgeArgs};

fn unique_bucket(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("cli-purge-{prefix}-{ts}-{n}")
}

fn purge_args(bucket: &str, dry_run: bool) -> PurgeArgs {
    PurgeArgs {
        bucket: bucket.into(),
        dry_run,
        json: false,
        endpoint_url: Some(minio_endpoint_url()),
        region: Some("us-east-1".into()),
        profile: None,
        access_key_id: Some(MINIO_ACCESS_KEY.into()),
        secret_access_key: Some(MINIO_SECRET_KEY.into()),
        force_path_style: true,
    }
}

#[tokio::test]
async fn purge_removes_only_expired_entries() {
    skip_unless_minio!();
    let bucket = unique_bucket("ok");
    let s3 = common::minio_client().await;
    s3.create_bucket().bucket(&bucket).send().await.unwrap();

    // Seed: one expired (year 2000), one fresh (year 2099).
    s3.put_object()
        .bucket(&bucket)
        .key(".deltaglider/tmp/expired_doc.txt")
        .body(ByteStream::from(b"expired".to_vec()))
        .metadata("dg-expires-at", "2000-01-01T00:00:00Z")
        .send()
        .await
        .unwrap();
    s3.put_object()
        .bucket(&bucket)
        .key(".deltaglider/tmp/fresh_doc.txt")
        .body(ByteStream::from(b"fresh".to_vec()))
        .metadata("dg-expires-at", "2099-12-31T23:59:59Z")
        .send()
        .await
        .unwrap();
    // Also seed an entry with NO expires-at — it must survive.
    s3.put_object()
        .bucket(&bucket)
        .key(".deltaglider/tmp/no_expires_doc.txt")
        .body(ByteStream::from(b"keep".to_vec()))
        .send()
        .await
        .unwrap();

    // Execute.
    assert_eq!(
        purge_run(purge_args(&bucket, false)).await,
        deltaglider_proxy::cli::config::EXIT_OK
    );

    // Expired one should be gone; fresh + no-expires should survive.
    let expired_gone = s3
        .head_object()
        .bucket(&bucket)
        .key(".deltaglider/tmp/expired_doc.txt")
        .send()
        .await
        .is_err();
    let fresh_ok = s3
        .head_object()
        .bucket(&bucket)
        .key(".deltaglider/tmp/fresh_doc.txt")
        .send()
        .await
        .is_ok();
    let no_expires_ok = s3
        .head_object()
        .bucket(&bucket)
        .key(".deltaglider/tmp/no_expires_doc.txt")
        .send()
        .await
        .is_ok();
    assert!(expired_gone, "expired entry should be deleted");
    assert!(fresh_ok, "fresh entry should survive");
    assert!(no_expires_ok, "no-expires entry should survive");

    // Cleanup.
    for k in [
        ".deltaglider/tmp/fresh_doc.txt",
        ".deltaglider/tmp/no_expires_doc.txt",
    ] {
        s3.delete_object().bucket(&bucket).key(k).send().await.ok();
    }
    s3.delete_bucket().bucket(&bucket).send().await.ok();
}

#[tokio::test]
async fn purge_dry_run_does_not_delete() {
    skip_unless_minio!();
    let bucket = unique_bucket("dry");
    let s3 = common::minio_client().await;
    s3.create_bucket().bucket(&bucket).send().await.unwrap();

    s3.put_object()
        .bucket(&bucket)
        .key(".deltaglider/tmp/expired.txt")
        .body(ByteStream::from(b"expired".to_vec()))
        .metadata("dg-expires-at", "2000-01-01T00:00:00Z")
        .send()
        .await
        .unwrap();

    assert_eq!(
        purge_run(purge_args(&bucket, true)).await,
        deltaglider_proxy::cli::config::EXIT_OK
    );

    // Object should still be there.
    assert!(s3
        .head_object()
        .bucket(&bucket)
        .key(".deltaglider/tmp/expired.txt")
        .send()
        .await
        .is_ok());

    s3.delete_object()
        .bucket(&bucket)
        .key(".deltaglider/tmp/expired.txt")
        .send()
        .await
        .ok();
    s3.delete_bucket().bucket(&bucket).send().await.ok();
}
