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
    crate::common::expect_server_errors("a maintenance write gate answers 503 (asserted)");
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

/// jobs-1: routing is per-instance YAML and config sync does not carry it,
/// so a migrate on a multi-instance deployment lets peers keep writing to the
/// source that cleanup deletes. The start is refused with 409.
#[tokio::test]
async fn test_migrate_refused_when_a_coordination_bucket_is_set() {
    let dir_a = tempfile::TempDir::new().unwrap();
    let dir_b = tempfile::TempDir::new().unwrap();
    let bucket = "migmulti";
    // config_sync on a filesystem singleton degrades to a warning at boot.
    let server = TestServer::builder()
        .bucket(bucket)
        .config_sync_bucket("dgp-sync")
        .extra_yaml_storage_section(&two_backend_yaml(dir_a.path(), dir_b.path()))
        .build()
        .await;
    let endpoint = server.endpoint();
    let admin = admin_http_client(&endpoint).await;
    let r = start_migrate(&admin, &endpoint, bucket, "dst", false).await;
    assert_eq!(r.status(), 409);
    let body = r.text().await.unwrap();
    assert!(
        body.contains("multi-instance") && body.contains("move-a-bucket-between-backends"),
        "refusal must name the reason and link the doc, got: {body}"
    );
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

/// A cancel does not wait for the disk. A migrate that waits for a full target
/// disk and is cancelled unwinds at its next claim, while the disk is STILL
/// full: its staged copies (which free space) and its staging route go, and the
/// source stays authoritative. It used to stay `cancelling` for as long as the
/// disk was full, each claim deferring it again.
#[tokio::test]
async fn test_a_migrate_that_waits_for_disk_can_be_cancelled() {
    common::expect_server_errors("level critical refuses the write that this test tries");
    let dir_a = tempfile::TempDir::new().unwrap();
    let dir_b = tempfile::TempDir::new().unwrap();
    let free = tempfile::TempDir::new().unwrap();
    let free_file = free.path().join("free-percent");
    let set_free = |percent: u8| std::fs::write(&free_file, percent.to_string()).unwrap();
    set_free(50);
    let bucket = "migdiskcan";
    let server = TestServer::builder()
        .bucket(bucket)
        .extra_yaml_storage_section(&two_backend_yaml(dir_a.path(), dir_b.path()))
        .env("DGP_TEST_MAINTENANCE_OBJECT_STALL_MS", "100")
        .env(
            "DGP_TEST_DISK_MONITOR_FREE_FILE",
            &free_file.display().to_string(),
        )
        .env("DGP_TEST_MAINTENANCE_DEFER_SECS", "2")
        .env("DGP_DISK_MONITOR_INTERVAL_SECS", "1")
        .build()
        .await;
    let http = server.http();
    let endpoint = server.endpoint();
    seed(&http, &endpoint, bucket, 300).await;
    let admin = admin_http_client(&endpoint).await;
    let resp = start_migrate(&admin, &endpoint, bucket, "dst", false).await;
    assert_eq!(resp.status(), 202);
    let job_id = resp.json::<serde_json::Value>().await.unwrap()["job_id"]
        .as_i64()
        .unwrap();
    for _ in 0..1000 {
        let job = newest_job(&admin, &endpoint).await;
        if job["phase"] == "copy" && job["progress"]["processed"].as_i64().unwrap_or(0) >= 20 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    set_free(3);
    // The job waits once a delete passes.
    let mut waiting = false;
    for _ in 0..300 {
        let resp = http
            .delete(format!("{endpoint}/{bucket}/obj-299.json"))
            .send()
            .await
            .unwrap();
        if resp.status().as_u16() == 204 {
            waiting = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(waiting, "the job never waited for disk");
    assert!(
        walkdir_files(&dir_b.path().join(bucket))
            .iter()
            .any(|p| p.contains("obj-")),
        "the wait landed mid-copy"
    );

    let cancel = admin
        .post(format!(
            "{endpoint}/_/api/admin/jobs/maintenance:{job_id}/cancel"
        ))
        .send()
        .await
        .unwrap();
    assert!(cancel.status().is_success(), "cancel: {}", cancel.status());

    // The disk stays full: the cancel still ends the job.
    wait_job_done(&admin, &endpoint, bucket).await;
    let job = newest_migrate_job(&admin, &endpoint).await;
    assert_eq!(job["status"], "cancelled", "{job}");
    assert!(
        !walkdir_files(&dir_b.path().join(bucket))
            .iter()
            .any(|p| p.contains("obj-")),
        "the staged copies are removed"
    );
    assert!(transient_keys(&admin, &endpoint).await.is_empty());
    assert_ne!(
        bucket_backend(&admin, &endpoint, bucket).await.as_deref(),
        Some("dst"),
        "the source stays authoritative"
    );
    assert!(get_bytes(&http, &endpoint, bucket, "obj-000.json")
        .await
        .starts_with(MARKER));
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

/// NEW-10: `releases` and `downloads` store into one real bucket. The
/// migrate gates and copies `releases` only, so writes through `downloads`
/// are not copied, and the cleanup deletes them. The start is refused with
/// 409, and the refusal names the alias.
#[tokio::test]
async fn test_migrate_refuses_a_bucket_with_an_alias() {
    let dir_a = tempfile::TempDir::new().unwrap();
    let dir_b = tempfile::TempDir::new().unwrap();
    let server = TestServer::builder()
        .bucket("releases")
        .bucket_policy("releases", "backend: src")
        .bucket_policy("downloads", "backend: src\nalias: releases")
        .extra_yaml_storage_section(&two_backend_yaml(dir_a.path(), dir_b.path()))
        .build()
        .await;
    let http = server.http();
    let endpoint = server.endpoint();
    http.put(format!("{endpoint}/releases"))
        .send()
        .await
        .unwrap();
    seed(&http, &endpoint, "releases", 2).await;
    // ListBuckets shows both names of the one storage.
    let listed = server
        .s3_client()
        .await
        .list_buckets()
        .send()
        .await
        .unwrap();
    let names: Vec<_> = listed.buckets().iter().filter_map(|b| b.name()).collect();
    assert!(
        names.contains(&"releases") && names.contains(&"downloads"),
        "{names:?}"
    );

    let admin = admin_http_client(&endpoint).await;
    for (bucket, other) in [("releases", "'downloads'"), ("downloads", "'releases'")] {
        let resp = start_migrate(&admin, &endpoint, bucket, "dst", true).await;
        assert_eq!(
            resp.status(),
            409,
            "the migrate of '{bucket}' must be refused"
        );
        let body = resp.text().await.unwrap();
        assert!(
            body.contains("shares its storage") && body.contains(other),
            "{body}"
        );
    }
    // No job, and the data stays where it is.
    for i in 0..2 {
        let bytes = get_bytes(&http, &endpoint, "downloads", &format!("obj-{i:03}.json")).await;
        assert!(bytes.starts_with(MARKER));
    }
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
/// count and the mirror option, at once in the start request's answer.
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

    // Maint M4: the refusal is the start request's answer (a 409 that the
    // dialog shows, in its own words), not a job that fails a moment later.
    let jobs_before = newest_job(&admin, &endpoint).await["id"].clone();
    let resp = start_migrate(&admin, &endpoint, bucket, "src", false).await;
    assert_eq!(resp.status(), 409);
    let err = resp.text().await.unwrap_or_default();
    assert!(err.contains("already holds 2 object(s)"), "{err}");
    assert!(
        err.contains("Make the destination an exact mirror"),
        "{err}"
    );
    assert_eq!(
        newest_job(&admin, &endpoint).await["id"],
        jobs_before,
        "a refused start creates no job"
    );
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

/// Explore #9: a cancel used to wait for the end of a 1000-object page, so a
/// bucket smaller than one page could not be cancelled at all, and the
/// cancelled attempt left its copies on the destination. Now the job checks
/// for a cancel every few objects, removes the copies it made (and only
/// those), and releases the write gate.
#[tokio::test]
async fn test_migrate_cancel_mid_copy_removes_its_staged_copies() {
    let dir_a = tempfile::TempDir::new().unwrap();
    let dir_b = tempfile::TempDir::new().unwrap();
    let bucket = "migcanmid";
    let server = TestServer::builder()
        .bucket(bucket)
        .extra_yaml_storage_section(&two_backend_yaml(dir_a.path(), dir_b.path()))
        .build()
        .await;
    let http = server.http();
    let endpoint = server.endpoint();
    seed(&http, &endpoint, bucket, 400).await;
    // An object that was on the destination before the job: not ours.
    let planted_dir = dir_b.path().join(bucket).join("deltaspaces");
    std::fs::create_dir_all(&planted_dir).unwrap();
    std::fs::write(planted_dir.join("zz-foreign.json"), b"not from this job").unwrap();

    let admin = admin_http_client(&endpoint).await;
    let resp = start_migrate_body(
        &admin,
        &endpoint,
        bucket,
        serde_json::json!({ "target_backend": "dst", "target": "mirror" }),
    )
    .await;
    assert_eq!(resp.status(), 202);
    let job_id = resp.json::<serde_json::Value>().await.unwrap()["job_id"]
        .as_i64()
        .unwrap();

    // Wait until the copy is under way (progress is persisted mid-page).
    for _ in 0..1000 {
        let job = newest_job(&admin, &endpoint).await;
        let status = job["status"].as_str().unwrap_or_default();
        if job["progress"]["processed"].as_i64().unwrap_or(0) >= 20
            || matches!(status, "succeeded" | "failed" | "cancelled")
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let cancel = admin
        .post(format!(
            "{endpoint}/_/api/admin/jobs/maintenance:{job_id}/cancel"
        ))
        .send()
        .await
        .unwrap();
    assert!(cancel.status().is_success(), "cancel: {}", cancel.status());
    wait_job_done(&admin, &endpoint, bucket).await;

    let job = newest_job(&admin, &endpoint).await;
    assert_eq!(job["status"], "cancelled", "job: {job}");
    let processed = job["progress"]["processed"].as_i64().unwrap_or(0);
    assert!(
        (20..400).contains(&processed),
        "the cancel must stop the copy mid-page, after it started: {job}"
    );
    assert_ne!(
        bucket_backend(&admin, &endpoint, bucket).await.as_deref(),
        Some("dst")
    );
    assert!(transient_keys(&admin, &endpoint).await.is_empty());
    let left: Vec<String> = walkdir_files(&dir_b.path().join(bucket))
        .into_iter()
        .filter(|p| p.contains("obj-"))
        .collect();
    assert!(
        left.is_empty(),
        "staged copies left on the destination: {left:?}"
    );
    assert!(
        planted_dir.join("zz-foreign.json").exists(),
        "the cleanup deleted an object this job did not write"
    );
    put_object(
        &http,
        &endpoint,
        bucket,
        "after.json",
        b"ok".to_vec(),
        "application/json",
    )
    .await;
}

/// A cancel that a shutdown interrupts must still unwind. SIGTERM while a
/// migrate is `cancelling` hands the row back with its status kept and its
/// lease released; the boot scan then settled it `cancelled` without the
/// pre-flip unwind, which left the staging route and the staged copies on
/// the destination. Now the next worker claims the row and runs the cancel.
#[tokio::test]
async fn test_migrate_cancel_interrupted_by_shutdown_still_unwinds() {
    let dir_a = tempfile::TempDir::new().unwrap();
    let dir_b = tempfile::TempDir::new().unwrap();
    let bucket = "migcansig";
    let mut server = TestServer::builder()
        .bucket(bucket)
        .extra_yaml_storage_section(&two_backend_yaml(dir_a.path(), dir_b.path()))
        .build()
        .await;
    let http = server.http();
    let endpoint = server.endpoint();
    seed(&http, &endpoint, bucket, 400).await;
    let admin = admin_http_client(&endpoint).await;
    let resp = start_migrate(&admin, &endpoint, bucket, "dst", false).await;
    assert_eq!(resp.status(), 202);
    let job_id = resp.json::<serde_json::Value>().await.unwrap()["job_id"]
        .as_i64()
        .unwrap();
    for _ in 0..1000 {
        let job = newest_job(&admin, &endpoint).await;
        if job["progress"]["processed"].as_i64().unwrap_or(0) >= 20 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    server.terminate();

    // The row as a SIGTERM during `cancelling` leaves it: the operator's
    // cancel is recorded, the runner released its lease and stopped.
    let dir = server.config_path().parent().unwrap().to_path_buf();
    let key = std::fs::read_to_string(dir.join("deltaglider_config.db.key")).unwrap();
    {
        let db = deltaglider_proxy::config_db::JobsDb::open_or_create(
            &dir.join("deltaglider_jobs.db"),
            key.trim(),
        )
        .unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let job = db
            .maintenance_claim_next_job("stopped-runner", now, 60)
            .unwrap()
            .expect("the SIGTERM left the job resumable");
        assert_eq!(job.id, job_id);
        assert_eq!(job.phase, "copy", "SIGTERM must land in the copy");
        db.maintenance_request_cancel(job_id).unwrap();
        assert!(db
            .maintenance_release_for_resume(job_id, "stopped-runner")
            .unwrap());
    }
    assert!(
        walkdir_files(&dir_b.path().join(bucket))
            .iter()
            .any(|p| p.contains("obj-")),
        "precondition: staged copies exist on the destination"
    );

    server
        .respawn_with_env(&[(
            "DGP_BOOTSTRAP_PASSWORD_HASH",
            common::TEST_BOOTSTRAP_PASSWORD_HASH,
        )])
        .await;
    let endpoint = server.endpoint();
    let admin = admin_http_client(&endpoint).await;
    wait_job_done(&admin, &endpoint, bucket).await;

    let job = newest_job(&admin, &endpoint).await;
    assert_eq!(job["status"], "cancelled", "job: {job}");
    assert_ne!(
        bucket_backend(&admin, &endpoint, bucket).await.as_deref(),
        Some("dst")
    );
    assert!(
        transient_keys(&admin, &endpoint).await.is_empty(),
        "the staging route survived the cancel"
    );
    let left: Vec<String> = walkdir_files(&dir_b.path().join(bucket))
        .into_iter()
        .filter(|p| p.contains("obj-"))
        .collect();
    assert!(left.is_empty(), "staged copies left: {left:?}");
    let http = server.http();
    assert_eq!(
        get_bytes(&http, &endpoint, bucket, "obj-399.json").await,
        [MARKER, b" object 399"].concat()
    );
    put_object(
        &http,
        &endpoint,
        bucket,
        "after.json",
        b"ok".to_vec(),
        "application/json",
    )
    .await;
}

/// A resumed copy does not trust a target file that a crash may have torn.
/// The copy writes without the per-object fsync, so after a crash a file
/// can keep its metadata and lose its data. The stand-in here: stop the
/// proxy mid-copy, truncate every staged copy (the metadata xattr stays),
/// and resume. Every object must still read back whole after the flip.
#[tokio::test]
async fn test_migrate_resume_recopies_possibly_torn_copies() {
    let dir_a = tempfile::TempDir::new().unwrap();
    let dir_b = tempfile::TempDir::new().unwrap();
    let bucket = "migtorn";
    let mut server = TestServer::builder()
        .bucket(bucket)
        .extra_yaml_storage_section(&two_backend_yaml(dir_a.path(), dir_b.path()))
        .build()
        .await;
    let http = server.http();
    let endpoint = server.endpoint();
    seed(&http, &endpoint, bucket, 400).await;
    let admin = admin_http_client(&endpoint).await;
    let resp = start_migrate(&admin, &endpoint, bucket, "dst", false).await;
    assert_eq!(resp.status(), 202);
    for _ in 0..1000 {
        let job = newest_job(&admin, &endpoint).await;
        if job["progress"]["processed"].as_i64().unwrap_or(0) >= 20 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    server.terminate();
    let staged: Vec<String> = walkdir_files(&dir_b.path().join(bucket))
        .into_iter()
        .filter(|p| p.contains("obj-"))
        .collect();
    assert!(!staged.is_empty(), "the stop must land mid-copy");
    assert!(staged.len() < 400, "the stop must land mid-copy");
    for p in &staged {
        std::fs::OpenOptions::new()
            .write(true)
            .open(p)
            .unwrap()
            .set_len(0)
            .unwrap();
    }

    server
        .respawn_with_env(&[(
            "DGP_BOOTSTRAP_PASSWORD_HASH",
            common::TEST_BOOTSTRAP_PASSWORD_HASH,
        )])
        .await;
    let endpoint = server.endpoint();
    let admin = admin_http_client(&endpoint).await;
    wait_job_done(&admin, &endpoint, bucket).await;
    let job = newest_job(&admin, &endpoint).await;
    assert_eq!(job["status"], "succeeded", "job: {job}");
    assert_eq!(
        bucket_backend(&admin, &endpoint, bucket).await.as_deref(),
        Some("dst")
    );
    let http = server.http();
    for i in 0..400 {
        assert_eq!(
            get_bytes(&http, &endpoint, bucket, &format!("obj-{i:03}.json")).await,
            [MARKER, format!(" object {i}").as_bytes()].concat(),
            "obj-{i:03}"
        );
    }
}

/// Manual bench (not in CI): wall time of a filesystem → filesystem migrate
/// of `DGP_BENCH_N` (default 1000) small objects.
#[tokio::test]
#[ignore = "manual: DGP_BENCH_N=1000 cargo test --test all -- --ignored migrate_job_test::bench_migrate_small_objects --nocapture"]
async fn bench_migrate_small_objects() {
    let dir_a = tempfile::TempDir::new().unwrap();
    let dir_b = tempfile::TempDir::new().unwrap();
    let bucket = "migbench";
    let server = TestServer::builder()
        .bucket(bucket)
        .extra_yaml_storage_section(&two_backend_yaml(dir_a.path(), dir_b.path()))
        .build()
        .await;
    let http = server.http();
    let endpoint = server.endpoint();
    let n: usize = std::env::var("DGP_BENCH_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1000);
    seed(&http, &endpoint, bucket, n).await;
    let admin = admin_http_client(&endpoint).await;
    let t = std::time::Instant::now();
    let resp = start_migrate(&admin, &endpoint, bucket, "dst", false).await;
    assert_eq!(resp.status(), 202);
    wait_job_done(&admin, &endpoint, bucket).await;
    let el = t.elapsed();
    assert_eq!(newest_job(&admin, &endpoint).await["status"], "succeeded");
    eprintln!(
        "migrate bench: {n} objects in {el:?} = {:.2} ms/object",
        el.as_secs_f64() * 1000.0 / n as f64
    );
}

/// Maint M1 + B3: a running migrate row reports its phase and a total (the
/// count next to the copy), so its percent is determinate; the bucket
/// status reports the write gate (`writes_blocked`) apart from the row (`active`):
/// a cancelling migrate releases the source before its row settles.
#[tokio::test]
async fn test_migrate_row_reports_progress_and_status_follows_the_gate() {
    let dir_a = tempfile::TempDir::new().unwrap();
    let dir_b = tempfile::TempDir::new().unwrap();
    let bucket = "migprog";
    let server = TestServer::builder()
        .bucket(bucket)
        .extra_yaml_storage_section(&two_backend_yaml(dir_a.path(), dir_b.path()))
        .env("DGP_TEST_MAINTENANCE_OBJECT_STALL_MS", "20")
        .build()
        .await;
    let http = server.http();
    let endpoint = server.endpoint();
    seed(&http, &endpoint, bucket, 300).await;
    let admin = admin_http_client(&endpoint).await;
    let resp = start_migrate(&admin, &endpoint, bucket, "dst", false).await;
    assert_eq!(resp.status(), 202);
    let job_id = resp.json::<serde_json::Value>().await.unwrap()["job_id"]
        .as_i64()
        .unwrap();

    // The copy, with a known total and a determinate percent.
    let mut seen = serde_json::Value::Null;
    for _ in 0..500 {
        seen = newest_job(&admin, &endpoint).await;
        if seen["phase"] == "copy"
            && seen["progress"]["total"] == 300
            && seen["percent"].is_u64()
            && seen["progress"]["processed"].as_i64().unwrap_or(0) > 0
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(seen["phase"], "copy", "{seen}");
    assert_eq!(seen["progress"]["total"], 300, "{seen}");
    assert!(seen["percent"].as_u64().is_some_and(|p| p < 50), "{seen}");

    let status =
        |v: &serde_json::Value| (v["writes_blocked"].as_bool(), v["active"]["status"].clone());
    let read = || async {
        admin
            .get(format!("{endpoint}/_/api/admin/jobs/bucket/{bucket}"))
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap()
    };
    let v = read().await;
    assert_eq!(status(&v), (Some(true), "running".into()), "{v}");
    assert_eq!(v["active"]["writes_blocked"], true, "{v}");

    let cancel = admin
        .post(format!(
            "{endpoint}/_/api/admin/jobs/maintenance:{job_id}/cancel"
        ))
        .send()
        .await
        .unwrap();
    assert!(cancel.status().is_success(), "cancel: {}", cancel.status());
    let mut last = serde_json::Value::Null;
    for _ in 0..600 {
        last = read().await;
        assert!(last["writes_blocked"].is_boolean(), "{last}");
        if last["active"].is_null() {
            break;
        }
        assert_eq!(
            last["active"]["writes_blocked"], last["writes_blocked"],
            "{last}"
        );
        assert!(
            !(last["writes_blocked"] == false && last["active"]["status"] == "running"),
            "a running job always holds the gate: {last}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(last["active"].is_null(), "{last}");
    assert_eq!(last["writes_blocked"], false, "{last}");
    assert_eq!(newest_job(&admin, &endpoint).await["status"], "cancelled");
}

/// Compatibility review C1: the way out of a read-only bucket. A bucket on
/// backend `src` holds plaintext objects; then `src` encrypts with the
/// proxy and its verdict is non-CAS, so the bucket is read-only and its
/// plaintext (no conversion window) is refused to clients. A migrate to a
/// backend with atomic writes reads the plaintext as the job (not as a
/// client), encrypts it on the target, and reports each such object as an
/// adoption (failure ring and audit). After the move the history is
/// readable again.
#[tokio::test]
async fn test_migrate_moves_a_read_only_bucket_with_plaintext_history() {
    crate::common::expect_server_errors(
        "a refused read of an unopened sealed object answers 500 (asserted)",
    );
    const KEY_A: &str = "a1a2a3a4a5a6a7a8a9aaabacadaeafb0b1b2b3b4b5b6b7b8b9babbbcbdbebfc0";
    const KEY_B: &str = "b1b2b3b4b5b6b7b8b9babbbcbdbebfc0a1a2a3a4a5a6a7a8a9aaabacadaeafb0";
    let dir_a = tempfile::TempDir::new().unwrap();
    let dir_b = tempfile::TempDir::new().unwrap();
    let bucket = "migro";
    let mut server = TestServer::builder()
        .bucket(bucket)
        .extra_yaml_storage_section(&two_backend_yaml(dir_a.path(), dir_b.path()))
        .build()
        .await;
    let endpoint = server.endpoint();
    seed(&server.http(), &endpoint, bucket, 3).await;

    // Both backends now encrypt with the proxy; `src` has no atomic writes.
    let cfg = std::fs::read_to_string(server.config_path()).unwrap();
    let mut out = String::new();
    for line in cfg.lines() {
        out.push_str(line);
        out.push('\n');
        for (dir, key, id) in [
            (dir_a.path(), KEY_A, "mig-a"),
            (dir_b.path(), KEY_B, "mig-b"),
        ] {
            if line.trim() == format!("path: \"{}\"", dir.display()) {
                out.push_str(&format!(
                    "    encryption:\n      mode: aes256-gcm-proxy\n      key: \"{key}\"\n      key_id: {id}\n"
                ));
            }
        }
    }
    assert_eq!(out.matches("aes256-gcm-proxy").count(), 2, "{out}");
    std::fs::write(server.config_path(), out).unwrap();
    server
        .respawn_with_env(&[("DGP_TEST_FORCE_NONCAS_BACKEND", "src")])
        .await;
    let http = server.http();
    let admin = admin_http_client(&endpoint).await;
    let get = |key: String| http.get(format!("{endpoint}/{bucket}/{key}")).send();
    assert!(
        !get("obj-000.json".into())
            .await
            .unwrap()
            .status()
            .is_success(),
        "premise: plaintext without a window is refused to clients"
    );

    let resp = start_migrate(&admin, &endpoint, bucket, "dst", false).await;
    assert_eq!(resp.status(), 202, "{:?}", resp.text().await);
    wait_job_done(&admin, &endpoint, bucket).await;
    let job = newest_job(&admin, &endpoint).await;
    assert_eq!(job["kind"], "migrate", "{job}");
    assert_eq!(job["status"], "succeeded", "{job}");
    assert_eq!(
        bucket_backend(&admin, &endpoint, bucket).await.as_deref(),
        Some("dst")
    );
    for i in 0..3 {
        let r = get(format!("obj-{i:03}.json")).await.unwrap();
        assert_eq!(r.status(), 200, "history readable on the target");
        assert!(r.bytes().await.unwrap().starts_with(MARKER));
    }
    // Stored encrypted on the target.
    let stored = walkdir_files(&dir_b.path().join(bucket))
        .into_iter()
        .find(|f| f.ends_with("obj-000.json"))
        .expect("copied");
    assert!(
        !std::fs::read(&stored).unwrap().starts_with(MARKER),
        "encrypted at rest on the target"
    );

    // Reported: the failure ring samples, and the audit.
    let id = job["id"].as_str().unwrap().to_string();
    let failures: serde_json::Value = admin
        .get(format!("{endpoint}/_/api/admin/jobs/{id}/failures"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let adopted = failures.to_string().matches("adopted_after_cutoff").count();
    assert_eq!(adopted, 3, "three adoptions sampled: {failures}");
    let audit: serde_json::Value = admin
        .get(format!("{endpoint}/_/api/admin/audit?limit=200"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let audited = audit["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["action"] == "encryption_conversion_adopted_after_cutoff")
        .count();
    assert_eq!(audited, 3, "{audit}");
}

/// L1 (#171): a key that an older release stored below a `.dg` directory is
/// an object: LIST shows it, so a migrate meets it. The destination refuses
/// to write it again, so the job stops before the flip and the source stays
/// authoritative with its data. The owner deletes the key (a delete is
/// allowed), and the migrate then completes.
#[tokio::test]
async fn test_migrate_meets_a_legacy_dg_key_and_stops_before_the_flip() {
    crate::common::expect_server_errors("the legacy key's copy is refused (asserted)");
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
    seed(&http, &endpoint, bucket, 3).await;
    // What release 2.1 stored: no S3 request can write it now.
    let legacy_dir = dir_a.path().join(bucket).join("deltaspaces/old/.dg");
    std::fs::create_dir_all(&legacy_dir).unwrap();
    std::fs::write(legacy_dir.join("keep.jpg"), b"legacy bytes").unwrap();
    let listing = listed_keys(&http, &endpoint, bucket).await;
    assert!(listing.contains("old/.dg/keep.jpg"), "{listing}");

    let admin = admin_http_client(&endpoint).await;
    let resp = start_migrate(&admin, &endpoint, bucket, "dst", false).await;
    assert_eq!(resp.status(), 202);
    wait_job_done(&admin, &endpoint, bucket).await;
    let job = newest_job(&admin, &endpoint).await;
    assert_ne!(job["status"], "succeeded", "job: {job}");
    assert_eq!(
        bucket_backend(&admin, &endpoint, bucket).await.as_deref(),
        None,
        "no flip"
    );
    assert_eq!(
        get_bytes(&http, &endpoint, bucket, "old/.dg/keep.jpg").await,
        b"legacy bytes"
    );
    assert!(transient_keys(&admin, &endpoint).await.is_empty());

    delete_object(&http, &endpoint, bucket, "old/.dg/keep.jpg").await;
    let resp = start_migrate(&admin, &endpoint, bucket, "dst", false).await;
    assert_eq!(resp.status(), 202);
    wait_job_done(&admin, &endpoint, bucket).await;
    let job = newest_job(&admin, &endpoint).await;
    assert_eq!(job["status"], "succeeded", "job: {job}");
}

/// A capability probe key that a stopped probe left in the source bucket is
/// no object of the bucket: the engine refuses to store it, so the copy skips it
/// and the migrate completes.
#[tokio::test]
async fn test_migrate_skips_a_probe_key_in_the_source() {
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
    seed(&http, &endpoint, bucket, 3).await;
    let probe = format!(
        "{}{}",
        deltaglider_proxy::coordination::cas_probe::PROBE_PREFIX,
        "0b9f5a52-3c1e-4d7a-9a6e-2f4c8d1e7b30"
    );
    let (folder, name) = probe.rsplit_once('/').unwrap();
    let planted = dir_a.path().join(bucket).join("deltaspaces").join(folder);
    std::fs::create_dir_all(&planted).unwrap();
    std::fs::write(planted.join(name), b"probe litter").unwrap();
    let listing = listed_keys(&http, &endpoint, bucket).await;
    assert!(listing.contains(&probe), "the key is listed: {listing}");

    let admin = admin_http_client(&endpoint).await;
    let resp = start_migrate(&admin, &endpoint, bucket, "dst", false).await;
    assert_eq!(resp.status(), 202);
    wait_job_done(&admin, &endpoint, bucket).await;
    let job = newest_job(&admin, &endpoint).await;
    assert_eq!(job["status"], "succeeded", "job: {job}");
    assert_eq!(job["progress"]["failed"], 0, "job: {job}");
    assert_eq!(
        bucket_backend(&admin, &endpoint, bucket).await.as_deref(),
        Some("dst")
    );
    assert!(
        !dir_b
            .path()
            .join(bucket)
            .join("deltaspaces")
            .join(&probe)
            .exists(),
        "the probe key was copied"
    );
}

/// A migrate whose copy waits for DISK space does not keep the bucket from
/// deleting. The job holds the write gate for its whole life, so a delete of
/// the source that the gate refused would wait for the job, and the job for
/// the disk. The whole path runs WITHOUT a restart and on the real clock: the
/// disk goes critical while the copy runs (the file seam, read at every
/// measure), the job waits (2 s, seam), deletes pass, a delete JOB deletes
/// (not Skipped, no failure), the disk recovers, and the job resumes on its
/// own.
///
/// Correctness: the stop lands in the COPY (some keys are on the destination).
/// A key that was copied and then deleted at the source must not come back at
/// the destination after the flip: the job recorded its wait in its params,
/// so its verify prunes the destination of every key that the source lacks.
/// A write is refused all along. The free space is the seam's, never the
/// host's.
#[tokio::test]
async fn test_a_migrate_that_waits_for_disk_lets_deletes_pass_and_the_flip_keeps_them() {
    common::expect_server_errors("level critical refuses the write that this test tries");
    let dir_a = tempfile::TempDir::new().unwrap();
    let dir_b = tempfile::TempDir::new().unwrap();
    let free = tempfile::TempDir::new().unwrap();
    let free_file = free.path().join("free-percent");
    let set_free = |percent: u8| std::fs::write(&free_file, percent.to_string()).unwrap();
    set_free(50);
    let bucket = "migdisk";
    let server = TestServer::builder()
        .bucket(bucket)
        .extra_yaml_storage_section(&two_backend_yaml(dir_a.path(), dir_b.path()))
        .env("DGP_TEST_MAINTENANCE_OBJECT_STALL_MS", "100")
        .env(
            "DGP_TEST_DISK_MONITOR_FREE_FILE",
            &free_file.display().to_string(),
        )
        .env("DGP_TEST_MAINTENANCE_DEFER_SECS", "2")
        .env("DGP_DISK_MONITOR_INTERVAL_SECS", "1")
        .build()
        .await;
    let http = server.http();
    let endpoint = server.endpoint();
    seed(&http, &endpoint, bucket, 300).await;
    let admin = admin_http_client(&endpoint).await;
    let resp = start_migrate(&admin, &endpoint, bucket, "dst", false).await;
    assert_eq!(resp.status(), 202);
    for _ in 0..1000 {
        let job = newest_job(&admin, &endpoint).await;
        if job["phase"] == "copy" && job["progress"]["processed"].as_i64().unwrap_or(0) >= 20 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    // The disk of the target fills up mid-copy.
    set_free(3);
    let mut deleted = false;
    for _ in 0..300 {
        let resp = http
            .delete(format!("{endpoint}/{bucket}/obj-000.json"))
            .send()
            .await
            .unwrap();
        if resp.status().as_u16() == 204 {
            deleted = true;
            break;
        }
        assert_eq!(resp.status().as_u16(), 503, "armed until the wait starts");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(
        deleted,
        "a delete never passed while the job waited for disk"
    );
    let staged: Vec<String> = walkdir_files(&dir_b.path().join(bucket))
        .into_iter()
        .filter(|p| p.contains("obj-"))
        .collect();
    assert!(
        staged.len() >= 20 && staged.len() < 300,
        "the wait landed mid-copy: {}",
        staged.len()
    );
    assert!(
        staged.iter().any(|p| p.contains("obj-000.json")),
        "obj-000 was copied before the wait"
    );
    // DeleteObjects passes as well.
    server
        .s3_client()
        .await
        .delete_objects()
        .bucket(bucket)
        .delete(
            aws_sdk_s3::types::Delete::builder()
                .objects(
                    aws_sdk_s3::types::ObjectIdentifier::builder()
                        .key("obj-299.json")
                        .build()
                        .unwrap(),
                )
                .build()
                .unwrap(),
        )
        .send()
        .await
        .expect("DeleteObjects passes while the job waits for disk");
    // The admin bulk delete passes too (it frees space); an admin copy and a
    // move into the held bucket do not (they store bytes).
    let resp = admin
        .post(format!("{endpoint}/_/api/admin/objects/delete"))
        .json(&serde_json::json!({ "bucket": bucket, "keys": ["obj-200.json"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "bulk delete during a disk wait"
    );
    let answer: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(answer["deleted"].as_u64(), Some(1), "{answer}");
    let gone = http
        .head(format!("{endpoint}/{bucket}/obj-200.json"))
        .send()
        .await
        .unwrap();
    assert_eq!(gone.status().as_u16(), 404, "obj-200 is deleted");
    for op in ["copy", "move"] {
        let resp = admin
            .post(format!("{endpoint}/_/api/admin/objects/{op}"))
            .json(&serde_json::json!({
                "source_bucket": bucket, "dest_bucket": bucket, "dest_prefix": "into/",
                "items": [{ "source_key": "obj-001.json", "relative": "obj-001.json" }]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status().as_u16(),
            409,
            "an admin {op} into the held bucket stores bytes"
        );
    }
    // A write is still refused; a read works.
    let w = http
        .put(format!("{endpoint}/{bucket}/new.json"))
        .body(b"x".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(w.status().as_u16(), 503, "a write stays refused");
    assert_eq!(
        get_bytes(&http, &endpoint, bucket, "obj-001.json").await,
        [MARKER, b" object 1"].concat()
    );
    // A delete JOB deletes while the job waits: not skipped, not failed.
    let resp = admin
        .post(format!("{endpoint}/_/api/admin/objects/delete-jobs"))
        .json(&serde_json::json!({
            "bucket": bucket, "prefixes": [], "keys": ["obj-100.json"], "expected": 1
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 202);
    let delete_job = resp.json::<serde_json::Value>().await.unwrap()["job_id"]
        .as_i64()
        .unwrap();
    let mut settled = serde_json::Value::Null;
    for _ in 0..300 {
        let all: serde_json::Value = admin
            .get(format!("{endpoint}/_/api/admin/jobs"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        settled = all["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|j| j["id"] == format!("maintenance:{delete_job}"))
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        if settled["status"] == "succeeded" || settled["status"] == "failed" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(settled["status"], "succeeded", "{settled}");
    let failures: serde_json::Value = admin
        .get(format!(
            "{endpoint}/_/api/admin/jobs/maintenance:{delete_job}/failures"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        failures["failures"].as_array().map(Vec::len),
        Some(0),
        "the delete job skipped keys during the wait: {failures}"
    );
    for key in ["obj-000.json", "obj-100.json", "obj-299.json"] {
        let gone = http
            .head(format!("{endpoint}/{bucket}/{key}"))
            .send()
            .await
            .unwrap();
        assert_eq!(
            gone.status().as_u16(),
            404,
            "{key} is deleted at the source"
        );
    }

    // Space again, NO restart: the job resumes on its own, copies the rest,
    // verifies, prunes the destination and flips.
    set_free(50);
    wait_job_done(&admin, &endpoint, bucket).await;
    let job = newest_migrate_job(&admin, &endpoint).await;
    assert_eq!(job["status"], "succeeded", "job: {job}");
    assert_eq!(
        bucket_backend(&admin, &endpoint, bucket).await.as_deref(),
        Some("dst")
    );
    // The bucket is served by the destination now: the keys that clients
    // deleted during the wait are gone there too, the rest is whole.
    for key in ["obj-000.json", "obj-100.json", "obj-299.json"] {
        let head = http
            .head(format!("{endpoint}/{bucket}/{key}"))
            .send()
            .await
            .unwrap();
        assert_eq!(
            head.status().as_u16(),
            404,
            "{key} came back after the flip"
        );
    }
    for (key, n) in [
        ("obj-001.json", 1),
        ("obj-150.json", 150),
        ("obj-298.json", 298),
    ] {
        assert_eq!(
            get_bytes(&http, &endpoint, bucket, key).await,
            [MARKER, format!(" object {n}").as_bytes()].concat(),
            "{key}"
        );
    }
}

/// A migrate whose copies need a SPOOL file (objects above the spool
/// threshold, across two backends) waits while the spool disk refuses new
/// writes (level high), whatever the disk of the target says. The object that
/// met the refusal is not a failure; the wait lets deletes pass and the job
/// records it, so the verify prunes the key that a client deleted after its
/// copy; with space again the job copies the rest and flips.
#[tokio::test]
async fn test_a_migrate_whose_copies_need_a_spool_waits_at_a_refusing_spool() {
    common::expect_server_errors("the spool disk refuses the writes that need spool space");
    let dir_a = tempfile::TempDir::new().unwrap();
    let dir_b = tempfile::TempDir::new().unwrap();
    let free = tempfile::TempDir::new().unwrap();
    let free_file = free.path().join("free-percent");
    let set_free = |percent: u8| std::fs::write(&free_file, percent.to_string()).unwrap();
    set_free(50);
    let bucket = "migspool";
    let server = TestServer::builder()
        .bucket(bucket)
        .extra_yaml_storage_section(&two_backend_yaml(dir_a.path(), dir_b.path()))
        .env("DGP_SPOOL_THRESHOLD_BYTES", "1024")
        .env("DGP_TEST_MAINTENANCE_OBJECT_STALL_MS", "100")
        .env(
            "DGP_TEST_DISK_MONITOR_FREE_FILE",
            &free_file.display().to_string(),
        )
        .env("DGP_TEST_MAINTENANCE_DEFER_SECS", "2")
        .env("DGP_DISK_MONITOR_INTERVAL_SECS", "1")
        .build()
        .await;
    let http = server.http();
    let endpoint = server.endpoint();
    let body = |i: usize| {
        let mut b = [MARKER, format!(" object {i}").as_bytes()].concat();
        b.resize(8 * 1024, b'.');
        b
    };
    for i in 0..150 {
        put_object(
            &http,
            &endpoint,
            bucket,
            &format!("obj-{i:03}.json"),
            body(i),
            "application/json",
        )
        .await;
    }
    let admin = admin_http_client(&endpoint).await;
    let resp = start_migrate(&admin, &endpoint, bucket, "dst", false).await;
    assert_eq!(resp.status(), 202);
    for _ in 0..1000 {
        let job = newest_job(&admin, &endpoint).await;
        if job["phase"] == "copy" && job["progress"]["processed"].as_i64().unwrap_or(0) >= 10 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    // Level high of the spool disk (8% free): no backend write is refused.
    set_free(8);
    let mut deleted = false;
    for _ in 0..300 {
        let resp = http
            .delete(format!("{endpoint}/{bucket}/obj-000.json"))
            .send()
            .await
            .unwrap();
        if resp.status().as_u16() == 204 {
            deleted = true;
            break;
        }
        assert_eq!(resp.status().as_u16(), 503, "armed until the wait starts");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(deleted, "a delete never passed while the job waited");
    let job = newest_migrate_job(&admin, &endpoint).await;
    assert_ne!(job["status"], "failed", "a refused copy is a wait: {job}");
    assert!(
        job["progress"]["failed"].as_i64().unwrap_or(0) == 0,
        "no failure recorded: {job}"
    );

    set_free(50);
    wait_job_done(&admin, &endpoint, bucket).await;
    let job = newest_migrate_job(&admin, &endpoint).await;
    assert_eq!(job["status"], "succeeded", "job: {job}");
    assert_eq!(
        bucket_backend(&admin, &endpoint, bucket).await.as_deref(),
        Some("dst")
    );
    let gone = http
        .head(format!("{endpoint}/{bucket}/obj-000.json"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        gone.status().as_u16(),
        404,
        "obj-000 came back after the flip"
    );
    for n in [1usize, 75, 149] {
        assert_eq!(
            get_bytes(&http, &endpoint, bucket, &format!("obj-{n:03}.json")).await,
            body(n),
            "obj-{n:03}"
        );
    }
}

/// The newest MIGRATE row of the jobs list (a delete job is newer).
async fn newest_migrate_job(admin: &reqwest::Client, endpoint: &str) -> serde_json::Value {
    let v: serde_json::Value = admin
        .get(format!("{endpoint}/_/api/admin/jobs"))
        .send()
        .await
        .expect("jobs GET failed")
        .json()
        .await
        .expect("jobs not JSON");
    v["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|j| j["kind"] == "migrate")
        .cloned()
        .unwrap_or(serde_json::Value::Null)
}
