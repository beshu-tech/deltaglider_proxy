//! AWS Signature Version 4 (SigV4) verification middleware
//!
//! When proxy credentials are configured, all incoming requests must carry a valid
//! `Authorization: AWS4-HMAC-SHA256 ...` header signed with the proxy's credentials,
//! or use a presigned URL with SigV4 query string authentication.
//!
//! The middleware reconstructs the canonical request from the incoming HTTP request,
//! derives the signing key from the proxy's secret access key, and compares the
//! computed signature against the one provided by the client.
//!
//! For GET/HEAD/DELETE requests the payload hash is either `UNSIGNED-PAYLOAD` or
//! the SHA-256 of the empty string — this is a header-only check. For PUT requests,
//! the payload hash in `x-amz-content-sha256` is trusted (the body is verified
//! downstream by the engine's SHA-256 check).

use super::S3Error;
use axum::body::Body;
use axum::http::Request;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tracing::{debug, warn};

type HmacSha256 = Hmac<Sha256>;

/// Shared auth configuration extracted from Config at startup.
#[derive(Clone)]
pub struct AuthConfig {
    pub access_key_id: String,
    pub secret_access_key: String,
}

/// Common intermediate representation for SigV4 parameters,
/// populated from either Authorization header or presigned URL query params.
struct SigV4Params {
    access_key: String,
    credential_scope: String,
    signed_headers: String,
    signature: String,
    amz_date: String,
    payload_hash: String,
    canonical_query_string: String,
}

impl SigV4Params {
    /// Extract SigV4 parameters from the Authorization header path.
    #[allow(clippy::result_large_err)]
    fn from_headers(request: &Request<Body>) -> Result<Self, Response> {
        let auth_header = match request.headers().get("authorization") {
            Some(v) => match v.to_str() {
                Ok(s) => s.to_string(),
                Err(_) => {
                    warn!("SigV4: invalid Authorization header encoding");
                    return Err(S3Error::InvalidArgument(
                        "Invalid Authorization header encoding".to_string(),
                    )
                    .into_response());
                }
            },
            None => {
                debug!("SigV4: no Authorization header, rejecting");
                return Err(S3Error::AccessDenied.into_response());
            }
        };

        let parsed = match parse_auth_header(&auth_header) {
            Some(p) => p,
            None => {
                warn!("SigV4: failed to parse Authorization header");
                return Err(S3Error::InvalidArgument(
                    "Invalid Authorization header format".to_string(),
                )
                .into_response());
            }
        };

        let payload_hash = request
            .headers()
            .get("x-amz-content-sha256")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("UNSIGNED-PAYLOAD")
            .to_string();

        let amz_date = get_amz_date(request.headers());

        let query_string = request.uri().query().unwrap_or("");
        let canonical_query_string = build_canonical_query_string(query_string, &[]);

        Ok(SigV4Params {
            access_key: parsed.access_key,
            credential_scope: parsed.credential_scope,
            signed_headers: parsed.signed_headers,
            signature: parsed.signature,
            amz_date,
            payload_hash,
            canonical_query_string,
        })
    }

