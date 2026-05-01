//! Regression tests for the public IAM identity resolver used by the browser.
//!
//! `POST /_/api/iam/identity` accepts S3 IAM credentials and returns the same
//! effective permissions that the SigV4 path uses, without creating an admin
//! session. Non-admin browser users depend on this to enable only the controls
//! their prefix-scoped permissions allow.

mod common;

use common::{admin_http_client, TestServer};
use reqwest::StatusCode;
use serde_json::json;

#[derive(Clone, Debug)]
struct UserCreds {
    access_key_id: String,
    secret_access_key: String,
    id: i64,
}

async fn create_user(
    admin: &reqwest::Client,
    endpoint: &str,
    name: &str,
    permissions: Vec<serde_json::Value>,
) -> UserCreds {
    let resp = admin
        .post(format!("{endpoint}/_/api/admin/users"))
        .json(&json!({ "name": name, "permissions": permissions }))
        .send()
        .await
        .expect("create user request");
    assert_eq!(resp.status(), StatusCode::CREATED, "create user {name}");

    let body: serde_json::Value = resp.json().await.expect("create user JSON");
    UserCreds {
        access_key_id: body["access_key_id"].as_str().unwrap().to_string(),
        secret_access_key: body["secret_access_key"].as_str().unwrap().to_string(),
        id: body["id"].as_i64().unwrap(),
    }
}

async fn create_group(
    admin: &reqwest::Client,
    endpoint: &str,
    name: &str,
    permissions: Vec<serde_json::Value>,
) -> i64 {
    let resp = admin
        .post(format!("{endpoint}/_/api/admin/groups"))
        .json(&json!({ "name": name, "permissions": permissions }))
        .send()
        .await
        .expect("create group request");
    assert_eq!(resp.status(), StatusCode::CREATED, "create group {name}");

    let body: serde_json::Value = resp.json().await.expect("create group JSON");
    body["id"].as_i64().unwrap()
}

async fn add_to_group(admin: &reqwest::Client, endpoint: &str, group_id: i64, user_id: i64) {
    let resp = admin
        .post(format!("{endpoint}/_/api/admin/groups/{group_id}/members"))
        .json(&json!({ "user_id": user_id }))
        .send()
        .await
        .expect("add group member request");
    assert!(
        resp.status().is_success(),
        "add user {user_id} to group {group_id}: {}",
        resp.status()
    );
}

async fn identity_response(
    client: &reqwest::Client,
    endpoint: &str,
    access_key_id: &str,
    secret_access_key: &str,
) -> reqwest::Response {
    client
        .post(format!("{endpoint}/_/api/iam/identity"))
        .json(&json!({
            "access_key_id": access_key_id,
            "secret_access_key": secret_access_key,
        }))
        .send()
        .await
        .expect("identity request")
}

#[tokio::test]
async fn identity_returns_effective_permissions_without_admin_session() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .build()
        .await;
    let endpoint = server.endpoint();
    let admin = admin_http_client(&endpoint).await;

    let user = create_user(
        &admin,
        &endpoint,
        "identity-prefix-user",
        vec![json!({
            "effect": "Allow",
            "actions": ["read"],
            "resources": ["artifacts/team-a/*"],
        })],
    )
    .await;
    let writers = create_group(
        &admin,
        &endpoint,
        "identity-prefix-writers",
        vec![
            json!({
                "effect": "Allow",
                "actions": ["write", "delete"],
                "resources": ["artifacts/team-a/*"],
            }),
            json!({
                "effect": "Allow",
                "actions": ["list"],
                "resources": ["artifacts", "artifacts/*"],
                "conditions": {
                    "StringLike": {
                        "s3:prefix": ["", "team-a/", "team-a/*"]
                    }
                },
            }),
        ],
    )
    .await;
    add_to_group(&admin, &endpoint, writers, user.id).await;

    let anonymous = reqwest::Client::builder()
        .cookie_store(true)
        .build()
        .unwrap();
    let resp = identity_response(
        &anonymous,
        &endpoint,
        &user.access_key_id,
        &user.secret_access_key,
    )
    .await;

    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers().get(reqwest::header::SET_COOKIE).is_none(),
        "identity must not mint a dgp_session cookie"
    );

    let body: serde_json::Value = resp.json().await.expect("identity JSON");
    assert_eq!(body["mode"], "iam");
    assert_eq!(body["user"]["name"], "identity-prefix-user");
    assert_eq!(body["user"]["access_key_id"], user.access_key_id);
    assert_eq!(body["user"]["is_admin"], false);

    let permissions = body["user"]["permissions"].as_array().unwrap();
    assert!(
        permissions
            .iter()
            .any(|p| p["actions"] == json!(["read"])
                && p["resources"] == json!(["artifacts/team-a/*"])),
        "direct read permission missing: {permissions:?}"
    );
    assert!(
        permissions
            .iter()
            .any(|p| p["actions"] == json!(["write", "delete"])
                && p["resources"] == json!(["artifacts/team-a/*"])),
        "group-inherited write/delete permission missing: {permissions:?}"
    );
    assert!(
        permissions.iter().any(|p| p["actions"] == json!(["list"])
            && p["conditions"]["StringLike"]["s3:prefix"] == json!(["", "team-a/", "team-a/*"])),
        "group-inherited ListBucket condition missing: {permissions:?}"
    );

    let session = anonymous
        .get(format!("{endpoint}/_/api/admin/session"))
        .send()
        .await
        .expect("session request");
    assert_eq!(
        session.status(),
        StatusCode::UNAUTHORIZED,
        "identity must not create a session"
    );

    let whoami: serde_json::Value = anonymous
        .get(format!("{endpoint}/_/api/whoami"))
        .send()
        .await
        .expect("whoami request")
        .json()
        .await
        .expect("whoami JSON");
    assert_eq!(whoami["mode"], "iam");
    assert!(
        whoami.get("user").is_none(),
        "whoami should remain session-based for anonymous identity clients"
    );

    let admin_whoami: serde_json::Value = admin
        .get(format!("{endpoint}/_/api/whoami"))
        .send()
        .await
        .expect("admin whoami request")
        .json()
        .await
        .expect("admin whoami JSON");
    assert_eq!(admin_whoami["mode"], "iam");
    assert_eq!(admin_whoami["user"]["name"], "admin");
    assert_eq!(admin_whoami["user"]["is_admin"], true);
}

#[tokio::test]
async fn identity_rejects_wrong_secret_and_does_not_create_session() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .build()
        .await;
    let endpoint = server.endpoint();
    let admin = admin_http_client(&endpoint).await;
    let user = create_user(
        &admin,
        &endpoint,
        "identity-wrong-secret",
        vec![json!({
            "effect": "Allow",
            "actions": ["read"],
            "resources": ["artifacts/team-a/*"],
        })],
    )
    .await;

    let anonymous = reqwest::Client::builder()
        .cookie_store(true)
        .build()
        .unwrap();
    let resp = identity_response(
        &anonymous,
        &endpoint,
        &user.access_key_id,
        "definitely-wrong",
    )
    .await;

    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    assert!(
        resp.headers().get(reqwest::header::SET_COOKIE).is_none(),
        "failed identity lookup must not mint a dgp_session cookie"
    );

    let session = anonymous
        .get(format!("{endpoint}/_/api/admin/session"))
        .send()
        .await
        .expect("session request");
    assert_eq!(session.status(), StatusCode::UNAUTHORIZED);

    let protected = anonymous
        .get(format!("{endpoint}/_/api/admin/users"))
        .send()
        .await
        .expect("protected admin request");
    assert_eq!(protected.status(), StatusCode::UNAUTHORIZED);
}
