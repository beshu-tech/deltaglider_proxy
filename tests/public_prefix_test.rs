//! Integration tests for public prefix (unauthenticated read-only) access.

mod common;

use common::TestServer;

/// Helper: unauthenticated HTTP client (no SigV4, no cookies).
fn anon_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("anon client")
}

/// Upload an object using the authenticated S3 client.
async fn put_object(server: &TestServer, key: &str, body: &[u8]) {
    let client = server.s3_client().await;
    client
        .put_object()
        .bucket(server.bucket())
        .key(key)
        .body(aws_sdk_s3::primitives::ByteStream::from(body.to_vec()))
        .send()
        .await
        .expect("PUT failed");
}

const BUCKET: &str = "testpub";

fn builder() -> common::TestServerBuilder {
    TestServer::builder()
        .bucket(BUCKET)
        .auth("TESTKEY", "TESTSECRET")
        .bucket_policy(BUCKET, r#"public_prefixes = ["builds/"]"#)
}

// ═══════════════════════════════════════════════════
// Happy path
// ═══════════════════════════════════════════════════

#[tokio::test]
async fn test_public_get_object() {
    let server = builder().build().await;
    put_object(&server, "builds/v1.zip", b"hello-public").await;

    let resp = anon_client()
        .get(format!("{}/{}/builds/v1.zip", server.endpoint(), BUCKET))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.bytes().await.unwrap().as_ref(), b"hello-public");
}

#[tokio::test]
async fn test_public_head_object() {
    let server = builder().build().await;
    put_object(&server, "builds/v1.zip", b"hello").await;

    let resp = anon_client()
        .head(format!("{}/{}/builds/v1.zip", server.endpoint(), BUCKET))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test]
async fn test_public_list_with_prefix() {
    let server = builder().build().await;
    put_object(&server, "builds/v1.zip", b"data").await;
    put_object(&server, "builds/v2.zip", b"data2").await;

    let resp = anon_client()
        .get(format!(
            "{}/{}?list-type=2&prefix=builds/",
            server.endpoint(),
            BUCKET
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("builds/v1.zip"));
    assert!(body.contains("builds/v2.zip"));
}

#[tokio::test]
async fn test_public_list_subprefix() {
    let server = builder().build().await;
    put_object(&server, "builds/v2/file.zip", b"data").await;

    // Narrower prefix "builds/v2/" under public "builds/" → allowed
    let resp = anon_client()
        .get(format!(
            "{}/{}?list-type=2&prefix=builds/v2/",
            server.endpoint(),
            BUCKET
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("builds/v2/file.zip"));
}

// ═══════════════════════════════════════════════════
// Deny path (security-critical)
// ═══════════════════════════════════════════════════

#[tokio::test]
async fn test_public_get_outside_prefix() {
    let server = builder().build().await;
    put_object(&server, "secret/data.zip", b"secret").await;

    let resp = anon_client()
        .get(format!("{}/{}/secret/data.zip", server.endpoint(), BUCKET))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
}

#[tokio::test]
async fn test_public_put_denied() {
    let server = builder().build().await;

    // Anonymous PUT to public prefix → 403
    let resp = anon_client()
        .put(format!("{}/{}/builds/evil.zip", server.endpoint(), BUCKET))
        .body("evil data")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
}

#[tokio::test]
async fn test_public_delete_denied() {
    let server = builder().build().await;
    put_object(&server, "builds/v1.zip", b"data").await;

    let resp = anon_client()
        .delete(format!("{}/{}/builds/v1.zip", server.endpoint(), BUCKET))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
}

#[tokio::test]
async fn test_public_list_outside_prefix() {
    let server = builder().build().await;

    let resp = anon_client()
        .get(format!(
            "{}/{}?list-type=2&prefix=secret/",
            server.endpoint(),
            BUCKET
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
}

// ═══════════════════════════════════════════════════
// Boundary & edge cases
// ═══════════════════════════════════════════════════

#[tokio::test]
async fn test_public_prefix_boundary() {
    let server = TestServer::builder()
        .bucket(BUCKET)
        .auth("TESTKEY", "TESTSECRET")
        .bucket_policy(BUCKET, r#"public_prefixes = ["pub/"]"#)
        .build()
        .await;

    put_object(&server, "pub/file.txt", b"public").await;
    put_object(&server, "public/file.txt", b"not-public").await;

    // "pub/" matches
    let resp = anon_client()
        .get(format!("{}/{}/pub/file.txt", server.endpoint(), BUCKET))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // "public/" does NOT match "pub/"
    let resp = anon_client()
        .get(format!("{}/{}/public/file.txt", server.endpoint(), BUCKET))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
}

#[tokio::test]
async fn test_public_multiple_prefixes() {
    let server = TestServer::builder()
        .bucket(BUCKET)
        .auth("TESTKEY", "TESTSECRET")
        .bucket_policy(BUCKET, r#"public_prefixes = ["builds/", "docs/"]"#)
        .build()
        .await;

    put_object(&server, "builds/v1.zip", b"build").await;
    put_object(&server, "docs/readme.md", b"doc").await;
    put_object(&server, "secret/key", b"secret").await;

    // Both public prefixes accessible
    let resp = anon_client()
        .get(format!("{}/{}/builds/v1.zip", server.endpoint(), BUCKET))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let resp = anon_client()
        .get(format!("{}/{}/docs/readme.md", server.endpoint(), BUCKET))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Non-public path denied
    let resp = anon_client()
        .get(format!("{}/{}/secret/key", server.endpoint(), BUCKET))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
}

#[tokio::test]
async fn test_public_list_no_prefix_denied() {
    // Anonymous LIST without prefix must NOT leak all keys (Finding 3 regression test)
    let server = builder().build().await;
    put_object(&server, "builds/v1.zip", b"data").await;
    put_object(&server, "secret/key.txt", b"secret").await;

    // LIST with no prefix → 403 (not 200 with all keys)
    let resp = anon_client()
        .get(format!("{}/{}?list-type=2", server.endpoint(), BUCKET))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        403,
        "Anonymous LIST without prefix should be denied"
    );
}

#[tokio::test]
async fn test_public_with_auth_still_works() {
    let server = builder().build().await;
    put_object(&server, "builds/v1.zip", b"data").await;

    // Authenticated GET on public path should work via normal IAM
    let client = server.s3_client().await;
    let result = client
        .get_object()
        .bucket(BUCKET)
        .key("builds/v1.zip")
        .send()
        .await;
    assert!(result.is_ok());
}
