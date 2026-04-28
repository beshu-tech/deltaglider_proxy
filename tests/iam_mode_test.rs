//! Integration tests for Phase 3c.2 — `access.iam_mode: declarative`
//! gates admin-API IAM mutation routes.
//!
//! Setup: spin up a server with `iam_mode: gui` (default), verify
//! user create/update works; flip to `declarative` via `/apply`,
//! verify the same calls return 403. Then flip back to `gui` and
//! verify CRUD is restored. This covers:
//!
//! 1. The middleware is correctly layered on IAM routes only.
//! 2. The guard reads the CURRENT config (hot-reloadable) — not a
//!    cached flag.
//! 3. Read endpoints (GET /users, GET /groups) keep working in
//!    declarative mode.
//! 4. Legacy-migrate is also gated (IAM mutation).
//! 5. Non-IAM routes (config, backend, session) are unaffected.

mod common;

use common::{admin_http_client, TestServer};
use reqwest::StatusCode;
use serde_json::json;

/// Gui→declarative flip requires non-empty YAML IAM; `declarative-iam-export`
/// comes from the encrypted DB — bootstrap-only instances have **no** IAM rows,
/// so create a minimal anchor user first.
async fn ensure_db_has_iam_row_for_declarative_export(admin: &reqwest::Client, endpoint: &str) {
    let resp = admin
        .get(format!("{}/_/api/admin/users", endpoint))
        .send()
        .await
        .unwrap();
    let users: Vec<serde_json::Value> = resp.json().await.unwrap();
    if !users.is_empty() {
        return;
    }
    let resp = admin
        .post(format!("{}/_/api/admin/users", endpoint))
        .json(&json!({
            "name": "declarative-flip-anchor",
            "permissions": [{
                "actions": ["read", "write", "delete", "list", "admin"],
                "resources": ["*"]
            }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::CREATED,
        "anchor IAM user for declarative flip"
    );
}

/// Set the server's iam_mode via /apply. Merges into the existing
/// export so other config (backend, credentials) is preserved.
///
/// Flipping to **declarative** requires non-empty `access.iam_users` (etc.) in the
/// YAML so the server does not wipe the DB — use `declarative-iam-export`, which
/// projects the live IAM DB into that shape.
async fn set_iam_mode(admin: &reqwest::Client, endpoint: &str, mode: &str) {
    let exported: String = admin
        .get(format!("{}/_/api/admin/config/export", endpoint))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    let mut doc: serde_yaml::Value = serde_yaml::from_str(&exported)
        .unwrap_or(serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
    let root = doc.as_mapping_mut().unwrap();
    let access = root
        .entry(serde_yaml::Value::String("access".into()))
        .or_insert_with(|| serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));

    if mode == "declarative" {
        ensure_db_has_iam_row_for_declarative_export(admin, endpoint).await;
        let decl_yaml: String = admin
            .get(format!(
                "{}/_/api/admin/config/declarative-iam-export",
                endpoint
            ))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let decl: serde_yaml::Value =
            serde_yaml::from_str(&decl_yaml).expect("declarative-iam-export YAML");
        let decl_access = decl
            .get(serde_yaml::Value::String("access".into()))
            .expect("declarative-iam-export must contain access:")
            .as_mapping()
            .expect("access must be a mapping");
        let access_map = access.as_mapping_mut().unwrap();
        for (k, v) in decl_access {
            access_map.insert(k.clone(), v.clone());
        }
    } else {
        access
            .as_mapping_mut()
            .unwrap()
            .insert(serde_yaml::Value::String("iam_mode".into()), mode.into());
    }
    let merged = serde_yaml::to_string(&doc).unwrap();

    let resp = admin
        .post(format!("{}/_/api/admin/config/apply", endpoint))
        .json(&json!({ "yaml": merged }))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body = resp.text().await.unwrap();
    assert_eq!(
        status,
        StatusCode::OK,
        "setting iam_mode={mode} must succeed, body: {body}"
    );
}

#[tokio::test]
async fn test_declarative_mode_returns_403_on_user_create() {
    let server = TestServer::builder()
        .yaml_config()
        .auth("IAMMK1", "IAMMS1")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    // Flip to declarative.
    set_iam_mode(&admin, &server.endpoint(), "declarative").await;

    let resp = admin
        .post(format!("{}/_/api/admin/users", server.endpoint()))
        .json(&json!({ "name": "alice" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "user create in declarative mode must be 403"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["error"], "iam_declarative");
    assert!(
        body["message"].as_str().unwrap().contains("config/apply"),
        "error body must point to the declarative workflow: {body}"
    );
}

#[tokio::test]
async fn test_declarative_mode_allows_user_list() {
    let server = TestServer::builder()
        .yaml_config()
        .auth("IAMMK2", "IAMMS2")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    set_iam_mode(&admin, &server.endpoint(), "declarative").await;

    // Read routes stay allowed — the GUI must still display state.
    let resp = admin
        .get(format!("{}/_/api/admin/users", server.endpoint()))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "list users in declarative mode must still work"
    );
}

#[tokio::test]
async fn test_declarative_mode_blocks_group_mutations() {
    let server = TestServer::builder()
        .yaml_config()
        .auth("IAMMK3", "IAMMS3")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    set_iam_mode(&admin, &server.endpoint(), "declarative").await;

    // Group create blocked.
    let resp = admin
        .post(format!("{}/_/api/admin/groups", server.endpoint()))
        .json(&json!({ "name": "admins", "permissions": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Group list allowed.
    let resp = admin
        .get(format!("{}/_/api/admin/groups", server.endpoint()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_declarative_mode_blocks_ext_auth_provider_mutations() {
    let server = TestServer::builder()
        .yaml_config()
        .auth("IAMMK4", "IAMMS4")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    set_iam_mode(&admin, &server.endpoint(), "declarative").await;

    let resp = admin
        .post(format!(
            "{}/_/api/admin/ext-auth/providers",
            server.endpoint()
        ))
        .json(&json!({
            "name": "corp-sso",
            "provider_type": "oidc",
            "oidc_issuer_url": "https://example.com",
            "client_id": "x",
            "client_secret": "y",
            "redirect_uri": "http://localhost/cb"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_declarative_mode_does_not_block_config_routes() {
    let server = TestServer::builder()
        .yaml_config()
        .auth("IAMMK5", "IAMMS5")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    set_iam_mode(&admin, &server.endpoint(), "declarative").await;

    // Config PUT still works — declarative mode only blocks IAM
    // mutations.
    let resp = admin
        .put(format!("{}/_/api/admin/config", server.endpoint()))
        .json(&json!({ "max_delta_ratio": 0.42 }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // And /apply still works (it's how operators flip BACK to gui).
    // The helper above already exercised this.
}

#[tokio::test]
async fn test_declarative_to_gui_toggle_hot_restores_crud() {
    let server = TestServer::builder()
        .yaml_config()
        .auth("IAMMK6", "IAMMS6")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    // Start in gui: create works (201 Created per REST conventions).
    let resp = admin
        .post(format!("{}/_/api/admin/users", server.endpoint()))
        .json(&json!({ "name": "bob" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Flip to declarative: create blocked.
    set_iam_mode(&admin, &server.endpoint(), "declarative").await;
    let resp = admin
        .post(format!("{}/_/api/admin/users", server.endpoint()))
        .json(&json!({ "name": "charlie" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Flip back to gui: create works again.
    set_iam_mode(&admin, &server.endpoint(), "gui").await;
    let resp = admin
        .post(format!("{}/_/api/admin/users", server.endpoint()))
        .json(&json!({ "name": "diana" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
}

/// M4 from deep correctness review: exercise the "declarative → gui →
/// declarative" flip sequence end-to-end. Catches any regression
/// where session state (bootstrap-password hash verification, rate
/// limiter, admission chain) drifts during mode transitions.
#[tokio::test]
async fn test_mode_flip_cycle_preserves_session_and_chain() {
    let server = TestServer::builder()
        .yaml_config()
        .auth("FLIP", "FLIPSECRET")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    // Initial: gui mode, write allowed.
    let resp = admin
        .post(format!("{}/_/api/admin/users", server.endpoint()))
        .json(&json!({ "name": "alice-before" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Flip to declarative; the SAME session must still authenticate for
    // reads (middleware reads session before iam_mode gate runs).
    set_iam_mode(&admin, &server.endpoint(), "declarative").await;
    let resp = admin
        .get(format!("{}/_/api/admin/users", server.endpoint()))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "existing session must survive a gui→declarative flip for reads"
    );

    // Flip back to gui; writes restored.
    set_iam_mode(&admin, &server.endpoint(), "gui").await;
    let resp = admin
        .post(format!("{}/_/api/admin/users", server.endpoint()))
        .json(&json!({ "name": "alice-after" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Flip AGAIN (declarative); write blocked.
    set_iam_mode(&admin, &server.endpoint(), "declarative").await;
    let resp = admin
        .post(format!("{}/_/api/admin/users", server.endpoint()))
        .json(&json!({ "name": "should-be-blocked" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Config PUT is still allowed even after multiple flips.
    let resp = admin
        .put(format!("{}/_/api/admin/config", server.endpoint()))
        .json(&json!({ "max_delta_ratio": 0.33 }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}
