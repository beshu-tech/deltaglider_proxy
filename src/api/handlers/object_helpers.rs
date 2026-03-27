//! Private helpers for object handlers: range parsing, conditionals, response overrides,
//! PUT/COPY internals, multipart upload parts, and body decoding.

use super::{
    base64_decode, build_object_headers, extract_content_type, extract_user_metadata, header_value,
    xml_response, AppState, ObjectQuery, S3Error,
};
use crate::iam::{AuthenticatedUser, S3Action};
use crate::types::FileMetadata;
use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use std::sync::Arc;
use tracing::{debug, info, instrument, warn};

use crate::api::aws_chunked::{decode_aws_chunked, get_decoded_content_length, is_aws_chunked};

// ---------------------------------------------------------------------------
// PUT / COPY internals
// ---------------------------------------------------------------------------

/// PUT object handler (internal)
/// Called by put_object_or_copy after validation
#[instrument(skip(state, body))]
pub(super) async fn put_object_inner(
    state: &Arc<AppState>,
    bucket: &str,
    key: &str,
    headers: &HeaderMap,
    body: &Bytes,
) -> Result<Response, S3Error> {
    info!("PUT {}/{} ({} bytes)", bucket, key, body.len());

    // Validate Content-MD5 header if present (same pattern as upload_part)
    if let Some(content_md5) = headers.get("content-md5").and_then(|v| v.to_str().ok()) {
        use md5::Digest;
        let computed = md5::Md5::digest(body);
        match base64_decode(content_md5) {
            Some(expected) => {
                if computed.as_slice() != expected.as_slice() {
                    return Err(S3Error::BadDigest);
                }
            }
            None => {
                return Err(S3Error::InvalidDigest);
            }
        }
    }

    // S3 directory marker: zero-byte object with trailing slash (e.g. "folder/")
    // Used by Cyberduck, AWS Console, etc. to create "folders".
    // Bypass delta engine and store directly on the backend.
    if key.ends_with('/') && body.is_empty() {
        info!("Creating directory marker: {}/{}", bucket, key);
        state
            .engine
            .load()
            .storage()
            .put_directory_marker(bucket, key)
            .await
            .map_err(|e| S3Error::InternalError(e.to_string()))?;
        // MD5 of empty content: d41d8cd98f00b204e9800998ecf8427e
        return Ok((
            StatusCode::OK,
            [
                ("ETag", "\"d41d8cd98f00b204e9800998ecf8427e\"".to_string()),
                ("x-amz-storage-type", "directory".to_string()),
            ],
            "",
        )
            .into_response());
    }

    let content_type = extract_content_type(headers);
    let user_metadata = extract_user_metadata(headers);

    let result = state
        .engine
        .load()
        .store(bucket, key, body, content_type, user_metadata)
        .await?;

    let storage_type = result.metadata.storage_info.label();

    debug!(
        "Stored {}/{} as {}, saved {} bytes",
        bucket,
        key,
        storage_type,
        result.metadata.file_size as i64 - result.stored_size as i64
    );

    Ok((
        StatusCode::OK,
        [
            ("ETag", result.metadata.etag()),
            ("x-amz-storage-type", storage_type.to_string()),
        ],
        "",
    )
        .into_response())
}

