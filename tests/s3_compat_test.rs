// SPDX-License-Identifier: GPL-3.0-only

//! S3 API compatibility tests
//!
//! Phase 1: Range requests, conditional headers, Content-MD5 validation,
//! CopyObject metadata directive, per-request UUID, and Accept-Ranges.
//! Phase 2: ACL stubs, response overrides, bucket naming validation,
//! and real ListBuckets creation dates. Uses reqwest for raw HTTP
//! to verify XML responses and header values.

mod common;

use base64::Engine;
use common::{
    admin_http_client, generate_binary, mutate_binary, put_and_get_storage_type, put_object,
    test_setup, upload_test_data, TestServer,
};
use hmac::{Hmac, Mac};
use sha2::Sha256;

// ============================================================================
// 1.5 Per-Request UUID + Accept-Ranges
// ============================================================================

#[tokio::test]
async fn test_success_response_has_request_id() {
    let server = TestServer::filesystem().await;
    let client = reqwest::Client::new();
    let endpoint = server.endpoint();
    let bucket = server.bucket();

    put_object(
        &client,
        &endpoint,
        bucket,
        "reqid.txt",
        b"hello".to_vec(),
        "text/plain",
    )
    .await;

    let resp = client
        .get(format!("{}/{}/reqid.txt", endpoint, bucket))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());

    let request_id = resp
        .headers()
        .get("x-amz-request-id")
        .expect("x-amz-request-id should be present on success responses");
    let id_str = request_id.to_str().unwrap();
    assert_eq!(id_str.len(), 36, "request-id should be a UUID: {}", id_str);
    assert!(
        id_str.contains('-'),
        "request-id should be hyphenated UUID: {}",
        id_str
    );
}

#[tokio::test]
async fn test_error_response_has_unique_request_id() {
    let server = TestServer::filesystem().await;
    let client = reqwest::Client::new();
    let endpoint = server.endpoint();
    let bucket = server.bucket();

    let resp = client
        .get(format!("{}/{}/nonexistent.txt", endpoint, bucket))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 404);

    let request_id = resp
        .headers()
        .get("x-amz-request-id")
        .expect("x-amz-request-id should be present on error responses");
    let id_str = request_id.to_str().unwrap();
    assert_eq!(id_str.len(), 36, "request-id should be a UUID: {}", id_str);

    let body = resp.text().await.unwrap();
    assert!(
        body.contains("<RequestId>"),
        "Error XML should contain RequestId"
    );
    assert!(
        !body.contains("00000000-0000-0000-0000-000000000000"),
        "RequestId should not be the hardcoded zero UUID"
    );
}

#[tokio::test]
async fn test_request_ids_are_unique_across_requests() {
    let server = TestServer::filesystem().await;
    let client = reqwest::Client::new();
    let endpoint = server.endpoint();
    let bucket = server.bucket();

    put_object(
        &client,
        &endpoint,
        bucket,
        "unique.txt",
        b"data".to_vec(),
        "text/plain",
    )
    .await;

    let resp1 = client
        .get(format!("{}/{}/unique.txt", endpoint, bucket))
        .send()
        .await
        .unwrap();
    let id1 = resp1
        .headers()
        .get("x-amz-request-id")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    let resp2 = client
        .get(format!("{}/{}/unique.txt", endpoint, bucket))
        .send()
        .await
        .unwrap();
    let id2 = resp2
        .headers()
        .get("x-amz-request-id")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    assert_ne!(id1, id2, "Each request should get a unique request ID");
}

#[tokio::test]
async fn test_head_has_accept_ranges_bytes() {
    let server = TestServer::filesystem().await;
    let client = reqwest::Client::new();
    let endpoint = server.endpoint();
    let bucket = server.bucket();

    put_object(
        &client,
        &endpoint,
        bucket,
        "ar.txt",
        b"data".to_vec(),
        "text/plain",
    )
    .await;

    let resp = client
        .head(format!("{}/{}/ar.txt", endpoint, bucket))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());

    let accept_ranges = resp
        .headers()
        .get("accept-ranges")
        .expect("Accept-Ranges should be present on HEAD responses");
    assert_eq!(accept_ranges.to_str().unwrap(), "bytes");
}

// ============================================================================
// 1.3 Content-MD5 Validation
// ============================================================================

#[tokio::test]
async fn test_put_with_correct_content_md5() {
    let server = TestServer::filesystem().await;
    let client = reqwest::Client::new();
    let endpoint = server.endpoint();
    let bucket = server.bucket();

    let data = b"hello world";
    use md5::Digest;
    let md5_hash = md5::Md5::digest(data);
    use base64::Engine;
    let md5_b64 = base64::engine::general_purpose::STANDARD.encode(md5_hash);

    let resp = client
        .put(format!("{}/{}/md5ok.txt", endpoint, bucket))
        .header("content-md5", &md5_b64)
        .header("content-type", "text/plain")
        .body(data.to_vec())
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "PUT with correct Content-MD5 should succeed: {}",
        resp.status()
    );
}

#[tokio::test]
async fn test_put_with_wrong_content_md5_returns_bad_digest() {
    let server = TestServer::filesystem().await;
    let client = reqwest::Client::new();
    let endpoint = server.endpoint();
    let bucket = server.bucket();

    let data = b"hello world";
    use base64::Engine;
    let wrong_md5 = base64::engine::general_purpose::STANDARD.encode(b"0000000000000000");

    let resp = client
        .put(format!("{}/{}/md5bad.txt", endpoint, bucket))
        .header("content-md5", &wrong_md5)
        .header("content-type", "text/plain")
        .body(data.to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        400,
        "PUT with wrong Content-MD5 should return 400"
    );

    let body = resp.text().await.unwrap();
    assert!(
        body.contains("BadDigest"),
        "Error should be BadDigest: {}",
        body
    );
}

#[tokio::test]
async fn test_put_without_content_md5_succeeds() {
    let server = TestServer::filesystem().await;
    let client = reqwest::Client::new();
    let endpoint = server.endpoint();
    let bucket = server.bucket();

    let resp = client
        .put(format!("{}/{}/nomd5.txt", endpoint, bucket))
        .header("content-type", "text/plain")
        .body("no md5 header")
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "PUT without Content-MD5 should succeed"
    );
}

// ============================================================================
// 1.4 CopyObject Metadata Directive
// ============================================================================

#[tokio::test]
async fn test_copy_default_copies_metadata() {
    let server = TestServer::filesystem().await;
    let client = reqwest::Client::new();
    let endpoint = server.endpoint();
    let bucket = server.bucket();

    client
        .put(format!("{}/{}/copy-src.txt", endpoint, bucket))
        .header("content-type", "text/plain")
        .header("x-amz-meta-custom-key", "custom-value")
        .body("source data")
        .send()
        .await
        .unwrap();

    let resp = client
        .put(format!("{}/{}/copy-dst.txt", endpoint, bucket))
        .header("x-amz-copy-source", format!("{}/copy-src.txt", bucket))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());

    let get_resp = client
        .get(format!("{}/{}/copy-dst.txt", endpoint, bucket))
        .send()
        .await
        .unwrap();
    assert_eq!(
        get_resp
            .headers()
            .get("x-amz-meta-custom-key")
            .and_then(|v| v.to_str().ok()),
        Some("custom-value"),
        "Custom metadata should be copied by default"
    );
}

#[tokio::test]
async fn test_copy_replace_uses_request_metadata() {
    let server = TestServer::filesystem().await;
    let client = reqwest::Client::new();
    let endpoint = server.endpoint();
    let bucket = server.bucket();

    client
        .put(format!("{}/{}/replace-src.txt", endpoint, bucket))
        .header("content-type", "text/plain")
        .header("x-amz-meta-original", "from-source")
        .body("source data")
        .send()
        .await
        .unwrap();

    let resp = client
        .put(format!("{}/{}/replace-dst.txt", endpoint, bucket))
        .header("x-amz-copy-source", format!("{}/replace-src.txt", bucket))
        .header("x-amz-metadata-directive", "REPLACE")
        .header("x-amz-meta-replaced", "new-value")
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());

    let get_resp = client
        .get(format!("{}/{}/replace-dst.txt", endpoint, bucket))
        .send()
        .await
        .unwrap();
    assert_eq!(
        get_resp
            .headers()
            .get("x-amz-meta-replaced")
            .and_then(|v| v.to_str().ok()),
        Some("new-value"),
        "New metadata should be used with REPLACE directive"
    );
    assert_eq!(
        get_resp
            .headers()
            .get("x-amz-meta-original")
            .and_then(|v| v.to_str().ok()),
        None,
        "Source metadata should NOT be present with REPLACE directive"
    );
}

#[tokio::test]
async fn test_copy_replace_content_type() {
    let server = TestServer::filesystem().await;
    let client = reqwest::Client::new();
    let endpoint = server.endpoint();
    let bucket = server.bucket();

    client
        .put(format!("{}/{}/ct-src.json", endpoint, bucket))
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();

    let resp = client
        .put(format!("{}/{}/ct-dst.txt", endpoint, bucket))
        .header("x-amz-copy-source", format!("{}/ct-src.json", bucket))
        .header("x-amz-metadata-directive", "REPLACE")
        .header("content-type", "text/plain")
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());

    let get_resp = client
        .get(format!("{}/{}/ct-dst.txt", endpoint, bucket))
        .send()
        .await
        .unwrap();
    assert_eq!(
        get_resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok()),
        Some("text/plain"),
        "Content-Type should be replaced"
    );
}

#[tokio::test]
async fn test_copy_to_self_with_replace() {
    let server = TestServer::filesystem().await;
    let client = reqwest::Client::new();
    let endpoint = server.endpoint();
    let bucket = server.bucket();

    client
        .put(format!("{}/{}/self-copy.txt", endpoint, bucket))
        .header("content-type", "text/plain")
        .header("x-amz-meta-version", "1")
        .body("same data")
        .send()
        .await
        .unwrap();

    let resp = client
        .put(format!("{}/{}/self-copy.txt", endpoint, bucket))
        .header("x-amz-copy-source", format!("{}/self-copy.txt", bucket))
        .header("x-amz-metadata-directive", "REPLACE")
        .header("x-amz-meta-version", "2")
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());

    let get_resp = client
        .get(format!("{}/{}/self-copy.txt", endpoint, bucket))
        .send()
        .await
        .unwrap();
    assert_eq!(
        get_resp
            .headers()
            .get("x-amz-meta-version")
            .and_then(|v| v.to_str().ok()),
        Some("2"),
        "Metadata should be updated via self-copy with REPLACE"
    );
    let body = get_resp.bytes().await.unwrap();
    assert_eq!(body.as_ref(), b"same data");
}

// ============================================================================
// 1.2 Conditional Requests
// ============================================================================

#[tokio::test]
async fn test_get_if_match_matching_etag() {
    let (server, http) = test_setup().await;
    put_object(
        &http,
        &server.endpoint(),
        server.bucket(),
        "ifm.txt",
        b"data".to_vec(),
        "text/plain",
    )
    .await;

    let head_resp = http
        .head(format!("{}/{}/ifm.txt", server.endpoint(), server.bucket()))
        .send()
        .await
        .unwrap();
    let etag = head_resp
        .headers()
        .get("etag")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    let resp = http
        .get(format!("{}/{}/ifm.txt", server.endpoint(), server.bucket()))
        .header("if-match", &etag)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
}

#[tokio::test]
async fn test_get_if_match_non_matching() {
    let (server, http) = test_setup().await;
    put_object(
        &http,
        &server.endpoint(),
        server.bucket(),
        "ifm-bad.txt",
        b"data".to_vec(),
        "text/plain",
    )
    .await;

    let resp = http
        .get(format!(
            "{}/{}/ifm-bad.txt",
            server.endpoint(),
            server.bucket()
        ))
        .header("if-match", "\"nonexistent-etag\"")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        412,
        "Non-matching If-Match should return 412"
    );
}

