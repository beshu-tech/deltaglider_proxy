// SPDX-License-Identifier: BUSL-1.1

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

use crate::common;

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

/// S8: the config DB key is NOT the bootstrap password. Replica B with a
/// DIFFERENT bootstrap password but the SAME `DGP_CONFIG_DB_KEY` (the harness
/// default for sync servers) pulls A's IAM state at startup.
#[tokio::test]
async fn ha_replica_with_other_bootstrap_password_pulls_state() {
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
    let user_name = unique_user_name("ha-other-pw");
    let resp = admin_http_client(&server_a.endpoint())
        .await
        .post(format!("{}/_/api/admin/users", server_a.endpoint()))
        .json(&json!({
            "name": user_name,
            "permissions": [{"actions": ["read"], "resources": ["*"]}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201);
    wait_for_sync_object(&sync_key).await;

    let other_password = "another-bootstrap-password-for-B";
    let server_b = TestServer::builder()
        .auth("HAKEY-B5", "HASECRET-B5-1234567890")
        .s3_endpoint(&minio_endpoint_url())
        .bucket(MINIO_BUCKET)
        .config_sync_bucket(MINIO_BUCKET)
        .bootstrap_password(other_password)
        .config_sync_object_key(&sync_key)
        .build()
        .await;
    let admin_b = admin_http_client_with_password(&server_b.endpoint(), other_password).await;
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
        "B (other bootstrap password, same DB key) must see A's user; got: {users:?}"
    );
}

/// Wrong-key rejection: replica B boots with a DIFFERENT `DGP_CONFIG_DB_KEY`.
/// Its sync refuses A's DB (the SQLCipher key does not open it): B's local
/// DB stays intact, A's user does not reach B, and an operator-triggered
/// sync-now reports the failure instead of pretending all is current.
#[tokio::test]
async fn ha_replica_with_wrong_db_key_refuses_the_synced_db() {
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
    let user_name = unique_user_name("ha-wrong-key");
    admin_http_client(&server_a.endpoint())
        .await
        .post(format!("{}/_/api/admin/users", server_a.endpoint()))
        .json(&json!({
            "name": user_name,
            "permissions": [{"actions": ["read"], "resources": ["*"]}]
        }))
        .send()
        .await
        .unwrap();
    wait_for_sync_object(&sync_key).await;

    let server_b = TestServer::builder()
        .auth("HAKEY-B4", "HASECRET-B4-1234567890")
        .s3_endpoint(&minio_endpoint_url())
        .bucket(MINIO_BUCKET)
        .config_sync_bucket(MINIO_BUCKET)
        .env(
            "DGP_CONFIG_DB_KEY",
            "a-different-config-db-key-0123456789abcdef",
        )
        .config_sync_object_key(&sync_key)
        .build()
        .await;

    // B's own DB still opens (login works) and holds no user of A.
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
        !users.iter().any(|u| u["name"] == user_name),
        "B must not merge a DB under another key; got: {users:?}"
    );
    let sync_now = admin_b
        .post(format!(
            "{}/_/api/admin/config/sync-now",
            server_b.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        sync_now.status().as_u16(),
        502,
        "sync-now must report the refused download"
    );

    // A's admin API (and its DB) is untouched.
    let resp = admin_http_client(&server_a.endpoint())
        .await
        .get(format!("{}/_/api/admin/users", server_a.endpoint()))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
}

/// S8 upgrade path: the sync object was written by a node on the release
/// before S8, so it is keyed with the bootstrap password hash.
/// Seed the sync object with a DB under the legacy bootstrap hash, boot a
/// node on it, and return whether the node merged the seeded user.
async fn boot_on_a_legacy_sync_object(accept_legacy: bool) -> bool {
    let sync_key = unique_config_sync_object_key();
    let user_name = unique_user_name("ha-legacy");
    let dir = tempfile::tempdir().unwrap();
    let legacy = dir.path().join("legacy.db");
    {
        let db = deltaglider_proxy::config_db::ConfigDb::open_or_create(
            &legacy,
            common::TEST_BOOTSTRAP_PASSWORD_HASH,
        )
        .unwrap();
        db.create_user(&user_name, "AKLEGACY0001", "legacy-secret", true, &[])
            .unwrap();
    }
    common::minio_client()
        .await
        .put_object()
        .bucket(MINIO_BUCKET)
        .key(&sync_key)
        .body(std::fs::read(&legacy).unwrap().into())
        .send()
        .await
        .expect("seed the legacy sync object");

    let mut builder = TestServer::builder()
        .auth("HAKEY-L1", "HASECRET-L1-1234567890")
        .s3_endpoint(&minio_endpoint_url())
        .bucket(MINIO_BUCKET)
        .config_sync_bucket(MINIO_BUCKET)
        .config_sync_object_key(&sync_key);
    if accept_legacy {
        builder = builder.env("DGP_CONFIG_DB_ACCEPT_LEGACY_SYNC", "true");
    }
    let server = builder.build().await;
    if accept_legacy {
        // The merged copy was under a fallback key: the boot re-uploads it
        // under the config DB key, so the bucket leaves the hash.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let body = common::minio_client()
                .await
                .get_object()
                .bucket(MINIO_BUCKET)
                .key(&sync_key)
                .send()
                .await
                .unwrap()
                .body
                .collect()
                .await
                .unwrap()
                .into_bytes();
            std::fs::write(&legacy, &body).unwrap();
            if deltaglider_proxy::config_db::probe_key(&legacy, common::TEST_CONFIG_DB_KEY)
                .unwrap_or(false)
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the synced copy stays under the legacy hash"
            );
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        }
    }
    let users: Vec<serde_json::Value> = admin_http_client(&server.endpoint())
        .await
        .get(format!("{}/_/api/admin/users", server.endpoint()))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    users.iter().any(|u| u["name"] == user_name)
}

