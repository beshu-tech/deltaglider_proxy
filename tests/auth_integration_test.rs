// SPDX-License-Identifier: BUSL-1.1

//! Integration tests for authentication and authorization at the HTTP layer.
//!
//! Unlike `iam_test.rs` and `iam_authorization_test.rs` which test the permission
//! model through the AWS SDK, these tests exercise the actual SigV4 signing,
//! presigned URLs, clock skew, replay detection, rate limiting, and admin API
//! user lifecycle — verifying the auth *layer* as a black box.

use crate::common;

use aws_sdk_s3::presigning::PresigningConfig;
use aws_sdk_s3::primitives::ByteStream;
use common::{
    admin_http_client, get_iam_version, metrics_text, prometheus_counter_has_labels,
    wait_for_iam_rebuild, TestServer,
};
use hmac::{Hmac, Mac};
use reqwest::StatusCode;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::time::Duration;

type HmacSha256 = Hmac<Sha256>;

// ============================================================================
// Helper: create IAM user via admin API
// ============================================================================

#[derive(Clone, Debug)]
struct UserCreds {
    access_key_id: String,
    secret_access_key: String,
    id: i64,
}

async fn create_user(
    admin: &reqwest::Client,
    server: &TestServer,
    name: &str,
    permissions: Vec<serde_json::Value>,
) -> UserCreds {
    let resp = admin
        .post(format!("{}/_/api/admin/users", server.endpoint()))
        .json(&json!({
            "name": name,
            "permissions": permissions,
        }))
        .send()
        .await
        .expect("create user request failed");
    assert_eq!(resp.status().as_u16(), 201, "create user '{}' failed", name);
    let body: serde_json::Value = resp.json().await.unwrap();
    UserCreds {
        access_key_id: body["access_key_id"].as_str().unwrap().to_string(),
        secret_access_key: body["secret_access_key"].as_str().unwrap().to_string(),
        id: body["id"].as_i64().unwrap(),
    }
}

// ============================================================================
// SigV4 signing helpers (manual, for crafting invalid/custom requests)
// ============================================================================

fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).unwrap();
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

/// Derive the SigV4 signing key.
fn derive_signing_key(secret: &str, date: &str, region: &str, service: &str) -> Vec<u8> {
    let k_date = hmac_sha256(format!("AWS4{}", secret).as_bytes(), date.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    hmac_sha256(&k_service, b"aws4_request")
}

/// Build a manually signed GET request with an optional timestamp override.
fn build_signed_get(
    endpoint: &str,
    path: &str,
    access_key: &str,
    secret_key: &str,
    timestamp: &str, // "20260328T120000Z"
) -> reqwest::RequestBuilder {
    let date = &timestamp[..8]; // "20260328"
    let region = "us-east-1";
    let service = "s3";
    let credential_scope = format!("{}/{}/{}/aws4_request", date, region, service);

    // Extract host from endpoint (e.g. "http://127.0.0.1:19042" → "127.0.0.1:19042")
    let host = endpoint
        .strip_prefix("http://")
        .or_else(|| endpoint.strip_prefix("https://"))
        .unwrap_or(endpoint)
        .to_string();

    let payload_hash = "UNSIGNED-PAYLOAD";

    // Canonical headers (sorted)
    let canonical_headers = format!(
        "host:{}\nx-amz-content-sha256:{}\nx-amz-date:{}\n",
        host, payload_hash, timestamp
    );
    let signed_headers = "host;x-amz-content-sha256;x-amz-date";

    // Canonical request
    let canonical_request = format!(
        "GET\n{}\n\n{}\n{}\n{}",
        path, canonical_headers, signed_headers, payload_hash
    );

    let canonical_request_hash = sha256_hex(canonical_request.as_bytes());

    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{}\n{}",
        timestamp, credential_scope, canonical_request_hash
    );

    let signing_key = derive_signing_key(secret_key, date, region, service);
    let signature = hex::encode(hmac_sha256(&signing_key, string_to_sign.as_bytes()));

    let auth_header = format!(
        "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
        access_key, credential_scope, signed_headers, signature
    );

    let full_url = format!("{}{}", endpoint, path);
    reqwest::Client::new()
        .get(&full_url)
        .header("authorization", auth_header)
        .header("x-amz-date", timestamp)
        .header("x-amz-content-sha256", payload_hash)
        .header("host", host)
}

/// Build a manually signed PUT request (empty body) for replay detection tests.
fn build_signed_put(
    endpoint: &str,
    path: &str,
    access_key: &str,
    secret_key: &str,
    timestamp: &str,
) -> reqwest::RequestBuilder {
    let date = &timestamp[..8];
    let region = "us-east-1";
    let service = "s3";
    let credential_scope = format!("{}/{}/{}/aws4_request", date, region, service);

    let host = endpoint
        .strip_prefix("http://")
        .or_else(|| endpoint.strip_prefix("https://"))
        .unwrap_or(endpoint)
        .to_string();

    let payload_hash = sha256_hex(b"");

    let canonical_headers = format!(
        "host:{}\nx-amz-content-sha256:{}\nx-amz-date:{}\n",
        host, payload_hash, timestamp
    );
    let signed_headers = "host;x-amz-content-sha256;x-amz-date";

    let canonical_request = format!(
        "PUT\n{}\n\n{}\n{}\n{}",
        path, canonical_headers, signed_headers, payload_hash
    );

    let canonical_request_hash = sha256_hex(canonical_request.as_bytes());

    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{}\n{}",
        timestamp, credential_scope, canonical_request_hash
    );

    let signing_key = derive_signing_key(secret_key, date, region, service);
    let signature = hex::encode(hmac_sha256(&signing_key, string_to_sign.as_bytes()));

    let auth_header = format!(
        "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
        access_key, credential_scope, signed_headers, signature
    );

    let full_url = format!("{}{}", endpoint, path);
    reqwest::Client::new()
        .put(&full_url)
        .header("authorization", auth_header)
        .header("x-amz-date", timestamp)
        .header("x-amz-content-sha256", &payload_hash)
        .header("host", host)
}

// ============================================================================
// 1. Presigned URL tests
// ============================================================================

/// Presigned GET URL: upload via SDK, then download via unsigned HTTP GET on presigned URL.
#[tokio::test]
async fn test_presigned_get_url() {
    let server = TestServer::builder()
        .auth("testkey", "testsecret")
        .build()
        .await;
    let client = server.s3_client_with_creds("testkey", "testsecret").await;

    // Upload a test object
    client
        .put_object()
        .bucket(server.bucket())
        .key("presigned/file.txt")
        .body(ByteStream::from(b"presigned download data".to_vec()))
        .send()
        .await
        .expect("PUT should succeed");

    // Generate a presigned GET URL valid for 300 seconds
    let presign_config = PresigningConfig::builder()
        .expires_in(Duration::from_secs(300))
        .build()
        .unwrap();

    let presigned = client
        .get_object()
        .bucket(server.bucket())
        .key("presigned/file.txt")
        .presigned(presign_config)
        .await
        .expect("presign should succeed");

    // Use a plain HTTP client (no SigV4) to fetch the presigned URL
    let http = reqwest::Client::new();
    let resp = http
        .get(presigned.uri())
        .send()
        .await
        .expect("presigned GET failed");

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "presigned GET should return 200, got {}",
        resp.status()
    );
    let body = resp.bytes().await.unwrap();
    assert_eq!(body.as_ref(), b"presigned download data");
}

/// Presigned PUT URL: generate presigned PUT, then upload via unsigned HTTP PUT.
#[tokio::test]
async fn test_presigned_put_url() {
    let server = TestServer::builder()
        .auth("testkey", "testsecret")
        .build()
        .await;
    let client = server.s3_client_with_creds("testkey", "testsecret").await;

    let presign_config = PresigningConfig::builder()
        .expires_in(Duration::from_secs(300))
        .build()
        .unwrap();

    let presigned = client
        .put_object()
        .bucket(server.bucket())
        .key("presigned/upload.txt")
        .presigned(presign_config)
        .await
        .expect("presign PUT should succeed");

    // Upload via plain HTTP
    let http = reqwest::Client::new();
    let resp = http
        .put(presigned.uri())
        .body(b"presigned upload data".to_vec())
        .send()
        .await
        .expect("presigned PUT request failed");

    assert!(
        resp.status().is_success(),
        "presigned PUT should succeed, got {}",
        resp.status()
    );

    // Verify the object was stored correctly
    let result = client
        .get_object()
        .bucket(server.bucket())
        .key("presigned/upload.txt")
        .send()
        .await
        .expect("GET after presigned PUT should succeed");

    let body = result.body.collect().await.unwrap().into_bytes();
    assert_eq!(body.as_ref(), b"presigned upload data");
}

/// Presigned URL with IAM: user with read-only permissions can presign GET but not PUT.
#[tokio::test]
async fn test_presigned_url_respects_iam_permissions() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .build()
        .await;

    let admin = admin_http_client(&server.endpoint()).await;

    // Create admin user and reader user
    let admin_user = create_user(
        &admin,
        &server,
        "presign_admin",
        vec![json!({"effect": "Allow", "actions": ["*"], "resources": ["*"]})],
    )
    .await;

    let reader = create_user(
        &admin,
        &server,
        "presign_reader",
        vec![json!({"effect": "Allow", "actions": ["read", "list"], "resources": ["*"]})],
    )
    .await;

    // Upload as admin
    let admin_client = server
        .s3_client_with_creds(&admin_user.access_key_id, &admin_user.secret_access_key)
        .await;
    admin_client
        .put_object()
        .bucket(server.bucket())
        .key("presign-iam/file.txt")
        .body(ByteStream::from(b"admin uploaded".to_vec()))
        .send()
        .await
        .unwrap();

    // Reader can presign and GET
    let reader_client = server
        .s3_client_with_creds(&reader.access_key_id, &reader.secret_access_key)
        .await;

    let presign_config = PresigningConfig::builder()
        .expires_in(Duration::from_secs(300))
        .build()
        .unwrap();

    let presigned_get = reader_client
        .get_object()
        .bucket(server.bucket())
        .key("presign-iam/file.txt")
        .presigned(presign_config.clone())
        .await
        .unwrap();

    let http = reqwest::Client::new();
    let resp = http.get(presigned_get.uri()).send().await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "reader presigned GET should work"
    );

    // Reader presigns PUT — signing succeeds (client-side), but the server rejects it
    let presigned_put = reader_client
        .put_object()
        .bucket(server.bucket())
        .key("presign-iam/forbidden.txt")
        .presigned(presign_config)
        .await
        .unwrap();

    let resp = http
        .put(presigned_put.uri())
        .body(b"should fail".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "reader presigned PUT should be rejected by IAM"
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
        "IAM short-circuit denial should increment sanitized PUT/403/put_object metrics"
    );
}

// ============================================================================
// 2. Clock skew rejection
// ============================================================================

/// Request signed with a timestamp far in the past should be rejected.
#[tokio::test]
async fn test_clock_skew_past_rejected() {
    let server = TestServer::builder()
        .auth("testkey", "testsecret")
        .build()
        .await;

    // Sign with a timestamp from 2020 — way beyond the 5-minute skew window
    let resp = build_signed_get(
        &server.endpoint(),
        &format!("/{}", server.bucket()),
        "testkey",
        "testsecret",
        "20200101T000000Z",
    )
    .send()
    .await
    .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "Request with old timestamp should be rejected, got {}",
        resp.status()
    );
}

/// `DGP_CLOCK_SKEW_SECONDS` is the tolerance s3s enforces (S23). It was
/// documented but never passed to s3s, which kept its own 900 s default.
#[tokio::test]
async fn test_clock_skew_setting_is_enforced() {
    let server = TestServer::builder()
        .auth("testkey", "testsecret")
        .env("DGP_CLOCK_SKEW_SECONDS", "60")
        .build()
        .await;
    let five_min_ago = (chrono::Utc::now() - chrono::Duration::minutes(5))
        .format("%Y%m%dT%H%M%SZ")
        .to_string();
    let resp = build_signed_get(
        &server.endpoint(),
        &format!("/{}", server.bucket()),
        "testkey",
        "testsecret",
        &five_min_ago,
    )
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "5 min > 60 s skew");
}