    /// Extract SigV4 parameters from presigned URL query params.
    #[allow(clippy::result_large_err)]
    fn from_query(request: &Request<Body>) -> Result<Self, Response> {
        let query_string = request.uri().query().unwrap_or("");

        let params: std::collections::HashMap<String, String> = query_string
            .split('&')
            .filter(|s| !s.is_empty())
            .filter_map(|pair| {
                let (k, v) = pair.split_once('=')?;
                Some((percent_decode(k), percent_decode(v)))
            })
            .collect();

        let credential = params.get("X-Amz-Credential").cloned().unwrap_or_default();
        let signed_headers = params
            .get("X-Amz-SignedHeaders")
            .cloned()
            .unwrap_or_default();
        let signature = params.get("X-Amz-Signature").cloned().unwrap_or_default();
        let amz_date = params.get("X-Amz-Date").cloned().unwrap_or_default();
        let expires = params.get("X-Amz-Expires").cloned().unwrap_or_default();

        if credential.is_empty() || signature.is_empty() {
            debug!("SigV4 presigned: missing credential or signature");
            return Err(S3Error::AccessDenied.into_response());
        }

        // Parse credential: AKID/date/region/service/aws4_request
        let (access_key, credential_scope) = match credential.split_once('/') {
            Some(pair) => pair,
            None => {
                warn!("SigV4 presigned: invalid credential format");
                return Err(S3Error::AccessDenied.into_response());
            }
        };

        // Check expiration — hard-fail on parse errors
        if !expires.is_empty() {
            let expires_secs: i64 = expires.parse().map_err(|_| {
                warn!("SigV4 presigned: unparseable X-Amz-Expires: {:?}", expires);
                S3Error::InvalidArgument(format!("Invalid X-Amz-Expires: {}", expires))
                    .into_response()
            })?;

            let request_time = chrono::NaiveDateTime::parse_from_str(&amz_date, "%Y%m%dT%H%M%SZ")
                .map_err(|_| {
                warn!("SigV4 presigned: unparseable X-Amz-Date: {:?}", amz_date);
                S3Error::InvalidArgument(format!("Invalid X-Amz-Date: {}", amz_date))
                    .into_response()
            })?;

            let request_utc = request_time.and_utc();
            let now = chrono::Utc::now();
            let expiry = request_utc + chrono::Duration::seconds(expires_secs);
            if now > expiry {
                debug!("SigV4 presigned: URL expired (expired at {})", expiry);
                return Err(S3Error::AccessDenied.into_response());
            }
        }

        let canonical_query_string =
            build_canonical_query_string(query_string, &["X-Amz-Signature"]);

        Ok(SigV4Params {
            access_key: access_key.to_string(),
            credential_scope: credential_scope.to_string(),
            signed_headers,
            signature,
            amz_date,
            payload_hash: "UNSIGNED-PAYLOAD".to_string(),
            canonical_query_string,
        })
    }
}

/// Verify the SigV4 signature against the reconstructed canonical request.
#[allow(clippy::result_large_err)]
fn verify_signature(
    params: &SigV4Params,
    auth: &AuthConfig,
    method: &str,
    uri_path: &str,
    headers: &axum::http::HeaderMap,
    uri: &axum::http::Uri,
) -> Result<(), Response> {
    // Verify the access key matches
    if params.access_key != auth.access_key_id {
        debug!("SigV4: access key mismatch");
        return Err(S3Error::AccessDenied.into_response());
    }

    // Build sorted signed headers
    let signed_headers_list: Vec<&str> = params.signed_headers.split(';').collect();
    let mut header_pairs: Vec<(String, String)> = Vec::new();
    for header_name in &signed_headers_list {
        let value = if *header_name == "host" {
            // HTTP/1.1 sends Host header; HTTP/2 uses :authority pseudo-header
            // which hyper exposes via the request URI authority, not the headers map.
            headers
                .get("host")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
                .or_else(|| uri.authority().map(|a| a.to_string()))
                .unwrap_or_default()
        } else {
            headers
                .get(*header_name)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string()
        };
        // Trim header values per SigV4 spec (collapse spaces, trim)
        let trimmed = value.split_whitespace().collect::<Vec<_>>().join(" ");
        header_pairs.push((header_name.to_string(), trimmed));
    }
    header_pairs.sort_by(|a, b| a.0.cmp(&b.0));

    let canonical_headers: String = header_pairs
        .iter()
        .map(|(k, v)| format!("{}:{}\n", k, v))
        .collect();

    // Build the canonical request
    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        method,
        uri_encode_path(uri_path),
        params.canonical_query_string,
        canonical_headers,
        params.signed_headers,
        params.payload_hash
    );

    debug!("SigV4 canonical request:\n{}", canonical_request);

    // Hash the canonical request
    let canonical_request_hash = hex::encode(Sha256::digest(canonical_request.as_bytes()));

    // Build the string to sign
    let string_to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{}\n{}",
        params.amz_date, params.credential_scope, canonical_request_hash
    );

    debug!("SigV4 string to sign:\n{}", string_to_sign);

    // Derive the signing key and compute signature
    let signing_key = derive_signing_key(&auth.secret_access_key, &params.credential_scope);
    let computed_signature = hex::encode(hmac_sha256(&signing_key, string_to_sign.as_bytes()));

    debug!(
        "SigV4: computed={}, provided={}",
        &computed_signature[..8],
        &params.signature[..std::cmp::min(8, params.signature.len())]
    );

    if computed_signature != params.signature {
        warn!("SigV4: signature mismatch");
        return Err(S3Error::SignatureDoesNotMatch.into_response());
    }

    debug!("SigV4: signature verified successfully");
    Ok(())
}

