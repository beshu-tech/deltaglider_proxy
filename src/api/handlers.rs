//! S3 API request handlers

use super::aws_chunked::{decode_aws_chunked, get_decoded_content_length, is_aws_chunked};
use super::errors::S3Error;
use super::extractors::{ValidatedBucket, ValidatedPath};
use super::xml::{
    escape_xml, BucketInfo, CopyObjectResult, DeleteError, DeleteRequest, DeleteResult,
    DeletedObject, ListBucketResult, ListBucketsResult, S3Object,
};
use crate::deltaglider::DynEngine;
use crate::types::StorageInfo;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{debug, info, instrument, warn};

/// Application state shared across handlers
pub struct AppState {
    pub engine: DynEngine,
    pub default_bucket: String,
}

/// Query parameters for bucket-level GET operations
#[derive(Debug, Deserialize, Default)]
pub struct BucketGetQuery {
    pub prefix: Option<String>,
    #[serde(rename = "list-type")]
    pub list_type: Option<u8>,
    #[serde(rename = "max-keys")]
    pub max_keys: Option<u32>,
    #[serde(rename = "continuation-token")]
    pub continuation_token: Option<String>,
    /// GetBucketLocation query parameter
    pub location: Option<String>,
    /// GetBucketVersioning query parameter
    pub versioning: Option<String>,
    /// ListMultipartUploads query parameter
    pub uploads: Option<String>,
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

    let content_type = headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let result = state
        .engine
        .store(&bucket, key, &body, content_type)
        .await?;

