// SPDX-License-Identifier: BUSL-1.1

//! Integration tests for config DB backup/restore — the manual equivalent of
//! config sync. Tests the full IAM state export/import flow across two server instances.
//! Requires MinIO for S3 backend tests.

use crate::common;

use common::{admin_http_client, TestServer};
use serde_json::json;

#[tokio::test]
async fn test_config_db_backup_export_import() {
    // Export IAM state from server A, import into server B.
    // This is the real code path for config sync portability.
    skip_unless_minio!();

    let server = TestServer::builder()
        .auth("BKKEY1", "BKSECRET1")
        .s3_endpoint(&common::minio_endpoint_url())
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    // Create user + group on server A
    let resp = admin
        .post(format!("{}/_/api/admin/users", server.endpoint()))
        .json(&json!({
            "name": "backup-user",
            "permissions": [{ "actions": ["read", "write"], "resources": ["mybucket/*"] }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201);

    let resp = admin
        .post(format!("{}/_/api/admin/groups", server.endpoint()))
        .json(&json!({ "name": "backup-group", "description": "test group" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201);

    // Export backup (IAM JSON; default GET is zip)
    let resp = admin
        .get(format!(
            "{}/_/api/admin/backup?format=json",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let backup: serde_json::Value = resp.json().await.unwrap();
    assert!(!backup["users"].as_array().unwrap().is_empty());
    assert!(!backup["groups"].as_array().unwrap().is_empty());

    // Start a SECOND server and import the backup
    let server2 = TestServer::builder()
        .auth("BKKEY2", "BKSECRET2")
        .s3_endpoint(&common::minio_endpoint_url())
        .build()
        .await;
    let admin2 = admin_http_client(&server2.endpoint()).await;

    let resp = admin2
        .post(format!("{}/_/api/admin/backup", server2.endpoint()))
        .json(&backup)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    // Verify imported data on server B
    let resp = admin2
        .get(format!("{}/_/api/admin/users", server2.endpoint()))
        .send()
        .await
        .unwrap();
    let users: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert!(
        users.iter().any(|u| u["name"] == "backup-user"),
        "Imported user should exist on second server"
    );

    let resp = admin2
        .get(format!("{}/_/api/admin/groups", server2.endpoint()))
        .send()
        .await
        .unwrap();
    let groups: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert!(
        groups.iter().any(|g| g["name"] == "backup-group"),
        "Imported group should exist on second server"
    );
}

/// S8 follow-up: the bootstrap hash no longer encrypts the config DB, so a
/// full restore of a backup from an instance with ANOTHER admin password is
/// not refused any more: it adopts the backup's password.
#[tokio::test]
async fn full_restore_adopts_a_different_bootstrap_password() {
    let src_password = "source-instance-password-1";
    let source = TestServer::builder()
        .auth("BKSRC", "BKSRCSECRET")
        .bootstrap_password(src_password)
        .build()
        .await;
    let src_admin = common::admin_http_client_with_password(&source.endpoint(), src_password).await;
    let zip = src_admin
        .get(format!("{}/_/api/admin/backup", source.endpoint()))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();

    let target = TestServer::builder()
        .auth("BKDST", "BKDSTSECRET")
        .build()
        .await;
    let resp = admin_http_client(&target.endpoint())
        .await
        .post(format!("{}/_/api/admin/backup", target.endpoint()))
        .header("content-type", "application/zip")
        .body(zip.to_vec())
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body = resp.text().await.unwrap();
    assert_eq!(
        status.as_u16(),
        200,
        "a different bootstrap hash must not block: {body}"
    );

    // The target's admin password is now the source's.
    let login = |pw: &'static str| {
        let ep = target.endpoint();
        async move {
            reqwest::Client::new()
                .post(format!("{ep}/_/api/admin/login"))
                .json(&json!({ "password": pw }))
                .send()
                .await
                .unwrap()
                .status()
        }
    };
    assert!(
        login(src_password).await.is_success(),
        "the backup's password must work"
    );
    assert!(
        !login(common::TEST_BOOTSTRAP_PASSWORD).await.is_success(),
        "the old password must stop working"
    );
}
