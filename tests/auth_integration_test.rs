//! Integration tests for authentication and authorization at the HTTP layer.
//!
//! Unlike `iam_test.rs` and `iam_authorization_test.rs` which test the permission
//! model through the AWS SDK, these tests exercise the actual SigV4 signing,
//! presigned URLs, clock skew, replay detection, rate limiting, and admin API
//! user lifecycle — verifying the auth *layer* as a black box.

mod common;

use aws_sdk_s3::presigning::PresigningConfig;
use aws_sdk_s3::primitives::ByteStream;
use common::{admin_http_client, get_iam_version, wait_for_iam_rebuild, TestServer};
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

/// Sending the exact same signed PUT request twice within 5 seconds should trigger replay detection.
/// Replay detection only applies to mutating methods (PUT/DELETE) — idempotent methods
/// (GET/HEAD) are exempt because clients legitimately retry them and sequential probes
/// (e.g. checksum auto-detection) can produce identical signatures.
#[tokio::test]
async fn test_replay_attack_detected() {
    let server = TestServer::builder()
        .auth("testkey", "testsecret")
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

/// Sending the same signed GET request twice should NOT trigger replay detection,
/// because idempotent methods are exempt. This prevents false rejections of
/// sequential HEAD probes (checksum auto-detection) and legitimate retries.
#[tokio::test]
async fn test_idempotent_requests_not_flagged_as_replay() {
    let server = TestServer::builder()
        .auth("testkey", "testsecret")
        .build()
        .await;

    let now = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let path = format!("/{}", server.bucket());

    // Send two identical GET requests with the same timestamp
    let resp1 = build_signed_get(&server.endpoint(), &path, "testkey", "testsecret", &now)
        .send()
        .await
        .unwrap();
    let resp2 = build_signed_get(&server.endpoint(), &path, "testkey", "testsecret", &now)
        .send()
        .await
        .unwrap();

    // Both should succeed — GET is idempotent, no replay rejection
    assert!(
        resp1.status().is_success() || resp1.status() == StatusCode::NOT_FOUND,
        "first GET should not be rejected, got {}",
        resp1.status()
    );
    assert!(
        resp2.status().is_success() || resp2.status() == StatusCode::NOT_FOUND,
        "second GET should not be flagged as replay, got {}",
        resp2.status()
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

/// Multiple rapid auth failures should trigger rate limiting (progressive delay or lockout).
#[tokio::test]
async fn test_brute_force_rate_limiting() {
    // Override rate limiter to small values for fast testing
    std::env::set_var("DGP_RATE_LIMIT_MAX_ATTEMPTS", "5");
    std::env::set_var("DGP_RATE_LIMIT_WINDOW_SECS", "60");
    std::env::set_var("DGP_RATE_LIMIT_LOCKOUT_SECS", "60");

    let server = TestServer::builder()
        .auth("testkey", "testsecret")
        .build()
        .await;

    let now = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();

    // Send rapid requests with wrong credentials from the same "IP"
    // (via X-Forwarded-For header, trusted because DGP_TRUST_PROXY_HEADERS=true in tests)
    // Rate limit overridden to 5 attempts for this test.
    let mut statuses = Vec::new();
    for i in 0..15 {
        let resp = build_signed_get(
            &server.endpoint(),
            &format!("/{}", server.bucket()),
            "testkey",
            &format!("wrong_secret_{}", i),
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
    let result = s3.list_objects_v2().bucket(server.bucket()).send().await;
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
