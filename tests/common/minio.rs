// SPDX-License-Identifier: BUSL-1.1

//! MinIO: the test bucket, a direct client, and the `skip_unless_minio!` gate.

use aws_credential_types::Credentials;
use aws_sdk_s3::config::{BehaviorVersion, Region};
use aws_sdk_s3::Client;
use std::time::Duration;

/// MinIO configuration constants
pub const MINIO_BUCKET: &str = "deltaglider-test";

/// A bucket name that is unique per call within this process AND across
/// processes: nanosecond timestamp plus a process-wide counter, so two calls
/// in the same clock tick (parallel tests start within microseconds of each
/// other) still get different names.
pub fn unique_bucket(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::SeqCst);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("{prefix}-{ts}-{n}")
}

/// MinIO endpoint — reads MINIO_ENDPOINT env var, falls back to localhost:9000
pub fn minio_endpoint_url() -> String {
    std::env::var("MINIO_ENDPOINT").unwrap_or_else(|_| "http://localhost:9000".to_string())
}
pub const MINIO_ACCESS_KEY: &str = "minioadmin";
pub const MINIO_SECRET_KEY: &str = "minioadmin";

/// The `s3` CLI connection flags for the test MinIO.
pub fn minio_aws_args() -> deltaglider_proxy::cli::aws_args::AwsArgs {
    deltaglider_proxy::cli::aws_args::AwsArgs {
        endpoint_url: Some(minio_endpoint_url()),
        region: Some("us-east-1".into()),
        profile: None,
        access_key_id: Some(MINIO_ACCESS_KEY.into()),
        secret_access_key: Some(MINIO_SECRET_KEY.into()),
        force_path_style: true,
    }
}

// === MinIO gating ===

/// Create an S3 client pointing directly at MinIO (not through the proxy)
pub async fn minio_client() -> Client {
    let credentials = Credentials::new(MINIO_ACCESS_KEY, MINIO_SECRET_KEY, None, None, "test");
    let config = aws_sdk_s3::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new("us-east-1"))
        .endpoint_url(minio_endpoint_url())
        .credentials_provider(credentials)
        .force_path_style(true)
        .build();
    Client::from_conf(config)
}

/// A bucket or object older than this belongs to a finished run: no test lives this long.
const STALE_AGE_SECS: i64 = 2 * 3600;

/// Tests create buckets and objects and do not delete them (a panic would
/// skip any cleanup anyway), and MinIO's scanner re-crawls all of them for
/// ever. So the first MinIO test of each process deletes what earlier runs
/// left (other buckets, and objects in [`MINIO_BUCKET`]), on its own thread
/// so that no test waits for it.
fn start_stale_bucket_sweep() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        std::thread::spawn(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("bucket sweep runtime");
            rt.block_on(sweep_stale_buckets());
        });
    });
}

async fn sweep_stale_buckets() {
    let client = minio_client().await;
    let Ok(listed) = client.list_buckets().send().await else {
        return;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let mut removed = 0;
    for bucket in listed.buckets() {
        let (Some(name), Some(created)) = (bucket.name(), bucket.creation_date()) else {
            continue;
        };
        if name == MINIO_BUCKET || now - created.secs() < STALE_AGE_SECS {
            continue;
        }
        // MinIO's force delete removes the objects, versions and pending
        // uploads in the same request.
        let deleted = client
            .delete_bucket()
            .bucket(name)
            .customize()
            .mutate_request(|req| {
                req.headers_mut().insert("x-minio-force-delete", "true");
            })
            .send()
            .await;
        if deleted.is_ok() {
            removed += 1;
        }
    }
    if removed > 0 {
        eprintln!("MinIO: deleted {removed} test buckets of earlier runs");
    }
    sweep_stale_objects(&client, now).await;
}

/// The shared bucket stays (the gate checks it); its old objects go.
async fn sweep_stale_objects(client: &Client, now: i64) {
    use aws_sdk_s3::types::{Delete, ObjectIdentifier};
    let mut pages = client
        .list_objects_v2()
        .bucket(MINIO_BUCKET)
        .into_paginator()
        .send();
    while let Some(Ok(page)) = pages.next().await {
        let stale: Vec<ObjectIdentifier> = page
            .contents()
            .iter()
            .filter(|o| {
                o.last_modified()
                    .is_some_and(|t| now - t.secs() >= STALE_AGE_SECS)
            })
            .filter_map(|o| {
                ObjectIdentifier::builder()
                    .set_key(o.key().map(Into::into))
                    .build()
                    .ok()
            })
            .collect();
        if stale.is_empty() {
            continue;
        }
        let Ok(delete) = Delete::builder()
            .set_objects(Some(stale))
            .quiet(true)
            .build()
        else {
            continue;
        };
        let _ = client
            .delete_objects()
            .bucket(MINIO_BUCKET)
            .delete(delete)
            .send()
            .await;
    }
}

/// Check if MinIO is available (TCP probe + HeadBucket with 2s timeout)
pub async fn minio_available() -> bool {
    // Quick TCP check first — parse host:port from endpoint URL
    let endpoint = minio_endpoint_url();
    let addr = endpoint
        .trim_start_matches("http://")
        .trim_start_matches("https://");
    if std::net::TcpStream::connect(addr).is_err() {
        return false;
    }

    let client = minio_client().await;

    // Verify the specific test bucket exists (not just any S3-compatible service)
    let result = tokio::time::timeout(
        Duration::from_secs(2),
        client.head_bucket().bucket(MINIO_BUCKET).send(),
    )
    .await;
    let available = matches!(result, Ok(Ok(_)));
    if available {
        start_stale_bucket_sweep();
    }
    available
}

/// Macro to skip a test if MinIO is not available.
/// Use at the start of any test that requires MinIO.
#[macro_export]
macro_rules! skip_unless_minio {
    () => {
        if !common::minio_available().await {
            eprintln!("MinIO not available, skipping test");
            return;
        }
    };
}
