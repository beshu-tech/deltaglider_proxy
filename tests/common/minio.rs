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
        // retries-on: MinIO is not the system under test; a transient MinIO
        // error must not fail a run (or panic the probe).
        .build();
    Client::from_conf(config)
}

/// A bucket or object older than this belongs to a finished run: no test lives this long.
const STALE_AGE_SECS: i64 = 2 * 3600;

/// Is a bucket or object with this timestamp (Unix seconds) left by a
/// finished run? A time at or before the Unix epoch is not a real time:
/// MinIO lists a bucket that another test creates at this instant with
/// `0001-01-01T00:00:00Z`, and a sweep that took that for "old" deleted
/// the bucket of a running test (its versioning went with it).
fn is_stale(timestamp_secs: i64, now: i64) -> bool {
    timestamp_secs > 0 && now - timestamp_secs >= STALE_AGE_SECS
}

/// Buckets the sweep never deletes, however old. [`MINIO_BUCKET`] is the gate
/// the harness checks; `deltaglider-data` is the dev bucket that
/// `docker-compose.yml` creates on the same MinIO.
const PROTECTED_BUCKETS: [&str; 2] = [MINIO_BUCKET, "deltaglider-data"];

/// Does the sweep delete this bucket? Pure: unit-tested below.
fn is_sweepable_bucket(name: &str, created_secs: i64, now: i64) -> bool {
    !PROTECTED_BUCKETS.contains(&name) && is_stale(created_secs, now)
}

/// `DGP_TEST_REQUIRE_MINIO` value → must the MinIO tests run? CI sets it, so
/// an unreachable MinIO fails the run instead of skipping every S3 test as a
/// pass. Pure: unit-tested below.
fn minio_required(value: Option<&str>) -> bool {
    super::env_flag_on(value)
}

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
        if !is_sweepable_bucket(name, created.secs(), now) {
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
            .filter(|o| o.last_modified().is_some_and(|t| is_stale(t.secs(), now)))
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

/// Check if MinIO is available (TCP probe + HeadBucket, each with a timeout).
///
/// When `DGP_TEST_REQUIRE_MINIO` is set (CI) an unavailable MinIO PANICS: a
/// skip is a libtest pass, so a dead MinIO would turn every S3 test green
/// without running it.
pub async fn minio_available() -> bool {
    let available = probe_minio().await;
    if !available && minio_required(std::env::var("DGP_TEST_REQUIRE_MINIO").ok().as_deref()) {
        panic!(
            "MinIO is required (DGP_TEST_REQUIRE_MINIO is set) but the bucket {MINIO_BUCKET:?} \
             is not reachable at {}",
            minio_endpoint_url()
        );
    }
    if available {
        start_stale_bucket_sweep();
    }
    available
}

async fn probe_minio() -> bool {
    use std::net::ToSocketAddrs;
    // Quick TCP check first — parse host:port from endpoint URL
    let endpoint = minio_endpoint_url();
    let addr = endpoint
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_end_matches('/');
    // Every resolved address, as `TcpStream::connect` tries them: `localhost`
    // may resolve to `::1` first while MinIO listens on IPv4 only.
    let reachable = addr.to_socket_addrs().is_ok_and(|mut it| {
        it.any(|a| std::net::TcpStream::connect_timeout(&a, Duration::from_secs(2)).is_ok())
    });
    if !reachable {
        return false;
    }

    let client = minio_client().await;

    // Verify the specific test bucket exists (not just any S3-compatible service)
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        client.head_bucket().bucket(MINIO_BUCKET).send(),
    )
    .await;
    matches!(result, Ok(Ok(_)))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sweep_spares_the_protected_buckets() {
        let now = 1_000_000_000;
        let old = now - 10 * 3600;
        assert!(!is_sweepable_bucket(MINIO_BUCKET, old, now));
        assert!(!is_sweepable_bucket("deltaglider-data", old, now));
        assert!(is_sweepable_bucket("some-test-bucket", old, now));
        assert!(!is_sweepable_bucket("some-test-bucket", now - 60, now));
        assert!(
            !is_sweepable_bucket("some-test-bucket", 0, now),
            "epoch is not a time"
        );
    }

    #[test]
    fn minio_required_truth_table() {
        for on in ["1", "true", "yes", " 1 "] {
            assert!(minio_required(Some(on)), "{on:?}");
        }
        for off in ["", "0", "false", "FALSE", "  "] {
            assert!(!minio_required(Some(off)), "{off:?}");
        }
        assert!(!minio_required(None));
    }
}
