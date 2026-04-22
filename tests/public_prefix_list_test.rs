//! HTTP-layer integration tests for anonymous LIST against public prefixes.
//!
//! Regression coverage for the AWS-parity bug where
//! `aws s3 ls s3://bucket/ror/libs` (no trailing slash) returned
//! AccessDenied against a public-prefix-configured bucket. The
//! unit tests in `src/api/auth.rs` verify the IAM policy's match
//! shape; these tests verify the whole request path — admission →
//! anonymous principal mint → IAM evaluate → backend LIST → XML
//! response filtering — through a spawned proxy and real HTTP.

mod common;

use common::TestServer;
use reqwest::StatusCode;

/// Spawn a proxy with `bucket/public-prefix/` set as public, seed two
/// objects under the public prefix and one control object outside it.
/// Returns the server + an anonymous HTTP client.
async fn server_with_public_prefix(
    bucket: &str,
    public_prefix: &str,
) -> (TestServer, reqwest::Client) {
    let server = TestServer::builder()
        .bucket(bucket)
        .auth("admin", "admin-secret-1234567890")
        .bucket_policy(bucket, &format!("public_prefixes = [\"{public_prefix}\"]"))
        .build()
        .await;

    // Seed three objects: two under the public prefix, one outside.
    // Use a signed put through the admin SigV4 creds.
    let signed = aws_sdk_s3::Client::from_conf(
        aws_sdk_s3::Config::builder()
            .credentials_provider(aws_sdk_s3::config::Credentials::new(
                "admin",
                "admin-secret-1234567890",
                None,
                None,
                "test",
            ))
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .endpoint_url(server.endpoint())
            .force_path_style(true)
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .build(),
    );

    for key in [
        // Matches the public prefix at top level.
        format!("{public_prefix}alpha.txt"),
        // Matches deeper.
        format!("{public_prefix}sub/beta.txt"),
        // OUTSIDE the public prefix. Must never leak in anon LIST.
        "private/secret.txt".to_string(),
    ] {
        signed
            .put_object()
            .bucket(bucket)
            .key(&key)
            .body(aws_sdk_s3::primitives::ByteStream::from_static(b"hi"))
            .send()
            .await
            .unwrap_or_else(|e| panic!("seed PUT {key} failed: {e:?}"));
    }

    let anon = reqwest::Client::new();
    (server, anon)
}

/// Parse the XML LIST response body into a flat list of keys + common prefixes.
fn parse_list_xml(body: &str) -> (Vec<String>, Vec<String>) {
    // Minimal regex-based parse — good enough for these tests. Real
    // clients use aws-sdk-s3's XML parser; we want a dependency-free
    // check here.
    let keys: Vec<String> = body
        .split("<Key>")
        .skip(1)
        .filter_map(|s| s.split("</Key>").next())
        .map(String::from)
        .collect();
    let prefixes: Vec<String> = body
        .split("<Prefix>")
        .skip(1)
        .filter_map(|s| s.split("</Prefix>").next())
        // First <Prefix> element is the request echo, subsequent ones
        // (inside CommonPrefixes blocks) are directory entries.
        .skip(1)
        .map(String::from)
        .collect();
    (keys, prefixes)
}

/// Anonymous LIST with `prefix=ror/libs` (no slash) against public
/// prefix `ror/libs/` must return 200 and only keys under that
/// prefix — the AWS-compatible behaviour.
#[tokio::test]
async fn anonymous_list_no_slash_parent_allowed() {
    let (server, anon) = server_with_public_prefix("testbk", "ror/libs/").await;

    let resp = anon
        .get(format!(
            "{}/testbk/?list-type=2&prefix=ror/libs",
            server.endpoint()
        ))
        .send()
        .await
        .expect("HTTP send");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "prefix=ror/libs (no slash) must return 200, got {}",
        resp.status()
    );
    let body = resp.text().await.unwrap();
    let (keys, _prefixes) = parse_list_xml(&body);
    assert!(
        keys.iter().any(|k| k == "ror/libs/alpha.txt"),
        "expected alpha.txt in listing, got keys={keys:?}"
    );
    assert!(
        !keys.iter().any(|k| k == "private/secret.txt"),
        "private key leaked into anonymous listing: {keys:?}"
    );
}