/// Request signed with a timestamp far in the future should be rejected.
#[tokio::test]
async fn test_clock_skew_future_rejected() {
    let server = TestServer::builder()
        .auth("testkey", "testsecret")
        .build()
        .await;

    // Sign with a timestamp from 2099
    let resp = build_signed_get(
        &server.endpoint(),
        &format!("/{}", server.bucket()),
        "testkey",
        "testsecret",
        "20990101T000000Z",
    )
    .send()
    .await
    .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "Request with future timestamp should be rejected, got {}",
        resp.status()
    );
}

/// Request signed with a current timestamp should succeed.
#[tokio::test]
async fn test_clock_skew_current_accepted() {
    let server = TestServer::builder()
        .auth("testkey", "testsecret")
        .build()
        .await;

    let now = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();

    let resp = build_signed_get(
        &server.endpoint(),
        &format!("/{}", server.bucket()),
        "testkey",
        "testsecret",
        &now,
    )
    .send()
    .await
    .unwrap();

    // Should not be 403 — could be 200 (bucket list) or 404 (bucket not found),
    // but NOT a clock skew rejection
    assert_ne!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "Request with current timestamp should not be rejected"
    );
}

// ============================================================================
// 3. Invalid/tampered signatures
// ============================================================================

/// Request with a completely wrong secret key should be rejected.
#[tokio::test]
async fn test_wrong_secret_key_rejected() {
    let server = TestServer::builder()
        .auth("testkey", "testsecret")
        .build()
        .await;

    let now = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();

    let resp = build_signed_get(
        &server.endpoint(),
        &format!("/{}", server.bucket()),
        "testkey",
        "wrong_secret_key_here",
        &now,
    )
    .send()
    .await
    .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "Request signed with wrong secret should be rejected"
    );
}

/// Request with an unknown access key ID should be rejected.
#[tokio::test]
async fn test_unknown_access_key_rejected() {
    let server = TestServer::builder()
        .auth("testkey", "testsecret")
        .build()
        .await;

    let now = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();

    let resp = build_signed_get(
        &server.endpoint(),
        &format!("/{}", server.bucket()),
        "NONEXISTENT_KEY",
        "testsecret",
        &now,
    )
    .send()
    .await
    .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "Request with unknown access key should be rejected"
    );
}

/// Request with a mangled Authorization header should be rejected.
#[tokio::test]
async fn test_malformed_auth_header_rejected() {
    let server = TestServer::builder()
        .auth("testkey", "testsecret")
        .build()
        .await;

    let http = reqwest::Client::new();
    let resp = http
        .get(format!("{}/{}", server.endpoint(), server.bucket()))
        .header("authorization", "AWS4-HMAC-SHA256 garbage")
        .header("x-amz-content-sha256", "UNSIGNED-PAYLOAD")
        .header("x-amz-date", "20260328T120000Z")
        .send()
        .await
        .unwrap();

    // Should be 400 (invalid argument) or 403 (access denied)
    assert!(
        resp.status() == StatusCode::BAD_REQUEST || resp.status() == StatusCode::FORBIDDEN,
        "malformed auth header should be rejected, got {}",
        resp.status()
    );
}

/// Request with no auth header at all should be rejected when auth is configured.
#[tokio::test]
async fn test_no_auth_header_rejected_when_auth_enabled() {
    let server = TestServer::builder()
        .auth("testkey", "testsecret")
        .build()
        .await;

    let http = reqwest::Client::new();
    let resp = http
        .get(format!("{}/{}", server.endpoint(), server.bucket()))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "request without auth should be rejected when auth is enabled"
    );
}

// ============================================================================
// 4. Replay attack detection
// ============================================================================

/// Sending the exact same signed PUT request twice within the replay window
/// should trigger replay detection.
#[tokio::test]
async fn test_replay_attack_detected() {
    // The harness disables replay detection by default (assertion-style
    // probes repeat signatures); this test validates the wave-3 contract
    // with the production default window.
    let server = TestServer::builder()
        .auth("testkey", "testsecret")
        .production_security_defaults()
        .build()
        .await;

    let now = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let path = format!("/{}/replay-test.txt", server.bucket());

    // Use PUT (mutating method) — replay detection applies to these.
    let resp1 = build_signed_put(&server.endpoint(), &path, "testkey", "testsecret", &now)
        .send()
        .await
        .unwrap();

    let status1 = resp1.status();
    // A duplicate inside the signing second is an SDK retry and is served;
    // one after it is a replay.
    tokio::time::sleep(Duration::from_millis(1100)).await;

    // Same exact request (same signature because same timestamp+path+key)
    let resp2 = build_signed_put(&server.endpoint(), &path, "testkey", "testsecret", &now)
        .send()
        .await
        .unwrap();

    // The first request should succeed (valid credentials, current timestamp).
    assert!(
        status1.is_success(),
        "first request should not be rejected, got {}",
        status1
    );

    // The second request uses the exact same signature — replay cache should catch it.
    // Expected: 400 (InvalidArgument "Request replay detected") per auth.rs.
    assert!(
        resp2.status() == StatusCode::BAD_REQUEST || resp2.status() == StatusCode::FORBIDDEN,
        "replayed request should be rejected as 400 or 403, got {}",
        resp2.status()
    );
}

/// S23: with no `DGP_REPLAY_WINDOW_SECS`, the replay window is the clock-skew
/// window (900 s). A captured PUT replayed after the old 2 s default, while
/// its signature is still inside the skew, must be rejected.
#[tokio::test]
async fn s23_mutation_replay_after_two_seconds_is_rejected_by_default() {
    let server = TestServer::builder()
        .auth("testkey", "testsecret")
        .production_security_defaults()
        .build()
        .await;
    let now = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let path = format!("/{}/s23-replay.txt", server.bucket());
    let first = build_signed_put(&server.endpoint(), &path, "testkey", "testsecret", &now)
        .send()
        .await
        .unwrap();
    assert!(first.status().is_success(), "first PUT: {}", first.status());
    tokio::time::sleep(std::time::Duration::from_millis(3100)).await;
    let replay = build_signed_put(&server.endpoint(), &path, "testkey", "testsecret", &now)
        .send()
        .await
        .unwrap();
    assert_eq!(
        replay.status(),
        StatusCode::BAD_REQUEST,
        "a PUT replayed after 3 s must be rejected by the default window"
    );
}

/// Sending the same signed GET request twice within the replay window must be
/// TOLERATED, not rejected.
///
/// boto3/botocore emit byte-identical SigV4 signatures for the same idempotent
/// request issued (or auto-retried) within one signing second, because SigV4
/// timestamps have 1-second granularity. Replaying an idempotent read just
/// re-reads the same bytes, so the second identical GET is served normally.
/// Since S23, GET/HEAD signatures do not enter the replay cache at all;
/// mutating methods (see `test_replay_attack_detected`) stay strict.
///
/// Regression for beshu-tech/deltaglider_proxy#24: the GET/HEAD exemption that
/// fixed #7 had been removed in a security wave, which made retry-happy boto3
/// clients self-DoS via the auth-failure lockout. This locks in read-path
/// tolerance.
#[tokio::test]
async fn test_idempotent_get_replay_within_window_tolerated() {
    // Production replay defaults (the harness disables replay detection).
    let server = TestServer::builder()
        .auth("testkey", "testsecret")
        .production_security_defaults()
        .build()
        .await;

    let now = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let path = format!("/{}", server.bucket());

    // Two identical GET requests with the same timestamp produce the same
    // canonical request, hence an identical SigV4 signature. A read never
    // enters the replay cache, so the second is served, not 400'd.
    let resp1 = build_signed_get(&server.endpoint(), &path, "testkey", "testsecret", &now)
        .send()
        .await
        .unwrap();
    let resp2 = build_signed_get(&server.endpoint(), &path, "testkey", "testsecret", &now)
        .send()
        .await
        .unwrap();

    // The first request should succeed (valid credentials, current timestamp).
    assert!(
        resp1.status().is_success() || resp1.status() == StatusCode::NOT_FOUND,
        "first GET should not be rejected, got {}",
        resp1.status()
    );
    // The second identical GET must be tolerated — same outcome as the first,
    // and never a 400 replay rejection.
    assert_eq!(
        resp2.status(),
        resp1.status(),
        "second identical GET should be tolerated (same status as the first), got {}",
        resp2.status()
    );
    assert_ne!(
        resp2.status(),
        StatusCode::BAD_REQUEST,
        "idempotent-read replay must not be rejected as a replay attack"
    );
}

// ============================================================================
// 5. Unauthenticated endpoint access
// ============================================================================

/// Health endpoint should be accessible without auth.
#[tokio::test]
async fn test_health_endpoint_no_auth_needed() {
    let server = TestServer::builder()
        .auth("testkey", "testsecret")
        .build()
        .await;

    let http = reqwest::Client::new();
    let resp = http
        .get(format!("{}/_/health", server.endpoint()))
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "/_/health should work without auth"
    );
}

/// Metrics endpoint should be accessible without auth.
#[tokio::test]
async fn test_metrics_endpoint_no_auth_needed() {
    let server = TestServer::builder()
        .auth("testkey", "testsecret")
        .build()
        .await;

    let http = reqwest::Client::new();
    let resp = http
        .get(format!("{}/_/metrics", server.endpoint()))
        .send()
        .await
        .unwrap();

    assert!(
        resp.status().is_success(),
        "/_/metrics should work without auth, got {}",
        resp.status()
    );
}

/// Anonymous callers must not learn the exact build version: `/_/api/whoami`
/// carries `version` only for a live session, and `deltaglider_build_info` on
/// the public `/_/metrics` has an empty `version` label unless the operator
/// opts in with `DGP_METRICS_EXPOSE_VERSION=true`.
#[tokio::test]
async fn test_build_version_is_not_disclosed_to_anonymous_callers() {
    let server = TestServer::builder()
        .auth("testkey", "testsecret")
        .build()
        .await;
    let version = env!("CARGO_PKG_VERSION");
    let whoami_url = format!("{}/_/api/whoami", server.endpoint());

    let anon: serde_json::Value = reqwest::Client::new()
        .get(&whoami_url)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        anon.get("version").is_none(),
        "anonymous whoami leaked the version: {anon}"
    );
    assert!(
        anon.get("mode").is_some(),
        "the login page still needs `mode` before login: {anon}"
    );

    let admin = admin_http_client(&server.endpoint()).await;
    let authed: serde_json::Value = admin
        .get(&whoami_url)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        authed["version"], version,
        "a live session must see the running version"
    );

    let metrics = metrics_text(&server.endpoint()).await;
    assert!(
        metrics.contains("deltaglider_build_info{"),
        "build_info series must survive"
    );
    assert!(
        !metrics.contains(&format!("version=\"{version}\"")),
        "public /_/metrics leaked the version:\n{metrics}"
    );
}

/// `DGP_METRICS_EXPOSE_VERSION=true` restores the `version` label for
/// operators whose fleet dashboards key on it.
#[tokio::test]
async fn test_metrics_build_info_version_on_operator_opt_in() {
    let server = TestServer::builder()
        .auth("testkey", "testsecret")
        .env("DGP_METRICS_EXPOSE_VERSION", "true")
        .build()
        .await;
    let metrics = metrics_text(&server.endpoint()).await;
    assert!(
        metrics.contains(&format!("version=\"{}\"", env!("CARGO_PKG_VERSION"))),
        "opt-in must put the version back on build_info:\n{metrics}"
    );
}

