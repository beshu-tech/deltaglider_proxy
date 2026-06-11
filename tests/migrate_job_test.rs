// SPDX-License-Identifier: GPL-3.0-only

//! Integration tests for `kind = "migrate"` maintenance jobs — the durable,
//! resumable, write-gated replacement for the old synchronous bucket
//! migration: gate 503s writes during the copy (the stale-copy race fix),
//! reads stay up, the route flips + persists on success, transients never
//! leak, and pre-flip cancellation leaves the source authoritative.
//!
//! Two filesystem backends — no MinIO needed.

mod common;

use common::{admin_http_client, get_bytes, put_object, TestServer};

const MARKER: &[u8] = b"MIGRATE_TEST_MARKER_0123456789";

fn two_backend_yaml(dir_a: &std::path::Path, dir_b: &std::path::Path) -> String {
    format!(
        concat!(
            "backends:\n",
            "  - name: src\n",
            "    type: filesystem\n",
            "    path: \"{}\"\n",
            "  - name: dst\n",
            "    type: filesystem\n",
            "    path: \"{}\"\n",
            "default_backend: src\n",
        ),
        dir_a.display(),
        dir_b.display()
    )
}

async fn seed(http: &reqwest::Client, endpoint: &str, bucket: &str, n: usize) {
    for i in 0..n {
        let body = [MARKER, format!(" object {i}").as_bytes()].concat();
        put_object(
            http,
            endpoint,
            bucket,
            &format!("obj-{i:03}.json"),
            body,
            "application/json",
        )
        .await;
    }
}

async fn start_migrate(
    admin: &reqwest::Client,
    endpoint: &str,
    bucket: &str,
    target: &str,
    delete_source: bool,
) -> reqwest::Response {
    admin
        .post(format!("{endpoint}/_/api/admin/buckets/{bucket}/migrate"))
        .json(&serde_json::json!({ "target_backend": target, "delete_source": delete_source }))
        .send()
        .await
        .expect("migrate POST failed")
}

