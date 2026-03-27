//! Object-level S3 handlers: GET, HEAD, PUT (with copy detection), DELETE.

use super::bucket::get_acl_response;
use super::object_helpers::{
    apply_response_overrides, build_range_response, check_conditionals, copy_object_inner,
    decode_body, parse_range_header, put_object_inner, upload_part, upload_part_copy,
};
use super::{audit_log_s3, build_object_headers, xml_response, AppState, ObjectQuery, S3Error};
use crate::deltaglider::RetrieveResponse;
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
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, S3Error> {
    let auth_user = auth_user.map(|axum::Extension(u)| u);

    // PUT /{bucket}/{key}?acl — accept and ignore (ACL stub)
    if query.acl.is_some() {
        info!("PUT object ACL (stub): {}/{}", bucket, key);
        return Ok(StatusCode::OK.into_response());
    }

    // PUT /{bucket}/{key}?tagging — accept and ignore (tagging stub)
    if query.tagging.is_some() {
        info!("PUT object tagging (stub): {}/{}", bucket, key);
        // Verify the object exists first; S3 returns 404 for tagging on non-existent objects
        state.engine.load().head(&bucket, &key).await?;
        return Ok(StatusCode::OK.into_response());
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
        );
    }

    // Copy vs direct put
    let is_copy = headers.contains_key("x-amz-copy-source");
    let result = if is_copy {
        copy_object_inner(&state, &bucket, &key, &headers, &auth_user).await
    } else {
        put_object_inner(&state, &bucket, &key, &headers, &decoded_body).await
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
    // GET /{bucket}/{key}?tagging — return empty tagging response
    if query.tagging.is_some() {
        info!("GET object tagging (stub): {}/{}", bucket, key);
        // Verify the object exists first; S3 returns 404 for tagging on non-existent objects
        state.engine.load().head(&bucket, &key).await?;
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<Tagging><TagSet/></Tagging>"#;
        return Ok(xml_response(xml));
    }

    // GET /{bucket}/{key}?acl — return canned ACL response
    if query.acl.is_some() {
        info!("GET object ACL: {}/{}", bucket, key);
        // Verify the object exists first; S3 returns 404 for ACL on non-existent objects
        state.engine.load().head(&bucket, &key).await?;
        return get_acl_response();
    }

    // ListParts
    if let Some(upload_id) = &query.upload_id {
        info!("ListParts {}/{} uploadId={}", bucket, key, upload_id);
        let parts = state.multipart.list_parts(upload_id, &bucket, &key)?;
        let result = ListPartsResult {
            bucket: bucket.clone(),
            key: key.clone(),
            upload_id: upload_id.clone(),
            parts,
            max_parts: 1000,
            is_truncated: false,
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

            // For streamed responses with Range requests, we need to buffer first
            if let Some(ref range) = range_request {
                use futures::TryStreamExt;
                let chunks: Vec<Bytes> = stream
                    .map_err(|e| std::io::Error::other(e.to_string()))
                    .try_collect()
                    .await
                    .map_err(|e| S3Error::InternalError(e.to_string()))?;
                let total_len: usize = chunks.iter().map(|b| b.len()).sum();
                let mut data = Vec::with_capacity(total_len);
                for chunk in &chunks {
                    data.extend_from_slice(chunk);
                }
                return build_range_response(data, &metadata, range, None, &query);
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
    // DELETE /{bucket}/{key}?tagging — no-op (tagging stub)
    if query.tagging.is_some() {
        info!("DELETE object tagging (stub): {}/{}", bucket, key);
        // Verify the object exists first; S3 returns 404 for tagging on non-existent objects
        state.engine.load().head(&bucket, &key).await?;
        return Ok(StatusCode::NO_CONTENT.into_response());
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

    info!("DELETE {}/{}", bucket, key);

    if let Err(err) = state.engine.load().delete(&bucket, &key).await {
        match S3Error::from(err) {
            S3Error::NoSuchKey(_) => {}
            other => return Err(other),
        }
    }

    debug!("Deleted {}/{}", bucket, key);

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

    info!(
        "DELETE multiple objects in {} ({} objects)",
        bucket,
        delete_req.objects.len()
    );

    let quiet = delete_req.quiet.unwrap_or(false);
    let mut deleted = Vec::new();
    let mut errors = Vec::new();

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
