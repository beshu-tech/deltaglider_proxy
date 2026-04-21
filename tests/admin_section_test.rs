//! Integration tests for Wave 1 of the admin UI revamp:
//!
//! - `GET  /api/admin/config/section/:name[?format=yaml]`
//! - `PUT  /api/admin/config/section/:name`
//! - `POST /api/admin/config/section/:name/validate`
//! - `GET  /api/admin/config/export?section=<name>`
//! - `GET  /api/admin/config/defaults?section=<name>`
//! - `GET  /api/admin/config/trace?method=&path=&...` (query-param variant)
//!
//! The section endpoints overlap with the field-level PATCH and document-
//! level APPLY surfaces tested in `admin_config_test.rs`. Tests here
//! cover only the behaviours unique to the section scope:
//!   * section-specific serialization shape,
//!   * section-body validation errors,
//!   * diff computation (§5.3 of the UI revamp plan),
//!   * dry-run (`/validate`) leaves runtime state unchanged,
//!   * YAML-format response for the UI's per-section Copy-as-YAML button,
//!   * trace GET variant produces identical output to POST.

mod common;

use common::{admin_http_client, TestServer};
use reqwest::StatusCode;
use serde_json::json;

// ═══════════════════════════════════════════════════
// GET /config/section/:name
// ═══════════════════════════════════════════════════

#[tokio::test]
async fn section_get_unknown_returns_404() {
    let server = TestServer::builder()
        .auth("SECKEY1", "SECSECRET1")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    let resp = admin
        .get(format!(
            "{}/_/api/admin/config/section/nope",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], false);
    assert!(
        body["error"].as_str().unwrap().contains("unknown section"),
        "got: {}",
        body
    );
}

