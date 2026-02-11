//! AWS chunked transfer encoding decoder
//!
//! When AWS SDK uses STREAMING-AWS4-HMAC-SHA256-PAYLOAD, the body is sent in a chunked format:
//!
//! ```text
//! <hex-chunk-size>;chunk-signature=<signature>\r\n
//! <chunk-data>\r\n
//! ...
//! 0;chunk-signature=<signature>\r\n
//! ```
//!
//! This module decodes that format to extract the actual payload.

use axum::body::Bytes;
use axum::http::HeaderMap;
use tracing::{debug, warn};

/// Check if the request uses AWS chunked encoding
pub fn is_aws_chunked(headers: &HeaderMap) -> bool {
    headers
        .get("x-amz-content-sha256")
        .and_then(|v| v.to_str().ok())
        .map(|v| v == "STREAMING-AWS4-HMAC-SHA256-PAYLOAD")
        .unwrap_or(false)
}

/// Get the decoded content length from headers
pub fn get_decoded_content_length(headers: &HeaderMap) -> Option<usize> {
    headers
        .get("x-amz-decoded-content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
}

/// Decode AWS chunked transfer encoding
///
/// Format:
///
/// ```text
/// <hex-chunk-size>;chunk-signature=<signature>\r\n
/// <chunk-data>\r\n
/// ```
///
/// Returns the decoded payload or None if decoding fails
pub fn decode_aws_chunked(body: &Bytes, expected_length: Option<usize>) -> Option<Bytes> {
    let mut result = Vec::with_capacity(expected_length.unwrap_or(body.len()));
    let mut pos = 0;

    while pos < body.len() {
        // Find the chunk header line (ends with \r\n)
        let header_end = find_crlf(&body[pos..])?;
        let header_line = &body[pos..pos + header_end];
        pos += header_end + 2; // Skip past \r\n

        // Parse chunk size from header: "<hex-size>;chunk-signature=..."
        let header_str = std::str::from_utf8(header_line).ok()?;
        let chunk_size_hex = header_str.split(';').next()?;
        let chunk_size = usize::from_str_radix(chunk_size_hex.trim(), 16).ok()?;

        debug!(
            "AWS chunked: parsed chunk header '{}', size={}",
            header_str, chunk_size
        );

        // End of chunks
        if chunk_size == 0 {
            break;
        }

        // Read chunk data
        if pos + chunk_size > body.len() {
            warn!(
                "AWS chunked: not enough data for chunk (need {}, have {})",
                chunk_size,
                body.len() - pos
            );
            return None;
        }
        result.extend_from_slice(&body[pos..pos + chunk_size]);
        pos += chunk_size;

        // Skip trailing \r\n after chunk data
        if pos + 2 <= body.len() && &body[pos..pos + 2] == b"\r\n" {
            pos += 2;
        }
    }

    // Verify length if expected
    if let Some(expected) = expected_length {
        if result.len() != expected {
            warn!(
                "AWS chunked: decoded length {} doesn't match expected {}",
                result.len(),
                expected
            );
            // Return anyway, some clients might have slightly different behavior
        }
    }

    debug!(
        "AWS chunked: decoded {} bytes from {} byte payload",
        result.len(),
        body.len()
    );

    Some(Bytes::from(result))
}

/// Find the position of \r\n in a byte slice
fn find_crlf(data: &[u8]) -> Option<usize> {
    (0..data.len().saturating_sub(1)).find(|&i| data[i] == b'\r' && data[i + 1] == b'\n')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_single_chunk() {
        // Format: "2a;chunk-signature=...\r\n<42 bytes of data>\r\n0;chunk-signature=...\r\n"
        let body = Bytes::from(
            "2a;chunk-signature=abc123\r\ntest content Wed Dec 17 16:48:05 UTC 2025\n\r\n0;chunk-signature=def456\r\n"
        );
        let result = decode_aws_chunked(&body, Some(42)).unwrap();
        assert_eq!(result.len(), 42);
        assert!(result.starts_with(b"test content"));
    }

    #[test]
    fn test_is_aws_chunked() {
        let mut headers = HeaderMap::new();
        assert!(!is_aws_chunked(&headers));

        headers.insert(
            "x-amz-content-sha256",
            "STREAMING-AWS4-HMAC-SHA256-PAYLOAD".parse().unwrap(),
        );
        assert!(is_aws_chunked(&headers));
    }
}
