// SPDX-License-Identifier: GPL-3.0-only

//! Integration tests for `deltaglider_proxy cp` against MinIO. Covers
//! the upload happy path and a recursive upload with an exclude
//! filter — enough to pin the direction-detection + filter wiring
//! end-to-end; broader matrix (download, S3-to-S3, dryrun) is
//! covered by the unit tests in `src/cli/cp.rs`.

mod common;

use common::{minio_client, minio_endpoint_url, MINIO_ACCESS_KEY, MINIO_SECRET_KEY};
use deltaglider_proxy::cli::cp::{run, CpArgs};

fn unique_bucket(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("cli-cp-{prefix}-{ts}-{n}")
}

fn default_args(src: String, dst: String) -> CpArgs {
    CpArgs {
        src,
        dst,
        recursive: false,
        include: vec![],
        exclude: vec![],
        dryrun: false,
        no_delta: false,
        max_ratio: None,
        content_type: None,
        metadata: vec![],
        quiet: false,
        endpoint_url: Some(minio_endpoint_url()),
        region: Some("us-east-1".into()),
        profile: None,
        access_key_id: Some(MINIO_ACCESS_KEY.into()),
        secret_access_key: Some(MINIO_SECRET_KEY.into()),
        force_path_style: true,
    }
}

#[tokio::test]
async fn cp_uploads_a_single_local_file_to_s3() {
    skip_unless_minio!();
    let bucket = unique_bucket("upload");
    let s3 = minio_client().await;
    s3.create_bucket().bucket(&bucket).send().await.unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let local_path = tmp.path().join("hello.txt");
    std::fs::write(&local_path, b"hello deltaglider").unwrap();

    let args = default_args(
        local_path.to_string_lossy().to_string(),
        format!("s3://{bucket}/uploads/hello.txt"),
    );
    let code = run(args).await;
    assert_eq!(code, deltaglider_proxy::cli::config::EXIT_OK);

    // Verify via the direct MinIO client through the engine's stored
    // bytes — we don't decode deltas here (the engine handles small
    // text files as passthrough by default), so a direct GET should
    // come back with our original payload.
    let got = s3
        .get_object()
        .bucket(&bucket)
        .key("uploads/hello.txt")
        .send()
        .await
        .unwrap()
        .body
        .collect()
        .await
        .unwrap()
        .into_bytes();
    assert_eq!(&got[..], b"hello deltaglider");

    // Cleanup.
    s3.delete_object()
        .bucket(&bucket)
        .key("uploads/hello.txt")
        .send()
        .await
        .ok();
    s3.delete_bucket().bucket(&bucket).send().await.ok();
}

#[tokio::test]
async fn cp_recursive_upload_respects_exclude_filter() {
    skip_unless_minio!();
    let bucket = unique_bucket("recursive");
    let s3 = minio_client().await;
    s3.create_bucket().bucket(&bucket).send().await.unwrap();

    // Local tree:
    //   tmp/keep-1.txt
    //   tmp/keep-2.txt
    //   tmp/skip.tmp
    //   tmp/sub/keep-3.txt
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("keep-1.txt"), b"k1").unwrap();
    std::fs::write(tmp.path().join("keep-2.txt"), b"k2").unwrap();
    std::fs::write(tmp.path().join("skip.tmp"), b"x").unwrap();
    std::fs::create_dir_all(tmp.path().join("sub")).unwrap();
    std::fs::write(tmp.path().join("sub").join("keep-3.txt"), b"k3").unwrap();

    let mut args = default_args(
        tmp.path().to_string_lossy().to_string(),
        format!("s3://{bucket}/dst/"),
    );
    args.recursive = true;
    args.exclude = vec!["*.tmp".into()];
    let code = run(args).await;
    assert_eq!(code, deltaglider_proxy::cli::config::EXIT_OK);

    // Inspect via MinIO direct: there should be exactly 3 keys, all
    // ending in `.txt`. No `.tmp` survives the filter.
    let listing = s3
        .list_objects_v2()
        .bucket(&bucket)
        .prefix("dst/")
        .send()
        .await
        .unwrap();
    let keys: Vec<String> = listing
        .contents()
        .iter()
        .filter_map(|o| o.key().map(String::from))
        .collect();
    assert_eq!(keys.len(), 3, "expected 3 .txt uploads, got {keys:?}");
    assert!(keys.iter().all(|k| k.ends_with(".txt")), "{keys:?}");

    // Cleanup.
    for k in &keys {
        s3.delete_object().bucket(&bucket).key(k).send().await.ok();
    }
    s3.delete_bucket().bucket(&bucket).send().await.ok();
}
