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

async fn user_names(admin: &reqwest::Client, ep: &str) -> Vec<String> {
    let users: Vec<serde_json::Value> = admin
        .get(format!("{ep}/_/api/admin/users"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut names: Vec<String> = users
        .iter()
        .map(|u| u["name"].as_str().unwrap().to_string())
        .collect();
    names.sort();
    names
}

/// Browser review #3: a restore is point-in-time. Users and groups that the
/// backup does not hold are deleted, and users it holds are overwritten.
/// `iam=merge` keeps the old behaviour (add what is missing, keep the rest).
#[tokio::test]
async fn restore_replaces_iam_state_unless_merge_is_asked() {
    let server = TestServer::builder()
        .auth("BKRPL", "BKRPLSECRET")
        .build()
        .await;
    let ep = server.endpoint();
    let admin = admin_http_client(&ep).await;
    let create = |name: &'static str| {
        let admin = admin.clone();
        let ep = ep.clone();
        async move {
            let resp = admin
                .post(format!("{ep}/_/api/admin/users"))
                .json(&json!({
                    "name": name,
                    "permissions": [{ "actions": ["read"], "resources": ["releases/*"] }]
                }))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status().as_u16(), 201);
        }
    };
    create("ci-uploader").await;
    let backup: serde_json::Value = admin
        .get(format!("{ep}/_/api/admin/backup?format=json"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    // After the backup: a new user and a new group appear. (The first
    // user also migrates the bootstrap key into `legacy-admin`.)
    create("dana").await;
    let resp = admin
        .post(format!("{ep}/_/api/admin/groups"))
        .json(&json!({ "name": "Engineering", "description": "" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201);

    // Merge keeps them.
    let resp = admin
        .post(format!("{ep}/_/api/admin/backup?iam=merge"))
        .json(&backup)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "{}",
        resp.text().await.unwrap()
    );
    assert_eq!(
        user_names(&admin, &ep).await,
        ["ci-uploader", "dana", "legacy-admin"]
    );

    // The default (replace) goes back to the backup's state.
    let resp = admin
        .post(format!("{ep}/_/api/admin/backup"))
        .json(&backup)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
    let result: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(result["users_deleted"], 1, "{result}");
    assert_eq!(result["groups_deleted"], 1, "{result}");
    assert_eq!(
        user_names(&admin, &ep).await,
        ["ci-uploader", "legacy-admin"]
    );
    let groups: Vec<serde_json::Value> = admin
        .get(format!("{ep}/_/api/admin/groups"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(groups.is_empty(), "Engineering must be gone: {groups:?}");

    // One transaction: a backup that fails half-way changes nothing.
    let mut broken = backup.clone();
    broken["groups"] = json!([
        { "id": 1, "name": "dup", "permissions": [], "member_ids": [] },
        { "id": 2, "name": "dup", "permissions": [], "member_ids": [] }
    ]);
    let resp = admin
        .post(format!("{ep}/_/api/admin/backup"))
        .json(&broken)
        .send()
        .await
        .unwrap();
    assert!(!resp.status().is_success());
    assert_eq!(
        user_names(&admin, &ep).await,
        ["ci-uploader", "legacy-admin"]
    );
}

/// Rebuild a backup zip with `iam.json` edited and the manifest hashes
/// recomputed, so the zip passes Phase A.
fn zip_with_iam(zip: &[u8], edit: impl FnOnce(&mut serde_json::Value)) -> Vec<u8> {
    zip_edit(zip, |files| {
        let (_, iam_bytes) = files.iter_mut().find(|(n, _)| n == "iam.json").unwrap();
        let mut iam: serde_json::Value = serde_json::from_slice(iam_bytes).unwrap();
        edit(&mut iam);
        *iam_bytes = serde_json::to_vec_pretty(&iam).unwrap();
    })
}

/// Rebuild a backup zip with its files edited and the manifest hashes
/// recomputed.
fn zip_edit(zip: &[u8], edit: impl FnOnce(&mut Vec<(String, Vec<u8>)>)) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    use std::io::{Read, Write};
    let mut src = zip::ZipArchive::new(std::io::Cursor::new(zip)).unwrap();
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    for i in 0..src.len() {
        let mut f = src.by_index(i).unwrap();
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).unwrap();
        files.push((f.name().to_string(), buf));
    }
    edit(&mut files);
    let hex = |b: &[u8]| {
        Sha256::digest(b)
            .iter()
            .map(|x| format!("{x:02x}"))
            .collect::<String>()
    };
    let (_, mbytes) = files.iter().find(|(n, _)| n == "manifest.json").unwrap();
    let mut manifest: serde_json::Value = serde_json::from_slice(mbytes).unwrap();
    for e in manifest["files"].as_array_mut().unwrap() {
        let name = e["name"].as_str().unwrap().to_string();
        let (_, b) = files.iter().find(|(n, _)| *n == name).unwrap();
        e["sha256"] = json!(hex(b));
        e["bytes"] = json!(b.len());
    }
    let manifest = serde_json::to_vec_pretty(&manifest).unwrap();
    let mut out = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default();
    for (name, bytes) in &files {
        out.start_file(name.as_str(), opts).unwrap();
        let b = if name == "manifest.json" {
            &manifest
        } else {
            bytes
        };
        out.write_all(b).unwrap();
    }
    out.finish().unwrap().into_inner()
}

/// H14c: a full restore is atomic. When the IAM phase fails after the
/// config and the secrets are applied, the restore puts the running config
/// (in memory and on disk) and the admin password back.
#[tokio::test]
async fn failed_iam_phase_rolls_back_config_and_password() {
    let src_password = "source-instance-password-2";
    let source = TestServer::builder()
        .auth("BKSRC2", "BKSRC2SECRET")
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
    // Duplicate group names: the IAM transaction fails (see above).
    let zip = zip_with_iam(&zip, |iam| {
        iam["groups"] = json!([
            { "id": 1, "name": "dup", "permissions": [], "member_ids": [] },
            { "id": 2, "name": "dup", "permissions": [], "member_ids": [] }
        ]);
    });

    let target = TestServer::builder()
        .auth("BKDST2", "BKDST2SECRET")
        .build()
        .await;
    let ep = target.endpoint();
    let resp = admin_http_client(&ep)
        .await
        .post(format!("{ep}/_/api/admin/backup"))
        .header("content-type", "application/zip")
        .body(zip)
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(!status.is_success(), "{status} {body}");
    assert_eq!(body["stage"], "restore_iam", "{body}");

    // The target's own config is live again, and on disk.
    let admin = admin_http_client(&ep).await; // the target's own password
    let cfg: serde_json::Value = admin
        .get(format!("{ep}/_/api/admin/config"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(cfg["access_key_id"], "BKDST2", "{cfg}");
    let on_disk = std::fs::read_to_string(target.config_path()).unwrap();
    assert!(on_disk.contains("BKDST2"), "{on_disk}");
    assert!(!on_disk.contains("BKSRC2"), "{on_disk}");
    let login = reqwest::Client::new()
        .post(format!("{ep}/_/api/admin/login"))
        .json(&json!({ "password": src_password }))
        .send()
        .await
        .unwrap();
    assert!(
        !login.status().is_success(),
        "the backup's admin password must not stay after a failed restore"
    );
}

/// H14c: a backup whose config is in declarative IAM mode reconciles the
/// IAM DB to its YAML during the config phase. When the IAM phase then
/// fails, the rollback also puts the GUI-mode DB back.
#[tokio::test]
async fn failed_restore_of_a_declarative_config_puts_the_iam_db_back() {
    let source = TestServer::builder()
        .auth("BKSRC3", "BKSRC3SECRET")
        .build()
        .await;
    let sep = source.endpoint();
    let src_admin = admin_http_client(&sep).await;
    let resp = src_admin
        .post(format!("{sep}/_/api/admin/users"))
        .json(&json!({
            "name": "ci-uploader",
            "permissions": [{ "actions": ["read"], "resources": ["releases/*"] }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201);
    let created: serde_json::Value = resp.json().await.unwrap();
    let secret = created["secret_access_key"].as_str().unwrap().to_string();
    // Flip the source to declarative: its live IAM becomes the YAML.
    let exported = src_admin
        .get(format!("{sep}/_/api/admin/config/export"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let decl = src_admin
        .get(format!("{sep}/_/api/admin/config/declarative-iam-export"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let mut doc: serde_yaml::Value = serde_yaml::from_str(&exported).unwrap();
    let decl: serde_yaml::Value = serde_yaml::from_str(&decl).unwrap();
    let access = doc
        .as_mapping_mut()
        .unwrap()
        .entry("access".into())
        .or_insert_with(|| serde_yaml::Value::Mapping(Default::default()));
    for (k, v) in decl["access"].as_mapping().unwrap() {
        access
            .as_mapping_mut()
            .unwrap()
            .insert(k.clone(), v.clone());
    }
    let resp = src_admin
        .post(format!("{sep}/_/api/admin/config/apply"))
        .json(&json!({ "yaml": serde_yaml::to_string(&doc).unwrap() }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "{}",
        resp.text().await.unwrap()
    );
    let zip = src_admin
        .get(format!("{sep}/_/api/admin/backup"))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let zip = zip_with_iam(&zip, |iam| {
        iam["groups"] = json!([
            { "id": 1, "name": "dup", "permissions": [], "member_ids": [] },
            { "id": 2, "name": "dup", "permissions": [], "member_ids": [] }
        ]);
    });
    // The export redacts the declarative user's secret; put it in, so the
    // config phase reconciles the target DB.
    let zip = zip_edit(&zip, |files| {
        let (_, yaml) = files.iter_mut().find(|(n, _)| n == "config.yaml").unwrap();
        let mut doc: serde_yaml::Value = serde_yaml::from_slice(yaml).unwrap();
        for u in doc["access"]["iam_users"].as_sequence_mut().unwrap() {
            if u["name"] == "ci-uploader" {
                u["secret_access_key"] = secret.clone().into();
            }
        }
        *yaml = serde_yaml::to_string(&doc).unwrap().into_bytes();
    });

    let target = TestServer::builder()
        .auth("BKDST3", "BKDST3SECRET")
        .build()
        .await;
    let ep = target.endpoint();
    let admin = admin_http_client(&ep).await;
    let resp = admin
        .post(format!("{ep}/_/api/admin/users"))
        .json(&json!({
            "name": "dana",
            "permissions": [{ "actions": ["read"], "resources": ["downloads/*"] }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201);
    let resp = admin
        .post(format!("{ep}/_/api/admin/backup"))
        .header("content-type", "application/zip")
        .body(zip)
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(!status.is_success(), "{status} {body}");
    assert_eq!(body["stage"], "restore_iam", "{body}");

    let cfg: serde_json::Value = admin
        .get(format!("{ep}/_/api/admin/config"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(cfg["iam_mode"], "gui", "{cfg}");
    assert_eq!(user_names(&admin, &ep).await, ["dana", "legacy-admin"]);
}
