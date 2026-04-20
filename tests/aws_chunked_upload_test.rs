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

mod common;

use common::TestServer;

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
    let client = reqwest::Client::new();
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
    let server = TestServer::builder().build().await;
    let bucket = server.bucket().to_string();

    // Deterministic 4 KiB binary payload — every byte value cycled so
    // any off-by-one framing leak shows up immediately in the diff.
    let payload: Vec<u8> = (0..4096u32).map(|i| (i & 0xff) as u8).collect();

    let wire = frame_unsigned_trailer(&payload, Some("x-amz-checksum-crc64nvme:xEkkN635Gbg="));
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
    let server = TestServer::builder().build().await;
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
    let server = TestServer::builder().build().await;
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
    let server = TestServer::builder().build().await;
    let bucket = server.bucket().to_string();

    let payload: Vec<u8> = (0..1024u32).map(|i| (i & 0xff) as u8).collect();
    let wire = frame_signed_trailer(&payload, "x-amz-checksum-sha256:dGVzdA==");

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
    let server = TestServer::builder().build().await;
    let bucket = server.bucket().to_string();

    // Same payload size as the corrupted object. Fill with a
    // deterministic pattern that doesn't accidentally look like chunk
    // framing (avoid literal "\r\n0\r\n" shapes inside the payload).
    let size = 0xc107usize;
    let payload: Vec<u8> = (0..size).map(|i| ((i * 31) & 0xff) as u8).collect();

    let wire = frame_unsigned_trailer(&payload, Some("x-amz-checksum-crc64nvme:xEkkN635Gbg="));

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
