//! Integration tests for IAM multi-user authentication and authorization.
//!
//! Tests verify that:
//! - Multiple users can authenticate with different credentials
//! - Permissions are enforced (read/write/delete/list per bucket)
//! - Disabled users are rejected
//! - Legacy single-credential mode still works
//! - Open access mode (no auth) still works

mod common;

use common::TestServer;

/// Open access mode: no auth configured → all operations succeed without credentials.
#[tokio::test]
async fn test_open_access_no_auth_required() {
    let server = TestServer::filesystem().await;
    let client = reqwest::Client::new();

    // PUT without any auth headers
    let url = format!("{}/{}/open/file.txt", server.endpoint(), server.bucket());
    let resp = client
        .put(&url)
        .body(b"open access data".to_vec())
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "PUT should succeed in open access mode, got {}",
        resp.status()
    );

    // GET without auth
    let resp = client.get(&url).send().await.unwrap();
    assert!(resp.status().is_success());
    let body = resp.bytes().await.unwrap();
    assert_eq!(body.as_ref(), b"open access data");
}

/// Legacy mode: single credential pair → all operations succeed with correct creds.
#[tokio::test]
async fn test_legacy_single_credential_auth() {
    let server = TestServer::builder()
        .auth("testkey", "testsecret")
        .build()
        .await;
    let client = server.s3_client_with_creds("testkey", "testsecret").await;

    // PUT with correct credentials
    client
        .put_object()
        .bucket(server.bucket())
        .key("legacy/file.txt")
        .body(aws_sdk_s3::primitives::ByteStream::from(
            b"legacy auth data".to_vec(),
        ))
        .send()
        .await
        .expect("PUT with correct legacy creds should succeed");

    // GET with correct credentials
    let result = client
        .get_object()
        .bucket(server.bucket())
        .key("legacy/file.txt")
        .send()
        .await
        .expect("GET with correct legacy creds should succeed");

    let body = result.body.collect().await.unwrap().into_bytes();
    assert_eq!(body.as_ref(), b"legacy auth data");
}

/// Legacy mode: wrong credentials → 403.
#[tokio::test]
async fn test_legacy_wrong_credentials_rejected() {
    let server = TestServer::builder()
        .auth("testkey", "testsecret")
        .build()
        .await;
    let client = server.s3_client_with_creds("wrongkey", "wrongsecret").await;

    let result = client
        .list_objects_v2()
        .bucket(server.bucket())
        .send()
        .await;

    assert!(
        result.is_err(),
        "Request with wrong credentials should be rejected"
    );
}
