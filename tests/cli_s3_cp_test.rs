// SPDX-License-Identifier: GPL-3.0-only

//! Integration tests for `deltaglider_proxy s3 cp` against MinIO.
//!
//! The QA pyramid here is intentional:
//! - Bottom (unit tests in `src/cli/cp.rs`): pure decision functions
//!   — direction detection, metadata-flag parsing, dst-path resolution,
//!   exit-code derivation.
//! - Middle (this file): each direction (upload, download, S3→S3),
//!   each major optional flag (recursive+exclude, --no-delta, --dryrun)
//!   exercised end-to-end against a real MinIO so the engine →
//!   storage → wire path is locked in.
//! - Top (binary-spawn tests): see `tests/cli_admin_test.rs` style
//!   if/when we add binary-level smoke for the s3 subgroup.

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
        max_object_size_mb: None,
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

// ════════════════════════════════════════════════════════════════════
// Direction: S3 → local (download)
// ════════════════════════════════════════════════════════════════════

/// `cp s3://bucket/key local.zip` round-trips the bytes through the
/// engine's GET path. Exercises `download_one` + reference reconstruction
/// (small files go passthrough, so this is the simpler reference-free
/// path).
#[tokio::test]
async fn cp_downloads_a_single_s3_object_to_local() {
    skip_unless_minio!();
    let bucket = unique_bucket("download");
    let s3 = minio_client().await;
    s3.create_bucket().bucket(&bucket).send().await.unwrap();

    // Seed the bucket via cp upload (exercising the same metadata
    // shape the proxy would emit).
    let tmp = tempfile::tempdir().unwrap();
    let upload_path = tmp.path().join("payload.bin");
    let payload: Vec<u8> = (0..4096).map(|i| (i % 251) as u8).collect();
    std::fs::write(&upload_path, &payload).unwrap();

    let upload_args = default_args(
        upload_path.to_string_lossy().to_string(),
        format!("s3://{bucket}/data/payload.bin"),
    );
    assert_eq!(
        run(upload_args).await,
        deltaglider_proxy::cli::config::EXIT_OK
    );

    // Now download to a NEW local path.
    let download_path = tmp.path().join("downloaded.bin");
    let download_args = default_args(
        format!("s3://{bucket}/data/payload.bin"),
        download_path.to_string_lossy().to_string(),
    );
    assert_eq!(
        run(download_args).await,
        deltaglider_proxy::cli::config::EXIT_OK
    );

    // Bytes must match exactly — this is the canary for any encoding
    // drift in the GET path.
    let downloaded = std::fs::read(&download_path).unwrap();
    assert_eq!(downloaded, payload, "downloaded bytes do not match upload");

    // Cleanup.
    s3.delete_object()
        .bucket(&bucket)
        .key("data/payload.bin")
        .send()
        .await
        .ok();
    s3.delete_bucket().bucket(&bucket).send().await.ok();
}

// ════════════════════════════════════════════════════════════════════
// Direction: S3 → S3 (cross-bucket copy)
// ════════════════════════════════════════════════════════════════════

/// `cp s3://src/key s3://dst/key` delegates to `migrate_s3_to_s3`
/// internally. Pins the dispatch path (Direction::S3ToS3) and the
/// observable outcome (object lands on dst with original bytes).
#[tokio::test]
async fn cp_copies_between_two_s3_locations() {
    skip_unless_minio!();
    let src_bucket = unique_bucket("s3src");
    let dst_bucket = unique_bucket("s3dst");
    let s3 = minio_client().await;
    s3.create_bucket().bucket(&src_bucket).send().await.unwrap();
    s3.create_bucket().bucket(&dst_bucket).send().await.unwrap();

    // Seed src via upload.
    let tmp = tempfile::tempdir().unwrap();
    let payload_path = tmp.path().join("data.bin");
    let payload = b"cross-bucket payload";
    std::fs::write(&payload_path, payload).unwrap();
    let seed = default_args(
        payload_path.to_string_lossy().to_string(),
        format!("s3://{src_bucket}/releases/data.bin"),
    );
    assert_eq!(run(seed).await, deltaglider_proxy::cli::config::EXIT_OK);

    // S3 → S3 copy.
    let copy_args = default_args(
        format!("s3://{src_bucket}/releases/data.bin"),
        format!("s3://{dst_bucket}/backup/data.bin"),
    );
    assert_eq!(
        run(copy_args).await,
        deltaglider_proxy::cli::config::EXIT_OK
    );

    // Verify dst object exists with the original bytes.
    let got = s3
        .get_object()
        .bucket(&dst_bucket)
        .key("backup/data.bin")
        .send()
        .await
        .expect("dst object should exist after S3→S3 cp")
        .body
        .collect()
        .await
        .unwrap()
        .into_bytes();
    assert_eq!(&got[..], payload);

    // Cleanup.
    s3.delete_object()
        .bucket(&src_bucket)
        .key("releases/data.bin")
        .send()
        .await
        .ok();
    s3.delete_object()
        .bucket(&dst_bucket)
        .key("backup/data.bin")
        .send()
        .await
        .ok();
    s3.delete_bucket().bucket(&src_bucket).send().await.ok();
    s3.delete_bucket().bucket(&dst_bucket).send().await.ok();
}

