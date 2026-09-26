// SPDX-License-Identifier: BUSL-1.1

//! Integration tests for `kind = "migrate"` maintenance jobs — the durable,
//! resumable, write-gated replacement for the old synchronous bucket
//! migration: gate 503s writes during the copy (the stale-copy race fix),
//! reads stay up, the route flips + persists on success, transients never
//! leak, and pre-flip cancellation leaves the source authoritative.
//!
//! Two filesystem backends — no MinIO needed.

use crate::common;

use common::{
    admin_http_client, delete_object, get_bytes, list_objects_raw, put_object, TestServer,
};

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

async fn seed(http: &impl crate::common::S3Requests, endpoint: &str, bucket: &str, n: usize) {
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

async fn start_migrate_body(
    admin: &reqwest::Client,
    endpoint: &str,
    bucket: &str,
    body: serde_json::Value,
) -> reqwest::Response {
    admin
        .post(format!("{endpoint}/_/api/admin/buckets/{bucket}/migrate"))
        .json(&body)
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
    let http = server.http();
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
    let http = server.http();
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
    let http = server.http();
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
    let http = server.http();
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

/// D9: a bucket whose policy has an `alias` lives under the ALIAS name on
/// its backend. The migrate must copy from, flip to and clean up under the
/// real name — not the virtual one. Before the fix the flip routed the bucket
/// to an empty target bucket, and cleanup deleted an unrelated bucket that
/// happened to carry the virtual name on the source backend.
#[tokio::test]
async fn test_migrate_honours_bucket_alias() {
    let dir_a = tempfile::TempDir::new().unwrap();
    let dir_b = tempfile::TempDir::new().unwrap();
    let bucket = "migalias";
    let server = TestServer::builder()
        .bucket(bucket)
        .bucket_policy(bucket, "backend: src\nalias: real-store")
        // An unrelated bucket whose REAL name equals the virtual name above.
        .bucket_policy("decoy", "backend: src\nalias: migalias")
        .extra_yaml_storage_section(&two_backend_yaml(dir_a.path(), dir_b.path()))
        .build()
        .await;
    let http = server.http();
    let endpoint = server.endpoint();
    for b in [bucket, "decoy"] {
        http.put(format!("{endpoint}/{b}")).send().await.unwrap();
    }
    seed(&http, &endpoint, bucket, 5).await;
    put_object(
        &http,
        &endpoint,
        "decoy",
        "keep-me.json",
        b"decoy data".to_vec(),
        "application/json",
    )
    .await;
    assert!(
        dir_a.path().join("real-store").exists(),
        "alias fixture: {:?}\n{}",
        walkdir_files(dir_a.path()),
        std::fs::read_to_string(server.config_path()).unwrap()
    );

    let admin = admin_http_client(&endpoint).await;
    let resp = start_migrate(&admin, &endpoint, bucket, "dst", true).await;
    assert_eq!(resp.status(), 202);
    wait_job_done(&admin, &endpoint, bucket).await;
    let job = newest_job(&admin, &endpoint).await;
    assert_eq!(job["status"], "succeeded", "job: {job}");

    // The migrated bucket serves its data from the target.
    for i in 0..5 {
        let bytes = get_bytes(&http, &endpoint, bucket, &format!("obj-{i:03}.json")).await;
        assert!(bytes.starts_with(MARKER), "obj-{i:03} lost after the flip");
    }
    // The unrelated bucket is untouched.
    assert_eq!(
        get_bytes(&http, &endpoint, "decoy", "keep-me.json").await,
        b"decoy data"
    );
    // The source real-name bucket is emptied (delete_source).
    let leftover = walkdir_files(&dir_a.path().join("real-store"));
    assert!(leftover.is_empty(), "source objects left: {leftover:?}");
}

/// D10: a key that already exists on the target is not proof of a copy. A
/// cancelled earlier attempt leaves old copies behind, and the source keeps
/// taking writes until the retry. The retry must re-copy a target object
/// that differs from the source, or the flip serves the stale copy and
/// delete_source removes the only fresh one.
#[tokio::test]
async fn test_migrate_recopies_a_stale_target_object() {
    let dir_a = tempfile::TempDir::new().unwrap();
    let dir_b = tempfile::TempDir::new().unwrap();
    let bucket = "migstale";
    let server = TestServer::builder()
        .bucket(bucket)
        .extra_yaml_storage_section(&two_backend_yaml(dir_a.path(), dir_b.path()))
        .build()
        .await;
    let http = server.http();
    let endpoint = server.endpoint();
    let fresh = [MARKER, b" fresh".as_slice()].concat();
    put_object(
        &http,
        &endpoint,
        bucket,
        "k.json",
        fresh.clone(),
        "application/json",
    )
    .await;
    // Plant the leftover straight on the target backend's disk.
    let planted = dir_b.path().join(bucket).join("deltaspaces");
    std::fs::create_dir_all(&planted).unwrap();
    std::fs::write(
        planted.join("k.json"),
        b"stale copy from a cancelled attempt",
    )
    .unwrap();

    let admin = admin_http_client(&endpoint).await;
    // A non-empty destination needs `target: mirror` (the default refuses).
    let resp = start_migrate_body(
        &admin,
        &endpoint,
        bucket,
        serde_json::json!({ "target_backend": "dst", "delete_source": true, "target": "mirror" }),
    )
    .await;
    assert_eq!(resp.status(), 202);
    wait_job_done(&admin, &endpoint, bucket).await;
    let job = newest_job(&admin, &endpoint).await;
    assert_eq!(job["status"], "succeeded", "job: {job}");
    assert_eq!(
        get_bytes(&http, &endpoint, bucket, "k.json").await,
        fresh,
        "the flip serves the stale target copy"
    );
}

/// Browser review #8: a migrated object keeps its created-at. The copy used
/// to stamp the migration time, so listings showed every object as new,
/// lifecycle ages restarted, and newer-wins compared the wrong times.
/// Covers a small (buffered) object and a delta-eligible one.
#[tokio::test]
async fn test_migrate_keeps_created_at() {
    let dir_a = tempfile::TempDir::new().unwrap();
    let dir_b = tempfile::TempDir::new().unwrap();
    let bucket = "migtime";
    let server = TestServer::builder()
        .bucket(bucket)
        .extra_yaml_storage_section(&two_backend_yaml(dir_a.path(), dir_b.path()))
        .build()
        .await;
    let http = server.http();
    let endpoint = server.endpoint();
    let keys = ["notes.json", "app-1.0.0.zip", "app-1.0.1.zip"];
    for key in keys {
        let body = [MARKER, key.as_bytes(), &[7u8; 4096]].concat();
        put_object(
            &http,
            &endpoint,
            bucket,
            key,
            body,
            "application/octet-stream",
        )
        .await;
    }
    let s3 = server.s3_client().await;
    let mut before = Vec::new();
    for key in keys {
        let h = s3
            .head_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .unwrap();
        before.push(h.last_modified().copied().unwrap());
    }
    // LastModified has one-second resolution: make the migration later.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

    let admin = admin_http_client(&endpoint).await;
    let resp = start_migrate(&admin, &endpoint, bucket, "dst", false).await;
    assert_eq!(resp.status(), 202);
    wait_job_done(&admin, &endpoint, bucket).await;
    assert_eq!(newest_job(&admin, &endpoint).await["status"], "succeeded");
    assert_eq!(
        bucket_backend(&admin, &endpoint, bucket).await.as_deref(),
        Some("dst")
    );

    for (key, was) in keys.iter().zip(before) {
        let h = s3
            .head_object()
            .bucket(bucket)
            .key(*key)
            .send()
            .await
            .unwrap();
        assert_eq!(
            h.last_modified().copied().unwrap(),
            was,
            "{key}: the migrated copy must keep the original time"
        );
    }
}

async fn listed_keys(
    http: &impl crate::common::S3Requests,
    endpoint: &str,
    bucket: &str,
) -> String {
    list_objects_raw(http, endpoint, bucket, "").await
}

/// Explore #5: A → B keeps a safety copy on A. An object deleted on B and a
/// migrate back to A used to bring the deleted object back from that copy.
/// The default now refuses a destination that holds objects, and names the
/// count and the `mirror` option.
#[tokio::test]
async fn test_migrate_back_refuses_a_destination_with_an_old_copy() {
    let dir_a = tempfile::TempDir::new().unwrap();
    let dir_b = tempfile::TempDir::new().unwrap();
    let bucket = "migback";
    let server = TestServer::builder()
        .bucket(bucket)
        .extra_yaml_storage_section(&two_backend_yaml(dir_a.path(), dir_b.path()))
        .build()
        .await;
    let http = server.http();
    let endpoint = server.endpoint();
    for key in ["keep.json", "gone.json"] {
        put_object(
            &http,
            &endpoint,
            bucket,
            key,
            MARKER.to_vec(),
            "application/json",
        )
        .await;
    }
    let admin = admin_http_client(&endpoint).await;
    assert_eq!(
        start_migrate(&admin, &endpoint, bucket, "dst", false)
            .await
            .status(),
        202
    );
    wait_job_done(&admin, &endpoint, bucket).await;
    assert_eq!(
        bucket_backend(&admin, &endpoint, bucket).await.as_deref(),
        Some("dst")
    );
    delete_object(&http, &endpoint, bucket, "gone.json").await;

    assert_eq!(
        start_migrate(&admin, &endpoint, bucket, "src", false)
            .await
            .status(),
        202
    );
    wait_job_done(&admin, &endpoint, bucket).await;
    let job = newest_job(&admin, &endpoint).await;
    assert_eq!(job["status"], "failed", "job: {job}");
    let err = job["last_error"].as_str().unwrap_or_default();
    assert!(err.contains("already holds 2 object(s)"), "{err}");
    assert!(err.contains("mirror"), "{err}");
    assert_eq!(
        bucket_backend(&admin, &endpoint, bucket).await.as_deref(),
        Some("dst"),
        "a refused migrate leaves the bucket where it is"
    );
    assert!(transient_keys(&admin, &endpoint).await.is_empty());
    let list = listed_keys(&http, &endpoint, bucket).await;
    assert!(
        !list.contains("gone.json"),
        "deleted object came back: {list}"
    );
    assert!(list.contains("keep.json"), "{list}");
    // The source copy on src is untouched by the refusal.
    assert!(!walkdir_files(&dir_a.path().join(bucket)).is_empty());
}

/// Explore #5: `target: mirror` makes the destination an exact copy — an
/// object that exists only on the destination is deleted before the flip.
#[tokio::test]
async fn test_migrate_mirror_deletes_destination_extras() {
    let dir_a = tempfile::TempDir::new().unwrap();
    let dir_b = tempfile::TempDir::new().unwrap();
    let bucket = "migmirror";
    let server = TestServer::builder()
        .bucket(bucket)
        .extra_yaml_storage_section(&two_backend_yaml(dir_a.path(), dir_b.path()))
        .build()
        .await;
    let http = server.http();
    let endpoint = server.endpoint();
    for key in ["keep.json", "gone.json"] {
        put_object(
            &http,
            &endpoint,
            bucket,
            key,
            MARKER.to_vec(),
            "application/json",
        )
        .await;
    }
    let admin = admin_http_client(&endpoint).await;
    assert_eq!(
        start_migrate(&admin, &endpoint, bucket, "dst", false)
            .await
            .status(),
        202
    );
    wait_job_done(&admin, &endpoint, bucket).await;
    delete_object(&http, &endpoint, bucket, "gone.json").await;

    let resp = start_migrate_body(
        &admin,
        &endpoint,
        bucket,
        serde_json::json!({ "target_backend": "src", "target": "mirror" }),
    )
    .await;
    assert_eq!(resp.status(), 202);
    assert_eq!(
        resp.json::<serde_json::Value>().await.unwrap()["target"],
        "mirror"
    );
    wait_job_done(&admin, &endpoint, bucket).await;
    let job = newest_job(&admin, &endpoint).await;
    assert_eq!(job["status"], "succeeded", "job: {job}");
    assert_eq!(job["detail"]["target"], "mirror", "job: {job}");
    assert_eq!(
        bucket_backend(&admin, &endpoint, bucket).await.as_deref(),
        Some("src")
    );
    let list = listed_keys(&http, &endpoint, bucket).await;
    assert!(
        !list.contains("gone.json"),
        "mirror kept a deleted object: {list}"
    );
    assert!(list.contains("keep.json"), "{list}");

    // The delete is audited.
    let audit: serde_json::Value = admin
        .get(format!("{endpoint}/_/api/admin/audit?limit=200"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        audit
            .to_string()
            .contains("maintenance_migrate_mirror_delete"),
        "no audit entry: {audit}"
    );
}