#[tokio::test]
async fn test_get_if_none_match_matching() {
    let (server, http) = test_setup().await;
    put_object(
        &http,
        &server.endpoint(),
        server.bucket(),
        "inm.txt",
        b"data".to_vec(),
        "text/plain",
    )
    .await;

    let head_resp = http
        .head(format!("{}/{}/inm.txt", server.endpoint(), server.bucket()))
        .send()
        .await
        .unwrap();
    let etag = head_resp
        .headers()
        .get("etag")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    let resp = http
        .get(format!("{}/{}/inm.txt", server.endpoint(), server.bucket()))
        .header("if-none-match", &etag)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        304,
        "Matching If-None-Match should return 304"
    );
}

#[tokio::test]
async fn test_get_if_none_match_non_matching() {
    let (server, http) = test_setup().await;
    put_object(
        &http,
        &server.endpoint(),
        server.bucket(),
        "inm-ok.txt",
        b"data".to_vec(),
        "text/plain",
    )
    .await;

    let resp = http
        .get(format!(
            "{}/{}/inm-ok.txt",
            server.endpoint(),
            server.bucket()
        ))
        .header("if-none-match", "\"different-etag\"")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 200);
}

#[tokio::test]
async fn test_get_if_modified_since_not_modified() {
    let (server, http) = test_setup().await;
    put_object(
        &http,
        &server.endpoint(),
        server.bucket(),
        "ims.txt",
        b"data".to_vec(),
        "text/plain",
    )
    .await;

    let resp = http
        .get(format!("{}/{}/ims.txt", server.endpoint(), server.bucket()))
        .header("if-modified-since", "Sun, 01 Jan 2090 00:00:00 GMT")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        304,
        "If-Modified-Since with future date should return 304"
    );
}

#[tokio::test]
async fn test_get_if_modified_since_modified() {
    let (server, http) = test_setup().await;
    put_object(
        &http,
        &server.endpoint(),
        server.bucket(),
        "ims-ok.txt",
        b"data".to_vec(),
        "text/plain",
    )
    .await;

    let resp = http
        .get(format!(
            "{}/{}/ims-ok.txt",
            server.endpoint(),
            server.bucket()
        ))
        .header("if-modified-since", "Thu, 01 Jan 2000 00:00:00 GMT")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "If-Modified-Since with past date should return 200"
    );
}

#[tokio::test]
async fn test_head_if_none_match() {
    let (server, http) = test_setup().await;
    put_object(
        &http,
        &server.endpoint(),
        server.bucket(),
        "head-inm.txt",
        b"data".to_vec(),
        "text/plain",
    )
    .await;

    let head_resp = http
        .head(format!(
            "{}/{}/head-inm.txt",
            server.endpoint(),
            server.bucket()
        ))
        .send()
        .await
        .unwrap();
    let etag = head_resp
        .headers()
        .get("etag")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    let resp = http
        .head(format!(
            "{}/{}/head-inm.txt",
            server.endpoint(),
            server.bucket()
        ))
        .header("if-none-match", &etag)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        304,
        "HEAD with matching If-None-Match should return 304"
    );
}

#[tokio::test]
async fn test_conditional_precedence_order() {
    let (server, http) = test_setup().await;
    let endpoint = server.endpoint();
    let bucket = server.bucket();

    put_object(
        &http,
        &endpoint,
        bucket,
        "prec.txt",
        b"data".to_vec(),
        "text/plain",
    )
    .await;

    let head_resp = http
        .head(format!("{}/{}/prec.txt", endpoint, bucket))
        .send()
        .await
        .unwrap();
    let etag = head_resp
        .headers()
        .get("etag")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    // If-Match fails (412) should take precedence over If-None-Match (304)
    let resp = http
        .get(format!("{}/{}/prec.txt", endpoint, bucket))
        .header("if-match", "\"wrong-etag\"")
        .header("if-none-match", &etag)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        412,
        "If-Match (412) should take precedence over If-None-Match (304)"
    );
}

// ============================================================================
// 1.1 Range Requests
// ============================================================================

#[tokio::test]
async fn test_get_range_first_100_bytes() {
    let (server, http) = test_setup().await;
    let data = upload_test_data(
        &http,
        &server.endpoint(),
        server.bucket(),
        "range.bin",
        1000,
    )
    .await;

    let resp = http
        .get(format!(
            "{}/{}/range.bin",
            server.endpoint(),
            server.bucket()
        ))
        .header("range", "bytes=0-99")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        206,
        "Range request should return 206"
    );

    let content_range = resp
        .headers()
        .get("content-range")
        .expect("Content-Range should be present")
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(content_range, "bytes 0-99/1000");

    let body = resp.bytes().await.unwrap();
    assert_eq!(body.len(), 100);
    assert_eq!(body.as_ref(), &data[0..100]);
}

#[tokio::test]
async fn test_get_range_last_100_bytes() {
    let (server, http) = test_setup().await;
    let data = upload_test_data(
        &http,
        &server.endpoint(),
        server.bucket(),
        "range-last.bin",
        1000,
    )
    .await;

    let resp = http
        .get(format!(
            "{}/{}/range-last.bin",
            server.endpoint(),
            server.bucket()
        ))
        .header("range", "bytes=900-999")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 206);

    let body = resp.bytes().await.unwrap();
    assert_eq!(body.len(), 100);
    assert_eq!(body.as_ref(), &data[900..1000]);
}

#[tokio::test]
async fn test_get_range_middle_slice() {
    let (server, http) = test_setup().await;
    let data = upload_test_data(
        &http,
        &server.endpoint(),
        server.bucket(),
        "range-mid.bin",
        1000,
    )
    .await;

    let resp = http
        .get(format!(
            "{}/{}/range-mid.bin",
            server.endpoint(),
            server.bucket()
        ))
        .header("range", "bytes=200-499")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 206);

    let content_range = resp
        .headers()
        .get("content-range")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(content_range, "bytes 200-499/1000");

    let body = resp.bytes().await.unwrap();
    assert_eq!(body.len(), 300);
    assert_eq!(body.as_ref(), &data[200..500]);
}

#[tokio::test]
async fn test_get_range_suffix() {
    let (server, http) = test_setup().await;
    let data = upload_test_data(
        &http,
        &server.endpoint(),
        server.bucket(),
        "range-suffix.bin",
        1000,
    )
    .await;

    let resp = http
        .get(format!(
            "{}/{}/range-suffix.bin",
            server.endpoint(),
            server.bucket()
        ))
        .header("range", "bytes=-100")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 206);

    let content_range = resp
        .headers()
        .get("content-range")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(content_range, "bytes 900-999/1000");

    let body = resp.bytes().await.unwrap();
    assert_eq!(body.len(), 100);
    assert_eq!(body.as_ref(), &data[900..1000]);
}

#[tokio::test]
async fn test_get_range_open_end() {
    let (server, http) = test_setup().await;
    let data = upload_test_data(
        &http,
        &server.endpoint(),
        server.bucket(),
        "range-open.bin",
        1000,
    )
    .await;

    let resp = http
        .get(format!(
            "{}/{}/range-open.bin",
            server.endpoint(),
            server.bucket()
        ))
        .header("range", "bytes=100-")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 206);

    let content_range = resp
        .headers()
        .get("content-range")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(content_range, "bytes 100-999/1000");

    let body = resp.bytes().await.unwrap();
    assert_eq!(body.len(), 900);
    assert_eq!(body.as_ref(), &data[100..1000]);
}

#[tokio::test]
async fn test_get_range_full_file() {
    let (server, http) = test_setup().await;
    let data = upload_test_data(
        &http,
        &server.endpoint(),
        server.bucket(),
        "range-full.bin",
        500,
    )
    .await;

    let resp = http
        .get(format!(
            "{}/{}/range-full.bin",
            server.endpoint(),
            server.bucket()
        ))
        .header("range", "bytes=0-")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 206);

    let body = resp.bytes().await.unwrap();
    assert_eq!(body.len(), 500);
    assert_eq!(body.as_ref(), data.as_slice());
}

#[tokio::test]
async fn test_get_range_invalid_returns_416() {
    let (server, http) = test_setup().await;
    upload_test_data(
        &http,
        &server.endpoint(),
        server.bucket(),
        "range-invalid.bin",
        100,
    )
    .await;

    let resp = http
        .get(format!(
            "{}/{}/range-invalid.bin",
            server.endpoint(),
            server.bucket()
        ))
        .header("range", "bytes=200-300")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        416,
        "Range beyond file size should return 416"
    );
}

#[tokio::test]
async fn test_get_range_beyond_file_size() {
    let (server, http) = test_setup().await;
    let data = upload_test_data(
        &http,
        &server.endpoint(),
        server.bucket(),
        "range-beyond.bin",
        100,
    )
    .await;

    let resp = http
        .get(format!(
            "{}/{}/range-beyond.bin",
            server.endpoint(),
            server.bucket()
        ))
        .header("range", "bytes=50-999")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 206);

    let content_range = resp
        .headers()
        .get("content-range")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(
        content_range, "bytes 50-99/100",
        "End should be clamped to file size"
    );

    let body = resp.bytes().await.unwrap();
    assert_eq!(body.len(), 50);
    assert_eq!(body.as_ref(), &data[50..100]);
}

#[tokio::test]
async fn test_get_range_on_passthrough_file() {
    let (server, http) = test_setup().await;
    let data = generate_binary(500, 50);
    put_object(
        &http,
        &server.endpoint(),
        server.bucket(),
        "range-pt.png",
        data.clone(),
        "image/png",
    )
    .await;

    let resp = http
        .get(format!(
            "{}/{}/range-pt.png",
            server.endpoint(),
            server.bucket()
        ))
        .header("range", "bytes=0-99")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        206,
        "Range on passthrough file should return 206"
    );

    let body = resp.bytes().await.unwrap();
    assert_eq!(body.len(), 100);
    assert_eq!(body.as_ref(), &data[0..100]);
}

#[tokio::test]
async fn test_get_range_on_delta_file() {
    let (server, http) = test_setup().await;
    let endpoint = server.endpoint();
    let bucket = server.bucket();

    let base_data = generate_binary(10_000, 100);
    put_object(
        &http,
        &endpoint,
        bucket,
        "delta-prefix/ref.bin",
        base_data.clone(),
        "application/octet-stream",
    )
    .await;

    let mut modified_data = base_data;
    for i in 0..50 {
        modified_data[i * 100] = modified_data[i * 100].wrapping_add(1);
    }
    put_object(
        &http,
        &endpoint,
        bucket,
        "delta-prefix/delta.bin",
        modified_data.clone(),
        "application/octet-stream",
    )
    .await;

    let resp = http
        .get(format!("{}/{}/delta-prefix/delta.bin", endpoint, bucket))
        .header("range", "bytes=0-99")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        206,
        "Range on delta file should return 206"
    );

    let body = resp.bytes().await.unwrap();
    assert_eq!(body.len(), 100);
    assert_eq!(
        body.as_ref(),
        &modified_data[0..100],
        "Range content should match the original data"
    );
}

/// Set up a delta-compressed file for Range testing. Returns (server, http_client, original_data).
async fn setup_delta_range_test() -> (TestServer, reqwest::Client, Vec<u8>) {
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();

    // Upload base zip (becomes reference)
    let base = generate_binary(10_000, 42);
    put_object(
        &http,
        &server.endpoint(),
        server.bucket(),
        "range-delta/base.zip",
        base.clone(),
        "application/zip",
    )
    .await;

    // Upload variant (becomes delta)
    let modified = mutate_binary(&base, 0.05);
    let st = put_and_get_storage_type(
        &http,
        &server.endpoint(),
        server.bucket(),
        "range-delta/variant.zip",
        modified.clone(),
        "application/zip",
    )
    .await;
    assert_eq!(st, "delta", "Should be stored as delta");

    (server, http, modified)
}

