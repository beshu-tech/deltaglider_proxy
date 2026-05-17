// SPDX-License-Identifier: GPL-3.0-only

//! Integration tests for `deltaglider_proxy verify`.

mod common;

use aws_sdk_s3::primitives::ByteStream;
use common::{minio_endpoint_url, MINIO_ACCESS_KEY, MINIO_SECRET_KEY};
use deltaglider_proxy::cli::cp::{run as cp_run, CpArgs};
use deltaglider_proxy::cli::verify::{run as verify_run, VerifyArgs};

fn unique_bucket(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("cli-verify-{prefix}-{ts}-{n}")
}

fn cp_args(src: String, dst: String) -> CpArgs {
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
        quiet: true,
        endpoint_url: Some(minio_endpoint_url()),
        region: Some("us-east-1".into()),
        profile: None,
        access_key_id: Some(MINIO_ACCESS_KEY.into()),
        secret_access_key: Some(MINIO_SECRET_KEY.into()),
        force_path_style: true,
    }
}

fn verify_args(url: String) -> VerifyArgs {
    VerifyArgs {
        url,
        endpoint_url: Some(minio_endpoint_url()),
        region: Some("us-east-1".into()),
        profile: None,
        access_key_id: Some(MINIO_ACCESS_KEY.into()),
        secret_access_key: Some(MINIO_SECRET_KEY.into()),
        force_path_style: true,
    }
}

#[tokio::test]
async fn verify_ok_after_round_trip() {
    skip_unless_minio!();
    let bucket = unique_bucket("ok");

    let tmp = tempfile::tempdir().unwrap();
    let local = tmp.path().join("payload.zip");
    let payload: Vec<u8> = (0..32_768).map(|i| (i % 251) as u8).collect();
    std::fs::write(&local, &payload).unwrap();

    // Upload via cp → engine handles delta routing (passthrough for
    // a single zip; reference if delta-eligible).
    assert_eq!(
        cp_run(cp_args(
            local.to_string_lossy().to_string(),
            format!("s3://{bucket}/payload.zip"),
        ))
        .await,
        deltaglider_proxy::cli::config::EXIT_OK
    );

    let code = verify_run(verify_args(format!("s3://{bucket}/payload.zip"))).await;
    assert_eq!(code, deltaglider_proxy::cli::config::EXIT_OK);

    // Cleanup.
    let s3 = common::minio_client().await;
    s3.delete_object()
        .bucket(&bucket)
        .key("payload.zip")
        .send()
        .await
        .ok();
    s3.delete_bucket().bucket(&bucket).send().await.ok();
}

#[tokio::test]
async fn verify_returns_not_found_for_missing_object() {
    skip_unless_minio!();
    let bucket = unique_bucket("missing");
    let s3 = common::minio_client().await;
    s3.create_bucket().bucket(&bucket).send().await.unwrap();

    // We don't upload anything — verify should return NOT_FOUND.
    let code = verify_run(verify_args(format!("s3://{bucket}/never-uploaded.zip"))).await;
    assert_eq!(code, deltaglider_proxy::cli::config::EXIT_NOT_FOUND);

    s3.delete_bucket().bucket(&bucket).send().await.ok();
}

#[tokio::test]
async fn verify_detects_passthrough_byte_corruption() {
    // Belt-and-suspenders test: upload a `.txt` file (so it's stored
    // as passthrough — no engine-side delta reconstruction), then
    // corrupt a byte directly via the MinIO client. `verify` should
    // catch the SHA256 drift even though no codec ran.
    skip_unless_minio!();
    let bucket = unique_bucket("corrupt");
    let s3 = common::minio_client().await;
    s3.create_bucket().bucket(&bucket).send().await.unwrap();

    let tmp = tempfile::tempdir().unwrap();
    let local = tmp.path().join("doc.txt");
    std::fs::write(&local, b"legitimate-content").unwrap();

    assert_eq!(
        cp_run(cp_args(
            local.to_string_lossy().to_string(),
            format!("s3://{bucket}/doc.txt"),
        ))
        .await,
        deltaglider_proxy::cli::config::EXIT_OK
    );

    // Overwrite the stored bytes (passthrough → direct overwrite is
    // enough). The xattrs / user-metadata that carry `file_sha256`
    // stay tied to the ORIGINAL content, so subsequent verify will
    // mismatch.
    s3.put_object()
        .bucket(&bucket)
        .key("doc.txt")
        .body(ByteStream::from(b"poisoned-content".to_vec()))
        .send()
        .await
        .unwrap();

    let code = verify_run(verify_args(format!("s3://{bucket}/doc.txt"))).await;
    // We allow either INTEGRITY (our SHA recompute) or HTTP (the
    // engine may have noticed the size drift first via its own
    // metadata-vs-bytes invariant). Either signals corruption.
    assert!(
        code == deltaglider_proxy::cli::config::EXIT_INTEGRITY
            || code == deltaglider_proxy::cli::config::EXIT_HTTP,
        "expected INTEGRITY (9) or HTTP (5), got {code}",
    );

    s3.delete_object()
        .bucket(&bucket)
        .key("doc.txt")
        .send()
        .await
        .ok();
    s3.delete_bucket().bucket(&bucket).send().await.ok();
}