    let storage_type = match &result.metadata.storage_info {
        StorageInfo::Reference { .. } => "Reference",
        StorageInfo::Delta { .. } => "Delta",
        StorageInfo::Direct => "Direct",
    };

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

/// GET object handler
/// GET /{bucket}/{key}
#[instrument(skip(state))]
pub async fn get_object(
    State(state): State<Arc<AppState>>,
    ValidatedPath { bucket, key }: ValidatedPath,
) -> Result<Response, S3Error> {
    info!("GET {}/{}", bucket, key);

    let (data, metadata) = state.engine.retrieve(&bucket, &key).await?;

    let storage_type = match &metadata.storage_info {
        StorageInfo::Reference { .. } => "reference",
        StorageInfo::Delta { .. } => "delta",
        StorageInfo::Direct => "direct",
    };

    debug!(
        "Retrieved {}/{} ({} bytes, stored as {})",
        bucket,
        key,
        data.len(),
        storage_type
    );

    let content_type = metadata
        .content_type
        .clone()
        .unwrap_or_else(|| "application/octet-stream".to_string());

    Ok((
        StatusCode::OK,
        [
            ("ETag", metadata.etag()),
            ("Content-Length", metadata.file_size.to_string()),
            ("Content-Type", content_type),
            (
                "Last-Modified",
                metadata
                    .created_at
                    .format("%a, %d %b %Y %H:%M:%S GMT")
                    .to_string(),
            ),
        ],
        data,
    )
        .into_response())
}

/// HEAD object handler
/// HEAD /{bucket}/{key}
#[instrument(skip(state))]
pub async fn head_object(
    State(state): State<Arc<AppState>>,
    ValidatedPath { bucket, key }: ValidatedPath,
) -> Result<Response, S3Error> {
    info!("HEAD {}/{}", bucket, key);

    let metadata = state.engine.head(&bucket, &key).await?;

    let content_type = metadata
        .content_type
        .clone()
        .unwrap_or_else(|| "application/octet-stream".to_string());

    Ok((
        StatusCode::OK,
        [
            ("ETag", metadata.etag()),
            ("Content-Length", metadata.file_size.to_string()),
            ("Content-Type", content_type),
            (
                "Last-Modified",
                metadata
                    .created_at
                    .format("%a, %d %b %Y %H:%M:%S GMT")
                    .to_string(),
            ),
        ],
    )
        .into_response())
}

/// Bucket-level GET handler - dispatches to appropriate operation based on query params
/// GET /{bucket}?list-type=2&prefix=  -> ListObjectsV2
/// GET /{bucket}?location            -> GetBucketLocation
/// GET /{bucket}?versioning          -> GetBucketVersioning
/// GET /{bucket}?uploads             -> ListMultipartUploads
#[instrument(skip(state))]
pub async fn bucket_get_handler(
    State(state): State<Arc<AppState>>,
    ValidatedBucket(bucket): ValidatedBucket,
    Query(query): Query<BucketGetQuery>,
) -> Result<Response, S3Error> {
    // Check for GetBucketLocation
    if query.location.is_some() {
        info!("GET bucket location: {}", bucket);
        return get_bucket_location(&bucket).await;
    }

    // Check for GetBucketVersioning
    if query.versioning.is_some() {
        info!("GET bucket versioning: {}", bucket);
        return get_bucket_versioning(&bucket).await;
    }

    // Check for ListMultipartUploads
    if query.uploads.is_some() {
        info!("LIST multipart uploads: {}", bucket);
        return list_multipart_uploads(&bucket).await;
    }

    // Default: ListObjects
    if let Some(list_type) = query.list_type {
        if list_type != 2 {
            return Err(S3Error::InvalidArgument(
                "Only ListObjectsV2 is supported (list-type=2)".to_string(),
            ));
        }
    }
    let prefix = query.prefix.unwrap_or_default();
    info!("LIST {}/{}*", bucket, prefix);

    let page = state
        .engine
        .list_objects_v2(
            &bucket,
            &prefix,
            query.max_keys.unwrap_or(1000),
            query.continuation_token.as_deref(),
        )
        .await?;

    let s3_objects: Vec<S3Object> = page
        .objects
        .into_iter()
        .map(|(key, meta)| S3Object::new(key, meta.file_size, meta.created_at, meta.etag()))
        .collect();

    let result = ListBucketResult::new_v2(
        bucket,
        prefix,
        query.max_keys.unwrap_or(1000),
        s3_objects,
        query.continuation_token,
        page.next_continuation_token,
        page.is_truncated,
    );
    let xml = result.to_xml();

    Ok((StatusCode::OK, [("Content-Type", "application/xml")], xml).into_response())
}

/// GetBucketLocation handler
/// GET /{bucket}?location
async fn get_bucket_location(_bucket: &str) -> Result<Response, S3Error> {
    // Return a fixed location - we use us-east-1 as default
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<LocationConstraint xmlns="http://s3.amazonaws.com/doc/2006-03-01/">us-east-1</LocationConstraint>"#;
    Ok((StatusCode::OK, [("Content-Type", "application/xml")], xml).into_response())
}

/// GetBucketVersioning handler
/// GET /{bucket}?versioning
async fn get_bucket_versioning(_bucket: &str) -> Result<Response, S3Error> {
    // Return empty VersioningConfiguration - versioning is not enabled
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<VersioningConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"/>"#;
    Ok((StatusCode::OK, [("Content-Type", "application/xml")], xml).into_response())
}

/// ListMultipartUploads handler (stub)
/// GET /{bucket}?uploads
async fn list_multipart_uploads(bucket: &str) -> Result<Response, S3Error> {
    // Return empty list - no ongoing multipart uploads
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<ListMultipartUploadsResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Bucket>{}</Bucket>
  <KeyMarker/>
  <UploadIdMarker/>
  <MaxUploads>1000</MaxUploads>
  <IsTruncated>false</IsTruncated>
</ListMultipartUploadsResult>"#,
        escape_xml(bucket)
    );
    Ok((StatusCode::OK, [("Content-Type", "application/xml")], xml).into_response())
}

/// Alias for backward compatibility
pub async fn list_objects(
    state: State<Arc<AppState>>,
    bucket: ValidatedBucket,
    query: Query<BucketGetQuery>,
) -> Result<Response, S3Error> {
    bucket_get_handler(state, bucket, query).await
}