#[tokio::test]
async fn test_range_delta_last_bytes() {
    let (server, http, modified) = setup_delta_range_test().await;
    let url = format!(
        "{}/{}/range-delta/variant.zip",
        server.endpoint(),
        server.bucket()
    );

    let resp = http
        .get(&url)
        .header("range", "bytes=9900-9999")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 206);

    let body = resp.bytes().await.unwrap();
    assert_eq!(body.len(), 100);
    assert_eq!(body.as_ref(), &modified[9900..=9999]);
}

#[tokio::test]
async fn test_range_delta_middle_slice() {
    let (server, http, modified) = setup_delta_range_test().await;
    let url = format!(
        "{}/{}/range-delta/variant.zip",
        server.endpoint(),
        server.bucket()
    );

    let resp = http
        .get(&url)
        .header("range", "bytes=2000-4999")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 206);

    let content_range = resp
        .headers()
        .get("content-range")
        .expect("Content-Range header should be present")
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(
        content_range, "bytes 2000-4999/10000",
        "Content-Range should match expected format"
    );

    let body = resp.bytes().await.unwrap();
    assert_eq!(body.len(), 3000);
    assert_eq!(body.as_ref(), &modified[2000..=4999]);
}

#[tokio::test]
async fn test_range_delta_suffix() {
    let (server, http, modified) = setup_delta_range_test().await;
    let url = format!(
        "{}/{}/range-delta/variant.zip",
        server.endpoint(),
        server.bucket()
    );

    let resp = http
        .get(&url)
        .header("range", "bytes=-500")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 206);

    let content_range = resp
        .headers()
        .get("content-range")
        .expect("Content-Range header should be present")
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(
        content_range, "bytes 9500-9999/10000",
        "Content-Range for suffix range should cover last 500 bytes"
    );

    let body = resp.bytes().await.unwrap();
    assert_eq!(body.len(), 500);
    assert_eq!(body.as_ref(), &modified[9500..]);
}

#[tokio::test]
async fn test_range_delta_open_end() {
    let (server, http, modified) = setup_delta_range_test().await;
    let url = format!(
        "{}/{}/range-delta/variant.zip",
        server.endpoint(),
        server.bucket()
    );

    let resp = http
        .get(&url)
        .header("range", "bytes=5000-")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 206);

    let content_range = resp
        .headers()
        .get("content-range")
        .expect("Content-Range header should be present")
        .to_str()
        .unwrap()
        .to_string();
    assert_eq!(
        content_range, "bytes 5000-9999/10000",
        "Content-Range for open-end range should go to end of file"
    );

    let content_length: usize = resp
        .headers()
        .get("content-length")
        .expect("Content-Length should be present")
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(content_length, 5000, "Content-Length should be 5000");

    let body = resp.bytes().await.unwrap();
    assert_eq!(body.len(), 5000);
    assert_eq!(body.as_ref(), &modified[5000..]);
}

#[tokio::test]
async fn test_range_delta_full_file() {
    let (server, http, modified) = setup_delta_range_test().await;
    let url = format!(
        "{}/{}/range-delta/variant.zip",
        server.endpoint(),
        server.bucket()
    );

    let resp = http
        .get(&url)
        .header("range", "bytes=0-")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        206,
        "bytes=0- should return 206 Partial Content"
    );

    let body = resp.bytes().await.unwrap();
    assert_eq!(body.len(), modified.len());
    assert_eq!(body.as_ref(), &modified[..]);
}

#[tokio::test]
async fn test_range_delta_invalid_416() {
    let (server, http, _modified) = setup_delta_range_test().await;
    let url = format!(
        "{}/{}/range-delta/variant.zip",
        server.endpoint(),
        server.bucket()
    );

    let resp = http
        .get(&url)
        .header("range", "bytes=20000-30000")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        416,
        "Range beyond file size should return 416 Range Not Satisfiable"
    );
}

#[tokio::test]
async fn test_range_delta_clamped() {
    let (server, http, modified) = setup_delta_range_test().await;
    let url = format!(
        "{}/{}/range-delta/variant.zip",
        server.endpoint(),
        server.bucket()
    );

    let resp = http
        .get(&url)
        .header("range", "bytes=9500-999999")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 206);

    let content_range = resp
        .headers()
        .get("content-range")
        .expect("Content-Range header should be present")
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        content_range.contains("9999/10000"),
        "Content-Range should show actual end (9999), got: {}",
        content_range
    );

    let body = resp.bytes().await.unwrap();
    assert_eq!(body.len(), 500);
    assert_eq!(body.as_ref(), &modified[9500..]);
}

#[tokio::test]
async fn test_range_delta_single_byte() {
    let (server, http, modified) = setup_delta_range_test().await;
    let url = format!(
        "{}/{}/range-delta/variant.zip",
        server.endpoint(),
        server.bucket()
    );

    let resp = http
        .get(&url)
        .header("range", "bytes=5000-5000")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 206);

    let body = resp.bytes().await.unwrap();
    assert_eq!(
        body.len(),
        1,
        "Single byte range should return exactly 1 byte"
    );
    assert_eq!(body[0], modified[5000]);
}

#[tokio::test]
async fn test_range_delta_content_length() {
    let (server, http, _modified) = setup_delta_range_test().await;
    let url = format!(
        "{}/{}/range-delta/variant.zip",
        server.endpoint(),
        server.bucket()
    );

    let range_specs = [
        ("bytes=0-99", 100),
        ("bytes=0-9999", 10000),
        ("bytes=1000-1999", 1000),
        ("bytes=9999-9999", 1),
    ];

    for (range, expected_len) in &range_specs {
        let resp = http.get(&url).header("range", *range).send().await.unwrap();
        assert_eq!(
            resp.status().as_u16(),
            206,
            "Range {} should return 206",
            range
        );

        let content_length: usize = resp
            .headers()
            .get("content-length")
            .unwrap_or_else(|| panic!("Content-Length missing for range {}", range))
            .to_str()
            .unwrap()
            .parse()
            .unwrap();

        let body = resp.bytes().await.unwrap();
        assert_eq!(
            content_length, *expected_len,
            "Content-Length for range {} should be {}",
            range, expected_len
        );
        assert_eq!(
            body.len(),
            *expected_len,
            "Body length for range {} should match Content-Length",
            range
        );
    }
}

#[tokio::test]
async fn test_range_delta_sequential_requests() {
    let (server, http, modified) = setup_delta_range_test().await;
    let url = format!(
        "{}/{}/range-delta/variant.zip",
        server.endpoint(),
        server.bucket()
    );

    // First range request: beginning of file
    let resp1 = http
        .get(&url)
        .header("range", "bytes=0-499")
        .send()
        .await
        .unwrap();
    assert_eq!(resp1.status().as_u16(), 206);
    let body1 = resp1.bytes().await.unwrap();
    assert_eq!(body1.as_ref(), &modified[0..500]);

    // Second range request: end of file
    let resp2 = http
        .get(&url)
        .header("range", "bytes=9500-9999")
        .send()
        .await
        .unwrap();
    assert_eq!(resp2.status().as_u16(), 206);
    let body2 = resp2.bytes().await.unwrap();
    assert_eq!(body2.as_ref(), &modified[9500..]);

    // Verify the two ranges are different (sanity check)
    assert_ne!(
        body1.as_ref(),
        body2.as_ref(),
        "Different ranges should return different data"
    );
}

// ============================================================================
// 2.1 ACL stubs
// ============================================================================

#[tokio::test]
async fn test_get_object_acl_returns_full_control() {
    let (server, http) = test_setup().await;
    let url = format!("{}/{}/acl-test.txt", server.endpoint(), server.bucket());
    http.put(&url).body("hello").send().await.unwrap();

    let acl_url = format!("{}?acl", url);
    let resp = http.get(&acl_url).send().await.unwrap();
    assert_eq!(resp.status(), 200);

    let body = resp.text().await.unwrap();
    assert!(
        body.contains("<Permission>FULL_CONTROL</Permission>"),
        "ACL response should contain FULL_CONTROL, got: {}",
        body
    );
    assert!(
        body.contains("<ID>dgp</ID>"),
        "ACL response should contain owner ID 'dgp', got: {}",
        body
    );
    assert!(
        body.contains("<DisplayName>deltaglider</DisplayName>"),
        "ACL response should contain display name 'deltaglider', got: {}",
        body
    );
    assert!(
        body.contains("<AccessControlPolicy>"),
        "ACL response should contain AccessControlPolicy root, got: {}",
        body
    );
}

// test_put_object_acl_accepted — REMOVED. Asserted the pre-Wave-4-M4
// permissive 200-with-XML-discarded behaviour. The honest 501 path is
// covered by `test_put_object_acl_returns_501` +
// `test_put_object_acl_on_missing_object_404_wins` in
// tests/s3_correctness_test.rs.

#[tokio::test]
async fn test_get_bucket_acl_returns_full_control() {
    let (server, http) = test_setup().await;
    let url = format!("{}/{}?acl", server.endpoint(), server.bucket());
    let resp = http.get(&url).send().await.unwrap();
    assert_eq!(resp.status(), 200);

    let body = resp.text().await.unwrap();
    assert!(
        body.contains("<Permission>FULL_CONTROL</Permission>"),
        "Bucket ACL should contain FULL_CONTROL, got: {}",
        body
    );
    assert!(
        body.contains("<ID>dgp</ID>"),
        "Bucket ACL should contain owner ID 'dgp', got: {}",
        body
    );
}

// test_put_bucket_acl_accepted — REMOVED. Same Wave-4 M4 staleness as
// the object-ACL counterpart above; honest behaviour is verified in
// `test_put_bucket_acl_returns_501_not_fake_200` +
// `test_put_bucket_acl_on_missing_bucket_404_wins`
// (tests/s3_correctness_test.rs).

// ============================================================================
// 2.2 Response override query parameters
// ============================================================================

#[tokio::test]
async fn test_get_with_response_content_type() {
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();

    // Upload an object
    let url = format!(
        "{}/{}/response-override.txt",
        server.endpoint(),
        server.bucket()
    );
    http.put(&url)
        .header("content-type", "text/plain")
        .body("some data")
        .send()
        .await
        .unwrap();

    // GET with response-content-type override
    let get_url = format!("{}?response-content-type=application/pdf", url);
    let resp = http.get(&get_url).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap(),
        "application/pdf",
        "Content-Type should be overridden to application/pdf"
    );
}

#[tokio::test]
async fn test_get_with_response_content_disposition() {
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();

    // Upload an object
    let url = format!(
        "{}/{}/disposition-test.txt",
        server.endpoint(),
        server.bucket()
    );
    http.put(&url).body("download me").send().await.unwrap();

    // GET with response-content-disposition override
    let get_url = format!(
        "{}?response-content-disposition=attachment%3B%20filename%3D%22custom.txt%22",
        url
    );
    let resp = http.get(&get_url).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let cd = resp
        .headers()
        .get("content-disposition")
        .expect("Content-Disposition header should be present")
        .to_str()
        .unwrap();
    assert!(
        cd.contains("attachment"),
        "Content-Disposition should contain 'attachment', got: {}",
        cd
    );
}

