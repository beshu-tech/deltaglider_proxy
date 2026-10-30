// SPDX-License-Identifier: BUSL-1.1

//! Integration coverage for AWS streaming-chunked object uploads.
//!
//! This test file reproduces — and locks down — the production corruption
//! caused by `STREAMING-UNSIGNED-PAYLOAD-TRAILER` uploads being stored
//! verbatim (chunk framing + trailer bytes included) because the proxy's
//! `is_aws_chunked` predicate only recognised the legacy
//! `STREAMING-AWS4-HMAC-SHA256-PAYLOAD` value.
//!
//! We exercise each streaming variant end-to-end:
//!
//! | variant                                        | per-chunk sig | trailer |
//! |------------------------------------------------|:-------------:|:-------:|
//! | `STREAMING-AWS4-HMAC-SHA256-PAYLOAD`           | yes           | no      |
//! | `STREAMING-AWS4-HMAC-SHA256-PAYLOAD-TRAILER`   | yes           | yes     |
//! | `STREAMING-UNSIGNED-PAYLOAD-TRAILER`           | no            | yes     |
//!
//! For each variant we PUT a chunk-framed body and GET the object back,
//! asserting byte-exact equality with the original payload. A regression
//! of the production bug would surface here as extra bytes (the framing)
//! in the GET response.
//!
//! The tests run against an open-access TestServer (no SigV4 auth) so
//! we can craft raw HTTP requests with arbitrary body framing. The
//! chunk-signature extension is not verified by the proxy today (SigV4
//! checks cover headers, not per-chunk content), so we include the
//! extension as a literal string where the variant demands it; AWS SDKs
//! do the same.

use crate::common;

use base64::Engine as _;
use common::TestServer;
use s3s::crypto::Checksum as _;

fn b64(digest: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(digest)
}

/// The `x-amz-checksum-crc64nvme` trailer line of `payload`.
fn crc64nvme_trailer(payload: &[u8]) -> String {
    format!(
        "x-amz-checksum-crc64nvme:{}",
        b64(&s3s::crypto::Crc64Nvme::checksum(payload))
    )
}

/// Build a STREAMING-UNSIGNED-PAYLOAD-TRAILER chunk-framed body:
///
/// ```text
/// <hex-size>\r\n<data>\r\n
/// ...
/// 0\r\n[<trailer>\r\n]\r\n
/// ```
fn frame_unsigned_trailer(payload: &[u8], trailer_line: Option<&str>) -> Vec<u8> {
    let mut wire = Vec::with_capacity(payload.len() + 64);
    wire.extend_from_slice(format!("{:x}\r\n", payload.len()).as_bytes());
    wire.extend_from_slice(payload);
    wire.extend_from_slice(b"\r\n0\r\n");
    if let Some(line) = trailer_line {
        wire.extend_from_slice(line.as_bytes());
        wire.extend_from_slice(b"\r\n");
    }
    wire.extend_from_slice(b"\r\n");
    wire
}

/// Build a legacy-signed STREAMING-AWS4-HMAC-SHA256-PAYLOAD body:
///
/// ```text
/// <hex>;chunk-signature=<dummy>\r\n<data>\r\n
/// 0;chunk-signature=<dummy>\r\n\r\n
/// ```
///
/// The proxy doesn't verify per-chunk signatures (it relies on SigV4 on
/// the headers), so we use dummy hex. This matches the wire shape of
/// real SDK output; only the signature *value* is synthetic.
fn frame_signed_legacy(payload: &[u8]) -> Vec<u8> {
    let mut wire = Vec::with_capacity(payload.len() + 128);
    wire.extend_from_slice(
        format!(
            "{:x};chunk-signature=0000000000000000000000000000000000000000000000000000000000000001\r\n",
            payload.len()
        )
        .as_bytes(),
    );
    wire.extend_from_slice(payload);
    wire.extend_from_slice(b"\r\n0;chunk-signature=0000000000000000000000000000000000000000000000000000000000000002\r\n\r\n");
    wire
}

/// Build a signed-trailer body (`STREAMING-AWS4-HMAC-SHA256-PAYLOAD-TRAILER`):
fn frame_signed_trailer(payload: &[u8], trailer_line: &str) -> Vec<u8> {
    let mut wire = Vec::with_capacity(payload.len() + 256);
    wire.extend_from_slice(
        format!(
            "{:x};chunk-signature=0000000000000000000000000000000000000000000000000000000000000001\r\n",
            payload.len()
        )
        .as_bytes(),
    );
    wire.extend_from_slice(payload);
    wire.extend_from_slice(b"\r\n0;chunk-signature=0000000000000000000000000000000000000000000000000000000000000002\r\n");
    wire.extend_from_slice(trailer_line.as_bytes());
    wire.extend_from_slice(b"\r\n\r\n");
    wire
}

