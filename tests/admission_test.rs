//! Integration tests for the Phase 2 admission chain.
//!
//! Two test surfaces:
//! 1. **Trace endpoint** (`POST /_/api/admin/config/trace`) — admin-API
//!    unit-test of the evaluator: does a synthetic request reach the right
//!    decision against a live chain?
//! 2. **End-to-end S3 path** — does admission produce the same 200/403
//!    outcomes the old inline public-prefix code did, across the refactor?
//!    The dedicated `public_prefix_test` suite already exercises this
//!    exhaustively; these tests add trace-vs-live parity checks so the
//!    trace endpoint never lies.

mod common;

use common::{admin_http_client, TestServer};
use reqwest::StatusCode;
use serde_json::json;

// ═══════════════════════════════════════════════════
// /config/trace — basic plumbing
// ═══════════════════════════════════════════════════

#[tokio::test]
async fn test_trace_echoes_resolved_inputs() {
    let server = TestServer::builder().auth("TRACEK", "TRACES").build().await;
    let admin = admin_http_client(&server.endpoint()).await;

    let resp = admin
        .post(format!("{}/_/api/admin/config/trace", server.endpoint()))
        .json(&json!({
            "method": "get",
            "path": "/My-Bucket/some%20key",
            "query": "prefix=releases%2F",
            "authenticated": false
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();

    // Bucket is lowercased; key is percent-decoded; prefix is percent-
    // decoded. This mirrors what the live middleware would do.
    assert_eq!(body["resolved"]["method"], "GET");
    assert_eq!(body["resolved"]["bucket"], "my-bucket");
    assert_eq!(body["resolved"]["key"], "some key");
    assert_eq!(body["resolved"]["list_prefix"], "releases/");
    assert_eq!(body["resolved"]["authenticated"], false);
}

#[tokio::test]
async fn test_trace_continue_when_no_admission_rules() {
    // Default deployment: no public_prefixes configured → admission chain
    // is empty → every request gets Continue { matched: null }.
    let server = TestServer::builder()
        .auth("TRACEK2", "TRACES2")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    let resp = admin
        .post(format!("{}/_/api/admin/config/trace", server.endpoint()))
        .json(&json!({
            "method": "GET",
            "path": "/any-bucket/any-key",
            "authenticated": false
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body["admission"]["decision"], "continue");
    assert!(body["admission"]["matched"].is_null());
}

// ═══════════════════════════════════════════════════
// Phase 2 acceptance scenarios (from the plan)
// ═══════════════════════════════════════════════════

/// Scenario 1: anonymous GET on a public-prefixed bucket → admission emits
/// AllowAnonymous, and the live S3 path serves the object without SigV4.
#[tokio::test]
async fn test_acceptance_anonymous_get_on_public_bucket_allowed() {
    let server = TestServer::builder()
        .auth("ACCEPT1K", "ACCEPT1S")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    // Configure a public prefix on `mybucket`.
    let resp = admin
        .put(format!("{}/_/api/admin/config", server.endpoint()))
        .json(&json!({
            "bucket_policies": {
                "mybucket": {
                    "public_prefixes": ["releases/"]
                }
            }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Trace confirms admission produces AllowAnonymous.
    let trace: serde_json::Value = admin
        .post(format!("{}/_/api/admin/config/trace", server.endpoint()))
        .json(&json!({
            "method": "GET",
            "path": "/mybucket/releases/v1.zip",
            "authenticated": false
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        trace["admission"]["decision"], "allow-anonymous",
        "trace should emit allow-anonymous for anonymous GET on public path"
    );
    assert_eq!(trace["admission"]["matched"], "public-prefix:mybucket");
}

/// Scenario 2: anonymous GET on a private bucket → admission emits
/// Continue, and SigV4 then rejects with 403.
#[tokio::test]
async fn test_acceptance_anonymous_get_on_private_bucket_denied() {
    let server = TestServer::builder()
        .auth("ACCEPT2K", "ACCEPT2S")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    // Trace against the default empty chain.
    let trace: serde_json::Value = admin
        .post(format!("{}/_/api/admin/config/trace", server.endpoint()))
        .json(&json!({
            "method": "GET",
            "path": "/private-bucket/secret.txt",
            "authenticated": false
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(trace["admission"]["decision"], "continue");

    // Live path: an unauthenticated GET is rejected by SigV4.
    let live = reqwest::Client::new()
        .get(format!("{}/private-bucket/secret.txt", server.endpoint()))
        .send()
        .await
        .unwrap();
    assert!(
        live.status() == StatusCode::FORBIDDEN || live.status() == StatusCode::UNAUTHORIZED,
        "expected 403/401, got {}",
        live.status()
    );
}

/// Scenario 3: authenticated PUT to a public-prefixed bucket → admission
/// emits Continue (write methods never ride a public-prefix grant). SigV4
/// verifies the signature, and the write proceeds normally.
#[tokio::test]
async fn test_acceptance_authenticated_put_on_public_bucket_goes_through_auth() {
    let server = TestServer::builder()
        .auth("ACCEPT3K", "ACCEPT3S")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    // Configure public prefix on `mybucket` (same as scenario 1).
    let resp = admin
        .put(format!("{}/_/api/admin/config", server.endpoint()))
        .json(&json!({
            "bucket_policies": {
                "mybucket": { "public_prefixes": ["releases/"] }
            }
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Trace: PUT on the public path must Continue (not AllowAnonymous).
    // This is the critical invariant — public-prefix grants are read-only.
    let trace: serde_json::Value = admin
        .post(format!("{}/_/api/admin/config/trace", server.endpoint()))
        .json(&json!({
            "method": "PUT",
            "path": "/mybucket/releases/v1.zip",
            "authenticated": true
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        trace["admission"]["decision"], "continue",
        "PUT must never ride a public-prefix grant, even authenticated"
    );
}

/// Hot-reload coverage: after a bucket policy update, the admission chain
/// must reflect the new state on the very next trace. If the rebuild site
/// drifts between `rebuild_bucket_derived_snapshots` and some forgotten
/// bucket-policies mutator, this test catches it.
#[tokio::test]
async fn test_chain_rebuilds_on_bucket_policy_hot_reload() {
    let server = TestServer::builder().auth("HOTK", "HOTS").build().await;
    let admin = admin_http_client(&server.endpoint()).await;

    // Pre-configuration: chain is empty → trace returns Continue.
    let before: serde_json::Value = admin
        .post(format!("{}/_/api/admin/config/trace", server.endpoint()))
        .json(&json!({
            "method": "GET",
            "path": "/rolling/releases/a.zip",
            "authenticated": false
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(before["admission"]["decision"], "continue");

    // Configure a public prefix via update_config.
    admin
        .put(format!("{}/_/api/admin/config", server.endpoint()))
        .json(&json!({
            "bucket_policies": {
                "rolling": { "public_prefixes": ["releases/"] }
            }
        }))
        .send()
        .await
        .unwrap();

    // Same trace: chain has rebuilt and the block fires.
    let after: serde_json::Value = admin
        .post(format!("{}/_/api/admin/config/trace", server.endpoint()))
        .json(&json!({
            "method": "GET",
            "path": "/rolling/releases/a.zip",
            "authenticated": false
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(after["admission"]["decision"], "allow-anonymous");
    assert_eq!(after["admission"]["matched"], "public-prefix:rolling");
}

/// Trace-vs-live parity: trace's bucket/key parsing must match what the
/// live middleware does for every path shape we care about. If they drift,
/// trace would lie about what the live path decides.
#[tokio::test]
async fn test_trace_parses_path_components_same_as_middleware() {
    let server = TestServer::builder().auth("PARSEK", "PARSES").build().await;
    let admin = admin_http_client(&server.endpoint()).await;

    // Configure a public prefix with a specific bucket case / encoded key
    // to exercise the normalisation edge cases.
    admin
        .put(format!("{}/_/api/admin/config", server.endpoint()))
        .json(&json!({
            "bucket_policies": {
                "parse-bucket": { "public_prefixes": ["deep/path/"] }
            }
        }))
        .send()
        .await
        .unwrap();

    // Trace with a mixed-case bucket and percent-encoded key.
    let trace: serde_json::Value = admin
        .post(format!("{}/_/api/admin/config/trace", server.endpoint()))
        .json(&json!({
            "method": "HEAD",
            "path": "/PARSE-BUCKET/deep/path/file%20name.txt",
            "authenticated": false
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(trace["resolved"]["bucket"], "parse-bucket");
    assert_eq!(trace["resolved"]["key"], "deep/path/file name.txt");
    assert_eq!(trace["admission"]["decision"], "allow-anonymous");
}

// ═══════════════════════════════════════════════════
// Phase 3b.2.b — operator-authored block dispatch
// ═══════════════════════════════════════════════════

/// Apply an admission YAML block to a running server and verify the
/// trace endpoint reports the resulting decision. Exercises the full
/// pipeline: `/apply` → spec→runtime compile → chain rebuild → trace.
///
/// The server's existing config (backend path, credentials) must be
/// preserved — the engine rebuilds on every apply, so a bare admission
/// YAML would try to spin up a default filesystem backend that doesn't
/// match the test's tempdir. We fetch the canonical export, splice the
/// operator's admission section in, and apply the merged document.
async fn apply_admission_yaml(
    admin: &reqwest::Client,
    endpoint: &str,
    admission_yaml_fragment: &str,
) {
    // Fetch the existing config as the base.
    let base_yaml: String = admin
        .get(format!("{}/_/api/admin/config/export", endpoint))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();

    // Parse the fragment for its `admission:` section, then splice.
    let fragment: serde_yaml::Value = serde_yaml::from_str(admission_yaml_fragment).unwrap();
    let frag_admission = fragment
        .get("admission")
        .cloned()
        .expect("fragment must have an `admission:` top-level key");

    let mut base: serde_yaml::Value = serde_yaml::from_str(&base_yaml)
        .unwrap_or(serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
    let root = base
        .as_mapping_mut()
        .expect("base config must be a mapping");
    root.insert("admission".into(), frag_admission);
    let merged = serde_yaml::to_string(&base).unwrap();

    let resp = admin
        .post(format!("{}/_/api/admin/config/apply", endpoint))
        .json(&json!({ "yaml": merged }))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let text = resp.text().await.unwrap();
    assert_eq!(
        status,
        StatusCode::OK,
        "apply must succeed, got status {status}, body: {text}"
    );
}

#[tokio::test]
async fn test_operator_deny_block_produces_deny_decision() {
    let server = TestServer::builder().auth("DENYK1", "DENYS1").build().await;
    let admin = admin_http_client(&server.endpoint()).await;

    apply_admission_yaml(
        &admin,
        &server.endpoint(),
        r#"
admission:
  blocks:
    - name: deny-bad-ips
      match:
        source_ip_list:
          - "203.0.113.0/24"
      action: deny
"#,
    )
    .await;

    // Trace from a blocked IP.
    let trace: serde_json::Value = admin
        .post(format!("{}/_/api/admin/config/trace", server.endpoint()))
        .json(&json!({
            "method": "GET",
            "path": "/any-bucket/any-key",
            "authenticated": false,
            "source_ip": "203.0.113.42"
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(trace["admission"]["decision"], "deny");
    assert_eq!(trace["admission"]["matched"], "deny-bad-ips");

    // Trace from an IP outside the blocked range — not denied.
    let trace: serde_json::Value = admin
        .post(format!("{}/_/api/admin/config/trace", server.endpoint()))
        .json(&json!({
            "method": "GET",
            "path": "/any-bucket/any-key",
            "authenticated": false,
            "source_ip": "198.51.100.1"
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(trace["admission"]["decision"], "continue");
}

#[tokio::test]
async fn test_operator_reject_block_produces_reject_with_status() {
    let server = TestServer::builder().auth("REJK1", "REJS1").build().await;
    let admin = admin_http_client(&server.endpoint()).await;

    apply_admission_yaml(
        &admin,
        &server.endpoint(),
        r#"
admission:
  blocks:
    - name: maint-reject
      match: {}
      action:
        type: reject
        status: 503
        message: "maintenance underway"
"#,
    )
    .await;

    let trace: serde_json::Value = admin
        .post(format!("{}/_/api/admin/config/trace", server.endpoint()))
        .json(&json!({
            "method": "GET",
            "path": "/b/k",
            "authenticated": false
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(trace["admission"]["decision"], "reject");
    assert_eq!(trace["admission"]["matched"], "maint-reject");
    assert_eq!(trace["admission"]["status"], 503);
    assert_eq!(trace["admission"]["message"], "maintenance underway");
}

#[tokio::test]
async fn test_operator_deny_short_circuits_live_s3_path() {
    // End-to-end dispatch check: an operator-authored deny block must
    // stop a live S3 request at the admission middleware — 403 with no
    // SigV4 attempt. We don't have easy ConnectInfo wiring, but
    // DGP_TRUST_PROXY_HEADERS=true makes the rate-limiter's
    // `extract_client_ip` honor X-Forwarded-For, which admission reuses.
    //
    // Because the test server won't have that env set, this test
    // exercises the path_glob + authenticated predicates instead — they
    // don't need source_ip to fire.
    let server = TestServer::builder().auth("DENYLK", "DENYLS").build().await;
    let admin = admin_http_client(&server.endpoint()).await;

    apply_admission_yaml(
        &admin,
        &server.endpoint(),
        r#"
admission:
  blocks:
    - name: block-unauth-put
      match:
        method: [PUT, POST, DELETE]
        authenticated: false
      action: deny
"#,
    )
    .await;

    // Hit an unauthenticated PUT on an arbitrary bucket/key. The
    // admission middleware should return 403 before SigV4 runs.
    let resp = reqwest::Client::new()
        .put(format!("{}/testbkt/somekey", server.endpoint()))
        .body(b"some data".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "unauthenticated PUT must be denied by admission"
    );
    // Body is the S3-style XML error shape.
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("AccessDenied"),
        "expected S3 AccessDenied XML, got: {body}"
    );
    assert!(
        body.contains("admission-deny:block-unauth-put"),
        "error message must name the matched block: {body}"
    );
}

#[tokio::test]
async fn test_operator_reject_short_circuits_live_s3_path() {
    let server = TestServer::builder().auth("REJLK", "REJLS").build().await;
    let admin = admin_http_client(&server.endpoint()).await;

    apply_admission_yaml(
        &admin,
        &server.endpoint(),
        r#"
admission:
  blocks:
    - name: maint
      match: {}
      action:
        type: reject
        status: 503
        message: "maintenance window"
"#,
    )
    .await;

    // Any request — authenticated or not — should bounce with 503.
    let resp = reqwest::Client::new()
        .get(format!("{}/any-bucket/any-key", server.endpoint()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = resp.text().await.unwrap();
    assert_eq!(body, "maintenance window");
}

#[tokio::test]
async fn test_operator_path_glob_predicate_via_trace() {
    let server = TestServer::builder().auth("PGK1", "PGS1").build().await;
    let admin = admin_http_client(&server.endpoint()).await;

    apply_admission_yaml(
        &admin,
        &server.endpoint(),
        r#"
admission:
  blocks:
    - name: allow-public-zips
      match:
        method: [GET, HEAD]
        bucket: releases
        path_glob: "*.zip"
      action: allow-anonymous
"#,
    )
    .await;

    // Matches the glob → allow-anonymous.
    let trace: serde_json::Value = admin
        .post(format!("{}/_/api/admin/config/trace", server.endpoint()))
        .json(&json!({
            "method": "GET",
            "path": "/releases/v1.zip",
            "authenticated": false
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(trace["admission"]["decision"], "allow-anonymous");

    // Doesn't match the glob → continue.
    let trace: serde_json::Value = admin
        .post(format!("{}/_/api/admin/config/trace", server.endpoint()))
        .json(&json!({
            "method": "GET",
            "path": "/releases/v1.tar.gz",
            "authenticated": false
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(trace["admission"]["decision"], "continue");
}

#[tokio::test]
async fn test_admission_blocks_roundtrip_through_export_apply() {
    // GitOps dogfood for Phase 3b.2.b: apply a block, export the
    // canonical YAML, re-apply it, trace once more — behavior
    // unchanged on the second apply.
    let server = TestServer::builder().auth("RTK1", "RTS1").build().await;
    let admin = admin_http_client(&server.endpoint()).await;

    let original_yaml = r#"
admission:
  blocks:
    - name: deny-bad
      match:
        source_ip_list: ["203.0.113.5"]
      action: deny
"#;

    apply_admission_yaml(&admin, &server.endpoint(), original_yaml).await;

    // Export and verify the block survived.
    let exported: String = admin
        .get(format!("{}/_/api/admin/config/export", server.endpoint()))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        exported.contains("deny-bad"),
        "exported YAML must contain the operator block: {exported}"
    );
    assert!(
        exported.contains("203.0.113.5"),
        "exported YAML must preserve the IP: {exported}"
    );

    // Re-apply the full export verbatim — must succeed (idempotent).
    // Note: we POST `exported` directly here (not through the
    // apply_admission_yaml splicer) because the export is already the
    // full, merged config.
    let resp = admin
        .post(format!("{}/_/api/admin/config/apply", server.endpoint()))
        .json(&json!({ "yaml": exported }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Trace: decision still the same.
    let trace: serde_json::Value = admin
        .post(format!("{}/_/api/admin/config/trace", server.endpoint()))
        .json(&json!({
            "method": "GET",
            "path": "/any/any",
            "authenticated": false,
            "source_ip": "203.0.113.5"
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(trace["admission"]["decision"], "deny");
    assert_eq!(trace["admission"]["matched"], "deny-bad");
}
