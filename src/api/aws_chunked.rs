//! AWS chunked transfer-encoding decoder.
//!
//! AWS SDKs upload object bodies using a framed on-the-wire format when
//! they advertise a `STREAMING-*` value in the `x-amz-content-sha256`
//! header. There are several closely-related variants; all of them must
//! be decoded by the proxy before the payload is handed to storage,
//! otherwise the framing bytes end up concatenated into the object and
//! every subsequent GET serves corrupt content.
//!
//! # Variants recognised
//!
//! | `x-amz-content-sha256` value                     | Per-chunk sig | Trailer | Source |
//! |--------------------------------------------------|:-------------:|:-------:|--------|
//! | `STREAMING-AWS4-HMAC-SHA256-PAYLOAD`             | yes           | no      | Legacy SigV4 streaming |
//! | `STREAMING-AWS4-HMAC-SHA256-PAYLOAD-TRAILER`     | yes           | yes     | SigV4 streaming + flexible checksum |
//! | `STREAMING-UNSIGNED-PAYLOAD-TRAILER`             | no            | yes     | **Default in modern AWS SDK v3+ with flexible checksums** |
//! | `STREAMING-AWS4-ECDSA-P256-SHA256-PAYLOAD`       | yes           | no      | SigV4a (regional) streaming |
//! | `STREAMING-AWS4-ECDSA-P256-SHA256-PAYLOAD-TRAILER` | yes         | yes     | SigV4a streaming + flexible checksum |
//!
//! The detection predicate uses the `STREAMING-` prefix so new variants
//! (future regional-sig schemes, etc.) are recognised without a code
//! change. The decoder itself is variant-agnostic: it parses the chunk
//! size before any optional `;chunk-signature=...` extension, and after
//! the zero-terminator it consumes any trailer lines up to the final
//! empty line.
//!
//! # Wire format (all variants)
//!
//! ```text
//! <hex-size>[;chunk-signature=<sig>]\r\n
//! <chunk-bytes>\r\n
//! ...
//! 0[;chunk-signature=<sig>]\r\n
//! [<trailer-name>:<trailer-value>\r\n]...
//! \r\n
//! ```
//!
//! The decoder rejects any input that doesn't match this grammar; it does
//! not fall back to "pass the body through unchanged" — that is the bug
//! this module exists to prevent.
//!
//! # Integrity (trailer checksums)
//!
//! When a trailer carries `x-amz-checksum-{crc32,crc32c,sha1,sha256,crc64nvme}`,
//! the client expects the server to verify the checksum against the
//! decoded body. Today we parse and log the trailer but do NOT verify —
//! adding verification requires pulling in crc32c/crc64nvme crates and is
//! a separate piece of work. The decoded body is still handed downstream
//! unchanged, which is strictly better than the pre-fix behaviour (where
//! the trailer bytes silently became part of the object). Tracked as a
//! follow-up; tests below include a trailer-present case to lock the
//! parsing behaviour so the follow-up can only add verification, not
//! change the decoding shape.

use axum::body::Bytes;
use axum::http::HeaderMap;
use tracing::{debug, warn};

/// Prefix that identifies every AWS streaming-payload content-sha256
/// value. Any header value starting with this string indicates the
/// request body is chunk-framed and must be decoded before storage.
pub const STREAMING_PREFIX: &str = "STREAMING-";

/// Return `true` when the request's `x-amz-content-sha256` marks the
/// body as AWS chunk-framed.
///
/// The predicate uses the `STREAMING-` prefix so we automatically
/// recognise new AWS variants without a code change — the alternative
/// (an allow-list of exact strings) is what silently corrupted
/// production before: uploads with `STREAMING-UNSIGNED-PAYLOAD-TRAILER`
/// slipped past an allow-list that only knew about the legacy
/// `STREAMING-AWS4-HMAC-SHA256-PAYLOAD` value.
pub fn is_aws_chunked(headers: &HeaderMap) -> bool {
    headers
        .get("x-amz-content-sha256")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.starts_with(STREAMING_PREFIX))
        .unwrap_or(false)
}