/// Trailing-slash form — must also work (regression guard).
#[tokio::test]
async fn anonymous_list_trailing_slash_allowed() {
    let (server, anon) = server_with_public_prefix("testbk", "ror/libs/").await;

    let resp = anon
        .get(format!(
            "{}/testbk/?list-type=2&prefix=ror/libs/",
            server.endpoint()
        ))
        .send()
        .await
        .expect("HTTP send");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.text().await.unwrap();
    let (keys, _) = parse_list_xml(&body);
    assert!(keys.iter().any(|k| k == "ror/libs/alpha.txt"));
    assert!(keys.iter().any(|k| k == "ror/libs/sub/beta.txt"));
}

/// Delimiter form (what `aws s3 ls` actually sends) — delimiter
/// collapses children into CommonPrefixes. Must work both with and
/// without trailing slash on the prefix.
#[tokio::test]
async fn anonymous_list_delimiter_no_slash_parent() {
    let (server, anon) = server_with_public_prefix("testbk", "ror/libs/").await;

    let resp = anon
        .get(format!(
            "{}/testbk/?list-type=2&prefix=ror/libs&delimiter=/",
            server.endpoint()
        ))
        .send()
        .await
        .expect("HTTP send");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "delimiter LIST with no-slash parent must succeed"
    );
}

/// Deeper no-slash form (`prefix=ror/libs/sub`) also works — the
/// public prefix is a genuine ancestor, so the narrow-match logic
/// must not care how many path segments deep the request is.
#[tokio::test]
async fn anonymous_list_deep_no_slash_allowed() {
    let (server, anon) = server_with_public_prefix("testbk", "ror/libs/").await;

    let resp = anon
        .get(format!(
            "{}/testbk/?list-type=2&prefix=ror/libs/sub",
            server.endpoint()
        ))
        .send()
        .await
        .expect("HTTP send");
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.text().await.unwrap();
    let (keys, _) = parse_list_xml(&body);
    assert!(keys.iter().any(|k| k == "ror/libs/sub/beta.txt"));
}

/// Security: `ror/libsomething` is a false parent — the public prefix
/// `ror/libs/` does NOT authorise it. Must be denied even though it
/// starts with the same characters as the public prefix up to the
/// final `/`.
#[tokio::test]
async fn anonymous_list_false_parent_denied() {
    let (server, anon) = server_with_public_prefix("testbk", "ror/libs/").await;

    let resp = anon
        .get(format!(
            "{}/testbk/?list-type=2&prefix=ror/libsomething",
            server.endpoint()
        ))
        .send()
        .await
        .expect("HTTP send");
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "false-parent prefix must not bypass public-prefix scope"
    );
    // Belt-and-braces: even if a regression turned this 403 into a
    // 200-with-empty-body, verify the response doesn't leak any key.
    let body = resp.text().await.unwrap_or_default();
    let (keys, _) = parse_list_xml(&body);
    assert!(
        keys.is_empty(),
        "false-parent denial must not leak any key (got {keys:?})"
    );
    assert!(
        body.contains("AccessDenied") || body.is_empty(),
        "403 body should carry AccessDenied XML; got: {body}"
    );
}

/// Security: unrelated prefix in the same bucket — classic deny.
#[tokio::test]
async fn anonymous_list_unrelated_prefix_denied() {
    let (server, anon) = server_with_public_prefix("testbk", "ror/libs/").await;

    let resp = anon
        .get(format!(
            "{}/testbk/?list-type=2&prefix=private/",
            server.endpoint()
        ))
        .send()
        .await
        .expect("HTTP send");
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let body = resp.text().await.unwrap_or_default();
    let (keys, _) = parse_list_xml(&body);
    assert!(
        keys.is_empty(),
        "unrelated-prefix denial must not leak any key"
    );
    assert!(!body.contains("private/secret.txt"));
}

