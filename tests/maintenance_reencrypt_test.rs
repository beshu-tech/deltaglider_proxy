// SPDX-License-Identifier: GPL-3.0-only

//! Integration tests for the one-off bucket re-encryption maintenance job
//! (`src/maintenance/`): durable job rows, the per-bucket WRITE gate
//! (503 SlowDown on writes, reads stay up), idempotent skip, deltaspace
//! reference re-encryption, and the decrypt-on-disable marker-stripping
//! regression.
//!
//! Filesystem backend only — no MinIO needed.

mod common;

use common::{
    admin_http_client, generate_binary, get_bytes, mutate_binary, put_object, TestServer,
};

const KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const KEY_ID: &str = "maint-test-key-1";
const PLAINTEXT_MARKER: &[u8] = b"MAINT_PLAINTEXT_MARKER_0123456789";

// ── Admin API helpers ──────────────────────────────────────────────────────

async fn put_storage_encryption(admin: &reqwest::Client, endpoint: &str, body: serde_json::Value) {
    let resp = admin
        .put(format!("{endpoint}/_/api/admin/config/section/storage"))
        .json(&serde_json::json!({ "backend_encryption": body }))
        .send()
        .await
        .expect("section PUT failed");
    assert!(
        resp.status().is_success(),
        "storage section PUT failed: {} {}",
        resp.status(),
        resp.text().await.unwrap_or_default()
    );
}

async fn enable_encryption(admin: &reqwest::Client, endpoint: &str) {
    put_storage_encryption(
        admin,
        endpoint,
        serde_json::json!({ "mode": "aes256-gcm-proxy", "key": KEY, "key_id": KEY_ID }),
    )
    .await;
}

async fn start_reencrypt(
    admin: &reqwest::Client,
    endpoint: &str,
    bucket: &str,
) -> serde_json::Value {
    let resp = admin
        .post(format!("{endpoint}/_/api/admin/jobs/reencrypt"))
        .json(&serde_json::json!({ "buckets": [bucket] }))
        .send()
        .await
        .expect("reencrypt POST failed");
    assert!(
        resp.status().is_success(),
        "reencrypt POST failed: {}",
        resp.status()
    );
    resp.json().await.expect("reencrypt response not JSON")
}

/// Poll the session-light bucket endpoint until no job is active.
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
    panic!("maintenance job on '{bucket}' did not finish within 60s");
}

/// Newest job row from the admin list.
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

async fn outbox_total(admin: &reqwest::Client, endpoint: &str) -> i64 {
    let v: serde_json::Value = admin
        .get(format!("{endpoint}/_/api/admin/event-outbox?limit=1"))
        .send()
        .await
        .expect("outbox GET failed")
        .json()
        .await
        .expect("outbox not JSON");
    v["total"].as_i64().unwrap_or(0)
}

/// Seed: `n` plaintext text objects (each embedding the marker) + a
/// similar .zip pair under `rel/` so the bucket grows a deltaspace with
/// a reference.bin.
async fn seed_bucket(http: &reqwest::Client, endpoint: &str, bucket: &str, n: usize) -> Vec<u8> {
    for i in 0..n {
        let body = [PLAINTEXT_MARKER, format!(" object {i}").as_bytes()].concat();
        put_object(
            http,
            endpoint,
            bucket,
            &format!("plain-{i:02}.json"),
            body,
            "application/json",
        )
        .await;
    }
    let base = generate_binary(100_000, 42);
    // mutate_binary is rng-based, NOT deterministic — return the exact
    // bytes so the post-job roundtrip can compare against them.
    let variant = mutate_binary(&base, 0.01);
    put_object(
        http,
        endpoint,
        bucket,
        "rel/base.zip",
        base,
        "application/zip",
    )
    .await;
    put_object(
        http,
        endpoint,
        bucket,
        "rel/v1.zip",
        variant.clone(),
        "application/zip",
    )
    .await;
    variant
}