/// COPY object handler (internal)
/// Called by put_object_or_copy after validation
#[instrument(skip(state, auth_user))]
pub(super) async fn copy_object_inner(
    state: &Arc<AppState>,
    bucket: &str,
    key: &str,
    headers: &HeaderMap,
    auth_user: &Option<AuthenticatedUser>,
) -> Result<Response, S3Error> {
    // Get copy source
    let copy_source = headers
        .get("x-amz-copy-source")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| S3Error::InvalidRequest("Missing x-amz-copy-source header".to_string()))?;

    // Parse source: /bucket/key or bucket/key (URL-encoded)
    let copy_source = urlencoding::decode(copy_source)
        .map_err(|_| S3Error::InvalidArgument("Invalid copy source encoding".to_string()))?;
    let copy_source = copy_source.trim_start_matches('/');

    let (source_bucket, source_key) = copy_source
        .split_once('/')
        .ok_or_else(|| S3Error::InvalidArgument("Copy source must be bucket/key".to_string()))?;

    // IAM check: verify the authenticated user has read access to the copy source
    if let Some(ref user) = auth_user {
        if !user.can(S3Action::Read, source_bucket, source_key) {
            return Err(S3Error::AccessDenied);
        }
    }

    info!(
        "COPY {}/{} -> {}/{}",
        source_bucket, source_key, bucket, key
    );

    // Load engine once for the entire copy operation to ensure consistency.
    let engine = state.engine.load();

    // Check source object size before loading into memory to avoid transient
    // memory spikes if max_object_size was reduced after the object was stored.
    // Note: file_size may be 0 for unmanaged objects (fallback metadata), so we
    // also check the actual data size after retrieval below.
    let source_meta_head = engine.head(source_bucket, source_key).await?;
    if source_meta_head.file_size > engine.max_object_size() {
        return Err(S3Error::EntityTooLarge {
            size: source_meta_head.file_size,
            max: engine.max_object_size(),
        });
    }

    // Retrieve source object
    let (data, source_meta) = engine.retrieve(source_bucket, source_key).await?;

    // Double-check actual data size (metadata may report 0 for unmanaged objects)
    if data.len() as u64 > engine.max_object_size() {
        return Err(S3Error::EntityTooLarge {
            size: data.len() as u64,
            max: engine.max_object_size(),
        });
    }

    // Handle x-amz-metadata-directive: COPY (default) or REPLACE
    let metadata_directive = headers
        .get("x-amz-metadata-directive")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("COPY");

    let (dest_content_type, dest_user_metadata) =
        if metadata_directive.eq_ignore_ascii_case("REPLACE") {
            // REPLACE: use metadata from the copy request headers
            let ct = extract_content_type(headers);
            let um = extract_user_metadata(headers);
            (ct, um)
        } else {
            // COPY (default): preserve source metadata
            (
                source_meta.content_type.clone(),
                source_meta.user_metadata.clone(),
            )
        };

    // Store as new object with the chosen metadata
    let result = engine
        .store(bucket, key, &data, dest_content_type, dest_user_metadata)
        .await?;

    debug!(
        "Copied {}/{} -> {}/{} ({} bytes)",
        source_bucket,
        source_key,
        bucket,
        key,
        data.len()
    );

    let copy_result = crate::api::xml::CopyObjectResult {
        etag: result.metadata.etag(),
        last_modified: result.metadata.created_at,
    };
    let xml = copy_result.to_xml();

    Ok(xml_response(xml))
}

// ---------------------------------------------------------------------------
// Body decoding and multipart upload parts
// ---------------------------------------------------------------------------

/// Decode the request body, handling AWS chunked transfer encoding if present.
pub(super) fn decode_body(headers: &HeaderMap, body: Bytes) -> Result<Bytes, S3Error> {
    if !is_aws_chunked(headers) {
        return Ok(body);
    }

    let expected_len = get_decoded_content_length(headers);
    debug!(
        "Decoding AWS chunked payload: {} bytes, expected decoded: {:?}",
        body.len(),
        expected_len
    );
    match decode_aws_chunked(&body, expected_len) {
        Some(decoded) => {
            debug!(
                "Successfully decoded AWS chunked: {} -> {} bytes",
                body.len(),
                decoded.len()
            );
            Ok(decoded)
        }
        None => {
            warn!(
                "Failed to decode AWS chunked payload ({} bytes), rejecting request",
                body.len()
            );
            Err(S3Error::InvalidArgument(
                "Failed to decode AWS chunked transfer encoding".to_string(),
            ))
        }
    }
}

/// Handle a multipart upload part (PUT with ?partNumber&uploadId).
pub(super) fn upload_part(
    state: &Arc<AppState>,
    bucket: &str,
    key: &str,
    headers: &HeaderMap,
    part_num: u32,
    upload_id: &str,
    body: Bytes,
) -> Result<Response, S3Error> {
    info!(
        "UploadPart {}/{} part={} uploadId={}",
        bucket, key, part_num, upload_id
    );

    // Validate Content-MD5 header if present
    if let Some(content_md5) = headers.get("content-md5").and_then(|v| v.to_str().ok()) {
        use md5::Digest;
        let computed = md5::Md5::digest(&body);
        match base64_decode(content_md5) {
            Some(expected) => {
                if computed.as_slice() != expected.as_slice() {
                    return Err(S3Error::BadDigest);
                }
            }
            None => {
                return Err(S3Error::InvalidDigest);
            }
        }
    }

    let etag = state
        .multipart
        .upload_part(upload_id, bucket, key, part_num, body)?;
    Ok((StatusCode::OK, [("ETag", etag)], "").into_response())
}