#[tokio::test]
async fn test_presigned_url_with_response_overrides() {
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();

    // Upload an object
    let url = format!(
        "{}/{}/presigned-test.bin",
        server.endpoint(),
        server.bucket()
    );
    http.put(&url)
        .header("content-type", "application/octet-stream")
        .body("binary data")
        .send()
        .await
        .unwrap();

    // GET with multiple response overrides (simulating a presigned URL)
    let get_url = format!(
        "{}?response-content-type=text/csv&response-cache-control=no-cache&response-content-encoding=gzip&response-content-language=en-US&response-expires=Thu%2C%2001%20Dec%202025%2016%3A00%3A00%20GMT",
        url
    );
    let resp = http.get(&get_url).send().await.unwrap();
    assert_eq!(resp.status(), 200);

    assert_eq!(
        resp.headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap(),
        "text/csv"
    );
    assert_eq!(
        resp.headers()
            .get("cache-control")
            .unwrap()
            .to_str()
            .unwrap(),
        "no-cache"
    );
    assert_eq!(
        resp.headers()
            .get("content-encoding")
            .unwrap()
            .to_str()
            .unwrap(),
        "gzip"
    );
    assert_eq!(
        resp.headers()
            .get("content-language")
            .unwrap()
            .to_str()
            .unwrap(),
        "en-US"
    );
    assert!(
        resp.headers().get("expires").is_some(),
        "Expires header should be present"
    );
}

// ============================================================================
// 2.3 CreateBucket naming validation
// ============================================================================
//
// The 7 test_create_bucket_* cases that used to live here were
// superseded by the parametric unit test in
// `src/api/handlers/bucket.rs::tests::validate_bucket_name_*`. The
// integration round-trip (HTTP PUT → handler → XML error body) added
// no unique signal over the pure-function check; every one spawned a
// full TestServer to exercise a string validator. Moved to unit
// tests to reclaim ~1.5s of CI per run and keep the bucket-naming
// truth table in one place.
//
// If the CreateBucket HTTP surface ever grows real behaviour
// (location constraints, bucket policy, ACLs), bring integration
// tests back — but keep the pure-validator coverage where it is.

// ============================================================================
// 2.4 ListBuckets real creation dates
// ============================================================================

#[tokio::test]
async fn test_list_buckets_creation_date_not_now() {
    let server = TestServer::filesystem().await;
    let http = reqwest::Client::new();

    // Create a bucket
    let bucket_name = "date-test-bucket";
    let url = format!("{}/{}", server.endpoint(), bucket_name);
    http.put(&url).send().await.unwrap();

    // Small delay to ensure "now" is different from creation time
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // List buckets
    let list_url = format!("{}/", server.endpoint());
    let resp = http.get(&list_url).send().await.unwrap();
    assert_eq!(resp.status(), 200);

    let body = resp.text().await.unwrap();
    assert!(
        body.contains("<CreationDate>"),
        "ListBuckets should contain CreationDate elements, got: {}",
        body
    );

    // Parse the creation date to verify it's a valid timestamp, not "now"
    // The creation date should be in ISO 8601 format
    if let Some(start) = body.find("<CreationDate>") {
        let after = &body[start + "<CreationDate>".len()..];
        if let Some(end) = after.find("</CreationDate>") {
            let date_str = &after[..end];
            // Verify it's a valid date string (not empty, contains expected format)
            assert!(
                date_str.contains('T') && date_str.contains('Z'),
                "CreationDate should be in ISO 8601 format, got: {}",
                date_str
            );
            // Parse it to verify it's a valid date
            // Try parsing with chrono's RFC 3339 parser which handles variable fractional seconds
            let parsed = chrono::DateTime::parse_from_rfc3339(date_str).or_else(|_| {
                chrono::NaiveDateTime::parse_from_str(date_str, "%Y-%m-%dT%H:%M:%S%.fZ")
                    .map(|ndt| ndt.and_utc().fixed_offset())
            });
            assert!(
                parsed.is_ok(),
                "CreationDate should be parseable, got: {} (error: {:?})",
                date_str,
                parsed.err()
            );
        }
    }
}

// ============================================================================
// ListObjectsV2 metadata=true extension
// ============================================================================

#[tokio::test]
async fn test_list_v2_metadata_true_returns_user_metadata() {
    let server = TestServer::filesystem().await;
    let client = reqwest::Client::new();
    let endpoint = server.endpoint();
    let bucket = server.bucket();

    // Upload an object with custom user metadata under a prefix (directory)
    let url = format!("{}/{}/metadir/meta-test.txt", endpoint, bucket);
    let resp = client
        .put(&url)
        .header("content-type", "text/plain")
        .header("x-amz-meta-custom-key", "custom-value")
        .header("x-amz-meta-another-key", "another-value")
        .body("hello metadata")
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "PUT failed: {}", resp.status());

    // List with metadata=true
    let list_url = format!(
        "{}/{}?list-type=2&metadata=true&prefix=metadir/",
        endpoint, bucket
    );
    let resp = client.get(&list_url).send().await.unwrap();
    assert!(resp.status().is_success(), "LIST failed: {}", resp.status());
    let body = resp.text().await.unwrap();

    // Verify UserMetadata is present
    assert!(
        body.contains("<UserMetadata>"),
        "Response should contain <UserMetadata> when metadata=true, got:\n{}",
        body
    );

    // Verify custom metadata keys appear with x-amz-meta- prefix
    assert!(
        body.contains("x-amz-meta-user-custom-key"),
        "Response should contain user custom metadata key, got:\n{}",
        body
    );
    assert!(
        body.contains("custom-value"),
        "Response should contain user custom metadata value, got:\n{}",
        body
    );
    assert!(
        body.contains("x-amz-meta-user-another-key"),
        "Response should contain second custom metadata key, got:\n{}",
        body
    );
    assert!(
        body.contains("another-value"),
        "Response should contain second custom metadata value, got:\n{}",
        body
    );
}

#[tokio::test]
async fn test_list_v2_metadata_false_no_user_metadata() {
    let server = TestServer::filesystem().await;
    let client = reqwest::Client::new();
    let endpoint = server.endpoint();
    let bucket = server.bucket();

    // Upload an object with custom user metadata
    let url = format!("{}/{}/nomdir/no-meta-test.txt", endpoint, bucket);
    let resp = client
        .put(&url)
        .header("content-type", "text/plain")
        .header("x-amz-meta-some-key", "some-value")
        .body("hello no metadata")
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "PUT failed: {}", resp.status());

    // List WITHOUT metadata=true (default)
    let list_url = format!("{}/{}?list-type=2&prefix=nomdir/", endpoint, bucket);
    let resp = client.get(&list_url).send().await.unwrap();
    assert!(resp.status().is_success(), "LIST failed: {}", resp.status());
    let body = resp.text().await.unwrap();

    // Verify UserMetadata is NOT present
    assert!(
        !body.contains("<UserMetadata>"),
        "Response should NOT contain <UserMetadata> without metadata=true, got:\n{}",
        body
    );
}

#[tokio::test]
async fn test_list_v2_metadata_true_dg_metadata() {
    let server = TestServer::filesystem().await;
    let client = reqwest::Client::new();
    let endpoint = server.endpoint();
    let bucket = server.bucket();

    // Upload a file under a prefix (DG will add its own metadata)
    let url = format!("{}/{}/dgdir/dg-meta-test.txt", endpoint, bucket);
    let resp = client
        .put(&url)
        .header("content-type", "text/plain")
        .body("hello dg metadata")
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "PUT failed: {}", resp.status());

    // List with metadata=true
    let list_url = format!(
        "{}/{}?list-type=2&metadata=true&prefix=dgdir/",
        endpoint, bucket
    );
    let resp = client.get(&list_url).send().await.unwrap();
    assert!(resp.status().is_success(), "LIST failed: {}", resp.status());
    let body = resp.text().await.unwrap();

    // Verify DG-specific metadata keys appear
    assert!(
        body.contains("x-amz-meta-dg-note"),
        "Response should contain dg-note metadata, got:\n{}",
        body
    );
    assert!(
        body.contains("x-amz-meta-dg-file-size"),
        "Response should contain dg-file-size metadata, got:\n{}",
        body
    );
    assert!(
        body.contains("x-amz-meta-dg-tool"),
        "Response should contain dg-tool metadata, got:\n{}",
        body
    );
    assert!(
        body.contains("x-amz-meta-dg-file-sha256"),
        "Response should contain dg-file-sha256 metadata, got:\n{}",
        body
    );

    // Verify the note value is "passthrough" for a simple text file
    assert!(
        body.contains("<Value>passthrough</Value>"),
        "dg-note should be 'passthrough' for a text file, got:\n{}",
        body
    );
}

// ============================================================================
// Conditional edge cases
// ============================================================================

#[tokio::test]
async fn test_conditional_on_nonexistent_object_returns_404() {
    let (server, http) = test_setup().await;

    let resp = http
        .get(format!(
            "{}/{}/does-not-exist.txt",
            server.endpoint(),
            server.bucket()
        ))
        .header("if-match", "\"some-etag\"")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        404,
        "If-Match on nonexistent object should return 404, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn test_get_if_match_wildcard() {
    let (server, http) = test_setup().await;
    put_object(
        &http,
        &server.endpoint(),
        server.bucket(),
        "ifm-wild.txt",
        b"wildcard test".to_vec(),
        "text/plain",
    )
    .await;

    let resp = http
        .get(format!(
            "{}/{}/ifm-wild.txt",
            server.endpoint(),
            server.bucket()
        ))
        .header("if-match", "*")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "If-Match: * should match any existing object, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn test_get_range_on_zero_byte_file() {
    let (server, http) = test_setup().await;
    put_object(
        &http,
        &server.endpoint(),
        server.bucket(),
        "empty.bin",
        vec![],
        "application/octet-stream",
    )
    .await;

    let resp = http
        .get(format!(
            "{}/{}/empty.bin",
            server.endpoint(),
            server.bucket()
        ))
        .header("range", "bytes=0-0")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        416,
        "Range on zero-byte file should return 416, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn test_head_conditional_if_none_match() {
    let (server, http) = test_setup().await;
    put_object(
        &http,
        &server.endpoint(),
        server.bucket(),
        "head-cond.txt",
        b"head conditional".to_vec(),
        "text/plain",
    )
    .await;

    let head_resp = http
        .head(format!(
            "{}/{}/head-cond.txt",
            server.endpoint(),
            server.bucket()
        ))
        .send()
        .await
        .unwrap();
    let etag = head_resp
        .headers()
        .get("etag")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    // HEAD with matching If-None-Match should return 304
    let resp = http
        .head(format!(
            "{}/{}/head-cond.txt",
            server.endpoint(),
            server.bucket()
        ))
        .header("if-none-match", &etag)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        304,
        "HEAD with matching If-None-Match should return 304, got {}",
        resp.status()
    );

    // HEAD with non-matching If-None-Match should return 200
    let resp = http
        .head(format!(
            "{}/{}/head-cond.txt",
            server.endpoint(),
            server.bucket()
        ))
        .header("if-none-match", "\"different-etag\"")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "HEAD with non-matching If-None-Match should return 200, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn test_get_acl_nonexistent_object_returns_404() {
    let (server, http) = test_setup().await;

    let resp = http
        .get(format!(
            "{}/{}/no-such-object.txt?acl",
            server.endpoint(),
            server.bucket()
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        404,
        "GET ?acl on nonexistent object should return 404, got {}",
        resp.status()
    );
}

#[tokio::test]
async fn test_get_acl_nonexistent_bucket_returns_404() {
    let (server, http) = test_setup().await;

    let resp = http
        .get(format!("{}/nonexistent-bucket-xyz?acl", server.endpoint()))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        404,
        "GET ?acl on nonexistent bucket should return 404, got {}",
        resp.status()
    );
}

// ============================================================================
// UploadPartCopy
// ============================================================================

