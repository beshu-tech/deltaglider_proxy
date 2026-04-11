//! Integration tests for admin config hot-reload and backend CRUD.
//! All tests spawn a real proxy process and make real HTTP requests.

mod common;

use common::{admin_http_client, TestServer};
use reqwest::StatusCode;
use serde_json::json;

// ═══════════════════════════════════════════════════
// Config hot-reload
// ═══════════════════════════════════════════════════

#[tokio::test]
async fn test_config_get_returns_current_state() {
    let server = TestServer::builder()
        .auth("CFGKEY1", "CFGSECRET1")
        .max_delta_ratio(0.75)
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    let resp = admin
        .get(format!("{}/_/api/admin/config", server.endpoint()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let cfg: serde_json::Value = resp.json().await.unwrap();
    assert!(cfg["max_delta_ratio"].is_number());
    assert!(cfg["backend_type"].is_string());
    assert!(cfg["auth_enabled"].is_boolean());
    assert!(cfg["bucket_policies"].is_object());
}

#[tokio::test]
async fn test_config_update_max_delta_ratio() {
    let server = TestServer::builder()
        .auth("CFGKEY2", "CFGSECRET2")
        .max_delta_ratio(0.75)
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    // Change delta ratio
    let resp = admin
        .put(format!("{}/_/api/admin/config", server.endpoint()))
        .json(&json!({ "max_delta_ratio": 0.5 }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["success"], true);

    // Verify change persisted
    let resp = admin
        .get(format!("{}/_/api/admin/config", server.endpoint()))
        .send()
        .await
        .unwrap();
    let cfg: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(cfg["max_delta_ratio"].as_f64().unwrap(), 0.5);
}

#[tokio::test]
async fn test_config_update_bucket_policies_with_quota() {
    let server = TestServer::builder()
        .auth("CFGKEY3", "CFGSECRET3")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    // Set a bucket policy with quota
    let resp = admin
        .put(format!("{}/_/api/admin/config", server.endpoint()))
        .json(&json!({
            "bucket_policies": {
                "testbucket": {
                    "compression": false,
                    "quota_bytes": 1073741824
                }
            }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Verify
    let resp = admin
        .get(format!("{}/_/api/admin/config", server.endpoint()))
        .send()
        .await
        .unwrap();
    let cfg: serde_json::Value = resp.json().await.unwrap();
    let policies = &cfg["bucket_policies"]["testbucket"];
    assert_eq!(policies["compression"], false);
    assert_eq!(policies["quota_bytes"], 1073741824u64);
}

#[tokio::test]
async fn test_config_update_restart_required() {
    let server = TestServer::builder()
        .auth("CFGKEY4", "CFGSECRET4")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    let resp = admin
        .put(format!("{}/_/api/admin/config", server.endpoint()))
        .json(&json!({ "listen_addr": "0.0.0.0:9999" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["requires_restart"], true);
    assert!(!body["warnings"].as_array().unwrap().is_empty());
}

// ═══════════════════════════════════════════════════
// Backend CRUD
// ═══════════════════════════════════════════════════

#[tokio::test]
async fn test_backend_list() {
    let server = TestServer::builder()
        .auth("BEKEY1", "BESECRET1")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    let resp = admin
        .get(format!("{}/_/api/admin/backends", server.endpoint()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["backends"].is_array());
}

#[tokio::test]
async fn test_backend_create_and_delete_filesystem() {
    let server = TestServer::builder()
        .auth("BEKEY2", "BESECRET2")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().to_str().unwrap();

    // Create
    let resp = admin
        .post(format!("{}/_/api/admin/backends", server.endpoint()))
        .json(&json!({
            "name": "test-fs-backend",
            "type": "filesystem",
            "path": path
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Verify in list
    let resp = admin
        .get(format!("{}/_/api/admin/backends", server.endpoint()))
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    let backends = body["backends"].as_array().unwrap();
    assert!(
        backends.iter().any(|b| b["name"] == "test-fs-backend"),
        "Created backend should appear in list"
    );

    // Create a second backend so the first isn't the only/default one
    let tmp2 = tempfile::tempdir().unwrap();
    let path2 = tmp2.path().to_str().unwrap();
    let resp = admin
        .post(format!("{}/_/api/admin/backends", server.endpoint()))
        .json(&json!({
            "name": "test-fs-backend-2",
            "type": "filesystem",
            "path": path2,
            "set_default": true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Now delete the first (non-default) backend
    let resp = admin
        .delete(format!(
            "{}/_/api/admin/backends/test-fs-backend",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "Should be able to delete non-default backend"
    );

    // Verify removed
    let resp = admin
        .get(format!("{}/_/api/admin/backends", server.endpoint()))
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    let backends = body["backends"].as_array().unwrap();
    assert!(
        !backends.iter().any(|b| b["name"] == "test-fs-backend"),
        "Deleted backend should not appear in list"
    );
}
