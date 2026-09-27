// SPDX-License-Identifier: BUSL-1.1

//! Multipart upload verbs: create, upload part, abort, list parts, list
//! uploads, and the detached CompleteMultipartUpload pipeline.

use super::*;

pub(super) async fn create_multipart_upload(
    svc: &DeltaGliderS3Service,
    req: s3s::S3Request<s3s::dto::CreateMultipartUploadInput>,
) -> s3s::S3Result<s3s::S3Response<s3s::dto::CreateMultipartUploadOutput>> {
    let input = req.input;
    // Earliest reject: parts are staged in-memory, but refusing at
    // create saves the client uploading them at all (403-before-404).
    crate::api::handlers::object_helpers::check_client_write_allowed(&svc.state, &input.bucket)?;
    check_user_metadata_size_s3s(input.metadata.as_ref())?;
    ensure_bucket_exists_s3s(&svc.state, &input.bucket).await?;
    let engine = svc.state.engine.load();
    let delta_limit = engine.tuning().mpu_delta_reconstruct_max_bytes;
    let user_metadata = input.metadata.unwrap_or_default();
    // A write that tries no delta is never assembled in memory: its parts go
    // to relay files from the first one. Completion decides again (the config
    // can change mid-upload) and reads relay files either way.
    let relay_now = !engine.write_tries_delta(&input.bucket, &input.key, &user_metadata);
    let upload_id = svc.state.multipart.create_with_relay_policy(
        &input.bucket,
        &input.key,
        input.content_type.clone(),
        user_metadata,
        Some(delta_limit),
        relay_now,
    )?;
    Ok(s3s::S3Response::new(
        s3s::dto::CreateMultipartUploadOutput {
            bucket: Some(input.bucket),
            key: Some(input.key),
            upload_id: Some(upload_id),
            ..Default::default()
        },
    ))
}

pub(super) async fn upload_part(
    svc: &DeltaGliderS3Service,
    req: s3s::S3Request<s3s::dto::UploadPartInput>,
) -> s3s::S3Result<s3s::S3Response<s3s::dto::UploadPartOutput>> {
    let chunked_headers = headers_if_still_aws_chunked(&req);
    let input = req.input;
    ensure_bucket_exists_s3s(&svc.state, &input.bucket).await?;
    let max_object_size = svc.state.engine.load().max_object_size();
    // Review C9: the store's per-upload cap follows the live config.
    svc.state.multipart.set_max_object_size(max_object_size);
    let data = collect_blob_limited(input.body, max_object_size, chunked_headers.as_ref()).await?;
    validate_content_md5_s3s(input.content_md5.as_deref(), &data)?;
    let etag = svc.state.multipart.upload_part(
        &input.upload_id,
        &input.bucket,
        &input.key,
        input.part_number as u32,
        data,
    )?;
    Ok(s3s::S3Response::new(s3s::dto::UploadPartOutput {
        e_tag: Some(parse_s3s_etag(&etag)?),
        ..Default::default()
    }))
}

pub(super) async fn abort_multipart_upload(
    svc: &DeltaGliderS3Service,
    req: s3s::S3Request<s3s::dto::AbortMultipartUploadInput>,
) -> s3s::S3Result<s3s::S3Response<s3s::dto::AbortMultipartUploadOutput>> {
    let input = req.input;
    svc.state
        .multipart
        .abort(&input.upload_id, &input.bucket, &input.key)?;
    Ok(s3s::S3Response::new(
        s3s::dto::AbortMultipartUploadOutput::default(),
    ))
}

