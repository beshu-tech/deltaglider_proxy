// SPDX-License-Identifier: GPL-3.0-only

//! HA multi-replica config-DB sync tests.
//!
//! `src/config_db_sync.rs` is the only supported mode for running
//! DeltaGlider Proxy in HA (multiple replicas sharing IAM state). It
//! was previously covered by a single export/import smoke test in
//! `config_sync_test.rs` (misleadingly named — that tests the manual
//! backup/restore admin endpoints, not the automated S3 sync).
//!
//! This file exercises the actual sync code path:
//!
//!   1. Startup pull — replica B boots, pulls A's state from S3.
//!   2. Operator-triggered propagation — A mutates, B calls sync-now,
//!      observes the change (the real-world equivalent of waiting
//!      for the 5-min poll tick).
//!   3. Wrong-passphrase rejection — replica with a different
//!      bootstrap password must NOT clobber its local state with an
//!      undecryptable download.
//!   4. ETag no-op — calling sync-now when already current is a
//!      cheap HEAD that does no DB reopen.
//!
//! All tests require MinIO and share the storage bucket with the
//! sync bucket. Each test uses a unique `config_sync_object_key` under
//! `.deltaglider/` (UUID-based) so parallel integration-test binaries
//! do not clobber the same object in `deltaglider-test`.

mod common;

use common::{
    admin_http_client, admin_http_client_with_password, minio_endpoint_url, TestServer,
    MINIO_BUCKET,
};
use serde_json::json;
use std::sync::atomic::{AtomicU64, Ordering};
use uuid::Uuid;

/// Monotonic prefix for the S3 user names used in these tests. The
/// sync bucket is shared (it's MINIO_BUCKET), so each test seeds
/// uniquely-named users to avoid cross-test contamination when run
/// in parallel.
static TEST_USER_SEQ: AtomicU64 = AtomicU64::new(0);

/// Globally unique object key: CI runs many integration test binaries in
/// parallel (separate processes), so timestamp + per-process counters can
/// still collide across crates sharing `MINIO_BUCKET`.
fn unique_config_sync_object_key() -> String {
    format!(".deltaglider/ha-ci-{}.db", Uuid::new_v4())
}

fn unique_user_name(prefix: &str) -> String {
    let n = TEST_USER_SEQ.fetch_add(1, Ordering::SeqCst);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    format!("{prefix}-{ts}-{n}")
}