/// DELETE object handler
/// DELETE /{bucket}/{key}
#[instrument(skip(state))]
pub async fn delete_object(
    State(state): State<Arc<AppState>>,
    ValidatedPath { bucket, key }: ValidatedPath,
) -> Result<Response, S3Error> {
    info!("DELETE {}/{}", bucket, key);

    if let Err(err) = state.engine.delete(&bucket, &key).await {
        match S3Error::from(err) {
            S3Error::NoSuchKey(_) => {}
            other => return Err(other),
        }
    }

    debug!("Deleted {}/{}", bucket, key);

    // S3 returns 204 No Content on successful delete
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Query parameters for bucket-level POST operations
#[derive(Debug, Deserialize, Default)]
pub struct BucketPostQuery {
    pub delete: Option<String>,
}

/// Query parameters for object-level operations (multipart upload)
#[derive(Debug, Deserialize, Default)]
pub struct ObjectQuery {
    /// CreateMultipartUpload (POST with ?uploads)
    pub uploads: Option<String>,
    /// UploadPart / CompleteMultipartUpload (with ?uploadId)
    #[serde(rename = "uploadId")]
    pub upload_id: Option<String>,
    /// UploadPart (PUT with ?partNumber)
    #[serde(rename = "partNumber")]
    pub part_number: Option<u32>,
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
    // Ensure this is a delete request
    if query.delete.is_none() {
        return Err(S3Error::InvalidRequest(
            "POST requires ?delete query parameter".to_string(),
        ));
    }

    // Parse XML body
    let body_str = String::from_utf8(body.to_vec())
        .map_err(|_| S3Error::MalformedXML)?;

    let delete_req = DeleteRequest::from_xml(&body_str)
        .map_err(|e| {
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
        match state.engine.delete(&bucket, key).await {
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

    Ok((StatusCode::OK, [("Content-Type", "application/xml")], xml).into_response())
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

    let (source_bucket, source_key) = copy_source.split_once('/').ok_or_else(|| {
        S3Error::InvalidArgument("Copy source must be bucket/key".to_string())
    })?;

    if source_bucket != state.default_bucket {
        return Err(S3Error::NoSuchBucket(source_bucket.to_string()));
    }

    info!(
        "COPY {}/{} -> {}/{}",
        source_bucket, source_key, bucket, key
    );

    // Retrieve source object
    let (data, source_meta) = state.engine.retrieve(source_bucket, source_key).await?;

    // Store as new object
    let result = state
        .engine
        .store(bucket, key, &data, source_meta.content_type.clone())
        .await?;

    debug!(
        "Copied {}/{} -> {}/{} ({} bytes)",
        source_bucket,
        source_key,
        bucket,
        key,
        data.len()
    );

    let copy_result = CopyObjectResult {
        etag: result.metadata.etag(),
        last_modified: result.metadata.created_at,
    };
    let xml = copy_result.to_xml();

    Ok((StatusCode::OK, [("Content-Type", "application/xml")], xml).into_response())
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
    // Check if this is a multipart upload part
    if query.part_number.is_some() && query.upload_id.is_some() {
        info!(
            "UploadPart {}/{} part={} uploadId={}",
            bucket,
            key,
            query.part_number.unwrap(),
            query.upload_id.as_ref().unwrap()
        );
        return Err(S3Error::NotImplemented(
            "Multipart upload is not supported. Use single PUT for uploads.".to_string(),
        ));
    }

    // Check if this is a copy operation
    if headers.contains_key("x-amz-copy-source") {
        copy_object_inner(&state, &bucket, &key, &headers).await
    } else {
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

        put_object_inner(&state, &bucket, &key, &headers, &decoded_body).await
    }
}

/// POST object handler for multipart upload operations
/// POST /{bucket}/{key}?uploads - CreateMultipartUpload
/// POST /{bucket}/{key}?uploadId=X - CompleteMultipartUpload
#[instrument(skip(_state, _body))]
pub async fn post_object(
    State(_state): State<Arc<AppState>>,
    ValidatedPath { bucket, key }: ValidatedPath,
    Query(query): Query<ObjectQuery>,
    _body: Bytes,
) -> Result<Response, S3Error> {
    // CreateMultipartUpload
    if query.uploads.is_some() {
        info!("CreateMultipartUpload {}/{}", bucket, key);
        return Err(S3Error::NotImplemented(
            "Multipart upload is not supported. Use single PUT for uploads.".to_string(),
        ));
    }

    // CompleteMultipartUpload
    if query.upload_id.is_some() {
        info!(
            "CompleteMultipartUpload {}/{} uploadId={}",
            bucket,
            key,
            query.upload_id.as_ref().unwrap()
        );
        return Err(S3Error::NotImplemented(
            "Multipart upload is not supported. Use single PUT for uploads.".to_string(),
        ));
    }

    Err(S3Error::InvalidRequest(
        "POST on object requires ?uploads or ?uploadId parameter".to_string(),
    ))
}

// ============================================================================
// Bucket Operations
// ============================================================================

/// CREATE bucket handler
/// PUT /{bucket}
#[instrument(skip(state))]
pub async fn create_bucket(
    State(state): State<Arc<AppState>>,
    Path(bucket): Path<String>,
) -> Result<Response, S3Error> {
    info!("CREATE bucket {}", bucket);

    // Only allow creating the configured default bucket
    if bucket != state.default_bucket {
        // In a multi-bucket implementation, we'd create a new bucket here
        // For now, we only support the single configured bucket
        return Err(S3Error::InvalidRequest(
            "Only the configured default bucket is supported".to_string(),
        ));
    }

    // The bucket always "exists" (it's created on first use)
    // Return 200 OK (S3 returns 200 for existing owned bucket)
    Ok((
        StatusCode::OK,
        [("Location", format!("/{}", bucket))],
        "",
    )
        .into_response())
}

/// DELETE bucket handler
/// DELETE /{bucket}
#[instrument(skip(state))]
pub async fn delete_bucket(
    State(state): State<Arc<AppState>>,
    ValidatedBucket(bucket): ValidatedBucket,
) -> Result<Response, S3Error> {
    info!("DELETE bucket {}", bucket);

    // Check if bucket is empty
    let objects = state.engine.list(&bucket, "").await?;
    if !objects.is_empty() {
        return Err(S3Error::BucketNotEmpty(bucket));
    }

    // Bucket is empty - in single-bucket mode, we just confirm it can be deleted
    // (actual deletion would require removing the bucket directory/prefix)
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// HEAD bucket handler
/// HEAD /{bucket}
#[instrument]
pub async fn head_bucket(
    ValidatedBucket(bucket): ValidatedBucket,
) -> Result<Response, S3Error> {
    info!("HEAD bucket {}", bucket);

    // Bucket exists (validated by extractor)
    Ok((StatusCode::OK, [("x-amz-bucket-region", "us-east-1")]).into_response())
}

/// LIST buckets handler
/// GET /
#[instrument(skip(state))]
pub async fn list_buckets(
    State(state): State<Arc<AppState>>,
) -> Result<Response, S3Error> {
    info!("LIST buckets");

    // Return the single configured bucket
    let result = ListBucketsResult {
        owner_id: "deltaglider_proxy".to_string(),
        owner_display_name: "DeltaGlider Proxy".to_string(),
        buckets: vec![BucketInfo {
            name: state.default_bucket.clone(),
            creation_date: Utc::now(), // We don't track actual creation time
        }],
    };
    let xml = result.to_xml();

    Ok((StatusCode::OK, [("Content-Type", "application/xml")], xml).into_response())
}

// ============================================================================
// Health Check
// ============================================================================

/// Health check response
#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub version: String,
    pub backend: String,
}

/// Health check handler
/// GET /health
pub async fn health_check() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "healthy".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        backend: "ready".to_string(),
    })
}