/// Unknown paths under `/_/` are honest 404s — only genuine SPA routes fall
/// back to `index.html`. A mistyped admin API path or a source map that is
/// not shipped must not come back as a 200 HTML page.
#[tokio::test]
async fn test_unknown_ui_paths_are_404_not_spa_fallback() {
    let server = TestServer::builder()
        .auth("testkey", "testsecret")
        .build()
        .await;
    let http = reqwest::Client::new();
    let get = |p: &str| http.get(format!("{}{}", server.endpoint(), p)).send();

    // Every method, not only GET: a GET-only catch-all made axum answer
    // POST/PUT/DELETE on a mistyped path with 405 `Allow: GET,HEAD`, which
    // reads as "resource exists".
    for method in [
        reqwest::Method::GET,
        reqwest::Method::POST,
        reqwest::Method::PUT,
        reqwest::Method::DELETE,
    ] {
        let api = http
            .request(
                method.clone(),
                format!("{}/_/api/admin/does-not-exist", server.endpoint()),
            )
            .send()
            .await
            .unwrap();
        assert_eq!(api.status(), StatusCode::NOT_FOUND, "{method}");
        assert_eq!(
            api.headers()
                .get(reqwest::header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("no-store"),
            "{method}: a 404 must not be cacheable"
        );
        assert_eq!(
            api.json::<serde_json::Value>().await.unwrap()["error"],
            "not_found",
            "{method}"
        );
    }

    for p in ["/_/assets/index-deadbeef.js.map", "/_/no-such-view"] {
        let resp = get(p).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::NOT_FOUND,
            "{p} must be a 404, not the SPA shell"
        );
        assert_eq!(
            resp.headers()
                .get(reqwest::header::CACHE_CONTROL)
                .and_then(|v| v.to_str().ok()),
            Some("no-store"),
            "{p}: a 404 must not be cacheable"
        );
    }

    // A genuine SPA route still serves the app. CI builds the UI before the
    // Rust tests; a local checkout without `dist/` gets the explicit
    // "Demo UI not built" 404 from `serve_index`, which is not a regression.
    let spa = get("/_/browse").await.unwrap();
    let status = spa.status();
    let content_type = spa
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = spa.text().await.unwrap();
    if status == StatusCode::NOT_FOUND && body == "Demo UI not built" {
        eprintln!("skipping SPA-route assertion: demo UI not built");
        return;
    }
    assert_eq!(status, StatusCode::OK, "/_/browse must serve the SPA shell");
    assert!(
        content_type.starts_with("text/html"),
        "SPA shell must be HTML, got {content_type}"
    );

    // No source map for any REAL chunk either. The shell names its entry
    // chunk; that chunk's `.map` must be absent, not only a made-up name.
    let chunk = body
        .split("/_/assets/")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .expect("index.html references an /_/assets/ chunk");
    let map = get(&format!("/_/assets/{chunk}.map")).await.unwrap();
    assert_eq!(
        map.status(),
        StatusCode::NOT_FOUND,
        "source map for the entry chunk {chunk} must not be served"
    );
}

/// Browser review #21: the UI's HTML and JS go out compressed when the
/// browser accepts it (they were sent raw: several MB per first load).
#[tokio::test]
async fn test_ui_assets_are_compressed() {
    let server = TestServer::builder()
        .auth("testkey", "testsecret")
        .build()
        .await;
    let http = reqwest::Client::new();
    let get = |p: String, enc: &'static str| {
        http.get(format!("{}{}", server.endpoint(), p))
            .header(reqwest::header::ACCEPT_ENCODING, enc)
            .send()
    };
    let shell = get("/_/browse".into(), "identity").await.unwrap();
    let body = shell.text().await.unwrap();
    if body == "Demo UI not built" {
        eprintln!("skipping: demo UI not built");
        return;
    }
    let chunk = body
        .split("/_/assets/")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .expect("index.html references an /_/assets/ chunk")
        .to_string();
    for (path, enc, want) in [
        ("/_/browse".to_string(), "gzip", "gzip"),
        (format!("/_/assets/{chunk}"), "br, gzip", "br"),
        (format!("/_/assets/{chunk}"), "gzip", "gzip"),
    ] {
        let resp = get(path.clone(), enc).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{path}");
        assert_eq!(
            resp.headers()
                .get(reqwest::header::CONTENT_ENCODING)
                .and_then(|v| v.to_str().ok()),
            Some(want),
            "{path} with Accept-Encoding: {enc}"
        );
    }
    // A client that accepts no encoding gets the raw bytes.
    let raw = get(format!("/_/assets/{chunk}"), "identity").await.unwrap();
    assert!(raw
        .headers()
        .get(reqwest::header::CONTENT_ENCODING)
        .is_none());
}

/// `DGP_METRICS_BEARER_TOKEN` turns the public scrape into a token-gated
/// one: Prometheus presents the token, the admin dashboard presents its
/// session, and anonymous callers get a 401 with no metric names to
/// fingerprint the release by.
#[tokio::test]
async fn test_metrics_bearer_token_gate() {
    let server = TestServer::builder()
        .auth("testkey", "testsecret")
        .env("DGP_METRICS_BEARER_TOKEN", "scrape-me-7")
        .build()
        .await;
    let url = format!("{}/_/metrics", server.endpoint());
    let http = reqwest::Client::new();

    let anon = http.get(&url).send().await.unwrap();
    assert_eq!(anon.status(), StatusCode::UNAUTHORIZED);
    assert!(anon
        .headers()
        .contains_key(reqwest::header::WWW_AUTHENTICATE));
    assert!(
        !anon.text().await.unwrap().contains("deltaglider_"),
        "no metric names for anonymous callers"
    );

    let wrong = http
        .get(&url)
        .bearer_auth("scrape-me-8")
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);

    let right = http
        .get(&url)
        .bearer_auth("scrape-me-7")
        .send()
        .await
        .unwrap();
    assert_eq!(right.status(), StatusCode::OK);
    assert!(right
        .text()
        .await
        .unwrap()
        .contains("deltaglider_build_info"));

    let admin = admin_http_client(&server.endpoint()).await;
    assert_eq!(
        admin.get(&url).send().await.unwrap().status(),
        StatusCode::OK,
        "the admin dashboard scrapes with its session"
    );
}

/// A wrong bearer is a failed credential check like a wrong password: it
/// counts against the per-IP limiter and locks the caller out.
#[tokio::test]
async fn test_metrics_bearer_token_is_rate_limited() {
    let server = TestServer::builder()
        .auth("testkey", "testsecret")
        .env("DGP_METRICS_BEARER_TOKEN", "scrape-me-9")
        .env("DGP_RATE_LIMIT_MAX_ATTEMPTS", "3")
        .build()
        .await;
    let url = format!("{}/_/metrics", server.endpoint());
    let http = reqwest::Client::new();

    let mut statuses = Vec::new();
    for i in 0..5 {
        let resp = http
            .get(&url)
            .bearer_auth(format!("wrong-{i}"))
            .send()
            .await
            .unwrap();
        statuses.push(resp.status().as_u16());
    }
    assert!(
        statuses.iter().take(3).all(|s| *s == 401) && statuses.iter().skip(3).all(|s| *s == 429),
        "3 wrong tokens then lockout, got {statuses:?}"
    );

    // The dashboard path (no Authorization header at all) is not a
    // credential check and is not counted: an anonymous scrape is still a
    // plain 401, not a 429.
    assert_eq!(
        http.get(&url).send().await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
}

/// The product docs and the canned-policy catalogue both change with every
/// release, so they are served only to a live session.
#[tokio::test]
async fn test_docs_and_policies_require_a_session() {
    let server = TestServer::builder()
        .auth("testkey", "testsecret")
        .build()
        .await;
    let http = reqwest::Client::new();
    for path in ["/_/api/docs", "/_/api/admin/policies"] {
        let resp = http
            .get(format!("{}{}", server.endpoint(), path))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "{path} must not be anonymous"
        );
    }

    let admin = admin_http_client(&server.endpoint()).await;
    let docs: serde_json::Value = admin
        .get(format!("{}/_/api/docs", server.endpoint()))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let groups = docs["manifest"]["groups"]
        .as_array()
        .expect("manifest groups");
    assert!(!groups.is_empty(), "manifest groups must be served");
    let entries = docs["docs"].as_array().expect("docs array");
    assert!(
        entries.len() > 10,
        "expected the full product docs, got {}",
        entries.len()
    );
    assert!(
        entries.iter().any(|d| d["path"] == "changelog"),
        "the changelog is served — behind the session"
    );
    assert!(entries
        .iter()
        .all(|d| d["content"].as_str().is_some_and(|c| !c.is_empty())));
}

/// HEAD / (connection probe) should be accessible without auth.
#[tokio::test]
async fn test_head_root_no_auth_needed() {
    let server = TestServer::builder()
        .auth("testkey", "testsecret")
        .build()
        .await;

    let http = reqwest::Client::new();
    let resp = http
        .head(format!("{}/", server.endpoint()))
        .send()
        .await
        .unwrap();

    assert!(
        resp.status().is_success(),
        "HEAD / should work without auth, got {}",
        resp.status()
    );
}

// ============================================================================
// 6. Admin API user lifecycle (CRUD → auth verification)
// ============================================================================

/// Full lifecycle: create user → authenticate → update permissions → verify → disable → verify → delete.
#[tokio::test]
async fn test_user_lifecycle_crud() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .build()
        .await;

    let admin = admin_http_client(&server.endpoint()).await;

    // 1. Create user with read-only permissions
    let user = create_user(
        &admin,
        &server,
        "lifecycle_user",
        vec![json!({"effect": "Allow", "actions": ["read", "list"], "resources": ["*"]})],
    )
    .await;

    // 2. Verify user can read
    let s3 = server
        .s3_client_with_creds(&user.access_key_id, &user.secret_access_key)
        .await;
    let list_result = s3.list_objects_v2().bucket(server.bucket()).send().await;
    assert!(list_result.is_ok(), "new user should be able to list");

    // 3. Verify user cannot write
    let put_result = s3
        .put_object()
        .bucket(server.bucket())
        .key("lifecycle/test.txt")
        .body(ByteStream::from(b"test".to_vec()))
        .send()
        .await;
    assert!(
        put_result.is_err(),
        "read-only user should not be able to write"
    );

    // 4. Update permissions: grant write
    // Snapshot the IAM version BEFORE the mutation so we can barrier on it.
    let before_version = get_iam_version(&admin, &server.endpoint()).await;
    let resp = admin
        .put(format!(
            "{}/_/api/admin/users/{}",
            server.endpoint(),
            user.id
        ))
        .json(&json!({
            "name": "lifecycle_user",
            "permissions": [{"effect": "Allow", "actions": ["*"], "resources": ["*"]}]
        }))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "update user should succeed, got {}",
        resp.status()
    );

    // 5. Verify user can now write (permissions updated via hot-swap).
    // Wait for IAM index rebuild deterministically — polls iam/version
    // until it advances past the baseline (typically <50ms on any
    // runner). Recreates the S3 client to drop stale SigV4 context.
    wait_for_iam_rebuild(&admin, &server.endpoint(), before_version).await;
    let s3 = server
        .s3_client_with_creds(&user.access_key_id, &user.secret_access_key)
        .await;
    // Use a unique body + key for the post-update PUT so its SigV4
    // signature cannot collide with step-3's failed write attempt
    // (SigV4 timestamps have 1s resolution; the replay window is 2s,
    // and the barrier often returns within a few ms). Previously the
    // `sleep(1s)` accidentally also served as a replay-window wait.
    let put_result = s3
        .put_object()
        .bucket(server.bucket())
        .key("lifecycle/after_update.txt")
        .body(ByteStream::from(b"after update".to_vec()))
        .send()
        .await;
    assert!(
        put_result.is_ok(),
        "user should be able to write after permission update: {:?}",
        put_result.err()
    );

    // 6. Disable the user
    let resp = admin
        .put(format!(
            "{}/_/api/admin/users/{}",
            server.endpoint(),
            user.id
        ))
        .json(&json!({
            "name": "lifecycle_user",
            "enabled": false,
            "permissions": [{"effect": "Allow", "actions": ["*"], "resources": ["*"]}]
        }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "disable user should succeed");

    // 7. Verify disabled user is rejected
    let list_result = s3.list_objects_v2().bucket(server.bucket()).send().await;
    assert!(list_result.is_err(), "disabled user should be rejected");

    // 8. Delete the user
    let resp = admin
        .delete(format!(
            "{}/_/api/admin/users/{}",
            server.endpoint(),
            user.id
        ))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "delete user should succeed");

    // 9. Verify deleted user is rejected
    let list_result = s3.list_objects_v2().bucket(server.bucket()).send().await;
    assert!(list_result.is_err(), "deleted user should be rejected");
}

