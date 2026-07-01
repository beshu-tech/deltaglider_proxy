// SPDX-License-Identifier: GPL-3.0-only

//! Bootstrap-password-mismatch boot lifecycle (filesystem backend, no MinIO).
//!
//! Regression: boot 1 with a wrong `DGP_BOOTSTRAP_PASSWORD_HASH` parks the
//! good config DB as `.db.bak` and creates an empty wrong-key DB; boot 2 with
//! the SAME wrong hash used to open that empty DB fine and report healthy
//! (`mismatch=false`), unblocking sync and risking the empty DB overwriting
//! the good copy. The fix makes the lingering `.db.bak` STICKY (mismatch stays
//! true), and a later boot with the CORRECT hash promotes the backup back into
//! place so recovery actually terminates.

mod common;

use common::{admin_http_client, TestServer};
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

#[tokio::test]
async fn wrong_hash_boot_stays_locked_and_correct_hash_promotes_backup() {
    let mut server = TestServer::builder()
        .auth("testkey", "testsecret")
        .build()
        .await;
    let endpoint = server.endpoint();
    let data_dir = server
        .config_path()
        .parent()
        .expect("config dir")
        .to_path_buf();
    let bak_path = data_dir.join("deltaglider_config.db.bak");
    let discarded_path = data_dir.join("deltaglider_config.db.discarded");

    // Healthy boot: create an IAM user so the config DB has real data.
    let admin = admin_http_client(&endpoint).await;
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

    // ── Boot 2: wrong hash → good DB parked as .db.bak, S3 locked ──
    let wrong_hash = bcrypt::hash("totally-the-wrong-password", 4).unwrap();
    server
        .respawn_with_env(&[("DGP_BOOTSTRAP_PASSWORD_HASH", &wrong_hash)])
        .await;
    assert_locked(&whoami(&endpoint).await, "boot 2 (first wrong-hash boot)");
    assert_eq!(
        raw_put_status(&endpoint, server.bucket()).await,
        503,
        "boot 2: S3 writes must be rejected with 503 while locked"
    );
    assert!(bak_path.exists(), "boot 2 must park the good DB as .db.bak");

    // ── Boot 3: SAME wrong hash opens the empty DB fine — the regression:
    // the lingering .db.bak must keep the node locked (sticky mismatch). ──
    server
        .respawn_with_env(&[("DGP_BOOTSTRAP_PASSWORD_HASH", &wrong_hash)])
        .await;
    assert_locked(&whoami(&endpoint).await, "boot 3 (second wrong-hash boot)");
    assert_eq!(
        raw_put_status(&endpoint, server.bucket()).await,
        503,
        "boot 3: S3 must STAY locked on a re-boot with the same wrong hash"
    );
    assert!(
        bak_path.exists(),
        "boot 3 must leave the good .db.bak alone"
    );

    // ── Boot 4: correct hash (from the config file) → promote .db.bak ──
    server.respawn_with_env(&[]).await;
    let who = whoami(&endpoint).await;
    assert_ne!(
        who["config_db_mismatch"], true,
        "boot 4: correct hash must clear the mismatch, got: {who}"
    );
    assert!(
        !bak_path.exists(),
        "boot 4: promote must consume .db.bak (recovery terminates)"
    );
    assert!(
        !discarded_path.exists(),
        "boot 4: promote must clean up .db.discarded after verifying the DB"
    );

    // The promoted DB still has alice → IAM mode with the user intact.
    let admin = admin_http_client(&endpoint).await;
    let users: serde_json::Value = admin
        .get(format!("{endpoint}/_/api/admin/users"))
        .send()
        .await
        .expect("list users")
        .json()
        .await
        .expect("users JSON");
    let names: Vec<&str> = users
        .as_array()
        .expect("users array")
        .iter()
        .filter_map(|u| u["name"].as_str())
        .collect();
    assert!(
        names.contains(&"alice"),
        "boot 4: promoted DB must still contain user 'alice', got {names:?}"
    );

    // S3 plane is unlocked again (legacy creds auto-migrated to legacy-admin).
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
