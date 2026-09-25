// SPDX-License-Identifier: BUSL-1.1

//! HTTP-layer integration tests for anonymous LIST against public prefixes.
//!
//! Regression coverage for the AWS-parity bug where
//! `aws s3 ls s3://bucket/ror/libs` (no trailing slash) returned
//! AccessDenied against a public-prefix-configured bucket. The
//! unit tests in `src/api/auth.rs` verify the IAM policy's match
//! shape; these tests verify the whole request path — admission →
//! anonymous principal mint → IAM evaluate → backend LIST → XML
//! response filtering — through a spawned proxy and real HTTP.
//!
//! ## QA trim (2026-04-23)
//!
//! This file originally held 10 HTTP-layer tests; the unit tests in
//! `src/api/auth.rs` + `src/admission/evaluator.rs` already cover
//! the policy shape exhaustively, so the cases here are deliberately
//! narrow:
//!
//!   1. No-slash parent — the ACTUAL reported bug + its fix.
//!   2. False-parent denial — security invariant (no substring leak).
//!   3. Partial-public root denial — security invariant (no scope bleed).
//!   4. Admin still sees everything — "the fix didn't break normal path".
//!
//! The removed cases (trailing-slash, deep-prefix, entire-bucket,
//! delimiter variants, multi-prefix, unrelated-prefix) were all
//! variations on the same pattern-match logic already covered by
//! unit tests. Adding them back at this layer only earns its cost
//! on a genuinely new request-pipeline seam.

use crate::common;

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
        .bucket_policy(bucket, &format!("public_prefixes: [\"{public_prefix}\"]"))
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

/// Object metadata for an anonymous reader must not carry the proxy's
/// `dg-tool = deltaglider_proxy/<version>` stamp — it names the exact build.
/// A signed principal keeps the full provenance.
#[tokio::test]
async fn anonymous_object_metadata_has_no_tool_stamp() {
    let (server, anon) = server_with_public_prefix("testbk", "ror/libs/").await;
    let url = format!("{}/testbk/ror/libs/alpha.txt", server.endpoint());

    for method in [reqwest::Method::HEAD, reqwest::Method::GET] {
        let resp = anon.request(method.clone(), &url).send().await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{method}");
        assert!(
            resp.headers().get("x-amz-meta-dg-tool").is_none(),
            "{method} leaked dg-tool to an anonymous caller"
        );
        assert!(
            resp.headers().get("x-amz-meta-dg-file-size").is_some(),
            "{method} must keep the rest of the metadata"
        );
    }

    let list = anon
        .get(format!(
            "{}/testbk?list-type=2&prefix=ror/libs/&metadata=true",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(list.status(), StatusCode::OK);
    assert!(
        !list.text().await.unwrap().contains("dg-tool"),
        "metadata=true LIST leaked dg-tool to an anonymous caller"
    );

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
    let head = signed
        .head_object()
        .bucket("testbk")
        .key("ror/libs/alpha.txt")
        .send()
        .await
        .unwrap();
    let tool = head
        .metadata()
        .and_then(|m| m.get("dg-tool"))
        .expect("a signed caller keeps the dg-tool provenance");
    assert!(tool.starts_with("deltaglider_proxy/"), "{tool}");

    // A presigned URL authenticates as the signer, but whoever holds the
    // link is an anonymous party (the docs recommend presigned links for
    // third parties). The link holder must not get the provenance either.
    let presigned = signed
        .get_object()
        .bucket("testbk")
        .key("ror/libs/alpha.txt")
        .presigned(
            aws_sdk_s3::presigning::PresigningConfig::expires_in(std::time::Duration::from_secs(
                300,
            ))
            .unwrap(),
        )
        .await
        .unwrap();
    let resp = anon.get(presigned.uri()).send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "presigned GET must work");
    assert!(
        resp.headers().get("x-amz-meta-dg-tool").is_none(),
        "presigned GET leaked dg-tool to the link holder"
    );
}

/// Review S5: object bytes share the admin UI's origin. An uploaded HTML page
/// must reach a browser sandboxed, and an anonymous caller must not relabel a
/// public object with `response-content-type` (S3 refuses that with 400).
#[tokio::test]
async fn get_object_is_sandboxed_and_anonymous_overrides_are_refused() {
    let (server, anon) = server_with_public_prefix("testbk", "ror/libs/").await;
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
    signed
        .put_object()
        .bucket("testbk")
        .key("ror/libs/page.html")
        .content_type("text/html")
        .body(aws_sdk_s3::primitives::ByteStream::from_static(
            b"<script>alert(document.cookie)</script>",
        ))
        .send()
        .await
        .expect("seed html");

    let resp = anon
        .get(format!("{}/testbk/ror/libs/page.html", server.endpoint()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get("content-security-policy")
            .and_then(|v| v.to_str().ok()),
        Some("sandbox"),
        "an uploaded HTML page must be served sandboxed"
    );
    assert_eq!(
        resp.headers()
            .get("x-content-type-options")
            .and_then(|v| v.to_str().ok()),
        Some("nosniff")
    );

    let relabel = anon
        .get(format!(
            "{}/testbk/ror/libs/alpha.txt?response-content-type=text/html",
            server.endpoint()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        relabel.status(),
        StatusCode::BAD_REQUEST,
        "anonymous response-* override must be refused"
    );
}
