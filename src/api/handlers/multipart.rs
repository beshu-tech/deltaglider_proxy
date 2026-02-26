//! Multipart upload S3 handlers: CreateMultipartUpload, CompleteMultipartUpload.

use super::{
    body_to_utf8, extract_content_type, extract_user_metadata, xml_response, AppState, ObjectQuery,
    S3Error,
};
use crate::api::extractors::ValidatedPath;
use crate::api::xml::{
    CompleteMultipartUploadRequest, CompleteMultipartUploadResult, InitiateMultipartUploadResult,
};
use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use std::sync::Arc;
use tracing::{debug, info, instrument, warn};

/// POST object handler — dispatches multipart upload operations by query param.
#[instrument(skip(state, body))]
pub async fn post_object(
    State(state): State<Arc<AppState>>,
    ValidatedPath { bucket, key }: ValidatedPath,
    Query(query): Query<ObjectQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, S3Error> {
    if query.uploads.is_some() {
        initiate_multipart_upload(&state, &bucket, &key, &headers)
    } else if let Some(upload_id) = &query.upload_id {
        complete_multipart_upload(&state, &bucket, &key, upload_id, body).await
    } else {
        Err(S3Error::InvalidRequest(
            "POST on object requires ?uploads or ?uploadId parameter".to_string(),
        ))
    }
}

/// POST /{bucket}/{key}?uploads — CreateMultipartUpload
fn initiate_multipart_upload(
    state: &AppState,
    bucket: &str,
    key: &str,
    headers: &HeaderMap,
) -> Result<Response, S3Error> {
    info!("CreateMultipartUpload {}/{}", bucket, key);

    let content_type = extract_content_type(headers);
    let user_metadata = extract_user_metadata(headers);
    let upload_id = state
        .multipart
        .create(bucket, key, content_type, user_metadata);

    let xml = InitiateMultipartUploadResult {
        bucket: bucket.to_string(),
        key: key.to_string(),
        upload_id,
    }
    .to_xml();
    Ok(xml_response(xml))
}

/// POST /{bucket}/{key}?uploadId=X — CompleteMultipartUpload
async fn complete_multipart_upload(
    state: &AppState,
    bucket: &str,
    key: &str,
    upload_id: &str,
    body: Bytes,
) -> Result<Response, S3Error> {
    info!(
        "CompleteMultipartUpload {}/{} uploadId={}",
        bucket, key, upload_id
    );

    let body_str = body_to_utf8(&body)?;
    let complete_req = CompleteMultipartUploadRequest::from_xml(body_str).map_err(|e| {
        warn!("Failed to parse CompleteMultipartUpload XML: {}", e);
        S3Error::MalformedXML
    })?;

    let requested_parts: Vec<(u32, String)> = complete_req
        .parts
        .iter()
        .map(|p| (p.part_number, p.etag.clone()))
        .collect();

    // Bifurcate: non-delta-eligible files use the chunked path to avoid
    // assembling all parts into a single contiguous buffer (~2x memory savings).
    let engine = state.engine.load();
    let (multipart_etag, store_result) = if !engine.is_delta_eligible(key) {
        let completed = state
            .multipart
            .complete_parts(upload_id, bucket, key, &requested_parts)?;
        let etag = completed.etag.clone();
        let result = engine
            .store_passthrough_chunked(
                bucket,
                key,
                &completed.parts,
                completed.total_size,
                completed.content_type,
                completed.user_metadata,
            )
            .await?;
        (etag, result)
    } else {
        let completed = state
            .multipart
            .complete(upload_id, bucket, key, &requested_parts)?;
        let etag = completed.etag.clone();
        let result = engine
            .store(
                bucket,
                key,
                &completed.data,
                completed.content_type,
                completed.user_metadata,
            )
            .await?;
        (etag, result)
    };

    state.multipart.remove(upload_id);

    debug!(
        "CompleteMultipartUpload {}/{} stored as {}",
        bucket,
        key,
        store_result.metadata.storage_info.label(),
    );

    let xml = CompleteMultipartUploadResult {
        location: format!("/{}/{}", bucket, key),
        bucket: bucket.to_string(),
        key: key.to_string(),
        etag: multipart_etag,
    }
    .to_xml();
    Ok((
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
        .into_response())
}