pub(super) async fn list_parts(
    svc: &DeltaGliderS3Service,
    req: s3s::S3Request<s3s::dto::ListPartsInput>,
) -> s3s::S3Result<s3s::S3Response<s3s::dto::ListPartsOutput>> {
    let input = req.input;
    let max_parts = input.max_parts.unwrap_or(1000).clamp(1, 1000) as u32;
    let marker = input.part_number_marker.unwrap_or(0) as u32;
    let (parts, is_truncated, next_marker) = svc.state.multipart.list_parts_paginated(
        &input.upload_id,
        &input.bucket,
        &input.key,
        marker,
        max_parts,
    )?;
    let parts = parts
        .into_iter()
        .map(|p| s3s::dto::Part {
            part_number: Some(p.part_number as i32),
            e_tag: parse_s3s_etag(&p.etag).ok(),
            last_modified: Some(SystemTime::from(p.last_modified).into()),
            size: Some(i64::try_from(p.size).unwrap_or(i64::MAX)),
            ..Default::default()
        })
        .collect();
    Ok(s3s::S3Response::new(s3s::dto::ListPartsOutput {
        bucket: Some(input.bucket),
        key: Some(input.key),
        upload_id: Some(input.upload_id),
        max_parts: Some(max_parts as i32),
        part_number_marker: Some(marker as i32),
        next_part_number_marker: Some(next_marker as i32),
        is_truncated: Some(is_truncated),
        parts: Some(parts),
        ..Default::default()
    }))
}

pub(super) async fn complete_multipart_upload(
    svc: &DeltaGliderS3Service,
    req: s3s::S3Request<s3s::dto::CompleteMultipartUploadInput>,
) -> s3s::S3Result<s3s::S3Response<s3s::dto::CompleteMultipartUploadOutput>> {
    let input = req.input;
    // Defense-in-depth: the marker can be hot-applied mid-upload, so
    // Complete (the moment bytes reach the backend) re-checks.
    crate::api::handlers::object_helpers::check_client_write_allowed(&svc.state, &input.bucket)?;
    ensure_bucket_exists_s3s(&svc.state, &input.bucket).await?;
    let requested_parts = completed_parts_to_request(input.multipart_upload.as_ref())?;
    svc.state
        .multipart
        .set_max_object_size(svc.state.engine.load().max_object_size());

    // Completion registry: exactly one request runs the store pipeline, on a
    // DETACHED task (a client disconnect must not cancel a half-done store);
    // identical retries join the in-flight outcome or hit the tombstone.
    let begin = svc.state.multipart.begin_complete(
        &input.upload_id,
        &input.bucket,
        &input.key,
        &requested_parts,
    )?;
    let complete_response =
        |etag: &str,
         meta: Option<&crate::types::FileMetadata>|
         -> s3s::S3Result<s3s::S3Response<s3s::dto::CompleteMultipartUploadOutput>> {
            let location = format!("/{}/{}", input.bucket, input.key);
            let mut resp = s3s::S3Response::new(s3s::dto::CompleteMultipartUploadOutput {
                bucket: Some(input.bucket.clone()),
                key: Some(input.key.clone()),
                e_tag: Some(parse_s3s_etag(etag)?),
                location: Some(location),
                ..Default::default()
            });
            // Parity with the legacy axum handler: `x-amz-storage-type` (+
            // stored-size) so operators can observe how the multipart landed.
            if let Some(meta) = meta {
                add_storage_debug_headers(svc, &mut resp.headers, meta);
            }
            Ok(resp)
        };
    match begin {
        crate::multipart::BeginComplete::AlreadyDone { etag } => complete_response(&etag, None),
        crate::multipart::BeginComplete::Join(rx) => match await_completion_outcome(rx).await {
            Ok(etag) => complete_response(&etag, None),
            Err(failure) => Err(failure.to_s3s()),
        },
        crate::multipart::BeginComplete::Owner(publisher) => {
            // Admission (quota) + routing decisions run ONLY for the owner: a
            // tombstone/join retry must never be re-admitted — its bytes are
            // already committed, and the freeze/quota gates would wrongly
            // reject a completion that has in fact succeeded (review MAJOR 1).
            let total_parts_size: u64 = requested_parts
                .iter()
                .filter_map(|(num, _)| svc.state.multipart.get_part_size(&input.upload_id, *num))
                .sum();
            // The quota gate, then the same conditional write as PutObject
            // (If-None-Match: * is create-only), under the same per-key lock,
            // held until the store ends. Only the owner checks: a retry that
            // joins or hits the tombstone sees its OWN object and must not 412.
            let write_lock = crate::api::handlers::object_helpers::store_client_multipart_admit(
                &svc.state,
                &input.bucket,
                &input.key,
                total_parts_size,
                &crate::deltaglider::Precondition {
                    if_match: input.if_match.clone(),
                    if_none_match: input.if_none_match.clone(),
                },
            )
            .await?;
            let user_metadata = svc
                .state
                .multipart
                .user_metadata(&input.upload_id)
                .unwrap_or_default();
            let force_chunked_passthrough = completion_stores_from_parts(
                &svc.state.engine.load(),
                &input.bucket,
                &input.key,
                &user_metadata,
                total_parts_size,
            );
            let state = svc.state.clone();
            let (bucket, key, upload_id) = (
                input.bucket.clone(),
                input.key.clone(),
                input.upload_id.clone(),
            );
            let parts = requested_parts.clone();
            let handle = tokio::spawn(async move {
                let _write_lock = write_lock;
                let result = run_multipart_completion(
                    state,
                    bucket,
                    key,
                    upload_id,
                    parts,
                    force_chunked_passthrough,
                )
                .await
                .map_err(s3s::S3Error::from);
                match &result {
                    Ok((etag, _)) => publisher.publish(Ok(etag.clone())),
                    Err(e) => publisher.publish(Err(crate::multipart::CompletionFailure::of(e))),
                }
                result
            });
            match handle.await {
                Ok(Ok((etag, meta))) => complete_response(&etag, meta.as_ref()),
                Ok(Err(e)) => Err(e),
                Err(join_err) => Err(s3s::S3Error::from(
                    crate::api::errors::S3Error::InternalError(format!(
                        "completion task failed: {join_err}"
                    )),
                )),
            }
        }
    }
}

