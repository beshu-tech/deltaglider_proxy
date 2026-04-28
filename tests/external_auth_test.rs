//! Integration tests for external authentication: provider CRUD, group mapping,
//! whoami, preview, backup/restore, and sync memberships.
//!
//! These tests exercise the admin API endpoints via HTTP against a real TestServer.
//! The actual OAuth redirect flow (browser → IdP → callback) cannot be tested
//! without a real OIDC provider, but everything around it is covered.

mod common;

use common::{admin_http_client, TestServer};
use reqwest::StatusCode;
use serde_json::json;

// ── Helpers ──

async fn setup() -> (TestServer, reqwest::Client) {
    let server = TestServer::builder()
        .auth("test_key", "test_secret")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;
    (server, admin)
}

async fn create_provider(admin: &reqwest::Client, endpoint: &str, name: &str) -> serde_json::Value {
    let resp = admin
        .post(format!("{}/_/api/admin/ext-auth/providers", endpoint))
        .json(&json!({
            "name": name,
            "provider_type": "oidc",
            "enabled": true,
            "display_name": format!("{} Display", name),
            "client_id": "test-client-id.apps.googleusercontent.com",
            "client_secret": "test-secret-12345",
            "issuer_url": "https://accounts.google.com",
            "scopes": "openid email profile",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        201,
        "create provider '{}' failed",
        name
    );
    resp.json().await.unwrap()
}

async fn create_group(admin: &reqwest::Client, endpoint: &str, name: &str) -> serde_json::Value {
    let resp = admin
        .post(format!("{}/_/api/admin/groups", endpoint))
        .json(&json!({
            "name": name,
            "description": format!("{} group", name),
            "permissions": [{"effect": "Allow", "actions": ["read", "list"], "resources": ["*"]}],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        201,
        "create group '{}' failed",
        name
    );
    resp.json().await.unwrap()
}

async fn create_mapping_rule(
    admin: &reqwest::Client,
    endpoint: &str,
    match_type: &str,
    match_value: &str,
    group_id: i64,
) -> serde_json::Value {
    let resp = admin
        .post(format!("{}/_/api/admin/ext-auth/mappings", endpoint))
        .json(&json!({
            "match_type": match_type,
            "match_value": match_value,
            "group_id": group_id,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201, "create mapping rule failed");
    resp.json().await.unwrap()
}

// ============================================================================
// Provider CRUD
// ============================================================================

#[tokio::test]
async fn test_create_provider() {
    let (server, admin) = setup().await;
    let provider = create_provider(&admin, &server.endpoint(), "google-corp").await;

    assert_eq!(provider["name"], "google-corp");
    assert_eq!(provider["provider_type"], "oidc");
    assert_eq!(provider["enabled"], true);
    assert_eq!(provider["display_name"], "google-corp Display");
    assert!(provider["id"].as_i64().unwrap() > 0);
}

#[tokio::test]
async fn test_list_providers_masks_secrets() {
    let (server, admin) = setup().await;
    create_provider(&admin, &server.endpoint(), "prov1").await;

    let resp = admin
        .get(format!(
            "{}/_/api/admin/ext-auth/providers",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let providers: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert_eq!(providers.len(), 1);
    assert_eq!(
        providers[0]["client_secret"], "****",
        "Secret should be masked in list response"
    );
}

#[tokio::test]
async fn test_update_provider_partial() {
    let (server, admin) = setup().await;
    let provider = create_provider(&admin, &server.endpoint(), "prov1").await;
    let id = provider["id"].as_i64().unwrap();

    let resp = admin
        .put(format!(
            "{}/_/api/admin/ext-auth/providers/{}",
            server.endpoint(),
            id
        ))
        .json(&json!({"enabled": false}))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let updated: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(updated["enabled"], false);
    assert_eq!(updated["name"], "prov1", "Name should be unchanged");
}

#[tokio::test]
async fn test_delete_provider() {
    let (server, admin) = setup().await;
    let provider = create_provider(&admin, &server.endpoint(), "to-delete").await;
    let id = provider["id"].as_i64().unwrap();

    let resp = admin
        .delete(format!(
            "{}/_/api/admin/ext-auth/providers/{}",
            server.endpoint(),
            id
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // Verify gone
    let resp = admin
        .get(format!(
            "{}/_/api/admin/ext-auth/providers",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    let providers: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert!(providers.is_empty());
}

#[tokio::test]
async fn test_provider_requires_session() {
    let (server, _admin) = setup().await;
    // Use a client without session cookie
    let anon = reqwest::Client::new();
    let resp = anon
        .post(format!(
            "{}/_/api/admin/ext-auth/providers",
            server.endpoint()
        ))
        .json(&json!({"name": "x", "provider_type": "oidc"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn test_test_provider_bad_issuer() {
    let (server, admin) = setup().await;

    // Create with a bad issuer URL
    let resp = admin
        .post(format!(
            "{}/_/api/admin/ext-auth/providers",
            server.endpoint()
        ))
        .json(&json!({
            "name": "bad-issuer",
            "provider_type": "oidc",
            "enabled": true,
            "client_id": "cid",
            "client_secret": "csec",
            "issuer_url": "https://this-does-not-exist.invalid",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 201);
    let provider: serde_json::Value = resp.json().await.unwrap();
    let id = provider["id"].as_i64().unwrap();

    // Test connection — should fail
    let resp = admin
        .post(format!(
            "{}/_/api/admin/ext-auth/providers/{}/test",
            server.endpoint(),
            id
        ))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success()); // Endpoint returns 200 with success=false
    let result: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(result["success"], false);
    assert!(result["error"].as_str().is_some());
}

// ============================================================================
// Group Mapping Rules
// ============================================================================

#[tokio::test]
async fn test_create_mapping_rule() {
    let (server, admin) = setup().await;
    let group = create_group(&admin, &server.endpoint(), "employees").await;
    let gid = group["id"].as_i64().unwrap();

    let rule = create_mapping_rule(
        &admin,
        &server.endpoint(),
        "email_domain",
        "company.com",
        gid,
    )
    .await;
    assert_eq!(rule["match_type"], "email_domain");
    assert_eq!(rule["match_value"], "company.com");
    assert_eq!(rule["group_id"], gid);
}

#[tokio::test]
async fn test_create_mapping_invalid_match_type() {
    let (server, admin) = setup().await;
    let group = create_group(&admin, &server.endpoint(), "g").await;
    let gid = group["id"].as_i64().unwrap();

    let resp = admin
        .post(format!(
            "{}/_/api/admin/ext-auth/mappings",
            server.endpoint()
        ))
        .json(&json!({
            "match_type": "invalid_type",
            "match_value": "anything",
            "group_id": gid,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_create_mapping_invalid_regex() {
    let (server, admin) = setup().await;
    let group = create_group(&admin, &server.endpoint(), "g").await;
    let gid = group["id"].as_i64().unwrap();

    let resp = admin
        .post(format!(
            "{}/_/api/admin/ext-auth/mappings",
            server.endpoint()
        ))
        .json(&json!({
            "match_type": "email_regex",
            "match_value": "[invalid",
            "group_id": gid,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_update_and_delete_mapping_rule() {
    let (server, admin) = setup().await;
    let group = create_group(&admin, &server.endpoint(), "g").await;
    let gid = group["id"].as_i64().unwrap();
    let rule =
        create_mapping_rule(&admin, &server.endpoint(), "email_domain", "old.com", gid).await;
    let rule_id = rule["id"].as_i64().unwrap();

    // Update
    let resp = admin
        .put(format!(
            "{}/_/api/admin/ext-auth/mappings/{}",
            server.endpoint(),
            rule_id
        ))
        .json(&json!({"match_value": "new.com"}))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let updated: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(updated["match_value"], "new.com");

    // Delete
    let resp = admin
        .delete(format!(
            "{}/_/api/admin/ext-auth/mappings/{}",
            server.endpoint(),
            rule_id
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

// ============================================================================
// Preview
// ============================================================================

#[tokio::test]
async fn test_preview_mapping_match() {
    let (server, admin) = setup().await;
    let group = create_group(&admin, &server.endpoint(), "employees").await;
    let gid = group["id"].as_i64().unwrap();
    create_mapping_rule(
        &admin,
        &server.endpoint(),
        "email_domain",
        "company.com",
        gid,
    )
    .await;

    let resp = admin
        .post(format!(
            "{}/_/api/admin/ext-auth/mappings/preview",
            server.endpoint()
        ))
        .json(&json!({"email": "alice@company.com"}))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let result: serde_json::Value = resp.json().await.unwrap();
    let group_names = result["group_names"].as_array().unwrap();
    assert_eq!(group_names.len(), 1);
    assert_eq!(group_names[0], "employees");
}

#[tokio::test]
async fn test_preview_mapping_no_match() {
    let (server, admin) = setup().await;
    let group = create_group(&admin, &server.endpoint(), "employees").await;
    let gid = group["id"].as_i64().unwrap();
    create_mapping_rule(
        &admin,
        &server.endpoint(),
        "email_domain",
        "company.com",
        gid,
    )
    .await;

    let resp = admin
        .post(format!(
            "{}/_/api/admin/ext-auth/mappings/preview",
            server.endpoint()
        ))
        .json(&json!({"email": "alice@other.com"}))
        .send()
        .await
        .unwrap();
    let result: serde_json::Value = resp.json().await.unwrap();
    assert!(result["group_names"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn test_preview_mapping_multiple_rules() {
    let (server, admin) = setup().await;
    let g1 = create_group(&admin, &server.endpoint(), "all-staff").await;
    let g2 = create_group(&admin, &server.endpoint(), "admins").await;

    create_mapping_rule(
        &admin,
        &server.endpoint(),
        "email_domain",
        "company.com",
        g1["id"].as_i64().unwrap(),
    )
    .await;
    create_mapping_rule(
        &admin,
        &server.endpoint(),
        "email_exact",
        "admin@company.com",
        g2["id"].as_i64().unwrap(),
    )
    .await;

    let resp = admin
        .post(format!(
            "{}/_/api/admin/ext-auth/mappings/preview",
            server.endpoint()
        ))
        .json(&json!({"email": "admin@company.com"}))
        .send()
        .await
        .unwrap();
    let result: serde_json::Value = resp.json().await.unwrap();
    let names = result["group_names"].as_array().unwrap();
    assert_eq!(names.len(), 2, "Should match both domain and exact rules");
}

// ============================================================================
// Whoami
// ============================================================================

#[tokio::test]
async fn test_whoami_no_providers() {
    let (server, _admin) = setup().await;
    let resp = reqwest::get(format!("{}/_/api/whoami", server.endpoint()))
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();

    // No external_providers field or empty
    let providers = body.get("external_providers");
    assert!(
        providers.is_none() || providers.unwrap().as_array().unwrap().is_empty(),
        "Should have no external providers on fresh server"
    );
}

#[tokio::test]
async fn test_whoami_with_provider() {
    let (server, admin) = setup().await;
    create_provider(&admin, &server.endpoint(), "google").await;

    let resp = reqwest::get(format!("{}/_/api/whoami", server.endpoint()))
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    let providers = body["external_providers"].as_array().unwrap();
    assert_eq!(providers.len(), 1);
    assert_eq!(providers[0]["name"], "google");
    assert_eq!(providers[0]["type"], "oidc");
}

#[tokio::test]
async fn test_whoami_disabled_provider_excluded() {
    let (server, admin) = setup().await;
    let provider = create_provider(&admin, &server.endpoint(), "disabled-prov").await;
    let id = provider["id"].as_i64().unwrap();

    // Disable it
    admin
        .put(format!(
            "{}/_/api/admin/ext-auth/providers/{}",
            server.endpoint(),
            id
        ))
        .json(&json!({"enabled": false}))
        .send()
        .await
        .unwrap();

    let resp = reqwest::get(format!("{}/_/api/whoami", server.endpoint()))
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    let providers = body
        .get("external_providers")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    assert!(
        providers.is_empty(),
        "Disabled provider should not appear in whoami"
    );
}

// ============================================================================
// External Identities & Sync
// ============================================================================

#[tokio::test]
async fn test_list_identities_empty() {
    let (server, admin) = setup().await;
    let resp = admin
        .get(format!(
            "{}/_/api/admin/ext-auth/identities",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let ids: Vec<serde_json::Value> = resp.json().await.unwrap();
    assert!(ids.is_empty());
}

#[tokio::test]
async fn test_sync_memberships_no_changes() {
    let (server, admin) = setup().await;
    let resp = admin
        .post(format!(
            "{}/_/api/admin/ext-auth/sync-memberships",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let result: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(result["users_updated"], 0);
    assert_eq!(result["memberships_changed"], 0);
}

// ============================================================================
// Backup includes external auth data
// ============================================================================

#[tokio::test]
async fn test_backup_includes_providers_and_rules() {
    let (server, admin) = setup().await;

    // Create provider + group + mapping rule
    create_provider(&admin, &server.endpoint(), "google").await;
    let group = create_group(&admin, &server.endpoint(), "employees").await;
    create_mapping_rule(
        &admin,
        &server.endpoint(),
        "email_domain",
        "company.com",
        group["id"].as_i64().unwrap(),
    )
    .await;

    // Export backup (legacy JSON; default response is application/zip)
    let resp = admin
        .get(format!(
            "{}/_/api/admin/backup?format=json",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let backup: serde_json::Value = resp.json().await.unwrap();

    assert_eq!(backup["version"], 2);
    assert!(!backup["auth_providers"].as_array().unwrap().is_empty());
    assert!(!backup["mapping_rules"].as_array().unwrap().is_empty());
    assert_eq!(backup["auth_providers"][0]["name"], "google");
    assert_eq!(backup["mapping_rules"][0]["match_value"], "company.com");
}

// ============================================================================
// OAuth authorize (redirect behavior)
// ============================================================================

#[tokio::test]
async fn test_oauth_authorize_no_providers() {
    let (server, _admin) = setup().await;

    // No redirect-following client
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();

    let resp = client
        .get(format!(
            "{}/_/api/admin/oauth/authorize/nonexistent",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    // Should return 404 or error page (not a 302 redirect)
    assert_ne!(resp.status(), StatusCode::FOUND);
}

#[tokio::test]
async fn test_oauth_callback_missing_state() {
    let (server, _admin) = setup().await;

    let resp = reqwest::get(format!(
        "{}/_/api/admin/oauth/callback?code=test",
        server.endpoint()
    ))
    .await
    .unwrap();
    // Should return error page (HTML), not a server error
    let status = resp.status();
    assert!(
        status.is_success() || status == StatusCode::OK,
        "Should return an error page, got {}",
        status
    );
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("Missing state parameter") || body.contains("Authentication Failed"),
        "Should show error about missing state"
    );
}

#[tokio::test]
async fn test_oauth_callback_invalid_state() {
    let (server, _admin) = setup().await;

    let resp = reqwest::get(format!(
        "{}/_/api/admin/oauth/callback?code=test&state=bogus-state-token",
        server.endpoint()
    ))
    .await
    .unwrap();
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("invalid")
            || body.contains("expired")
            || body.contains("Authentication Failed"),
        "Should show error about invalid state"
    );
}

#[tokio::test]
async fn test_oauth_callback_provider_error() {
    let (server, _admin) = setup().await;

    let resp = reqwest::get(format!(
        "{}/_/api/admin/oauth/callback?error=access_denied&error_description=User+cancelled",
        server.endpoint()
    ))
    .await
    .unwrap();
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("access_denied") || body.contains("User cancelled"),
        "Should show the provider's error"
    );
}