/// Security: no prefix at all ("list the whole bucket") against a
/// bucket with only a partial public prefix — must deny. The anon
/// principal doesn't have permission to see what's outside the
/// authorised subtree.
#[tokio::test]
async fn anonymous_list_bucket_root_denied_when_only_partial_public() {
    let (server, anon) = server_with_public_prefix("testbk", "ror/libs/").await;

    let resp = anon
        .get(format!("{}/testbk/?list-type=2", server.endpoint()))
        .send()
        .await
        .expect("HTTP send");
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "bucket-root LIST with partial public prefix must be denied (would leak non-public keys)"
    );
    let body = resp.text().await.unwrap_or_default();
    let (keys, _) = parse_list_xml(&body);
    assert!(
        keys.is_empty() && !body.contains("private/secret.txt"),
        "bucket-root denial must not leak any key (got body of {} bytes, keys={keys:?})",
        body.len()
    );
}

/// Entire bucket public (`public: true` shorthand → `public_prefixes: [""]`).
/// Every anonymous LIST form must succeed — with a prefix, without,
/// delimiter or not.
#[tokio::test]
async fn anonymous_list_entire_bucket_public() {
    let (server, anon) = server_with_public_prefix("testbk", "").await;

    // Empty prefix
    let r1 = anon
        .get(format!("{}/testbk/?list-type=2", server.endpoint()))
        .send()
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    // Specific prefix
    let r2 = anon
        .get(format!(
            "{}/testbk/?list-type=2&prefix=private/",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r2.status(), StatusCode::OK);
}

/// Multiple public prefixes in the same bucket — each one must be
/// independently listable by its no-slash parent form, and a false
/// parent for one doesn't silently bleed into the other.
#[tokio::test]
async fn anonymous_list_multiple_public_prefixes() {
    let server = TestServer::builder()
        .bucket("multibk")
        .auth("admin", "admin-secret-1234567890")
        .bucket_policy("multibk", "public_prefixes = [\"releases/\", \"docs/\"]")
        .build()
        .await;

    let signed = aws_sdk_s3::Client::from_conf(
        aws_sdk_s3::Config::builder()
            .credentials_provider(aws_sdk_s3::config::Credentials::new(
                "admin",
                "admin-secret-1234567890",
                None,
                None,
                "test",
            ))
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .endpoint_url(server.endpoint())
            .force_path_style(true)
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .build(),
    );
    for key in ["releases/v1.zip", "docs/readme.md", "private/secret.txt"] {
        signed
            .put_object()
            .bucket("multibk")
            .key(key)
            .body(aws_sdk_s3::primitives::ByteStream::from_static(b"x"))
            .send()
            .await
            .unwrap();
    }

    let anon = reqwest::Client::new();

    // Each public-prefix parent form succeeds independently.
    for prefix in ["releases", "docs"] {
        let r = anon
            .get(format!(
                "{}/multibk/?list-type=2&prefix={prefix}",
                server.endpoint()
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK, "prefix={prefix}");
    }
    // False parent in multi-prefix buckets still denies.
    let r_false = anon
        .get(format!(
            "{}/multibk/?list-type=2&prefix=releasesleaks",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r_false.status(), StatusCode::FORBIDDEN);
    let body = r_false.text().await.unwrap_or_default();
    let (keys, _) = parse_list_xml(&body);
    assert!(keys.is_empty(), "multi-prefix false-parent must not leak");
    assert!(!body.contains("private/secret.txt"));
}

/// An authenticated admin still lists the full bucket regardless of
/// public-prefix config — the fix must not break the normal path.
#[tokio::test]
async fn authenticated_admin_sees_full_bucket_regardless_of_public_prefix() {
    let (server, _anon) = server_with_public_prefix("testbk", "ror/libs/").await;

    let signed = aws_sdk_s3::Client::from_conf(
        aws_sdk_s3::Config::builder()
            .credentials_provider(aws_sdk_s3::config::Credentials::new(
                "admin",
                "admin-secret-1234567890",
                None,
                None,
                "test",
            ))
            .region(aws_sdk_s3::config::Region::new("us-east-1"))
            .endpoint_url(server.endpoint())
            .force_path_style(true)
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .build(),
    );
    let out = signed
        .list_objects_v2()
        .bucket("testbk")
        .send()
        .await
        .expect("admin LIST should succeed");
    let keys: Vec<String> = out
        .contents()
        .iter()
        .map(|o| o.key().unwrap_or("").to_string())
        .collect();
    assert!(keys.iter().any(|k| k == "ror/libs/alpha.txt"));
    assert!(keys.iter().any(|k| k == "private/secret.txt"));
}