#[tokio::test]
async fn test_upload_part_copy() {
    let (server, http) = test_setup().await;
    let endpoint = server.endpoint();
    let bucket = server.bucket();

    // Upload a source object
    let source_data = b"hello world from source object for copy part test";
    put_object(
        &http,
        &endpoint,
        bucket,
        "copy-source.txt",
        source_data.to_vec(),
        "text/plain",
    )
    .await;

    // Initiate multipart upload
    let resp = http
        .post(format!("{}/{}/copy-dest.txt?uploads", endpoint, bucket))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let body = resp.text().await.unwrap();
    let upload_id = body
        .split("<UploadId>")
        .nth(1)
        .unwrap()
        .split("</UploadId>")
        .next()
        .unwrap();

    // UploadPartCopy: copy source object as part 1
    let resp = http
        .put(format!(
            "{}/{}/copy-dest.txt?partNumber=1&uploadId={}",
            endpoint, bucket, upload_id
        ))
        .header("x-amz-copy-source", format!("{}/copy-source.txt", bucket))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "UploadPartCopy should succeed, got {}",
        resp.status()
    );
    let copy_result = resp.text().await.unwrap();
    assert!(
        copy_result.contains("<CopyPartResult"),
        "Response should be CopyPartResult XML: {}",
        copy_result
    );
    assert!(
        copy_result.contains("<ETag>"),
        "CopyPartResult should contain ETag"
    );

    // Extract ETag from CopyPartResult
    let etag = copy_result
        .split("<ETag>")
        .nth(1)
        .unwrap()
        .split("</ETag>")
        .next()
        .unwrap();

    // Complete multipart upload
    let complete_xml = format!(
        r#"<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>{}</ETag></Part></CompleteMultipartUpload>"#,
        etag
    );
    let resp = http
        .post(format!(
            "{}/{}/copy-dest.txt?uploadId={}",
            endpoint, bucket, upload_id
        ))
        .header("content-type", "application/xml")
        .body(complete_xml)
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "CompleteMultipartUpload should succeed after UploadPartCopy, got {}",
        resp.status()
    );

    // Verify the copied object has the same content
    let resp = http
        .get(format!("{}/{}/copy-dest.txt", endpoint, bucket))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let result_data = resp.bytes().await.unwrap();
    assert_eq!(
        result_data.as_ref(),
        source_data,
        "Copied object should have same content as source"
    );
}

#[tokio::test]
async fn test_upload_part_copy_with_range() {
    let (server, http) = test_setup().await;
    let endpoint = server.endpoint();
    let bucket = server.bucket();

    // Upload a source object
    let source_data = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    put_object(
        &http,
        &endpoint,
        bucket,
        "range-source.txt",
        source_data.to_vec(),
        "text/plain",
    )
    .await;

    // Initiate multipart upload
    let resp = http
        .post(format!("{}/{}/range-dest.txt?uploads", endpoint, bucket))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let body = resp.text().await.unwrap();
    let upload_id = body
        .split("<UploadId>")
        .nth(1)
        .unwrap()
        .split("</UploadId>")
        .next()
        .unwrap();

    // UploadPartCopy with range: bytes=5-14 (FGHIJKLMNO, 10 bytes)
    let resp = http
        .put(format!(
            "{}/{}/range-dest.txt?partNumber=1&uploadId={}",
            endpoint, bucket, upload_id
        ))
        .header("x-amz-copy-source", format!("{}/range-source.txt", bucket))
        .header("x-amz-copy-source-range", "bytes=5-14")
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "UploadPartCopy with range should succeed, got {}",
        resp.status()
    );
    let copy_result = resp.text().await.unwrap();
    let etag = copy_result
        .split("<ETag>")
        .nth(1)
        .unwrap()
        .split("</ETag>")
        .next()
        .unwrap();

    // Complete multipart upload
    let complete_xml = format!(
        r#"<CompleteMultipartUpload><Part><PartNumber>1</PartNumber><ETag>{}</ETag></Part></CompleteMultipartUpload>"#,
        etag
    );
    let resp = http
        .post(format!(
            "{}/{}/range-dest.txt?uploadId={}",
            endpoint, bucket, upload_id
        ))
        .header("content-type", "application/xml")
        .body(complete_xml)
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "CompleteMultipartUpload should succeed, got {}",
        resp.status()
    );

    // Verify the object contains only the ranged bytes
    let resp = http
        .get(format!("{}/{}/range-dest.txt", endpoint, bucket))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success());
    let result_data = resp.bytes().await.unwrap();
    assert_eq!(
        result_data.as_ref(),
        b"FGHIJKLMNO",
        "Ranged copy should contain bytes 5-14 of source"
    );
}

// ============================================================================
// Form POST upload compatibility (create_presigned_post)
// ============================================================================

type HmacSha256 = Hmac<Sha256>;

fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC key init");
    mac.update(data);
    let out = mac.finalize().into_bytes();
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&out);
    arr
}

fn derive_post_signing_key(secret: &str, date: &str, region: &str) -> [u8; 32] {
    let k_date = hmac_sha256(format!("AWS4{}", secret).as_bytes(), date.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, b"s3");
    hmac_sha256(&k_service, b"aws4_request")
}

#[tokio::test]
async fn test_form_post_upload_succeeds_with_presigned_policy() {
    let server = TestServer::builder()
        .auth("POSTACCESSKEY", "POSTSECRETKEY123")
        .build()
        .await;
    let client = reqwest::Client::new();
    let bucket = server.bucket();
    let endpoint = server.endpoint();
    let key = "post/uploads/hello.txt";
    let amz_date = "20260507T120000Z";
    let credential = "POSTACCESSKEY/20260507/us-east-1/s3/aws4_request";
    let policy = serde_json::json!({
        "expiration": "2099-01-01T00:00:00.000Z",
        "conditions": [
            { "bucket": bucket },
            ["starts-with", "$key", "post/uploads/"],
            { "x-amz-algorithm": "AWS4-HMAC-SHA256" },
            { "x-amz-credential": credential },
            { "x-amz-date": amz_date },
            ["content-length-range", 1, 1048576]
        ]
    });
    let policy_b64 = base64::engine::general_purpose::STANDARD.encode(policy.to_string());
    let signing_key = derive_post_signing_key("POSTSECRETKEY123", "20260507", "us-east-1");
    let signature = hex::encode(hmac_sha256(&signing_key, policy_b64.as_bytes()));
    let form = reqwest::multipart::Form::new()
        .text("key", key)
        .text("policy", policy_b64)
        .text("x-amz-algorithm", "AWS4-HMAC-SHA256")
        .text("x-amz-credential", credential)
        .text("x-amz-date", amz_date)
        .text("x-amz-signature", signature)
        .part(
            "file",
            reqwest::multipart::Part::bytes(b"hello-form-post".to_vec())
                .file_name("hello.txt")
                .mime_str("text/plain")
                .unwrap(),
        );
    let resp = client
        .post(format!("{}/{}", endpoint, bucket))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        204,
        "POST form upload should return 204, got {}",
        resp.status()
    );

    let s3 = server.s3_client().await;
    let got = s3
        .get_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .expect("GET uploaded object");
    let body = got.body.collect().await.expect("collect body").into_bytes();
    assert_eq!(body.as_ref(), b"hello-form-post");
}

/// C2 regression: a form field NOT covered by any policy condition is rejected
/// (AWS's "every field must have a condition" rule). Here the policy signs only
/// bucket/key/algorithm/credential/date but the form smuggles an unapproved
/// `x-amz-meta-injected` — must 403, not store the metadata.
#[tokio::test]
async fn test_form_post_rejects_field_not_covered_by_policy() {
    let server = TestServer::builder()
        .auth("POSTACCESSKEY", "POSTSECRETKEY123")
        .build()
        .await;
    let client = reqwest::Client::new();
    let (bucket, endpoint) = (server.bucket(), server.endpoint());
    let amz_date = "20260507T120000Z";
    let credential = "POSTACCESSKEY/20260507/us-east-1/s3/aws4_request";
    let policy = serde_json::json!({
        "expiration": "2099-01-01T00:00:00.000Z",
        "conditions": [
            { "bucket": bucket },
            ["starts-with", "$key", "post/"],
            { "x-amz-algorithm": "AWS4-HMAC-SHA256" },
            { "x-amz-credential": credential },
            { "x-amz-date": amz_date }
        ]
    });
    let policy_b64 = base64::engine::general_purpose::STANDARD.encode(policy.to_string());
    let signing_key = derive_post_signing_key("POSTSECRETKEY123", "20260507", "us-east-1");
    let signature = hex::encode(hmac_sha256(&signing_key, policy_b64.as_bytes()));
    let form = reqwest::multipart::Form::new()
        .text("key", "post/x.txt")
        .text("policy", policy_b64)
        .text("x-amz-algorithm", "AWS4-HMAC-SHA256")
        .text("x-amz-credential", credential)
        .text("x-amz-date", amz_date)
        .text("x-amz-signature", signature)
        // Smuggled — no policy condition references x-amz-meta-injected.
        .text("x-amz-meta-injected", "evil")
        .part(
            "file",
            reqwest::multipart::Part::bytes(b"x".to_vec())
                .file_name("x.txt")
                .mime_str("text/plain")
                .unwrap(),
        );
    let resp = client
        .post(format!("{}/{}", endpoint, bucket))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        403,
        "a form field absent from the policy must be rejected, got {}",
        resp.status()
    );
}

/// C1 regression: a form-POST to a traversal bucket name (`..`) is rejected
/// BEFORE any filesystem path is constructed — `root.join("..")` must never be
/// reached. reqwest normalises `..` out of a URL, so we send a RAW HTTP request
/// over a TCP socket carrying the literal `POST /.. HTTP/1.1` request-line.
#[tokio::test]
async fn test_form_post_rejects_bucket_name_traversal() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let server = TestServer::builder()
        .auth("POSTACCESSKEY", "POSTSECRETKEY123")
        .build()
        .await;
    let addr = server.endpoint().replace("http://", "");

    // Minimal multipart body — the bucket-name gate fires before parse/auth,
    // so the body content doesn't matter, only that content-type is multipart.
    let boundary = "X";
    let body = format!(
        "--{b}\r\nContent-Disposition: form-data; name=\"key\"\r\n\r\nx.txt\r\n\
         --{b}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"x.txt\"\r\n\
         Content-Type: text/plain\r\n\r\nx\r\n--{b}--\r\n",
        b = boundary
    );
    let req = format!(
        "POST /.. HTTP/1.1\r\nHost: {addr}\r\n\
         Content-Type: multipart/form-data; boundary={boundary}\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let mut stream = tokio::net::TcpStream::connect(&addr).await.unwrap();
    stream.write_all(req.as_bytes()).await.unwrap();
    let mut resp = String::new();
    stream.read_to_string(&mut resp).await.unwrap();
    let status_line = resp.lines().next().unwrap_or("");
    assert!(
        status_line.contains(" 400 ") || status_line.contains(" 404 "),
        "form-POST to a `..` bucket must be rejected (400/404), not routed to \
         root.join(\"..\"); got status line: {status_line:?}"
    );
    // And it must NOT be a 2xx (the write must never land).
    assert!(
        !status_line.contains(" 200 ") && !status_line.contains(" 204 "),
        "traversal bucket write must never succeed; got: {status_line:?}"
    );
}