/// Rolling upgrade: with DGP_CONFIG_DB_ACCEPT_LEGACY_SYNC=true, a copy that an
/// old-release node wrote under the bootstrap hash merges.
#[tokio::test]
async fn ha_legacy_hash_keyed_sync_object_is_accepted_when_opted_in() {
    skip_unless_minio!();
    assert!(
        boot_on_a_legacy_sync_object(true).await,
        "a legacy hash-keyed sync object must merge with the opt-in"
    );
}

/// S8: the hash is not a secret, so by default it never opens a synced copy.
#[tokio::test]
async fn ha_legacy_hash_keyed_sync_object_is_refused_by_default() {
    skip_unless_minio!();
    assert!(
        !boot_on_a_legacy_sync_object(false).await,
        "a copy under the bootstrap hash must not merge without the opt-in"
    );
}

/// Wait until the sync object exists in MinIO (the upload is a background
/// task of the admin mutation).
async fn wait_for_sync_object(sync_key: &str) {
    let s3 = common::minio_client().await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while s3
        .head_object()
        .bucket(MINIO_BUCKET)
        .key(sync_key)
        .send()
        .await
        .is_err()
    {
        if std::time::Instant::now() >= deadline {
            panic!("the config DB never appeared in the sync bucket");
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// X-ray H10: concurrent same-node mutations must not self-clobber.
///
/// Each admin user-create fires an independent background `upload_with_reconcile`.
/// Before the per-node upload lock, two of these could interleave read-then-PUT:
/// one wins the CAS, the other 412s and its reconcile whole-table-replaces the
/// live IAM with the older remote blob — silently deleting a user for which the
/// admin already got a 201. No peer node is involved; the node loses its own
/// committed write. This creates a batch of users as fast as possible on ONE
/// sync-enabled node and asserts every one survives in that node's own DB.
#[tokio::test]
async fn ha_concurrent_same_node_creates_do_not_self_clobber() {
    skip_unless_minio!();

    let sync_key = unique_config_sync_object_key();
    let server = TestServer::builder()
        .auth("HAKEY-SC", "HASECRET-SC-1234567890")
        .s3_endpoint(&minio_endpoint_url())
        .bucket(MINIO_BUCKET)
        .config_sync_bucket(MINIO_BUCKET)
        .config_sync_object_key(&sync_key)
        .build()
        .await;
    let endpoint = server.endpoint();

    // Fire N creates concurrently so their background uploads overlap.
    let names: Vec<String> = (0..8)
        .map(|i| unique_user_name(&format!("h10-{i}")))
        .collect();
    let mut handles = Vec::new();
    for name in &names {
        let admin = admin_http_client(&endpoint).await;
        let ep = endpoint.clone();
        let name = name.clone();
        handles.push(tokio::spawn(async move {
            let resp = admin
                .post(format!("{ep}/_/api/admin/users"))
                .json(&json!({
                    "name": name,
                    "permissions": [{"actions": ["read"], "resources": ["*"]}]
                }))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status().as_u16(), 201, "create {name} must 201");
        }));
    }
    for h in handles {
        h.await.unwrap();
    }

    // Let the overlapping background uploads (and any reconcile retries) settle.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // Every user the admin got a 201 for must still be present on THIS node.
    let admin = admin_http_client(&endpoint).await;
    let users: Vec<serde_json::Value> = admin
        .get(format!("{endpoint}/_/api/admin/users"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    for name in &names {
        assert!(
            users.iter().any(|u| u["name"] == *name),
            "user {name} was 201-created but is missing — self-clobber (H10); got {} users",
            users.len()
        );
    }
}

async fn user_names(admin: &reqwest::Client, endpoint: &str) -> Vec<String> {
    let users: Vec<serde_json::Value> = admin
        .get(format!("{endpoint}/_/api/admin/users"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    users
        .iter()
        .filter_map(|u| u["name"].as_str().map(str::to_string))
        .collect()
}

async fn create_user(admin: &reqwest::Client, endpoint: &str, name: &str) {
    let resp = admin
        .post(format!("{endpoint}/_/api/admin/users"))
        .json(&json!({
            "name": name,
            "permissions": [{"actions": ["read"], "resources": ["*"]}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201, "create {name} must 201");
}

/// D16: two nodes create different users while each holds a stale copy. The
/// second upload hits a CAS conflict; the old table-replace reconcile dropped
/// that node's own new user. The three-way merge keeps both, on both nodes.
#[tokio::test]
async fn ha_concurrent_creates_on_two_nodes_both_survive() {
    skip_unless_minio!();

    let sync_key = unique_config_sync_object_key();
    let build = |key: &'static str, secret: &'static str| {
        TestServer::builder()
            .auth(key, secret)
            .s3_endpoint(&minio_endpoint_url())
            .bucket(MINIO_BUCKET)
            .config_sync_bucket(MINIO_BUCKET)
            .config_sync_object_key(&sync_key)
            .build()
    };
    let server_a = build("HAKEY-3W-A", "HASECRET-3W-A-1234567890").await;
    let server_b = build("HAKEY-3W-B", "HASECRET-3W-B-1234567890").await;
    let (ep_a, ep_b) = (server_a.endpoint(), server_b.endpoint());
    let admin_a = admin_http_client(&ep_a).await;
    let admin_b = admin_http_client(&ep_b).await;
    let sync_now = |admin: reqwest::Client, ep: String| async move {
        let resp = admin
            .post(format!("{ep}/_/api/admin/config/sync-now"))
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "sync-now: {}", resp.status());
    };

    // A seeds the bucket; B pulls it, so both share one merge base.
    let seed = unique_user_name("3w-seed");
    create_user(&admin_a, &ep_a, &seed).await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !user_names(&admin_b, &ep_b).await.contains(&seed) {
        assert!(std::time::Instant::now() < deadline, "B never saw the seed");
        sync_now(admin_b.clone(), ep_b.clone()).await;
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    // B uploads first; A's upload then conflicts and reconciles.
    let on_b = unique_user_name("3w-b");
    create_user(&admin_b, &ep_b, &on_b).await;
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    let on_a = unique_user_name("3w-a");
    create_user(&admin_a, &ep_a, &on_a).await;

    // A's reconcile brings B's user in and keeps its own.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let names = user_names(&admin_a, &ep_a).await;
        assert!(
            names.contains(&on_a),
            "A lost its own new user in the reconcile: {names:?}"
        );
        if names.contains(&on_b) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "A never merged B's user: {names:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    // B pulls the reconciled copy: everything, on both nodes.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        sync_now(admin_b.clone(), ep_b.clone()).await;
        let names = user_names(&admin_b, &ep_b).await;
        if [&seed, &on_a, &on_b].iter().all(|n| names.contains(n)) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "B is missing a user: {names:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    // A delete on B reaches A (base present, so it is a delete, not a gap).
    let users: Vec<serde_json::Value> = admin_b
        .get(format!("{ep_b}/_/api/admin/users"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let seed_id = users.iter().find(|u| u["name"] == seed).unwrap()["id"]
        .as_i64()
        .unwrap();
    let resp = admin_b
        .delete(format!("{ep_b}/_/api/admin/users/{seed_id}"))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "delete: {}", resp.status());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        sync_now(admin_a.clone(), ep_a.clone()).await;
        let names = user_names(&admin_a, &ep_a).await;
        if !names.contains(&seed) {
            assert!(names.contains(&on_a) && names.contains(&on_b), "{names:?}");
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the delete never reached A"
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}