/// Upload `payload` to the test server via a streaming-chunked PUT using
/// the supplied framing function + content-sha256 value, then download
/// the object and return the raw bytes. The caller asserts equality with
/// the original `payload` — any non-match indicates the proxy failed to
/// decode the framing before storing.
async fn put_then_get(
    server: &TestServer,
    bucket: &str,
    key: &str,
    payload: &[u8],
    wire_body: Vec<u8>,
    content_sha256: &str,
) -> Vec<u8> {
    let client = server.http();
    let put_url = format!("{}/{}/{}", server.endpoint(), bucket, key);

    let put_resp = client
        .put(&put_url)
        .header("x-amz-content-sha256", content_sha256)
        .header("x-amz-decoded-content-length", payload.len().to_string())
        .header("content-length", wire_body.len().to_string())
        .body(wire_body)
        .send()
        .await
        .expect("PUT request failed to send");
    assert!(
        put_resp.status().is_success(),
        "PUT failed: status={} body={:?}",
        put_resp.status(),
        put_resp.text().await.ok()
    );

    let get_resp = client
        .get(&put_url)
        .send()
        .await
        .expect("GET request failed to send");
    assert!(
        get_resp.status().is_success(),
        "GET failed: status={}",
        get_resp.status()
    );
    get_resp.bytes().await.expect("GET body").to_vec()
}

#[tokio::test]
async fn streaming_unsigned_payload_trailer_roundtrips_byte_exact() {
    // This is the exact variant that corrupted production: AWS SDK v3's
    // default for flexible-checksum uploads. A bucket populated with
    // payloads framed this way must decode cleanly.
    // Open access: a hand-built aws-chunked body without the headers and
    // chunk signatures a signed client sends. SigV4 streaming: `authed` below.
    let server = TestServer::builder().open_access().build().await;
    let bucket = server.bucket().to_string();

    // Deterministic 4 KiB binary payload — every byte value cycled so
    // any off-by-one framing leak shows up immediately in the diff.
    let payload: Vec<u8> = (0..4096u32).map(|i| (i & 0xff) as u8).collect();

    let wire = frame_unsigned_trailer(&payload, Some(&crc64nvme_trailer(&payload)));
    let retrieved = put_then_get(
        &server,
        &bucket,
        "unsigned-trailer.bin",
        &payload,
        wire,
        "STREAMING-UNSIGNED-PAYLOAD-TRAILER",
    )
    .await;

    assert_eq!(
        retrieved, payload,
        "byte-for-byte round trip must match payload for STREAMING-UNSIGNED-PAYLOAD-TRAILER"
    );
}

#[tokio::test]
async fn streaming_unsigned_payload_trailer_without_trailer_line_roundtrips() {
    // Some SDKs emit the unsigned streaming content-sha256 value but
    // send no trailer line (just `0\r\n\r\n`). Must still decode.
    // Open access: a hand-built aws-chunked body without the headers and
    // chunk signatures a signed client sends. SigV4 streaming: `authed` below.
    let server = TestServer::builder().open_access().build().await;
    let bucket = server.bucket().to_string();

    let payload = b"hello-from-trailerless-upload".to_vec();
    let wire = frame_unsigned_trailer(&payload, None);

    let retrieved = put_then_get(
        &server,
        &bucket,
        "unsigned-no-trailer.txt",
        &payload,
        wire,
        "STREAMING-UNSIGNED-PAYLOAD-TRAILER",
    )
    .await;

    assert_eq!(retrieved, payload);
}

#[tokio::test]
async fn streaming_legacy_signed_payload_roundtrips_byte_exact() {
    // Legacy pre-v3 SDK path. This worked before the fix too; covered
    // here to make sure the refactored decoder didn't regress it.
    // Open access: a hand-built aws-chunked body without the headers and
    // chunk signatures a signed client sends. SigV4 streaming: `authed` below.
    let server = TestServer::builder().open_access().build().await;
    let bucket = server.bucket().to_string();

    let payload: Vec<u8> = (0..2048u32).map(|i| (i & 0xff) as u8).collect();
    let wire = frame_signed_legacy(&payload);

    let retrieved = put_then_get(
        &server,
        &bucket,
        "signed-legacy.bin",
        &payload,
        wire,
        "STREAMING-AWS4-HMAC-SHA256-PAYLOAD",
    )
    .await;

    assert_eq!(retrieved, payload);
}

