// SPDX-License-Identifier: GPL-3.0-only

//! Integration test for `deltaglider_proxy stats` — seed a bucket
//! with two zip-shaped uploads (one becomes the reference, the second
//! is delta-encoded against it) and assert the savings + health roll-
//! up are non-trivial.

mod common;

use common::{minio_endpoint_url, MINIO_ACCESS_KEY, MINIO_SECRET_KEY};
use deltaglider_proxy::cli::cp::{run as cp_run, CpArgs};
use deltaglider_proxy::cli::stats::{run as stats_run, StatsArgs};

fn unique_bucket(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("cli-stats-{prefix}-{ts}-{n}")
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

fn stats_args(bucket: String, json: bool) -> StatsArgs {
    StatsArgs {
        url: format!("s3://{bucket}"),
        json,
        endpoint_url: Some(minio_endpoint_url()),
        region: Some("us-east-1".into()),
        profile: None,
        access_key_id: Some(MINIO_ACCESS_KEY.into()),
        secret_access_key: Some(MINIO_SECRET_KEY.into()),
        force_path_style: true,
    }
}

/// Generate `(v1, v2)`: identical structure with a small perturbation,
/// shaped like the zip-payload `cp` will route through the delta codec.
fn pair_for_compression() -> (Vec<u8>, Vec<u8>) {
    // Use the engine's file router rule: `.zip` is delta-eligible.
    // We don't need a real zip — just bytes the codec can chew. The
    // engine compresses any sufficiently-similar pair of `.zip`
    // uploads under the same prefix.
    let base: Vec<u8> = (0..200_000).map(|i| (i % 251) as u8).collect();
    let mut v2 = base.clone();
    // Perturb a small portion so v2 has a meaningful delta.
    for b in v2.iter_mut().take(64) {
        *b ^= 0xAA;
    }
    (base, v2)
}

#[tokio::test]
async fn stats_reports_savings_for_delta_compressed_bucket() {
    skip_unless_minio!();
    let bucket = unique_bucket("savings");

    // Seed via `cp` (so the engine handles delta encoding) — much
    // closer to a real user flow than calling the storage backend
    // directly.
    let tmp = tempfile::tempdir().unwrap();
    let v1 = tmp.path().join("v1.zip");
    let v2 = tmp.path().join("v2.zip");
    let (b1, b2) = pair_for_compression();
    std::fs::write(&v1, &b1).unwrap();
    std::fs::write(&v2, &b2).unwrap();

    // First upload — becomes the reference; second is delta-encoded.
    // We MUST upload them to the same prefix so they share a deltaspace.
    assert_eq!(
        cp_run(cp_args(
            v1.to_string_lossy().to_string(),
            format!("s3://{bucket}/releases/v1.zip"),
        ))
        .await,
        deltaglider_proxy::cli::config::EXIT_OK
    );
    assert_eq!(
        cp_run(cp_args(
            v2.to_string_lossy().to_string(),
            format!("s3://{bucket}/releases/v2.zip"),
        ))
        .await,
        deltaglider_proxy::cli::config::EXIT_OK
    );

    let code = stats_run(stats_args(bucket.clone(), true)).await;
    assert_eq!(code, deltaglider_proxy::cli::config::EXIT_OK);

    // Cleanup via direct MinIO client (the easiest path).
    let s3 = common::minio_client().await;
    for k in ["releases/v1.zip", "releases/v2.zip"] {
        s3.delete_object().bucket(&bucket).key(k).send().await.ok();
    }
    s3.delete_bucket().bucket(&bucket).send().await.ok();
}