async fn wait_job_done(admin: &reqwest::Client, endpoint: &str, bucket: &str) {
    for _ in 0..600 {
        let v: serde_json::Value = admin
            .get(format!("{endpoint}/_/api/admin/jobs/bucket/{bucket}"))
            .send()
            .await
            .expect("status GET failed")
            .json()
            .await
            .expect("status not JSON");
        if v["active"].is_null() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("migrate job on '{bucket}' did not finish within 60s");
}

async fn newest_job(admin: &reqwest::Client, endpoint: &str) -> serde_json::Value {
    let v: serde_json::Value = admin
        .get(format!("{endpoint}/_/api/admin/jobs"))
        .send()
        .await
        .expect("jobs GET failed")
        .json()
        .await
        .expect("jobs not JSON");
    v["jobs"][0].clone()
}

async fn bucket_backend(admin: &reqwest::Client, endpoint: &str, bucket: &str) -> Option<String> {
    let cfg: serde_json::Value = admin
        .get(format!("{endpoint}/_/api/admin/config"))
        .send()
        .await
        .expect("config GET failed")
        .json()
        .await
        .expect("config not JSON");
    cfg["bucket_policies"][bucket]["backend"]
        .as_str()
        .map(String::from)
}

async fn transient_keys(admin: &reqwest::Client, endpoint: &str) -> Vec<String> {
    let cfg: serde_json::Value = admin
        .get(format!("{endpoint}/_/api/admin/config"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    cfg["bucket_policies"]
        .as_object()
        .map(|m| {
            m.keys()
                .filter(|k| k.starts_with("__dgmigrate_"))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test]
async fn test_migrate_full_cycle() {
    let dir_a = tempfile::TempDir::new().unwrap();
    let dir_b = tempfile::TempDir::new().unwrap();
    let bucket = "migbkt";
    let server = TestServer::builder()
        .bucket(bucket)
        .extra_yaml_storage_section(&two_backend_yaml(dir_a.path(), dir_b.path()))
        .build()
        .await;
    let http = reqwest::Client::new();
    let endpoint = server.endpoint();

    seed(&http, &endpoint, bucket, 30).await;
    assert!(
        dir_a.path().join(bucket).exists(),
        "seeded objects should land on the src backend"
    );

    let admin = admin_http_client(&endpoint).await;
    let resp = start_migrate(&admin, &endpoint, bucket, "dst", false).await;
    assert_eq!(resp.status(), 202, "job-creating POST returns 202");
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["bucket"], bucket);
    assert_eq!(body["from_backend"], "src");
    assert_eq!(body["to_backend"], "dst");
    assert!(body["job_id"].as_i64().is_some());

    // ── Mid-job: writes gated (the stale-copy race fix), reads up. ──
    let put_resp = http
        .put(format!("{endpoint}/{bucket}/gate-probe.json"))
        .body("blocked?")
        .send()
        .await
        .unwrap();
    assert_eq!(
        put_resp.status(),
        503,
        "writes must be gated during migrate"
    );
    assert!(put_resp
        .text()
        .await
        .unwrap_or_default()
        .contains("SlowDown"));
    let read = get_bytes(&http, &endpoint, bucket, "obj-000.json").await;
    assert!(
        read.starts_with(MARKER),
        "reads must keep working during migrate"
    );

    wait_job_done(&admin, &endpoint, bucket).await;

    // ── Job row: migrate kind, completed, all 30 copied. ──
    let job = newest_job(&admin, &endpoint).await;
    assert_eq!(job["kind"], "migrate", "job: {job}");
    assert_eq!(job["status"], "succeeded", "job: {job}");
    assert_eq!(job["progress"]["processed"], 30, "job: {job}");
    assert_eq!(job["progress"]["failed"], 0, "job: {job}");

    // ── Config flipped + persisted; no transient leak. ──
    assert_eq!(
        bucket_backend(&admin, &endpoint, bucket).await.as_deref(),
        Some("dst")
    );
    assert!(transient_keys(&admin, &endpoint).await.is_empty());

    // ── Data serves from the destination; writes resume and land there. ──
    let dst_bucket_dir = dir_b.path().join(bucket);
    assert!(dst_bucket_dir.exists(), "objects should exist on dst");
    for key in ["obj-000.json", "obj-029.json"] {
        let bytes = get_bytes(&http, &endpoint, bucket, key).await;
        assert!(
            bytes.starts_with(MARKER),
            "GET {key} should round-trip post-flip"
        );
    }
    put_object(
        &http,
        &endpoint,
        bucket,
        "after.json",
        b"after".to_vec(),
        "application/json",
    )
    .await;

    // ── Same-backend migrate now rejected. ──
    let dup = start_migrate(&admin, &endpoint, bucket, "dst", false).await;
    assert_eq!(dup.status(), 400, "already on dst");
}

#[tokio::test]
async fn test_migrate_delete_source() {
    let dir_a = tempfile::TempDir::new().unwrap();
    let dir_b = tempfile::TempDir::new().unwrap();
    let bucket = "migdel";
    let server = TestServer::builder()
        .bucket(bucket)
        .extra_yaml_storage_section(&two_backend_yaml(dir_a.path(), dir_b.path()))
        .build()
        .await;
    let http = reqwest::Client::new();
    let endpoint = server.endpoint();
    seed(&http, &endpoint, bucket, 10).await;

    let admin = admin_http_client(&endpoint).await;
    let resp = start_migrate(&admin, &endpoint, bucket, "dst", true).await;
    assert_eq!(resp.status(), 202);
    wait_job_done(&admin, &endpoint, bucket).await;

    let job = newest_job(&admin, &endpoint).await;
    assert_eq!(job["status"], "succeeded", "job: {job}");

    // Destination serves; source object files are gone.
    let bytes = get_bytes(&http, &endpoint, bucket, "obj-000.json").await;
    assert!(bytes.starts_with(MARKER));
    let leftover = walkdir_files(&dir_a.path().join(bucket));
    assert!(
        leftover.is_empty(),
        "source objects should be deleted, found: {leftover:?}"
    );
    assert!(transient_keys(&admin, &endpoint).await.is_empty());
}

fn walkdir_files(root: &std::path::Path) -> Vec<String> {
    let mut out = Vec::new();
    if !root.exists() {
        return out;
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    out.push(p.display().to_string());
                }
            }
        }
    }
    out
}

#[tokio::test]
async fn test_migrate_validations() {
    let dir_a = tempfile::TempDir::new().unwrap();
    let dir_b = tempfile::TempDir::new().unwrap();
    let bucket = "migval";
    let server = TestServer::builder()
        .bucket(bucket)
        .extra_yaml_storage_section(&two_backend_yaml(dir_a.path(), dir_b.path()))
        .build()
        .await;
    let http = reqwest::Client::new();
    let endpoint = server.endpoint();
    let admin = admin_http_client(&endpoint).await;

    // Unknown target backend.
    let r = start_migrate(&admin, &endpoint, bucket, "nope", false).await;
    assert_eq!(r.status(), 400);
    // Ghost bucket (the old synchronous handler skipped this check).
    let r = start_migrate(&admin, &endpoint, "ghostbucket", "dst", false).await;
    assert_eq!(r.status(), 404);

    // Duplicate active job → 409 (gate + partial unique index).
    seed(&http, &endpoint, bucket, 120).await;
    let first = start_migrate(&admin, &endpoint, bucket, "dst", false).await;
    assert_eq!(first.status(), 202);
    let second = start_migrate(&admin, &endpoint, bucket, "dst", false).await;
    assert_eq!(second.status(), 409, "active job must block a second one");
    wait_job_done(&admin, &endpoint, bucket).await;
}

#[tokio::test]
async fn test_migrate_cancel_preflip_restores_source() {
    let dir_a = tempfile::TempDir::new().unwrap();
    let dir_b = tempfile::TempDir::new().unwrap();
    let bucket = "migcan";
    let server = TestServer::builder()
        .bucket(bucket)
        .extra_yaml_storage_section(&two_backend_yaml(dir_a.path(), dir_b.path()))
        .build()
        .await;
    let http = reqwest::Client::new();
    let endpoint = server.endpoint();
    seed(&http, &endpoint, bucket, 150).await;

    let admin = admin_http_client(&endpoint).await;
    let resp = start_migrate(&admin, &endpoint, bucket, "dst", false).await;
    assert_eq!(resp.status(), 202);
    let job_id = resp.json::<serde_json::Value>().await.unwrap()["job_id"]
        .as_i64()
        .unwrap();

    let cancel = admin
        .post(format!(
            "{endpoint}/_/api/admin/jobs/maintenance:{job_id}/cancel"
        ))
        .send()
        .await
        .unwrap();
    assert!(
        cancel.status().is_success() || cancel.status() == 409,
        "cancel: {}",
        cancel.status()
    );

    wait_job_done(&admin, &endpoint, bucket).await;
    let job = newest_job(&admin, &endpoint).await;
    let status = job["status"].as_str().unwrap();
    assert!(
        status == "cancelled" || status == "succeeded",
        "terminal expected: {job}"
    );

    // No transient leak either way; routing matches the outcome; data reads.
    assert!(transient_keys(&admin, &endpoint).await.is_empty());
    let backend = bucket_backend(&admin, &endpoint, bucket).await;
    if status == "cancelled" {
        assert_ne!(
            backend.as_deref(),
            Some("dst"),
            "cancelled = source authoritative"
        );
    } else {
        assert_eq!(backend.as_deref(), Some("dst"));
    }
    let bytes = get_bytes(&http, &endpoint, bucket, "obj-000.json").await;
    assert!(bytes.starts_with(MARKER));
    // Gate released regardless of outcome.
    put_object(
        &http,
        &endpoint,
        bucket,
        "post-cancel.json",
        b"ok".to_vec(),
        "application/json",
    )
    .await;
}