#[tokio::test]
async fn streaming_signed_payload_trailer_roundtrips_byte_exact() {
    // Signed + trailing checksum. Used by AWS SDKs configured for both
    // SigV4 per-chunk signing AND flexible checksums.
    // Open access: a hand-built aws-chunked body without the headers and
    // chunk signatures a signed client sends. SigV4 streaming: `authed` below.
    let server = TestServer::builder().open_access().build().await;
    let bucket = server.bucket().to_string();

    let payload: Vec<u8> = (0..1024u32).map(|i| (i & 0xff) as u8).collect();
    let trailer = format!(
        "x-amz-checksum-sha256:{}",
        b64(&s3s::crypto::Sha256::checksum(&payload))
    );
    let wire = frame_signed_trailer(&payload, &trailer);

    let retrieved = put_then_get(
        &server,
        &bucket,
        "signed-trailer.bin",
        &payload,
        wire,
        "STREAMING-AWS4-HMAC-SHA256-PAYLOAD-TRAILER",
    )
    .await;

    assert_eq!(retrieved, payload);
}

/// Full-fidelity reproduction of the production corruption pattern: a
/// 0xc107-byte binary payload framed with `STREAMING-UNSIGNED-PAYLOAD-
/// TRAILER` and a CRC64NVME trailer, exactly the shape of the
/// `Activation-keys.cy.ts.mp4` file the user reported.
///
/// Before the fix, the GET would return a 0xc107 + 52 byte body
/// containing the framing bytes. This test locks down the fix: the GET
/// body must equal the raw payload, byte for byte, length and content.
#[tokio::test]
async fn production_corruption_pattern_is_fixed() {
    // Open access: a hand-built aws-chunked body without the headers and
    // chunk signatures a signed client sends. SigV4 streaming: `authed` below.
    let server = TestServer::builder().open_access().build().await;
    let bucket = server.bucket().to_string();

    // Same payload size as the corrupted object. Fill with a
    // deterministic pattern that doesn't accidentally look like chunk
    // framing (avoid literal "\r\n0\r\n" shapes inside the payload).
    let size = 0xc107usize;
    let payload: Vec<u8> = (0..size).map(|i| ((i * 31) & 0xff) as u8).collect();

    let wire = frame_unsigned_trailer(&payload, Some(&crc64nvme_trailer(&payload)));

    // Framed wire body must be exactly 52 bytes longer than the
    // payload: `<hex>\r\n` (6: `c107\r\n`) + `\r\n` after data (2) +
    // `0\r\n` (3) + trailer line + `\r\n` (39) + final `\r\n` (2) = 52.
    assert_eq!(
        wire.len(),
        payload.len() + 52,
        "framed body length must match the production pattern"
    );

    let retrieved = put_then_get(
        &server,
        &bucket,
        "prod-repro.mp4",
        &payload,
        wire,
        "STREAMING-UNSIGNED-PAYLOAD-TRAILER",
    )
    .await;

    assert_eq!(
        retrieved.len(),
        payload.len(),
        "GET body length must match the payload length (no framing bytes leaked)"
    );
    assert_eq!(
        retrieved, payload,
        "GET body must equal payload byte-for-byte"
    );
}

/// A trailer checksum that does not match the decoded body is refused
/// with `BadDigest`, and nothing is stored.
#[tokio::test]
async fn unsigned_trailer_with_a_wrong_checksum_is_bad_digest() {
    // Open access: the adapter de-frames this body itself (s3s does not).
    let server = TestServer::builder().open_access().build().await;
    let payload = b"payload-with-a-checksum".to_vec();
    let wire = frame_unsigned_trailer(&payload, Some(&crc64nvme_trailer(b"other bytes")));
    let url = format!("{}/{}/bad-trailer.bin", server.endpoint(), server.bucket());
    let resp = server
        .http()
        .put(&url)
        .header("x-amz-content-sha256", "STREAMING-UNSIGNED-PAYLOAD-TRAILER")
        .header("x-amz-trailer", "x-amz-checksum-crc64nvme")
        .header("x-amz-decoded-content-length", payload.len().to_string())
        .header("content-length", wire.len().to_string())
        .body(wire)
        .send()
        .await
        .expect("PUT sends");
    assert_eq!(resp.status(), 400);
    assert!(resp
        .text()
        .await
        .unwrap()
        .contains("<Code>BadDigest</Code>"));
    let get = server.http().get(&url).send().await.expect("GET sends");
    assert_eq!(get.status(), 404, "a refused body must not be stored");
}

