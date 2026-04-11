//! Integration tests for admin login-as (IAM user impersonation).

mod common;

use common::{admin_http_client, TestServer};
use reqwest::StatusCode;
use serde_json::json;

/// Create an IAM user via the admin API and return (access_key_id, secret_access_key).
async fn create_user(
    admin: &reqwest::Client,
    endpoint: &str,
    name: &str,
    permissions: serde_json::Value,
) -> (String, String) {
    let resp = admin
        .post(format!("{}/_/api/admin/users", endpoint))
        .json(&json!({ "name": name, "permissions": permissions }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body: serde_json::Value = resp.json().await.unwrap();
    (
        body["access_key_id"].as_str().unwrap().to_string(),
        body["secret_access_key"].as_str().unwrap().to_string(),
    )
}

fn admin_perms() -> serde_json::Value {
    json!([{ "actions": ["*"], "resources": ["*"] }])
}

fn readonly_perms() -> serde_json::Value {
    json!([{ "actions": ["read", "list"], "resources": ["*"] }])
}

#[tokio::test]
async fn test_login_as_admin_succeeds() {
    let server = TestServer::builder()
        .auth("BOOTSTRAP", "BOOTSTRAPSECRET")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    // Create an IAM admin user
    let (ak, sk) = create_user(&admin, &server.endpoint(), "test-admin", admin_perms()).await;

    // Login-as that user
    let login_client = reqwest::Client::builder()
        .cookie_store(true)
        .no_proxy()
        .build()
        .unwrap();
    let resp = login_client
        .post(format!("{}/_/api/admin/login-as", server.endpoint()))
        .json(&json!({ "access_key_id": ak, "secret_access_key": sk }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Verify admin session works — can access config
    let resp = login_client
        .get(format!("{}/_/api/admin/config", server.endpoint()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_login_as_non_admin_rejected() {
    let server = TestServer::builder()
        .auth("BOOTSTRAP2", "BOOTSTRAPSECRET2")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    // Create a read-only user (not admin)
    let (ak, sk) = create_user(&admin, &server.endpoint(), "reader", readonly_perms()).await;

    // Login-as should be rejected (not admin)
    let resp = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("{}/_/api/admin/login-as", server.endpoint()))
        .json(&json!({ "access_key_id": ak, "secret_access_key": sk }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_login_as_wrong_secret_rejected() {
    let server = TestServer::builder()
        .auth("BOOTSTRAP3", "BOOTSTRAPSECRET3")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    let (ak, _sk) = create_user(&admin, &server.endpoint(), "admin2", admin_perms()).await;

    let resp = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("{}/_/api/admin/login-as", server.endpoint()))
        .json(&json!({ "access_key_id": ak, "secret_access_key": "wrong-secret" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_login_as_unknown_key_rejected() {
    let server = TestServer::builder()
        .auth("BOOTSTRAP4", "BOOTSTRAPSECRET4")
        .build()
        .await;

    let resp = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("{}/_/api/admin/login-as", server.endpoint()))
        .json(&json!({ "access_key_id": "nonexistent", "secret_access_key": "anything" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_login_as_disabled_user_rejected() {
    let server = TestServer::builder()
        .auth("BOOTSTRAP5", "BOOTSTRAPSECRET5")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    // Create admin user
    let (ak, sk) = create_user(&admin, &server.endpoint(), "to-disable", admin_perms()).await;

    // Get user ID from list
    let resp = admin
        .get(format!("{}/_/api/admin/users", server.endpoint()))
        .send()
        .await
        .unwrap();
    let users: Vec<serde_json::Value> = resp.json().await.unwrap();
    let user_id = users
        .iter()
        .find(|u| u["access_key_id"].as_str() == Some(&ak))
        .unwrap()["id"]
        .as_i64()
        .unwrap();

    // Disable the user
    admin
        .put(format!(
            "{}/_/api/admin/users/{}",
            server.endpoint(),
            user_id
        ))
        .json(&json!({ "enabled": false }))
        .send()
        .await
        .unwrap();

    // Login-as should fail
    let resp = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("{}/_/api/admin/login-as", server.endpoint()))
        .json(&json!({ "access_key_id": ak, "secret_access_key": sk }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}