// ============================================================================
// 7. Rate limiting / brute force protection
// ============================================================================

/// Wrong-SECRET signatures (a known access key) feed the brute-force
/// limiter, and a valid identity no longer resets it before s3s verifies the
/// signature (S19). Before the fix every attempt called `record_success`, so
/// the counter never grew and a secret-guessing loop was never throttled.
#[tokio::test]
async fn test_wrong_secret_signatures_are_rate_limited() {
    let server = TestServer::builder()
        .auth("testkey", "testsecret")
        .env("DGP_RATE_LIMIT_MAX_ATTEMPTS", "3")
        .env("DGP_RATE_LIMIT_WINDOW_SECS", "60")
        .env("DGP_RATE_LIMIT_LOCKOUT_SECS", "60")
        .build()
        .await;
    let path = format!("/{}", server.bucket());
    let now = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    for _ in 0..3 {
        let resp = build_signed_get(&server.endpoint(), &path, "testkey", "wrong-secret", &now)
            .header("x-forwarded-for", "10.0.0.98")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }
    // The IP is locked out now: even the right secret gets SlowDown.
    let now = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let resp = build_signed_get(&server.endpoint(), &path, "testkey", "testsecret", &now)
        .header("x-forwarded-for", "10.0.0.98")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        503,
        "wrong-secret signatures must lock the IP out"
    );
    // Another IP is not affected, and a verified request still succeeds.
    let resp = build_signed_get(&server.endpoint(), &path, "testkey", "testsecret", &now)
        .header("x-forwarded-for", "10.0.0.97")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

/// Multiple rapid auth failures should trigger rate limiting (progressive delay or lockout).
#[tokio::test]
async fn test_brute_force_rate_limiting() {
    // Override rate limiter to small values for fast testing. Passed to the
    // proxy child only: all integration tests share one process (tests/all.rs),
    // so std::env::set_var would leak into every proxy other tests start.
    let server = TestServer::builder()
        .auth("testkey", "testsecret")
        .env("DGP_RATE_LIMIT_MAX_ATTEMPTS", "5")
        .env("DGP_RATE_LIMIT_WINDOW_SECS", "60")
        .env("DGP_RATE_LIMIT_LOCKOUT_SECS", "60")
        .build()
        .await;

    let now = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();

    // Send rapid requests with UNKNOWN access keys from the same "IP" (via
    // X-Forwarded-For, trusted because DGP_TRUST_PROXY_HEADERS=true in tests).
    // Unknown access keys; wrong-secret signatures on a known key are
    // covered by `test_wrong_secret_signatures_are_rate_limited`.
    let mut statuses = Vec::new();
    for i in 0..15 {
        let resp = build_signed_get(
            &server.endpoint(),
            &format!("/{}", server.bucket()),
            &format!("WRONGKEY{}", i),
            "irrelevant_secret",
            &now,
        )
        .header("x-forwarded-for", "10.0.0.99")
        .send()
        .await
        .unwrap();
        statuses.push(resp.status());
    }

    // After many failures, we should see either:
    // - 403 (still rejecting, but with progressive delay)
    // - 429/503 (rate limited / slow down)
    // At minimum, verify none caused a server error
    for (i, status) in statuses.iter().enumerate() {
        assert_ne!(
            *status,
            StatusCode::INTERNAL_SERVER_ERROR,
            "attempt {} should not cause server error",
            i
        );
    }

    // Rate limiter threshold overridden to 5 failures for this test.
    // After 15 rapid failures, we must see 503 SlowDown responses.
    let rate_limited_count = statuses
        .iter()
        .filter(|s| s.as_u16() == 503 || s.as_u16() == 429)
        .count();

    // At least some requests after the 5th should be rate-limited
    assert!(
        rate_limited_count > 0,
        "expected rate limiting after 5+ failures, but all {} responses were: {:?}",
        statuses.len(),
        statuses.iter().map(|s| s.as_u16()).collect::<Vec<_>>()
    );

    // Verify the server didn't crash — no 500s
    for (i, status) in statuses.iter().enumerate() {
        assert_ne!(
            *status,
            StatusCode::INTERNAL_SERVER_ERROR,
            "attempt {} should not cause server error",
            i
        );
    }
}

// ============================================================================
// 8. IAM conditions at HTTP level
// ============================================================================

/// s3:prefix condition: deny listing with dotfile prefix, allow normal prefix.
#[tokio::test]
async fn test_iam_prefix_condition_blocks_dotfile_listing() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .build()
        .await;

    let admin = admin_http_client(&server.endpoint()).await;

    // Create user: Allow read+list on bucket, Deny list when prefix starts with "."
    let user = create_user(
        &admin,
        &server,
        "prefix_user",
        vec![
            json!({
                "effect": "Allow",
                "actions": ["read", "list"],
                "resources": [format!("{}/*", server.bucket())]
            }),
            json!({
                "effect": "Deny",
                "actions": ["list"],
                "resources": [server.bucket()],
                "conditions": {"StringLike": {"s3:prefix": ".*"}}
            }),
        ],
    )
    .await;

    let s3 = server
        .s3_client_with_creds(&user.access_key_id, &user.secret_access_key)
        .await;

    // List with normal prefix — should succeed
    let normal_list = s3
        .list_objects_v2()
        .bucket(server.bucket())
        .prefix("docs/")
        .send()
        .await;
    assert!(
        normal_list.is_ok(),
        "listing with normal prefix should succeed"
    );

    // List with dotfile prefix — Deny condition should fire
    let dotfile_list = s3
        .list_objects_v2()
        .bucket(server.bucket())
        .prefix(".hidden/")
        .send()
        .await;
    assert!(
        dotfile_list.is_err(),
        "listing with dotfile prefix should be denied by Deny+condition"
    );
}

// ============================================================================
// 9. CORS preflight passthrough
// ============================================================================

/// OPTIONS requests should pass through without auth.
#[tokio::test]
async fn test_options_cors_preflight_no_auth() {
    let server = TestServer::builder()
        .auth("testkey", "testsecret")
        .build()
        .await;

    let http = reqwest::Client::new();
    let resp = http
        .request(
            reqwest::Method::OPTIONS,
            format!("{}/{}", server.endpoint(), server.bucket()),
        )
        .header("origin", "https://example.com")
        .header("access-control-request-method", "PUT")
        .send()
        .await
        .unwrap();

    // OPTIONS should not return 403
    assert_ne!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "OPTIONS preflight should not require auth"
    );
}

// ============================================================================
// 10. Admin API: groups lifecycle
// ============================================================================

/// Create a group, add a user to it, verify the user inherits group permissions.
#[tokio::test]
async fn test_group_creation_and_permission_inheritance() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .build()
        .await;

    let admin = admin_http_client(&server.endpoint()).await;

    // Create a user with NO direct permissions
    let user = create_user(&admin, &server, "group_test_user", vec![]).await;

    // Verify user can't do anything
    let s3 = server
        .s3_client_with_creds(&user.access_key_id, &user.secret_access_key)
        .await;
    let result = s3.list_objects_v2().bucket(server.bucket()).send().await;
    assert!(result.is_err(), "user with no permissions should be denied");

    // Create a group with read+list permissions AND add the user as a
    // member in one call. Snapshot the version first so we can barrier.
    let before_version = get_iam_version(&admin, &server.endpoint()).await;
    let resp = admin
        .post(format!("{}/_/api/admin/groups", server.endpoint()))
        .json(&json!({
            "name": "readers",
            "permissions": [{"effect": "Allow", "actions": ["read", "list"], "resources": ["*"]}],
            "member_ids": [user.id]
        }))
        .send()
        .await
        .unwrap();

    assert!(
        resp.status().is_success(),
        "create group with member_ids should succeed, got {}",
        resp.status()
    );

    // Wait for IAM index rebuild deterministically — poll iam/version
    // instead of sleeping. The group-create + member-add mutation bumps
    // the counter once the new IamIndex is stored.
    wait_for_iam_rebuild(&admin, &server.endpoint(), before_version).await;

    // Re-create S3 client to ensure fresh SigV4 signing context
    let s3 = server
        .s3_client_with_creds(&user.access_key_id, &user.secret_access_key)
        .await;

    // Verify user can now list (inherited from group).
    // This exercises the full flow: create group → add member → IAM rebuild →
    // group permissions merge → SigV4 auth → list allowed.
    //
    // `.max_keys(1)` differentiates the canonical request from the earlier
    // "verify denied" `list_objects_v2()` call (line ~1063). Without it, both
    // requests sign to the same SigV4 signature and the second one would
    // trip a replay cache. GET/HEAD no longer enter it and the harness
    // disables replay detection, so this is belt and braces. The bucket
    // starts empty, so max_keys=1 does not change the observable result.
    let result = s3
        .list_objects_v2()
        .bucket(server.bucket())
        .max_keys(1)
        .send()
        .await;
    assert!(
        result.is_ok(),
        "user should be able to list after being added to group: {:?}",
        result.err()
    );
}

// ============================================================================
// 11. Edge cases
// ============================================================================

/// Verify that Basic auth (non-SigV4) is rejected.
#[tokio::test]
async fn test_basic_auth_rejected() {
    let server = TestServer::builder()
        .auth("testkey", "testsecret")
        .build()
        .await;

    let http = reqwest::Client::new();
    let resp = http
        .get(format!("{}/{}", server.endpoint(), server.bucket()))
        .header("authorization", "Basic dGVzdGtleTp0ZXN0c2VjcmV0")
        .send()
        .await
        .unwrap();

    assert!(
        resp.status() == StatusCode::BAD_REQUEST || resp.status() == StatusCode::FORBIDDEN,
        "Basic auth should be rejected, got {}",
        resp.status()
    );
}

/// Verify that the empty bucket path (ListBuckets) requires auth when auth is enabled.
#[tokio::test]
async fn test_list_buckets_requires_auth() {
    let server = TestServer::builder()
        .auth("testkey", "testsecret")
        .build()
        .await;

    let http = reqwest::Client::new();
    let resp = http
        .get(format!("{}/", server.endpoint()))
        .send()
        .await
        .unwrap();

    // GET / without auth should be 403 (not HEAD / which is allowed as connection probe)
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "GET / (ListBuckets) without auth should be rejected"
    );
}

// ============================================================================
// QA finding #10: SigV4 tampering edge cases
// ============================================================================
//
// The tests above cover the "obvious negative" cases: wrong secret,
// unknown key, missing header, malformed header, clock skew, replay.
// These three tests hit three more-subtle security invariants:
//
//   1. Signed-header tampering — changing a header that was covered
//      by the signature must invalidate it.
//   2. Presigned-URL post-disable rejection — generating a presigned
//      URL for a user who is then disabled must invalidate the URL
//      for its remaining validity window.
//   3. Unsigned-header tolerance (spec compliance) — the verifier
//      must NOT reject requests that include extra headers not in
//      the `SignedHeaders` list. AWS clients rely on this.

