// SPDX-License-Identifier: GPL-3.0-only

//! Integration tests for `deltaglider_proxy migrate`.

mod common;

use common::{minio_endpoint_url, MINIO_ACCESS_KEY, MINIO_SECRET_KEY};
use deltaglider_proxy::cli::cp::{run as cp_run, CpArgs};
use deltaglider_proxy::cli::migrate::{run as migrate_run, MigrateArgs};

fn unique_bucket(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("cli-migrate-{prefix}-{ts}-{n}")
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
        max_object_size_mb: None,
    }
}

fn migrate_args(src: String, dst: String) -> MigrateArgs {
    MigrateArgs {
        src,
        dst,
        include: vec![],
        exclude: vec![],
        no_preserve_prefix: false,
        dry_run: false,
        yes: true, // skip the prompt in tests
        max_ratio: None,
        no_delta: false,
        quiet: true,
        endpoint_url: Some(minio_endpoint_url()),
        source_endpoint_url: None,
        region: Some("us-east-1".into()),
        profile: None,
        access_key_id: Some(MINIO_ACCESS_KEY.into()),
        secret_access_key: Some(MINIO_SECRET_KEY.into()),
        force_path_style: true,
        max_object_size_mb: None,
    }
}

async fn cleanup(bucket: &str) {
    let s3 = common::minio_client().await;
    // Empty the bucket the rough way — list everything, delete each key.
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
async fn migrate_dry_run_lists_without_copying() {
    skip_unless_minio!();
    let src_bucket = unique_bucket("src");
    let dst_bucket = unique_bucket("dst");
    let s3 = common::minio_client().await;
    // Create BOTH buckets — cp doesn't auto-create the source bucket.
    s3.create_bucket().bucket(&src_bucket).send().await.unwrap();
    s3.create_bucket().bucket(&dst_bucket).send().await.unwrap();

    // Seed src via cp.
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("doc.txt");
    std::fs::write(&f, b"hello migrate").unwrap();
    assert_eq!(
        cp_run(cp_args(
            f.to_string_lossy().to_string(),
            format!("s3://{src_bucket}/releases/doc.txt"),
        ))
        .await,
        deltaglider_proxy::cli::config::EXIT_OK
    );

    // Dry run — no copies should happen.
    let mut args = migrate_args(
        format!("s3://{src_bucket}/releases/"),
        format!("s3://{dst_bucket}/backup/"),
    );
    args.dry_run = true;
    args.quiet = false; // we want the planning output
    assert_eq!(
        migrate_run(args).await,
        deltaglider_proxy::cli::config::EXIT_OK
    );

    // Verify dst is still empty.
    let listed = s3
        .list_objects_v2()
        .bucket(&dst_bucket)
        .send()
        .await
        .unwrap();
    assert_eq!(
        listed.key_count().unwrap_or(0),
        0,
        "dry-run leaked writes onto destination"
    );

    cleanup(&src_bucket).await;
    cleanup(&dst_bucket).await;
}

#[tokio::test]
async fn migrate_copies_with_preserve_prefix() {
    skip_unless_minio!();
    let src_bucket = unique_bucket("src");
    let dst_bucket = unique_bucket("dst");
    let s3 = common::minio_client().await;
    // Create BOTH buckets — cp doesn't auto-create the source bucket.
    s3.create_bucket().bucket(&src_bucket).send().await.unwrap();
    s3.create_bucket().bucket(&dst_bucket).send().await.unwrap();

    // Seed src.
    let tmp = tempfile::tempdir().unwrap();
    let f = tmp.path().join("doc.txt");
    std::fs::write(&f, b"hello migrate").unwrap();
    assert_eq!(
        cp_run(cp_args(
            f.to_string_lossy().to_string(),
            format!("s3://{src_bucket}/releases/doc.txt"),
        ))
        .await,
        deltaglider_proxy::cli::config::EXIT_OK
    );

    // Migrate with preserve_prefix (default). The source prefix is
    // `releases/`, dst is `backup/` → effective dst prefix
    // `backup/releases/`. Object should land at
    // `s3://dst/backup/releases/doc.txt`.
    let args = migrate_args(
        format!("s3://{src_bucket}/releases/"),
        format!("s3://{dst_bucket}/backup/"),
    );
    assert_eq!(
        migrate_run(args).await,
        deltaglider_proxy::cli::config::EXIT_OK
    );

    // Assert destination key exists.
    let head = s3
        .head_object()
        .bucket(&dst_bucket)
        .key("backup/releases/doc.txt")
        .send()
        .await;
    assert!(
        head.is_ok(),
        "expected backup/releases/doc.txt on dst, got {head:?}"
    );

    cleanup(&src_bucket).await;
    cleanup(&dst_bucket).await;
}

// ════════════════════════════════════════════════════════════════════
// Resume after partial completion
// ════════════════════════════════════════════════════════════════════
//
// The migrate command's headline feature is that it skips
// already-copied keys on re-run — what saved us during the
// Hetzner→AWS migration earlier in this session. We exercise it by
// simulating a partially-completed migration: seed the source with
// 3 files, pre-populate the destination with one of them as if a
// prior migrate run had landed it, then run migrate and assert the
// already-present file isn't re-copied (its sentinel body survives).

#[tokio::test]
async fn migrate_skips_already_present_objects_on_resume() {
    skip_unless_minio!();
    let src_bucket = unique_bucket("src");
    let dst_bucket = unique_bucket("dst");
    let s3 = common::minio_client().await;
    s3.create_bucket().bucket(&src_bucket).send().await.unwrap();
    s3.create_bucket().bucket(&dst_bucket).send().await.unwrap();

    // Seed source with 3 files under `releases/`.
    let tmp = tempfile::tempdir().unwrap();
    for name in &["one.txt", "two.txt", "three.txt"] {
        let f = tmp.path().join(name);
        std::fs::write(&f, format!("contents of {name}").as_bytes()).unwrap();
        assert_eq!(
            cp_run(cp_args(
                f.to_string_lossy().to_string(),
                format!("s3://{src_bucket}/releases/{name}"),
            ))
            .await,
            deltaglider_proxy::cli::config::EXIT_OK
        );
    }

    // Simulate a previously-completed copy of `one.txt` by directly
    // PUTing it onto the destination at the same effective prefix
    // (`backup/releases/one.txt`) but with a SENTINEL body. If
    // migrate re-copies it, the body changes; if migrate skips it
    // (the correct resume behavior), the sentinel survives.
    use aws_sdk_s3::primitives::ByteStream;
    let sentinel: &[u8] = b"SENTINEL_PREVIOUSLY_MIGRATED";
    s3.put_object()
        .bucket(&dst_bucket)
        .key("backup/releases/one.txt")
        .body(ByteStream::from(sentinel.to_vec()))
        .send()
        .await
        .unwrap();

    // Run migrate. Default `--yes` is set in `migrate_args`.
    let args = migrate_args(
        format!("s3://{src_bucket}/releases/"),
        format!("s3://{dst_bucket}/backup/"),
    );
    assert_eq!(
        migrate_run(args).await,
        deltaglider_proxy::cli::config::EXIT_OK
    );

    // Assert that `one.txt` on dst still contains the sentinel (NOT
    // the source's content) — migrate must have skipped it.
    let one_body = s3
        .get_object()
        .bucket(&dst_bucket)
        .key("backup/releases/one.txt")
        .send()
        .await
        .unwrap()
        .body
        .collect()
        .await
        .unwrap()
        .into_bytes();
    assert_eq!(
        &one_body[..],
        sentinel,
        "migrate must skip already-present destination keys on resume; \
         instead it overwrote one.txt with source content"
    );

    // Assert that `two.txt` and `three.txt` DID land — these were
    // missing from the destination, so the resume must have copied them.
    for name in &["two.txt", "three.txt"] {
        let head = s3
            .head_object()
            .bucket(&dst_bucket)
            .key(format!("backup/releases/{name}"))
            .send()
            .await;
        assert!(
            head.is_ok(),
            "migrate must copy missing key {name}; head got {head:?}"
        );
    }

    cleanup(&src_bucket).await;
    cleanup(&dst_bucket).await;
}
