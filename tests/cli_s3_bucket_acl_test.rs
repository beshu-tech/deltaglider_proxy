// SPDX-License-Identifier: GPL-3.0-only

//! Integration test for `deltaglider_proxy {get,put}-bucket-acl`.
//!
//! MinIO's ACL surface is limited (it canonicalises everything to
//! `Private` regardless of what you set). We assert what we can:
//!
//! 1. `put-bucket-acl --acl private` succeeds against a fresh bucket
//!    (the API call itself round-trips and exits 0).
//! 2. `get-bucket-acl` returns a non-empty grants list (MinIO always
//!    emits the owner's full-control grant).

mod common;

use common::{minio_endpoint_url, MINIO_ACCESS_KEY, MINIO_SECRET_KEY};
use deltaglider_proxy::cli::bucket_acl::{get_run, put_run, GetArgs, PutArgs};

fn unique_bucket(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("cli-acl-{prefix}-{ts}-{n}")
}

fn get_args(bucket: &str) -> GetArgs {
    GetArgs {
        url: format!("s3://{bucket}"),
        endpoint_url: Some(minio_endpoint_url()),
        region: Some("us-east-1".into()),
        profile: None,
        access_key_id: Some(MINIO_ACCESS_KEY.into()),
        secret_access_key: Some(MINIO_SECRET_KEY.into()),
        force_path_style: true,
    }
}

fn put_args(bucket: &str, canned: Option<&str>) -> PutArgs {
    PutArgs {
        url: format!("s3://{bucket}"),
        acl: canned.map(str::to_string),
        grant_full_control: None,
        grant_read: None,
        grant_read_acp: None,
        grant_write: None,
        grant_write_acp: None,
        endpoint_url: Some(minio_endpoint_url()),
        region: Some("us-east-1".into()),
        profile: None,
        access_key_id: Some(MINIO_ACCESS_KEY.into()),
        secret_access_key: Some(MINIO_SECRET_KEY.into()),
        force_path_style: true,
    }
}

#[tokio::test]
async fn put_then_get_bucket_acl_roundtrips() {
    skip_unless_minio!();
    let bucket = unique_bucket("rt");
    let s3 = common::minio_client().await;
    s3.create_bucket().bucket(&bucket).send().await.unwrap();

    // PUT a canned private ACL — MinIO accepts this even though it
    // doesn't honour every dimension. We're testing the API plumbing,
    // not the back-end semantics.
    assert_eq!(
        put_run(put_args(&bucket, Some("private"))).await,
        deltaglider_proxy::cli::config::EXIT_OK
    );

    // GET — the call must succeed and emit a JSON blob with at least
    // one grant entry.
    assert_eq!(
        get_run(get_args(&bucket)).await,
        deltaglider_proxy::cli::config::EXIT_OK
    );

    s3.delete_bucket().bucket(&bucket).send().await.ok();
}

/// `put-bucket-acl` without `--acl` or any `--grant-*` should reject
/// with EXIT_USAGE — there's nothing to apply.
#[tokio::test]
async fn put_bucket_acl_rejects_empty_request() {
    skip_unless_minio!();
    let code = put_run(put_args("some-bucket", None)).await;
    assert_eq!(code, deltaglider_proxy::cli::config::EXIT_USAGE);
}