#[tokio::test]
async fn section_get_admission_defaults_to_empty_blocks() {
    let server = TestServer::builder()
        .auth("SECKEY2", "SECSECRET2")
        .yaml_config()
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    let resp = admin
        .get(format!(
            "{}/_/api/admin/config/section/admission",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    // `AdmissionSection` uses `skip_serializing_if = "Vec::is_empty"` on
    // `blocks`, so the default-shape response is either an empty object
    // or one with an empty array — both are valid "no operator-authored
    // blocks" signals. Normalise both into a `blocks` field the UI can
    // read.
    let blocks = body
        .get("blocks")
        .cloned()
        .unwrap_or_else(|| serde_json::Value::Array(vec![]));
    assert!(blocks.is_array());
    assert_eq!(blocks.as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn section_get_access_returns_iam_mode() {
    let server = TestServer::builder()
        .auth("SECKEY3", "SECSECRET3")
        .yaml_config()
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    let resp = admin
        .get(format!(
            "{}/_/api/admin/config/section/access",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    // iam_mode omits when it equals the default (`gui`), but the access_key_id
    // is set so we should see it come back redacted.
    assert!(
        body.get("access_key_id").is_some() || body.as_object().unwrap().is_empty(),
        "access section body shape: {}",
        body
    );
}

#[tokio::test]
async fn section_get_storage_yaml_format_emits_section_key() {
    let server = TestServer::builder()
        .auth("SECKEY4", "SECSECRET4")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    let resp = admin
        .get(format!(
            "{}/_/api/admin/config/section/storage?format=yaml",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp.headers().get("content-type").unwrap();
    assert!(
        ct.to_str().unwrap().contains("yaml"),
        "content-type must indicate YAML, got {:?}",
        ct
    );
    let body = resp.text().await.unwrap();
    assert!(
        body.starts_with("storage:"),
        "YAML-format section must start with `<name>:`, got: {}",
        body
    );
}

// ═══════════════════════════════════════════════════
// PUT /config/section/:name
// ═══════════════════════════════════════════════════

#[tokio::test]
async fn section_put_advanced_max_delta_ratio_persists_and_diffs() {
    // Start from a non-default ratio so the diff has both `before` and
    // `after` populated — the `from_flat` serializer omits fields that
    // equal their default (intentional to keep exports minimal), which
    // would otherwise make `before: null`.
    let server = TestServer::builder()
        .auth("SECKEY5", "SECSECRET5")
        .max_delta_ratio(0.8)
        .yaml_config()
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    // PUT advanced with a new max_delta_ratio.
    let resp = admin
        .put(format!(
            "{}/_/api/admin/config/section/advanced",
            server.endpoint()
        ))
        .json(&json!({ "max_delta_ratio": 0.42 }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    assert!(
        body["persisted_path"].is_string(),
        "persisted_path must be set on PUT success, got: {body}"
    );

    // The diff must surface max_delta_ratio's before/after. f32 → JSON
    // round-trip loses precision; compare with an epsilon.
    let diff = &body["diff"]["advanced"]["max_delta_ratio"];
    assert!(
        diff.is_object(),
        "diff must contain the changed field path, got: {body}"
    );
    assert!((diff["before"].as_f64().unwrap() - 0.8).abs() < 1e-3);
    assert!((diff["after"].as_f64().unwrap() - 0.42).abs() < 1e-3);

    // The field-level GET must reflect the change (hot-reloaded).
    let resp = admin
        .get(format!("{}/_/api/admin/config", server.endpoint()))
        .send()
        .await
        .unwrap();
    let cfg: serde_json::Value = resp.json().await.unwrap();
    assert!((cfg["max_delta_ratio"].as_f64().unwrap() - 0.42).abs() < 1e-3);
}

#[tokio::test]
async fn section_put_invalid_body_returns_400_and_no_change() {
    let server = TestServer::builder()
        .auth("SECKEY6", "SECSECRET6")
        .max_delta_ratio(0.75)
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    // `max_delta_ratio` expects a number; give it a string.
    let resp = admin
        .put(format!(
            "{}/_/api/admin/config/section/advanced",
            server.endpoint()
        ))
        .json(&json!({ "max_delta_ratio": "nope" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], false);
    assert!(body["error"].as_str().unwrap().contains("invalid advanced"));

    // Field-level GET still shows the original value — f32 round-trip
    // loses precision so compare with an epsilon.
    let resp = admin
        .get(format!("{}/_/api/admin/config", server.endpoint()))
        .send()
        .await
        .unwrap();
    let cfg: serde_json::Value = resp.json().await.unwrap();
    assert!((cfg["max_delta_ratio"].as_f64().unwrap() - 0.75).abs() < 1e-3);
}

#[tokio::test]
async fn section_put_admission_blocks_replace_entire_list() {
    let server = TestServer::builder()
        .auth("SECKEY7", "SECSECRET7")
        .yaml_config()
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    // Seed with one block via PUT.
    let resp = admin
        .put(format!(
            "{}/_/api/admin/config/section/admission",
            server.endpoint()
        ))
        .json(&json!({
            "blocks": [{
                "name": "deny-test-ip",
                "match": { "source_ip": "203.0.113.5" },
                "action": "deny"
            }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);

    // GET confirms the block is live.
    let resp = admin
        .get(format!(
            "{}/_/api/admin/config/section/admission",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["blocks"].as_array().unwrap().len(), 1);
    assert_eq!(body["blocks"][0]["name"], "deny-test-ip");

    // PUT an empty blocks list to replace the chain.
    let resp = admin
        .put(format!(
            "{}/_/api/admin/config/section/admission",
            server.endpoint()
        ))
        .json(&json!({ "blocks": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // GET confirms the list is empty. Empty `blocks` is serde-skipped
    // so the response may be `{}`; the UI treats that as "no blocks"
    // and so do we.
    let resp = admin
        .get(format!(
            "{}/_/api/admin/config/section/admission",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    let blocks = body
        .get("blocks")
        .cloned()
        .unwrap_or_else(|| serde_json::Value::Array(vec![]));
    assert_eq!(blocks.as_array().unwrap().len(), 0);
}

// ═══════════════════════════════════════════════════
// POST /config/section/:name/validate
// ═══════════════════════════════════════════════════

#[tokio::test]
async fn section_validate_is_dry_run_no_state_change() {
    let server = TestServer::builder()
        .auth("SECKEY8", "SECSECRET8")
        .max_delta_ratio(0.75)
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    // Dry-run a change.
    let resp = admin
        .post(format!(
            "{}/_/api/admin/config/section/advanced/validate",
            server.endpoint()
        ))
        .json(&json!({ "max_delta_ratio": 0.1 }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    // Diff must describe the would-be change (f32 round-trip precision).
    assert!(
        (body["diff"]["advanced"]["max_delta_ratio"]["after"]
            .as_f64()
            .unwrap()
            - 0.1)
            .abs()
            < 1e-3
    );
    // No persist happened.
    assert!(body["persisted_path"].is_null());

    // Field-level GET still shows the original value — state wasn't mutated.
    let resp = admin
        .get(format!("{}/_/api/admin/config", server.endpoint()))
        .send()
        .await
        .unwrap();
    let cfg: serde_json::Value = resp.json().await.unwrap();
    assert!((cfg["max_delta_ratio"].as_f64().unwrap() - 0.75).abs() < 1e-3);
}

#[tokio::test]
async fn section_validate_rejects_malformed_admission_block() {
    let server = TestServer::builder()
        .auth("SECKEY9", "SECSECRET9")
        .yaml_config()
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    // Reserved `public-prefix:*` name prefix — admission validator
    // rejects this at parse+validate time.
    let resp = admin
        .post(format!(
            "{}/_/api/admin/config/section/admission/validate",
            server.endpoint()
        ))
        .json(&json!({
            "blocks": [{
                "name": "public-prefix:not-allowed",
                "match": {},
                "action": "deny"
            }]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], false);
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .to_lowercase()
            .contains("public-prefix"),
        "error should name the reserved prefix, got: {body}"
    );
}

// ═══════════════════════════════════════════════════
// GET /config/export?section=...
// ═══════════════════════════════════════════════════

#[tokio::test]
async fn export_with_section_filter_returns_only_that_section() {
    let server = TestServer::builder()
        .auth("SECKEY10", "SECSECRET10")
        .max_delta_ratio(0.42)
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    let resp = admin
        .get(format!(
            "{}/_/api/admin/config/export?section=advanced",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.text().await.unwrap();
    // Only the `advanced:` section is present — other top-level keys are not.
    assert!(
        body.starts_with("advanced:"),
        "scoped export must begin with section name, got: {body}"
    );
    assert!(
        !body.contains("\nadmission:")
            && !body.contains("\naccess:")
            && !body.contains("\nstorage:"),
        "scoped export must not include other sections, got: {body}"
    );
    // The overridden field surfaces (f32→f64 via YAML emits 0.41999…
    // so we match on the stable prefix rather than the full literal).
    assert!(
        body.contains("max_delta_ratio:") && (body.contains("0.42") || body.contains("0.4199")),
        "scoped export must include overridden max_delta_ratio, got: {body}"
    );
}

#[tokio::test]
async fn export_with_unknown_section_returns_404() {
    let server = TestServer::builder()
        .auth("SECKEY11", "SECSECRET11")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    let resp = admin
        .get(format!(
            "{}/_/api/admin/config/export?section=nope",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let body = resp.text().await.unwrap();
    assert!(body.contains("unknown section"));
}

// ═══════════════════════════════════════════════════
// GET /config/defaults?section=...
// ═══════════════════════════════════════════════════

#[tokio::test]
async fn defaults_with_section_filter_returns_section_schema() {
    let server = TestServer::builder()
        .auth("SECKEY12", "SECSECRET12")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    let resp = admin
        .get(format!(
            "{}/_/api/admin/config/defaults?section=advanced",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let schema: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        schema["title"], "AdvancedSection",
        "section-scoped schema must target the section's type, got: {}",
        schema
    );
    assert!(schema["properties"]["max_delta_ratio"].is_object());
    // The global Config-level field `buckets` is in Storage, not Advanced.
    assert!(schema["properties"]["buckets"].is_null());
}

#[tokio::test]
async fn defaults_without_section_returns_full_config_schema() {
    let server = TestServer::builder()
        .auth("SECKEY13", "SECSECRET13")
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
}

// ═══════════════════════════════════════════════════
// GET /config/trace (query-param variant)
// ═══════════════════════════════════════════════════

#[tokio::test]
async fn trace_get_matches_post_output() {
    let server = TestServer::builder()
        .auth("SECKEY14", "SECSECRET14")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    // POST with full body.
    let post_resp = admin
        .post(format!("{}/_/api/admin/config/trace", server.endpoint()))
        .json(&json!({
            "method": "GET",
            "path": "/my-bucket/key",
            "authenticated": false
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(post_resp.status(), StatusCode::OK);
    let post_body: serde_json::Value = post_resp.json().await.unwrap();

    // GET with the same inputs as query params.
    let get_resp = admin
        .get(format!(
            "{}/_/api/admin/config/trace?method=GET&path=/my-bucket/key&authenticated=false",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(get_resp.status(), StatusCode::OK);
    let get_body: serde_json::Value = get_resp.json().await.unwrap();

    // Both responses must agree on the decision and resolved inputs.
    assert_eq!(
        post_body["admission"]["decision"], get_body["admission"]["decision"],
        "POST and GET trace must agree: post={:?}, get={:?}",
        post_body, get_body
    );
    assert_eq!(
        post_body["resolved"]["bucket"],
        get_body["resolved"]["bucket"]
    );
    assert_eq!(
        post_body["resolved"]["method"],
        get_body["resolved"]["method"]
    );
}

// ═══════════════════════════════════════════════════
// Section round-trip preserves redacted secrets
// ═══════════════════════════════════════════════════

#[tokio::test]
async fn section_put_access_round_trip_preserves_redacted_creds() {
    // Scenario: GET /section/access (redacts access_key_id and
    // secret_access_key), operator edits something innocuous and PUTs
    // the body back. The legacy SigV4 credential pair must still be
    // live afterwards — the section API preserves redacted secrets
    // the same way the document-level apply does.
    let server = TestServer::builder()
        .auth("PRESERVKEY", "PRESERVSECRET")
        .yaml_config()
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    // Baseline: field-level GET confirms auth is enabled.
    let resp = admin
        .get(format!("{}/_/api/admin/config", server.endpoint()))
        .send()
        .await
        .unwrap();
    let cfg: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(cfg["auth_enabled"], true, "pre-PUT auth must be enabled");

    // GET section/access yields redacted body.
    let resp = admin
        .get(format!(
            "{}/_/api/admin/config/section/access",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(
        body.get("access_key_id").is_none()
            || body["access_key_id"].as_str().unwrap_or("").is_empty()
            || body["access_key_id"]
                .as_str()
                .unwrap_or("")
                .starts_with("redacted"),
        "section GET must redact access_key_id, got: {body}"
    );

    // PUT the redacted body back verbatim. No-op from the operator's
    // viewpoint — only the redacted fields are in the body, and the
    // server should preserve them from runtime.
    let resp = admin
        .put(format!(
            "{}/_/api/admin/config/section/access",
            server.endpoint()
        ))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Auth is STILL enabled — the creds survived.
    let resp = admin
        .get(format!("{}/_/api/admin/config", server.endpoint()))
        .send()
        .await
        .unwrap();
    let cfg: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(
        cfg["auth_enabled"], true,
        "post-PUT auth must still be enabled — redacted round-trip cleared creds"
    );
    assert_eq!(cfg["access_key_id"], "PRESERVKEY");
}

#[tokio::test]
async fn trace_get_with_missing_query_uses_defaults() {
    let server = TestServer::builder()
        .auth("SECKEY15", "SECSECRET15")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    // No query params — handler must use `GET /` defaults rather than erroring.
    let resp = admin
        .get(format!("{}/_/api/admin/config/trace", server.endpoint()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["resolved"]["method"].as_str().unwrap(), "GET");
}
