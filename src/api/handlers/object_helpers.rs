//! Private helpers for object handlers: range parsing, conditionals, response overrides,
//! PUT/COPY internals, multipart upload parts, and body decoding.

use super::{
    base64_decode, build_object_headers, extract_content_type, extract_user_metadata, header_value,
    xml_response, AppState, ObjectQuery, S3Error,
};
use crate::event_outbox::{current_unix_seconds, EventKind, EventSource, NewEvent};
use crate::iam::{AuthenticatedUser, S3Action};
use crate::types::FileMetadata;
use axum::body::Bytes;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use std::sync::Arc;
use tracing::{debug, info, instrument, warn};

use crate::api::aws_chunked::{decode_aws_chunked, get_decoded_content_length, is_aws_chunked};

pub(super) async fn enqueue_object_event(state: &Arc<AppState>, event: NewEvent) {
    enqueue_object_events(state, &[event]).await;
}

pub(super) async fn enqueue_object_events(state: &Arc<AppState>, events: &[NewEvent]) {
    if events.is_empty() {
        return;
    }
    let Some(config_db) = state.config_db.as_ref() else {
        return;
    };
    let db = config_db.lock().await;
    if let Err(err) = db.event_outbox_insert_many(events) {
        warn!(
            "failed to append {} object event(s), first kind={} bucket={} key={:?}: {}",
            events.len(),
            events[0].kind.as_str(),
            events[0].bucket,
            events[0].key,
            err
        );
    }
}

// ---------------------------------------------------------------------------
// Content-MD5 validation (shared by PUT and UploadPart)
// ---------------------------------------------------------------------------

/// Validate the Content-MD5 header against the body, if present.
/// Returns Ok(()) if the header is absent or the digest matches.
pub(super) fn validate_content_md5(headers: &HeaderMap, body: &[u8]) -> Result<(), S3Error> {
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
    Ok(())
}

// ---------------------------------------------------------------------------
// Copy-source parsing (shared by CopyObject and UploadPartCopy)
// ---------------------------------------------------------------------------

/// Parse and validate the `x-amz-copy-source` header.
/// Returns `(source_bucket, source_key)` after URL-decoding and path-traversal checks.
pub(super) fn parse_copy_source(headers: &HeaderMap) -> Result<(String, String), S3Error> {
    let raw = headers
        .get("x-amz-copy-source")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| S3Error::InvalidRequest("Missing x-amz-copy-source header".to_string()))?;

    let decoded = urlencoding::decode(raw)
        .map_err(|_| S3Error::InvalidArgument("Invalid copy source encoding".to_string()))?;
    let trimmed = decoded.trim_start_matches('/');
    if trimmed.contains('?') {
        return Err(S3Error::InvalidArgument(
            "Copy source versionId/query parameters are not supported".to_string(),
        ));
    }

    let (bucket, key) = trimmed
        .split_once('/')
        .ok_or_else(|| S3Error::InvalidArgument("Copy source must be bucket/key".to_string()))?;

    // Validate source bucket and key to prevent path traversal on filesystem backend.
    // Check for ".." as a standalone path segment (not substring — "file..v2.tar.gz" is valid).
    if bucket.split('/').any(|s| s == ".." || s == ".")
        || key.split('/').any(|s| s == ".." || s == ".")
    {
        return Err(S3Error::InvalidArgument(
            "Copy source must not contain '.' or '..' path segments".to_string(),
        ));
    }

    Ok((bucket.to_string(), key.to_string()))
}

