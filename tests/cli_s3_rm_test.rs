// SPDX-License-Identifier: GPL-3.0-only

//! Integration tests for `deltaglider_proxy rm` against MinIO.

mod common;

use aws_sdk_s3::primitives::ByteStream;
use common::{minio_client, minio_endpoint_url, MINIO_ACCESS_KEY, MINIO_SECRET_KEY};
use deltaglider_proxy::cli::rm::{run, RmArgs};

fn unique_bucket(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("cli-rm-{prefix}-{ts}-{n}")
}

fn make_args(url: String) -> RmArgs {
    RmArgs {
        url,
        recursive: false,
        include: vec![],
        exclude: vec![],
        dryrun: false,
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
async fn rm_deletes_a_single_key() {
    skip_unless_minio!();
    let bucket = unique_bucket("single");
    let s3 = minio_client().await;
    s3.create_bucket().bucket(&bucket).send().await.unwrap();
    s3.put_object()
        .bucket(&bucket)
        .key("delete-me.txt")
        .body(ByteStream::from(b"bye".to_vec()))
        .send()
        .await
        .unwrap();

    let args = make_args(format!("s3://{bucket}/delete-me.txt"));
    let code = run(args).await;
    assert_eq!(code, deltaglider_proxy::cli::config::EXIT_OK);

    // Verify gone via the direct MinIO client.
    let head = s3
        .head_object()
        .bucket(&bucket)
        .key("delete-me.txt")
        .send()
        .await;
    assert!(head.is_err(), "object should be gone after `rm`");

    s3.delete_bucket().bucket(&bucket).send().await.ok();
}

#[tokio::test]
async fn rm_recursive_with_include_only_touches_matching_keys() {
    skip_unless_minio!();
    let bucket = unique_bucket("recursive");
    let s3 = minio_client().await;
    s3.create_bucket().bucket(&bucket).send().await.unwrap();

    // Mix: 3 .zip, 2 .txt — the include filter should hit only .zip.
    for (i, ext) in [
        ("a", "zip"),
        ("b", "zip"),
        ("c", "zip"),
        ("d", "txt"),
        ("e", "txt"),
    ]
    .iter()
    .enumerate()
    {
        let key = format!("{i}-{}.{}", ext.0, ext.1);
        s3.put_object()
            .bucket(&bucket)
            .key(&key)
            .body(ByteStream::from(format!("body-{i}").into_bytes()))
            .send()
            .await
            .unwrap();
    }

    let mut args = make_args(format!("s3://{bucket}/"));
    args.recursive = true;
    args.include = vec!["*.zip".into()];
    let code = run(args).await;
    assert_eq!(code, deltaglider_proxy::cli::config::EXIT_OK);

    // Three .zip files should be gone, two .txt should still be there.
    let listing = s3.list_objects_v2().bucket(&bucket).send().await.unwrap();
    let remaining: Vec<String> = listing
        .contents()
        .iter()
        .filter_map(|o| o.key().map(String::from))
        .collect();
    assert_eq!(remaining.len(), 2, "only the two .txt files should survive");
    assert!(remaining.iter().all(|k| k.ends_with(".txt")));

    // Cleanup.
    for k in remaining {
        s3.delete_object().bucket(&bucket).key(&k).send().await.ok();
    }
    s3.delete_bucket().bucket(&bucket).send().await.ok();
}