/// Check whether the query string contains presigned URL parameters.
/// Uses proper key-level parsing instead of substring matching.
fn has_presigned_query_params(query: &str) -> bool {
    query.split('&').filter(|s| !s.is_empty()).any(|pair| {
        let key = pair.split_once('=').map(|(k, _)| k).unwrap_or(pair);
        percent_decode(key) == "X-Amz-Algorithm"
    })
}

/// Axum middleware that verifies SigV4 signatures when auth is configured.
///
/// Inserted as a layer around the router. If `auth` is `None` (no credentials
/// configured), all requests pass through unchanged.
pub async fn sigv4_auth_middleware(
    request: Request<Body>,
    next: Next,
) -> Result<Response, Response> {
    // Auth config is stored in request extensions by the Extension layer
    let auth = request
        .extensions()
        .get::<Option<Arc<AuthConfig>>>()
        .cloned()
        .flatten();

    let auth = match auth {
        Some(a) => a,
        None => return Ok(next.run(request).await),
    };

    // Log every incoming request before auth check for debugging
    debug!(
        "Incoming request: {} {} (has auth header: {})",
        request.method(),
        request.uri(),
        request.headers().contains_key("authorization")
    );

    // Let CORS preflight requests through — browsers send OPTIONS without credentials
    if request.method() == axum::http::Method::OPTIONS {
        return Ok(next.run(request).await);
    }

    // Let HEAD / through unauthenticated — S3 clients (Cyberduck, etc.) use this as
    // a connection probe before sending real requests. Real S3 returns 200 for HEAD /.
    if request.method() == axum::http::Method::HEAD && request.uri().path() == "/" {
        debug!("SigV4: allowing unauthenticated HEAD / (connection probe)");
        return Ok(next.run(request).await);
    }

    // Let /health through unauthenticated — used by Docker HEALTHCHECK and load balancers
    if request.uri().path() == "/health" {
        return Ok(next.run(request).await);
    }

    let query_string = request.uri().query().unwrap_or("");
    let params = if has_presigned_query_params(query_string) {
        SigV4Params::from_query(&request)?
    } else {
        SigV4Params::from_headers(&request)?
    };

    let method = request.method().as_str().to_string();
    let uri_path = request.uri().path().to_string();
    let uri = request.uri().clone();

    verify_signature(&params, &auth, &method, &uri_path, request.headers(), &uri)?;

    Ok(next.run(request).await)
}

/// Parsed components of an AWS SigV4 Authorization header.
struct ParsedAuthHeader {
    access_key: String,
    credential_scope: String,
    signed_headers: String,
    signature: String,
}