/// Read the decoded content length hint the SDK provides alongside the
/// streaming body. Used to pre-size the decode buffer and to reject
/// truncated inputs whose decoded length doesn't match what was
/// advertised.
pub fn get_decoded_content_length(headers: &HeaderMap) -> Option<usize> {
    headers
        .get("x-amz-decoded-content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
}

/// Decode an AWS streaming-chunked body into the underlying payload.
///
/// Returns `None` on any structural inconsistency — truncated chunks,
/// missing CRLFs, bad hex sizes, missing terminator, missing final
/// empty line, or a decoded length that disagrees with
/// `expected_length`. **The decoder never falls back to passing the
/// raw body through** — that behaviour is what produced the production
/// corruption this module guards against.
///
/// # Format
///
/// ```text
/// <hex-size>[;chunk-signature=<sig>]\r\n
/// <chunk-bytes>\r\n            (repeat zero or more times)
/// 0[;chunk-signature=<sig>]\r\n
/// [<trailer-name>:<trailer-value>\r\n]...
/// \r\n
/// ```
///
/// The optional `;chunk-signature=...` extension is ignored — we trust
/// SigV4 verification on the headers to decide whether the caller is
/// authorised. Chunk-level signature verification is a separate piece
/// of work and is not implemented here (nor in the pre-fix code).
pub fn decode_aws_chunked(body: &Bytes, expected_length: Option<usize>) -> Option<Bytes> {
    let mut result = Vec::with_capacity(expected_length.unwrap_or(body.len()));
    let mut pos = 0;

    loop {
        // Each chunk starts with a header line. A missing header means
        // the body ended before we saw a terminator — reject.
        let header_end = find_crlf(&body[pos..])?;
        let header_line = &body[pos..pos + header_end];
        pos += header_end + 2; // past \r\n

        // Chunk size is the hex token before the first `;`. The rest of
        // the line (`;chunk-signature=<sig>`, future extensions) is
        // ignored — only the size matters for framing.
        let header_str = std::str::from_utf8(header_line).ok()?;
        let size_token = header_str.split(';').next()?.trim();
        let chunk_size = usize::from_str_radix(size_token, 16).ok()?;

        debug!(
            "aws_chunked: header='{}' size={} pos={}",
            header_str, chunk_size, pos
        );

        if chunk_size == 0 {
            // Zero-size chunk is the terminator. Everything that
            // follows is a sequence of trailer lines ending with an
            // empty line (\r\n). Consume them (and their content) so
            // the trailer bytes never leak into storage.
            //
            // Accepted shapes:
            //   0\r\n\r\n                                  (no trailers)
            //   0;chunk-signature=...\r\n\r\n              (legacy signed)
            //   0\r\n x-amz-checksum-crc32:...\r\n\r\n     (unsigned+trailer)
            //   0;chunk-signature=...\r\n key:v\r\n ... \r\n   (signed+trailer)
            consume_trailers(body, &mut pos)?;
            break;
        }

        // Read the chunk body. Bail if the declared size exceeds what
        // remains — truncation must surface as a decode failure, not a
        // silent partial store.
        if pos + chunk_size > body.len() {
            warn!(
                "aws_chunked: truncated chunk at pos={} (need {}, have {})",
                pos,
                chunk_size,
                body.len() - pos
            );
            return None;
        }
        result.extend_from_slice(&body[pos..pos + chunk_size]);
        pos += chunk_size;

        // Every data chunk is followed by a CRLF. Tolerating a missing
        // CRLF here is the exact laxity that masked the bug in
        // production; require it.
        if pos + 2 > body.len() || &body[pos..pos + 2] != b"\r\n" {
            warn!("aws_chunked: missing CRLF after chunk body at pos={}", pos);
            return None;
        }
        pos += 2;
    }

    if let Some(expected) = expected_length {
        if result.len() != expected {
            warn!(
                "aws_chunked: decoded length {} != expected {}, rejecting",
                result.len(),
                expected
            );
            return None;
        }
    }

    debug!(
        "aws_chunked: decoded {} bytes from {} byte payload",
        result.len(),
        body.len()
    );

    Some(Bytes::from(result))
}

/// Consume zero or more trailer lines after a zero-size terminator
/// chunk, ending with an empty line (`\r\n`). Returns `Some(())` when
/// the trailer section is well-formed, `None` if the input is
/// truncated or malformed.
///
/// We log each trailer we see — specifically the `x-amz-checksum-*`
/// family — so operators can see that flexible-checksum uploads are
/// arriving; verifying those checksums against the decoded body is
/// tracked as a follow-up (see module doc).
fn consume_trailers(body: &Bytes, pos: &mut usize) -> Option<()> {
    loop {
        let line_end = find_crlf(&body[*pos..])?;
        let line = &body[*pos..*pos + line_end];
        *pos += line_end + 2; // past \r\n

        if line.is_empty() {
            // Empty line terminates the trailer section.
            return Some(());
        }

        if let Ok(line_str) = std::str::from_utf8(line) {
            if line_str.to_ascii_lowercase().starts_with("x-amz-checksum-") {
                // Trailer checksum present but not yet verified — see
                // module docs. Logged at debug (not warn) because this
                // is the expected path for modern SDK uploads.
                debug!("aws_chunked: received trailer checksum '{}'", line_str);
            } else {
                debug!("aws_chunked: received trailer line '{}'", line_str);
            }
        }
    }
}

/// Find the position of `\r\n` in a byte slice. Returns `None` if the
/// sequence isn't present — callers treat that as a truncation error.
fn find_crlf(data: &[u8]) -> Option<usize> {
    (0..data.len().saturating_sub(1)).find(|&i| data[i] == b'\r' && data[i + 1] == b'\n')
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── is_aws_chunked ────────────────────────────────────────────────

    #[test]
    fn is_aws_chunked_accepts_all_known_streaming_variants() {
        for value in [
            "STREAMING-AWS4-HMAC-SHA256-PAYLOAD",
            "STREAMING-AWS4-HMAC-SHA256-PAYLOAD-TRAILER",
            "STREAMING-UNSIGNED-PAYLOAD-TRAILER",
            "STREAMING-AWS4-ECDSA-P256-SHA256-PAYLOAD",
            "STREAMING-AWS4-ECDSA-P256-SHA256-PAYLOAD-TRAILER",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert("x-amz-content-sha256", value.parse().unwrap());
            assert!(
                is_aws_chunked(&headers),
                "expected {value} to be recognised as STREAMING-*"
            );
        }
    }

    #[test]
    fn is_aws_chunked_rejects_non_streaming_values() {
        for value in [
            "UNSIGNED-PAYLOAD",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert("x-amz-content-sha256", value.parse().unwrap());
            assert!(
                !is_aws_chunked(&headers),
                "expected {value:?} to NOT match STREAMING-*"
            );
        }

        // Missing header altogether → not chunked.
        assert!(!is_aws_chunked(&HeaderMap::new()));
    }

    // ── Legacy signed variant (STREAMING-AWS4-HMAC-SHA256-PAYLOAD) ────

    #[test]
    fn decode_legacy_signed_single_chunk() {
        let body = Bytes::from(
            "2a;chunk-signature=abc123\r\ntest content Wed Dec 17 16:48:05 UTC 2025\n\r\n0;chunk-signature=def456\r\n\r\n"
        );
        let result = decode_aws_chunked(&body, Some(42)).unwrap();
        assert_eq!(result.len(), 42);
        assert!(result.starts_with(b"test content"));
    }

    #[test]
    fn decode_legacy_signed_multi_chunk() {
        let body = Bytes::from(
            "5;chunk-signature=aaa\r\nhello\r\n6;chunk-signature=bbb\r\n world\r\n0;chunk-signature=ccc\r\n\r\n"
        );
        let result = decode_aws_chunked(&body, Some(11)).unwrap();
        assert_eq!(result.as_ref(), b"hello world");
    }

    #[test]
    fn decode_legacy_signed_empty_payload() {
        let body = Bytes::from("0;chunk-signature=abc\r\n\r\n");
        let result = decode_aws_chunked(&body, Some(0)).unwrap();
        assert!(result.is_empty());
    }

    // ── Unsigned trailer variant (STREAMING-UNSIGNED-PAYLOAD-TRAILER) ─
    // This is what the AWS SDK v3 defaults to when flexible checksums
    // are enabled — the variant that triggered the production bug.

    #[test]
    fn decode_unsigned_trailer_single_chunk() {
        let body = Bytes::from("b\r\nhello world\r\n0\r\n\r\n");
        let result = decode_aws_chunked(&body, Some(11)).unwrap();
        assert_eq!(result.as_ref(), b"hello world");
    }

    #[test]
    fn decode_unsigned_trailer_with_checksum() {
        // Two chunks, followed by zero-terminator and a trailer line
        // carrying the CRC64NVME checksum AWS v3 SDKs emit by default.
        let body = Bytes::from(
            "5\r\nhello\r\n6\r\n world\r\n0\r\nx-amz-checksum-crc64nvme:xEkkN635Gbg=\r\n\r\n",
        );
        let result = decode_aws_chunked(&body, Some(11)).unwrap();
        assert_eq!(result.as_ref(), b"hello world");
    }

    #[test]
    fn decode_unsigned_trailer_empty_payload_with_trailer() {
        // Empty upload with only a trailer checksum — pathological but
        // valid per spec.
        let body = Bytes::from("0\r\nx-amz-checksum-crc32:AAAAAA==\r\n\r\n");
        let result = decode_aws_chunked(&body, Some(0)).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn decode_unsigned_trailer_empty_payload_no_trailer() {
        let body = Bytes::from("0\r\n\r\n");
        let result = decode_aws_chunked(&body, Some(0)).unwrap();
        assert!(result.is_empty());
    }

    // ── Signed trailer variant ────────────────────────────────────────

    #[test]
    fn decode_signed_trailer_with_checksum() {
        let body = Bytes::from(
            "5;chunk-signature=aaa\r\nhello\r\n0;chunk-signature=bbb\r\nx-amz-checksum-sha256:deadbeef\r\n\r\n"
        );
        let result = decode_aws_chunked(&body, Some(5)).unwrap();
        assert_eq!(result.as_ref(), b"hello");
    }

    // ── Malformed inputs (must reject with None, never pass through) ──

    #[test]
    fn decode_rejects_garbage_input() {
        let body = Bytes::from("this is not chunked data at all");
        assert!(decode_aws_chunked(&body, None).is_none());
    }

    #[test]
    fn decode_rejects_truncated_chunk() {
        // Header says 0x64 = 100 bytes but only 5 follow.
        let body = Bytes::from("64;chunk-signature=abc\r\nhello");
        assert!(decode_aws_chunked(&body, None).is_none());
    }

    #[test]
    fn decode_rejects_missing_crlf_after_chunk() {
        // 5 bytes of data but no trailing CRLF before the terminator.
        let body = Bytes::from("5\r\nhello0\r\n\r\n");
        assert!(decode_aws_chunked(&body, None).is_none());
    }

    #[test]
    fn decode_rejects_bad_hex() {
        let body = Bytes::from("zz\r\n\r\n");
        assert!(decode_aws_chunked(&body, None).is_none());
    }

    #[test]
    fn decode_rejects_missing_trailer_terminator() {
        // Zero-chunk followed by a trailer line but no final empty
        // line. The decoder must wait for the `\r\n` empty line and
        // reject when the body ends first.
        let body = Bytes::from("0\r\nx-amz-checksum-crc32:AAAAAA==\r\n");
        assert!(decode_aws_chunked(&body, None).is_none());
    }

    #[test]
    fn decode_rejects_length_mismatch() {
        // Body says 5 bytes but decoded length claims 3 expected.
        let body = Bytes::from("5\r\nhello\r\n0\r\n\r\n");
        assert!(decode_aws_chunked(&body, Some(3)).is_none());
    }

    // ── Production-failure regression test ─────────────────────────────
    //
    // Rebuilds the exact byte pattern of the corrupted object the user
    // pointed at: a 49415-byte payload ("<0xc107> bytes") framed with
    // STREAMING-UNSIGNED-PAYLOAD-TRAILER and a CRC64NVME trailer. The
    // pre-fix decoder didn't recognise the content-sha256 value so the
    // entire framed body, including the `c107\r\n` prefix and
    // `0\r\n<checksum>\r\n\r\n` suffix, was stored verbatim as the
    // object. This test locks the fix: a single-shot decode must
    // produce exactly the 49415-byte raw payload.

    #[test]
    fn decode_matches_production_corruption_pattern() {
        // Payload same size as the corrupted object's real MP4 content.
        let payload: Vec<u8> = (0..0xc107u32).map(|i| (i & 0xff) as u8).collect();

        // Assemble a STREAMING-UNSIGNED-PAYLOAD-TRAILER wire body with
        // the real CRC64NVME-style trailer line.
        let mut wire = Vec::with_capacity(payload.len() + 64);
        wire.extend_from_slice(b"c107\r\n");
        wire.extend_from_slice(&payload);
        wire.extend_from_slice(b"\r\n0\r\nx-amz-checksum-crc64nvme:xEkkN635Gbg=\r\n\r\n");

        // Wire body should be 52 bytes longer than the payload (6 byte
        // header `c107\r\n`, 2 byte `\r\n` after data, 3 byte `0\r\n`,
        // 39 byte trailer `x-amz-checksum-crc64nvme:xEkkN635Gbg=\r\n`,
        // 2 byte terminating `\r\n` = 52).
        assert_eq!(wire.len(), payload.len() + 52);

        let decoded = decode_aws_chunked(&Bytes::from(wire), Some(payload.len())).unwrap();
        assert_eq!(
            decoded.as_ref(),
            payload.as_slice(),
            "decoded body must equal the raw payload byte-for-byte"
        );
    }

    // ── 3-chunk unsigned-trailer, large payload ───────────────────────

    #[test]
    fn decode_three_chunks_unsigned_trailer_large() {
        // Three chunks of 1000 bytes each, then a trailer. Stresses the
        // loop + CRLF handling without being so large the test is slow.
        let chunk_size = 1000usize;
        let total = chunk_size * 3;
        let payload: Vec<u8> = (0..total).map(|i| (i % 251) as u8).collect();

        let mut wire = Vec::with_capacity(total + 64);
        for i in 0..3 {
            wire.extend_from_slice(format!("{:x}\r\n", chunk_size).as_bytes());
            wire.extend_from_slice(&payload[i * chunk_size..(i + 1) * chunk_size]);
            wire.extend_from_slice(b"\r\n");
        }
        wire.extend_from_slice(b"0\r\nx-amz-checksum-crc32c:AAAAAA==\r\n\r\n");

        let decoded = decode_aws_chunked(&Bytes::from(wire), Some(total)).unwrap();
        assert_eq!(decoded.len(), total);
        assert_eq!(decoded.as_ref(), payload.as_slice());
    }
}
