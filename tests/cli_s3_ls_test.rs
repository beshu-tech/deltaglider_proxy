// SPDX-License-Identifier: GPL-3.0-only

//! Integration tests for `deltaglider_proxy ls` against the shared
//! MinIO instance (`MINIO_ENDPOINT`, default localhost:9000). Each
//! test creates its own bucket so cross-test contamination is bounded.

mod common;

use aws_sdk_s3::primitives::ByteStream;
use common::{minio_client, minio_endpoint_url, MINIO_ACCESS_KEY, MINIO_SECRET_KEY};
use deltaglider_proxy::cli::ls::{run, LsArgs};

/// Build a fresh bucket name for one test. Uniqueness comes from the
/// counter + nanosecond timestamp so parallel-running tests don't
/// collide on the same name.
fn unique_bucket(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("cli-ls-{prefix}-{ts}-{n}")
}

/// Common arg shape — endpoint + creds shared by every test.
fn make_args(url: Option<String>) -> LsArgs {
    LsArgs {
        url,
        recursive: false,
        human_readable: false,
        summarize: false,
        page_size: 1000,
        endpoint_url: Some(minio_endpoint_url()),
        region: Some("us-east-1".into()),
        profile: None,
        access_key_id: Some(MINIO_ACCESS_KEY.into()),
        secret_access_key: Some(MINIO_SECRET_KEY.into()),
        force_path_style: true,
    }
}

#[tokio::test]
async fn ls_non_recursive_emits_common_prefixes_and_objects() {
    skip_unless_minio!();
    let bucket = unique_bucket("noncr");
    let s3 = minio_client().await;
    s3.create_bucket().bucket(&bucket).send().await.unwrap();

    // Seed: 1 top-level object + 1 object inside a sub-prefix. The
    // non-recursive `ls` should surface the top-level key directly
    // and the sub-prefix as a `PRE` row.
    s3.put_object()
        .bucket(&bucket)
        .key("top.txt")
        .body(ByteStream::from(b"hi".to_vec()))
        .send()
        .await
        .unwrap();
    s3.put_object()
        .bucket(&bucket)
        .key("sub/nested.txt")
        .body(ByteStream::from(b"hello".to_vec()))
        .send()
        .await
        .unwrap();

    // We just exercise the success path; we don't capture stdout in
    // this MVP (would require redirecting tokio's println!). Hooking
    // a stdout-capture into the per-command runner is a follow-up
    // refinement — for now we trust the format unit tests in
    // `src/cli/ls.rs` and assert the exit code.
    let code = run(make_args(Some(format!("s3://{bucket}")))).await;
    assert_eq!(code, deltaglider_proxy::cli::config::EXIT_OK);

    // Cleanup
    s3.delete_object()
        .bucket(&bucket)
        .key("top.txt")
        .send()
        .await
        .ok();
    s3.delete_object()
        .bucket(&bucket)
        .key("sub/nested.txt")
        .send()
        .await
        .ok();
    s3.delete_bucket().bucket(&bucket).send().await.ok();
}

#[tokio::test]
async fn ls_recursive_walks_all_pages() {
    skip_unless_minio!();
    let bucket = unique_bucket("recursive");
    let s3 = minio_client().await;
    s3.create_bucket().bucket(&bucket).send().await.unwrap();

    // Seed: 5 objects across two sub-prefixes — `-r` should see them
    // all without surfacing CommonPrefixes.
    for i in 0..5 {
        let key = format!("sub{}/file-{i}.txt", i % 2);
        s3.put_object()
            .bucket(&bucket)
            .key(&key)
            .body(ByteStream::from(format!("body-{i}").into_bytes()))
            .send()
            .await
            .unwrap();
    }

    let mut args = make_args(Some(format!("s3://{bucket}")));
    args.recursive = true;
    args.summarize = true;
    args.page_size = 2; // force pagination
    let code = run(args).await;
    assert_eq!(code, deltaglider_proxy::cli::config::EXIT_OK);

    // Cleanup
    for i in 0..5 {
        let key = format!("sub{}/file-{i}.txt", i % 2);
        s3.delete_object()
            .bucket(&bucket)
            .key(&key)
            .send()
            .await
            .ok();
    }
    s3.delete_bucket().bucket(&bucket).send().await.ok();
}
