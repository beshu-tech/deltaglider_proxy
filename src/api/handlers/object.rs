//! Object-level S3 handlers: GET, HEAD, PUT (with copy detection), DELETE.

use super::bucket::get_acl_response;
use super::object_helpers::{
    apply_response_overrides, build_range_response, check_conditionals, copy_object_inner,
    decode_body, enqueue_object_event, enqueue_object_events, parse_range_header, put_object_inner,
    resolve_range, upload_part, upload_part_copy,
};
use super::{audit_log_s3, build_object_headers, xml_response, AppState, ObjectQuery, S3Error};
use crate::deltaglider::RetrieveResponse;
use crate::event_outbox::{current_unix_seconds, EventKind, EventSource, NewEvent};
use crate::iam::{AuthenticatedUser, S3Action};
use axum::body::{Body, Bytes};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use std::sync::Arc;
use tracing::{debug, info, instrument, warn};

use crate::api::extractors::{ValidatedBucket, ValidatedPath};
use crate::api::xml::{DeleteError, DeleteRequest, DeleteResult, DeletedObject, ListPartsResult};

/// Query parameters for bucket-level POST operations
#[derive(Debug, serde::Deserialize, Default)]
pub struct BucketPostQuery {
    pub delete: Option<String>,
}

/// PUT object handler with copy detection and multipart upload support
/// PUT /{bucket}/{key}
/// Detects x-amz-copy-source header to dispatch to copy operation
/// Detects ?partNumber&uploadId for multipart upload part
#[instrument(skip(state, body))]
pub async fn put_object_or_copy(
    State(state): State<Arc<AppState>>,
    ValidatedPath { bucket, key }: ValidatedPath,
    Query(query): Query<ObjectQuery>,
    auth_user: Option<axum::Extension<AuthenticatedUser>>,
    // H1: SigV4-verified `x-amz-content-sha256` header value, stashed
    // by the auth middleware after successful signature verification.
    // Used by put_object_inner / upload_part to confirm the body's
    // actual SHA-256 matches what the client signed.
    signed_payload_hash: Option<axum::Extension<crate::api::auth::SignedPayloadHash>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, S3Error> {
    let auth_user = auth_user.map(|axum::Extension(u)| u);
    let signed_payload_hash = signed_payload_hash.map(|axum::Extension(h)| h);

    // PUT /{bucket}/{key}?acl — return 501 NotImplemented.
    //
    // M4 fix: pre-fix this returned 200 OK while silently discarding
    // the ACL XML. Clients believed grants had been applied. We use
    // IAM policies instead; ACLs are unsupported. NoSuchKey takes
    // precedence over 501 (matches AWS).
    if query.acl.is_some() {
        info!("PUT object ACL: unsupported (returning 501)");
        state.engine.load().head(&bucket, &key).await?;
        return Err(S3Error::NotImplemented(
            "Object ACLs are not supported by this proxy; use IAM policies instead".to_string(),
        ));
    }

    // PUT /{bucket}/{key}?tagging — return 501 NotImplemented.
    // L1 fix: NoSuchKey precedence over 501.
    if query.tagging.is_some() {
        info!("PUT object tagging: unsupported (returning 501)");
        state.engine.load().head(&bucket, &key).await?;
        return Err(S3Error::NotImplemented(
            "Object tagging is not supported by this proxy".to_string(),
        ));
    }

    let decoded_body = decode_body(&headers, body)?;

    // Multipart upload part (with optional copy-source)
    if let (Some(part_num), Some(upload_id)) = (&query.part_number, &query.upload_id) {
        if headers.contains_key("x-amz-copy-source") {
            return upload_part_copy(
                &state, &bucket, &key, &headers, *part_num, upload_id, &auth_user,
            )
            .await;
        }
        return upload_part(
            &state,
            &bucket,
            &key,
            &headers,
            *part_num,
            upload_id,
            decoded_body,
            signed_payload_hash.as_ref(),
        )
        .await;
    }

    // Copy vs direct put
    let is_copy = headers.contains_key("x-amz-copy-source");
    let result = if is_copy {
        copy_object_inner(&state, &bucket, &key, &headers, &auth_user).await
    } else {
        put_object_inner(
            &state,
            &bucket,
            &key,
            &headers,
            &decoded_body,
            signed_payload_hash.as_ref(),
        )
        .await
    };

    if result.is_ok() {
        let user_name = auth_user
            .as_ref()
            .map(|u| u.name.as_str())
            .unwrap_or("anonymous");
        let action = if is_copy { "s3_copy" } else { "s3_put" };
        audit_log_s3(action, user_name, &headers, &bucket, &key);
    }

    result
}

/// GET object handler
/// GET /{bucket}/{key}
/// GET /{bucket}/{key}?uploadId=X - ListParts
///
/// Direct files are streamed from the backend (constant memory, low TTFB).
/// Delta files are reconstructed in memory and sent as a buffered response.
/// Supports Range requests and conditional headers.
#[instrument(skip(state))]
pub async fn get_object(
    State(state): State<Arc<AppState>>,
    ValidatedPath { bucket, key }: ValidatedPath,
    Query(query): Query<ObjectQuery>,
    req_headers: HeaderMap,
) -> Result<Response, S3Error> {
    // GET /{bucket}/{key}?tagging — return 501 NotImplemented.
    //
    // M4 fix: 404 wins over 501 when the object doesn't exist. AWS
    // returns NoSuchKey for tagging on a missing object; we should
    // surface that BEFORE the "we don't support this" 501 so clients
    // can distinguish "object missing" from "feature missing."
    if query.tagging.is_some() {
        info!("GET object tagging: unsupported (returning 501)");
        // Propagate NoSuchKey/NoSuchBucket; only swallow non-404 errors.
        state.engine.load().head(&bucket, &key).await?;
        return Err(S3Error::NotImplemented(
            "Object tagging is not supported by this proxy".to_string(),
        ));
    }

    // GET /{bucket}/{key}?acl — return canned ACL response
    if query.acl.is_some() {
        info!("GET object ACL: {}/{}", bucket, key);
        // Verify the object exists first; S3 returns 404 for ACL on non-existent objects
        state.engine.load().head(&bucket, &key).await?;
        return get_acl_response();
    }

    // ListParts (L1 fix: paginate instead of claiming is_truncated=false).
    if let Some(upload_id) = &query.upload_id {
        info!("ListParts {}/{} uploadId={}", bucket, key, upload_id);
        let marker = query.part_number_marker.unwrap_or(0);
        // S3 default + cap: 1000 parts per page.
        let max_parts = query.max_parts.unwrap_or(1000).clamp(1, 1000);
        let (parts, is_truncated, next_marker) = state
            .multipart
            .list_parts_paginated(upload_id, &bucket, &key, marker, max_parts)?;
        let result = ListPartsResult {
            bucket: bucket.clone(),
            key: key.clone(),
            upload_id: upload_id.clone(),
            parts,
            max_parts,
            is_truncated,
            part_number_marker: marker,
            next_part_number_marker: next_marker,
        };
        let xml = result.to_xml();
        return Ok(xml_response(xml));
    }

    info!("GET {}/{}", bucket, key);

    // Parse Range header early (before retrieval) so we know if it's requested
    let range_request = req_headers
        .get("range")
        .and_then(|v| v.to_str().ok())
        .and_then(parse_range_header);

    // For Range requests on passthrough objects, try the range-aware path first
    // to avoid buffering the entire file.
    if let Some(ref range) = range_request {
        // We need to know the total file size to resolve the range.
        // HEAD is cheap and gives us the metadata we need.
        let metadata = state.engine.load().head(&bucket, &key).await?;

        // Check conditional headers before fetching body
        if let Some(err) = check_conditionals(&req_headers, &metadata) {
            return Err(err);
        }

        let total = metadata.file_size;
        let (start, end) = resolve_range(range, total).ok_or(S3Error::InvalidRange)?;

        // Try range-aware streaming (only succeeds for passthrough objects)
        if let Some((stream, content_length, meta)) = state
            .engine
            .load()
            .retrieve_stream_range(&bucket, &key, start, end)
            .await?
        {
            // Build 206 Partial Content response from the range stream
            let mut headers = build_object_headers(&meta);
            let range_len = content_length;
            headers.insert(
                "Content-Length",
                HeaderValue::from_str(&range_len.to_string()).unwrap(),
            );
            headers.insert(
                "Content-Range",
                HeaderValue::from_str(&format!("bytes {}-{}/{}", start, end, total)).unwrap(),
            );
            apply_response_overrides(&mut headers, &query);
            let body = Body::from_stream(stream);
            return Ok((StatusCode::PARTIAL_CONTENT, headers, body).into_response());
        }

        // Fall through: delta/reference/unmanaged objects need full retrieval
        // then buffered slicing.
        let response = state.engine.load().retrieve_stream(&bucket, &key).await?;
        return match response {
            RetrieveResponse::Streamed {
                stream, metadata, ..
            } => {
                use futures::TryStreamExt;
                let chunks: Vec<Bytes> = stream
                    .map_err(|e| std::io::Error::other(e.to_string()))
                    .try_collect()
                    .await
                    .map_err(|e| {
                        S3Error::InternalError(crate::api::errors::sanitise_for_client(&e))
                    })?;
                let total_len: usize = chunks.iter().map(|b| b.len()).sum();
                let mut data = Vec::with_capacity(total_len);
                for chunk in &chunks {
                    data.extend_from_slice(chunk);
                }
                build_range_response(data, &metadata, range, None, &query)
            }
            RetrieveResponse::Buffered {
                data,
                metadata,
                cache_hit,
            } => build_range_response(data, &metadata, range, cache_hit, &query),
        };
    }

    let response = state.engine.load().retrieve_stream(&bucket, &key).await?;

    match response {
        RetrieveResponse::Streamed {
            stream, metadata, ..
        } => {
            debug!(
                "Streaming {}/{} (stored as {})",
                bucket,
                key,
                metadata.storage_info.label()
            );

            // Check conditional headers before streaming body
            if let Some(err) = check_conditionals(&req_headers, &metadata) {
                return Err(err);
            }

            let mut headers = build_object_headers(&metadata);
            apply_response_overrides(&mut headers, &query);
            let body = Body::from_stream(stream);
            Ok((StatusCode::OK, headers, body).into_response())
        }
        RetrieveResponse::Buffered {
            data,
            metadata,
            cache_hit,
        } => {
            debug!(
                "Retrieved {}/{} ({} bytes, stored as {})",
                bucket,
                key,
                data.len(),
                metadata.storage_info.label()
            );

            // Check conditional headers before returning body
            if let Some(err) = check_conditionals(&req_headers, &metadata) {
                return Err(err);
            }

            // Handle Range request
            if let Some(ref range) = range_request {
                return build_range_response(data, &metadata, range, cache_hit, &query);
            }

            let mut headers = build_object_headers(&metadata);
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
            apply_response_overrides(&mut headers, &query);
            Ok((StatusCode::OK, headers, data).into_response())
        }
    }
}

/// HEAD object handler
/// HEAD /{bucket}/{key}
/// Supports conditional headers (If-Match, If-None-Match, If-Modified-Since, If-Unmodified-Since).
#[instrument(skip(state))]
pub async fn head_object(
    State(state): State<Arc<AppState>>,
    ValidatedPath { bucket, key }: ValidatedPath,
    req_headers: HeaderMap,
) -> Result<Response, S3Error> {
    info!("HEAD {}/{}", bucket, key);

    let metadata = state.engine.load().head(&bucket, &key).await?;

    // Check conditional headers
    if let Some(err) = check_conditionals(&req_headers, &metadata) {
        return Err(err);
    }

    let headers = build_object_headers(&metadata);
    Ok((StatusCode::OK, headers).into_response())
}

/// DELETE object handler
/// DELETE /{bucket}/{key}
/// DELETE /{bucket}/{key}?uploadId=X - AbortMultipartUpload
#[instrument(skip(state))]
pub async fn delete_object(
    State(state): State<Arc<AppState>>,
    ValidatedPath { bucket, key }: ValidatedPath,
    Query(query): Query<ObjectQuery>,
    auth_user: Option<axum::Extension<AuthenticatedUser>>,
    headers: HeaderMap,
) -> Result<Response, S3Error> {
    // DELETE /{bucket}/{key}?tagging — return 501 NotImplemented.
    // L1 fix: 404 NoSuchKey wins over 501 NotImplemented when the
    // object doesn't exist. Pre-fix DELETE returned 501 even for
    // missing keys; AWS returns 404 first.
    if query.tagging.is_some() {
        info!("DELETE object tagging: unsupported (returning 501)");
        state.engine.load().head(&bucket, &key).await?;
        return Err(S3Error::NotImplemented(
            "Object tagging is not supported by this proxy".to_string(),
        ));
    }

    // AbortMultipartUpload
    if let Some(upload_id) = &query.upload_id {
        info!(
            "AbortMultipartUpload {}/{} uploadId={}",
            bucket, key, upload_id
        );
        state.multipart.abort(upload_id, &bucket, &key)?;
        return Ok(StatusCode::NO_CONTENT.into_response());
    }

    // Recursive prefix delete: DELETE /{bucket}/{prefix}/ (trailing slash)
    if key.ends_with('/') {
        info!("DELETE recursive {}/{}*", bucket, key);
        let engine = state.engine.load();

        // E1 fix: iterate pages of 1000 keys at a time instead of materialising
        // the full listing with u32::MAX. A prefix with millions of keys used
        // to balloon memory by ~300 B × key-count before a single delete ran.
        // Page-level progress means interrupting mid-run leaves partial
        // deletes — same semantics as AWS console's recursive delete.
        const DELETE_PAGE_SIZE: u32 = 1000;

        let mut deleted = 0u32;
        let mut denied = 0u32;
        let mut next_token: Option<String> = None;

        loop {
            let page = engine
                .list_objects(
                    &bucket,
                    &key,
                    None,
                    DELETE_PAGE_SIZE,
                    next_token.as_deref(),
                    false,
                )
                .await?;

            let mut page_deleted_events = Vec::new();
            for (obj_key, _meta) in &page.objects {
                // Per-object IAM check (respects Deny rules on sub-prefixes)
                if let Some(axum::Extension(ref user)) = auth_user {
                    if !user.can(S3Action::Delete, &bucket, obj_key) {
                        denied += 1;
                        continue;
                    }
                }
                match engine.delete(&bucket, obj_key).await {
                    Ok(()) => {
                        deleted += 1;
                        page_deleted_events.push(NewEvent::new(
                            EventKind::ObjectDeleted,
                            bucket.as_str(),
                            obj_key.as_str(),
                            EventSource::S3Api,
                            current_unix_seconds(),
                            serde_json::json!({ "delete_type": "recursive" }),
                        ));
                    }
                    Err(e) => {
                        let s3_err = S3Error::from(e);
                        if matches!(s3_err, S3Error::NoSuchKey(_)) {
                            deleted += 1; // Already gone
                        } else {
                            warn!("Failed to delete {}/{}: {}", bucket, obj_key, s3_err);
                        }
                    }
                }
            }
            enqueue_object_events(&state, &page_deleted_events).await;

            if !page.is_truncated {
                break;
            }
            // continuation_token is the ENGINE-LEVEL token. Feed it straight
            // back without re-encoding (that's only for client-visible tokens).
            next_token = page.next_continuation_token;
            if next_token.is_none() {
                // Defensive: truncated=true but no token → don't infinite-loop.
                warn!(
                    "Recursive delete: is_truncated=true but no continuation token at {}/{}*",
                    bucket, key
                );
                break;
            }
        }

        let user_name = auth_user
            .as_ref()
            .map(|axum::Extension(u)| u.name.as_str())
            .unwrap_or("anonymous");
        audit_log_s3("s3_delete_recursive", user_name, &headers, &bucket, &key);
        return Ok((
            StatusCode::OK,
            axum::Json(serde_json::json!({"deleted": deleted, "denied": denied})),
        )
            .into_response());
    }

    info!("DELETE {}/{}", bucket, key);

    let mut deleted_existing = false;
    if let Err(err) = state.engine.load().delete(&bucket, &key).await {
        match S3Error::from(err) {
            S3Error::NoSuchKey(_) => {}
            other => return Err(other),
        }
    } else {
        deleted_existing = true;
    }

    debug!("Deleted {}/{}", bucket, key);
    if deleted_existing {
        enqueue_object_event(
            &state,
            NewEvent::new(
                EventKind::ObjectDeleted,
                bucket.as_str(),
                key.as_str(),
                EventSource::S3Api,
                current_unix_seconds(),
                serde_json::json!({ "delete_type": "single" }),
            ),
        )
        .await;
    }

    let user_name = auth_user
        .as_ref()
        .map(|u| u.name.as_str())
        .unwrap_or("anonymous");
    audit_log_s3("s3_delete", user_name, &headers, &bucket, &key);

    // S3 returns 204 No Content on successful delete
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// DELETE multiple objects handler
/// POST /{bucket}?delete
#[instrument(skip(state, body))]
pub async fn delete_objects(
    State(state): State<Arc<AppState>>,
    ValidatedBucket(bucket): ValidatedBucket,
    Query(query): Query<BucketPostQuery>,
    auth_user: Option<axum::Extension<AuthenticatedUser>>,
    req_headers: HeaderMap,
    body: Bytes,
) -> Result<Response, S3Error> {
    use super::body_to_utf8;

    // Ensure this is a delete request
    if query.delete.is_none() {
        return Err(S3Error::InvalidRequest(
            "POST requires ?delete query parameter".to_string(),
        ));
    }

    // Parse XML body
    let body_str = body_to_utf8(&body)?;

    let delete_req = DeleteRequest::from_xml(body_str).map_err(|e| {
        warn!("Failed to parse DeleteObjects XML: {}", e);
        S3Error::MalformedXML
    })?;
    validate_delete_objects_count(delete_req.objects.len())?;

    info!(
        "DELETE multiple objects in {} ({} objects)",
        bucket,
        delete_req.objects.len()
    );

    let quiet = delete_req.quiet.unwrap_or(false);
    let mut deleted = Vec::new();
    let mut errors = Vec::new();
    let mut deleted_events = Vec::new();

    for obj in delete_req.objects {
        let key = obj.key.trim_start_matches('/');

        // Per-key authorization check: the middleware only validated at bucket level,
        // but each key needs its own permission check for prefix-scoped policies.
        if let Some(axum::Extension(ref user)) = auth_user {
            if !user.can(S3Action::Delete, &bucket, key) {
                errors.push(DeleteError {
                    key: obj.key.clone(),
                    version_id: obj.version_id.clone(),
                    code: "AccessDenied".to_string(),
                    message: "Access Denied".to_string(),
                });
                continue;
            }
        }

        match state.engine.load().delete(&bucket, key).await {
            Ok(()) => {
                debug!("Deleted {}/{}", bucket, key);
                deleted_events.push(NewEvent::new(
                    EventKind::ObjectDeleted,
                    bucket.as_str(),
                    key,
                    EventSource::S3Api,
                    current_unix_seconds(),
                    serde_json::json!({ "delete_type": "batch" }),
                ));
                deleted.push(DeletedObject {
                    key: obj.key.clone(),
                    version_id: obj.version_id.clone(),
                });
            }
            Err(e) => {
                let s3_err = S3Error::from(e);
                // S3 treats NoSuchKey as success in batch delete
                if matches!(s3_err, S3Error::NoSuchKey(_)) {
                    deleted.push(DeletedObject {
                        key: obj.key.clone(),
                        version_id: obj.version_id.clone(),
                    });
                } else {
                    warn!("Failed to delete {}/{}: {}", bucket, key, s3_err);
                    errors.push(DeleteError {
                        key: obj.key.clone(),
                        version_id: obj.version_id.clone(),
                        code: s3_err.code().to_string(),
                        message: s3_err.to_string(),
                    });
                }
            }
        }
    }

    enqueue_object_events(&state, &deleted_events).await;

    // Audit log each successfully deleted object
    if !deleted.is_empty() {
        let user_name = auth_user
            .as_ref()
            .map(|u| u.name.as_str())
            .unwrap_or("anonymous");
        for d in &deleted {
            audit_log_s3("s3_delete", user_name, &req_headers, &bucket, &d.key);
        }
    }

    let result = DeleteResult { deleted, errors };
    let xml = result.to_xml(quiet);

    Ok(xml_response(xml))
}

fn validate_delete_objects_count(count: usize) -> Result<(), S3Error> {
    if count > 1000 {
        return Err(S3Error::InvalidArgument(
            "DeleteObjects supports at most 1000 keys per request".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delete_objects_rejects_more_than_1000_keys() {
        assert!(validate_delete_objects_count(1000).is_ok());
        assert!(matches!(
            validate_delete_objects_count(1001),
            Err(S3Error::InvalidArgument(_))
        ));
    }
}