/// Whether CompleteMultipartUpload stores the parts as they are, never
/// assembled into one buffer: the write tries no delta, or it is too large
/// to rebuild for one.
pub(super) fn completion_stores_from_parts(
    engine: &crate::deltaglider::DynEngine,
    bucket: &str,
    key: &str,
    user_metadata: &std::collections::HashMap<String, String>,
    total_parts_size: u64,
) -> bool {
    !engine.write_tries_delta(bucket, key, user_metadata)
        || total_parts_size > engine.tuning().mpu_delta_reconstruct_max_bytes
}

pub(super) async fn list_multipart_uploads(
    svc: &DeltaGliderS3Service,
    req: s3s::S3Request<s3s::dto::ListMultipartUploadsInput>,
) -> s3s::S3Result<s3s::S3Response<s3s::dto::ListMultipartUploadsOutput>> {
    let input = req.input;
    ensure_bucket_exists_s3s(&svc.state, &input.bucket).await?;
    let max_uploads = input.max_uploads.unwrap_or(1000).clamp(1, 1000) as u32;
    let enc = ListKeyEncoding::of(input.encoding_type.as_ref());
    let (uploads, is_truncated, next_key, next_upload_id) =
        svc.state.multipart.list_uploads_paginated(
            Some(&input.bucket),
            input.prefix.as_deref(),
            input.key_marker.as_deref().unwrap_or(""),
            input.upload_id_marker.as_deref().unwrap_or(""),
            max_uploads,
        );
    let uploads = uploads
        .into_iter()
        .map(|u| s3s::dto::MultipartUpload {
            key: Some(enc.apply(u.key)),
            upload_id: Some(u.upload_id),
            initiated: Some(SystemTime::from(u.initiated).into()),
            ..Default::default()
        })
        .collect();
    Ok(s3s::S3Response::new(s3s::dto::ListMultipartUploadsOutput {
        bucket: Some(input.bucket),
        delimiter: enc.apply_opt(input.delimiter),
        encoding_type: input.encoding_type,
        is_truncated: Some(is_truncated),
        key_marker: enc.apply_opt(input.key_marker),
        max_uploads: Some(max_uploads as i32),
        next_key_marker: enc.apply_opt((!next_key.is_empty()).then_some(next_key)),
        next_upload_id_marker: (!next_upload_id.is_empty()).then_some(next_upload_id),
        prefix: enc.apply_opt(input.prefix),
        upload_id_marker: input.upload_id_marker,
        uploads: Some(uploads),
        ..Default::default()
    }))
}

