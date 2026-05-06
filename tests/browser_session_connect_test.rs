//! Integration tests: S3 browser-lift session + open-mode browser session.

mod common;

use common::{admin_http_client, TestServer};
use reqwest::StatusCode;
use serde_json::json;

async fn create_user(
    admin: &reqwest::Client,
    endpoint: &str,
    name: &str,
    permissions: serde_json::Value,
) -> (String, String) {
    let resp = admin
        .post(format!("{endpoint}/_/api/admin/users"))
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
async fn browser_lift_session_stores_s3_creds_config_forbidden_login_as_admin_ok() {
    let server = TestServer::builder()
        .auth("BSC1", "BSC1SECRET")
        .build()
        .await;
    let ep = server.endpoint();
    let admin = admin_http_client(&ep).await;

    let (reader_ak, reader_sk) =
        create_user(&admin, &ep, "browser-lift-reader", readonly_perms()).await;
    let (admin_ak, admin_sk) = create_user(&admin, &ep, "browser-lift-admin", admin_perms()).await;

    let lift = reqwest::Client::builder()
        .cookie_store(true)
        .no_proxy()
        .build()
        .unwrap();
    let resp = lift
        .post(format!("{ep}/_/api/admin/session/browser-connect"))
        .json(&json!({
            "access_key_id": reader_ak,
            "secret_access_key": reader_sk,
            "endpoint": ep,
            "bucket": "",
            "region": "us-east-1",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let creds = lift
        .get(format!("{ep}/_/api/admin/session/s3-credentials"))
        .send()
        .await
        .unwrap();
    assert_eq!(creds.status(), StatusCode::OK);

    let cfg = lift
        .get(format!("{ep}/_/api/admin/config"))
        .send()
        .await
        .unwrap();
    assert_eq!(cfg.status(), StatusCode::FORBIDDEN);

    let admin_client = reqwest::Client::builder()
        .cookie_store(true)
        .no_proxy()
        .build()
        .unwrap();
    let login = admin_client
        .post(format!("{ep}/_/api/admin/login-as"))
        .json(&json!({
            "access_key_id": admin_ak,
            "secret_access_key": admin_sk,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(login.status(), StatusCode::OK);

    let cfg2 = admin_client
        .get(format!("{ep}/_/api/admin/config"))
        .send()
        .await
        .unwrap();
    assert_eq!(cfg2.status(), StatusCode::OK);
}

#[tokio::test]
async fn open_browser_connect_requires_open_auth_mode() {
    let server = TestServer::builder()
        .auth("BSC2", "BSC2SECRET")
        .build()
        .await;
    let ep = server.endpoint();
    let resp = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("{ep}/_/api/admin/session/open-browser-connect"))
        .json(&json!({ "endpoint": ep, "bucket": "" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn open_browser_connect_sets_anonymous_creds_and_blocks_config() {
    let server = TestServer::builder().build().await;
    let ep = server.endpoint();
    let c = reqwest::Client::builder()
        .cookie_store(true)
        .no_proxy()
        .build()
        .unwrap();
    let r = c
        .post(format!("{ep}/_/api/admin/session/open-browser-connect"))
        .json(&json!({ "endpoint": ep, "bucket": "" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);

    let creds = c
        .get(format!("{ep}/_/api/admin/session/s3-credentials"))
        .send()
        .await
        .unwrap();
    assert_eq!(creds.status(), StatusCode::OK);
    let j: serde_json::Value = creds.json().await.unwrap();
    assert_eq!(j["access_key_id"], "anonymous");

    let cfg = c
        .get(format!("{ep}/_/api/admin/config"))
        .send()
        .await
        .unwrap();
    assert_eq!(cfg.status(), StatusCode::FORBIDDEN);
}