/// Handle UploadPartCopy: PUT with ?partNumber&uploadId and x-amz-copy-source header.
/// Retrieves data from the source object (optionally sliced by x-amz-copy-source-range),
/// then stores it as a multipart part.
#[instrument(skip(state, auth_user))]
pub(super) async fn upload_part_copy(
    state: &Arc<AppState>,
    bucket: &str,
    key: &str,
    headers: &HeaderMap,
    part_num: u32,
    upload_id: &str,
    auth_user: &Option<AuthenticatedUser>,
) -> Result<Response, S3Error> {
    // Parse x-amz-copy-source header (reuses same logic as copy_object_inner)
    let copy_source = headers
        .get("x-amz-copy-source")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| S3Error::InvalidRequest("Missing x-amz-copy-source header".to_string()))?;

    let copy_source = urlencoding::decode(copy_source)
        .map_err(|_| S3Error::InvalidArgument("Invalid copy source encoding".to_string()))?;
    let copy_source = copy_source.trim_start_matches('/');

    let (source_bucket, source_key) = copy_source
        .split_once('/')
        .ok_or_else(|| S3Error::InvalidArgument("Copy source must be bucket/key".to_string()))?;

    // IAM check: verify the authenticated user has read access to the copy source
    if let Some(ref user) = auth_user {
        if !user.can(S3Action::Read, source_bucket, source_key) {
            return Err(S3Error::AccessDenied);
        }
    }

    info!(
        "UploadPartCopy {}/{} part={} uploadId={} from {}/{}",
        bucket, key, part_num, upload_id, source_bucket, source_key
    );

    // Retrieve source object data
    let engine = state.engine.load();
    let (data, _source_meta) = engine.retrieve(source_bucket, source_key).await?;

    // Optionally apply x-amz-copy-source-range: bytes=X-Y
    let part_data = if let Some(range_str) = headers
        .get("x-amz-copy-source-range")
        .and_then(|v| v.to_str().ok())
    {
        let range_str = range_str.strip_prefix("bytes=").ok_or_else(|| {
            S3Error::InvalidArgument("Invalid copy-source-range format".to_string())
        })?;
        let (start_str, end_str) = range_str.split_once('-').ok_or_else(|| {
            S3Error::InvalidArgument("Invalid copy-source-range format".to_string())
        })?;
        let start: usize = start_str
            .parse()
            .map_err(|_| S3Error::InvalidArgument("Invalid range start".to_string()))?;
        let end: usize = end_str
            .parse()
            .map_err(|_| S3Error::InvalidArgument("Invalid range end".to_string()))?;

        if start > end || end >= data.len() {
            return Err(S3Error::InvalidRange);
        }

        Bytes::from(data[start..=end].to_vec())
    } else {
        Bytes::from(data)
    };

    // Store as multipart part (same as upload_part)
    let etag = state
        .multipart
        .upload_part(upload_id, bucket, key, part_num, part_data)?;

    // Return CopyPartResult XML
    let result = crate::api::xml::CopyPartResult {
        etag: etag.clone(),
        last_modified: chrono::Utc::now(),
    };
    let xml = result.to_xml();

    Ok(xml_response(xml))
}

// ---------------------------------------------------------------------------
// Conditional request evaluation (If-Match, If-None-Match, etc.)
// ---------------------------------------------------------------------------

/// Parse an HTTP date string (RFC 2822 / RFC 7231 format).
fn parse_http_date(s: &str) -> Option<DateTime<Utc>> {
    chrono::DateTime::parse_from_rfc2822(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(s, "%a, %d %b %Y %H:%M:%S GMT")
                .ok()
                .map(|ndt| ndt.and_utc())
        })
}

