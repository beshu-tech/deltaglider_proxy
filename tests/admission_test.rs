// SPDX-License-Identifier: BUSL-1.1

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

use crate::common;

use common::{admin_http_client, metrics_text, prometheus_counter_has_labels, TestServer};
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
    // The XML body escapes quotes (&apos;), so match the two fragments separately.
    assert!(
        body.contains("Blocked by access rule") && body.contains("block-unauth-put"),
        "error message must name the matched block: {body}"
    );

    let metrics = metrics_text(&server.endpoint()).await;
    assert!(
        prometheus_counter_has_labels(
            &metrics,
            "deltaglider_http_requests_total",
            &[
                "method=\"PUT\"",
                "status=\"403\"",
                "operation=\"put_object\""
            ],
        ),
        "admission short-circuit denial should increment sanitized PUT/403/put_object metrics"
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
    // An S3 XML error, so an S3 SDK reads the operator's message (a
    // plain-text body failed its XML parse and lost the message).
    assert!(resp.headers().contains_key("x-amz-request-id"));
    let body = resp.text().await.unwrap();
    assert!(body.contains("<Code>SlowDown</Code>"), "{body}");
    assert!(
        body.contains("<Message>maintenance window</Message>"),
        "{body}"
    );
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

/// M4 from deep correctness review (sub-case b): bucket `public: true`
/// AND an operator-authored admission block referencing that same
/// bucket. Operator block should fire BEFORE the synthesised
/// public-prefix block; both should coexist in the chain without
/// name collision (the operator block cannot be named
/// `public-prefix:...` — H3 guards that).
#[tokio::test]
async fn test_operator_block_coexists_with_bucket_public_shorthand() {
    let server = TestServer::builder().auth("COEXK", "COEXS").build().await;
    let admin = admin_http_client(&server.endpoint()).await;

    // Fetch current config and merge: add a public bucket AND an
    // operator deny block for a subset of its IPs.
    let exported: String = admin
        .get(format!("{}/_/api/admin/config/export", server.endpoint()))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let mut doc: serde_yaml::Value = serde_yaml::from_str(&exported)
        .unwrap_or(serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
    let root = doc.as_mapping_mut().unwrap();

    // storage.buckets.docs = { public: true }
    let storage = root
        .entry(serde_yaml::Value::String("storage".into()))
        .or_insert_with(|| serde_yaml::Value::Mapping(serde_yaml::Mapping::new()))
        .as_mapping_mut()
        .unwrap();
    let buckets = storage
        .entry(serde_yaml::Value::String("buckets".into()))
        .or_insert_with(|| serde_yaml::Value::Mapping(serde_yaml::Mapping::new()))
        .as_mapping_mut()
        .unwrap();
    let mut bucket_policy = serde_yaml::Mapping::new();
    bucket_policy.insert("public".into(), true.into());
    buckets.insert("docs".into(), serde_yaml::Value::Mapping(bucket_policy));

    // admission.blocks[0] = deny range 203.0.113.0/24 targeting `docs`
    let admission_yaml = serde_yaml::from_str::<serde_yaml::Value>(
        r#"
blocks:
  - name: deny-bad-ips-from-docs
    match:
      bucket: docs
      source_ip_list: ["203.0.113.0/24"]
    action: deny
"#,
    )
    .unwrap();
    root.insert("admission".into(), admission_yaml);

    let merged = serde_yaml::to_string(&doc).unwrap();
    let resp = admin
        .post(format!("{}/_/api/admin/config/apply", server.endpoint()))
        .json(&json!({ "yaml": merged }))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body = resp.text().await.unwrap();
    assert_eq!(status, StatusCode::OK, "apply failed: {body}");

    // Blocked IP: operator deny wins.
    let trace: serde_json::Value = admin
        .post(format!("{}/_/api/admin/config/trace", server.endpoint()))
        .json(&json!({
            "method": "GET",
            "path": "/docs/readme.md",
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
    assert_eq!(trace["admission"]["matched"], "deny-bad-ips-from-docs");

    // Unblocked IP: synthesised public-prefix block admits.
    let trace: serde_json::Value = admin
        .post(format!("{}/_/api/admin/config/trace", server.endpoint()))
        .json(&json!({
            "method": "GET",
            "path": "/docs/readme.md",
            "authenticated": false,
            "source_ip": "198.51.100.7"
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(trace["admission"]["decision"], "allow-anonymous");
    assert_eq!(trace["admission"]["matched"], "public-prefix:docs");
}

/// M4 from deep correctness review (sub-case c): a mixed-shape YAML
/// (flat root key `listen_addr:` alongside sectioned `storage:`)
/// POSTed to `/apply` should produce a 400 with a named classifier
/// error — not a silent accept.
#[tokio::test]
async fn test_apply_rejects_mixed_flat_plus_sectioned_yaml() {
    let server = TestServer::builder().auth("MIXEDK", "MIXEDS").build().await;
    let admin = admin_http_client(&server.endpoint()).await;

    let mixed = r#"
listen_addr: "127.0.0.1:9000"
storage:
  filesystem: /var/dgp
"#;
    let resp = admin
        .post(format!("{}/_/api/admin/config/apply", server.endpoint()))
        .json(&json!({ "yaml": mixed }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "mixed flat+sectioned YAML must be rejected via /apply"
    );
    let body: serde_json::Value = resp.json().await.unwrap();
    let err = body["error"].as_str().unwrap_or("");
    assert!(
        err.contains("listen_addr") && err.contains("storage"),
        "error must name both conflicting keys, got: {err}"
    );
}

// ── review second pass (failing tests for findings) ──────────────────────

/// Review-2 (ee7bbb98): admission uses the resolved client IP. A client
/// that is not a trusted proxy writes its own `X-Forwarded-For`, so one
/// forged header must not escape a `source_ip` deny block.
#[tokio::test]
async fn review2_source_ip_deny_not_bypassed_by_forged_xff() {
    let bucket = "xffdeny";
    let server = TestServer::builder()
        .bucket(bucket)
        .auth("XFFK", "XFFS")
        // The loopback client is no trusted proxy here.
        .env("DGP_TRUSTED_PROXY_CIDRS", "192.0.2.1/32")
        .bucket_policy(bucket, "public: true")
        .build()
        .await;
    server
        .s3_client()
        .await
        .put_object()
        .bucket(bucket)
        .key("k.txt")
        .body(aws_sdk_s3::primitives::ByteStream::from_static(b"x"))
        .send()
        .await
        .unwrap();
    let admin = admin_http_client(&server.endpoint()).await;
    apply_admission_yaml(
        &admin,
        &server.endpoint(),
        r#"
admission:
  blocks:
    - name: deny-loopback
      match:
        source_ip_list: ["127.0.0.1/32"]
      action: deny
"#,
    )
    .await;
    let url = format!("{}/{}/k.txt", server.endpoint(), bucket);
    let http = reqwest::Client::new();
    assert_eq!(
        http.get(&url).send().await.unwrap().status(),
        StatusCode::FORBIDDEN,
        "precondition: the deny block matches the loopback client"
    );
    let spoofed = http
        .get(&url)
        .header("x-forwarded-for", "198.51.100.1")
        .send()
        .await
        .unwrap();
    assert_eq!(
        spoofed.status(),
        StatusCode::FORBIDDEN,
        "a forged X-Forwarded-For escaped a source_ip deny block"
    );
}

/// A browser form POST names its bucket in the path. For admission (and for
/// s3s) `//bucket` has an EMPTY bucket, so a block on `bucket` does not fire.
/// The router must then not serve it as an upload into `bucket`: that upload
/// skipped every admission block on the bucket. Found by the
/// middleware-vs-s3s contract test (`api::s3s_contract_tests`).
#[tokio::test]
async fn form_post_to_an_empty_leading_segment_does_not_skip_admission() {
    let server = TestServer::builder().open_access().build().await;
    let admin = admin_http_client(&server.endpoint()).await;
    let bucket = server.bucket().to_string();
    apply_admission_yaml(
        &admin,
        &server.endpoint(),
        &format!(
            r#"
admission:
  blocks:
    - name: freeze-uploads
      match:
        bucket: {bucket}
        method: [POST]
      action: deny
"#
        ),
    )
    .await;
    let form = |key: &str| {
        reqwest::multipart::Form::new()
            .text("key", key.to_string())
            .part(
                "file",
                reqwest::multipart::Part::bytes(b"payload".to_vec())
                    .file_name("f.txt")
                    .mime_str("text/plain")
                    .unwrap(),
            )
    };
    let http = reqwest::Client::new();

    // Control: the plain form POST meets the block.
    let resp = http
        .post(format!("{}/{bucket}", server.endpoint()))
        .multipart(form("plain.txt"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let resp = http
        .post(format!("{}//{bucket}", server.endpoint()))
        .multipart(form("escaped.txt"))
        .send()
        .await
        .unwrap();
    assert!(
        !resp.status().is_success(),
        "a form POST to //{bucket} must not be served, got {}",
        resp.status()
    );
    let resp = http
        .get(format!("{}/{bucket}/escaped.txt", server.endpoint()))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NOT_FOUND,
        "the form POST to //{bucket} stored an object past the admission block"
    );
}

async fn trace_anon(
    admin: &reqwest::Client,
    endpoint: &str,
    method: &str,
    path: &str,
    query: &str,
) -> serde_json::Value {
    admin
        .post(format!("{endpoint}/_/api/admin/config/trace"))
        .json(&json!({ "method": method, "path": path, "query": query, "authenticated": false }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

/// Explore #8: an operator `allow-anonymous` block on a bucket without
/// public prefixes admitted the request, then authorization refused it
/// (403) while the trace said `allow-anonymous`. Now the decision grants
/// exactly that read-class request; a write is never granted; the trace
/// reports the same grant.
#[tokio::test]
async fn test_operator_allow_anonymous_grants_the_matched_read() {
    let server = TestServer::builder()
        .auth("ANONOPK", "ANONOPS")
        .bucket("releases")
        .build()
        .await;
    let endpoint = server.endpoint();
    let http = server.http();
    for key in ["builds/app.zip", "builds/app.tar.gz"] {
        common::put_object(
            &http,
            &endpoint,
            "releases",
            key,
            b"payload".to_vec(),
            "application/octet-stream",
        )
        .await;
    }
    let admin = admin_http_client(&endpoint).await;
    apply_admission_yaml(
        &admin,
        &endpoint,
        r#"
admission:
  blocks:
    - name: allow-public-zips
      match:
        method: [GET, HEAD]
        bucket: releases
        path_glob: "*.zip"
      action: allow-anonymous
    - name: allow-list-builds
      match:
        method: [GET]
        bucket: releases
        path_glob: "builds/"
      action: allow-anonymous
    - name: never-a-write
      match:
        method: [PUT, DELETE]
        bucket: releases
      action: allow-anonymous
"#,
    )
    .await;

    let anon = reqwest::Client::new();
    // The matched read goes through, as the trace says.
    let trace = trace_anon(&admin, &endpoint, "GET", "/releases/builds/app.zip", "").await;
    assert_eq!(trace["admission"]["decision"], "allow-anonymous");
    assert_eq!(trace["anonymous_grant"]["action"], "read", "{trace}");
    let r = anon
        .get(format!("{endpoint}/releases/builds/app.zip"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.bytes().await.unwrap().as_ref(), b"payload");
    let r = anon
        .head(format!("{endpoint}/releases/builds/app.zip"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);

    // A key the block does not match stays private.
    let r = anon
        .get(format!("{endpoint}/releases/builds/app.tar.gz"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);

    // The matched LIST: exactly that prefix.
    let trace = trace_anon(
        &admin,
        &endpoint,
        "GET",
        "/releases",
        "list-type=2&prefix=builds/",
    )
    .await;
    assert_eq!(trace["anonymous_grant"]["action"], "list", "{trace}");
    let r = anon
        .get(format!("{endpoint}/releases?list-type=2&prefix=builds/"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let body = r.text().await.unwrap();
    // The LIST shows only the keys a block admits: `*.zip` admits app.zip,
    // no block admits app.tar.gz.
    assert!(
        body.contains("builds/app.zip") && !body.contains("builds/app.tar.gz"),
        "{body}"
    );

    // A write is never granted, even when an allow-anonymous block matches.
    let trace = trace_anon(&admin, &endpoint, "PUT", "/releases/builds/evil.zip", "").await;
    assert_eq!(trace["admission"]["decision"], "allow-anonymous");
    assert!(trace["anonymous_grant"].is_null(), "{trace}");
    let r = anon
        .put(format!("{endpoint}/releases/builds/evil.zip"))
        .body("x")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    let r = anon
        .delete(format!("{endpoint}/releases/builds/app.zip"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
}

// ═══════════════════════════════════════════════════
// A `path_glob` decides keys: every resource a request touches
// ═══════════════════════════════════════════════════

fn status_of<T, E: aws_sdk_s3::error::ProvideErrorMetadata>(
    r: Result<T, aws_sdk_s3::error::SdkError<E, aws_sdk_s3::config::http::HttpResponse>>,
) -> u16 {
    match r {
        Ok(_) => 200,
        Err(e) => e.raw_response().map(|r| r.status().as_u16()).unwrap_or(0),
    }
}

async fn put_keys(s3: &aws_sdk_s3::Client, bucket: &str, keys: &[&str]) {
    for key in keys {
        s3.put_object()
            .bucket(bucket)
            .key(*key)
            .body(aws_sdk_s3::primitives::ByteStream::from_static(b"payload"))
            .send()
            .await
            .unwrap();
    }
}

async fn listed(
    s3: &aws_sdk_s3::Client,
    bucket: &str,
    prefix: &str,
    delimiter: Option<&str>,
) -> (Vec<String>, Vec<String>) {
    let mut req = s3.list_objects_v2().bucket(bucket).prefix(prefix);
    if let Some(d) = delimiter {
        req = req.delimiter(d);
    }
    let out = req.send().await.unwrap();
    (
        out.contents()
            .iter()
            .map(|o| o.key().unwrap().to_string())
            .collect(),
        out.common_prefixes()
            .iter()
            .map(|p| p.prefix().unwrap().to_string())
            .collect(),
    )
}

/// An operator `deny` + `path_glob` refuses the key on the request line AND
/// the copy source, each DeleteObjects key, the UploadPartCopy source, and
/// every key and folder that a LIST returns.
#[tokio::test]
async fn a_deny_with_a_glob_covers_every_resource_of_a_request() {
    let server = TestServer::builder().auth("RRK1", "RRS1").build().await;
    let endpoint = server.endpoint();
    let bucket = server.bucket().to_string();
    let s3 = server.s3_client().await;
    put_keys(
        &s3,
        &bucket,
        &[
            "secret/s1.txt",
            "secret/s2.txt",
            "team/ok.txt",
            "team/bye.txt",
        ],
    )
    .await;
    let admin = admin_http_client(&endpoint).await;
    apply_admission_yaml(
        &admin,
        &endpoint,
        &format!(
            r#"
admission:
  blocks:
    - name: no-secret
      match:
        bucket: {bucket}
        path_glob: "secret/**"
      action: deny
"#
        ),
    )
    .await;

    // Control: the request line.
    assert_eq!(
        status_of(
            s3.get_object()
                .bucket(&bucket)
                .key("secret/s1.txt")
                .send()
                .await
        ),
        403
    );

    // The trace says the chain also decides per resource, and that a LIST narrows.
    let trace = trace_anon(
        &admin,
        &endpoint,
        "GET",
        &format!("/{bucket}"),
        "list-type=2",
    )
    .await;
    assert_eq!(trace["per_resource_rules"], true, "{trace}");
    assert_eq!(trace["listing_filtered"], true, "{trace}");
    let trace = trace_anon(
        &admin,
        &endpoint,
        "GET",
        &format!("/{bucket}/team/ok.txt"),
        "",
    )
    .await;
    assert_eq!(trace["listing_filtered"], false, "{trace}");

    // CopyObject source.
    let copy = s3
        .copy_object()
        .bucket(&bucket)
        .key("team/copied.txt")
        .copy_source(format!("{bucket}/secret/s1.txt"))
        .send()
        .await;
    assert_eq!(status_of(copy), 403, "CopyObject source");
    assert_eq!(
        status_of(
            s3.get_object()
                .bucket(&bucket)
                .key("team/copied.txt")
                .send()
                .await
        ),
        404,
        "nothing was copied"
    );
    // An allowed source still copies.
    let copy = s3
        .copy_object()
        .bucket(&bucket)
        .key("team/copied-ok.txt")
        .copy_source(format!("{bucket}/team/ok.txt"))
        .send()
        .await;
    assert_eq!(status_of(copy), 200);

    // UploadPartCopy source.
    let upload = s3
        .create_multipart_upload()
        .bucket(&bucket)
        .key("team/mpu.bin")
        .send()
        .await
        .unwrap();
    let part = s3
        .upload_part_copy()
        .bucket(&bucket)
        .key("team/mpu.bin")
        .upload_id(upload.upload_id().unwrap())
        .part_number(1)
        .copy_source(format!("{bucket}/secret/s1.txt"))
        .send()
        .await;
    assert_eq!(status_of(part), 403, "UploadPartCopy source");

    // DeleteObjects: the denied key is an error, the others go.
    let ids: Vec<_> = ["secret/s1.txt", "team/bye.txt"]
        .into_iter()
        .map(|k| {
            aws_sdk_s3::types::ObjectIdentifier::builder()
                .key(k)
                .build()
                .unwrap()
        })
        .collect();
    let out = s3
        .delete_objects()
        .bucket(&bucket)
        .delete(
            aws_sdk_s3::types::Delete::builder()
                .set_objects(Some(ids))
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap();
    let errors: Vec<_> = out
        .errors()
        .iter()
        .map(|e| (e.key().unwrap(), e.code().unwrap()))
        .collect();
    assert_eq!(errors, vec![("secret/s1.txt", "AccessDenied")], "{out:?}");
    let deleted: Vec<_> = out.deleted().iter().map(|d| d.key().unwrap()).collect();
    assert_eq!(deleted, vec!["team/bye.txt"], "{out:?}");

    // LIST: a prefix without a slash, no prefix, a delimiter.
    let (keys, _) = listed(&s3, &bucket, "secret", None).await;
    assert!(keys.is_empty(), "{keys:?}");
    let (keys, _) = listed(&s3, &bucket, "", None).await;
    assert!(keys.iter().all(|k| !k.starts_with("secret/")), "{keys:?}");
    assert!(keys.contains(&"team/ok.txt".to_string()), "{keys:?}");
    let (keys, prefixes) = listed(&s3, &bucket, "", Some("/")).await;
    assert!(
        keys.is_empty() && prefixes == vec!["team/".to_string()],
        "{keys:?} {prefixes:?}"
    );
    // A delimiter that cuts inside the folder name: `sec` would name `secret/`.
    let (_, prefixes) = listed(&s3, &bucket, "", Some("c")).await;
    assert!(
        prefixes.iter().all(|p| !p.starts_with("sec")),
        "{prefixes:?}"
    );

    // Lifting the rule lifts everything: nothing was lost.
    apply_admission_yaml(&admin, &endpoint, "admission:\n  blocks: []\n").await;
    assert_eq!(
        status_of(
            s3.get_object()
                .bucket(&bucket)
                .key("secret/s1.txt")
                .send()
                .await
        ),
        200
    );
    assert_eq!(
        status_of(
            s3.get_object()
                .bucket(&bucket)
                .key("secret/s2.txt")
                .send()
                .await
        ),
        200
    );
}

/// The deny also meets the key of a browser form POST, which the request
/// line (`POST /bucket`) does not name.
#[tokio::test]
async fn a_deny_with_a_glob_covers_the_form_post_key() {
    let server = TestServer::builder().open_access().build().await;
    let admin = admin_http_client(&server.endpoint()).await;
    let bucket = server.bucket().to_string();
    apply_admission_yaml(
        &admin,
        &server.endpoint(),
        &format!(
            r#"
admission:
  blocks:
    - name: no-secret
      match:
        bucket: {bucket}
        path_glob: "secret/**"
      action: deny
"#
        ),
    )
    .await;
    let form = |key: &str| {
        reqwest::multipart::Form::new()
            .text("key", key.to_string())
            .part(
                "file",
                reqwest::multipart::Part::bytes(b"payload".to_vec())
                    .file_name("f.txt")
                    .mime_str("text/plain")
                    .unwrap(),
            )
    };
    let http = reqwest::Client::new();
    let denied = http
        .post(format!("{}/{bucket}", server.endpoint()))
        .multipart(form("secret/x.txt"))
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    let ok = http
        .post(format!("{}/{bucket}", server.endpoint()))
        .multipart(form("team/x.txt"))
        .send()
        .await
        .unwrap();
    assert!(ok.status().is_success(), "{}", ok.status());
    let s3 = server.s3_client().await;
    apply_admission_yaml(&admin, &server.endpoint(), "admission:\n  blocks: []\n").await;
    let head = s3
        .head_object()
        .bucket(&bucket)
        .key("secret/x.txt")
        .send()
        .await;
    assert_eq!(status_of(head), 404, "the denied form POST stored nothing");
}

/// With `allow-anonymous` + `path_glob: "*.zip"` an anonymous LIST shows
/// the keys that glob matches, and the folders that hold one.
#[tokio::test]
async fn an_anonymous_list_shows_only_the_keys_the_glob_matches() {
    let server = TestServer::builder().auth("RRK2", "RRS2").build().await;
    let endpoint = server.endpoint();
    let bucket = server.bucket().to_string();
    let s3 = server.s3_client().await;
    put_keys(
        &s3,
        &bucket,
        &[
            "app.zip",
            "app.zip.bak",
            "release.zip.d/internal-roadmap.txt",
            "d/x.zip",
            "notes/a.txt",
        ],
    )
    .await;
    let admin = admin_http_client(&endpoint).await;
    apply_admission_yaml(
        &admin,
        &endpoint,
        &format!(
            r#"
admission:
  blocks:
    - name: public-zips
      match:
        method: [GET, HEAD]
        bucket: {bucket}
        path_glob: "*.zip"
      action: allow-anonymous
"#
        ),
    )
    .await;
    let anon = reqwest::Client::new();
    let list = |query: &str| {
        let url = format!("{endpoint}/{bucket}?list-type=2&{query}");
        let anon = anon.clone();
        async move { anon.get(url).send().await.unwrap() }
    };
    let r = list("prefix=app.zip").await;
    assert_eq!(r.status(), StatusCode::OK);
    let body = r.text().await.unwrap();
    assert!(body.contains("<Key>app.zip</Key>"), "{body}");
    assert!(!body.contains("app.zip.bak"), "{body}");

    let r = list("prefix=release.zip").await;
    assert_eq!(r.status(), StatusCode::OK);
    let body = r.text().await.unwrap();
    assert!(!body.contains("internal-roadmap"), "{body}");

    let r = list("delimiter=%2F").await;
    assert_eq!(r.status(), StatusCode::OK);
    let body = r.text().await.unwrap();
    assert!(body.contains("<Key>app.zip</Key>"), "{body}");
    assert!(
        body.contains("<Prefix>d/</Prefix>"),
        "the folder holds d/x.zip: {body}"
    );
    assert!(!body.contains("release.zip.d/"), "{body}");
    assert!(!body.contains("notes/"), "{body}");
    assert!(!body.contains("app.zip.bak"), "{body}");

    // A prefix that no matching key can start with: no grant.
    let r = list("prefix=notes%2F").await;
    assert_eq!(r.status(), StatusCode::OK);
    assert!(!r.text().await.unwrap().contains("a.txt"));

    // GET of a key the glob does not match stays refused.
    let r = anon
        .get(format!("{endpoint}/{bucket}/app.zip.bak"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    let r = anon
        .get(format!("{endpoint}/{bucket}/app.zip"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
}

// ═══════════════════════════════════════════════════
// `method` is the EFFECT on the resource
// ═══════════════════════════════════════════════════

/// A `method` in a block that decides per resource names what the request does
/// to that resource: a copy source, a listed key and a download entry are
/// read (GET; a block that names HEAD matches a read too), a DeleteObjects key
/// is deleted (DELETE), a form POST key is written (PUT).
#[tokio::test]
async fn method_in_a_per_resource_rule_is_the_effect_on_the_resource() {
    let server = TestServer::builder().auth("EFK1", "EFS1").build().await;
    let endpoint = server.endpoint();
    let bucket = server.bucket().to_string();
    let s3 = server.s3_client().await;
    put_keys(
        &s3,
        &bucket,
        &[
            "secret/s1.txt",
            "secret/s2.txt",
            "team/ok.txt",
            "team/bye.txt",
        ],
    )
    .await;
    let admin = admin_http_client(&endpoint).await;
    let rule = |methods: &str| {
        format!(
            r#"
admission:
  blocks:
    - name: no-secret
      match:
        bucket: {bucket}
        path_glob: "secret/**"
        method: [{methods}]
      action: deny
"#
        )
    };
    let copy = |source: &str, dest: &str| {
        s3.copy_object()
            .bucket(&bucket)
            .key(dest.to_string())
            .copy_source(format!("{bucket}/{source}"))
            .send()
    };
    let part_copy = |source: &str| {
        let s3 = s3.clone();
        let bucket = bucket.clone();
        let source = source.to_string();
        async move {
            let upload = s3
                .create_multipart_upload()
                .bucket(&bucket)
                .key("team/mpu.bin")
                .send()
                .await
                .unwrap();
            let part = s3
                .upload_part_copy()
                .bucket(&bucket)
                .key("team/mpu.bin")
                .upload_id(upload.upload_id().unwrap())
                .part_number(1)
                .copy_source(format!("{bucket}/{source}"))
                .send()
                .await;
            s3.abort_multipart_upload()
                .bucket(&bucket)
                .key("team/mpu.bin")
                .upload_id(upload.upload_id().unwrap())
                .send()
                .await
                .ok();
            status_of(part)
        }
    };
    let delete_keys = |keys: &[&str]| {
        let s3 = s3.clone();
        let bucket = bucket.clone();
        let ids: Vec<_> = keys
            .iter()
            .map(|k| {
                aws_sdk_s3::types::ObjectIdentifier::builder()
                    .key(*k)
                    .build()
                    .unwrap()
            })
            .collect();
        async move {
            let out = s3
                .delete_objects()
                .bucket(&bucket)
                .delete(
                    aws_sdk_s3::types::Delete::builder()
                        .set_objects(Some(ids))
                        .build()
                        .unwrap(),
                )
                .send()
                .await
                .unwrap();
            out.errors()
                .iter()
                .map(|e| e.key().unwrap().to_string())
                .collect::<Vec<_>>()
        }
    };

    // GET-only deny ("nobody reads secret/"): a copy source and a part-copy
    // source are reads, and so are the listed keys.
    apply_admission_yaml(&admin, &endpoint, &rule("GET")).await;
    assert_eq!(
        status_of(
            s3.get_object()
                .bucket(&bucket)
                .key("secret/s1.txt")
                .send()
                .await
        ),
        403,
        "request line"
    );
    assert_eq!(
        status_of(copy("secret/s1.txt", "team/c1.txt").await),
        403,
        "CopyObject source"
    );
    assert_eq!(
        part_copy("secret/s1.txt").await,
        403,
        "UploadPartCopy source"
    );
    let (keys, _) = listed(&s3, &bucket, "", None).await;
    assert!(keys.iter().all(|k| !k.starts_with("secret/")), "{keys:?}");
    // Controls: a read of an allowed key, a write and a delete of a secret key.
    assert_eq!(status_of(copy("team/ok.txt", "team/c2.txt").await), 200);
    assert_eq!(
        delete_keys(&["secret/s2.txt"]).await,
        Vec::<String>::new(),
        "DELETE is not named"
    );
    put_keys(&s3, &bucket, &["secret/s2.txt"]).await;

    // HEAD-only deny: a block that names HEAD matches a read of a copy source.
    apply_admission_yaml(&admin, &endpoint, &rule("HEAD")).await;
    assert_eq!(
        status_of(copy("secret/s1.txt", "team/c3.txt").await),
        403,
        "HEAD matches a read"
    );
    assert_eq!(
        status_of(
            s3.get_object()
                .bucket(&bucket)
                .key("secret/s1.txt")
                .send()
                .await
        ),
        403,
        "GET and HEAD are one read for a rule with a glob"
    );

    // DELETE-only deny: a DeleteObjects key is a delete.
    apply_admission_yaml(&admin, &endpoint, &rule("DELETE")).await;
    assert_eq!(
        delete_keys(&["secret/s1.txt", "team/bye.txt"]).await,
        vec!["secret/s1.txt"]
    );
    assert_eq!(
        status_of(copy("secret/s1.txt", "team/c4.txt").await),
        200,
        "a read is not a delete"
    );

    // PUT-only deny: the destination of a copy is a write; a read of the source is not.
    apply_admission_yaml(&admin, &endpoint, &rule("PUT")).await;
    assert_eq!(
        status_of(copy("team/ok.txt", "secret/new.txt").await),
        403,
        "copy destination"
    );
    assert_eq!(
        status_of(copy("secret/s1.txt", "team/c5.txt").await),
        200,
        "source is a read"
    );
}

/// A `method: [PUT]` deny also refuses the key of a browser form POST (a write).
#[tokio::test]
async fn a_put_only_deny_covers_the_form_post_key() {
    let server = TestServer::builder().open_access().build().await;
    let admin = admin_http_client(&server.endpoint()).await;
    let bucket = server.bucket().to_string();
    let rule = |methods: &str| {
        format!(
            r#"
admission:
  blocks:
    - name: no-secret
      match:
        bucket: {bucket}
        path_glob: "secret/**"
        method: [{methods}]
      action: deny
"#
        )
    };
    let form = |key: &str| {
        reqwest::multipart::Form::new()
            .text("key", key.to_string())
            .part(
                "file",
                reqwest::multipart::Part::bytes(b"payload".to_vec())
                    .file_name("f.txt")
                    .mime_str("text/plain")
                    .unwrap(),
            )
    };
    let http = reqwest::Client::new();
    let post = |key: &'static str| {
        let http = http.clone();
        let url = format!("{}/{bucket}", server.endpoint());
        let form = form(key);
        async move {
            http.post(url)
                .multipart(form)
                .send()
                .await
                .unwrap()
                .status()
        }
    };
    apply_admission_yaml(&admin, &server.endpoint(), &rule("PUT")).await;
    assert_eq!(post("secret/x.txt").await, StatusCode::FORBIDDEN);
    assert!(post("team/x.txt").await.is_success(), "control");
    // A GET-only deny does not name a write.
    apply_admission_yaml(&admin, &server.endpoint(), &rule("GET")).await;
    assert!(
        post("secret/y.txt").await.is_success(),
        "a GET-only deny does not cover a write"
    );
}

// ═══════════════════════════════════════════════════
// Keyless requests meet the blocks that cover every key below their prefix
// ═══════════════════════════════════════════════════

/// A maintenance `reject` on `db/**` answers a LIST of `db/` with its status,
/// not with an empty list (a `sync --delete` client would delete its copies).
#[tokio::test]
async fn a_reject_on_a_folder_answers_a_list_of_that_folder() {
    common::expect_server_errors("the maintenance reject answers 503 by design");
    let server = TestServer::builder().auth("KLK1", "KLS1").build().await;
    let endpoint = server.endpoint();
    let bucket = server.bucket().to_string();
    let s3 = server.s3_client().await;
    put_keys(&s3, &bucket, &["db/a.sql", "db/sub/b.sql", "docs/c.txt"]).await;
    let admin = admin_http_client(&endpoint).await;
    apply_admission_yaml(
        &admin,
        &endpoint,
        &format!(
            r#"
admission:
  blocks:
    - name: maintenance
      match:
        bucket: {bucket}
        path_glob: "db/**"
      action:
        type: reject
        status: 503
        message: back soon
"#
        ),
    )
    .await;
    let list = |prefix: &str| s3.list_objects_v2().bucket(&bucket).prefix(prefix).send();
    for prefix in ["db/", "db/sub/", "db/a"] {
        assert_eq!(status_of(list(prefix).await), 503, "LIST prefix={prefix}");
    }
    // Controls: a prefix outside. A LIST that reaches the folder only
    // partly still shows its keys: only a `deny` hides a key from a listing,
    // so a `sync --delete` client never deletes its copies of them.
    assert_eq!(status_of(list("docs/").await), 200);
    let want: Vec<String> = ["db/a.sql", "db/sub/b.sql", "docs/c.txt"]
        .map(String::from)
        .to_vec();
    let (keys, _) = listed(&s3, &bucket, "", None).await;
    assert_eq!(keys, want, "root LIST under a reject: {keys:?}");
    let (keys, folders) = listed(&s3, &bucket, "", Some("/")).await;
    assert!(keys.is_empty());
    assert_eq!(folders, vec!["db/".to_string(), "docs/".to_string()]);
    // The refused keys answer the rule's status for GET, copy source and delete.
    let get = s3.get_object().bucket(&bucket).key("db/a.sql").send().await;
    assert_eq!(status_of(get), 503);
    let copy = s3
        .copy_object()
        .bucket(&bucket)
        .key("docs/copy.txt")
        .copy_source(format!("{bucket}/db/a.sql"))
        .send()
        .await;
    assert_eq!(status_of(copy), 503);
    let del = s3
        .delete_object()
        .bucket(&bucket)
        .key("db/a.sql")
        .send()
        .await;
    assert_eq!(status_of(del), 503);
    // ListMultipartUploads shows the upload of a key that only a reject refuses.
    let mp = s3
        .create_multipart_upload()
        .bucket(&bucket)
        .key("docs/mp.bin")
        .send()
        .await
        .unwrap();
    let _ = mp;
    let uploads = s3
        .list_multipart_uploads()
        .bucket(&bucket)
        .send()
        .await
        .unwrap();
    assert_eq!(uploads.uploads().len(), 1);
}

/// Only a `deny` hides a key or a folder from a listing; the same glob as a
/// `deny` hides the db/ keys from the root LIST.
#[tokio::test]
async fn only_a_deny_hides_keys_from_a_listing() {
    let server = TestServer::builder().auth("KLK4", "KLS4").build().await;
    let endpoint = server.endpoint();
    let bucket = server.bucket().to_string();
    let s3 = server.s3_client().await;
    put_keys(&s3, &bucket, &["db/a.sql", "db/sub/b.sql", "docs/c.txt"]).await;
    let admin = admin_http_client(&endpoint).await;
    apply_admission_yaml(
        &admin,
        &endpoint,
        &format!(
            r#"
admission:
  blocks:
    - name: no-db
      match:
        bucket: {bucket}
        path_glob: "db/**"
      action: deny
"#
        ),
    )
    .await;
    let (keys, _) = listed(&s3, &bucket, "", None).await;
    assert_eq!(keys, vec!["docs/c.txt".to_string()], "{keys:?}");
    let (_, folders) = listed(&s3, &bucket, "", Some("/")).await;
    assert_eq!(folders, vec!["docs/".to_string()]);
}

/// A `continue` on `public/**` ends the walk for a LIST of `public/`, so the
/// catch-all `deny` below it does not fire; any other LIST meets the deny.
#[tokio::test]
async fn a_continue_on_a_folder_ends_the_walk_of_a_list_of_that_folder() {
    let server = TestServer::builder().auth("KLK2", "KLS2").build().await;
    let endpoint = server.endpoint();
    let bucket = server.bucket().to_string();
    let s3 = server.s3_client().await;
    put_keys(&s3, &bucket, &["public/a.txt", "private/b.txt"]).await;
    let admin = admin_http_client(&endpoint).await;
    apply_admission_yaml(
        &admin,
        &endpoint,
        &format!(
            r#"
admission:
  blocks:
    - name: public-ok
      match:
        bucket: {bucket}
        path_glob: "public/**"
      action: continue
    - name: catch-all
      match:
        bucket: {bucket}
      action: deny
"#
        ),
    )
    .await;
    let list = |prefix: &str| s3.list_objects_v2().bucket(&bucket).prefix(prefix).send();
    let (keys, _) = listed(&s3, &bucket, "public/", None).await;
    assert_eq!(keys, vec!["public/a.txt".to_string()]);
    assert_eq!(status_of(list("private/").await), 403);
    assert_eq!(
        status_of(list("").await),
        403,
        "a LIST of the whole bucket is not inside the glob"
    );
}

/// A `deny` of `**` for a network blocks the bucket-level requests of that
/// network (CreateBucket, HeadBucket, DeleteObjects, ListMultipartUploads).
#[tokio::test]
async fn a_whole_bucket_glob_with_a_source_network_blocks_bucket_level_requests() {
    let server = TestServer::builder().auth("KLK3", "KLS3").build().await;
    let endpoint = server.endpoint();
    let bucket = server.bucket().to_string();
    let fresh = format!("{bucket}-new");
    let s3 = server.s3_client().await;
    let admin = admin_http_client(&endpoint).await;
    let rules = |network: &str| {
        format!(
            r#"
admission:
  blocks:
    - name: no-net-existing
      match:
        bucket: {bucket}
        path_glob: "**"
        source_ip_list: ["{network}"]
      action: deny
    - name: no-net-fresh
      match:
        bucket: {fresh}
        path_glob: "**"
        source_ip_list: ["{network}"]
      action: deny
"#
        )
    };
    let delete_one = || {
        s3.delete_objects()
            .bucket(&bucket)
            .delete(
                aws_sdk_s3::types::Delete::builder()
                    .objects(
                        aws_sdk_s3::types::ObjectIdentifier::builder()
                            .key("k.txt")
                            .build()
                            .unwrap(),
                    )
                    .build()
                    .unwrap(),
            )
            .send()
    };
    // The network of the client is blocked.
    apply_admission_yaml(&admin, &endpoint, &rules("127.0.0.0/8")).await;
    assert_eq!(
        status_of(s3.create_bucket().bucket(&fresh).send().await),
        403,
        "CreateBucket"
    );
    assert_eq!(
        status_of(s3.head_bucket().bucket(&bucket).send().await),
        403,
        "HeadBucket"
    );
    assert_eq!(status_of(delete_one().await), 403, "DeleteObjects");
    assert_eq!(
        status_of(s3.list_multipart_uploads().bucket(&bucket).send().await),
        403,
        "ListMultipartUploads"
    );
    // Control: another network is not blocked.
    apply_admission_yaml(&admin, &endpoint, &rules("10.0.0.0/8")).await;
    assert_eq!(
        status_of(s3.head_bucket().bucket(&bucket).send().await),
        200
    );
    assert_eq!(status_of(delete_one().await), 200);
    assert_eq!(
        status_of(s3.create_bucket().bucket(&fresh).send().await),
        200
    );
}

/// A `deny` with only `method` and a network, and no other block, covers each
/// DeleteObjects key and the form POST key, as it does once any other block
/// exists. The request line of both is POST; the effect on the key is DELETE or PUT.
#[tokio::test]
async fn a_method_only_deny_covers_deleted_and_posted_keys() {
    let server = TestServer::builder().open_access().build().await;
    let endpoint = server.endpoint();
    let bucket = server.bucket().to_string();
    let s3 = server.s3_client().await;
    put_keys(&s3, &bucket, &["a/one.txt", "a/two.txt"]).await;
    let admin = admin_http_client(&endpoint).await;
    apply_admission_yaml(
        &admin,
        &endpoint,
        r#"
admission:
  blocks:
    - name: no-mutation
      match:
        method: [PUT, DELETE]
        source_ip_list: ["127.0.0.0/8"]
      action: deny
"#,
    )
    .await;
    let ids: Vec<_> = ["a/one.txt", "a/two.txt"]
        .iter()
        .map(|k| {
            aws_sdk_s3::types::ObjectIdentifier::builder()
                .key(*k)
                .build()
                .unwrap()
        })
        .collect();
    let out = s3
        .delete_objects()
        .bucket(&bucket)
        .delete(
            aws_sdk_s3::types::Delete::builder()
                .set_objects(Some(ids))
                .build()
                .unwrap(),
        )
        .send()
        .await;
    match out {
        Ok(o) => {
            assert!(o.deleted().is_empty(), "deleted: {:?}", o.deleted());
            assert_eq!(o.errors().len(), 2, "{:?}", o.errors());
        }
        Err(e) => assert_eq!(status_of::<(), _>(Err(e)), 403),
    }
    let http = reqwest::Client::new();
    let form = reqwest::multipart::Form::new()
        .text("key", "a/posted.txt")
        .part(
            "file",
            reqwest::multipart::Part::bytes(b"payload".to_vec())
                .file_name("f.txt")
                .mime_str("text/plain")
                .unwrap(),
        );
    let status = http
        .post(format!("{endpoint}/{bucket}"))
        .multipart(form)
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status, StatusCode::FORBIDDEN, "form POST key");
    let get = s3
        .get_object()
        .bucket(&bucket)
        .key("a/one.txt")
        .send()
        .await;
    assert!(get.is_ok(), "reads and the objects stay: {:?}", get.err());
}

/// GET and HEAD are one read: `deny {path_glob: secret/**, method: [GET]}`
/// refuses `HEAD secret/x` too (size, ETag, metadata); a HEAD of another key works.
#[tokio::test]
async fn a_get_rule_with_a_glob_also_refuses_head() {
    let server = TestServer::builder().auth("HDK1", "HDS1").build().await;
    let endpoint = server.endpoint();
    let bucket = server.bucket().to_string();
    let s3 = server.s3_client().await;
    put_keys(&s3, &bucket, &["secret/x", "team/x"]).await;
    let admin = admin_http_client(&endpoint).await;
    apply_admission_yaml(
        &admin,
        &endpoint,
        &format!(
            r#"
admission:
  blocks:
    - name: no-secret-read
      match:
        bucket: {bucket}
        path_glob: "secret/**"
        method: [GET]
      action: deny
"#
        ),
    )
    .await;
    let head = |k: &str| s3.head_object().bucket(&bucket).key(k).send();
    assert_eq!(status_of(head("secret/x").await), 403, "HEAD secret/x");
    assert_eq!(status_of(head("team/x").await), 200, "HEAD team/x");
    let get = s3.get_object().bucket(&bucket).key("secret/x").send().await;
    assert_eq!(status_of(get), 403);
}

/// The storage facts of a write (storage type, stored size, reference hash)
/// depend on the other objects of the folder: a compression oracle. A writer
/// gets them only when no admission rule refuses a key that lies directly in
/// the folder, as for an IAM user that may not read the whole folder.
#[tokio::test]
async fn delta_facts_are_withheld_from_a_folder_with_a_refused_key() {
    let server = TestServer::builder().auth("DFK1", "DFS1").build().await;
    let endpoint = server.endpoint();
    let bucket = server.bucket().to_string();
    let http = server.http();
    let admin = admin_http_client(&endpoint).await;
    apply_admission_yaml(
        &admin,
        &endpoint,
        &format!(
            r#"
admission:
  blocks:
    - name: no-hidden
      match:
        bucket: {bucket}
        path_glob: "secret/hidden*"
      action: deny
"#
        ),
    )
    .await;
    let facts = |r: &reqwest::Response| r.headers().contains_key("x-amz-storage-type");
    let beside = common::put_object(
        &http,
        &endpoint,
        &bucket,
        "secret/ok.txt",
        b"payload".to_vec(),
        "text/plain",
    )
    .await;
    assert!(beside.status().is_success(), "{}", beside.status());
    assert!(
        !facts(&beside),
        "a refused key lies in the folder: no facts"
    );
    let elsewhere = common::put_object(
        &http,
        &endpoint,
        &bucket,
        "team/ok.txt",
        b"payload".to_vec(),
        "text/plain",
    )
    .await;
    assert!(elsewhere.status().is_success());
    assert!(
        facts(&elsewhere),
        "no refused key in the folder: facts kept"
    );
    // A sibling folder that no rule reaches keeps them too.
    let sibling = common::put_object(
        &http,
        &endpoint,
        &bucket,
        "secret-docs/ok.txt",
        b"payload".to_vec(),
        "text/plain",
    )
    .await;
    assert!(facts(&sibling));
}