/// Tampering with a signed header AFTER signing must produce 403.
///
/// The test crafts a valid signed GET, then modifies the
/// `x-amz-content-sha256` header (which IS in the signed set) to a
/// different value before sending. Since the signature was computed
/// over the original header value, the server's recomputation must
/// differ → signature mismatch → reject.
///
/// This catches the class of bugs where the verifier would short-
/// circuit comparison on e.g. known headers, trust the client-
/// provided canonical headers instead of rebuilding from the raw
/// request, etc.
#[tokio::test]
async fn test_signed_header_tampering_rejected() {
    let server = TestServer::builder()
        .auth("tamper-key", "tamper-secret-1234567890")
        .build()
        .await;

    // Compute a valid GET signature over the normal headers.
    let timestamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let req = build_signed_get(
        &server.endpoint(),
        "/",
        "tamper-key",
        "tamper-secret-1234567890",
        &timestamp,
    );

    // REPLACE the signed `x-amz-content-sha256` with a different
    // value. `.header()` appends — we need to go through `headers_mut`
    // to actually overwrite the original. The signature was computed
    // over "UNSIGNED-PAYLOAD"; replace it with the SHA-256 of empty.
    // The server's canonical-request rebuild must use the NEW value,
    // get a different signature hash, and reject.
    let mut built = req.build().expect("build tampered request");
    let new_sha = sha256_hex(b"");
    built
        .headers_mut()
        .insert("x-amz-content-sha256", new_sha.parse().unwrap());
    let tampered = reqwest::Client::new()
        .execute(built)
        .await
        .expect("tampered send");

    assert_eq!(
        tampered.status(),
        StatusCode::FORBIDDEN,
        "tampered signed header must produce 403, got {}",
        tampered.status()
    );
}

/// A presigned URL issued BEFORE a user was disabled must not work
/// AFTER the disable. This is the "stolen URL" scenario: an attacker
/// obtains a valid presigned URL from logs/memory, then the admin
/// disables the user — the admin expects ALL the user's outstanding
/// URLs to fail.
///
/// Without this guard, a disabled user's URLs remain valid for the
/// remainder of the presign window (up to 7 days in AWS-S3-compatible
/// defaults), which contradicts "disable = no access."
#[tokio::test]
async fn test_presigned_url_rejected_after_user_disabled() {
    let server = TestServer::builder()
        .auth("bootstrap", "bootstrap-secret-1234567890")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    // Create a user with full access.
    let user = create_user(
        &admin,
        &server,
        "presigned_disable_target",
        vec![json!({"effect": "Allow", "actions": ["*"], "resources": ["*"]})],
    )
    .await;

    // Generate a presigned GET URL using the user's creds. We don't
    // even need an object — a presigned LIST (HEAD bucket) has the
    // same auth flow and is simpler to target.
    let s3 = server
        .s3_client_with_creds(&user.access_key_id, &user.secret_access_key)
        .await;

    // Seed + presign GET of a known key.
    s3.put_object()
        .bucket(server.bucket())
        .key("pre/disabled-target.txt")
        .body(ByteStream::from(b"whatever".to_vec()))
        .send()
        .await
        .expect("seed PUT");
    let presigned = s3
        .get_object()
        .bucket(server.bucket())
        .key("pre/disabled-target.txt")
        .presigned(
            PresigningConfig::builder()
                .expires_in(Duration::from_secs(300))
                .build()
                .unwrap(),
        )
        .await
        .expect("presign");
    let url = presigned.uri().to_string();

    // Verify URL works BEFORE disabling (sanity: the URL itself is valid).
    let http = reqwest::Client::new();
    let ok = http.get(&url).send().await.expect("pre-disable GET");
    assert_eq!(
        ok.status(),
        StatusCode::OK,
        "pre-disable presigned GET must succeed, got {}",
        ok.status()
    );

    // Disable the user.
    let before_version = get_iam_version(&admin, &server.endpoint()).await;
    let resp = admin
        .put(format!(
            "{}/_/api/admin/users/{}",
            server.endpoint(),
            user.id
        ))
        .json(&json!({
            "name": "presigned_disable_target",
            "enabled": false,
            "permissions": [{"effect": "Allow", "actions": ["*"], "resources": ["*"]}]
        }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "disable user");
    wait_for_iam_rebuild(&admin, &server.endpoint(), before_version).await;

    // Now the SAME presigned URL must fail. The signature is still
    // valid cryptographically — the rejection must come from the
    // auth layer's user-enabled check.
    let denied = http.get(&url).send().await.expect("post-disable GET");
    assert_eq!(
        denied.status(),
        StatusCode::FORBIDDEN,
        "post-disable presigned GET must return 403, got {}",
        denied.status()
    );
}

/// Per the SigV4 spec, extra HTTP headers not listed in
/// `SignedHeaders` are IGNORED by the verifier — they can be added
/// safely (for tracing, routing, etc.) without breaking the
/// signature. The proxy MUST NOT reject such requests, or standard
/// AWS clients sending `user-agent`, `accept-encoding`, etc. would
/// break.
///
/// This is a positive-path spec-compliance test, not a negative one.
/// It protects against a regression where the verifier rebuilds the
/// canonical request from ALL incoming headers (wrong) instead of
/// just the headers named in `SignedHeaders` (correct).
#[tokio::test]
async fn test_unsigned_extra_header_is_tolerated() {
    let server = TestServer::builder()
        .auth("spec-key", "spec-secret-1234567890")
        .build()
        .await;

    // Sign a GET / as ListBuckets. The helper only signs the three
    // canonical headers (host, x-amz-content-sha256, x-amz-date).
    let timestamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let req = build_signed_get(
        &server.endpoint(),
        "/",
        "spec-key",
        "spec-secret-1234567890",
        &timestamp,
    );

    // Add an arbitrary custom header NOT in SignedHeaders. Also add
    // `accept-encoding` (which every real browser/curl would send)
    // to cover the common client path.
    let resp = req
        .header("x-amz-custom-tracing", "trace-id-1234")
        .header("accept-encoding", "gzip, deflate")
        .send()
        .await
        .expect("unsigned-extra-header send");

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "extra-unsigned-header request must pass, got {} — \
         SigV4 verifier should ignore headers outside the SignedHeaders set",
        resp.status()
    );
}

// ============================================================================
// H1 fix: SigV4 must verify the actual body's SHA-256 matches the signed
// `x-amz-content-sha256` header value.
// ============================================================================

/// Build a valid signed PUT for `body_to_sign`, then send a DIFFERENT
/// body. Pre-fix the proxy stored the wrong body silently — the
/// signature was valid because it's computed over the canonical
/// request which only sees the header value, not the body.
#[tokio::test]
async fn test_sigv4_payload_hash_mismatch_rejected() {
    let server = TestServer::builder()
        .auth("hash-key", "hash-secret-1234567890")
        .build()
        .await;

    let signed_body: &[u8] = b"the body the client signed";
    let actual_body: &[u8] = b"a different body the attacker sent";

    let timestamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let date = &timestamp[..8];
    let region = "us-east-1";
    let service = "s3";
    let credential_scope = format!("{}/{}/{}/aws4_request", date, region, service);

    let endpoint = server.endpoint();
    let host = endpoint
        .strip_prefix("http://")
        .or_else(|| endpoint.strip_prefix("https://"))
        .unwrap_or(&endpoint);

    let payload_hash = sha256_hex(signed_body);
    let path = format!("/{}/{}", server.bucket(), "h1-mismatch.bin");

    let canonical_headers = format!(
        "host:{}\nx-amz-content-sha256:{}\nx-amz-date:{}\n",
        host, payload_hash, timestamp
    );
    let signed_headers = "host;x-amz-content-sha256;x-amz-date";

    let canonical_request = format!(
        "PUT\n{}\n\n{}\n{}\n{}",
        path, canonical_headers, signed_headers, payload_hash
    );
    let canonical_request_hash = sha256_hex(canonical_request.as_bytes());
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{}\n{}",
        timestamp, credential_scope, canonical_request_hash
    );
    let signing_key = derive_signing_key("hash-secret-1234567890", date, region, service);
    let signature = hex::encode(hmac_sha256(&signing_key, string_to_sign.as_bytes()));

    let auth_header = format!(
        "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
        "hash-key", credential_scope, signed_headers, signature
    );

    let resp = reqwest::Client::new()
        .put(format!("{}{}", endpoint, path))
        .header("authorization", auth_header)
        .header("x-amz-date", timestamp)
        .header("x-amz-content-sha256", &payload_hash)
        .header("host", host)
        .body(actual_body.to_vec())
        .send()
        .await
        .expect("send mismatched body");

    // Pre-fix: 200 (silently stores actual_body).
    // Post-fix: 400 BadDigest.
    assert_eq!(
        resp.status().as_u16(),
        400,
        "H1 REGRESSION: PUT with body mismatching signed hash must reject with 400, got {}",
        resp.status()
    );
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("BadDigest"),
        "expected BadDigest error code, got body: {}",
        body
    );
}

/// UNSIGNED-PAYLOAD must continue to accept arbitrary body content
/// — the client explicitly opted out of body-hash signing.
#[tokio::test]
async fn test_sigv4_unsigned_payload_accepts_any_body() {
    let server = TestServer::builder()
        .auth("unsigned-key", "unsigned-secret-1234567890")
        .build()
        .await;

    let body: &[u8] = b"anything goes";

    let timestamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let date = &timestamp[..8];
    let region = "us-east-1";
    let service = "s3";
    let credential_scope = format!("{}/{}/{}/aws4_request", date, region, service);

    let endpoint = server.endpoint();
    let host = endpoint
        .strip_prefix("http://")
        .or_else(|| endpoint.strip_prefix("https://"))
        .unwrap_or(&endpoint);

    let payload_hash = "UNSIGNED-PAYLOAD";
    let path = format!("/{}/{}", server.bucket(), "h1-unsigned.bin");

    let canonical_headers = format!(
        "host:{}\nx-amz-content-sha256:{}\nx-amz-date:{}\n",
        host, payload_hash, timestamp
    );
    let signed_headers = "host;x-amz-content-sha256;x-amz-date";

    let canonical_request = format!(
        "PUT\n{}\n\n{}\n{}\n{}",
        path, canonical_headers, signed_headers, payload_hash
    );
    let canonical_request_hash = sha256_hex(canonical_request.as_bytes());
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{}\n{}",
        timestamp, credential_scope, canonical_request_hash
    );
    let signing_key = derive_signing_key("unsigned-secret-1234567890", date, region, service);
    let signature = hex::encode(hmac_sha256(&signing_key, string_to_sign.as_bytes()));

    let auth_header = format!(
        "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
        "unsigned-key", credential_scope, signed_headers, signature
    );

    let resp = reqwest::Client::new()
        .put(format!("{}{}", endpoint, path))
        .header("authorization", auth_header)
        .header("x-amz-date", timestamp)
        .header("x-amz-content-sha256", payload_hash)
        .header("host", host)
        .body(body.to_vec())
        .send()
        .await
        .expect("send unsigned-payload");
    assert_eq!(
        resp.status().as_u16(),
        200,
        "UNSIGNED-PAYLOAD must continue to work, got {}",
        resp.status()
    );
}

/// A correctly-signed PUT (body actually matches the signed hash)
/// must succeed — sanity check that the H1 enforcement doesn't
/// break the happy path.
#[tokio::test]
async fn test_sigv4_payload_hash_match_succeeds() {
    let server = TestServer::builder()
        .auth("hash-ok-key", "hash-ok-secret-1234567890")
        .build()
        .await;

    let body: &[u8] = b"correctly-signed body";

    let timestamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let date = &timestamp[..8];
    let region = "us-east-1";
    let service = "s3";
    let credential_scope = format!("{}/{}/{}/aws4_request", date, region, service);

    let endpoint = server.endpoint();
    let host = endpoint
        .strip_prefix("http://")
        .or_else(|| endpoint.strip_prefix("https://"))
        .unwrap_or(&endpoint);

    let payload_hash = sha256_hex(body);
    let path = format!("/{}/{}", server.bucket(), "h1-match.bin");

    let canonical_headers = format!(
        "host:{}\nx-amz-content-sha256:{}\nx-amz-date:{}\n",
        host, payload_hash, timestamp
    );
    let signed_headers = "host;x-amz-content-sha256;x-amz-date";

    let canonical_request = format!(
        "PUT\n{}\n\n{}\n{}\n{}",
        path, canonical_headers, signed_headers, payload_hash
    );
    let canonical_request_hash = sha256_hex(canonical_request.as_bytes());
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{}\n{}",
        timestamp, credential_scope, canonical_request_hash
    );
    let signing_key = derive_signing_key("hash-ok-secret-1234567890", date, region, service);
    let signature = hex::encode(hmac_sha256(&signing_key, string_to_sign.as_bytes()));

    let auth_header = format!(
        "AWS4-HMAC-SHA256 Credential={}/{}, SignedHeaders={}, Signature={}",
        "hash-ok-key", credential_scope, signed_headers, signature
    );

    let resp = reqwest::Client::new()
        .put(format!("{}{}", endpoint, path))
        .header("authorization", auth_header)
        .header("x-amz-date", timestamp)
        .header("x-amz-content-sha256", &payload_hash)
        .header("host", host)
        .body(body.to_vec())
        .send()
        .await
        .expect("send signed body");
    assert_eq!(
        resp.status().as_u16(),
        200,
        "correctly-signed PUT must succeed, got {}",
        resp.status()
    );
}