/// Await a joined completion's outcome (initial watch value is None).
pub(super) async fn await_completion_outcome(
    mut rx: tokio::sync::watch::Receiver<Option<crate::multipart::CompletionResult>>,
) -> crate::multipart::CompletionResult {
    loop {
        if let Some(result) = rx.borrow().clone() {
            return result;
        }
        if rx.changed().await.is_err() {
            return Err(crate::multipart::CompletionFailure::internal(
                "completion task dropped before publishing",
            ));
        }
    }
}

/// The CompleteMultipartUpload store pipeline. Runs on a DETACHED tokio task so a
/// client disconnect cannot cancel a half-done store (which used to roll back the
/// whole upload and poison the SDK's retry). Owns finish/rollback + event emission.
pub(super) async fn run_multipart_completion(
    state: Arc<AppState>,
    bucket: String,
    key: String,
    upload_id: String,
    requested_parts: Vec<(u32, String)>,
    force_chunked_passthrough: bool,
) -> Result<(String, Option<FileMetadata>), crate::api::S3Error> {
    // Deterministic chaos hook for tests: hold the store window open.
    let stall_ms = crate::config::test_seams::test_seams().complete_stall_ms;
    if stall_ms > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(stall_ms)).await;
    }
    let engine = state.engine.load();
    let (etag, result) = if force_chunked_passthrough {
        let completed =
            state
                .multipart
                .complete_passthrough(&upload_id, &bucket, &key, &requested_parts)?;
        let etag = completed.etag.clone();
        let store_result = match completed.payload {
            crate::multipart::PassthroughPayload::Chunks(parts) => {
                engine
                    .store_passthrough_chunked_with_multipart_etag(
                        &bucket,
                        &key,
                        &parts,
                        completed.total_size,
                        completed.content_type,
                        completed.user_metadata,
                        etag.clone(),
                    )
                    .await
            }
            crate::multipart::PassthroughPayload::RelayedParts(paths) => {
                // Protect the relay part files from the idle-TTL sweeper while the
                // store reads them (H18) — a slow remote store can exceed
                // completing_timeout.
                let _store_guard = state.multipart.store_guard(&upload_id);
                engine
                    .store_passthrough_relayed_parts_with_multipart_etag(
                        &bucket,
                        &key,
                        &paths,
                        completed.total_size,
                        completed.content_type,
                        completed.user_metadata,
                        etag.clone(),
                    )
                    .await
            }
        };
        match store_result {
            Ok(result) => (etag, result),
            Err(e) => {
                state.multipart.rollback_upload(&upload_id);
                return Err(e.into());
            }
        }
    } else {
        let completed = state
            .multipart
            .complete(&upload_id, &bucket, &key, &requested_parts)?;
        let etag = completed.etag.clone();
        match engine
            .store_with_multipart_etag(
                &bucket,
                &key,
                &completed.data,
                completed.content_type,
                completed.user_metadata,
                etag.clone(),
            )
            .await
        {
            Ok(result) => (etag, result),
            Err(e) => {
                state.multipart.rollback_upload(&upload_id);
                return Err(e.into());
            }
        }
    };
    state.multipart.finish_upload(&upload_id);
    crate::api::handlers::object_helpers::store_client_multipart_commit(
        &state, &bucket, &key, &result,
    )
    .await;
    Ok((etag, Some(result.metadata)))
}

pub(super) fn completed_parts_to_request(
    upload: Option<&s3s::dto::CompletedMultipartUpload>,
) -> s3s::S3Result<Vec<(u32, String)>> {
    let parts = upload
        .and_then(|u| u.parts.as_ref())
        .ok_or_else(|| s3s::s3_error!(InvalidPart, "missing multipart parts"))?;
    parts
        .iter()
        .map(|p| {
            let part_number = p
                .part_number
                .ok_or_else(|| s3s::s3_error!(InvalidPart, "missing part number"))?;
            let etag = p
                .e_tag
                .as_ref()
                .ok_or_else(|| s3s::s3_error!(InvalidPart, "missing part ETag"))?
                .to_http_header()
                .map_err(|_| s3s::s3_error!(InvalidPart, "invalid part ETag"))?
                .to_str()
                .map_err(|_| s3s::s3_error!(InvalidPart, "invalid part ETag"))?
                .to_string();
            Ok((part_number as u32, etag))
        })
        .collect()
}
