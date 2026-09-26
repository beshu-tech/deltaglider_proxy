// SPDX-License-Identifier: BUSL-1.1

//! Config-DB key lifecycle at boot (filesystem backend, no MinIO).
//!
//! The SQLCipher key comes from `DGP_CONFIG_DB_KEY` or from the key file next
//! to the DB, never from the bootstrap password hash. These tests prove:
//! a bootstrap password change keeps the IAM DB; a DB keyed with the old
//! hash-derived key migrates on boot; a wrong DB key parks the good DB as
//! `.db.bak` and locks the node (sticky over re-boots), and the correct key
//! promotes the backup back into place.

use crate::common;

use common::{admin_http_client, admin_http_client_with_password, TestServer};
use serde_json::json;

async fn whoami(endpoint: &str) -> serde_json::Value {
    reqwest::Client::new()
        .get(format!("{endpoint}/_/api/whoami"))
        .send()
        .await
        .expect("whoami request")
        .json()
        .await
        .expect("whoami JSON")
}

/// Unsigned S3 PUT — under the config-DB lock the auth gate answers 503
/// BEFORE any signature check; on a healthy server it answers 403 instead.
async fn raw_put_status(endpoint: &str, bucket: &str) -> u16 {
    reqwest::Client::new()
        .put(format!("{endpoint}/{bucket}/mismatch-probe.bin"))
        .body(b"probe".to_vec())
        .send()
        .await
        .expect("raw PUT")
        .status()
        .as_u16()
}

fn assert_locked(who: &serde_json::Value, ctx: &str) {
    assert_eq!(
        who["config_db_mismatch"], true,
        "{ctx}: whoami must report config_db_mismatch, got: {who}"
    );
    assert_eq!(
        who["lock_state"], "locked",
        "{ctx}: whoami must report lock_state=locked, got: {who}"
    );
}

/// Two distinct, valid `DGP_CONFIG_DB_KEY` values.
const KEY_GOOD: &str = "good-config-db-key-0123456789abcdef0123456789";
const KEY_WRONG: &str = "wrong-config-db-key-0123456789abcdef012345678";

async fn create_alice(endpoint: &str) {
    let admin = admin_http_client(endpoint).await;
    let resp = admin
        .post(format!("{endpoint}/_/api/admin/users"))
        .json(&json!({
            "name": "alice",
            "permissions": [{ "effect": "Allow", "actions": ["*"], "resources": ["*"] }],
        }))
        .send()
        .await
        .expect("create user");
    assert_eq!(resp.status().as_u16(), 201, "create user failed");
}

async fn user_names(admin: &reqwest::Client, endpoint: &str) -> Vec<String> {
    let users: serde_json::Value = admin
        .get(format!("{endpoint}/_/api/admin/users"))
        .send()
        .await
        .expect("list users")
        .json()
        .await
        .expect("users JSON");
    users
        .as_array()
        .expect("users array")
        .iter()
        .filter_map(|u| u["name"].as_str().map(str::to_string))
        .collect()
}

fn data_dir(server: &TestServer) -> std::path::PathBuf {
    server
        .config_path()
        .parent()
        .expect("config dir")
        .to_path_buf()
}

/// S8: the bootstrap password hash is only the admin-login verifier. A new
/// hash (the effect of `--set-bootstrap-password`) must keep the IAM DB.
#[tokio::test]
async fn bootstrap_password_change_keeps_the_config_db() {
    let mut server = TestServer::builder()
        .auth("testkey", "testsecret")
        .build()
        .await;
    let endpoint = server.endpoint();
    create_alice(&endpoint).await;

    let new_password = "a-brand-new-bootstrap-password";
    let new_hash = bcrypt::hash(new_password, 4).unwrap();
    server
        .respawn_with_env(&[("DGP_BOOTSTRAP_PASSWORD_HASH", &new_hash)])
        .await;

    let who = whoami(&endpoint).await;
    assert_ne!(
        who["config_db_mismatch"], true,
        "a bootstrap password change must not lock the config DB, got: {who}"
    );
    assert!(
        !data_dir(&server).join("deltaglider_config.db.bak").exists(),
        "a bootstrap password change must not park the config DB"
    );
    let admin = admin_http_client_with_password(&endpoint, new_password).await;
    let names = user_names(&admin, &endpoint).await;
    assert!(
        names.iter().any(|n| n == "alice"),
        "the IAM DB must survive a bootstrap password change, got {names:?}"
    );
}

/// S8 upgrade path: a DB encrypted with the previous hash-derived key opens
/// on boot and is re-encrypted with the new key (here: a fresh key file).
#[tokio::test]
async fn legacy_hash_keyed_db_is_rekeyed_on_boot() {
    let mut server = TestServer::builder()
        .auth("testkey", "testsecret")
        .build()
        .await;
    let endpoint = server.endpoint();
    create_alice(&endpoint).await;

    let dir = data_dir(&server);
    let db_path = dir.join("deltaglider_config.db");
    let key_path = dir.join("deltaglider_config.db.key");
    server.kill();

    // Rebuild the pre-upgrade state: the DB keyed with the bootstrap hash and
    // no key file on disk.
    let file_key = std::fs::read_to_string(&key_path).expect("first boot writes the key file");
    {
        let db = deltaglider_proxy::config_db::ConfigDb::open_or_create(&db_path, file_key.trim())
            .expect("DB opens with the key file");
        db.rekey(common::TEST_BOOTSTRAP_PASSWORD_HASH)
            .expect("rekey to the legacy key");
    }
    std::fs::remove_file(&key_path).unwrap();

    server.respawn_with_env(&[]).await;
    let who = whoami(&endpoint).await;
    assert_ne!(
        who["config_db_mismatch"], true,
        "a legacy hash-keyed DB must migrate, not lock, got: {who}"
    );
    let admin = admin_http_client(&endpoint).await;
    let names = user_names(&admin, &endpoint).await;
    assert!(
        names.iter().any(|n| n == "alice"),
        "the migrated DB must keep its users, got {names:?}"
    );
    server.kill();

    let new_key = std::fs::read_to_string(&key_path).expect("boot writes a new key file");
    assert!(
        deltaglider_proxy::config_db::ConfigDb::open_or_create(&db_path, new_key.trim()).is_ok(),
        "the DB must be re-encrypted with the key-file key"
    );
    assert!(
        deltaglider_proxy::config_db::ConfigDb::open_or_create(
            &db_path,
            common::TEST_BOOTSTRAP_PASSWORD_HASH
        )
        .is_err(),
        "the DB must no longer open with the bootstrap hash"
    );
}

