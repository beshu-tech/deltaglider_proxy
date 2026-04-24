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
        // Bucket must exist before a multipart upload can be initiated.
        // Without this check, on the filesystem backend the later
        // `engine.store*` call (invoked by CompleteMultipartUpload) would
        // silently create the bucket via `ensure_dir` — see C2 security fix.
        // Initiate is the right place: catching it here fails fast, before
        // any UploadPart consumes memory.
        super::object_helpers::ensure_bucket_exists(&state, &bucket).await?;
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
        .create(bucket, key, content_type, user_metadata)?;

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
    state: &Arc<AppState>,
    bucket: &str,
    key: &str,
    upload_id: &str,
    body: Bytes,
) -> Result<Response, S3Error> {
    info!(
        "CompleteMultipartUpload {}/{} uploadId={}",
        bucket, key, upload_id
    );

    // Defence in depth: bucket may have been deleted between initiate and
    // complete. Without this check the subsequent `engine.store*` would
    // silently recreate the bucket directory on the filesystem backend
    // (C2 security fix).
    super::object_helpers::ensure_bucket_exists(state, bucket).await?;

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

    // Quota check before storing — estimate size from parts
    {
        let total_parts_size: u64 = requested_parts
            .iter()
            .filter_map(|(num, _)| state.multipart.get_part_size(upload_id, *num))
            .sum();
        super::object_helpers::check_quota(state, bucket, total_parts_size)?;
    }

    // Bifurcate: non-delta-eligible files use the chunked path to avoid
    // assembling all parts into a single contiguous buffer (~2x memory savings).
    //
    // C4 security fix: `complete()` / `complete_parts()` now flip the
    // upload to `Completing` atomically. We MUST call either
    // `finish_upload` (on success) or `rollback_upload` (on error) —
    // otherwise the upload stays stuck in `Completing` and rejects
    // both abort and further UploadPart calls until the sweeper GC's it.
    let engine = state.engine.load();
    let (multipart_etag, store_result) = if !engine.is_delta_eligible(key) {
        let completed = state
            .multipart
            .complete_parts(upload_id, bucket, key, &requested_parts)?;
        let etag = completed.etag.clone();
        match engine
            .store_passthrough_chunked(
                bucket,
                key,
                &completed.parts,
                completed.total_size,
                completed.content_type,
                completed.user_metadata,
            )
            .await
        {
            Ok(result) => (etag, result),
            Err(e) => {
                // Engine failure: return upload to Open so the client
                // can retry CompleteMultipartUpload without reuploading
                // parts. Matches S3's behaviour on InternalError during
                // complete.
                state.multipart.rollback_upload(upload_id);
                return Err(e.into());
            }
        }
    } else {
        let completed = state
            .multipart
            .complete(upload_id, bucket, key, &requested_parts)?;
        let etag = completed.etag.clone();
        match engine
            .store(
                bucket,
                key,
                &completed.data,
                completed.content_type,
                completed.user_metadata,
            )
            .await
        {
            Ok(result) => (etag, result),
            Err(e) => {
                state.multipart.rollback_upload(upload_id);
                return Err(e.into());
            }
        }
    };

    // Store succeeded — upload is terminal; remove from the map.
    state.multipart.finish_upload(upload_id);

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