fn assert_file_lacks_marker(path: &std::path::Path) {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
    assert!(
        !bytes
            .windows(PLAINTEXT_MARKER.len())
            .any(|w| w == PLAINTEXT_MARKER),
        "{path:?} still contains plaintext marker — not encrypted at rest"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// Full enable → re-encrypt cycle
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_reencrypt_full_cycle() {
    let bucket = "maintbkt";
    let server = TestServer::builder().bucket(bucket).build().await;
    let http = reqwest::Client::new();
    let endpoint = server.endpoint();

    let v1_expected = seed_bucket(&http, &endpoint, bucket, 40).await;

    let admin = admin_http_client(&endpoint).await;
    let outbox_before = outbox_total(&admin, &endpoint).await;

    // Flip the backend to proxy-AES. New writes encrypt; the 42 seeded
    // objects are still plaintext on disk.
    enable_encryption(&admin, &endpoint).await;

    let res = start_reencrypt(&admin, &endpoint, bucket).await;
    assert_eq!(
        res["started"][0]["bucket"], bucket,
        "job should start: {res}"
    );

    // ── While the job runs: writes 503-SlowDown, reads stay up. ──
    // The gate arms synchronously inside the POST handler, so this PUT
    // is deterministically gated unless the whole 42-object job already
    // finished — which a fresh proxy can't do in one local round-trip.
    let put_resp = http
        .put(format!("{endpoint}/{bucket}/gate-probe.txt"))
        .body("blocked?")
        .send()
        .await
        .expect("gated PUT request failed to send");
    assert_eq!(
        put_resp.status(),
        503,
        "writes must be gated during the job"
    );
    let put_body = put_resp.text().await.unwrap_or_default();
    assert!(
        put_body.contains("SlowDown"),
        "gated write should be an S3 SlowDown error, got: {put_body}"
    );
    let read_back = get_bytes(&http, &endpoint, bucket, "plain-00.json").await;
    assert!(
        read_back.starts_with(PLAINTEXT_MARKER),
        "reads must keep working during the job"
    );

    wait_job_done(&admin, &endpoint, bucket).await;

    // ── Job row: completed, everything rewritten, nothing failed. ──
    let job = newest_job(&admin, &endpoint).await;
    assert_eq!(job["status"], "succeeded", "job: {job}");
    assert_eq!(job["progress"]["failed"], 0, "job: {job}");
    assert_eq!(job["progress"]["total"], 42, "job: {job}");
    assert_eq!(
        job["progress"]["processed"], 42,
        "all objects were plaintext: {job}"
    );
    assert_eq!(job["percent"], 100, "job: {job}");

    // The dg-encrypted / dg-encryption-key-id markers are INTERNAL —
    // the S3 adapter strips dg-* from client responses (transparency),
    // so they are asserted behaviorally: the second run below skipping
    // every object proves each one carries the marker WITH the matching
    // key id (that's exactly the `needs_rewrite` predicate).

    // ── On-disk ciphertext: objects AND the deltaspace reference. ──
    let data_dir = server
        .data_dir()
        .expect("filesystem backend has a data dir");
    assert_file_lacks_marker(
        &data_dir
            .join(bucket)
            .join("deltaspaces")
            .join("plain-00.json"),
    );
    assert_file_lacks_marker(
        &data_dir
            .join(bucket)
            .join("deltaspaces")
            .join("plain-39.json"),
    );
    let reference = data_dir
        .join(bucket)
        .join("deltaspaces")
        .join("rel")
        .join("reference.bin");
    assert!(reference.exists(), "deltaspace reference should exist");
    let ref_bytes = std::fs::read(&reference).unwrap();
    let probe = &generate_binary(100_000, 42)[..64];
    assert!(
        !ref_bytes.windows(64).any(|w| w == probe),
        "reference.bin should be ciphertext after the references phase"
    );

    // ── Reads reconstruct everything transparently. ──
    for key in [
        "plain-00.json",
        "plain-39.json",
        "rel/base.zip",
        "rel/v1.zip",
    ] {
        let bytes = get_bytes(&http, &endpoint, bucket, key).await;
        assert!(!bytes.is_empty(), "GET {key} should round-trip");
    }
    let v1 = get_bytes(&http, &endpoint, bucket, "rel/v1.zip").await;
    assert_eq!(v1, v1_expected, "delta reconstruction after re-encryption");

    // ── No spurious object events from the rewrite. ──
    assert_eq!(
        outbox_total(&admin, &endpoint).await,
        outbox_before,
        "the maintenance job must not enqueue outbox events"
    );

    // ── Gate released: writes work again. ──
    put_object(
        &http,
        &endpoint,
        bucket,
        "after.txt",
        b"after".to_vec(),
        "text/plain",
    )
    .await;

    // ── Second run is a pure no-op (idempotency). ──
    start_reencrypt(&admin, &endpoint, bucket).await;
    wait_job_done(&admin, &endpoint, bucket).await;
    let job2 = newest_job(&admin, &endpoint).await;
    assert_eq!(job2["status"], "succeeded", "job2: {job2}");
    assert_eq!(
        job2["progress"]["processed"], 0,
        "everything already encrypted: {job2}"
    );
    assert_eq!(
        job2["progress"]["skipped"], 43,
        "42 seeded + after.txt: {job2}"
    );
}

// ═══════════════════════════════════════════════════════════════════════════
// Disable → decrypt (the stale-marker corruption regression)
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_decrypt_after_disable_strips_markers() {
    let bucket = "maintdec";
    let server = TestServer::builder().bucket(bucket).build().await;
    let http = reqwest::Client::new();
    let endpoint = server.endpoint();
    let admin = admin_http_client(&endpoint).await;

    // Write objects while encryption is ON — they land encrypted.
    enable_encryption(&admin, &endpoint).await;
    let body = [PLAINTEXT_MARKER, b" secret"].concat();
    put_object(
        &http,
        &endpoint,
        bucket,
        "doc.json",
        body.clone(),
        "text/plain",
    )
    .await;
    let data_dir = server.data_dir().unwrap();
    assert_file_lacks_marker(&data_dir.join(bucket).join("deltaspaces").join("doc.json"));

    // Disable encryption, keeping the legacy shim so the job (and any
    // client) can still READ the old objects during the transition.
    put_storage_encryption(
        &admin,
        &endpoint,
        serde_json::json!({ "mode": "none", "legacy_key": KEY, "legacy_key_id": KEY_ID }),
    )
    .await;

    start_reencrypt(&admin, &endpoint, bucket).await;
    wait_job_done(&admin, &endpoint, bucket).await;
    let job = newest_job(&admin, &endpoint).await;
    assert_eq!(job["status"], "succeeded", "job: {job}");
    assert_eq!(job["progress"]["failed"], 0, "job: {job}");
    assert_eq!(job["progress"]["processed"], 1, "job: {job}");

    // The marker must be GONE — a decrypted object that kept its
    // dg-encrypted metadata would fail AEAD on every read. The GET below
    // succeeding IS the regression assertion (adapter strips dg-* from
    // responses, so it can't be checked via HEAD).
    // Plaintext on disk, readable through the API.
    let disk = std::fs::read(data_dir.join(bucket).join("deltaspaces").join("doc.json")).unwrap();
    assert!(
        disk.windows(PLAINTEXT_MARKER.len())
            .any(|w| w == PLAINTEXT_MARKER),
        "object should be plaintext on disk after decrypt"
    );
    assert_eq!(get_bytes(&http, &endpoint, bucket, "doc.json").await, body);
}

// ═══════════════════════════════════════════════════════════════════════════
// Validation + cancel
// ═══════════════════════════════════════════════════════════════════════════

#[tokio::test]
async fn test_reencrypt_validation_errors() {
    let bucket = "maintval";
    let server = TestServer::builder().bucket(bucket).build().await;
    let endpoint = server.endpoint();
    let admin = admin_http_client(&endpoint).await;

    let res = start_reencrypt(&admin, &endpoint, "nosuchbucket").await;
    assert!(res["started"].as_array().unwrap().is_empty(), "{res}");
    assert!(
        res["errors"][0]["error"]
            .as_str()
            .unwrap()
            .contains("not found"),
        "{res}"
    );

    // Duplicate-active-job conflict.
    let http = reqwest::Client::new();
    let _ = seed_bucket(&http, &endpoint, bucket, 30).await;
    let admin2 = admin_http_client(&endpoint).await;
    enable_encryption(&admin2, &endpoint).await;
    let first = start_reencrypt(&admin2, &endpoint, bucket).await;
    assert_eq!(first["started"][0]["bucket"], bucket, "{first}");
    let dup = start_reencrypt(&admin2, &endpoint, bucket).await;
    assert!(
        dup["errors"][0]["error"]
            .as_str()
            .unwrap()
            .contains("already active"),
        "{dup}"
    );
    wait_job_done(&admin2, &endpoint, bucket).await;
}

#[tokio::test]
async fn test_cancel_releases_gate() {
    let bucket = "maintcan";
    let server = TestServer::builder().bucket(bucket).build().await;
    let http = reqwest::Client::new();
    let endpoint = server.endpoint();
    let admin = admin_http_client(&endpoint).await;

    let _ = seed_bucket(&http, &endpoint, bucket, 60).await;
    enable_encryption(&admin, &endpoint).await;
    let res = start_reencrypt(&admin, &endpoint, bucket).await;
    let job_id = res["started"][0]["job_id"].as_i64().expect("job id");

    let cancel = admin
        .post(format!(
            "{endpoint}/_/api/admin/jobs/maintenance:{job_id}/cancel"
        ))
        .send()
        .await
        .expect("cancel POST failed");
    // Either the job was still active (200: cancelled/cancelling) or it
    // already completed on a fast machine (409). Both are valid ends.
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
        "terminal state expected, got {job}"
    );

    // Whatever the outcome, the gate must be released.
    put_object(
        &http,
        &endpoint,
        bucket,
        "post-cancel.txt",
        b"ok".to_vec(),
        "text/plain",
    )
    .await;
}

// ═══════════════════════════════════════════════════════════════════════════
// Key ROTATION A → B (the headline correctness path)
// ═══════════════════════════════════════════════════════════════════════════

const KEY_B: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";
const KEY_B_ID: &str = "maint-test-key-2";

/// The subsystem's whole reason to exist: rotate a bucket's encryption key from
/// A to B (A retained as the legacy decrypt shim), re-encrypt, and prove every
/// object is (a) readable byte-identical, (b) re-stamped with B's key-id on
/// disk, (c) idempotent on a second run, and (d) STILL readable after A is
/// retired (legacy shim removed) — i.e. fully migrated off A.
#[tokio::test]
async fn test_reencrypt_key_rotation_a_to_b() {
    let bucket = "maintrot";
    let server = TestServer::builder().bucket(bucket).build().await;
    let http = reqwest::Client::new();
    let endpoint = server.endpoint();
    let admin = admin_http_client(&endpoint).await;

    // Encrypt under key A, seed passthrough + a delta pair (all stamped A).
    enable_encryption(&admin, &endpoint).await;
    let v1_expected = seed_bucket(&http, &endpoint, bucket, 8).await;
    let data_dir = server.data_dir().unwrap();

    // Every encrypted object on disk is stamped with A's key-id.
    let stamped_kids = |dir: &std::path::Path| -> Vec<String> {
        common::read_xattr_metadata(dir)
            .into_iter()
            .filter_map(|(_, v)| {
                v.get("user_metadata")
                    .and_then(|um| um.get("dg-encryption-key-id"))
                    .and_then(|k| k.as_str())
                    .map(String::from)
            })
            .collect()
    };
    let before = stamped_kids(data_dir);
    assert!(
        !before.is_empty(),
        "objects must be encrypted before rotation"
    );
    assert!(
        before.iter().all(|k| k == KEY_ID),
        "all objects stamped with A before rotation: {before:?}"
    );

    // ── Rotate: B primary, A retained as legacy decrypt shim. ──
    put_storage_encryption(
        &admin,
        &endpoint,
        serde_json::json!({
            "mode": "aes256-gcm-proxy",
            "key": KEY_B, "key_id": KEY_B_ID,
            "legacy_key": KEY, "legacy_key_id": KEY_ID,
        }),
    )
    .await;

    start_reencrypt(&admin, &endpoint, bucket).await;
    wait_job_done(&admin, &endpoint, bucket).await;
    let job = newest_job(&admin, &endpoint).await;
    assert_eq!(job["status"], "succeeded", "rotation job: {job}");
    assert_eq!(job["progress"]["failed"], 0, "rotation job: {job}");

    // (a) readable byte-identical under B; (b) re-stamped to B on disk.
    assert_eq!(
        get_bytes(&http, &endpoint, bucket, "plain-00.json").await,
        [PLAINTEXT_MARKER, b" object 0"].concat()
    );
    assert_eq!(
        get_bytes(&http, &endpoint, bucket, "rel/v1.zip").await,
        v1_expected,
        "delta object must reconstruct byte-identical under B"
    );
    let after = stamped_kids(data_dir);
    assert!(
        after.iter().all(|k| k == KEY_B_ID),
        "all objects re-stamped to B after rotation: {after:?}"
    );

    // (c) idempotent: a 2nd rotation re-encrypt skips everything.
    start_reencrypt(&admin, &endpoint, bucket).await;
    wait_job_done(&admin, &endpoint, bucket).await;
    let job2 = newest_job(&admin, &endpoint).await;
    assert_eq!(
        job2["progress"]["processed"], 0,
        "2nd run no rewrites: {job2}"
    );

    // (d) retire A (drop the legacy shim) — everything still readable under B.
    put_storage_encryption(
        &admin,
        &endpoint,
        serde_json::json!({ "mode": "aes256-gcm-proxy", "key": KEY_B, "key_id": KEY_B_ID }),
    )
    .await;
    assert_eq!(
        get_bytes(&http, &endpoint, bucket, "plain-00.json").await,
        [PLAINTEXT_MARKER, b" object 0"].concat(),
        "object must stay readable after A is retired"
    );
    assert_eq!(
        get_bytes(&http, &endpoint, bucket, "rel/v1.zip").await,
        v1_expected,
        "delta must stay readable after A is retired"
    );
}

/// A rotation to B WITHOUT the legacy shim, run against a bucket that still has
/// A-stamped objects: reading an un-rewritten A object must HARD-FAIL (never
/// serve ciphertext) with the rotation-hint error — pins pick_decrypt_key.
#[tokio::test]
async fn test_read_after_rotation_without_shim_hard_fails() {
    let bucket = "maintnoshim";
    let server = TestServer::builder().bucket(bucket).build().await;
    let http = reqwest::Client::new();
    let endpoint = server.endpoint();
    let admin = admin_http_client(&endpoint).await;

    enable_encryption(&admin, &endpoint).await;
    let body = [PLAINTEXT_MARKER, b" secret"].concat();
    put_object(&http, &endpoint, bucket, "a.json", body, "text/plain").await;

    // Rotate to B with NO legacy shim, and DON'T re-encrypt — a.json stays A.
    put_storage_encryption(
        &admin,
        &endpoint,
        serde_json::json!({ "mode": "aes256-gcm-proxy", "key": KEY_B, "key_id": KEY_B_ID }),
    )
    .await;

    // GET must fail (wrong/absent key), never return ciphertext.
    let resp = http
        .get(format!("{endpoint}/{bucket}/a.json"))
        .send()
        .await
        .expect("GET sent");
    assert!(
        resp.status().is_server_error() || resp.status().is_client_error(),
        "read of an A-stamped object under B-with-no-shim must fail, got {}",
        resp.status()
    );
}

/// Crash-resume: a re-encrypt job interrupted by a process restart must resume
/// from its cursor on boot (running→queued reconcile) and finish encrypting
/// EVERY object — no half-encrypted bucket, no lost work. The boot reconcile +
/// resume_token machinery is unit-tested; this pins the observable contract
/// end-to-end across a real restart.
#[tokio::test]
async fn test_reencrypt_resumes_after_restart() {
    let bucket = "maintresume";
    let mut server = TestServer::builder().bucket(bucket).build().await;
    let http = reqwest::Client::new();
    let endpoint = server.endpoint();
    let admin = admin_http_client(&endpoint).await;

    // Seed a large-ish plaintext set so the job is genuinely mid-flight when we
    // restart (each object is a small copy — enough pages to interrupt).
    const N: usize = 60;
    for i in 0..N {
        let body = [PLAINTEXT_MARKER, format!(" obj {i}").as_bytes()].concat();
        put_object(
            &http,
            &endpoint,
            bucket,
            &format!("plain-{i:03}.json"),
            body,
            "application/json",
        )
        .await;
    }

    enable_encryption(&admin, &endpoint).await;
    start_reencrypt(&admin, &endpoint, bucket).await;

    // Give the job a beat to start chewing pages, then kill + restart mid-run.
    // Re-inject the bootstrap hash: a prior storage section-PUT re-persists the
    // config and may not round-trip bootstrap_password_hash, so the fresh boot
    // would otherwise auto-generate a new one and 401 the admin login.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    server
        .respawn_with_env(&[(
            "DGP_BOOTSTRAP_PASSWORD_HASH",
            common::TEST_BOOTSTRAP_PASSWORD_HASH,
        )])
        .await;
    let endpoint = server.endpoint();
    let admin = admin_http_client(&endpoint).await;

    // Boot reconcile requeues the interrupted job; the worker resumes it. Wait
    // for the bucket to go idle, then assert the FINAL state is fully encrypted.
    wait_job_done(&admin, &endpoint, bucket).await;

    // Every object is ciphertext on disk (no plaintext marker survives) — the
    // resume finished the work the crash interrupted.
    let data_dir = server.data_dir().unwrap();
    for i in 0..N {
        let p = data_dir
            .join(bucket)
            .join("deltaspaces")
            .join(format!("plain-{i:03}.json"));
        if p.exists() {
            assert_file_lacks_marker(&p);
        }
    }
    // And every object still reads back correctly through the API.
    for i in [0usize, N / 2, N - 1] {
        assert_eq!(
            get_bytes(&http, &endpoint, bucket, &format!("plain-{i:03}.json")).await,
            [PLAINTEXT_MARKER, format!(" obj {i}").as_bytes()].concat(),
        );
    }
}