/// THE CI-403 REGRESSION: one presigned policy/signature uploads a BATCH of
/// distinct files (the AWS-intended `starts-with $key` pattern the ROR CI uses
/// for .zip/.sha512/.sha1). The replay guard used to 403 the 2nd file under a
/// live signature ("different fingerprint → block"). All must now succeed.
#[tokio::test]
async fn test_form_post_batch_under_one_signature_all_succeed() {
    let server = TestServer::builder()
        .auth("POSTACCESSKEY", "POSTSECRETKEY123")
        .build()
        .await;
    let client = reqwest::Client::new();
    let bucket = server.bucket();
    let endpoint = server.endpoint();
    let amz_date = "20260507T120000Z";
    let credential = "POSTACCESSKEY/20260507/us-east-1/s3/aws4_request";
    // One policy with starts-with $key → covers every file in the batch.
    let policy = serde_json::json!({
        "expiration": "2099-01-01T00:00:00.000Z",
        "conditions": [
            { "bucket": bucket },
            ["starts-with", "$key", "ror/builds/1.71/"],
            { "x-amz-algorithm": "AWS4-HMAC-SHA256" },
            { "x-amz-credential": credential },
            { "x-amz-date": amz_date },
            ["content-length-range", 1, 10485760]
        ]
    });
    let policy_b64 = base64::engine::general_purpose::STANDARD.encode(policy.to_string());
    // ONE signature, reused for the whole batch — exactly what the CI does.
    let signing_key = derive_post_signing_key("POSTSECRETKEY123", "20260507", "us-east-1");
    let signature = hex::encode(hmac_sha256(&signing_key, policy_b64.as_bytes()));

    // Three DISTINCT files (different keys + bodies) under the SAME signature.
    let files: [(&str, &[u8]); 3] = [
        ("ror/builds/1.71/app.zip", b"ZIP-CONTENT-BYTES"),
        ("ror/builds/1.71/app.zip.sha512", b"sha512-digest\n"),
        ("ror/builds/1.71/app.zip.sha1", b"sha1-digest\n"),
    ];
    for (key, content) in files {
        let form = reqwest::multipart::Form::new()
            .text("key", key)
            .text("policy", policy_b64.clone())
            .text("x-amz-algorithm", "AWS4-HMAC-SHA256")
            .text("x-amz-credential", credential)
            .text("x-amz-date", amz_date)
            .text("x-amz-signature", signature.clone())
            .part(
                "file",
                reqwest::multipart::Part::bytes(content.to_vec())
                    .file_name("f")
                    .mime_str("application/octet-stream")
                    .unwrap(),
            );
        let resp = client
            .post(format!("{}/{}", endpoint, bucket))
            .multipart(form)
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status().as_u16(),
            204,
            "batch file {key} under shared signature must succeed (was the CI 403), got {}",
            resp.status()
        );
    }

    // And an EXACT resend of one file (CI retry) is still idempotently allowed.
    let (rkey, rcontent) = files[0];
    let form = reqwest::multipart::Form::new()
        .text("key", rkey)
        .text("policy", policy_b64.clone())
        .text("x-amz-algorithm", "AWS4-HMAC-SHA256")
        .text("x-amz-credential", credential)
        .text("x-amz-date", amz_date)
        .text("x-amz-signature", signature.clone())
        .part(
            "file",
            reqwest::multipart::Part::bytes(rcontent.to_vec())
                .file_name("f")
                .mime_str("application/octet-stream")
                .unwrap(),
        );
    let resp = client
        .post(format!("{}/{}", endpoint, bucket))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        204,
        "idempotent resend of an identical file must still succeed"
    );
}

/// A delta-eligible POST (form-data) upload routed through the Phase-4.1 streaming
/// spool store (`DGP_SPOOL_THRESHOLD_BYTES=1`) must store + round-trip byte-exact.
/// POST is a separate ingest path from the s3s PUT — this guards that the spool
/// store is wired there too.
#[tokio::test]
async fn test_form_post_upload_routes_through_spool_store() {
    let server = TestServer::builder()
        .auth("POSTACCESSKEY", "POSTSECRETKEY123")
        .env("DGP_SPOOL_THRESHOLD_BYTES", "1")
        .build()
        .await;
    let client = reqwest::Client::new();
    let bucket = server.bucket();
    let endpoint = server.endpoint();
    let key = "post/rel/app.zip";
    let amz_date = "20260507T120000Z";
    let credential = "POSTACCESSKEY/20260507/us-east-1/s3/aws4_request";
    let policy = serde_json::json!({
        "expiration": "2099-01-01T00:00:00.000Z",
        "conditions": [
            { "bucket": bucket },
            ["starts-with", "$key", "post/rel/"],
            { "x-amz-algorithm": "AWS4-HMAC-SHA256" },
            { "x-amz-credential": credential },
            { "x-amz-date": amz_date },
            ["content-length-range", 1, 10485760]
        ]
    });
    let policy_b64 = base64::engine::general_purpose::STANDARD.encode(policy.to_string());
    let signing_key = derive_post_signing_key("POSTSECRETKEY123", "20260507", "us-east-1");
    let signature = hex::encode(hmac_sha256(&signing_key, policy_b64.as_bytes()));

    // A delta-eligible .zip body (>1 byte threshold → spool store path).
    let payload: Vec<u8> = (0..120_000u32).flat_map(|n| n.to_le_bytes()).collect();
    let form = reqwest::multipart::Form::new()
        .text("key", key)
        .text("policy", policy_b64)
        .text("x-amz-algorithm", "AWS4-HMAC-SHA256")
        .text("x-amz-credential", credential)
        .text("x-amz-date", amz_date)
        .text("x-amz-signature", signature)
        .part(
            "file",
            reqwest::multipart::Part::bytes(payload.clone())
                .file_name("app.zip")
                .mime_str("application/zip")
                .unwrap(),
        );
    let resp = client
        .post(format!("{}/{}", endpoint, bucket))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        204,
        "spooled POST upload should return 204, got {}",
        resp.status()
    );

    let s3 = server.s3_client().await;
    let got = s3
        .get_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .expect("GET uploaded object");
    let body = got.body.collect().await.expect("collect body").into_bytes();
    assert_eq!(
        body.as_ref(),
        &payload[..],
        "spooled POST upload must round-trip byte-exact"
    );
}

#[tokio::test]
async fn test_form_post_upload_rejects_invalid_signature() {
    let server = TestServer::builder()
        .auth("POSTACCESSKEY", "POSTSECRETKEY123")
        .build()
        .await;
    let client = reqwest::Client::new();
    let bucket = server.bucket();
    let endpoint = server.endpoint();
    let amz_date = "20260507T120000Z";
    let credential = "POSTACCESSKEY/20260507/us-east-1/s3/aws4_request";
    let policy = serde_json::json!({
        "expiration": "2099-01-01T00:00:00.000Z",
        "conditions": [
            { "bucket": bucket },
            ["starts-with", "$key", "post/uploads/"],
            { "x-amz-algorithm": "AWS4-HMAC-SHA256" },
            { "x-amz-credential": credential },
            { "x-amz-date": amz_date },
            ["content-length-range", 1, 1048576]
        ]
    });
    let policy_b64 = base64::engine::general_purpose::STANDARD.encode(policy.to_string());
    let signing_key = derive_post_signing_key("POSTSECRETKEY123", "20260507", "us-east-1");
    let signature = hex::encode(hmac_sha256(&signing_key, policy_b64.as_bytes()));
    let form = reqwest::multipart::Form::new()
        .text("key", "post/uploads/reject.txt")
        .text("policy", policy_b64)
        .text("x-amz-algorithm", "AWS4-HMAC-SHA256")
        .text("x-amz-credential", credential)
        .text("x-amz-date", amz_date)
        .text("x-amz-signature", format!("{signature}00"))
        .part(
            "file",
            reqwest::multipart::Part::bytes(b"body".to_vec())
                .file_name("reject.txt")
                .mime_str("text/plain")
                .unwrap(),
        );
    let resp = client
        .post(format!("{}/{}", endpoint, bucket))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        403,
        "Invalid POST policy signature should return 403"
    );
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("SignatureDoesNotMatch"),
        "Invalid POST signature should return SignatureDoesNotMatch XML, got: {}",
        body
    );
}

/// Browser presigned-POST builders (boto3, minio-js, aws-amplify)
/// auto-include `acl=private` when the user constructs a presigned-POST
/// without explicitly opting out. Pre-fix this returned 501
/// NotImplemented; we now silently accept it because the value matches
/// our owner-only default — same semantics as `x-amz-acl: private` on
/// regular PUT (header silently ignored).
#[tokio::test]
async fn test_form_post_upload_accepts_private_acl() {
    let server = TestServer::builder()
        .auth("POSTACCESSKEY", "POSTSECRETKEY123")
        .build()
        .await;
    let client = reqwest::Client::new();
    let bucket = server.bucket();
    let endpoint = server.endpoint();
    let key = "post/uploads/acl-private.txt";
    let amz_date = "20260507T120000Z";
    let credential = "POSTACCESSKEY/20260507/us-east-1/s3/aws4_request";
    let policy = serde_json::json!({
        "expiration": "2099-01-01T00:00:00.000Z",
        "conditions": [
            { "bucket": bucket },
            ["starts-with", "$key", "post/uploads/"],
            { "acl": "private" },
            { "x-amz-algorithm": "AWS4-HMAC-SHA256" },
            { "x-amz-credential": credential },
            { "x-amz-date": amz_date },
            ["content-length-range", 1, 1048576]
        ]
    });
    let policy_b64 = base64::engine::general_purpose::STANDARD.encode(policy.to_string());
    let signing_key = derive_post_signing_key("POSTSECRETKEY123", "20260507", "us-east-1");
    let signature = hex::encode(hmac_sha256(&signing_key, policy_b64.as_bytes()));
    let form = reqwest::multipart::Form::new()
        .text("key", key)
        .text("acl", "private")
        .text("policy", policy_b64)
        .text("x-amz-algorithm", "AWS4-HMAC-SHA256")
        .text("x-amz-credential", credential)
        .text("x-amz-date", amz_date)
        .text("x-amz-signature", signature)
        .part(
            "file",
            reqwest::multipart::Part::bytes(b"hello-acl-private".to_vec())
                .file_name("acl-private.txt")
                .mime_str("text/plain")
                .unwrap(),
        );
    let resp = client
        .post(format!("{}/{}", endpoint, bucket))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        204,
        "POST form upload with acl=private should return 204, got {}: {}",
        resp.status(),
        resp.text().await.unwrap_or_default()
    );

    // Confirm the object actually landed.
    let s3 = server.s3_client().await;
    let got = s3
        .get_object()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .expect("GET uploaded object");
    let body = got.body.collect().await.expect("collect body").into_bytes();
    assert_eq!(body.as_ref(), b"hello-acl-private");
}

/// Public-grant ACL variants are still rejected with 501 — silently
/// accepting them would lie about the object's visibility.
#[tokio::test]
async fn test_form_post_upload_rejects_public_read_acl() {
    let server = TestServer::builder()
        .auth("POSTACCESSKEY", "POSTSECRETKEY123")
        .build()
        .await;
    let client = reqwest::Client::new();
    let bucket = server.bucket();
    let endpoint = server.endpoint();
    let amz_date = "20260507T120000Z";
    let credential = "POSTACCESSKEY/20260507/us-east-1/s3/aws4_request";
    let policy = serde_json::json!({
        "expiration": "2099-01-01T00:00:00.000Z",
        "conditions": [
            { "bucket": bucket },
            ["starts-with", "$key", "post/uploads/"],
            { "x-amz-algorithm": "AWS4-HMAC-SHA256" },
            { "x-amz-credential": credential },
            { "x-amz-date": amz_date },
            ["content-length-range", 1, 1048576]
        ]
    });
    let policy_b64 = base64::engine::general_purpose::STANDARD.encode(policy.to_string());
    let signing_key = derive_post_signing_key("POSTSECRETKEY123", "20260507", "us-east-1");
    let signature = hex::encode(hmac_sha256(&signing_key, policy_b64.as_bytes()));
    let form = reqwest::multipart::Form::new()
        .text("key", "post/uploads/public.txt")
        .text("acl", "public-read")
        .text("policy", policy_b64)
        .text("x-amz-algorithm", "AWS4-HMAC-SHA256")
        .text("x-amz-credential", credential)
        .text("x-amz-date", amz_date)
        .text("x-amz-signature", signature)
        .part(
            "file",
            reqwest::multipart::Part::bytes(b"public".to_vec())
                .file_name("public.txt")
                .mime_str("text/plain")
                .unwrap(),
        );
    let resp = client
        .post(format!("{}/{}", endpoint, bucket))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        501,
        "POST form upload with acl=public-read should return 501, got {}",
        resp.status()
    );
    let body = resp.text().await.unwrap();
    assert!(
        body.contains("public-read"),
        "501 body should cite the rejected value, got: {}",
        body
    );
}