// ============================================================================
// Adversarial: prove s3s is the AUTHORITATIVE signature rejector
// ----------------------------------------------------------------------------
// These forge a WELL-FORMED signature (valid AKID, valid canonicalization,
// wrong signature bytes) so the request passes parse + identity resolution and
// is rejected purely on the SIGNATURE COMPARISON. They lock in the invariant
// that the SigV4-dedup refactor rests on: a forged signature is 403'd on every
// shape. They must pass BOTH with the outer verifier present AND after it is
// removed (s3s alone). See docs/plan/sigv4-dedup.
// ============================================================================

/// Flip the last 8 hex chars of a 64-hex SigV4 signature so it stays
/// well-formed but cryptographically wrong.
fn tamper_signature(sig: &str) -> String {
    let keep = &sig[..sig.len().saturating_sub(8)];
    format!("{keep}deadbeef")
}

/// A header-signed GET with a TAMPERED signature must be 403 (the signature
/// comparison rejects, not the parse or the identity lookup).
#[tokio::test]
async fn test_forged_header_signature_rejected() {
    let server = TestServer::builder()
        .auth("testkey", "testsecret")
        .build()
        .await;
    let now = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let path = format!("/{}", server.bucket());
    let date = &now[..8];
    let region = "us-east-1";
    let host = server
        .endpoint()
        .strip_prefix("http://")
        .unwrap_or(&server.endpoint())
        .to_string();
    let scope = format!("{date}/{region}/s3/aws4_request");
    let payload_hash = "UNSIGNED-PAYLOAD";
    let canonical_headers =
        format!("host:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{now}\n");
    let signed_headers = "host;x-amz-content-sha256;x-amz-date";
    let canonical_request =
        format!("GET\n{path}\n\n{canonical_headers}\n{signed_headers}\n{payload_hash}");
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{now}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );
    let signing_key = derive_signing_key("testsecret", date, region, "s3");
    let good_sig = hex::encode(hmac_sha256(&signing_key, string_to_sign.as_bytes()));
    let forged = tamper_signature(&good_sig);
    let auth_header = format!(
        "AWS4-HMAC-SHA256 Credential=testkey/{scope}, SignedHeaders={signed_headers}, Signature={forged}"
    );

    let resp = reqwest::Client::new()
        .get(format!("{}{path}", server.endpoint()))
        .header("authorization", auth_header)
        .header("x-amz-date", &now)
        .header("x-amz-content-sha256", payload_hash)
        .header("host", host)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "a forged header signature must be rejected 403, got {}",
        resp.status()
    );
}

/// A presigned URL whose X-Amz-Signature is TAMPERED must be 403.
#[tokio::test]
async fn test_forged_presigned_signature_rejected() {
    let server = TestServer::builder()
        .auth("testkey", "testsecret")
        .build()
        .await;
    let client = server.s3_client_with_creds("testkey", "testsecret").await;
    // Mint a genuine presigned GET URL, then corrupt its signature param.
    let presigned = client
        .get_object()
        .bucket(server.bucket())
        .key("whatever.txt")
        .presigned(PresigningConfig::expires_in(Duration::from_secs(300)).unwrap())
        .await
        .expect("presign");
    let url = presigned.uri().to_string();
    // Replace the signature value with a well-formed-but-wrong one.
    let forged_url = {
        let re_key = "X-Amz-Signature=";
        let idx = url.find(re_key).expect("has signature param") + re_key.len();
        let end = url[idx..].find('&').map(|e| idx + e).unwrap_or(url.len());
        let mut u = url.clone();
        u.replace_range(idx..end, &"0".repeat(end - idx));
        u
    };
    let resp = reqwest::Client::new()
        .get(&forged_url)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "a forged presigned signature must be rejected 403, got {}",
        resp.status()
    );
}

/// Two `Authorization` headers carrying a known access key and garbage
/// signatures. s3s reads the header with `get_unique`, sees no unique
/// value, and treats the request as anonymous — so it verifies nothing.
/// The identity our middleware resolved from the FIRST header must not be
/// honoured without an s3s-verified signature for the same key.
#[tokio::test]
async fn test_duplicate_authorization_headers_do_not_bypass_signature() {
    let server = TestServer::builder()
        .auth("testkey", "testsecret")
        .build()
        .await;
    let client = server.s3_client_with_creds("testkey", "testsecret").await;
    client
        .put_object()
        .bucket(server.bucket())
        .key("secret.txt")
        .body(ByteStream::from_static(b"top secret"))
        .send()
        .await
        .expect("put");

    let forged = |sig: &str| {
        format!(
            "AWS4-HMAC-SHA256 Credential=testkey/20260101/us-east-1/s3/aws4_request, \
             SignedHeaders=host, Signature={sig}"
        )
    };
    let url = format!("{}/{}/secret.txt", server.endpoint(), server.bucket());

    let get = reqwest::Client::new()
        .get(&url)
        .header("authorization", forged("00"))
        .header("authorization", forged("11"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        get.status(),
        StatusCode::FORBIDDEN,
        "duplicate Authorization headers must not read the object"
    );

    let del = reqwest::Client::new()
        .delete(&url)
        .header("authorization", forged("22"))
        .header("authorization", forged("33"))
        .send()
        .await
        .unwrap();
    assert_eq!(del.status(), StatusCode::FORBIDDEN);
    client
        .get_object()
        .bucket(server.bucket())
        .key("secret.txt")
        .send()
        .await
        .expect("object must survive the forged DELETE");
}

/// A low-privilege user signs a SigV2 presigned query with their OWN secret
/// and adds a v4 `Authorization` header that names another user's key. s3s
/// verifies the SigV2 query (the attacker); our middleware resolved the
/// header (the victim). The two identities must match or the request fails.
#[tokio::test]
async fn test_sigv2_query_cannot_borrow_another_users_identity() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;
    let before = get_iam_version(&admin, &server.endpoint()).await;
    let boss = create_user(
        &admin,
        &server,
        "boss",
        vec![json!({"actions": ["*"], "resources": ["*"]})],
    )
    .await;
    let mallory = create_user(&admin, &server, "mallory", vec![]).await;
    wait_for_iam_rebuild(&admin, &server.endpoint(), before).await;

    server
        .s3_client_with_creds(&boss.access_key_id, &boss.secret_access_key)
        .await
        .put_object()
        .bucket(server.bucket())
        .key("secret.txt")
        .body(ByteStream::from_static(b"top secret"))
        .send()
        .await
        .expect("put");

    let path = format!("/{}/secret.txt", server.bucket());
    let expires = (chrono::Utc::now().timestamp() + 600).to_string();
    let string_to_sign = format!("GET\n\n\n{expires}\n{path}");
    let sig = {
        use base64::Engine as _;
        let key = ring::hmac::Key::new(
            ring::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY,
            mallory.secret_access_key.as_bytes(),
        );
        base64::engine::general_purpose::STANDARD
            .encode(ring::hmac::sign(&key, string_to_sign.as_bytes()))
    };
    let url = format!(
        "{}{path}?AWSAccessKeyId={}&Expires={expires}&Signature={}",
        server.endpoint(),
        mallory.access_key_id,
        urlencoding::encode(&sig)
    );
    let boss_header = format!(
        "AWS4-HMAC-SHA256 Credential={}/20260101/us-east-1/s3/aws4_request, \
         SignedHeaders=host, Signature=00",
        boss.access_key_id
    );
    let resp = reqwest::Client::new()
        .get(&url)
        .header("authorization", boss_header)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "a SigV2 query signed by mallory must not act as boss"
    );
}

/// An admin session stops working the moment its user is disabled or loses
/// admin rights. The session KIND is fixed at login; the gate re-checks the
/// principal against the live IAM index on every admin request.
#[tokio::test]
async fn test_admin_session_ends_when_user_is_disabled_or_demoted() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;
    let users_url = format!("{}/_/api/admin/users", server.endpoint());

    let login = |creds: UserCreds| {
        let endpoint = server.endpoint();
        async move {
            let jar = std::sync::Arc::new(reqwest::cookie::Jar::default());
            let client = reqwest::Client::builder()
                .cookie_provider(jar)
                .build()
                .unwrap();
            let resp = client
                .post(format!("{endpoint}/_/api/admin/login-as"))
                .json(&json!({
                    "access_key_id": creds.access_key_id,
                    "secret_access_key": creds.secret_access_key,
                }))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "login-as");
            client
        }
    };

    let before = get_iam_version(&admin, &server.endpoint()).await;
    let ops = create_user(
        &admin,
        &server,
        "ops",
        vec![json!({"actions": ["*"], "resources": ["*"]})],
    )
    .await;
    let ops2 = create_user(
        &admin,
        &server,
        "ops2",
        vec![json!({"actions": ["*"], "resources": ["*"]})],
    )
    .await;
    wait_for_iam_rebuild(&admin, &server.endpoint(), before).await;
    let ops_session = login(ops.clone()).await;
    let ops2_session = login(ops2.clone()).await;
    assert_eq!(
        ops_session.get(&users_url).send().await.unwrap().status(),
        StatusCode::OK
    );

    // Disable ops.
    let before = get_iam_version(&admin, &server.endpoint()).await;
    let resp = admin
        .put(format!("{users_url}/{}", ops.id))
        .json(&json!({"enabled": false}))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "disable: {}", resp.status());
    wait_for_iam_rebuild(&admin, &server.endpoint(), before).await;
    let backdoor = ops_session
        .post(&users_url)
        .json(&json!({"name": "backdoor", "permissions": [{"actions": ["*"], "resources": ["*"]}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        backdoor.status(),
        StatusCode::FORBIDDEN,
        "a disabled admin must lose the admin API"
    );

    // Demote ops2 to read-only.
    let before = get_iam_version(&admin, &server.endpoint()).await;
    let resp = admin
        .put(format!("{users_url}/{}", ops2.id))
        .json(&json!({"permissions": [{"actions": ["read"], "resources": ["*"]}]}))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "demote: {}", resp.status());
    wait_for_iam_rebuild(&admin, &server.endpoint(), before).await;
    assert_eq!(
        ops2_session.get(&users_url).send().await.unwrap().status(),
        StatusCode::FORBIDDEN,
        "a demoted admin must lose the admin API"
    );
}

/// Sign a request whose WIRE path/query differ from the canonical ones s3s
/// verifies (s3s canonicalises the decoded form, so an encoded wire target
/// still carries a valid signature).
#[allow(clippy::too_many_arguments)]
fn signed_encoded(
    method: reqwest::Method,
    endpoint: &str,
    wire_path: &str,
    canonical_path: &str,
    wire_query: &str,
    canonical_query: &str,
    access_key: &str,
    secret_key: &str,
) -> reqwest::RequestBuilder {
    let now = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let date = &now[..8];
    let host = endpoint.strip_prefix("http://").unwrap().to_string();
    let scope = format!("{date}/us-east-1/s3/aws4_request");
    let payload_hash = "UNSIGNED-PAYLOAD";
    let headers = format!("host:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{now}\n");
    let signed_headers = "host;x-amz-content-sha256;x-amz-date";
    let canonical_request = format!(
        "{}\n{canonical_path}\n{canonical_query}\n{headers}\n{signed_headers}\n{payload_hash}",
        method.as_str()
    );
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{now}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    );
    let key = derive_signing_key(secret_key, date, "us-east-1", "s3");
    let signature = hex::encode(hmac_sha256(&key, string_to_sign.as_bytes()));
    let url = if wire_query.is_empty() {
        format!("{endpoint}{wire_path}")
    } else {
        format!("{endpoint}{wire_path}?{wire_query}")
    };
    reqwest::Client::new()
        .request(method, url)
        .header(
            "authorization",
            format!(
                "AWS4-HMAC-SHA256 Credential={access_key}/{scope}, \
                 SignedHeaders={signed_headers}, Signature={signature}"
            ),
        )
        .header("x-amz-date", now)
        .header("x-amz-content-sha256", payload_hash)
}

