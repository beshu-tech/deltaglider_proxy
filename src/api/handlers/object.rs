//! Object-level S3 handlers: GET, HEAD, PUT (with copy detection), DELETE.

use super::{
    base64_decode, build_object_headers, extract_content_type, extract_user_metadata, xml_response,
    AppState, ObjectQuery, S3Error,
};
use crate::deltaglider::RetrieveResponse;
use axum::body::{Body, Bytes};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use std::sync::Arc;
use tracing::{debug, info, instrument, warn};

use crate::api::aws_chunked::{decode_aws_chunked, get_decoded_content_length, is_aws_chunked};
use crate::api::extractors::{ValidatedBucket, ValidatedPath};
use crate::api::xml::{DeleteError, DeleteRequest, DeleteResult, DeletedObject, ListPartsResult};

/// Query parameters for bucket-level POST operations
#[derive(Debug, serde::Deserialize, Default)]
pub struct BucketPostQuery {
    pub delete: Option<String>,
}

/// PUT object handler (internal)
/// Called by put_object_or_copy after validation
#[instrument(skip(state, body))]
async fn put_object_inner(
    state: &Arc<AppState>,
    bucket: &str,
    key: &str,
    headers: &HeaderMap,
    body: &Bytes,
) -> Result<Response, S3Error> {
    info!("PUT {}/{} ({} bytes)", bucket, key, body.len());

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
#[instrument(skip(state))]
async fn copy_object_inner(
    state: &Arc<AppState>,
    bucket: &str,
    key: &str,
    headers: &HeaderMap,
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

    info!(
        "COPY {}/{} -> {}/{}",
        source_bucket, source_key, bucket, key
    );

    // Load engine once for the entire copy operation to ensure consistency.
    let engine = state.engine.load();

    // Check source object size before loading into memory to avoid transient
    // memory spikes if max_object_size was reduced after the object was stored.
    let source_meta_head = engine.head(source_bucket, source_key).await?;
    if source_meta_head.file_size > engine.max_object_size() {
        return Err(S3Error::EntityTooLarge {
            size: source_meta_head.file_size,
            max: engine.max_object_size(),
        });
    }

    // Retrieve source object
    let (data, source_meta) = engine.retrieve(source_bucket, source_key).await?;

    // Store as new object, preserving user metadata from source
    let result = engine
        .store(
            bucket,
            key,
            &data,
            source_meta.content_type.clone(),
            source_meta.user_metadata.clone(),
        )
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

/// PUT object handler with copy detection and multipart upload support
/// PUT /{bucket}/{key}
/// Detects x-amz-copy-source header to dispatch to copy operation
/// Detects ?partNumber&uploadId for multipart upload part
#[instrument(skip(state, body))]
pub async fn put_object_or_copy(
    State(state): State<Arc<AppState>>,
    ValidatedPath { bucket, key }: ValidatedPath,
    Query(query): Query<ObjectQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, S3Error> {
    // Decode AWS chunked transfer encoding if present
    let decoded_body = if is_aws_chunked(&headers) {
        let expected_len = get_decoded_content_length(&headers);
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
                decoded
            }
            None => {
                warn!(
                    "Failed to decode AWS chunked payload, using raw body ({} bytes)",
                    body.len()
                );
                body
            }
        }
    } else {
        body
    };

    // Check if this is a multipart upload part
    if let (Some(part_num), Some(upload_id)) = (&query.part_number, &query.upload_id) {
        info!(
            "UploadPart {}/{} part={} uploadId={}",
            bucket, key, part_num, upload_id
        );

        // Validate Content-MD5 header if present
        if let Some(content_md5) = headers.get("content-md5").and_then(|v| v.to_str().ok()) {
            use md5::Digest;
            let computed = md5::Md5::digest(&decoded_body);
            let expected = base64_decode(content_md5);
            if let Some(expected) = expected {
                if computed.as_slice() != expected.as_slice() {
                    return Err(S3Error::BadDigest);
                }
            }
        }

        let etag =
            state
                .multipart
                .upload_part(upload_id, &bucket, &key, *part_num, decoded_body)?;
        return Ok((StatusCode::OK, [("ETag", etag)], "").into_response());
    }

    // Check if this is a copy operation
    if headers.contains_key("x-amz-copy-source") {
        copy_object_inner(&state, &bucket, &key, &headers).await
    } else {
        put_object_inner(&state, &bucket, &key, &headers, &decoded_body).await
    }
}

/// GET object handler
/// GET /{bucket}/{key}
/// GET /{bucket}/{key}?uploadId=X - ListParts
///
/// Direct files are streamed from the backend (constant memory, low TTFB).
/// Delta files are reconstructed in memory and sent as a buffered response.
#[instrument(skip(state))]
pub async fn get_object(
    State(state): State<Arc<AppState>>,
    ValidatedPath { bucket, key }: ValidatedPath,
    Query(query): Query<ObjectQuery>,
) -> Result<Response, S3Error> {
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

    let response = state.engine.load().retrieve_stream(&bucket, &key).await?;

    match response {
        RetrieveResponse::Streamed { stream, metadata } => {
            debug!(
                "Streaming {}/{} (stored as {})",
                bucket,
                key,
                metadata.storage_info.label()
            );
            let headers = build_object_headers(&metadata);
            let body = Body::from_stream(stream);
            Ok((StatusCode::OK, headers, body).into_response())
        }
        RetrieveResponse::Buffered { data, metadata } => {
            debug!(
                "Retrieved {}/{} ({} bytes, stored as {})",
                bucket,
                key,
                data.len(),
                metadata.storage_info.label()
            );
            let headers = build_object_headers(&metadata);
            Ok((StatusCode::OK, headers, data).into_response())
        }
    }
}

/// HEAD object handler
/// HEAD /{bucket}/{key}
#[instrument(skip(state))]
pub async fn head_object(
    State(state): State<Arc<AppState>>,
    ValidatedPath { bucket, key }: ValidatedPath,
) -> Result<Response, S3Error> {
    info!("HEAD {}/{}", bucket, key);

    let metadata = state.engine.load().head(&bucket, &key).await?;

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
) -> Result<Response, S3Error> {
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

    let result = DeleteResult { deleted, errors };
    let xml = result.to_xml(quiet);

    Ok(xml_response(xml))
}