// ════════════════════════════════════════════════════════════════════
// `--content-type` flag is forwarded to engine.store
// ════════════════════════════════════════════════════════════════════

/// `--content-type "application/octet-stream"` flows through to
/// `engine.store(..., content_type, meta)` and lands on the stored
/// object as the `content-type` response header. Exercises the cp →
/// engine content-type plumbing — a regression here would cause
/// downloaders to receive `application/x-www-form-urlencoded` or
/// `binary/octet-stream` defaults and misroute file handlers.
#[tokio::test]
async fn cp_content_type_flag_forwarded_to_storage() {
    skip_unless_minio!();
    let bucket = unique_bucket("ctype");
    let s3 = minio_client().await;
    s3.create_bucket().bucket(&bucket).send().await.unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let local = tmp.path().join("payload.bin");
    std::fs::write(&local, b"binary payload").unwrap();

    let mut args = default_args(
        local.to_string_lossy().to_string(),
        format!("s3://{bucket}/files/payload.bin"),
    );
    args.content_type = Some("application/vnd.deltaglider-test".to_string());
    assert_eq!(run(args).await, deltaglider_proxy::cli::config::EXIT_OK);

    // HEAD the stored object via direct S3 client. A non-delta-eligible
    // extension (`.bin`) flows through the engine's passthrough path,
    // so the object lands at the literal key path. We check both the
    // object's HTTP content-type AND any user-metadata fields the
    // engine may have set, since the engine's choice of where to
    // persist content-type has shifted over versions.
    let head = s3
        .head_object()
        .bucket(&bucket)
        .key("files/payload.bin")
        .send()
        .await
        .expect("passthrough object must exist after upload");

    let http_ct = head.content_type().unwrap_or("").to_string();
    let user_meta = head.metadata().cloned().unwrap_or_default();
    let user_ct = user_meta.iter().find_map(|(k, v)| {
        if k.eq_ignore_ascii_case("dg-content-type") || k.eq_ignore_ascii_case("content-type") {
            Some(v.clone())
        } else {
            None
        }
    });
    // Accept either persistence shape (object content-type OR
    // user-meta) — both are valid implementations of the contract
    // "the operator-supplied content-type is preserved." Test will
    // pinpoint a regression where neither carries it.
    let found = http_ct == "application/vnd.deltaglider-test"
        || user_ct.as_deref() == Some("application/vnd.deltaglider-test");
    assert!(
        found,
        "--content-type must persist somewhere; http_ct={http_ct:?}, user_meta={user_meta:?}"
    );

    // Cleanup.
    let listing = s3
        .list_objects_v2()
        .bucket(&bucket)
        .prefix("files/")
        .send()
        .await
        .unwrap();
    for o in listing.contents() {
        if let Some(k) = o.key() {
            s3.delete_object().bucket(&bucket).key(k).send().await.ok();
        }
    }
    s3.delete_bucket().bucket(&bucket).send().await.ok();
}

// ════════════════════════════════════════════════════════════════════
// `--dryrun` plans without writing
// ════════════════════════════════════════════════════════════════════

/// `--dryrun` must print the planned operation but make no S3 writes.
/// The destination bucket starts empty and must remain empty after
/// the dryrun.
#[tokio::test]
async fn cp_dryrun_does_not_write_to_destination() {
    skip_unless_minio!();
    let bucket = unique_bucket("dryrun");
    let s3 = minio_client().await;
    s3.create_bucket().bucket(&bucket).send().await.unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let local = tmp.path().join("source.txt");
    std::fs::write(&local, b"this content must not land on s3").unwrap();

    let mut args = default_args(
        local.to_string_lossy().to_string(),
        format!("s3://{bucket}/planned/source.txt"),
    );
    args.dryrun = true;
    assert_eq!(run(args).await, deltaglider_proxy::cli::config::EXIT_OK);

    // Verify dst is still empty — no writes leaked through.
    let listing = s3
        .list_objects_v2()
        .bucket(&bucket)
        .prefix("planned/")
        .send()
        .await
        .unwrap();
    assert_eq!(
        listing.key_count().unwrap_or(0),
        0,
        "--dryrun leaked writes onto destination"
    );

    // Cleanup.
    s3.delete_bucket().bucket(&bucket).send().await.ok();
}