/// Startup pull: replica B starts with the same sync bucket + same
/// bootstrap password as A. B's `init_config_sync` downloads A's DB
/// and rebuilds the IAM index. Verify that a user created on A is
/// visible on B immediately after B finishes starting up.
///
/// This is the REAL onboarding path: when a new replica joins a
/// pool, it picks up state at boot. Previously only tested via the
/// manual backup/restore admin endpoint.
#[tokio::test]
async fn ha_startup_replica_pulls_state_from_s3() {
    skip_unless_minio!();

    let sync_key = unique_config_sync_object_key();

    // Server A: creates a user, which triggers an upload to S3.
    let server_a = TestServer::builder()
        .auth("HAKEY-A", "HASECRET-A-1234567890")
        .s3_endpoint(&minio_endpoint_url())
        .bucket(MINIO_BUCKET)
        .config_sync_bucket(MINIO_BUCKET)
        .config_sync_object_key(&sync_key)
        .build()
        .await;
    let admin_a = admin_http_client(&server_a.endpoint()).await;

    let user_name = unique_user_name("ha-startup");
    let resp = admin_a
        .post(format!("{}/_/api/admin/users", server_a.endpoint()))
        .json(&json!({
            "name": user_name,
            "permissions": [{"actions": ["read"], "resources": ["*"]}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201, "create user on A must succeed");

    // trigger_config_sync() fires a background tokio::spawn; wait for
    // the S3 PUT to land. 2s is a generous upper bound (MinIO local is
    // typically <50ms). The s3_client view is authoritative, so we
    // just HEAD the sync key until it appears.
    let s3 = server_a.s3_client().await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let head = s3
            .head_object()
            .bucket(MINIO_BUCKET)
            .key(&sync_key)
            .send()
            .await;
        if head.is_ok() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            panic!("config.db never appeared in sync bucket");
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // Server B: same sync bucket + same default bootstrap password.
    // Its startup sync should download A's DB and rebuild IAM.
    let server_b = TestServer::builder()
        .auth("HAKEY-B", "HASECRET-B-1234567890")
        .s3_endpoint(&minio_endpoint_url())
        .bucket(MINIO_BUCKET)
        .config_sync_bucket(MINIO_BUCKET)
        .config_sync_object_key(&sync_key)
        .build()
        .await;
    let admin_b = admin_http_client(&server_b.endpoint()).await;

    let users: Vec<serde_json::Value> = admin_b
        .get(format!("{}/_/api/admin/users", server_b.endpoint()))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        users.iter().any(|u| u["name"] == user_name),
        "B should see the user created on A after startup pull; got: {users:?}"
    );
}

/// Operator-triggered propagation: B is already running. A creates a
/// user AFTER B started. B's poll tick is 5 minutes — too slow for
/// tests. Call `POST /api/admin/config/sync-now` on B to force an
/// immediate pull. This is the same code path the periodic poll
/// uses, and the same affordance an operator would reach for when
/// they want immediate propagation.
#[tokio::test]
async fn ha_sync_now_propagates_post_startup_mutation() {
    skip_unless_minio!();

    let sync_key = unique_config_sync_object_key();

    let server_a = TestServer::builder()
        .auth("HAKEY-A2", "HASECRET-A2-1234567890")
        .s3_endpoint(&minio_endpoint_url())
        .bucket(MINIO_BUCKET)
        .config_sync_bucket(MINIO_BUCKET)
        .config_sync_object_key(&sync_key)
        .build()
        .await;
    let server_b = TestServer::builder()
        .auth("HAKEY-B2", "HASECRET-B2-1234567890")
        .s3_endpoint(&minio_endpoint_url())
        .bucket(MINIO_BUCKET)
        .config_sync_bucket(MINIO_BUCKET)
        .config_sync_object_key(&sync_key)
        .build()
        .await;

    let admin_a = admin_http_client(&server_a.endpoint()).await;
    let admin_b = admin_http_client(&server_b.endpoint()).await;

    // Baseline: whatever users already exist on B from startup sync.
    // We care about the DELTA post-mutation, not the absolute count.
    let users_before: Vec<serde_json::Value> = admin_b
        .get(format!("{}/_/api/admin/users", server_b.endpoint()))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let user_name = unique_user_name("ha-propagate");
    let resp = admin_a
        .post(format!("{}/_/api/admin/users", server_a.endpoint()))
        .json(&json!({
            "name": user_name,
            "permissions": [{"actions": ["read"], "resources": ["*"]}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201);

    // Wait for A's trigger_config_sync to actually upload. 2s ceiling.
    let s3 = server_a.s3_client().await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let head = s3
            .head_object()
            .bucket(MINIO_BUCKET)
            .key(&sync_key)
            .send()
            .await;
        if head.is_ok() {
            break;
        }
        if std::time::Instant::now() >= deadline {
            panic!("A's config.db never appeared in sync bucket");
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // Force B to pull. Returns { downloaded: true } the first time
    // (new ETag). The reopen-and-rebuild-IAM helper runs inline.
    let resp = admin_b
        .post(format!(
            "{}/_/api/admin/config/sync-now",
            server_b.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "sync-now must return 2xx when config_sync is configured, got {}",
        resp.status()
    );
    let body: serde_json::Value = resp.json().await.unwrap();

    // B now sees A's new user.
    let users_after: Vec<serde_json::Value> = admin_b
        .get(format!("{}/_/api/admin/users", server_b.endpoint()))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        users_after.iter().any(|u| u["name"] == user_name),
        "B should see A's new user after sync-now (or already from startup pull); \
         body={body:?}, users_before={}, users_after={users_after:?}",
        users_before.len()
    );
}

/// ETag optimisation: a second sync-now when B is already current
/// must report downloaded=false and not re-pull. This is the
/// bandwidth-saving invariant that makes the 5-min poll viable at
/// scale.
#[tokio::test]
async fn ha_sync_now_is_noop_when_etag_unchanged() {
    skip_unless_minio!();

    let sync_key = unique_config_sync_object_key();

    let server_a = TestServer::builder()
        .auth("HAKEY-A3", "HASECRET-A3-1234567890")
        .s3_endpoint(&minio_endpoint_url())
        .bucket(MINIO_BUCKET)
        .config_sync_bucket(MINIO_BUCKET)
        .config_sync_object_key(&sync_key)
        .build()
        .await;
    let admin_a = admin_http_client(&server_a.endpoint()).await;

    // Trigger an upload so the sync key exists with a known ETag.
    let user_name = unique_user_name("ha-etag");
    admin_a
        .post(format!("{}/_/api/admin/users", server_a.endpoint()))
        .json(&json!({ "name": user_name, "permissions": [] }))
        .send()
        .await
        .unwrap();

    // Wait for the upload.
    let s3 = server_a.s3_client().await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if s3
            .head_object()
            .bucket(MINIO_BUCKET)
            .key(&sync_key)
            .send()
            .await
            .is_ok()
        {
            break;
        }
        if std::time::Instant::now() >= deadline {
            panic!("config.db never appeared");
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let server_b = TestServer::builder()
        .auth("HAKEY-B3", "HASECRET-B3-1234567890")
        .s3_endpoint(&minio_endpoint_url())
        .bucket(MINIO_BUCKET)
        .config_sync_bucket(MINIO_BUCKET)
        .config_sync_object_key(&sync_key)
        .build()
        .await;
    let admin_b = admin_http_client(&server_b.endpoint()).await;

    // First sync-now: pulls A's state (B was just born, its
    // last_etag is None, so this downloads once).
    let first: serde_json::Value = admin_b
        .post(format!(
            "{}/_/api/admin/config/sync-now",
            server_b.endpoint()
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // It may also have downloaded on startup (init_config_sync), in
    // which case the first explicit call is already a no-op. Either
    // way, the SECOND call must be a no-op.
    let _ = first;

    let second: serde_json::Value = admin_b
        .post(format!(
            "{}/_/api/admin/config/sync-now",
            server_b.endpoint()
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        second["downloaded"], false,
        "sync-now when already current must be a no-op, got {second:?}"
    );
}

/// Cross-instance session revocation: a session minted on replica B is killed
/// by a revoke-by-identity issued on replica A. A persists the revocation
/// epoch in the synced DB and pushes it (reconcile-then-retry upload); B pulls
/// via sync-now, refreshes its revocation snapshot, and 401s the cookie.
#[tokio::test]
async fn ha_revocation_reaches_peer() {
    skip_unless_minio!();

    let sync_key = unique_config_sync_object_key();

    let server_a = TestServer::builder()
        .auth("HAKEY-A5", "HASECRET-A5-1234567890")
        .s3_endpoint(&minio_endpoint_url())
        .bucket(MINIO_BUCKET)
        .config_sync_bucket(MINIO_BUCKET)
        .config_sync_object_key(&sync_key)
        .build()
        .await;
    let admin_a = admin_http_client(&server_a.endpoint()).await;

    // IAM ADMIN user on A (login-as requires admin permissions).
    let user_name = unique_user_name("ha-revoke");
    let resp = admin_a
        .post(format!("{}/_/api/admin/users", server_a.endpoint()))
        .json(&json!({
            "name": user_name,
            "permissions": [{"actions": ["*"], "resources": ["*"]}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201, "create user on A must succeed");
    let body: serde_json::Value = resp.json().await.unwrap();
    let ak = body["access_key_id"].as_str().unwrap().to_string();
    let sk = body["secret_access_key"].as_str().unwrap().to_string();

    // Wait for A's user-create upload to land before booting B.
    let s3 = server_a.s3_client().await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if s3
            .head_object()
            .bucket(MINIO_BUCKET)
            .key(&sync_key)
            .send()
            .await
            .is_ok()
        {
            break;
        }
        if std::time::Instant::now() >= deadline {
            panic!("A's config.db never appeared in sync bucket");
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // Replica B pulls A's state at startup, so login-as works there.
    let server_b = TestServer::builder()
        .auth("HAKEY-B5", "HASECRET-B5-1234567890")
        .s3_endpoint(&minio_endpoint_url())
        .bucket(MINIO_BUCKET)
        .config_sync_bucket(MINIO_BUCKET)
        .config_sync_object_key(&sync_key)
        .build()
        .await;
    let admin_b = admin_http_client(&server_b.endpoint()).await;

    // Session minted ON B for the user (the stolen-cookie stand-in).
    let cookie_b = reqwest::Client::builder()
        .cookie_store(true)
        .no_proxy()
        .build()
        .unwrap();
    let resp = cookie_b
        .post(format!("{}/_/api/admin/login-as", server_b.endpoint()))
        .json(&json!({ "access_key_id": ak, "secret_access_key": sk }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "login-as on B must succeed after startup pull"
    );
    let resp = cookie_b
        .get(format!("{}/_/api/admin/config", server_b.endpoint()))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "B session must work pre-revoke"
    );

    // Revoke on A by identity — must be durable so B can converge.
    let resp = admin_a
        .post(format!(
            "{}/_/api/admin/sessions/revoke-user",
            server_a.endpoint()
        ))
        .json(&json!({ "identity": ak }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        body["persisted"], true,
        "revocation must be persisted to the synced DB, got {body:?}"
    );

    // B pulls the revocation. The push on A is awaited before its response,
    // but allow a few sync-now rounds in case the CAS upload needed the
    // poll-flush fallback (pushed=false is still a valid durable outcome).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let sync = admin_b
            .post(format!(
                "{}/_/api/admin/config/sync-now",
                server_b.endpoint()
            ))
            .send()
            .await
            .unwrap();
        assert!(sync.status().is_success(), "sync-now on B must be 2xx");

        let resp = cookie_b
            .get(format!("{}/_/api/admin/config", server_b.endpoint()))
            .send()
            .await
            .unwrap();
        if resp.status().as_u16() == 401 {
            break;
        }
        if std::time::Instant::now() >= deadline {
            panic!(
                "B's session for revoked identity still valid after sync-now; last status {}",
                resp.status()
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

/// Wrong-passphrase rejection: replica B boots with a DIFFERENT
/// bootstrap password than A. Its startup sync downloads A's DB,
/// the `ConfigDb::open_or_create` validation step fails (the SQLCipher
/// cipher doesn't match), and the download is discarded — B's local
/// DB is untouched.
///
/// Without this guard, B would replace its own DB with an un-
/// decryptable blob and never authenticate again. The current code
/// handles this as a WARN log (not an error), returning Ok(false)
/// from `download_if_newer`.
///
/// We can't observe the log directly, but we can observe the
/// consequence: B's admin login STILL works with B's own password
/// even after the (failed) download.
#[tokio::test]
async fn ha_replica_with_wrong_password_preserves_local_state() {
    skip_unless_minio!();

    let sync_key = unique_config_sync_object_key();

    let server_a = TestServer::builder()
        .auth("HAKEY-A4", "HASECRET-A4-1234567890")
        .s3_endpoint(&minio_endpoint_url())
        .bucket(MINIO_BUCKET)
        .config_sync_bucket(MINIO_BUCKET)
        .config_sync_object_key(&sync_key)
        .build()
        .await;
    let admin_a = admin_http_client(&server_a.endpoint()).await;
    admin_a
        .post(format!("{}/_/api/admin/users", server_a.endpoint()))
        .json(&json!({
            "name": unique_user_name("ha-wrong-pw"),
            "permissions": [{"actions": ["read"], "resources": ["*"]}]
        }))
        .send()
        .await
        .unwrap();

    // Wait for A's upload to land.
    let s3 = server_a.s3_client().await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if s3
            .head_object()
            .bucket(MINIO_BUCKET)
            .key(&sync_key)
            .send()
            .await
            .is_ok()
        {
            break;
        }
        if std::time::Instant::now() >= deadline {
            panic!("A's config.db never appeared");
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // Server B: DIFFERENT bootstrap password. Its startup attempt
    // to apply A's DB must fail the passphrase check gracefully.
    let wrong_password = "different-password-that-wont-match-A";
    let server_b = TestServer::builder()
        .auth("HAKEY-B4", "HASECRET-B4-1234567890")
        .s3_endpoint(&minio_endpoint_url())
        .bucket(MINIO_BUCKET)
        .config_sync_bucket(MINIO_BUCKET)
        .bootstrap_password(wrong_password)
        .config_sync_object_key(&sync_key)
        .build()
        .await;

    // Can still log in to B with B's own password — B's local state
    // wasn't clobbered by the failed download. (If the download had
    // overwritten B's DB with A's undecryptable blob, this login
    // would fail because B can't decrypt its own DB.)
    let _admin_b = admin_http_client_with_password(&server_b.endpoint(), wrong_password).await;

    // Further guard: A's S3 copy must still be decryptable with A's
    // password. Nothing in B's startup should have overwritten the
    // sync key. Fetch it raw and check that an admin-login against
    // A still works (end-to-end: A's DB is unchanged).
    let admin_a_again = admin_http_client(&server_a.endpoint()).await;
    let resp = admin_a_again
        .get(format!("{}/_/api/admin/users", server_a.endpoint()))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "A's admin API must still work after B's failed-passphrase sync attempt"
    );
}