/// Parse the Authorization header value.
///
/// Format: `AWS4-HMAC-SHA256 Credential=AKID/20260101/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature=abcdef...`
fn parse_auth_header(header: &str) -> Option<ParsedAuthHeader> {
    let header = header.trim();
    if !header.starts_with("AWS4-HMAC-SHA256") {
        return None;
    }

    let parts = header.strip_prefix("AWS4-HMAC-SHA256")?.trim();

    let mut credential = None;
    let mut signed_headers = None;
    let mut signature = None;

    for part in parts.split(',') {
        let part = part.trim();
        if let Some(val) = part.strip_prefix("Credential=") {
            credential = Some(val.trim().to_string());
        } else if let Some(val) = part.strip_prefix("SignedHeaders=") {
            signed_headers = Some(val.trim().to_string());
        } else if let Some(val) = part.strip_prefix("Signature=") {
            signature = Some(val.trim().to_string());
        }
    }

    let credential = credential?;
    let signed_headers = signed_headers?;
    let signature = signature?;

    // Parse credential: AKID/date/region/service/aws4_request
    let (access_key, credential_scope) = credential.split_once('/')?;

    Some(ParsedAuthHeader {
        access_key: access_key.to_string(),
        credential_scope: credential_scope.to_string(),
        signed_headers,
        signature,
    })
}

/// Get the x-amz-date header value (or Date header as fallback).
fn get_amz_date(headers: &axum::http::HeaderMap) -> String {
    headers
        .get("x-amz-date")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .or_else(|| {
            headers
                .get("date")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string())
        })
        .unwrap_or_default()
}

/// Build sorted canonical query string from raw query.
/// Keys in `exclude_keys` are omitted (used for presigned URLs to strip X-Amz-Signature).
fn build_canonical_query_string(query: &str, exclude_keys: &[&str]) -> String {
    if query.is_empty() {
        return String::new();
    }

    let mut pairs: Vec<(String, String)> = query
        .split('&')
        .filter(|s| !s.is_empty())
        .filter_map(|pair| {
            if let Some((k, v)) = pair.split_once('=') {
                let k_decoded = percent_decode(k);
                if exclude_keys.contains(&k_decoded.as_str()) {
                    return None;
                }
                let v_decoded = percent_decode(v);
                Some((uri_encode(&k_decoded, true), uri_encode(&v_decoded, true)))
            } else {
                let k_decoded = percent_decode(pair);
                if exclude_keys.contains(&k_decoded.as_str()) {
                    return None;
                }
                Some((uri_encode(&k_decoded, true), String::new()))
            }
        })
        .collect();

    pairs.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect::<Vec<_>>()
        .join("&")
}

/// Percent-decode a URI component (e.g. `%2F` → `/`).
fn percent_decode(input: &str) -> String {
    let mut result = Vec::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&input[i + 1..i + 3], 16) {
                result.push(byte);
                i += 3;
                continue;
            }
        }
        result.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&result).into_owned()
}

/// URI-encode a path component per SigV4 spec.
/// Path encoding preserves '/' characters.
/// Decodes first to avoid double-encoding (e.g. `%20` → `%2520`).
fn uri_encode_path(path: &str) -> String {
    path.split('/')
        .map(|segment| uri_encode(&percent_decode(segment), false))
        .collect::<Vec<_>>()
        .join("/")
}

/// URI-encode a string per SigV4 spec (RFC 3986).
/// Unreserved characters: A-Z a-z 0-9 - _ . ~
fn uri_encode(input: &str, encode_slash: bool) -> String {
    let mut encoded = String::with_capacity(input.len() * 3);
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char);
            }
            b'/' if !encode_slash => {
                encoded.push('/');
            }
            _ => {
                encoded.push_str(&format!("%{:02X}", byte));
            }
        }
    }
    encoded
}

/// Derive the SigV4 signing key from the secret access key and credential scope.
///
/// credential_scope format: `20260101/us-east-1/s3/aws4_request`
fn derive_signing_key(secret_access_key: &str, credential_scope: &str) -> Vec<u8> {
    let parts: Vec<&str> = credential_scope.split('/').collect();
    // parts: [date, region, service, "aws4_request"]
    let date = parts.first().copied().unwrap_or("");
    let region = parts.get(1).copied().unwrap_or("");
    let service = parts.get(2).copied().unwrap_or("");

    let k_secret = format!("AWS4{}", secret_access_key);
    let k_date = hmac_sha256(k_secret.as_bytes(), date.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, service.as_bytes());
    hmac_sha256(&k_service, b"aws4_request")
}