// ════════════════════════════════════════════════════════════════════
// Form-POST paranoia tests (added after the s3s migration landed)
//
// We rely heavily on browser form-POST uploads in production. The s3s
// adapter intercepts these via a method+content-type-aware middleware
// (`intercept_form_post_for_s3s` in `src/startup.rs`); the goal of
// this section is to pin every edge case that could regress the
// dispatch path, the body-collection path, or the response shape.
// ════════════════════════════════════════════════════════════════════

/// Reusable form-POST signed request builder. Returns the multipart
/// form and the URL to POST to. Caller supplies the file content +
/// any extra fields they want to override.
fn signed_post_form(
    bucket: &str,
    key: &str,
    file_bytes: &[u8],
    file_name: &str,
) -> reqwest::multipart::Form {
    let amz_date = "20260507T120000Z";
    let credential = "POSTACCESSKEY/20260507/us-east-1/s3/aws4_request";
    let policy = serde_json::json!({
        "expiration": "2099-01-01T00:00:00.000Z",
        "conditions": [
            { "bucket": bucket },
            ["starts-with", "$key", ""],
            { "x-amz-algorithm": "AWS4-HMAC-SHA256" },
            { "x-amz-credential": credential },
            { "x-amz-date": amz_date },
            ["content-length-range", 0, 10_485_760]
        ]
    });
    let policy_b64 = base64::engine::general_purpose::STANDARD.encode(policy.to_string());
    let signing_key = derive_post_signing_key("POSTSECRETKEY123", "20260507", "us-east-1");
    let signature = hex::encode(hmac_sha256(&signing_key, policy_b64.as_bytes()));
    reqwest::multipart::Form::new()
        .text("key", key.to_string())
        .text("policy", policy_b64)
        .text("x-amz-algorithm", "AWS4-HMAC-SHA256")
        .text("x-amz-credential", credential)
        .text("x-amz-date", amz_date)
        .text("x-amz-signature", signature)
        .part(
            "file",
            reqwest::multipart::Part::bytes(file_bytes.to_vec())
                .file_name(file_name.to_string())
                .mime_str("application/octet-stream")
                .unwrap(),
        )
}

/// `POST /<bucket>/<key>` (path with a nested key) must NOT be
/// intercepted by the form-POST middleware. The s3s `?delete`
/// XML batch and CreateMultipartUpload both POST to nested paths;
/// they must reach the s3s service unmodified. Verify by sending
/// `POST /<bucket>/<key>` with multipart body — it should NOT
/// route to form-POST upload (which would silently succeed) but
/// instead reach s3s and produce a non-204 error.
#[tokio::test]
async fn test_form_post_middleware_falls_through_for_nested_paths() {
    let server = TestServer::builder()
        .auth("POSTACCESSKEY", "POSTSECRETKEY123")
        .build()
        .await;
    let client = reqwest::Client::new();
    let bucket = server.bucket();
    let endpoint = server.endpoint();
    // Nested path — should NOT route to form-POST. Middleware bails
    // because `bucket.contains('/')` after trim.
    let form = signed_post_form(bucket, "form-key", b"x", "x.bin");
    let resp = client
        .post(format!("{}/{}/nested", endpoint, bucket))
        .multipart(form)
        .send()
        .await
        .unwrap();
    // The fact that we got SOME error response (not 204 OK) is the
    // assertion. Form-POST handler returns 204; s3s for unknown POST
    // on a nested path returns either 405 or NotImplemented — either
    // is fine; what matters is the form-POST handler did NOT run.
    assert_ne!(
        resp.status().as_u16(),
        204,
        "POST /<bucket>/<key> must NOT be routed to form-POST handler"
    );
}

/// `GET /<bucket>` (path matches but method is GET, not POST) must
/// NOT be intercepted — the middleware's first guard checks the
/// method.
#[tokio::test]
async fn test_form_post_middleware_falls_through_for_get_method() {
    let server = TestServer::builder()
        .auth("POSTACCESSKEY", "POSTSECRETKEY123")
        .build()
        .await;
    let client = reqwest::Client::new();
    let endpoint = server.endpoint();
    let bucket = server.bucket();
    // SDK-authenticated GET — proves the middleware doesn't claim
    // GET /<bucket>.
    let s3 = server.s3_client().await;
    let result = s3.list_objects_v2().bucket(bucket).send().await;
    assert!(
        result.is_ok(),
        "GET /<bucket> (ListObjectsV2) must not be intercepted by form-POST middleware: {:?}",
        result.err()
    );
    // Sanity: unsigned GET reaches the SigV4 verifier (auth=enabled),
    // not the form-POST handler.
    let resp = client
        .get(format!("{}/{}", endpoint, bucket))
        .send()
        .await
        .unwrap();
    assert!(
        resp.status() == reqwest::StatusCode::FORBIDDEN
            || resp.status() == reqwest::StatusCode::BAD_REQUEST,
        "unsigned GET /<bucket> should land in SigV4 verifier (403/400), got {}",
        resp.status()
    );
}

/// `POST /<bucket>` with `Content-Type: application/xml` (the
/// `?delete` DeleteObjects batch shape) must NOT be intercepted.
/// The middleware checks Content-Type starts_with "multipart/form-data".
#[tokio::test]
async fn test_form_post_middleware_falls_through_for_xml_delete_batch() {
    let server = TestServer::builder()
        .auth("POSTACCESSKEY", "POSTSECRETKEY123")
        .build()
        .await;
    let s3 = server.s3_client().await;
    let bucket = server.bucket();

    // Seed two objects, then batch-delete them. The SDK uses
    // `POST /<bucket>?delete` with `Content-Type: application/xml`.
    // If the form-POST middleware were over-eager, this would route
    // wrong and the delete would silently fail.
    use aws_sdk_s3::primitives::ByteStream;
    for k in ["delete-batch-1.bin", "delete-batch-2.bin"] {
        s3.put_object()
            .bucket(bucket)
            .key(k)
            .body(ByteStream::from_static(b"x"))
            .send()
            .await
            .unwrap();
    }
    use aws_sdk_s3::types::{Delete, ObjectIdentifier};
    let result = s3
        .delete_objects()
        .bucket(bucket)
        .delete(
            Delete::builder()
                .objects(
                    ObjectIdentifier::builder()
                        .key("delete-batch-1.bin")
                        .build()
                        .unwrap(),
                )
                .objects(
                    ObjectIdentifier::builder()
                        .key("delete-batch-2.bin")
                        .build()
                        .unwrap(),
                )
                .build()
                .unwrap(),
        )
        .send()
        .await;
    assert!(
        result.is_ok(),
        "POST /<bucket>?delete (XML batch) must reach s3s, not form-POST: {:?}",
        result.err()
    );
}

/// Form-POST with a trailing slash on the bucket path (`POST /<bucket>/`)
/// — middleware trims trailing slash, should still match.
#[tokio::test]
async fn test_form_post_with_trailing_slash_on_bucket_path() {
    let server = TestServer::builder()
        .auth("POSTACCESSKEY", "POSTSECRETKEY123")
        .build()
        .await;
    let client = reqwest::Client::new();
    let bucket = server.bucket();
    let endpoint = server.endpoint();
    let form = signed_post_form(bucket, "trailing/slash-test.bin", b"trailing", "x.bin");
    let resp = client
        .post(format!("{}/{}/", endpoint, bucket)) // <-- trailing slash
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        204,
        "POST /<bucket>/ (trailing slash) must route to form-POST: got {}",
        resp.status()
    );
}

/// Form-POST where `Content-Type` is `MULTIPART/FORM-DATA; boundary=...`
/// (uppercase). Browsers don't do this, but proxies / custom clients
/// might. The check is case-insensitive (`to_ascii_lowercase().starts_with`).
#[tokio::test]
async fn test_form_post_with_uppercase_content_type() {
    let server = TestServer::builder()
        .auth("POSTACCESSKEY", "POSTSECRETKEY123")
        .build()
        .await;
    let bucket = server.bucket();
    let endpoint = server.endpoint();

    // Manually construct the multipart body to control the
    // Content-Type header's case (reqwest's multipart helper
    // doesn't let you override the case).
    let boundary = "----paranoid-test-boundary-XYZ";
    let body = build_multipart_body(boundary, bucket, "uppercase-ct.bin", b"upper");

    let resp = reqwest::Client::new()
        .post(format!("{}/{}", endpoint, bucket))
        .header(
            "content-type",
            format!("MULTIPART/FORM-DATA; boundary={}", boundary),
        )
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        204,
        "case-insensitive Content-Type match must succeed, got {}",
        resp.status()
    );
}

/// Form-POST with extended Content-Type parameters
/// (`multipart/form-data; charset=utf-8; boundary=...`). Some HTTP
/// clients (notably older Firefox) emit the charset parameter. The
/// `starts_with("multipart/form-data")` check tolerates it.
#[tokio::test]
async fn test_form_post_with_charset_parameter_in_content_type() {
    let server = TestServer::builder()
        .auth("POSTACCESSKEY", "POSTSECRETKEY123")
        .build()
        .await;
    let bucket = server.bucket();
    let endpoint = server.endpoint();

    let boundary = "----charset-test-boundary";
    let body = build_multipart_body(boundary, bucket, "charset.bin", b"charset");

    let resp = reqwest::Client::new()
        .post(format!("{}/{}", endpoint, bucket))
        .header(
            "content-type",
            format!("multipart/form-data; charset=utf-8; boundary={}", boundary),
        )
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        204,
        "Content-Type with charset parameter must succeed, got {}",
        resp.status()
    );
}

/// Form-POST with an empty file body. The handler shouldn't crash;
/// either it succeeds (0-byte object) or rejects per the
/// content-length-range policy. Our policy allows [0, 10MB] so the
/// 0-byte case is policy-valid.
#[tokio::test]
async fn test_form_post_with_empty_file_body() {
    let server = TestServer::builder()
        .auth("POSTACCESSKEY", "POSTSECRETKEY123")
        .build()
        .await;
    let bucket = server.bucket();
    let endpoint = server.endpoint();

    let form = signed_post_form(bucket, "empty.bin", b"", "empty.bin");
    let resp = reqwest::Client::new()
        .post(format!("{}/{}", endpoint, bucket))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        204,
        "0-byte form-POST upload (policy allows [0, 10MB]) must succeed, got {}",
        resp.status()
    );

    let s3 = server.s3_client().await;
    let got = s3
        .get_object()
        .bucket(bucket)
        .key("empty.bin")
        .send()
        .await
        .expect("empty.bin must exist");
    let body = got.body.collect().await.unwrap().into_bytes();
    assert_eq!(
        body.len(),
        0,
        "uploaded object must be 0 bytes, got {} bytes",
        body.len()
    );
}

