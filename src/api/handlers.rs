//! S3 API request handlers

use super::aws_chunked::{decode_aws_chunked, get_decoded_content_length, is_aws_chunked};
use super::errors::S3Error;
use super::extractors::{ValidatedBucket, ValidatedPath};
use super::xml::{
    BucketInfo, CompleteMultipartUploadRequest, CompleteMultipartUploadResult, CopyObjectResult,
    DeleteError, DeleteRequest, DeleteResult, DeletedObject, InitiateMultipartUploadResult,
    ListBucketResult, ListBucketsResult, ListMultipartUploadsResult, ListPartsResult, S3Object,
};
use crate::deltaglider::{DynEngine, RetrieveResponse};
use crate::multipart::MultipartStore;
use crate::types::{FileMetadata, StorageInfo};
use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{debug, info, instrument, warn};

/// Application state shared across handlers
pub struct AppState {
    pub engine: DynEngine,
    pub multipart: Arc<MultipartStore>,
}

/// Query parameters for bucket-level GET operations
#[derive(Debug, Deserialize, Default)]
pub struct BucketGetQuery {
    pub prefix: Option<String>,
    pub delimiter: Option<String>,
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

    let user_metadata = extract_user_metadata(headers);

    let result = state
        .engine
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

/// Build response headers for an object including DeltaGlider custom metadata.
fn build_object_headers(metadata: &FileMetadata) -> HeaderMap {
    let stored_size = metadata.delta_size().unwrap_or(metadata.file_size);
    let content_type = metadata
        .content_type
        .clone()
        .unwrap_or_else(|| "application/octet-stream".to_string());

    let mut headers = HeaderMap::new();
    headers.insert("ETag", hval(&metadata.etag()));
    headers.insert("Content-Length", hval(&metadata.file_size.to_string()));
    headers.insert("Content-Type", hval(&content_type));
    headers.insert(
        "Last-Modified",
        hval(
            &metadata
                .created_at
                .format("%a, %d %b %Y %H:%M:%S GMT")
                .to_string(),
        ),
    );
    headers.insert("x-amz-storage-type", hval(metadata.storage_info.label()));
    headers.insert("x-deltaglider-stored-size", hval(&stored_size.to_string()));

    // DeltaGlider custom metadata (x-amz-meta-dg-*)
    use crate::types::meta_keys as mk;
    headers.insert(mk::H_TOOL, hval(&metadata.tool));
    headers.insert(mk::H_ORIGINAL_NAME, hval(&metadata.original_name));
    headers.insert(mk::H_FILE_SHA256, hval(&metadata.file_sha256));
    headers.insert(mk::H_FILE_SIZE, hval(&metadata.file_size.to_string()));

    match &metadata.storage_info {
        StorageInfo::Reference { source_name } => {
            headers.insert(mk::H_NOTE, hval("reference"));
            headers.insert(mk::H_SOURCE_NAME, hval(source_name));
        }
        StorageInfo::Delta {
            ref_key,
            ref_sha256,
            delta_size,
            delta_cmd,
        } => {
            headers.insert(mk::H_NOTE, hval("delta"));
            headers.insert(mk::H_REF_KEY, hval(ref_key));
            headers.insert(mk::H_REF_SHA256, hval(ref_sha256));
            headers.insert(mk::H_DELTA_SIZE, hval(&delta_size.to_string()));
            headers.insert(mk::H_DELTA_CMD, hval(delta_cmd));
        }
        StorageInfo::Direct => {
            headers.insert(mk::H_NOTE, hval("direct"));
        }
    }

    // User-provided custom metadata (x-amz-meta-*)
    for (key, value) in &metadata.user_metadata {
        let header_name = format!("x-amz-meta-{}", key);
        if let Ok(name) = axum::http::header::HeaderName::from_bytes(header_name.as_bytes()) {
            headers.insert(name, hval(value));
        }
    }

    headers
}

fn hval(s: &str) -> HeaderValue {
    HeaderValue::from_bytes(s.as_bytes()).unwrap_or_else(|_| HeaderValue::from_static(""))
}

/// Extract user-provided x-amz-meta-* headers, excluding DeltaGlider internal metadata (dg-*).
fn extract_user_metadata(headers: &HeaderMap) -> std::collections::HashMap<String, String> {
    use crate::types::meta_keys as mk;
    headers
        .iter()
        .filter_map(|(name, value)| {
            let name_str = name.as_str();
            if let Some(suffix) = name_str.strip_prefix(mk::AMZ_META_PREFIX) {
                if !suffix.starts_with("dg-") {
                    if let Ok(v) = value.to_str() {
                        return Some((suffix.to_string(), v.to_string()));
                    }
                }
            }
            None
        })
        .collect()
}

/// Decode base64 string to bytes (for Content-MD5 validation)
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(input.trim())
        .ok()
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
        return Ok((StatusCode::OK, [("Content-Type", "application/xml")], xml).into_response());
    }

    info!("GET {}/{}", bucket, key);

    let response = state.engine.retrieve_stream(&bucket, &key).await?;

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

    let metadata = state.engine.head(&bucket, &key).await?;

    let headers = build_object_headers(&metadata);
    Ok((StatusCode::OK, headers).into_response())
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
        let prefix = query.prefix.as_deref();
        return list_multipart_uploads(&state, &bucket, prefix).await;
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
    let delimiter = query.delimiter.clone();
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

    let all_objects: Vec<S3Object> = page
        .objects
        .into_iter()
        .map(|(key, meta)| S3Object::new(key, meta.file_size, meta.created_at, meta.etag()))
        .collect();

    // If a delimiter is set, compute CommonPrefixes and filter objects
    let (s3_objects, common_prefixes) = if let Some(ref delim) = delimiter {
        let mut prefixes = std::collections::BTreeSet::new();
        let mut direct_objects = Vec::new();

        for obj in all_objects {
            let after_prefix = &obj.key[prefix.len()..];
            if let Some(pos) = after_prefix.find(delim.as_str()) {
                // This key contains the delimiter after the prefix — it belongs to a common prefix
                let common = format!("{}{}{}", prefix, &after_prefix[..pos], delim);
                prefixes.insert(common);
            } else {
                // This key is directly at this level
                direct_objects.push(obj);
            }
        }

        (direct_objects, prefixes.into_iter().collect::<Vec<_>>())
    } else {
        (all_objects, Vec::new())
    };

    let result = ListBucketResult::new_v2(
        bucket,
        prefix,
        delimiter,
        query.max_keys.unwrap_or(1000),
        s3_objects,
        common_prefixes,
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

/// ListMultipartUploads handler
/// GET /{bucket}?uploads
async fn list_multipart_uploads(
    state: &Arc<AppState>,
    bucket: &str,
    prefix: Option<&str>,
) -> Result<Response, S3Error> {
    let uploads = state.multipart.list_uploads(Some(bucket), prefix);
    let result = ListMultipartUploadsResult {
        bucket: bucket.to_string(),
        uploads,
        prefix: prefix.unwrap_or("").to_string(),
        max_uploads: 1000,
        is_truncated: false,
    };
    let xml = result.to_xml();
    Ok((StatusCode::OK, [("Content-Type", "application/xml")], xml).into_response())
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
    let body_str = String::from_utf8(body.to_vec()).map_err(|_| S3Error::MalformedXML)?;

    let delete_req = DeleteRequest::from_xml(&body_str).map_err(|e| {
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

    let (source_bucket, source_key) = copy_source
        .split_once('/')
        .ok_or_else(|| S3Error::InvalidArgument("Copy source must be bucket/key".to_string()))?;

    info!(
        "COPY {}/{} -> {}/{}",
        source_bucket, source_key, bucket, key
    );

    // Check source object size before loading into memory to avoid transient
    // memory spikes if max_object_size was reduced after the object was stored.
    let source_meta_head = state.engine.head(source_bucket, source_key).await?;
    if source_meta_head.file_size > state.engine.max_object_size() {
        return Err(S3Error::EntityTooLarge {
            size: source_meta_head.file_size,
            max: state.engine.max_object_size(),
        });
    }

    // Retrieve source object
    let (data, source_meta) = state.engine.retrieve(source_bucket, source_key).await?;

    // Store as new object, preserving user metadata from source
    let result = state
        .engine
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

/// POST object handler for multipart upload operations
/// POST /{bucket}/{key}?uploads - CreateMultipartUpload
/// POST /{bucket}/{key}?uploadId=X - CompleteMultipartUpload
#[instrument(skip(state, body))]
pub async fn post_object(
    State(state): State<Arc<AppState>>,
    ValidatedPath { bucket, key }: ValidatedPath,
    Query(query): Query<ObjectQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, S3Error> {
    // CreateMultipartUpload
    if query.uploads.is_some() {
        info!("CreateMultipartUpload {}/{}", bucket, key);

        let content_type = headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let user_metadata = extract_user_metadata(&headers);

        let upload_id = state
            .multipart
            .create(&bucket, &key, content_type, user_metadata);

        let result = InitiateMultipartUploadResult {
            bucket: bucket.clone(),
            key: key.clone(),
            upload_id,
        };
        let xml = result.to_xml();
        return Ok((StatusCode::OK, [("Content-Type", "application/xml")], xml).into_response());
    }

    // CompleteMultipartUpload
    if let Some(upload_id) = &query.upload_id {
        info!(
            "CompleteMultipartUpload {}/{} uploadId={}",
            bucket, key, upload_id
        );

        let body_str = String::from_utf8(body.to_vec()).map_err(|_| S3Error::MalformedXML)?;
        let complete_req = CompleteMultipartUploadRequest::from_xml(&body_str).map_err(|e| {
            warn!("Failed to parse CompleteMultipartUpload XML: {}", e);
            S3Error::MalformedXML
        })?;

        let requested_parts: Vec<(u32, String)> = complete_req
            .parts
            .iter()
            .map(|p| (p.part_number, p.etag.clone()))
            .collect();

        let completed = state
            .multipart
            .complete(upload_id, &bucket, &key, &requested_parts)?;

        let multipart_etag = completed.etag.clone();

        // Store assembled object through the engine
        let store_result = state
            .engine
            .store(
                &bucket,
                &key,
                &completed.data,
                completed.content_type,
                completed.user_metadata,
            )
            .await?;

        // Remove upload only after successful store
        state.multipart.remove(upload_id);

        debug!(
            "CompleteMultipartUpload {}/{} stored as {}, {} bytes",
            bucket,
            key,
            store_result.metadata.storage_info.label(),
            completed.data.len()
        );

        let result = CompleteMultipartUploadResult {
            location: format!("/{}/{}", bucket, key),
            bucket: bucket.clone(),
            key: key.clone(),
            etag: multipart_etag,
        };
        let xml = result.to_xml();
        return Ok((
            StatusCode::OK,
            [
                ("Content-Type", "application/xml"),
                (
                    "x-amz-storage-type",
                    store_result.metadata.storage_info.label(),
                ),
            ],
            xml,
        )
            .into_response());
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

    if bucket.is_empty() {
        return Err(S3Error::InvalidArgument(
            "Bucket name cannot be empty".to_string(),
        ));
    }

    // Create the real bucket on the storage backend
    state.engine.create_bucket(&bucket).await?;

    Ok((StatusCode::OK, [("Location", format!("/{}", bucket))], "").into_response())
}

/// DELETE bucket handler
/// DELETE /{bucket}
#[instrument(skip(state))]
pub async fn delete_bucket(
    State(state): State<Arc<AppState>>,
    ValidatedBucket(bucket): ValidatedBucket,
) -> Result<Response, S3Error> {
    info!("DELETE bucket {}", bucket);

    // Check if bucket is empty (S3 requires buckets to be empty before deletion)
    let page = state.engine.list_objects_v2(&bucket, "", 1, None).await?;
    if !page.objects.is_empty() {
        return Err(S3Error::BucketNotEmpty(bucket.to_string()));
    }

    // Delete the real bucket on the storage backend
    state.engine.delete_bucket(&bucket).await?;

    Ok(StatusCode::NO_CONTENT.into_response())
}

/// HEAD bucket handler
/// HEAD /{bucket}
#[instrument(skip(state))]
pub async fn head_bucket(
    State(state): State<Arc<AppState>>,
    ValidatedBucket(bucket): ValidatedBucket,
) -> Result<Response, S3Error> {
    info!("HEAD bucket {}", bucket);

    // Check if bucket exists on the storage backend
    let exists = state.engine.head_bucket(&bucket).await?;
    if !exists {
        return Err(S3Error::NoSuchBucket(bucket.to_string()));
    }

    Ok((StatusCode::OK, [("x-amz-bucket-region", "us-east-1")]).into_response())
}

/// LIST buckets handler
/// GET /
#[instrument(skip(state))]
pub async fn list_buckets(State(state): State<Arc<AppState>>) -> Result<Response, S3Error> {
    info!("LIST buckets");

    // List real buckets from storage backend
    let mut bucket_list = state.engine.list_buckets().await?;
    bucket_list.sort();

    let result = ListBucketsResult {
        owner_id: "deltaglider_proxy".to_string(),
        owner_display_name: "DeltaGlider Proxy".to_string(),
        buckets: bucket_list
            .into_iter()
            .map(|name| BucketInfo {
                name,
                creation_date: Utc::now(),
            })
            .collect(),
    };
    let xml = result.to_xml();

    Ok((StatusCode::OK, [("Content-Type", "application/xml")], xml).into_response())
}

// ============================================================================
// Stats
// ============================================================================

/// Query parameters for /stats endpoint
#[derive(Debug, Deserialize, Default)]
pub struct StatsQuery {
    pub bucket: Option<String>,
}

/// Aggregate storage statistics
#[derive(Debug, Serialize)]
pub struct StatsResponse {
    pub total_objects: u64,
    pub total_original_size: u64,
    pub total_stored_size: u64,
    pub savings_percentage: f64,
}

/// Stats handler
/// GET /stats — aggregate stats across all buckets
/// GET /stats?bucket=NAME — stats for a specific bucket
pub async fn get_stats(
    State(state): State<Arc<AppState>>,
    Query(query): Query<StatsQuery>,
) -> Result<Json<StatsResponse>, S3Error> {
    let buckets_to_scan: Vec<String> = if let Some(ref bucket) = query.bucket {
        vec![bucket.clone()]
    } else {
        // Aggregate across all real buckets from storage
        state.engine.list_buckets().await.unwrap_or_default()
    };

    let mut total_objects: u64 = 0;
    let mut total_original_size: u64 = 0;
    let mut total_stored_size: u64 = 0;

    for bucket in &buckets_to_scan {
        let page = state
            .engine
            .list_objects_v2(bucket, "", u32::MAX, None)
            .await?;
        for (_key, meta) in &page.objects {
            total_objects += 1;
            total_original_size += meta.file_size;
            total_stored_size += meta.delta_size().unwrap_or(meta.file_size);
        }
    }

    let savings_percentage = if total_original_size > 0 {
        (1.0 - total_stored_size as f64 / total_original_size as f64) * 100.0
    } else {
        0.0
    };

    Ok(Json(StatsResponse {
        total_objects,
        total_original_size,
        total_stored_size,
        savings_percentage,
    }))
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