/// Authorization must check the resource s3s serves. s3s decodes the whole
/// path, so `/bucket%2Fkey` is an object GET and `secre%74.txt` is
/// `secret.txt`.
#[tokio::test]
async fn test_percent_encoded_path_cannot_escape_authorization() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;
    let b = server.bucket().to_string();
    // Seed while still in bootstrap mode; IAM users replace the bootstrap key.
    server
        .s3_client()
        .await
        .put_object()
        .bucket(&b)
        .key("secret.txt")
        .body(ByteStream::from_static(b"top secret"))
        .send()
        .await
        .expect("put");
    let before = get_iam_version(&admin, &server.endpoint()).await;
    let lister = create_user(
        &admin,
        &server,
        "lister",
        vec![json!({"actions": ["list"], "resources": ["*"]})],
    )
    .await;
    let denied = create_user(
        &admin,
        &server,
        "denied",
        vec![
            json!({"actions": ["*"], "resources": ["*"]}),
            json!({"effect": "Deny", "actions": ["read"], "resources": [format!("{b}/secret*")]}),
        ],
    )
    .await;
    wait_for_iam_rebuild(&admin, &server.endpoint(), before).await;

    let get = |wire: String, user: &UserCreds| {
        signed_encoded(
            reqwest::Method::GET,
            &server.endpoint(),
            &wire,
            &format!("/{b}/secret.txt"),
            "",
            "",
            &user.access_key_id,
            &user.secret_access_key,
        )
        .send()
    };

    // Control: the plain paths are refused.
    assert_eq!(
        get(format!("/{b}/secret.txt"), &lister)
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        get(format!("/{b}/secret.txt"), &denied)
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
    // An encoded separator must not turn a list grant into a read.
    let resp = get(format!("/{b}%2Fsecret.txt"), &lister).await.unwrap();
    assert_ne!(resp.status(), StatusCode::OK, "list grant read the object");
    // An encoded key character must not escape a key-scoped Deny.
    let resp = get(format!("/{b}/secre%74.txt"), &denied).await.unwrap();
    assert_ne!(resp.status(), StatusCode::OK, "Deny bypassed by encoding");

    // Extra leading slashes on the key: the engine serves `/k` as `k`.
    for wire in [format!("/{b}//secret.txt"), format!("/{b}/%2Fsecret.txt")] {
        let resp = signed_encoded(
            reqwest::Method::GET,
            &server.endpoint(),
            &wire,
            &format!("/{b}//secret.txt"),
            "",
            "",
            &denied.access_key_id,
            &denied.secret_access_key,
        )
        .send()
        .await
        .unwrap();
        assert_ne!(resp.status(), StatusCode::OK, "Deny bypassed by {wire}");
    }

    // The same through CopyObject: s3s keeps `b//secret.txt` as key
    // `/secret.txt`, the engine reads `secret.txt`.
    for source in [format!("{b}//secret.txt"), format!("{b}/%2Fsecret.txt")] {
        let resp = signed_encoded(
            reqwest::Method::PUT,
            &server.endpoint(),
            &format!("/{b}/stolen.txt"),
            &format!("/{b}/stolen.txt"),
            "",
            "",
            &denied.access_key_id,
            &denied.secret_access_key,
        )
        .header("x-amz-copy-source", &source)
        .send()
        .await
        .unwrap();
        assert_ne!(
            resp.status(),
            StatusCode::OK,
            "Deny bypassed through x-amz-copy-source: {source}"
        );
    }
}

/// On the filesystem backend the OS path join resolves `.` and empty
/// segments, so `a/./secret.txt` and `a//secret.txt` would open the file of
/// `a/secret.txt` while IAM authorizes the literal text. The engine refuses
/// such keys on that backend, so a Deny on `a/secret*` cannot be escaped.
#[tokio::test]
async fn test_dot_and_empty_key_segments_cannot_escape_deny_on_filesystem() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;
    let b = server.bucket().to_string();
    // Seed while still in bootstrap mode; IAM users replace the bootstrap key.
    server
        .s3_client()
        .await
        .put_object()
        .bucket(&b)
        .key("a/secret.txt")
        .body(ByteStream::from_static(b"top secret"))
        .send()
        .await
        .expect("put");
    let before = get_iam_version(&admin, &server.endpoint()).await;
    let denied = create_user(
        &admin,
        &server,
        "denied",
        vec![
            json!({"actions": ["*"], "resources": ["*"]}),
            json!({"effect": "Deny", "actions": ["read", "write", "delete"],
                   "resources": [format!("{b}/a/secret*")]}),
        ],
    )
    .await;
    let reader = create_user(
        &admin,
        &server,
        "reader",
        vec![json!({"actions": ["*"], "resources": ["*"]})],
    )
    .await;
    wait_for_iam_rebuild(&admin, &server.endpoint(), before).await;
    let client = server
        .s3_client_with_creds(&denied.access_key_id, &denied.secret_access_key)
        .await;

    assert!(
        client
            .get_object()
            .bucket(&b)
            .key("a/secret.txt")
            .send()
            .await
            .is_err(),
        "control: the plain key is denied"
    );
    for alias in ["a/./secret.txt", "./a/secret.txt", "a//secret.txt"] {
        let got = client.get_object().bucket(&b).key(alias).send().await;
        assert!(got.is_err(), "Deny bypassed on GET by {alias}");
        let head = client.head_object().bucket(&b).key(alias).send().await;
        assert!(head.is_err(), "Deny bypassed on HEAD by {alias}");
        let put = client
            .put_object()
            .bucket(&b)
            .key(alias)
            .body(ByteStream::from_static(b"overwritten"))
            .send()
            .await;
        assert!(put.is_err(), "Deny bypassed on PUT by {alias}");
        let _ = client.delete_object().bucket(&b).key(alias).send().await;
    }
    // Listing through an alias must not show the denied names either.
    for prefix in ["a/./", "a//", "./a/"] {
        let listed = client
            .list_objects_v2()
            .bucket(&b)
            .prefix(prefix)
            .delimiter("/")
            .send()
            .await;
        assert!(listed.is_err(), "listing through {prefix} was served");
    }
    // The object is intact: not overwritten and not deleted through an alias.
    let body = server
        .s3_client_with_creds(&reader.access_key_id, &reader.secret_access_key)
        .await
        .get_object()
        .bucket(&b)
        .key("a/secret.txt")
        .send()
        .await
        .expect("reader GET")
        .body
        .collect()
        .await
        .unwrap()
        .into_bytes();
    assert_eq!(&body[..], b"top secret");
}

/// An SSE response is one request that can last for hours, so the per-request
/// admin gate alone does not cut it. The log stream must end soon after its
/// admin is disabled.
#[tokio::test]
async fn test_admin_log_stream_ends_when_admin_is_disabled() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;
    let before = get_iam_version(&admin, &server.endpoint()).await;
    let ops = create_user(
        &admin,
        &server,
        "ops",
        vec![json!({"actions": ["*"], "resources": ["*"]})],
    )
    .await;
    wait_for_iam_rebuild(&admin, &server.endpoint(), before).await;

    let jar = std::sync::Arc::new(reqwest::cookie::Jar::default());
    let ops_session = reqwest::Client::builder()
        .cookie_provider(jar)
        .build()
        .unwrap();
    let resp = ops_session
        .post(format!("{}/_/api/admin/login-as", server.endpoint()))
        .json(&json!({
            "access_key_id": ops.access_key_id,
            "secret_access_key": ops.secret_access_key,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "login-as");
    let mut stream = ops_session
        .get(format!("{}/_/api/admin/logs/stream", server.endpoint()))
        .send()
        .await
        .unwrap();
    assert_eq!(stream.status(), StatusCode::OK);

    // While ops is an admin, the stream stays open.
    let open = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while stream.chunk().await.unwrap().is_some() {}
    })
    .await;
    assert!(
        open.is_err(),
        "the stream ended while the session was valid"
    );

    let before = get_iam_version(&admin, &server.endpoint()).await;
    let resp = admin
        .put(format!(
            "{}/_/api/admin/users/{}",
            server.endpoint(),
            ops.id
        ))
        .json(&json!({"enabled": false}))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "disable: {}", resp.status());
    wait_for_iam_rebuild(&admin, &server.endpoint(), before).await;

    let ended = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        // Drain whatever was in flight; `None` (or a read error) = closed.
        while let Ok(Some(_)) = stream.chunk().await {}
    })
    .await;
    assert!(
        ended.is_ok(),
        "the log stream kept running after its admin was disabled"
    );
}

