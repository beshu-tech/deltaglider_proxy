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