/// Build a multipart/form-data body with the given boundary. Used by
/// tests that need to control the Content-Type header's case or
/// parameters (reqwest's `multipart::Form` builder doesn't expose
/// that customization).
fn build_multipart_body(boundary: &str, bucket: &str, key: &str, file_bytes: &[u8]) -> Vec<u8> {
    let amz_date = "20260507T120000Z";
    let credential = "POSTACCESSKEY/20260507/us-east-1/s3/aws4_request";
    let policy = serde_json::json!({
        "expiration": "2099-01-01T00:00:00.000Z",
        "conditions": [
            { "bucket": bucket },
            ["starts-with", "$key", ""],
            { "x-amz-algorithm": "AWS4-HMAC-SHA256" },
            { "x-amz-credential": credential },
            { "x-amz-date": amz_date },
            ["content-length-range", 0, 10_485_760]
        ]
    });
    let policy_b64 = base64::engine::general_purpose::STANDARD.encode(policy.to_string());
    let signing_key = derive_post_signing_key("POSTSECRETKEY123", "20260507", "us-east-1");
    let signature = hex::encode(hmac_sha256(&signing_key, policy_b64.as_bytes()));

    let mut out = Vec::new();
    let crlf = b"\r\n";
    let dd_boundary = format!("--{}", boundary);
    for (name, value) in [
        ("key", key),
        ("policy", policy_b64.as_str()),
        ("x-amz-algorithm", "AWS4-HMAC-SHA256"),
        ("x-amz-credential", credential),
        ("x-amz-date", amz_date),
        ("x-amz-signature", signature.as_str()),
    ] {
        out.extend_from_slice(dd_boundary.as_bytes());
        out.extend_from_slice(crlf);
        out.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{}\"", name).as_bytes(),
        );
        out.extend_from_slice(crlf);
        out.extend_from_slice(crlf);
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(crlf);
    }
    // The file part.
    out.extend_from_slice(dd_boundary.as_bytes());
    out.extend_from_slice(crlf);
    out.extend_from_slice(
        b"Content-Disposition: form-data; name=\"file\"; filename=\"upload.bin\"",
    );
    out.extend_from_slice(crlf);
    out.extend_from_slice(b"Content-Type: application/octet-stream");
    out.extend_from_slice(crlf);
    out.extend_from_slice(crlf);
    out.extend_from_slice(file_bytes);
    out.extend_from_slice(crlf);
    // Final boundary.
    out.extend_from_slice(format!("--{}--", boundary).as_bytes());
    out.extend_from_slice(crlf);
    out
}

/// Form-POST that includes `success_action_redirect` (which we
/// explicitly reject as `NotImplemented`). Verify the rejection
/// fires correctly and produces a 501 with a useful body.
#[tokio::test]
async fn test_form_post_rejects_success_action_redirect() {
    let server = TestServer::builder()
        .auth("POSTACCESSKEY", "POSTSECRETKEY123")
        .build()
        .await;
    let bucket = server.bucket();
    let endpoint = server.endpoint();
    // Take the standard signed form, then bolt on success_action_redirect.
    let form = signed_post_form(bucket, "should-fail.bin", b"x", "x.bin")
        .text("success_action_redirect", "https://example.com/done");
    let resp = reqwest::Client::new()
        .post(format!("{}/{}", endpoint, bucket))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        501,
        "form-POST with success_action_redirect must be rejected as 501 NotImplemented, got {}",
        resp.status()
    );
}

/// Form-POST with a 1MB body — pins the body-collection path for
/// realistic payload sizes (not just the toy 5-byte tests). Helps
/// catch regressions where the body-stream-to-Bytes conversion
/// silently truncates or drops chunks.
#[tokio::test]
async fn test_form_post_with_1mb_body_round_trips() {
    let server = TestServer::builder()
        .auth("POSTACCESSKEY", "POSTSECRETKEY123")
        .build()
        .await;
    let bucket = server.bucket();
    let endpoint = server.endpoint();

    let payload: Vec<u8> = (0..1_048_576).map(|i| (i % 251) as u8).collect();
    let form = signed_post_form(bucket, "large/payload-1mb.bin", &payload, "blob.bin");
    let resp = reqwest::Client::new()
        .post(format!("{}/{}", endpoint, bucket))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 204, "1MB form-POST must succeed");

    let s3 = server.s3_client().await;
    let got = s3
        .get_object()
        .bucket(bucket)
        .key("large/payload-1mb.bin")
        .send()
        .await
        .expect("uploaded object must exist");
    let body = got.body.collect().await.unwrap().into_bytes();
    assert_eq!(body.len(), payload.len(), "round-tripped size must match");
    assert_eq!(
        body.as_ref(),
        payload.as_slice(),
        "round-tripped bytes must match (byte-exact)"
    );
}

/// Form-POST with binary (non-UTF8) file content — multipart parsing
/// must NOT treat the file body as text. Catches regressions where
/// the parser tries to validate UTF-8 on a binary part.
#[tokio::test]
async fn test_form_post_with_binary_non_utf8_content() {
    let server = TestServer::builder()
        .auth("POSTACCESSKEY", "POSTSECRETKEY123")
        .build()
        .await;
    let bucket = server.bucket();
    let endpoint = server.endpoint();

    let payload: Vec<u8> = vec![0x00, 0xFF, 0xC0, 0x80, 0x80, 0xFE, 0x00, 0xC3, 0x28];
    let form = signed_post_form(bucket, "binary.bin", &payload, "x.bin");
    let resp = reqwest::Client::new()
        .post(format!("{}/{}", endpoint, bucket))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        204,
        "binary content form-POST must succeed"
    );
    let s3 = server.s3_client().await;
    let got = s3
        .get_object()
        .bucket(bucket)
        .key("binary.bin")
        .send()
        .await
        .unwrap()
        .body
        .collect()
        .await
        .unwrap()
        .into_bytes();
    assert_eq!(&got[..], payload.as_slice());
}

/// Form-POST with a file BIGGER than the policy's `content-length-range`
/// upper bound. The policy permits [0, 10_485_760] (10 MB). A 12 MB
/// file must be rejected — proves the policy enforcement is not
/// bypassed on s3s.
#[tokio::test]
async fn test_form_post_rejects_payload_exceeding_policy_length_range() {
    let server = TestServer::builder()
        .auth("POSTACCESSKEY", "POSTSECRETKEY123")
        .build()
        .await;
    let bucket = server.bucket();
    let endpoint = server.endpoint();

    let payload: Vec<u8> = vec![0u8; 12 * 1024 * 1024];
    let form = signed_post_form(bucket, "too-big.bin", &payload, "big.bin");
    let resp = reqwest::Client::new()
        .post(format!("{}/{}", endpoint, bucket))
        .multipart(form)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    // The exact error code path depends on whether the body limit
    // layer (DefaultBodyLimit) intercepts first (would be 413) or
    // the form-POST handler's policy check fires (would be 400
    // with EntityTooLarge or similar). Both adapters may differ in
    // which layer trips first. The critical contract is: REJECTED
    // (not 2xx). Body-text checks are too brittle because the
    // body-limit layer may close the connection mid-stream and
    // return a near-empty XML stub before the handler can emit
    // a structured error.
    assert!(
        (400..500).contains(&status),
        "form-POST exceeding content-length-range must be rejected (4xx), got {}",
        status
    );

    // Sanity: object should NOT exist on the bucket — that's the
    // production safety contract.
    let s3 = server.s3_client().await;
    let head = s3
        .head_object()
        .bucket(bucket)
        .key("too-big.bin")
        .send()
        .await;
    assert!(
        head.is_err(),
        "rejected oversize POST must NOT have landed bytes on storage"
    );
}

/// Response from a successful form-POST upload should carry the
/// `ETag` header — operationally important for callers that want
/// to verify the upload landed without a separate HEAD call.
#[tokio::test]
async fn test_form_post_success_response_has_etag_header() {
    let server = TestServer::builder()
        .auth("POSTACCESSKEY", "POSTSECRETKEY123")
        .build()
        .await;
    let bucket = server.bucket();
    let endpoint = server.endpoint();
    let form = signed_post_form(bucket, "etag-check.bin", b"abc123", "etag.bin");
    let resp = reqwest::Client::new()
        .post(format!("{}/{}", endpoint, bucket))
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 204);
    let etag = resp
        .headers()
        .get("etag")
        .or_else(|| resp.headers().get("ETag"))
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    assert!(
        etag.as_deref().is_some_and(|e| !e.is_empty()),
        "form-POST 204 response must include ETag header; headers={:?}",
        resp.headers()
    );
}

// ============================================================================
// Tagging / versioning stubs
//
// The five "*_accepted" / "*_empty" / "test_delete_object_tagging" tests
// that previously lived here asserted the pre-Wave-4-M4 permissive
// behaviour (200 with the XML silently discarded, 204 on DELETE).
// Wave-4 M4 (commit e8fccf8) flipped these endpoints to honest 501 +
// 404-wins precedence; the corresponding regression coverage is in
// tests/s3_correctness_test.rs:
//   - test_object_tagging_get_returns_501
//   - test_object_tagging_put_returns_501
//   - test_bucket_tagging_returns_501
//   - test_delete_tagging_on_existing_object_returns_501
//   - test_put_bucket_versioning_returns_501
//   - test_get_bucket_versioning_on_missing_bucket_returns_404
// plus the 404-wins-over-501 trio for missing-bucket / missing-object.
// ============================================================================

// ============================================================================
// Usage Scanner API
// ============================================================================

#[tokio::test]
async fn test_scan_prefix_usage() {
    let (server, http) = test_setup().await;
    let endpoint = server.endpoint();
    let bucket = server.bucket();

    // Upload a few files under different prefixes
    put_object(
        &http,
        &endpoint,
        bucket,
        "scan/a/file1.txt",
        b"hello world".to_vec(),
        "text/plain",
    )
    .await;
    put_object(
        &http,
        &endpoint,
        bucket,
        "scan/a/file2.txt",
        b"more data here".to_vec(),
        "text/plain",
    )
    .await;
    put_object(
        &http,
        &endpoint,
        bucket,
        "scan/b/file3.txt",
        b"another file".to_vec(),
        "text/plain",
    )
    .await;

    // Login to admin API
    let admin = admin_http_client(&endpoint).await;

    // Trigger scan
    let resp = admin
        .post(format!("{}/_/api/admin/usage/scan", endpoint))
        .json(&serde_json::json!({"bucket": bucket, "prefix": "scan/"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 202, "POST scan should return 202");

    // Poll for result (max 10 seconds)
    let mut result = None;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let resp = admin
            .get(format!(
                "{}/_/api/admin/usage?bucket={}&prefix=scan/",
                endpoint, bucket
            ))
            .send()
            .await
            .unwrap();
        if resp.status().as_u16() == 200 {
            result = Some(resp.json::<serde_json::Value>().await.unwrap());
            break;
        }
    }

    let result = result.expect("Usage scan should complete within 10 seconds");
    assert_eq!(result["bucket"], bucket);
    assert_eq!(result["prefix"], "scan/");
    assert_eq!(result["total_objects"], 3);
    // total_size = 11 + 14 + 12 = 37
    assert_eq!(result["total_size"], 37);

    // Check children
    let children = result["children"].as_object().unwrap();
    assert!(
        children.contains_key("scan/a/"),
        "should have child scan/a/"
    );
    assert!(
        children.contains_key("scan/b/"),
        "should have child scan/b/"
    );
    assert_eq!(children["scan/a/"]["objects"], 2);
    assert_eq!(children["scan/b/"]["objects"], 1);
}

#[tokio::test]
async fn test_usage_cache_returns_result() {
    let (server, http) = test_setup().await;
    let endpoint = server.endpoint();
    let bucket = server.bucket();

    // Upload a file
    put_object(
        &http,
        &endpoint,
        bucket,
        "cache_test/file.txt",
        b"cached content".to_vec(),
        "text/plain",
    )
    .await;

    // Login to admin API
    let admin = admin_http_client(&endpoint).await;

    // Trigger scan
    admin
        .post(format!("{}/_/api/admin/usage/scan", endpoint))
        .json(&serde_json::json!({"bucket": bucket, "prefix": "cache_test/"}))
        .send()
        .await
        .unwrap();

    // Wait for scan to complete
    let mut completed = false;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let resp = admin
            .get(format!(
                "{}/_/api/admin/usage?bucket={}&prefix=cache_test/",
                endpoint, bucket
            ))
            .send()
            .await
            .unwrap();
        if resp.status().as_u16() == 200 {
            completed = true;
            break;
        }
    }
    assert!(completed, "First scan should complete");

    // Second GET should return cached result immediately
    let resp = admin
        .get(format!(
            "{}/_/api/admin/usage?bucket={}&prefix=cache_test/",
            endpoint, bucket
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status().as_u16(),
        200,
        "Cached result should be returned immediately"
    );
    let result = resp.json::<serde_json::Value>().await.unwrap();
    assert_eq!(result["total_objects"], 1);
    assert_eq!(result["total_size"], 14); // "cached content" = 14 bytes
}