/// Check conditional request headers against object metadata.
/// Returns Some(S3Error) if a conditional check fails, None if all pass.
/// Evaluation order per S3/HTTP spec: If-Match -> If-Unmodified-Since -> If-None-Match -> If-Modified-Since.
pub(super) fn check_conditionals(
    req_headers: &HeaderMap,
    metadata: &FileMetadata,
) -> Option<S3Error> {
    let etag = metadata.etag();
    let last_modified = metadata.created_at;

    // 1. If-Match: 412 if ETag doesn't match
    if let Some(if_match) = req_headers.get("if-match").and_then(|v| v.to_str().ok()) {
        let matches = if_match.split(',').any(|t| {
            let t = t.trim();
            t == "*" || t == etag || t.trim_matches('"') == etag.trim_matches('"')
        });
        if !matches {
            return Some(S3Error::PreconditionFailed);
        }
    }

    // 2. If-Unmodified-Since: 412 if modified after the date
    if let Some(if_unmod) = req_headers
        .get("if-unmodified-since")
        .and_then(|v| v.to_str().ok())
    {
        if let Some(date) = parse_http_date(if_unmod) {
            if last_modified > date {
                return Some(S3Error::PreconditionFailed);
            }
        }
    }

    // Format last_modified for HTTP header (used in 304 responses)
    let last_modified_str = last_modified
        .format("%a, %d %b %Y %H:%M:%S GMT")
        .to_string();

    // 3. If-None-Match: 304 if ETag matches
    if let Some(if_none_match) = req_headers
        .get("if-none-match")
        .and_then(|v| v.to_str().ok())
    {
        let matches = if_none_match.split(',').any(|t| {
            let t = t.trim();
            t == "*" || t == etag || t.trim_matches('"') == etag.trim_matches('"')
        });
        if matches {
            return Some(S3Error::NotModified {
                etag: etag.clone(),
                last_modified: last_modified_str.clone(),
            });
        }
    }

    // 4. If-Modified-Since: 304 if not modified after date
    if let Some(if_mod) = req_headers
        .get("if-modified-since")
        .and_then(|v| v.to_str().ok())
    {
        if let Some(date) = parse_http_date(if_mod) {
            if last_modified <= date {
                return Some(S3Error::NotModified {
                    etag: etag.clone(),
                    last_modified: last_modified_str,
                });
            }
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Range request parsing and response building
// ---------------------------------------------------------------------------

/// Represents a parsed byte range from the Range header.
#[derive(Debug, Clone)]
pub(super) enum ByteRange {
    /// bytes=X-Y (inclusive on both ends)
    FromTo(u64, u64),
    /// bytes=X- (from X to end)
    From(u64),
    /// bytes=-Y (last Y bytes)
    Suffix(u64),
}

/// Parse a Range header value. Returns None if the header is malformed.
pub(super) fn parse_range_header(range_str: &str) -> Option<ByteRange> {
    let range_str = range_str.strip_prefix("bytes=")?;

    if let Some(suffix) = range_str.strip_prefix('-') {
        let n: u64 = suffix.parse().ok()?;
        return Some(ByteRange::Suffix(n));
    }

    let (start_str, end_str) = range_str.split_once('-')?;
    let start: u64 = start_str.parse().ok()?;

    if end_str.is_empty() {
        Some(ByteRange::From(start))
    } else {
        let end: u64 = end_str.parse().ok()?;
        if end < start {
            None
        } else {
            Some(ByteRange::FromTo(start, end))
        }
    }
}

/// Resolve a ByteRange to concrete (start, end) inclusive offsets given a total file size.
/// Returns None if the range is not satisfiable.
pub(super) fn resolve_range(range: &ByteRange, total: u64) -> Option<(u64, u64)> {
    if total == 0 {
        return None;
    }
    match range {
        ByteRange::FromTo(start, end) => {
            if *start >= total {
                None
            } else {
                let end = std::cmp::min(*end, total - 1);
                Some((*start, end))
            }
        }
        ByteRange::From(start) => {
            if *start >= total {
                None
            } else {
                Some((*start, total - 1))
            }
        }
        ByteRange::Suffix(n) => {
            if *n == 0 {
                None
            } else if *n >= total {
                Some((0, total - 1))
            } else {
                Some((total - n, total - 1))
            }
        }
    }
}

/// Build a 206 Partial Content response from buffered data and a parsed range.
pub(super) fn build_range_response(
    data: Vec<u8>,
    metadata: &FileMetadata,
    range: &ByteRange,
    cache_hit: Option<bool>,
    query: &ObjectQuery,
) -> Result<Response, S3Error> {
    let total = data.len() as u64;
    let (start, end) = resolve_range(range, total).ok_or(S3Error::InvalidRange)?;

    let mut headers = build_object_headers(metadata);

    // Override Content-Length with the range length
    let range_len = end - start + 1;
    headers.insert("Content-Length", header_value(&range_len.to_string()));
    headers.insert(
        "Content-Range",
        header_value(&format!("bytes {}-{}/{}", start, end, total)),
    );

    if let Some(hit) = cache_hit {
        headers.insert(
            "x-deltaglider-cache",
            if hit {
                HeaderValue::from_static("hit")
            } else {
                HeaderValue::from_static("miss")
            },
        );
    }

    apply_response_overrides(&mut headers, query);

    let slice = data[start as usize..=end as usize].to_vec();
    Ok((StatusCode::PARTIAL_CONTENT, headers, slice).into_response())
}

/// Apply response header overrides from query parameters (for presigned URLs).
/// S3 supports: response-content-type, response-content-disposition,
/// response-cache-control, response-content-encoding, response-content-language, response-expires.
pub(super) fn apply_response_overrides(headers: &mut HeaderMap, query: &ObjectQuery) {
    if let Some(ref ct) = query.response_content_type {
        headers.insert("Content-Type", header_value(ct));
    }
    if let Some(ref cd) = query.response_content_disposition {
        headers.insert("Content-Disposition", header_value(cd));
    }
    if let Some(ref cc) = query.response_cache_control {
        headers.insert("Cache-Control", header_value(cc));
    }
    if let Some(ref ce) = query.response_content_encoding {
        headers.insert("Content-Encoding", header_value(ce));
    }
    if let Some(ref cl) = query.response_content_language {
        headers.insert("Content-Language", header_value(cl));
    }
    if let Some(ref ex) = query.response_expires {
        headers.insert("Expires", header_value(ex));
    }
}
