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

// ═══════════════════════════════════════════════════
// Phase 1 — Document-level config operations
// ═══════════════════════════════════════════════════

#[tokio::test]
async fn test_config_export_returns_yaml_with_secrets_redacted() {
    let server = TestServer::builder()
        .auth("EXPORTKEY", "EXPORTSECRETVALUE")
        .max_delta_ratio(0.42)
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    let resp = admin
        .get(format!("{}/_/api/admin/config/export", server.endpoint()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp.headers().get("content-type").unwrap();
    assert!(
        ct.to_str().unwrap().contains("yaml"),
        "content-type should indicate YAML, got {:?}",
        ct
    );
    let body = resp.text().await.unwrap();

    // Non-secret fields survive
    assert!(body.contains("0.42"), "max_delta_ratio should be present");
    // SigV4 creds must be redacted.
    assert!(
        !body.contains("EXPORTSECRETVALUE"),
        "SigV4 secret must be redacted from export, got: {body}"
    );
    assert!(
        !body.contains("EXPORTKEY"),
        "SigV4 access key must be redacted from export, got: {body}"
    );
}

#[tokio::test]
async fn test_config_defaults_returns_schema() {
    let server = TestServer::builder()
        .auth("DEFKEY", "DEFSECRET")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    let resp = admin
        .get(format!("{}/_/api/admin/config/defaults", server.endpoint()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let schema: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(schema["title"], "Config");
    assert!(schema["properties"].is_object());
    assert!(
        schema["properties"]["max_delta_ratio"].is_object(),
        "schema must describe max_delta_ratio field"
    );
}

#[tokio::test]
async fn test_config_validate_accepts_valid_yaml() {
    let server = TestServer::builder()
        .auth("VALKEY", "VALSECRET")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    let yaml = r#"
listen_addr: "127.0.0.1:9999"
max_delta_ratio: 0.5
backend:
  type: filesystem
  path: /tmp/dgp-validate-test
"#;

    let resp = admin
        .post(format!("{}/_/api/admin/config/validate", server.endpoint()))
        .json(&json!({ "yaml": yaml }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    assert!(body["error"].is_null());
}

#[tokio::test]
async fn test_config_validate_rejects_malformed_yaml() {
    let server = TestServer::builder()
        .auth("VALKEY2", "VALSECRET2")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    // Structurally invalid: max_delta_ratio must be a number, not a mapping.
    let resp = admin
        .post(format!("{}/_/api/admin/config/validate", server.endpoint()))
        .json(&json!({ "yaml": "max_delta_ratio:\n  not: a number\n" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], false);
    assert!(body["error"].is_string());
}

#[tokio::test]
async fn test_config_validate_reports_warnings_for_suspicious_values() {
    let server = TestServer::builder()
        .auth("VALKEY3", "VALSECRET3")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    // Valid YAML but max_delta_ratio out of [0, 1] — should warn, not fail.
    let yaml = r#"
listen_addr: "127.0.0.1:9000"
max_delta_ratio: 1.5
backend:
  type: filesystem
  path: /tmp/dgp-warn-test
"#;
    let resp = admin
        .post(format!("{}/_/api/admin/config/validate", server.endpoint()))
        .json(&json!({ "yaml": yaml }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    let warnings = body["warnings"].as_array().unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().unwrap().contains("max_delta_ratio")),
        "expected warning about out-of-range max_delta_ratio, got {warnings:?}"
    );
}

#[tokio::test]
async fn test_config_apply_hot_reloads_ratio() {
    let server = TestServer::builder()
        .auth("APPLYKEY", "APPLYSECRET")
        .max_delta_ratio(0.75)
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    // First export to get a canonical full doc (with secrets redacted).
    let exported = admin
        .get(format!("{}/_/api/admin/config/export", server.endpoint()))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    // Flip max_delta_ratio to 0.3. Replace the line in the exported doc.
    let modified = exported
        .lines()
        .map(|line| {
            if line.starts_with("max_delta_ratio:") {
                "max_delta_ratio: 0.3".to_string()
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    let resp = admin
        .post(format!("{}/_/api/admin/config/apply", server.endpoint()))
        .json(&json!({ "yaml": modified }))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let text = resp.text().await.unwrap();
    assert_eq!(status, StatusCode::OK, "apply should succeed, body: {text}");
    let body: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["applied"], true);
    assert!(body["error"].is_null());

    // GET /config now reflects the new ratio.
    let cfg: serde_json::Value = admin
        .get(format!("{}/_/api/admin/config", server.endpoint()))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        (cfg["max_delta_ratio"].as_f64().unwrap() - 0.3).abs() < 1e-6,
        "max_delta_ratio should be 0.3 after apply, got {}",
        cfg["max_delta_ratio"]
    );
}

#[tokio::test]
async fn test_config_apply_rejects_bad_yaml_without_state_change() {
    let server = TestServer::builder()
        .auth("APPLY2KEY", "APPLY2SECRET")
        .max_delta_ratio(0.77)
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    // max_delta_ratio needs to be a number; feeding a map here makes this
    // YAML syntactically legal but structurally invalid for Config → parse
    // fails at typed-deserialization time.
    let resp = admin
        .post(format!("{}/_/api/admin/config/apply", server.endpoint()))
        .json(&json!({ "yaml": "max_delta_ratio:\n  not: a number\n" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["applied"], false);

    // State must be unchanged.
    let cfg: serde_json::Value = admin
        .get(format!("{}/_/api/admin/config", server.endpoint()))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        (cfg["max_delta_ratio"].as_f64().unwrap() - 0.77).abs() < 1e-6,
        "max_delta_ratio should be unchanged after failed apply"
    );
}

#[tokio::test]
async fn test_config_export_apply_export_is_idempotent() {
    // Plan's Phase 1 acceptance criterion: export → apply → export must
    // produce byte-identical YAML. Any drift here means apply is not truly
    // atomic-full-document semantics and operators can't trust round-trips.
    let server = TestServer::builder()
        .auth("IDEMKEY", "IDEMSECRET")
        .max_delta_ratio(0.55)
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    let first_export = admin
        .get(format!("{}/_/api/admin/config/export", server.endpoint()))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    // Apply it unchanged. Must succeed without touching anything semantically.
    let resp = admin
        .post(format!("{}/_/api/admin/config/apply", server.endpoint()))
        .json(&json!({ "yaml": first_export }))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let text = resp.text().await.unwrap();
    assert_eq!(status, StatusCode::OK, "apply should succeed, body: {text}");

    let second_export = admin
        .get(format!("{}/_/api/admin/config/export", server.endpoint()))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    assert_eq!(
        first_export, second_export,
        "export → apply → export must be byte-identical"
    );
}

#[tokio::test]
async fn test_config_apply_preserves_sigv4_secret_on_redacted_roundtrip() {
    // Critical safety invariant: exported YAML has secrets redacted; when
    // POSTed back via apply, the server MUST preserve the runtime secret
    // rather than clearing it. Otherwise every "Copy as YAML → edit → apply"
    // round-trip would silently break auth.
    let server = TestServer::builder()
        .auth("PRESERVEKEY", "PRESERVESECRETVALUE")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    // Export (secrets redacted in the YAML).
    let exported = admin
        .get(format!("{}/_/api/admin/config/export", server.endpoint()))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        !exported.contains("PRESERVESECRETVALUE"),
        "precondition: export should redact the secret"
    );

    // Apply the exported-as-is YAML — no edits, secrets still absent.
    let resp = admin
        .post(format!("{}/_/api/admin/config/apply", server.endpoint()))
        .json(&json!({ "yaml": exported }))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let text = resp.text().await.unwrap();
    assert_eq!(status, StatusCode::OK, "apply should succeed, body: {text}");
    let body: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["applied"], true);

    // auth_enabled must still be true and access_key_id preserved.
    let cfg: serde_json::Value = admin
        .get(format!("{}/_/api/admin/config", server.endpoint()))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(cfg["auth_enabled"], true);
    assert_eq!(cfg["access_key_id"], "PRESERVEKEY");
}

// ═══════════════════════════════════════════════════
// Phase 1 — audit-driven correctness regressions
// ═══════════════════════════════════════════════════

#[tokio::test]
async fn test_apply_rejects_empty_yaml_body() {
    let server = TestServer::builder()
        .auth("EMPTYK", "EMPTYS")
        .max_delta_ratio(0.42)
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    // Empty YAML would silently deserialize to Config::default() without this
    // guard — catastrophic for any non-default deployment.
    for body in ["", "   ", "\n\n", "\t  \n  "] {
        let resp = admin
            .post(format!("{}/_/api/admin/config/apply", server.endpoint()))
            .json(&json!({ "yaml": body }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "empty YAML {body:?} should be rejected"
        );
    }

    // State unchanged.
    let cfg: serde_json::Value = admin
        .get(format!("{}/_/api/admin/config", server.endpoint()))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!((cfg["max_delta_ratio"].as_f64().unwrap() - 0.42).abs() < 1e-6);
}

#[tokio::test]
async fn test_apply_rejects_invalid_log_level_before_swap() {
    let server = TestServer::builder()
        .auth("LOGFK", "LOGFS")
        .max_delta_ratio(0.42)
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    let exported = admin
        .get(format!("{}/_/api/admin/config/export", server.endpoint()))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    // Replace log_level with garbage that EnvFilter rejects.
    let modified = exported
        .lines()
        .map(|line| {
            if line.starts_with("log_level:") {
                "log_level: \"!!!not-a-valid-filter\"".to_string()
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    let resp = admin
        .post(format!("{}/_/api/admin/config/apply", server.endpoint()))
        .json(&json!({ "yaml": modified }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Runtime max_delta_ratio unchanged — apply must have refused BEFORE any state swap.
    let cfg: serde_json::Value = admin
        .get(format!("{}/_/api/admin/config", server.endpoint()))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!((cfg["max_delta_ratio"].as_f64().unwrap() - 0.42).abs() < 1e-6);
}

#[tokio::test]
async fn test_apply_rejects_bootstrap_password_hash_change() {
    let server = TestServer::builder().auth("BOOTFK", "BOOTFS").build().await;
    let admin = admin_http_client(&server.endpoint()).await;

    // Build a YAML with an unrelated bcrypt hash. The real hash is server-
    // generated and not exposed via any read endpoint, so any operator-
    // provided hash is by definition different from the runtime one.
    let yaml = r#"
listen_addr: "127.0.0.1:9000"
max_delta_ratio: 0.5
bootstrap_password_hash: "$2b$12$abcdefghijklmnopqrstuvAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
backend:
  type: filesystem
  path: /tmp/dgp-bootstrap-reject
log_level: "info"
"#;

    let resp = admin
        .post(format!("{}/_/api/admin/config/apply", server.endpoint()))
        .json(&json!({ "yaml": yaml }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "apply must refuse bootstrap password changes"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["applied"], false);
    assert_eq!(body["persisted"], false);
    assert!(body["error"]
        .as_str()
        .unwrap()
        .contains("bootstrap_password_hash"));
}

#[tokio::test]
async fn test_apply_response_has_persisted_field() {
    // Happy path: persisted must be true and the HTTP status must be 200.
    let server = TestServer::builder()
        .auth("PERSFK", "PERSFS")
        .max_delta_ratio(0.55)
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    let exported = admin
        .get(format!("{}/_/api/admin/config/export", server.endpoint()))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    let resp = admin
        .post(format!("{}/_/api/admin/config/apply", server.endpoint()))
        .json(&json!({ "yaml": exported }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["applied"], true);
    assert_eq!(
        body["persisted"], true,
        "happy-path apply must set persisted: true"
    );
    assert!(body["persisted_path"].is_string());
}

#[tokio::test]
async fn test_apply_warns_on_asymmetric_sigv4_credentials() {
    // The operator sets only access_key_id on the incoming YAML. We must
    // NOT cross-wire the runtime secret_access_key with the new access_key_id
    // — that would produce a plausibly-authenticated state that silently
    // fails at signature verification. Warn instead.
    let server = TestServer::builder()
        .auth("ASYMK", "ASYMSECRET")
        .max_delta_ratio(0.5)
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    let exported = admin
        .get(format!("{}/_/api/admin/config/export", server.endpoint()))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    // Set only access_key_id; leave secret_access_key absent (= redacted None).
    let modified = exported
        .lines()
        .map(|line| {
            if line.starts_with("access_key_id:") {
                "access_key_id: \"NEWKEY\"".to_string()
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    let resp = admin
        .post(format!("{}/_/api/admin/config/apply", server.endpoint()))
        .json(&json!({ "yaml": modified }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["applied"], true);
    let warnings = body["warnings"]
        .as_array()
        .expect("warnings must be an array");
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().unwrap().contains("asymmetric")),
        "expected asymmetric-credentials warning, got {warnings:?}"
    );
}

#[tokio::test]
async fn test_backend_mutations_persist_to_configured_file_not_cwd_default() {
    // Regression coverage for a latent bug in `api/admin/backends.rs`:
    // two `persist_to_file` sites used `DEFAULT_CONFIG_FILENAME` directly
    // instead of `active_config_path(&state)`. When the operator had
    // launched with `--config /some/other/path`, admin-API backend
    // creations silently wrote to the wrong file — the in-memory config
    // had the new backend, the file the server reloaded from on restart
    // did not. This test verifies the fix: a backend create made via the
    // admin API is readable back from the SAME config file the server
    // was spawned with.
    let server = TestServer::builder()
        .auth("PERSISTKEY", "PERSISTSECRET")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    let tmp = tempfile::tempdir().unwrap();
    let backend_path = tmp.path().to_str().unwrap();

    // Create a backend via the admin API. This mutates state.config AND
    // must persist to the configured file.
    let resp = admin
        .post(format!("{}/_/api/admin/backends", server.endpoint()))
        .json(&json!({
            "name": "persist-regression-backend",
            "type": "filesystem",
            "path": backend_path
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // Read the config file the server was spawned with — the new backend
    // must appear here. If the fix regresses, the file will not contain
    // the backend (it went to `deltaglider_proxy.toml` in CWD instead).
    let on_disk = std::fs::read_to_string(server.config_path()).unwrap();
    assert!(
        on_disk.contains("persist-regression-backend"),
        "backend must be persisted to the configured file ({}); instead its contents are: {}",
        server.config_path().display(),
        on_disk
    );
}

// ═══════════════════════════════════════════════════
// Hygiene pass #2 — audit-driven regression coverage
// ═══════════════════════════════════════════════════

#[tokio::test]
async fn test_update_config_rejects_invalid_log_level() {
    // Regression: the PATCH path used to write the bad filter into
    // `cfg.log_level` and then warn. That poisoned the on-disk config;
    // the document-level APPLY path already validated up-front. Fix
    // makes PATCH symmetric with APPLY.
    let server = TestServer::builder()
        .auth("PATCHLL", "PATCHLLSECRET")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    // Baseline log level before the attempt.
    let before: serde_json::Value = admin
        .get(format!("{}/_/api/admin/config", server.endpoint()))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let baseline = before["log_level"].as_str().unwrap().to_string();

    let resp = admin
        .put(format!("{}/_/api/admin/config", server.endpoint()))
        .json(&json!({ "log_level": "!!!invalid filter!!!" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    let warnings = body["warnings"].as_array().unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().unwrap().contains("log_level")),
        "expected log_level warning, got {warnings:?}"
    );

    // Invalid filter must NOT have been stored.
    let after: serde_json::Value = admin
        .get(format!("{}/_/api/admin/config", server.endpoint()))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        after["log_level"].as_str().unwrap(),
        baseline,
        "bad filter must not be persisted to runtime config"
    );
}

#[tokio::test]
async fn test_update_config_rejects_empty_backend_path() {
    // Empty PathBuf silently becomes CWD on filesystem backends — almost
    // always an uncleared GUI form field. Reject up-front.
    let server = TestServer::builder()
        .auth("PATCHBP", "PATCHBPSECRET")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    let resp = admin
        .put(format!("{}/_/api/admin/config", server.endpoint()))
        .json(&json!({ "backend_path": "" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    let warnings = body["warnings"].as_array().unwrap();
    assert!(
        warnings
            .iter()
            .any(|w| w.as_str().unwrap().contains("backend_path")),
        "expected backend_path warning, got {warnings:?}"
    );
}

#[tokio::test]
async fn test_update_config_accepts_case_insensitive_backend_type() {
    // A client sending `backend_type: "S3"` (uppercase) used to get an
    // "Unknown backend type" warning. The canonical on-the-wire value is
    // lowercase, so re-POSTing what ConfigResponse exposed was fine, but
    // manual API callers got bitten. Normalise on the receive side.
    let server = TestServer::builder()
        .auth("PATCHBTT", "PATCHBTTSECRET")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    let resp = admin
        .put(format!("{}/_/api/admin/config", server.endpoint()))
        .json(&json!({ "backend_type": "S3" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    let warnings = body["warnings"].as_array().unwrap();
    assert!(
        !warnings
            .iter()
            .any(|w| w.as_str().unwrap().contains("Unknown backend type")),
        "uppercase 'S3' must be accepted, got warnings: {warnings:?}"
    );
}