/// Compute HMAC-SHA256.
fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC can take key of any size");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_auth_header() {
        let header = "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, Signature=fe5f80f77d5fa3beca038a248ff027d0445342fe2855ddc963176630326f1024";
        let parsed = parse_auth_header(header).unwrap();
        assert_eq!(parsed.access_key, "AKIAIOSFODNN7EXAMPLE");
        assert_eq!(
            parsed.credential_scope,
            "20130524/us-east-1/s3/aws4_request"
        );
        assert_eq!(
            parsed.signed_headers,
            "host;range;x-amz-content-sha256;x-amz-date"
        );
        assert_eq!(
            parsed.signature,
            "fe5f80f77d5fa3beca038a248ff027d0445342fe2855ddc963176630326f1024"
        );
    }

    #[test]
    fn test_parse_auth_header_invalid() {
        assert!(parse_auth_header("Basic dXNlcjpwYXNz").is_none());
        assert!(parse_auth_header("").is_none());
    }

    #[test]
    fn test_derive_signing_key() {
        // AWS SigV4 test vector from AWS documentation
        let key = derive_signing_key(
            "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
            "20130524/us-east-1/s3/aws4_request",
        );
        // This should produce a deterministic 32-byte key
        assert_eq!(key.len(), 32);
    }

    #[test]
    fn test_canonical_query_string() {
        assert_eq!(build_canonical_query_string("", &[]), "");
        assert_eq!(build_canonical_query_string("a=1&b=2", &[]), "a=1&b=2");
        // Should sort by key
        assert_eq!(build_canonical_query_string("b=2&a=1", &[]), "a=1&b=2");
        // Handles list-type parameter
        assert_eq!(
            build_canonical_query_string("list-type=2&prefix=test", &[]),
            "list-type=2&prefix=test"
        );
        // Pre-encoded values should not be double-encoded
        assert_eq!(
            build_canonical_query_string("delimiter=%2F&list-type=2&prefix=", &[]),
            "delimiter=%2F&list-type=2&prefix="
        );
    }

    #[test]
    fn test_canonical_query_string_with_exclusions() {
        assert_eq!(
            build_canonical_query_string("a=1&X-Amz-Signature=abc&b=2", &["X-Amz-Signature"]),
            "a=1&b=2"
        );
        // Exclude multiple keys
        assert_eq!(
            build_canonical_query_string("a=1&drop=x&b=2&skip=y", &["drop", "skip"]),
            "a=1&b=2"
        );
    }

    #[test]
    fn test_has_presigned_query_params() {
        assert!(has_presigned_query_params(
            "X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=foo"
        ));
        assert!(!has_presigned_query_params("list-type=2&prefix=test"));
        assert!(!has_presigned_query_params(""));
        // Should not match substring (e.g. a value containing "X-Amz-Algorithm=")
        assert!(!has_presigned_query_params("foo=X-Amz-Algorithm%3Dbar"));
    }

    #[test]
    fn test_uri_encode() {
        assert_eq!(uri_encode("hello", false), "hello");
        assert_eq!(uri_encode("hello world", false), "hello%20world");
        assert_eq!(uri_encode("a/b", true), "a%2Fb");
        assert_eq!(uri_encode("a/b", false), "a/b");
    }

    #[test]
    fn test_uri_encode_path() {
        assert_eq!(uri_encode_path("/bucket/key"), "/bucket/key");
        assert_eq!(
            uri_encode_path("/bucket/my file.zip"),
            "/bucket/my%20file.zip"
        );
        // Pre-encoded paths must not be double-encoded
        assert_eq!(
            uri_encode_path("/bucket/my%20file.zip"),
            "/bucket/my%20file.zip"
        );
    }

    #[test]
    fn test_hmac_sha256_deterministic() {
        let result1 = hmac_sha256(b"key", b"data");
        let result2 = hmac_sha256(b"key", b"data");
        assert_eq!(result1, result2);
        assert_eq!(result1.len(), 32);
    }
}