#[tokio::test]
async fn wrong_db_key_boot_stays_locked_and_correct_key_promotes_backup() {
    let mut server = TestServer::builder()
        .auth("testkey", "testsecret")
        .env("DGP_CONFIG_DB_KEY", KEY_GOOD)
        .build()
        .await;
    let endpoint = server.endpoint();
    let dir = data_dir(&server);
    let bak_path = dir.join("deltaglider_config.db.bak");
    let discarded_path = dir.join("deltaglider_config.db.discarded");
    create_alice(&endpoint).await;
    assert!(
        !dir.join("deltaglider_config.db.key").exists(),
        "an env key must not write a key file"
    );

    // ── Boot 2: wrong key → good DB parked as .db.bak, S3 locked ──
    server
        .respawn_with_env(&[("DGP_CONFIG_DB_KEY", KEY_WRONG)])
        .await;
    assert_locked(&whoami(&endpoint).await, "boot 2 (first wrong-key boot)");
    assert_eq!(
        raw_put_status(&endpoint, server.bucket()).await,
        503,
        "boot 2: S3 writes must be rejected with 503 while locked"
    );
    assert!(bak_path.exists(), "boot 2 must park the good DB as .db.bak");

    // ── Boot 3: SAME wrong key opens the empty DB fine; the lingering
    // .db.bak must keep the node locked (sticky mismatch). ──
    server
        .respawn_with_env(&[("DGP_CONFIG_DB_KEY", KEY_WRONG)])
        .await;
    assert_locked(&whoami(&endpoint).await, "boot 3 (second wrong-key boot)");
    assert_eq!(
        raw_put_status(&endpoint, server.bucket()).await,
        503,
        "boot 3: S3 must STAY locked on a re-boot with the same wrong key"
    );
    assert!(
        bak_path.exists(),
        "boot 3 must leave the good .db.bak alone"
    );

    // ── Boot 4: correct key → promote .db.bak ──
    server
        .respawn_with_env(&[("DGP_CONFIG_DB_KEY", KEY_GOOD)])
        .await;
    let who = whoami(&endpoint).await;
    assert_ne!(
        who["config_db_mismatch"], true,
        "boot 4: the correct key must clear the mismatch, got: {who}"
    );
    assert!(
        !bak_path.exists(),
        "boot 4: promote must consume .db.bak (recovery terminates)"
    );
    assert!(
        discarded_path.exists(),
        "boot 4: promote must KEEP .db.discarded — the swapped-out live file is \
         unverifiable and deleting it could destroy data under another key"
    );

    let admin = admin_http_client(&endpoint).await;
    let names = user_names(&admin, &endpoint).await;
    assert!(
        names.iter().any(|n| n == "alice"),
        "boot 4: promoted DB must still contain user 'alice', got {names:?}"
    );

    let client = server.s3_client().await;
    client
        .put_object()
        .bucket(server.bucket())
        .key("recovered.bin")
        .body(aws_sdk_s3::primitives::ByteStream::from(
            b"recovered".to_vec(),
        ))
        .send()
        .await
        .expect("boot 4: S3 PUT must succeed after recovery");
}

/// Key rotation: the new key in DGP_CONFIG_DB_KEY, the old one in
/// DGP_CONFIG_DB_KEY_PREVIOUS. The DB migrates; after that the old key is
/// no longer needed.
#[tokio::test]
async fn config_db_key_rotation_with_the_previous_key() {
    let mut server = TestServer::builder()
        .auth("testkey", "testsecret")
        .env("DGP_CONFIG_DB_KEY", KEY_GOOD)
        .build()
        .await;
    let endpoint = server.endpoint();
    create_alice(&endpoint).await;

    server
        .respawn_with_env(&[
            ("DGP_CONFIG_DB_KEY", KEY_WRONG),
            ("DGP_CONFIG_DB_KEY_PREVIOUS", KEY_GOOD),
        ])
        .await;
    let who = whoami(&endpoint).await;
    assert_ne!(
        who["config_db_mismatch"], true,
        "rotation locked the DB: {who}"
    );

    // The previous key is gone; the DB opens with the new key alone.
    server
        .respawn_with_env(&[("DGP_CONFIG_DB_KEY", KEY_WRONG)])
        .await;
    let who = whoami(&endpoint).await;
    assert_ne!(
        who["config_db_mismatch"], true,
        "the DB did not move to the new key: {who}"
    );
    let names = user_names(&admin_http_client(&endpoint).await, &endpoint).await;
    assert!(names.iter().any(|n| n == "alice"), "got {names:?}");
}