// ============================================================================
// Review C1: the SAME framing through a server that verifies SigV4.
//
// The tests above run in open-access mode without an Authorization header, so
// s3s never decodes the framing and the adapter must. Real SDK traffic is
// header-signed: s3s verifies every chunk signature and strips the framing,
// and the adapter used to decode a second time → 400 on every signed
// streaming PutObject and UploadPart. These requests carry REAL seed and
// chunk signatures, the shape production emits.
// ============================================================================

mod signed {
    use super::common::TestServer;
    use aws_sdk_s3::error::ProvideErrorMetadata as _;
    use hmac::{Hmac, Mac};
    use s3s::crypto::Checksum as _;
    use sha2::{Digest, Sha256};

    const AK: &str = "chunk-ak";
    const SK: &str = "chunk-secret-key";
    const REGION: &str = "us-east-1";
    const EMPTY_SHA: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
        let mut mac = Hmac::<Sha256>::new_from_slice(key).unwrap();
        mac.update(data);
        mac.finalize().into_bytes().to_vec()
    }

    fn sha_hex(data: &[u8]) -> String {
        hex::encode(Sha256::digest(data))
    }

    fn signing_key(date: &str) -> Vec<u8> {
        let k = hmac(format!("AWS4{SK}").as_bytes(), date.as_bytes());
        let k = hmac(&k, REGION.as_bytes());
        let k = hmac(&k, b"s3");
        hmac(&k, b"aws4_request")
    }

    fn uri_encode(s: &str) -> String {
        let mut out = String::new();
        for b in s.bytes() {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
                out.push(b as char);
            } else {
                out.push_str(&format!("%{b:02X}"));
            }
        }
        out
    }

    /// Send a `STREAMING-AWS4-HMAC-SHA256-PAYLOAD` PUT with valid seed and
    /// chunk signatures. `query` is `(name, value)` pairs, already sorted.
    async fn signed_streaming_put(
        server: &TestServer,
        path: &str,
        query: &[(&str, &str)],
        payload: &[u8],
    ) -> reqwest::Response {
        signed_put(server, path, query, payload, None).await
    }

    /// A header-signed `STREAMING-UNSIGNED-PAYLOAD-TRAILER` PUT (the AWS SDK
    /// default): s3s de-frames it and hands over the trailer.
    async fn signed_trailer_put(
        server: &TestServer,
        path: &str,
        query: &[(&str, &str)],
        payload: &[u8],
        trailer: (&str, &str),
    ) -> reqwest::Response {
        signed_put(server, path, query, payload, Some(trailer)).await
    }

    async fn signed_put(
        server: &TestServer,
        path: &str,
        query: &[(&str, &str)],
        payload: &[u8],
        trailer: Option<(&str, &str)>,
    ) -> reqwest::Response {
        let now = chrono::Utc::now();
        let ts = now.format("%Y%m%dT%H%M%SZ").to_string();
        let date = &ts[..8];
        let scope = format!("{date}/{REGION}/s3/aws4_request");
        let host = server.endpoint().trim_start_matches("http://").to_string();
        let content_sha = if trailer.is_some() {
            "STREAMING-UNSIGNED-PAYLOAD-TRAILER"
        } else {
            "STREAMING-AWS4-HMAC-SHA256-PAYLOAD"
        };
        let canonical_query = query
            .iter()
            .map(|(k, v)| format!("{}={}", uri_encode(k), uri_encode(v)))
            .collect::<Vec<_>>()
            .join("&");
        let mut signed_headers =
            "content-encoding;host;x-amz-content-sha256;x-amz-date;x-amz-decoded-content-length"
                .to_string();
        let mut canonical_headers = format!(
            "content-encoding:aws-chunked\nhost:{host}\nx-amz-content-sha256:{content_sha}\n\
             x-amz-date:{ts}\nx-amz-decoded-content-length:{}\n",
            payload.len()
        );
        if let Some((name, _)) = trailer {
            signed_headers.push_str(";x-amz-trailer");
            canonical_headers.push_str(&format!("x-amz-trailer:{name}\n"));
        }
        let canonical_request = format!(
            "PUT\n{path}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{content_sha}"
        );
        let key = signing_key(date);
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{ts}\n{scope}\n{}",
            sha_hex(canonical_request.as_bytes())
        );
        let seed = hex::encode(hmac(&key, string_to_sign.as_bytes()));

        // One data chunk plus the terminating empty chunk, each signed over
        // the previous signature (the chunk-signature chain).
        let mut body = Vec::new();
        let mut prev = seed.clone();
        if let Some((name, value)) = trailer {
            // Unsigned chunks, then the trailer block.
            body.extend_from_slice(format!("{:x}\r\n", payload.len()).as_bytes());
            body.extend_from_slice(payload);
            body.extend_from_slice(format!("\r\n0\r\n{name}:{value}\r\n\r\n").as_bytes());
        } else {
            for chunk in [payload, &[][..]] {
                let sts = format!(
                    "AWS4-HMAC-SHA256-PAYLOAD\n{ts}\n{scope}\n{prev}\n{EMPTY_SHA}\n{}",
                    sha_hex(chunk)
                );
                let sig = hex::encode(hmac(&key, sts.as_bytes()));
                body.extend_from_slice(
                    format!("{:x};chunk-signature={sig}\r\n", chunk.len()).as_bytes(),
                );
                body.extend_from_slice(chunk);
                body.extend_from_slice(b"\r\n");
                prev = sig;
            }
        }

        let url = if canonical_query.is_empty() {
            format!("{}{path}", server.endpoint())
        } else {
            format!("{}{path}?{canonical_query}", server.endpoint())
        };
        let mut request = reqwest::Client::new().put(url);
        if let Some((name, _)) = trailer {
            request = request.header("x-amz-trailer", name);
        }
        request
            .header(
                "authorization",
                format!(
                    "AWS4-HMAC-SHA256 Credential={AK}/{scope}, SignedHeaders={signed_headers}, \
                     Signature={seed}"
                ),
            )
            .header("content-encoding", "aws-chunked")
            .header("x-amz-content-sha256", content_sha)
            .header("x-amz-date", &ts)
            .header("x-amz-decoded-content-length", payload.len().to_string())
            .header("content-length", body.len().to_string())
            .body(body)
            .send()
            .await
            .expect("signed streaming PUT sends")
    }

    async fn authed_server() -> TestServer {
        TestServer::builder().auth(AK, SK).build().await
    }

    #[tokio::test]
    async fn signed_streaming_put_object_roundtrips() {
        let server = authed_server().await;
        let payload: Vec<u8> = (0..5000u32).map(|i| ((i * 7) & 0xff) as u8).collect();
        let path = format!("/{}/signed-stream.bin", server.bucket());
        let resp = signed_streaming_put(&server, &path, &[], &payload).await;
        let status = resp.status();
        assert!(
            status.is_success(),
            "signed streaming PUT must succeed, got {status}: {:?}",
            resp.text().await.ok()
        );
        let got = server
            .s3_client()
            .await
            .get_object()
            .bucket(server.bucket())
            .key("signed-stream.bin")
            .send()
            .await
            .expect("GET")
            .body
            .collect()
            .await
            .unwrap()
            .into_bytes();
        assert_eq!(got.as_ref(), payload.as_slice(), "no framing may leak");
    }

    #[tokio::test]
    async fn signed_streaming_upload_part_roundtrips() {
        let server = authed_server().await;
        let s3 = server.s3_client().await;
        let bucket = server.bucket().to_string();
        let key = "signed-mpu.bin";
        let upload_id = s3
            .create_multipart_upload()
            .bucket(&bucket)
            .key(key)
            .send()
            .await
            .expect("create MPU")
            .upload_id()
            .unwrap()
            .to_string();
        let payload: Vec<u8> = (0..3000u32).map(|i| ((i * 13) & 0xff) as u8).collect();
        let path = format!("/{bucket}/{key}");
        let resp = signed_streaming_put(
            &server,
            &path,
            &[("partNumber", "1"), ("uploadId", &upload_id)],
            &payload,
        )
        .await;
        let status = resp.status();
        let etag = resp
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        assert!(
            status.is_success(),
            "signed streaming UploadPart must succeed, got {status}: {:?}",
            resp.text().await.ok()
        );
        s3.complete_multipart_upload()
            .bucket(&bucket)
            .key(key)
            .upload_id(&upload_id)
            .multipart_upload(
                aws_sdk_s3::types::CompletedMultipartUpload::builder()
                    .parts(
                        aws_sdk_s3::types::CompletedPart::builder()
                            .part_number(1)
                            .e_tag(etag.expect("part ETag"))
                            .build(),
                    )
                    .build(),
            )
            .send()
            .await
            .expect("complete MPU");
        let got = s3
            .get_object()
            .bucket(&bucket)
            .key(key)
            .send()
            .await
            .expect("GET")
            .body
            .collect()
            .await
            .unwrap()
            .into_bytes();
        assert_eq!(got.as_ref(), payload.as_slice(), "no framing may leak");
    }

    async fn assert_bad_digest(resp: reqwest::Response) {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        assert_eq!(status, 400, "{text}");
        assert!(text.contains("<Code>BadDigest</Code>"), "{text}");
    }

    /// The trailer s3s takes off a signed request's body is checked.
    #[tokio::test]
    async fn signed_trailer_checksum_is_verified() {
        let server = authed_server().await;
        let payload: Vec<u8> = (0..4000u32).map(|i| ((i * 11) & 0xff) as u8).collect();
        let crc32c = super::b64(&s3s::crypto::Crc32c::checksum(&payload));
        let name = "x-amz-checksum-crc32c";
        let path = format!("/{}/trailer-ok.bin", server.bucket());
        let resp = signed_trailer_put(&server, &path, &[], &payload, (name, &crc32c)).await;
        let status = resp.status();
        assert!(
            status.is_success(),
            "right trailer must succeed, got {status}: {:?}",
            resp.text().await.ok()
        );

        let wrong = super::b64(&s3s::crypto::Crc32c::checksum(b"other"));
        let path = format!("/{}/trailer-bad.bin", server.bucket());
        assert_bad_digest(signed_trailer_put(&server, &path, &[], &payload, (name, &wrong)).await)
            .await;
        let s3 = server.s3_client().await;
        let head = s3
            .head_object()
            .bucket(server.bucket())
            .key("trailer-bad.bin")
            .send()
            .await;
        assert!(head.is_err(), "a refused body must not be stored");

        // UploadPart takes the same check.
        let upload_id = s3
            .create_multipart_upload()
            .bucket(server.bucket())
            .key("trailer-mpu.bin")
            .send()
            .await
            .expect("create MPU")
            .upload_id()
            .unwrap()
            .to_string();
        let path = format!("/{}/trailer-mpu.bin", server.bucket());
        let query = [("partNumber", "1"), ("uploadId", upload_id.as_str())];
        assert_bad_digest(
            signed_trailer_put(&server, &path, &query, &payload, (name, &wrong)).await,
        )
        .await;
    }

    /// The AWS SDK's `x-amz-checksum-*` request header is checked.
    #[tokio::test]
    async fn sdk_checksum_header_is_verified() {
        let server = authed_server().await;
        let s3 = server.s3_client().await;
        let body = b"sdk body".to_vec();
        let right = super::b64(&s3s::crypto::Sha256::checksum(&body));
        s3.put_object()
            .bucket(server.bucket())
            .key("sdk-ok.txt")
            .checksum_sha256(&right)
            .body(body.clone().into())
            .send()
            .await
            .expect("right checksum is accepted");

        let wrong = super::b64(&s3s::crypto::Sha256::checksum(b"other"));
        let err = s3
            .put_object()
            .bucket(server.bucket())
            .key("sdk-bad.txt")
            .checksum_sha256(&wrong)
            .body(body.clone().into())
            .send()
            .await
            .expect_err("wrong checksum is refused");
        assert_eq!(err.into_service_error().code(), Some("BadDigest"));

        let upload_id = s3
            .create_multipart_upload()
            .bucket(server.bucket())
            .key("sdk-mpu.bin")
            .send()
            .await
            .expect("create MPU")
            .upload_id()
            .unwrap()
            .to_string();
        let wrong_crc = super::b64(&s3s::crypto::Crc32::checksum(b"other"));
        let err = s3
            .upload_part()
            .bucket(server.bucket())
            .key("sdk-mpu.bin")
            .upload_id(&upload_id)
            .part_number(1)
            .checksum_crc32(&wrong_crc)
            .body(body.into())
            .send()
            .await
            .expect_err("wrong part checksum is refused");
        assert_eq!(err.into_service_error().code(), Some("BadDigest"));
    }
}