/// User names are unique, because `${iam:username}` expands to the name: a
/// create, clone or rename onto a name in use is refused with 409.
#[tokio::test]
async fn test_duplicate_user_names_are_refused() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;
    let users_url = format!("{}/_/api/admin/users", server.endpoint());
    let dana = create_user(&admin, &server, "dana", vec![]).await;
    let other = create_user(&admin, &server, "other", vec![]).await;

    let resp = admin
        .post(&users_url)
        .json(&json!({"name": "dana", "permissions": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT, "create");
    let resp = admin
        .put(format!("{users_url}/{}", other.id))
        .json(&json!({"name": "dana"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT, "rename");
    let resp = admin
        .post(format!("{users_url}/{}/clone", other.id))
        .json(&json!({"name": "dana"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CONFLICT, "clone");
    // Unchanged name on update is fine.
    let resp = admin
        .put(format!("{users_url}/{}", dana.id))
        .json(&json!({"name": "dana", "enabled": true}))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "same-name update: {}",
        resp.status()
    );
}

/// `$`-prefixed names are reserved: a rename to one is refused. A row that
/// already carries such a name stays editable when the name is unchanged.
#[tokio::test]
async fn test_rename_to_reserved_principal_name_is_refused() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;
    let alice = create_user(&admin, &server, "alice", vec![]).await;
    let url = format!("{}/_/api/admin/users/{}", server.endpoint(), alice.id);
    for name in ["$anonymous", "$bootstrap", "$x"] {
        let resp = admin
            .put(&url)
            .json(&json!({"name": name}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "rename to {name}");
    }
    let resp = admin
        .put(&url)
        .json(&json!({"name": "alice", "enabled": false}))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "plain update: {}",
        resp.status()
    );
}

/// A Deny on LIST with an `s3:prefix` condition must also match when the
/// query parameter NAME is percent-encoded (s3s decodes query keys).
#[tokio::test]
async fn test_percent_encoded_query_key_cannot_escape_prefix_deny() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;
    let b = server.bucket().to_string();
    // Seed while still in bootstrap mode; IAM users replace the bootstrap key.
    server
        .s3_client()
        .await
        .put_object()
        .bucket(&b)
        .key("secret/a.txt")
        .body(ByteStream::from_static(b"x"))
        .send()
        .await
        .expect("put");
    let before = get_iam_version(&admin, &server.endpoint()).await;
    let user = create_user(
        &admin,
        &server,
        "u",
        vec![
            json!({"actions": ["list"], "resources": [format!("{b}/*")]}),
            json!({"effect": "Deny", "actions": ["list"], "resources": [b.clone()],
                   "conditions": {"StringLike": {"s3:prefix": ["secret*"]}}}),
        ],
    )
    .await;
    wait_for_iam_rebuild(&admin, &server.endpoint(), before).await;

    let list = |wire_query: &str| {
        signed_encoded(
            reqwest::Method::GET,
            &server.endpoint(),
            &format!("/{b}"),
            &format!("/{b}"),
            wire_query,
            "list-type=2&prefix=secret",
            &user.access_key_id,
            &user.secret_access_key,
        )
        .send()
    };
    assert_eq!(
        list("list-type=2&prefix=secret").await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    let resp = list("list-type=2&%70refix=secret").await.unwrap();
    let status = resp.status();
    let body = resp.text().await.unwrap();
    assert!(
        !(status == StatusCode::OK && body.contains("secret/a.txt")),
        "prefix Deny bypassed by an encoded parameter name: {status} {body}"
    );
}

// ── review second pass (failing tests for findings) ──────────────────────

/// Review-2 (S19): ANY verified request resets the IP's failure counter, and
/// a presigned link is verified. So a wrong-secret loop that fetches one
/// public presigned link every MAX-1 guesses is never throttled.
#[tokio::test]
async fn review2_presigned_hits_do_not_reset_a_wrong_secret_loop() {
    use aws_sdk_s3::presigning::PresigningConfig;
    let server = TestServer::builder()
        .auth("testkey", "testsecret")
        .env("DGP_RATE_LIMIT_MAX_ATTEMPTS", "6")
        .env("DGP_RATE_LIMIT_WINDOW_SECS", "60")
        .env("DGP_RATE_LIMIT_LOCKOUT_SECS", "60")
        .build()
        .await;
    let client = server.s3_client().await;
    client
        .put_object()
        .bucket(server.bucket())
        .key("public.txt")
        .body(aws_sdk_s3::primitives::ByteStream::from_static(b"x"))
        .send()
        .await
        .unwrap();
    let presigned = client
        .get_object()
        .bucket(server.bucket())
        .key("public.txt")
        .presigned(
            PresigningConfig::builder()
                .expires_in(std::time::Duration::from_secs(300))
                .build()
                .unwrap(),
        )
        .await
        .unwrap();
    let path = format!("/{}", server.bucket());
    let http = reqwest::Client::new();
    // The presigned hit comes FIRST in each round: the sixth wrong secret
    // locks the IP, and a presigned GET after it would be refused too.
    for _ in 0..3 {
        let r = http
            .get(presigned.uri())
            .header("x-forwarded-for", "10.0.0.96")
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK, "presigned GET");
        for _ in 0..2 {
            let now = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
            let r = build_signed_get(&server.endpoint(), &path, "testkey", "wrong-secret", &now)
                .header("x-forwarded-for", "10.0.0.96")
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::FORBIDDEN);
        }
    }
    let now = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let resp = build_signed_get(&server.endpoint(), &path, "testkey", "testsecret", &now)
        .header("x-forwarded-for", "10.0.0.96")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        503,
        "six wrong secrets from one IP must lock it, presigned hits in between or not"
    );
}

// ============================================================================
// Production security defaults (replay window on, SSRF guard strict)
// ============================================================================

/// An SDK client for `server` with fast retries and one test interceptor.
fn retrying_client(
    server: &TestServer,
    interceptor: impl aws_sdk_s3::config::Intercept + 'static,
) -> aws_sdk_s3::Client {
    let conf = aws_sdk_s3::Config::builder()
        .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
        .region(aws_sdk_s3::config::Region::new("us-east-1"))
        .endpoint_url(server.endpoint())
        .credentials_provider(aws_credential_types::Credentials::new(
            common::TEST_ACCESS_KEY,
            common::TEST_SECRET_KEY,
            None,
            None,
            "test",
        ))
        .force_path_style(true)
        // A 10 ms backoff keeps the retry inside the first attempt's signing
        // second, so both attempts carry the SAME signature.
        .retry_config(
            aws_sdk_s3::config::retry::RetryConfig::standard()
                .with_max_attempts(3)
                .with_initial_backoff(Duration::from_millis(10)),
        )
        .interceptor(interceptor)
        .build();
    aws_sdk_s3::Client::from_conf(conf)
}

async fn get_body(server: &TestServer, key: &str) -> Vec<u8> {
    server
        .s3_client()
        .await
        .get_object()
        .bucket(server.bucket())
        .key(key)
        .send()
        .await
        .unwrap()
        .body
        .collect()
        .await
        .unwrap()
        .into_bytes()
        .to_vec()
}

/// Runs `before_retry` right before the SECOND attempt: the test clears the
/// server-side fault that made the first attempt fail.
struct OnRetry {
    attempts: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    before_retry: Box<dyn Fn() + Send + Sync>,
}

impl std::fmt::Debug for OnRetry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OnRetry")
    }
}

impl aws_sdk_s3::config::Intercept for OnRetry {
    fn name(&self) -> &'static str {
        "OnRetry"
    }

    fn read_before_attempt(
        &self,
        _context: &aws_sdk_s3::config::interceptors::BeforeTransmitInterceptorContextRef<'_>,
        _rc: &aws_sdk_s3::config::RuntimeComponents,
        _cfg: &mut aws_sdk_s3::config::ConfigBag,
    ) -> Result<(), aws_sdk_s3::error::BoxError> {
        if self
            .attempts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            == 1
        {
            (self.before_retry)();
        }
        Ok(())
    }
}

/// With the production replay window, an SDK retry after a server 5xx
/// succeeds. The first PutObject fails in the storage backend (a regular
/// file sits where the key's directory must go) and the proxy answers 500.
/// A failed mutation gives its replay-cache slot back (`replay_slot_kept`),
/// so the SDK's byte-identical retry is served, not refused as a replay.
#[tokio::test]
async fn production_defaults_sdk_retry_after_a_5xx_succeeds() {
    let server = TestServer::builder()
        .production_security_defaults()
        .build()
        .await;
    let blocker = server
        .data_dir()
        .expect("filesystem backend")
        .join(server.bucket())
        .join("deltaspaces");
    std::fs::create_dir_all(&blocker).unwrap();
    let blocker = blocker.join("blocked");
    std::fs::write(&blocker, b"not a directory").unwrap();
    let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let unblock = blocker.clone();
    let client = retrying_client(
        &server,
        OnRetry {
            attempts: attempts.clone(),
            before_retry: Box::new(move || std::fs::remove_file(&unblock).unwrap()),
        },
    );

    client
        .put_object()
        .bucket(server.bucket())
        .key("blocked/obj.txt")
        .body(ByteStream::from_static(b"retried payload"))
        .send()
        .await
        .expect("the SDK retry after a 500 must succeed");
    assert_eq!(
        attempts.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "one failed attempt, one retry"
    );
    assert_eq!(
        get_body(&server, "blocked/obj.txt").await,
        b"retried payload"
    );
}

/// Turns the FIRST attempt's response into a 500 after the server stored the
/// object: a gateway lost the response. The SDK retries on its own.
#[derive(Debug, Default)]
struct LoseFirstResponse {
    attempts: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl aws_sdk_s3::config::Intercept for LoseFirstResponse {
    fn name(&self) -> &'static str {
        "LoseFirstResponse"
    }

    fn modify_before_deserialization(
        &self,
        context: &mut aws_sdk_s3::config::interceptors::BeforeDeserializationInterceptorContextMut<
            '_,
        >,
        _rc: &aws_sdk_s3::config::RuntimeComponents,
        _cfg: &mut aws_sdk_s3::config::ConfigBag,
    ) -> Result<(), aws_sdk_s3::error::BoxError> {
        if self
            .attempts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            == 0
        {
            *context.response_mut().status_mut() = 500u16.try_into().expect("status");
        }
        Ok(())
    }
}

/// A PUT succeeds but its response is lost (a load balancer 502/504). The
/// SDK retries inside the same signing second, so the retry carries the
/// same signature. It must be served (real S3 accepts it), not refused as a
/// replay: the object is stored, and a refusal fails the client's upload.
#[tokio::test]
async fn production_defaults_sdk_retry_after_a_lost_response_succeeds() {
    let server = TestServer::builder()
        .production_security_defaults()
        .build()
        .await;
    let attempts = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let client = retrying_client(
        &server,
        LoseFirstResponse {
            attempts: attempts.clone(),
        },
    );
    client
        .put_object()
        .bucket(server.bucket())
        .key("lost.txt")
        .body(ByteStream::from_static(b"stored once"))
        .send()
        .await
        .expect("the SDK retry after a lost response must succeed");
    assert_eq!(get_body(&server, "lost.txt").await, b"stored once");
}

/// With the production SSRF default, a backend at a loopback http://
/// endpoint is refused, and the proxy never connects to it.
#[tokio::test]
async fn production_defaults_refuse_a_loopback_backend() {
    let server = TestServer::builder()
        .production_security_defaults()
        .build()
        .await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let accepted = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(3), listener.accept())
            .await
            .is_ok()
    });
    let admin = admin_http_client(&server.endpoint()).await;
    let resp = admin
        .post(format!("{}/_/api/admin/backends", server.endpoint()))
        .json(&json!({
            "name": "loopback",
            "type": "s3",
            "endpoint": format!("http://127.0.0.1:{port}"),
            "access_key_id": "k",
            "secret_access_key": "s",
        }))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    assert!(
        status.is_client_error(),
        "a loopback backend must be refused, got {status}: {body}"
    );
    assert!(
        !accepted.await.unwrap(),
        "the proxy connected to an SSRF-refused endpoint"
    );
}

// ============================================================================
// Boot: IAM users count as configured credentials
// ============================================================================

/// A declarative config whose `iam_users` are the only credentials boots:
/// the users are credentials. It used to exit "No authentication configured".
#[tokio::test]
async fn declarative_iam_users_without_a_bootstrap_pair_boot() {
    let server = TestServer::builder()
        .client_credentials_only("iac-boot-key", "iac-boot-secret-123")
        .extra_yaml_root(
            "iam_mode: declarative\n\
             iam_users:\n\
             \x20 - name: iac-boot\n\
             \x20   access_key_id: iac-boot-key\n\
             \x20   secret_access_key: iac-boot-secret-123\n\
             \x20   enabled: true\n\
             \x20   permissions:\n\
             \x20     - effect: Allow\n\
             \x20       actions: [\"*\"]\n\
             \x20       resources: [\"*\"]\n",
        )
        .build()
        .await;
    let s3 = server.s3_client().await;
    s3.put_object()
        .bucket(server.bucket())
        .key("boot.txt")
        .body(ByteStream::from_static(b"ok"))
        .send()
        .await
        .expect("the declarative user signs S3 requests");
}

/// GUI mode: once IAM users exist in the config DB, the bootstrap SigV4 pair
/// can go from the config and the proxy still boots, in IAM mode.
#[tokio::test]
async fn iam_db_users_without_a_bootstrap_pair_boot() {
    let mut server = TestServer::builder().build().await;
    let admin = admin_http_client(&server.endpoint()).await;
    let alice = create_user(
        &admin,
        &server,
        "alice-boot",
        vec![json!({"effect": "Allow", "actions": ["*"], "resources": ["*"]})],
    )
    .await;

    server.kill();
    let path = server.config_path().to_path_buf();
    let yaml = std::fs::read_to_string(&path).unwrap();
    let without_pair: String = yaml
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            !t.starts_with("access_key_id:") && !t.starts_with("secret_access_key:")
        })
        .map(|l| format!("{l}\n"))
        .collect();
    assert_ne!(yaml, without_pair, "the config carried a bootstrap pair");
    std::fs::write(&path, without_pair).unwrap();
    server.respawn_with_env(&[]).await;

    let s3 = server
        .s3_client_with_creds(&alice.access_key_id, &alice.secret_access_key)
        .await;
    s3.put_object()
        .bucket(server.bucket())
        .key("boot.txt")
        .body(ByteStream::from_static(b"ok"))
        .send()
        .await
        .expect("the DB user signs S3 requests");
}