/// Verify the authenticated user has read access to the copy source.
pub(super) fn check_copy_source_access(
    auth_user: &Option<AuthenticatedUser>,
    source_bucket: &str,
    source_key: &str,
) -> Result<(), S3Error> {
    if let Some(ref user) = auth_user {
        if !user.can(S3Action::Read, source_bucket, source_key) {
            return Err(S3Error::AccessDenied);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Quota enforcement (shared by PUT, COPY, and multipart complete)
// ---------------------------------------------------------------------------

/// Check if a write of `incoming_bytes` would exceed the bucket's quota.
/// Uses cached usage data (soft quota). Returns Ok(()) if allowed.
/// When quota_bytes=0, always rejects (freeze bucket). When no cache data
/// is available, triggers a background scan and allows optimistically.
pub(super) fn check_quota(
    state: &Arc<AppState>,
    bucket: &str,
    incoming_bytes: u64,
) -> Result<(), S3Error> {
    let engine = state.engine.load();
    if let Some(quota) = engine.bucket_policy_registry().quota_bytes(bucket) {
        // quota=0 means freeze — always reject, even without usage data
        if quota == 0 {
            return Err(S3Error::InternalError(
                "Bucket is frozen (quota = 0)".into(),
            ));
        }
        // get_or_scan: returns cached usage if available, otherwise triggers a
        // background scan and returns None (first PUT is optimistic).
        if let Some(usage) = state.usage_scanner.get_or_scan(state, bucket, "") {
            if usage.total_size.saturating_add(incoming_bytes) > quota {
                let used_mb = usage.total_size / (1024 * 1024);
                let quota_mb = quota / (1024 * 1024);
                return Err(S3Error::InternalError(format!(
                    "Bucket quota exceeded: {} MB used of {} MB limit",
                    used_mb, quota_mb,
                )));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Bucket existence precheck (shared by PUT, COPY, multipart Complete)
// ---------------------------------------------------------------------------

/// Verify that `bucket` exists on the storage backend BEFORE any write path
/// is allowed to proceed. This closes a silent-bucket-creation bug on the
/// filesystem backend (C2 from security audit): `ensure_dir` at
/// `src/storage/filesystem.rs::ensure_dir` calls `create_dir_all(parent)`,
/// which quietly creates the bucket root directory as a side effect of the
/// first PUT. That bypasses `s3:CreateBucket`-equivalent authorization and
/// diverges from the S3 backend (which rejects with `NoSuchBucket`).
///
/// This precheck + the belt-and-braces guard in `FilesystemBackend::put_*`
/// (which refuses to write when the bucket root is missing) together
/// produce a consistent cross-backend contract: writes to a non-existent
/// bucket always return `NoSuchBucket`, never implicitly create.
pub(super) async fn ensure_bucket_exists(
    state: &Arc<AppState>,
    bucket: &str,
) -> Result<(), S3Error> {
    let engine = state.engine.load();
    match engine.head_bucket(bucket).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(S3Error::NoSuchBucket(bucket.to_string())),
        Err(e) => {
            // Map engine errors through the existing conversion so e.g. a
            // missing underlying backend still surfaces as a meaningful
            // error instead of a mysterious 500.
            Err(S3Error::from(e))
        }
    }
}

// ---------------------------------------------------------------------------
// PUT / COPY internals
// ---------------------------------------------------------------------------

/// PUT object handler (internal)
/// Called by put_object_or_copy after validation
#[instrument(skip(state, body, signed_payload_hash))]
pub(super) async fn put_object_inner(
    state: &Arc<AppState>,
    bucket: &str,
    key: &str,
    headers: &HeaderMap,
    body: &Bytes,
    signed_payload_hash: Option<&crate::api::auth::SignedPayloadHash>,
) -> Result<Response, S3Error> {
    info!("PUT {}/{} ({} bytes)", bucket, key, body.len());

    validate_content_md5(headers, body)?;

    // H1 SigV4 fix: when the SigV4 middleware successfully verified a
    // signature over a 64-char hex `x-amz-content-sha256`, the actual
    // body's SHA-256 must match that value. Without this check, a
    // credentialed client could sign hash A and ship body B — the
    // signature is computed over the canonical request which only
    // sees the header value, not the body bytes. SigV4's integrity
    // contract requires the receiver to verify the body downstream.
    //
    // Sentinel values that disable verification (we record the value
    // but don't enforce a hash match): UNSIGNED-PAYLOAD (client
    // explicitly opted out) and STREAMING-* variants (per-chunk
    // signature scheme; see aws_chunked.rs for chunk-chain handling).
    if let Some(claimed) = signed_payload_hash {
        if claimed.requires_chunk_signature_verification() {
            return Err(S3Error::NotImplemented(
                "Signed AWS streaming payloads are not supported; use UNSIGNED-PAYLOAD or non-streaming SHA-256 payloads".to_string(),
            ));
        }
        if claimed.is_verifiable_hex() {
            use sha2::{Digest, Sha256};
            let actual = hex::encode(Sha256::digest(body.as_ref()));
            let matches: bool =
                subtle::ConstantTimeEq::ct_eq(actual.as_bytes(), claimed.as_str().as_bytes())
                    .into();
            if !matches {
                warn!(
                    "PUT {}/{}: SigV4 payload hash mismatch (claimed {}…, actual {}…)",
                    bucket,
                    key,
                    &claimed.as_str()[..8.min(claimed.as_str().len())],
                    &actual[..8],
                );
                return Err(S3Error::BadDigest);
            }
        }
    }

    // Bucket must exist before any write path touches the backend. See
    // `ensure_bucket_exists` for the full rationale (C2 security fix).
    ensure_bucket_exists(state, bucket).await?;

    // M2 fix: honour PUT conditional headers (If-Match,
    // If-None-Match, If-Modified-Since, If-Unmodified-Since) against
    // the EXISTING object at this key. Pre-fix, a PUT with a failing
    // If-Match silently overwrote anyway, breaking compare-and-swap
    // patterns clients use for safe overwrites and the
    // `If-None-Match: *` idempotent-create primitive.
    if let Some(err) = evaluate_put_conditionals(state, bucket, key, headers).await {
        return Err(err);
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
            .map_err(|e| S3Error::InternalError(crate::api::errors::sanitise_for_client(&e)))?;
        enqueue_object_event(
            state,
            NewEvent::new(
                EventKind::ObjectCreated,
                bucket,
                key,
                EventSource::S3Api,
                current_unix_seconds(),
                serde_json::json!({
                    "content_length": 0,
                    "storage_type": "directory",
                }),
            ),
        )
        .await;
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

    check_quota(state, bucket, body.len() as u64)?;

    let content_type = extract_content_type(headers);
    let user_metadata = extract_user_metadata(headers);

    let result = state
        .engine
        .load()
        .store(bucket, key, body, content_type, user_metadata)
        .await?;

    let storage_type = result.metadata.storage_info.label();
    enqueue_object_event(
        state,
        NewEvent::new(
            EventKind::ObjectCreated,
            bucket,
            key,
            EventSource::S3Api,
            current_unix_seconds(),
            serde_json::json!({
                "content_length": body.len(),
                "storage_type": storage_type,
                "etag": result.metadata.etag(),
            }),
        ),
    )
    .await;

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
    let (source_bucket, source_key) = parse_copy_source(headers)?;
    check_copy_source_access(auth_user, &source_bucket, &source_key)?;

    info!(
        "COPY {}/{} -> {}/{}",
        source_bucket, source_key, bucket, key
    );

    // Both source and destination buckets must exist. The source check
    // converts the head miss below into `NoSuchBucket` via the engine's
    // error mapping; the destination check is explicit here because we
    // haven't touched it yet and `ensure_dir` on the filesystem backend
    // would silently create it (C2 security fix). See
    // `ensure_bucket_exists` doc comment for background.
    ensure_bucket_exists(state, &source_bucket).await?;
    ensure_bucket_exists(state, bucket).await?;

    // Load engine once for the entire copy operation to ensure consistency.
    let engine = state.engine.load();

    // Check source object size before loading into memory to avoid transient
    // memory spikes if max_object_size was reduced after the object was stored.
    // Note: file_size may be 0 for unmanaged objects (fallback metadata), so we
    // also check the actual data size after retrieval below.
    let source_meta_head = engine.head(&source_bucket, &source_key).await?;
    if source_meta_head.file_size > engine.max_object_size() {
        return Err(S3Error::EntityTooLarge {
            size: source_meta_head.file_size,
            max: engine.max_object_size(),
        });
    }

    // M2 security fix: honour `x-amz-copy-source-if-*` preconditions
    // before touching the source. Pre-fix, these headers were silently
    // ignored — a client saying "copy only if source is still vX" got
    // an unconditional copy regardless. Evaluated against the HEAD
    // metadata so we don't pay the retrieve cost when the precondition
    // is going to fail anyway.
    if let Some(err) = check_copy_source_conditionals(headers, &source_meta_head) {
        return Err(err);
    }

    // Retrieve source object
    let (data, source_meta) = engine.retrieve(&source_bucket, &source_key).await?;

    // Double-check actual data size (metadata may report 0 for unmanaged objects)
    if data.len() as u64 > engine.max_object_size() {
        return Err(S3Error::EntityTooLarge {
            size: data.len() as u64,
            max: engine.max_object_size(),
        });
    }

    // Handle x-amz-metadata-directive: COPY (default) or REPLACE.
    //
    // M3 security fix: reject any value that's not one of the two
    // documented enum values (case-insensitive). Pre-fix, a typo like
    // "REPLAC" silently fell through to COPY and preserved the source
    // metadata the client was clearly trying to replace. That's a
    // correctness footgun — the Copy succeeded with metadata the
    // client explicitly chose NOT to write.
    let metadata_directive = headers
        .get("x-amz-metadata-directive")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("COPY");

    let directive_is_copy = metadata_directive.eq_ignore_ascii_case("COPY");
    let directive_is_replace = metadata_directive.eq_ignore_ascii_case("REPLACE");
    if !directive_is_copy && !directive_is_replace {
        return Err(S3Error::InvalidArgument(format!(
            "x-amz-metadata-directive must be COPY or REPLACE, got '{}'",
            metadata_directive
        )));
    }

    let (dest_content_type, dest_user_metadata) = if directive_is_replace {
        // REPLACE: use metadata from the copy request headers
        let ct = extract_content_type(headers);
        let um = extract_user_metadata(headers);
        (ct, um)
    } else {
        // COPY: preserve source metadata
        (
            source_meta.content_type.clone(),
            source_meta.user_metadata.clone(),
        )
    };

    // Quota check on destination bucket before storing
    check_quota(state, bucket, data.len() as u64)?;

    // Store as new object with the chosen metadata
    let result = engine
        .store(bucket, key, &data, dest_content_type, dest_user_metadata)
        .await?;
    enqueue_object_event(
        state,
        NewEvent::new(
            EventKind::ObjectCopied,
            bucket,
            key,
            EventSource::S3Api,
            current_unix_seconds(),
            serde_json::json!({
                "source_bucket": &source_bucket,
                "source_key": &source_key,
                "content_length": data.len(),
                "etag": result.metadata.etag(),
            }),
        ),
    )
    .await;

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
#[allow(clippy::too_many_arguments)]
pub(super) async fn upload_part(
    state: &Arc<AppState>,
    bucket: &str,
    key: &str,
    headers: &HeaderMap,
    part_num: u32,
    upload_id: &str,
    body: Bytes,
    signed_payload_hash: Option<&crate::api::auth::SignedPayloadHash>,
) -> Result<Response, S3Error> {
    info!(
        "UploadPart {}/{} part={} uploadId={}",
        bucket, key, part_num, upload_id
    );

    // M1 security fix: refuse part uploads when the target bucket
    // doesn't exist. Pre-fix, UploadPart accepted bytes into a
    // MultipartStore entry whose bucket had been deleted since
    // Initiate — orphan memory until the idle-TTL sweeper, and a
    // silent contract violation vs. S3 (which 404s immediately).
    ensure_bucket_exists(state, bucket).await?;

    validate_content_md5(headers, &body)?;

    // H1 SigV4 fix: same body-hash verification as put_object_inner —
    // each part's bytes must match the SHA-256 the client signed.
    if let Some(claimed) = signed_payload_hash {
        if claimed.requires_chunk_signature_verification() {
            return Err(S3Error::NotImplemented(
                "Signed AWS streaming payloads are not supported; use UNSIGNED-PAYLOAD or non-streaming SHA-256 payloads".to_string(),
            ));
        }
        if claimed.is_verifiable_hex() {
            use sha2::{Digest, Sha256};
            let actual = hex::encode(Sha256::digest(body.as_ref()));
            let matches: bool =
                subtle::ConstantTimeEq::ct_eq(actual.as_bytes(), claimed.as_str().as_bytes())
                    .into();
            if !matches {
                warn!(
                    "UploadPart {}/{} part={}: SigV4 payload hash mismatch",
                    bucket, key, part_num
                );
                return Err(S3Error::BadDigest);
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
    let (source_bucket, source_key) = parse_copy_source(headers)?;
    check_copy_source_access(auth_user, &source_bucket, &source_key)?;

    info!(
        "UploadPartCopy {}/{} part={} uploadId={} from {}/{}",
        bucket, key, part_num, upload_id, source_bucket, source_key
    );

    // M1 security fix: refuse when either the destination OR source
    // bucket doesn't exist. retrieve() below catches source-miss
    // indirectly but via a noisier path; the explicit check gives a
    // clean NoSuchBucket error and closes the orphan-state window.
    ensure_bucket_exists(state, &source_bucket).await?;
    ensure_bucket_exists(state, bucket).await?;

    // Retrieve source object data
    let engine = state.engine.load();

    // M2 security fix: honour `x-amz-copy-source-if-*` preconditions
    // for UploadPartCopy the same way CopyObject does.
    let source_meta_head = engine.head(&source_bucket, &source_key).await?;
    if let Some(err) = check_copy_source_conditionals(headers, &source_meta_head) {
        return Err(err);
    }

    let (data, _source_meta) = engine.retrieve(&source_bucket, &source_key).await?;

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

/// Check `x-amz-copy-source-if-*` preconditions against source metadata
/// for a CopyObject / UploadPartCopy request.
///
/// Per S3 spec, a failing copy-source precondition returns 412
/// PreconditionFailed — even for the `if-none-match` / `if-modified-
/// since` variants that would normally be 304 on a GET. That's because
/// a "copy was not performed" is not a cacheable state; the client must
/// actively retry.
///
/// Evaluation follows AWS S3 CopyObject paired-header rules:
///
/// - `if-match` + `if-unmodified-since`: AWS evaluates `if-match`
///   FIRST. If it passes, the request proceeds and
///   `if-unmodified-since` is IGNORED (the positive ETag is more
///   specific than a date guard). If `if-match` fails, the request
///   is rejected with 412 regardless of the date.
/// - `if-none-match` + `if-modified-since`: same precedence —
///   `if-none-match` wins. If it passes, ignore the date. If it
///   fails (ETag matches), 412.
/// - Solo headers behave as documented.
/// - The positive/negative pairs (`if-match` + `if-none-match` on
///   the same request) are AWS-undefined; we evaluate `if-match`
///   first (deny on failure), then `if-none-match` (deny on
///   failure), which matches what most S3-clones do.
///
/// L2 fix: pre-fix, the function evaluated all four headers
/// linearly with first-failure-wins, which broke combinations AWS
/// accepts (e.g. `if-match` passing alongside an
/// `if-unmodified-since` that "fails" — AWS treats the if-match
/// pass as the answer).
///
/// Returns Some(PreconditionFailed) on a real violation, None when
/// the request should proceed.
pub(super) fn check_copy_source_conditionals(
    req_headers: &HeaderMap,
    source_metadata: &FileMetadata,
) -> Option<S3Error> {
    let etag = source_metadata.etag();
    let etag_bare = etag.trim_matches('"');
    let last_modified = source_metadata.created_at;

    let if_match = req_headers
        .get("x-amz-copy-source-if-match")
        .and_then(|v| v.to_str().ok());
    let if_none_match = req_headers
        .get("x-amz-copy-source-if-none-match")
        .and_then(|v| v.to_str().ok());
    let if_modified_since = req_headers
        .get("x-amz-copy-source-if-modified-since")
        .and_then(|v| v.to_str().ok());
    let if_unmodified_since = req_headers
        .get("x-amz-copy-source-if-unmodified-since")
        .and_then(|v| v.to_str().ok());

    let etag_matches = |spec: &str| -> bool {
        spec.split(',').any(|t| {
            let t = t.trim();
            t == "*" || t == etag || t.trim_matches('"') == etag_bare
        })
    };

    // ── if-match + if-unmodified-since pair ──
    // AWS: if-match wins. Pass → ignore date. Fail → 412.
    if let Some(spec) = if_match {
        if etag_matches(spec) {
            // Suppress if-unmodified-since per AWS docs.
        } else {
            return Some(S3Error::PreconditionFailed);
        }
    } else if let Some(date_str) = if_unmodified_since {
        if let Some(date) = parse_http_date(date_str) {
            if last_modified > date {
                return Some(S3Error::PreconditionFailed);
            }
        }
    }

    // ── if-none-match + if-modified-since pair ──
    // AWS: if-none-match wins. Match → 412. Mismatch → ignore date.
    if let Some(spec) = if_none_match {
        if etag_matches(spec) {
            return Some(S3Error::PreconditionFailed);
        }
        // Otherwise the negative-ETag check passed → ignore date.
    } else if let Some(date_str) = if_modified_since {
        if let Some(date) = parse_http_date(date_str) {
            if last_modified <= date {
                return Some(S3Error::PreconditionFailed);
            }
        }
    }

    None
}

/// Evaluate PUT-side conditional headers against the EXISTING object
/// (if any) at `bucket/key`. Returns `Some(S3Error)` on a precondition
/// failure (the caller short-circuits with 412), `None` to proceed.
///
/// Semantics per AWS S3 PutObject (M2 fix):
///
/// - `If-Match: <etag>` — proceed only if the existing object's
///   ETag matches. 412 if missing or mismatch.
/// - `If-Match: *` — proceed only if an object exists at this key.
///   412 if missing.
/// - `If-None-Match: *` — proceed only if NO object exists. The
///   canonical idempotent-create primitive. 412 if exists.
/// - `If-None-Match: <etag>` — proceed only if existing ETag does
///   NOT match. 412 if it does match.
/// - `If-Unmodified-Since` — proceed only if existing was modified
///   ≤ the given date. 412 otherwise. Uses the same date parser as
///   the GET path.
/// - `If-Modified-Since` — proceed only if existing was modified
///   > the given date. 412 otherwise. Symmetric.
///
/// Evaluation order matches AWS HTTP-conditional-header semantics:
/// If-Match → If-Unmodified-Since → If-None-Match → If-Modified-Since.
/// First failing check wins.
async fn evaluate_put_conditionals(
    state: &Arc<AppState>,
    bucket: &str,
    key: &str,
    req_headers: &HeaderMap,
) -> Option<S3Error> {
    // Cheap exit: if no conditional header is present, skip the HEAD.
    if !has_any_put_conditional(req_headers) {
        return None;
    }

    let engine = state.engine.load();
    // NotFound or backend error → treat as missing for conditional purposes.
    let existing = engine.head(bucket, key).await.ok();

    // 1. If-Match
    if let Some(if_match) = req_headers.get("if-match").and_then(|v| v.to_str().ok()) {
        let if_match = if_match.trim();
        match &existing {
            None => return Some(S3Error::PreconditionFailed),
            Some(meta) => {
                let etag = meta.etag();
                let etag_bare = etag.trim_matches('"');
                let matches = if_match.split(',').any(|t| {
                    let t = t.trim();
                    t == "*" || t == etag || t.trim_matches('"') == etag_bare
                });
                if !matches {
                    return Some(S3Error::PreconditionFailed);
                }
            }
        }
    }

    // 2. If-Unmodified-Since
    if let Some(v) = req_headers
        .get("if-unmodified-since")
        .and_then(|v| v.to_str().ok())
    {
        if let (Some(meta), Some(date)) = (existing.as_ref(), parse_http_date(v)) {
            if meta.created_at > date {
                return Some(S3Error::PreconditionFailed);
            }
        }
    }

    // 3. If-None-Match
    if let Some(if_none) = req_headers
        .get("if-none-match")
        .and_then(|v| v.to_str().ok())
    {
        let if_none = if_none.trim();
        match &existing {
            None => {
                // Either form passes when nothing exists.
            }
            Some(meta) => {
                let etag = meta.etag();
                let etag_bare = etag.trim_matches('"');
                let matches = if_none.split(',').any(|t| {
                    let t = t.trim();
                    t == "*" || t == etag || t.trim_matches('"') == etag_bare
                });
                if matches {
                    // PUT semantics: 412, NOT 304 (304 is a GET concept).
                    return Some(S3Error::PreconditionFailed);
                }
            }
        }
    }

    // 4. If-Modified-Since
    if let Some(v) = req_headers
        .get("if-modified-since")
        .and_then(|v| v.to_str().ok())
    {
        if let (Some(meta), Some(date)) = (existing.as_ref(), parse_http_date(v)) {
            if meta.created_at <= date {
                return Some(S3Error::PreconditionFailed);
            }
        }
    }

    None
}

fn has_any_put_conditional(headers: &HeaderMap) -> bool {
    headers.contains_key("if-match")
        || headers.contains_key("if-none-match")
        || headers.contains_key("if-modified-since")
        || headers.contains_key("if-unmodified-since")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers_with_copy_source(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("x-amz-copy-source", value.parse().unwrap());
        headers
    }

    #[test]
    fn copy_source_rejects_version_id_query() {
        let headers = headers_with_copy_source("bucket/key.txt%3FversionId%3Dabc");
        assert!(matches!(
            parse_copy_source(&headers),
            Err(S3Error::InvalidArgument(_))
        ));
    }

    #[test]
    fn copy_source_preserves_key_slashes() {
        let headers = headers_with_copy_source("bucket/a//b.txt");
        let (_, key) = parse_copy_source(&headers).unwrap();
        assert_eq!(key, "a//b.txt");
    }
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
