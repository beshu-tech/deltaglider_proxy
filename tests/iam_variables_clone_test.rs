// SPDX-License-Identifier: GPL-3.0-only

//! IAM permission templates and duplicate API coverage.

mod common;

use aws_sdk_s3::primitives::ByteStream;
use common::{admin_http_client, TestServer};
use serde_json::json;

#[derive(Clone)]
struct UserCreds {
    id: i64,
    access_key_id: String,
    secret_access_key: String,
}

async fn create_user(
    admin: &reqwest::Client,
    server: &TestServer,
    name: &str,
    permissions: Vec<serde_json::Value>,
) -> UserCreds {
    let resp = admin
        .post(format!("{}/_/api/admin/users", server.endpoint()))
        .json(&json!({ "name": name, "permissions": permissions }))
        .send()
        .await
        .expect("create user request");
    assert_eq!(resp.status().as_u16(), 201, "create user {name}");
    let body: serde_json::Value = resp.json().await.unwrap();
    UserCreds {
        id: body["id"].as_i64().unwrap(),
        access_key_id: body["access_key_id"].as_str().unwrap().to_string(),
        secret_access_key: body["secret_access_key"].as_str().unwrap().to_string(),
    }
}

async fn seed_object(server: &TestServer, admin: &UserCreds, key: &str) {
    let s3 = server
        .s3_client_with_creds(&admin.access_key_id, &admin.secret_access_key)
        .await;
    s3.put_object()
        .bucket(server.bucket())
        .key(key)
        .body(ByteStream::from_static(b"seed"))
        .send()
        .await
        .expect("seed object");
}

async fn get_ok(server: &TestServer, user: &UserCreds, key: &str) -> bool {
    let s3 = server
        .s3_client_with_creds(&user.access_key_id, &user.secret_access_key)
        .await;
    s3.get_object()
        .bucket(server.bucket())
        .key(key)
        .send()
        .await
        .is_ok()
}

#[tokio::test]
async fn test_username_template_in_direct_permissions_scopes_to_own_prefix() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    let admin_user = create_user(
        &admin,
        &server,
        "admin",
        vec![json!({"effect": "Allow", "actions": ["*"], "resources": ["*"]})],
    )
    .await;
    let alice = create_user(
        &admin,
        &server,
        "alice",
        vec![json!({
            "effect": "Allow",
            "actions": ["read", "list"],
            "resources": [format!("{}/home/${{iam:username}}/*", server.bucket())]
        })],
    )
    .await;

    seed_object(&server, &admin_user, "home/alice/file.txt").await;
    seed_object(&server, &admin_user, "home/bob/file.txt").await;

    assert!(get_ok(&server, &alice, "home/alice/file.txt").await);
    assert!(!get_ok(&server, &alice, "home/bob/file.txt").await);
}

#[tokio::test]
async fn test_username_template_in_group_permissions_expands_per_member() {
    let server = TestServer::builder()
        .auth("bootstrap_key2", "bootstrap_secret2")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    let admin_user = create_user(
        &admin,
        &server,
        "admin",
        vec![json!({"effect": "Allow", "actions": ["*"], "resources": ["*"]})],
    )
    .await;
    let bob = create_user(&admin, &server, "bob", vec![]).await;

    let resp = admin
        .post(format!("{}/_/api/admin/groups", server.endpoint()))
        .json(&json!({
            "name": "home-readers",
            "permissions": [{
                "effect": "Allow",
                "actions": ["read", "list"],
                "resources": [format!("{}/home/${{iam:username}}/*", server.bucket())]
            }],
            "member_ids": [bob.id]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201, "create group");

    seed_object(&server, &admin_user, "home/bob/file.txt").await;
    seed_object(&server, &admin_user, "home/alice/file.txt").await;

    assert!(get_ok(&server, &bob, "home/bob/file.txt").await);
    assert!(!get_ok(&server, &bob, "home/alice/file.txt").await);
}

#[tokio::test]
async fn test_unknown_template_variable_rejected_by_user_api() {
    let server = TestServer::builder()
        .auth("bootstrap_key3", "bootstrap_secret3")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    let resp = admin
        .post(format!("{}/_/api/admin/users", server.endpoint()))
        .json(&json!({
            "name": "bad-template",
            "permissions": [{
                "effect": "Allow",
                "actions": ["read"],
                "resources": [format!("{}/home/${{email}}/*", server.bucket())]
            }]
        }))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_clone_user_copies_permissions_and_memberships_with_fresh_secret() {
    let server = TestServer::builder()
        .auth("bootstrap_key4", "bootstrap_secret4")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;
    let source = create_user(
        &admin,
        &server,
        "source-user",
        vec![json!({
            "effect": "Allow",
            "actions": ["read"],
            "resources": [format!("{}/source/*", server.bucket())]
        })],
    )
    .await;

    let group_resp = admin
        .post(format!("{}/_/api/admin/groups", server.endpoint()))
        .json(&json!({
            "name": "source-group",
            "permissions": [],
            "member_ids": [source.id]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(group_resp.status().as_u16(), 201);
    let group: serde_json::Value = group_resp.json().await.unwrap();
    let group_id = group["id"].as_i64().unwrap();

    let resp = admin
        .post(format!(
            "{}/_/api/admin/users/{}/clone",
            server.endpoint(),
            source.id
        ))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201, "clone user");
    let clone: serde_json::Value = resp.json().await.unwrap();

    assert_ne!(clone["id"].as_i64().unwrap(), source.id);
    assert_ne!(
        clone["access_key_id"].as_str().unwrap(),
        source.access_key_id
    );
    assert_ne!(
        clone["secret_access_key"].as_str().unwrap(),
        source.secret_access_key
    );
    assert_eq!(
        clone["permissions"][0]["resources"][0],
        format!("{}/source/*", server.bucket())
    );
    assert_eq!(
        clone["group_ids"].as_array().unwrap(),
        &vec![json!(group_id)]
    );
}

#[tokio::test]
async fn test_clone_group_copies_permissions_but_not_members_by_default() {
    let server = TestServer::builder()
        .auth("bootstrap_key5", "bootstrap_secret5")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;
    let user = create_user(&admin, &server, "member", vec![]).await;

    let group_resp = admin
        .post(format!("{}/_/api/admin/groups", server.endpoint()))
        .json(&json!({
            "name": "readers",
            "description": "source description",
            "permissions": [{
                "effect": "Allow",
                "actions": ["read"],
                "resources": [format!("{}/public/*", server.bucket())]
            }],
            "member_ids": [user.id]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(group_resp.status().as_u16(), 201);
    let group: serde_json::Value = group_resp.json().await.unwrap();

    let resp = admin
        .post(format!(
            "{}/_/api/admin/groups/{}/clone",
            server.endpoint(),
            group["id"].as_i64().unwrap()
        ))
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201, "clone group");
    let clone: serde_json::Value = resp.json().await.unwrap();

    assert_eq!(clone["description"], "source description");
    assert_eq!(
        clone["permissions"][0]["resources"][0],
        format!("{}/public/*", server.bucket())
    );
    assert_eq!(clone["member_ids"].as_array().unwrap().len(), 0);
}
