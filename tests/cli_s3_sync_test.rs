// SPDX-License-Identifier: GPL-3.0-only

//! Integration tests for `deltaglider_proxy sync`.

mod common;

use common::{minio_endpoint_url, MINIO_ACCESS_KEY, MINIO_SECRET_KEY};
use deltaglider_proxy::cli::sync::{run as sync_run, SyncArgs};

fn unique_bucket(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("cli-sync-{prefix}-{ts}-{n}")
}

fn sync_args(src: String, dst: String) -> SyncArgs {
    SyncArgs {
        src,
        dst,
        delete: false,
        dryrun: false,
        size_only: false,
        exact_timestamps: false,
        include: vec![],
        exclude: vec![],
        no_delta: false,
        quiet: true,
        endpoint_url: Some(minio_endpoint_url()),
        region: Some("us-east-1".into()),
        profile: None,
        access_key_id: Some(MINIO_ACCESS_KEY.into()),
        secret_access_key: Some(MINIO_SECRET_KEY.into()),
        force_path_style: true,
    }
}

async fn cleanup(bucket: &str) {
    let s3 = common::minio_client().await;
    if let Ok(out) = s3.list_objects_v2().bucket(bucket).send().await {
        for obj in out.contents() {
            if let Some(k) = obj.key() {
                s3.delete_object().bucket(bucket).key(k).send().await.ok();
            }
        }
    }
    s3.delete_bucket().bucket(bucket).send().await.ok();
}

#[tokio::test]
async fn sync_local_to_s3_uploads_new_files() {
    skip_unless_minio!();
    let bucket = unique_bucket("up");
    let s3 = common::minio_client().await;
    s3.create_bucket().bucket(&bucket).send().await.unwrap();

    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("a.txt"), b"alpha").unwrap();
    std::fs::write(tmp.path().join("b.txt"), b"bravo").unwrap();
    std::fs::create_dir(tmp.path().join("nested")).unwrap();
    std::fs::write(tmp.path().join("nested/c.txt"), b"charlie").unwrap();

    let args = sync_args(
        tmp.path().to_string_lossy().to_string(),
        format!("s3://{bucket}/data/"),
    );
    assert_eq!(
        sync_run(args).await,
        deltaglider_proxy::cli::config::EXIT_OK
    );

    // Verify all three keys present.
    for k in ["data/a.txt", "data/b.txt", "data/nested/c.txt"] {
        let head = s3.head_object().bucket(&bucket).key(k).send().await;
        assert!(head.is_ok(), "missing {k} after upload: {head:?}");
    }

    cleanup(&bucket).await;
}

#[tokio::test]
async fn sync_s3_to_local_downloads_missing_files() {
    skip_unless_minio!();
    let bucket = unique_bucket("down");
    let s3 = common::minio_client().await;
    s3.create_bucket().bucket(&bucket).send().await.unwrap();

    // Seed bucket via direct PUT (skip the engine route since we just
    // want the bytes to land).
    use aws_sdk_s3::primitives::ByteStream;
    s3.put_object()
        .bucket(&bucket)
        .key("data/x.txt")
        .body(ByteStream::from(b"xray".to_vec()))
        .send()
        .await
        .unwrap();
    s3.put_object()
        .bucket(&bucket)
        .key("data/y.txt")
        .body(ByteStream::from(b"yankee".to_vec()))
        .send()
        .await
        .unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let args = sync_args(
        format!("s3://{bucket}/data/"),
        tmp.path().to_string_lossy().to_string(),
    );
    assert_eq!(
        sync_run(args).await,
        deltaglider_proxy::cli::config::EXIT_OK
    );

    let x = std::fs::read(tmp.path().join("x.txt")).unwrap();
    let y = std::fs::read(tmp.path().join("y.txt")).unwrap();
    assert_eq!(x, b"xray");
    assert_eq!(y, b"yankee");

    cleanup(&bucket).await;
}

#[tokio::test]
async fn sync_with_delete_removes_orphans_on_dst() {
    skip_unless_minio!();
    let bucket = unique_bucket("del");
    let s3 = common::minio_client().await;
    s3.create_bucket().bucket(&bucket).send().await.unwrap();

    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("keep.txt"), b"keep").unwrap();

    // Pre-seed an orphan on the destination — sync --delete should
    // remove it.
    use aws_sdk_s3::primitives::ByteStream;
    s3.put_object()
        .bucket(&bucket)
        .key("data/orphan.txt")
        .body(ByteStream::from(b"orphan".to_vec()))
        .send()
        .await
        .unwrap();

    let mut args = sync_args(
        tmp.path().to_string_lossy().to_string(),
        format!("s3://{bucket}/data/"),
    );
    args.delete = true;
    assert_eq!(
        sync_run(args).await,
        deltaglider_proxy::cli::config::EXIT_OK
    );

    // keep.txt should be uploaded, orphan.txt should be gone.
    assert!(s3
        .head_object()
        .bucket(&bucket)
        .key("data/keep.txt")
        .send()
        .await
        .is_ok());
    assert!(s3
        .head_object()
        .bucket(&bucket)
        .key("data/orphan.txt")
        .send()
        .await
        .is_err());

    cleanup(&bucket).await;
}

#[tokio::test]
async fn sync_local_to_local_is_rejected() {
    // No MinIO needed — pure usage-error path.
    let tmp1 = tempfile::tempdir().unwrap();
    let tmp2 = tempfile::tempdir().unwrap();
    let args = sync_args(
        tmp1.path().to_string_lossy().to_string(),
        tmp2.path().to_string_lossy().to_string(),
    );
    assert_eq!(
        sync_run(args).await,
        deltaglider_proxy::cli::config::EXIT_USAGE
    );
}
