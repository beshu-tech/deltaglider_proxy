// SPDX-License-Identifier: BUSL-1.1

//! Object verbs: HEAD/GET/PUT/DELETE of one object, DeleteObjects, and the
//! object ACL and tagging stubs.

use super::*;

pub(super) async fn head_object(
    svc: &DeltaGliderS3Service,
    req: s3s::S3Request<s3s::dto::HeadObjectInput>,
) -> s3s::S3Result<s3s::S3Response<s3s::dto::HeadObjectOutput>> {
    let reader = Reader::for_object(&req.extensions, &req.input.bucket, &req.input.key);
    let if_range = req.headers.get(axum::http::header::IF_RANGE).cloned();
    let input = req.input;
    let engine = svc.state.engine.load();
    let meta = match engine.head(&input.bucket, &input.key).await {
        Ok(meta) => meta,
        Err(e) => {
            return Err(
                no_such_key_or_bucket(&svc.state, &input.bucket, s3s::S3Error::from(e)).await,
            )
        }
    };
    evaluate_read_conditionals_s3s(
        &meta,
        input.if_match.as_ref(),
        input.if_none_match.as_ref(),
        input.if_modified_since.as_ref(),
        input.if_unmodified_since.as_ref(),
    )?;

    let mut output = head_object_output_from_metadata(&meta, reader)?;
    if let Some(checked) = served_range(input.range.as_ref(), if_range.as_ref(), &meta)? {
        let range_len = checked.end.saturating_sub(checked.start);
        output.content_length = Some(i64::try_from(range_len).unwrap_or(i64::MAX));
        output.content_range = Some(format!(
            "bytes {}-{}/{}",
            checked.start,
            checked.end.saturating_sub(1),
            meta.file_size
        ));
    }

    // No `resp.status` here: s3s applies it only to custom routes. The
    // router turns a HEAD with `Content-Range` into 206
    // (`s3_router::head_range_is_partial`).
    let mut resp = s3s::S3Response::new(output);
    add_storage_debug_headers(svc, &mut resp.headers, &meta, reader);
    Ok(resp)
}

pub(super) async fn get_object(
    svc: &DeltaGliderS3Service,
    req: s3s::S3Request<s3s::dto::GetObjectInput>,
) -> s3s::S3Result<s3s::S3Response<s3s::dto::GetObjectOutput>> {
    let reader = Reader::for_object(&req.extensions, &req.input.bucket, &req.input.key);
    let anonymous_principal =
        crate::iam::request_principal(&req.extensions).is_some_and(|u| u.is_anonymous());
    let if_range = req.headers.get(axum::http::header::IF_RANGE).cloned();
    let input = req.input;
    check_response_overrides_allowed(&input, anonymous_principal)?;
    let engine = svc.state.engine.load();
    // A plain GET (no range, no conditional, no partNumber) reads the
    // metadata and the bytes from one response (`retrieve_stream_fresh`):
    // one round trip, and the ETag and the body describe one version.
    let plain = input.range.is_none()
        && input.part_number.is_none()
        && input.if_match.is_none()
        && input.if_none_match.is_none()
        && input.if_modified_since.is_none()
        && input.if_unmodified_since.is_none();
    if plain {
        let response = match engine
            .retrieve_stream_fresh(&input.bucket, &input.key)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                return Err(
                    no_such_key_or_bucket(&svc.state, &input.bucket, s3s::S3Error::from(e)).await,
                )
            }
        };
        return finish_get(svc, &input, reader, response);
    }
    // A ranged GET with no conditional, no If-Range and no partNumber: the
    // ranged read and the `.delta` HEAD in parallel; the backend answers a
    // 416 itself. `None`: the HEAD-first path below answers.
    let conditional_free = input.part_number.is_none()
        && if_range.is_none()
        && input.if_match.is_none()
        && input.if_none_match.is_none()
        && input.if_modified_since.is_none()
        && input.if_unmodified_since.is_none();
    if let (true, Some(s3s::dto::Range::Int { first, last })) = (conditional_free, &input.range) {
        if last.is_none_or(|last| last >= *first) {
            let end = last.unwrap_or(u64::MAX - 1);
            let ranged = match engine
                .retrieve_range_fresh(&input.bucket, &input.key, *first, end)
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    return Err(no_such_key_or_bucket(
                        &svc.state,
                        &input.bucket,
                        s3s::S3Error::from(e),
                    )
                    .await)
                }
            };
            if let Some((stream, content_length, metadata)) = ranged {
                let last_byte = first + content_length.saturating_sub(1);
                let content_range = format!("bytes {first}-{last_byte}/{}", metadata.file_size);
                return ranged_response(
                    svc,
                    &input,
                    reader,
                    stream,
                    content_length,
                    content_range,
                    &metadata,
                );
            }
        }
    }
    // A conditional or ranged GET reads fresh metadata first, so a
    // 304/412/416 never opens a body. The HEAD also refreshes the cache,
    // so the full read below opens the form it names with one GET.
    let head = match engine.head(&input.bucket, &input.key).await {
        Ok(head) => head,
        Err(e) => {
            return Err(
                no_such_key_or_bucket(&svc.state, &input.bucket, s3s::S3Error::from(e)).await,
            )
        }
    };
    evaluate_read_conditionals_s3s(
        &head,
        input.if_match.as_ref(),
        input.if_none_match.as_ref(),
        input.if_modified_since.as_ref(),
        input.if_unmodified_since.as_ref(),
    )?;
    let checked = served_range(input.range.as_ref(), if_range.as_ref(), &head)?
        .map(|checked| (checked, head.file_size));

    if let Some((checked, file_size)) = checked {
        let start = checked.start;
        let end_inclusive = checked.end.saturating_sub(1);
        let content_range = format!("bytes {start}-{end_inclusive}/{file_size}");

        if let Some((stream, content_length, metadata)) = engine
            .retrieve_stream_range(&input.bucket, &input.key, start, end_inclusive, None)
            .await?
        {
            served_the_judged_version(&head, &metadata, &input, true)?;
            return ranged_response(
                svc,
                &input,
                reader,
                stream,
                content_length,
                content_range,
                &metadata,
            );
        }

        let (data, metadata) = engine.retrieve(&input.bucket, &input.key).await?;
        served_the_judged_version(&head, &metadata, &input, true)?;
        // `checked` was validated against the HEAD `file_size`, but we
        // slice into the freshly-reconstructed `data`. If stored
        // `file_size` metadata is stale / larger than the actual bytes
        // (delta reconstruction yielding fewer bytes, inconsistent
        // metadata) the requested end can exceed `data.len()`. Use
        // `.get()` so a malformed/stale range returns `InvalidRange`
        // (400) instead of panicking the worker on an out-of-bounds slice.
        let slice_start = usize::try_from(start).unwrap_or(usize::MAX);
        let slice_end = usize::try_from(checked.end).unwrap_or(usize::MAX);
        let sliced = bytes::Bytes::copy_from_slice(
            data.get(slice_start..slice_end)
                .ok_or_else(|| s3s::s3_error!(InvalidRange))?,
        );
        let body = s3s::dto::StreamingBlob::from(s3s::Body::from(sliced));
        let mut output = get_object_output_from_metadata(&metadata, body, reader)?;
        let range_len = checked.end.saturating_sub(checked.start);
        output.content_length = Some(i64::try_from(range_len).unwrap_or(i64::MAX));
        output.content_range = Some(content_range);
        apply_get_response_overrides(&input, &mut output);
        let mut resp = s3s::S3Response::new(output);
        add_storage_debug_headers(svc, &mut resp.headers, &metadata, reader);
        add_get_object_security_headers(&mut resp);
        return Ok(resp);
    }

    let response = match engine
        .retrieve_stream_fresh(&input.bucket, &input.key)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            return Err(
                no_such_key_or_bucket(&svc.state, &input.bucket, s3s::S3Error::from(e)).await,
            )
        }
    };
    let served = match &response {
        RetrieveResponse::Streamed { metadata, .. }
        | RetrieveResponse::Buffered { metadata, .. } => metadata,
    };
    served_the_judged_version(&head, served, &input, false)?;
    finish_get(svc, &input, reader, response)
}

/// Pure: do two reads describe the same version, as a client sees it
/// (ETag, size, Last-Modified)?
fn same_client_version(a: &FileMetadata, b: &FileMetadata) -> bool {
    a.etag() == b.etag() && a.file_size == b.file_size && a.created_at == b.created_at
}

/// The HEAD-first GET judged its conditionals (and its range) on the
/// HEAD's metadata `judged`; `served` is the metadata of the response that
/// carries the bytes. When a write replaced the object between the two,
/// the conditionals are judged again on the served version (a 412 or 304
/// from it), and a ranged answer, whose range and If-Range were resolved
/// against the HEAD's version, is a retryable 503 SlowDown: the response
/// never joins one version's bytes to another version's ETag or size.
fn served_the_judged_version(
    judged: &FileMetadata,
    served: &FileMetadata,
    input: &s3s::dto::GetObjectInput,
    ranged: bool,
) -> s3s::S3Result<()> {
    if same_client_version(judged, served) {
        return Ok(());
    }
    evaluate_read_conditionals_s3s(
        served,
        input.if_match.as_ref(),
        input.if_none_match.as_ref(),
        input.if_modified_since.as_ref(),
        input.if_unmodified_since.as_ref(),
    )?;
    if ranged {
        return Err(s3s::s3_error!(
            SlowDown,
            "The object changed while it was read; retry the request"
        ));
    }
    Ok(())
}

/// The 206 response of a ranged GET served from a backend range stream.
fn ranged_response(
    svc: &DeltaGliderS3Service,
    input: &s3s::dto::GetObjectInput,
    reader: Reader,
    stream: BoxStream<'static, Result<bytes::Bytes, StorageError>>,
    content_length: u64,
    content_range: String,
    metadata: &FileMetadata,
) -> s3s::S3Result<s3s::S3Response<s3s::dto::GetObjectOutput>> {
    let body = s3s::dto::StreamingBlob::new(SyncStorageStream::new(stream));
    let mut output = get_object_output_from_metadata(metadata, body, reader)?;
    output.content_length = Some(i64::try_from(content_length).unwrap_or(i64::MAX));
    output.content_range = Some(content_range);
    apply_get_response_overrides(input, &mut output);
    // s3s answers 206 because `content_range` is set.
    let mut resp = s3s::S3Response::new(output);
    add_storage_debug_headers(svc, &mut resp.headers, metadata, reader);
    add_get_object_security_headers(&mut resp);
    Ok(resp)
}

/// The response of a full (not ranged) GET.
fn finish_get(
    svc: &DeltaGliderS3Service,
    input: &s3s::dto::GetObjectInput,
    reader: Reader,
    response: RetrieveResponse,
) -> s3s::S3Result<s3s::S3Response<s3s::dto::GetObjectOutput>> {
    let (body, metadata) = match response {
        RetrieveResponse::Streamed {
            stream, metadata, ..
        } => {
            let blob = s3s::dto::StreamingBlob::new(SyncStorageStream::new(stream));
            (blob, metadata)
        }
        RetrieveResponse::Buffered { data, metadata, .. } => {
            let blob = s3s::dto::StreamingBlob::from(s3s::Body::from(bytes::Bytes::from(data)));
            (blob, metadata)
        }
    };
    let mut output = get_object_output_from_metadata(&metadata, body, reader)?;
    apply_get_response_overrides(input, &mut output);
    let mut resp = s3s::S3Response::new(output);
    add_storage_debug_headers(svc, &mut resp.headers, &metadata, reader);
    add_get_object_security_headers(&mut resp);
    Ok(resp)
}

pub(super) async fn get_object_acl(
    svc: &DeltaGliderS3Service,
    req: s3s::S3Request<s3s::dto::GetObjectAclInput>,
) -> s3s::S3Result<s3s::S3Response<s3s::dto::GetObjectAclOutput>> {
    let input = req.input;
    svc.state
        .engine
        .load()
        .head(&input.bucket, &input.key)
        .await?;
    Ok(s3s::S3Response::new(s3s::dto::GetObjectAclOutput {
        owner: Some(default_acl_owner()),
        grants: Some(vec![default_full_control_grant()]),
        ..Default::default()
    }))
}

pub(super) async fn put_object_acl(
    svc: &DeltaGliderS3Service,
    req: s3s::S3Request<s3s::dto::PutObjectAclInput>,
) -> s3s::S3Result<s3s::S3Response<s3s::dto::PutObjectAclOutput>> {
    let input = req.input;
    svc.state
        .engine
        .load()
        .head(&input.bucket, &input.key)
        .await?;
    Err(s3s::s3_error!(
        NotImplemented,
        "Object ACL mutation is not supported by this proxy"
    ))
}

pub(super) async fn get_object_tagging(
    svc: &DeltaGliderS3Service,
    req: s3s::S3Request<s3s::dto::GetObjectTaggingInput>,
) -> s3s::S3Result<s3s::S3Response<s3s::dto::GetObjectTaggingOutput>> {
    let input = req.input;
    svc.state
        .engine
        .load()
        .head(&input.bucket, &input.key)
        .await?;
    Err(s3s::s3_error!(
        NotImplemented,
        "Object tagging is not supported by this proxy"
    ))
}

pub(super) async fn put_object_tagging(
    svc: &DeltaGliderS3Service,
    req: s3s::S3Request<s3s::dto::PutObjectTaggingInput>,
) -> s3s::S3Result<s3s::S3Response<s3s::dto::PutObjectTaggingOutput>> {
    let input = req.input;
    svc.state
        .engine
        .load()
        .head(&input.bucket, &input.key)
        .await?;
    Err(s3s::s3_error!(
        NotImplemented,
        "Object tagging is not supported by this proxy"
    ))
}

pub(super) async fn delete_object_tagging(
    svc: &DeltaGliderS3Service,
    req: s3s::S3Request<s3s::dto::DeleteObjectTaggingInput>,
) -> s3s::S3Result<s3s::S3Response<s3s::dto::DeleteObjectTaggingOutput>> {
    let input = req.input;
    svc.state
        .engine
        .load()
        .head(&input.bucket, &input.key)
        .await?;
    Err(s3s::s3_error!(
        NotImplemented,
        "Object tagging is not supported by this proxy"
    ))
}

pub(super) async fn delete_object(
    svc: &DeltaGliderS3Service,
    req: s3s::S3Request<s3s::dto::DeleteObjectInput>,
) -> s3s::S3Result<s3s::S3Response<s3s::dto::DeleteObjectOutput>> {
    let input = req.input;
    crate::api::handlers::object_helpers::check_client_write_allowed(&svc.state, &input.bucket)?;
    // `DELETE photos/` deletes the folder marker `photos/` only, as on S3
    // (review D3). Folder deletes list and batch-delete the keys.
    match svc
        .state
        .engine
        .load()
        .delete(&input.bucket, &input.key)
        .await
    {
        // Only a REAL delete (Ok) emits an event — a NotFound deleted
        // nothing, so there's nothing for replication to mirror.
        Ok(_) => {
            svc.emit_object_event(
                crate::event_outbox::EventKind::ObjectDeleted,
                &input.bucket,
                &input.key,
                serde_json::json!({}),
            )
            .await;
            Ok(s3s::S3Response::new(s3s::dto::DeleteObjectOutput::default()))
        }
        Err(crate::deltaglider::EngineError::NotFound(_)) => {
            crate::api::handlers::ensure_bucket_exists(&svc.state, &input.bucket).await?;
            Ok(s3s::S3Response::new(s3s::dto::DeleteObjectOutput::default()))
        }
        Err(e) => Err(s3s::S3Error::from(e)),
    }
}

pub(super) async fn delete_objects(
    svc: &DeltaGliderS3Service,
    req: s3s::S3Request<s3s::dto::DeleteObjectsInput>,
) -> s3s::S3Result<s3s::S3Response<s3s::dto::DeleteObjectsOutput>> {
    use crate::deltaglider::{DeleteHooks, DeleteItem, DeleteOutcome};
    let headers = req.headers.clone();
    let input = req.input;
    crate::api::handlers::object_helpers::check_client_write_allowed(&svc.state, &input.bucket)?;
    validate_delete_objects_count(input.delete.objects.len())?;
    // Per-key misses are successes, so a missing bucket is asked here.
    crate::api::handlers::ensure_bucket_exists(&svc.state, &input.bucket).await?;
    let quiet = input.delete.quiet.unwrap_or(false);
    // Per-key IAM authorization. The middleware only authorized the
    // bucket-level POST ?delete; each key in the batch body must be
    // checked individually, or a `Deny delete bucket/protected/*`
    // carve-out (or a prefix-scoped Allow) is silently ignored and
    // protected objects get batch-deleted. (X-ray H6/H26.)
    let auth_user = crate::iam::request_principal(&req.extensions).cloned();
    let policy_context = request_policy_context(&req.extensions);
    let objects = input.delete.objects;
    let mut allowed = vec![true; objects.len()];
    let mut keys = Vec::with_capacity(objects.len());
    for (i, obj) in objects.iter().enumerate() {
        let key = obj.key.trim_start_matches('/').to_string();
        if let Some(user) = auth_user.as_ref() {
            if !user.can_with_context(S3Action::Delete, &input.bucket, &key, &policy_context) {
                crate::audit::audit_log(
                    "access_denied",
                    &user.name,
                    "DeleteObjects",
                    &headers,
                    &input.bucket,
                    &key,
                );
                allowed[i] = false;
                continue;
            }
        }
        keys.push(key);
    }
    // The batch runs in a spawned task that this request awaits: a client
    // disconnect or the request timeout drops this future, not the task.
    // So every key that the batch deletes gets its ObjectDeleted event (one
    // per key as it ends, never only after the loop), and each folder gets
    // its reference reclaim. The write guard keeps a maintenance job's
    // drain waiting for the task, not just for this request.
    let state = svc.state.clone();
    let bucket = input.bucket.clone();
    let write = state.maintenance_gate.begin_write(&bucket);
    let batch = tokio::spawn(async move {
        let _write = write;
        let engine = state.engine.load_full();
        let on_outcome =
            |key: &str, outcome: &DeleteOutcome| -> futures::future::BoxFuture<'static, ()> {
                if !matches!(outcome, DeleteOutcome::Deleted(_)) {
                    return Box::pin(async {});
                }
                let event = crate::event_outbox::NewEvent::new(
                    crate::event_outbox::EventKind::ObjectDeleted,
                    bucket.as_str(),
                    key,
                    crate::event_outbox::EventSource::S3Api,
                    crate::replication::current_unix_seconds(),
                    serde_json::json!({}),
                );
                let state = state.clone();
                Box::pin(async move {
                    crate::api::handlers::object_helpers::enqueue_object_event(&state, event).await
                })
            };
        engine
            .delete_batch(
                &bucket,
                keys.into_iter().map(DeleteItem::new).collect(),
                DeleteHooks {
                    on_outcome: Some(&on_outcome),
                    ..Default::default()
                },
            )
            .await
    });
    let mut outcomes = batch
        .await
        .map_err(|e| {
            s3s::S3Error::from(crate::api::S3Error::InternalError(format!(
                "DeleteObjects task failed: {e}"
            )))
        })?
        .into_iter();
    let mut deleted = Vec::new();
    let mut errors = Vec::new();
    for (obj, allowed) in objects.into_iter().zip(allowed) {
        let s3_err: crate::api::S3Error = if !allowed {
            crate::api::S3Error::AccessDenied
        } else {
            match outcomes.next() {
                // A key that is already gone is deleted, as on S3.
                Some(DeleteOutcome::Deleted(_) | DeleteOutcome::NotFound) => {
                    if !quiet {
                        deleted.push(s3s::dto::DeletedObject {
                            key: Some(obj.key),
                            version_id: obj.version_id,
                            ..Default::default()
                        });
                    }
                    continue;
                }
                Some(DeleteOutcome::Failed(e)) => e.into(),
                // No condition and no stop hook: neither happens.
                Some(DeleteOutcome::Changed | DeleteOutcome::Skipped) | None => {
                    crate::api::S3Error::InternalError("the key was not deleted".to_string())
                }
            }
        };
        errors.push(s3s::dto::Error {
            key: Some(obj.key),
            version_id: obj.version_id,
            code: Some(s3_err.code().to_string()),
            message: Some(s3_err.to_string()),
        });
    }
    Ok(s3s::S3Response::new(s3s::dto::DeleteObjectsOutput {
        deleted: (!deleted.is_empty()).then_some(deleted),
        errors: (!errors.is_empty()).then_some(errors),
        ..Default::default()
    }))
}

pub(super) async fn put_object(
    svc: &DeltaGliderS3Service,
    req: s3s::S3Request<s3s::dto::PutObjectInput>,
) -> s3s::S3Result<s3s::S3Response<s3s::dto::PutObjectOutput>> {
    let body_ctx = BodyContext::of(&req);
    let s3s_verified_sha256 =
        s3s_verified_payload_sha256(&req.headers, &req.uri, req.credentials.is_some());
    let signed_payload_hash = req
        .extensions
        .get::<crate::api::auth::SignedPayloadHash>()
        .cloned();
    let writer = Reader::for_object(&req.extensions, &req.input.bucket, &req.input.key);
    let input = req.input;
    let engine = svc.state.engine.load();
    // Gate BEFORE the bucket check: a marked bucket refuses client bytes
    // regardless of backend reachability (403-before-404, no backend I/O
    // for a doomed request).
    crate::api::handlers::object_helpers::check_client_write_allowed(&svc.state, &input.bucket)?;
    check_user_metadata_size_s3s(input.metadata.as_ref())?;
    // One HeadBucket per bucket per 5 seconds, not one per PUT.
    crate::api::handlers::ensure_bucket_exists(&svc.state, &input.bucket).await?;

    let data = collect_blob_limited(input.body, engine.max_object_size(), &body_ctx).await?;
    // s3s verified the signed payload hash while the body streamed in: its
    // digest is the body's SHA-256. Otherwise a verifiable signed hash is
    // checked here, and an unsigned shape leaves the hashing to the engine.
    let known_sha256 = match s3s_verified_sha256 {
        Some(verified) => Some(verified),
        None => verify_signed_payload_hash_s3s(signed_payload_hash.as_ref(), &data).await?,
    };
    validate_content_md5_s3s(input.content_md5.as_deref(), &data)?;
    // The body is already collected here (SigV4 payload-hash verification
    // needs it); the engine spools a large delta-eligible body.
    let result = crate::api::handlers::object_helpers::store_client_write_hashed(
        &svc.state,
        crate::api::handlers::object_helpers::ClientWrite {
            bucket: &input.bucket,
            key: &input.key,
            data: data.clone(),
            content_type: input.content_type,
            user_metadata: input.metadata.unwrap_or_default(),
            precondition: &crate::deltaglider::Precondition {
                if_match: input.if_match,
                if_none_match: input.if_none_match,
            },
        },
        known_sha256,
    )
    .await?;
    let mut resp = s3s::S3Response::new(s3s::dto::PutObjectOutput {
        e_tag: Some(parse_s3s_etag(&result.metadata.etag())?),
        ..Default::default()
    });
    add_write_fact_headers(&mut resp.headers, &result, writer);
    Ok(resp)
}

/// The byte range a GET or HEAD serves; `None` serves the whole object.
///
/// - s3s's `Range::check` accepts a suffix range on an empty object as `0..0`.
///   S3 answers 416, and serving it sent `Content-Length: 1` with no body
///   (s3surface-3). An empty range is `InvalidRange`.
/// - s3s does not model `If-Range`. When it does not match the object, the
///   client holds bytes of another version: a partial answer would splice new
///   bytes onto old ones, so the whole object is served (RFC 9110 §13.1.5,
///   s3surface-4). An entity tag matches only by strong comparison; a date
///   matches only when it equals `Last-Modified`.
pub(super) fn served_range(
    range: Option<&s3s::dto::Range>,
    if_range: Option<&axum::http::HeaderValue>,
    meta: &FileMetadata,
) -> s3s::S3Result<Option<std::ops::Range<u64>>> {
    let Some(range) = range else {
        return Ok(None);
    };
    if let Some(validator) = if_range {
        if !if_range_matches(validator, meta)? {
            return Ok(None);
        }
    }
    match range.check(meta.file_size) {
        Ok(checked) if !checked.is_empty() => Ok(Some(checked)),
        _ => Err(invalid_range(meta.file_size)),
    }
}

/// A 416 that names the object length in `Content-Range: bytes */<len>`
/// (RFC 9110 §15.5.17), so a client can retry with a range that fits.
pub(super) fn invalid_range(file_size: u64) -> s3s::S3Error {
    let mut headers = axum::http::HeaderMap::new();
    if let Ok(value) = axum::http::HeaderValue::from_str(&format!("bytes */{file_size}")) {
        headers.insert(axum::http::header::CONTENT_RANGE, value);
    }
    super::error_with_headers(s3s::s3_error!(InvalidRange), headers)
}

/// Whether an `If-Range` validator names this object version. A value that
/// is neither an entity tag nor an HTTP date matches nothing.
pub(super) fn if_range_matches(
    validator: &axum::http::HeaderValue,
    meta: &FileMetadata,
) -> s3s::S3Result<bool> {
    let raw = validator.as_bytes().trim_ascii();
    if raw.starts_with(b"\"") || raw.starts_with(b"W/") {
        let current = parse_s3s_etag(&meta.etag())?;
        return Ok(s3s::dto::ETag::parse_http_header(raw)
            .is_ok_and(|candidate| candidate.strong_cmp(&current)));
    }
    let Ok(text) = std::str::from_utf8(raw) else {
        return Ok(false);
    };
    Ok(
        s3s::dto::Timestamp::parse(s3s::dto::TimestampFormat::HttpDate, text)
            .is_ok_and(|date| date == http_last_modified(meta)),
    )
}

/// A 304 with the validators S3 sends (`ETag`, `Last-Modified`; RFC 9110
/// §15.4.5). s3s still serializes an XML error body for it; the S3
/// response layer (`api::s3_router::strip_forbidden_body`) removes that
/// body and its `Content-Length`, because a 304 must not have content.
pub(super) fn not_modified(
    etag: &s3s::dto::ETag,
    last_modified: &s3s::dto::Timestamp,
) -> s3s::S3Error {
    let mut headers = axum::http::HeaderMap::new();
    if let Ok(value) = etag.to_http_header() {
        headers.insert(axum::http::header::ETAG, value);
    }
    let mut date = Vec::new();
    if last_modified
        .format(s3s::dto::TimestampFormat::HttpDate, &mut date)
        .is_ok()
    {
        if let Ok(value) = axum::http::HeaderValue::from_bytes(&date) {
            headers.insert(axum::http::header::LAST_MODIFIED, value);
        }
    }
    super::error_with_headers(s3s::s3_error!(NotModified), headers)
}

/// `Last-Modified` at the precision the wire carries: whole seconds. An HTTP
/// date has no sub-second part, so a date conditional that compared the
/// nanosecond `created_at` failed for a client that sent back the exact
/// `Last-Modified` it received (s3surface-1). Every date check uses this.
pub(super) fn http_last_modified(meta: &FileMetadata) -> s3s::dto::Timestamp {
    let whole =
        chrono::DateTime::from_timestamp(meta.created_at.timestamp(), 0).unwrap_or(meta.created_at);
    SystemTime::from(whole).into()
}

pub(super) fn evaluate_read_conditionals_s3s(
    meta: &FileMetadata,
    if_match: Option<&s3s::dto::ETagCondition>,
    if_none_match: Option<&s3s::dto::ETagCondition>,
    if_modified_since: Option<&s3s::dto::Timestamp>,
    if_unmodified_since: Option<&s3s::dto::Timestamp>,
) -> s3s::S3Result<()> {
    let current = parse_s3s_etag(&meta.etag())?;
    let last_modified = http_last_modified(meta);

    if let Some(cond) = if_match {
        let matches = cond.is_any()
            || cond
                .as_etag()
                .is_some_and(|candidate| candidate.strong_cmp(&current));
        if !matches {
            return Err(s3s::s3_error!(PreconditionFailed));
        }
        // AWS/S3: a passing If-Match wins over If-Unmodified-Since.
    } else if let Some(since) = if_unmodified_since {
        if last_modified > *since {
            return Err(s3s::s3_error!(PreconditionFailed));
        }
    }

    if let Some(cond) = if_none_match {
        let matches = cond.is_any()
            || cond
                .as_etag()
                .is_some_and(|candidate| candidate.weak_cmp(&current));
        if matches {
            return Err(not_modified(&current, &last_modified));
        }
        // AWS/S3: a passing If-None-Match wins over If-Modified-Since.
    } else if let Some(since) = if_modified_since {
        if last_modified <= *since {
            return Err(not_modified(&current, &last_modified));
        }
    }

    Ok(())
}

/// Object bytes are uploader-controlled and share the admin UI's origin.
/// An uploaded `text/html` or SVG opened in a browser ran script on that
/// origin: stored XSS against every admin who clicks it (review S5).
/// `sandbox` gives such a document an opaque origin and no script. Media and
/// PDF keep working: browsers render them without the page's origin.
/// `nosniff` is set globally for every response (main.rs).
pub(super) fn add_get_object_security_headers(
    resp: &mut s3s::S3Response<s3s::dto::GetObjectOutput>,
) {
    let content_type = resp.output.content_type.as_deref().unwrap_or("");
    if content_type_needs_sandbox(content_type) {
        resp.headers.insert(
            axum::http::header::CONTENT_SECURITY_POLICY,
            axum::http::HeaderValue::from_static("sandbox"),
        );
    }
}

/// Pure: may a browser treat this content type as an active document?
/// Unknown types count as active: only inert media and PDF are exempt.
/// A value with a `,` is a list: browsers use the LAST valid type in it
/// (Fetch "extract a MIME type"), so it is never inert. The essence must
/// be one well-formed `type/subtype` token, else it counts as active.
pub(super) fn content_type_needs_sandbox(content_type: &str) -> bool {
    if content_type.contains(',') {
        return true;
    }
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let well_formed = essence.split_once('/').is_some_and(|(t, sub)| {
        let token = |s: &str| {
            !s.is_empty()
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"!#$&-^_.+".contains(&b))
        };
        token(t) && token(sub)
    });
    if !well_formed {
        return true;
    }
    let inert = (essence.starts_with("image/") && !essence.contains("svg"))
        || essence.starts_with("video/")
        || essence.starts_with("audio/")
        || essence == "application/pdf";
    !inert
}

/// S3 refuses `response-*` overrides on anonymous requests (400
/// InvalidRequest). Honouring them let anyone relabel a public object as
/// `text/html` (review S5). Presigned URLs are signed, so they keep them.
pub(super) fn check_response_overrides_allowed(
    input: &s3s::dto::GetObjectInput,
    anonymous_principal: bool,
) -> s3s::S3Result<()> {
    let any_override = input.response_content_type.is_some()
        || input.response_content_disposition.is_some()
        || input.response_content_encoding.is_some()
        || input.response_content_language.is_some()
        || input.response_cache_control.is_some()
        || input.response_expires.is_some();
    if anonymous_principal && any_override {
        return Err(s3s::s3_error!(
            InvalidRequest,
            "Request specific response headers cannot be used for anonymous GET requests."
        ));
    }
    Ok(())
}

pub(super) fn apply_get_response_overrides(
    input: &s3s::dto::GetObjectInput,
    output: &mut s3s::dto::GetObjectOutput,
) {
    if let Some(v) = input.response_content_type.as_ref() {
        output.content_type = Some(v.clone());
    }
    if let Some(v) = input.response_content_disposition.as_ref() {
        output.content_disposition = Some(v.clone());
    }
    if let Some(v) = input.response_content_encoding.as_ref() {
        output.content_encoding = Some(v.clone());
    }
    if let Some(v) = input.response_content_language.as_ref() {
        output.content_language = Some(v.clone());
    }
    if let Some(v) = input.response_cache_control.as_ref() {
        output.cache_control = Some(v.clone());
    }
    if let Some(v) = input.response_expires.as_ref() {
        output.expires = Some(v.clone());
    }
}

/// Check a signed payload hash that s3s did not verify against the body
/// (`s3s_verified_payload_sha256` returned `None`). Returns the body's
/// SHA-256 when the check ran and passed, `None` for an unsigned shape. A
/// body above `BLOCKING_HASH_THRESHOLD` hashes on a blocking thread.
pub(super) async fn verify_signed_payload_hash_s3s(
    signed: Option<&crate::api::auth::SignedPayloadHash>,
    body: &bytes::Bytes,
) -> s3s::S3Result<Option<crate::deltaglider::KnownSha256>> {
    let Some(claimed) = signed.filter(|c| c.is_verifiable_hex()).cloned() else {
        return Ok(None);
    };
    let check = move |body: &[u8]| {
        // Delegate to the canonical verifier so axum + s3s can never desync
        // on the H1 integrity contract (BadDigest / NotImplemented).
        claimed.verify_against_body(body)?;
        Ok::<_, crate::api::S3Error>(crate::deltaglider::KnownSha256::from_verified_hex(
            claimed.as_str(),
        ))
    };
    let verified = if body.len() <= crate::deltaglider::BLOCKING_HASH_THRESHOLD {
        check(body)
    } else {
        let body = body.clone();
        tokio::task::spawn_blocking(move || check(&body))
            .await
            .map_err(|e| s3s::s3_error!(InternalError, "hash task: {e}"))?
    };
    Ok(verified?)
}

pub(super) fn validate_delete_objects_count(count: usize) -> s3s::S3Result<()> {
    if count > 1000 {
        return Err(s3s::s3_error!(
            InvalidArgument,
            "DeleteObjects supports at most 1000 keys per request"
        ));
    }
    Ok(())
}

pub(super) fn head_object_output_from_metadata(
    meta: &FileMetadata,
    reader: Reader,
) -> s3s::S3Result<s3s::dto::HeadObjectOutput> {
    let e_tag = parse_s3s_etag(&meta.etag())?;
    let last_modified = http_last_modified(meta);
    let content_length = i64::try_from(meta.file_size).unwrap_or(i64::MAX);
    // Treat a blank content-type the same as absent: some backends return
    // the `content-type` header present-but-empty for objects stored without
    // one, which arrives here as `Some("")`. An empty content-type on the wire
    // + `nosniff` makes browsers render raw bytes instead of downloading.
    let content_type = meta
        .content_type
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "application/octet-stream".to_string());

    Ok(s3s::dto::HeadObjectOutput {
        accept_ranges: Some("bytes".to_string()),
        content_length: Some(content_length),
        content_type: Some(content_type),
        e_tag: Some(e_tag),
        last_modified: Some(last_modified),
        metadata: Some(response_metadata_map(meta, reader)),
        ..Default::default()
    })
}

pub(super) fn get_object_output_from_metadata(
    meta: &FileMetadata,
    body: s3s::dto::StreamingBlob,
    reader: Reader,
) -> s3s::S3Result<s3s::dto::GetObjectOutput> {
    let head = head_object_output_from_metadata(meta, reader)?;
    Ok(s3s::dto::GetObjectOutput {
        accept_ranges: head.accept_ranges,
        body: Some(body),
        content_length: head.content_length,
        content_type: head.content_type,
        e_tag: head.e_tag,
        last_modified: head.last_modified,
        metadata: head.metadata,
        ..Default::default()
    })
}

/// S3 DeleteObjects: what a batch costs, and what a cut request leaves.
#[cfg(test)]
mod delete_objects_tests {
    use super::*;
    use crate::deltaglider::store_deltas;
    use crate::storage::{FakeS3, StorageBackend, StorageBackendExt};

    /// A service over a fake S3 with bucket `b` and an event outbox.
    async fn service() -> (DeltaGliderS3Service, Arc<FakeS3>) {
        let (engine, fake) = crate::deltaglider::s3_engine().await;
        let mut state = crate::api::handlers::AppState::for_tests(engine);
        Arc::get_mut(&mut state).unwrap().jobs_db = Some(Arc::new(tokio::sync::Mutex::new(
            crate::config_db::JobsDb::in_memory("test").unwrap(),
        )));
        let config = Arc::new(tokio::sync::RwLock::new(crate::config::Config::default()));
        (DeltaGliderS3Service::new(state, config), fake)
    }

    fn request(keys: &[String]) -> s3s::S3Request<s3s::dto::DeleteObjectsInput> {
        let input = s3s::dto::DeleteObjectsInput::builder()
            .bucket("b".to_string())
            .delete(s3s::dto::Delete {
                objects: keys
                    .iter()
                    .map(|k| s3s::dto::ObjectIdentifier {
                        key: k.clone(),
                        ..Default::default()
                    })
                    .collect(),
                quiet: None,
            })
            .build()
            .unwrap();
        s3s::S3Request {
            input,
            method: axum::http::Method::POST,
            uri: "/b?delete".parse().unwrap(),
            headers: Default::default(),
            extensions: Default::default(),
            credentials: None,
            region: None,
            service: None,
            trailing_headers: None,
        }
    }

    async fn deleted_events(svc: &DeltaGliderS3Service) -> i64 {
        let db = svc.state().jobs_db.as_ref().unwrap();
        crate::event_outbox::flush_appends(db).await;
        let db = db.lock().await;
        db.event_outbox_count(None).unwrap()
    }

    /// Wait (10 s at most) until `folder` has no reference.bin.
    async fn reclaimed(svc: &DeltaGliderS3Service, folder: &str) -> bool {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        let engine = svc.state().engine.load_full();
        while engine.storage().has_reference("b", folder).await.unwrap() {
            if tokio::time::Instant::now() > deadline {
                return false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        true
    }

    /// A request cut by a client disconnect or the request timeout does
    /// not cut the batch: every key goes, with its ObjectDeleted event,
    /// and the folder's reference.bin is reclaimed. The batch ran inside
    /// the request and stored the events after its loop, so a cut lost
    /// the events and the reclaim, and left a folder that lists no key.
    #[tokio::test]
    async fn a_cut_request_still_deletes_every_key_with_its_event() {
        let (svc, fake) = service().await;
        let keys = store_deltas(&svc.state().engine.load(), "run", 3).await;
        fake.set_delete_delay_ms(300);

        // Cut while the first DELETE is in flight.
        let cut = fake
            .cut_at(delete_objects(&svc, request(&keys)), |r| {
                r.starts_with("DELETE /b/run/")
            })
            .await;

        assert!(cut, "the request was cut");
        assert!(reclaimed(&svc, "run").await, "run/reference.bin stays");
        let engine = svc.state().engine.load_full();
        for key in &keys {
            assert!(engine.head("b", key).await.unwrap_err().is_not_found());
        }
        assert_eq!(deleted_events(&svc).await, 3);
    }

    /// One DELETE per key, and the folders of one request at once. Each
    /// key sent a second DELETE for its absent other variant, and the keys
    /// went one after another (about 300 s for 1,000 keys at 100 ms per
    /// request: the request timeout).
    #[tokio::test]
    async fn a_batch_sends_one_delete_per_key_and_runs_its_folders_at_once() {
        let (svc, fake) = service().await;
        let engine = svc.state().engine.load_full();
        let mut keys = Vec::new();
        for folder in 0..4 {
            for i in 0..25 {
                let key = format!("f{folder}/img-{i:02}.jpg");
                engine
                    .store("b", &key, b"x", None, Default::default())
                    .await
                    .unwrap();
                keys.push(key);
            }
        }
        fake.clear();
        fake.set_delete_delay_ms(20);

        let out = delete_objects(&svc, request(&keys)).await.unwrap().output;

        assert_eq!(out.deleted.map_or(0, |d| d.len()), keys.len());
        assert!(out.errors.is_none());
        let deletes = fake
            .requests()
            .iter()
            .filter(|r| r.starts_with("DELETE "))
            .count();
        let peak = fake.peak_deletes_in_flight();
        eprintln!("100 keys in 4 folders: {deletes} DELETEs, peak {peak} in flight");
        assert_eq!(deletes, keys.len());
        assert!(peak > 1, "the folders went one after another");
    }

    /// A retry of a request cut before its reclaim finds the keys gone and
    /// still reclaims their folder's reference.bin.
    #[tokio::test]
    async fn a_retry_reclaims_the_folder_a_cut_request_left() {
        let (svc, _fake) = service().await;
        let engine = svc.state().engine.load_full();
        let keys = store_deltas(&engine, "run", 2).await;
        for i in 0..keys.len() {
            engine
                .storage()
                .delete_delta("b", "run", &format!("build-{i:03}.zip"))
                .await
                .unwrap();
        }
        assert!(engine.storage().has_reference("b", "run").await.unwrap());

        let out = delete_objects(&svc, request(&keys)).await.unwrap().output;

        assert_eq!(out.deleted.map_or(0, |d| d.len()), keys.len());
        assert!(!engine.storage().has_reference("b", "run").await.unwrap());
        assert_eq!(deleted_events(&svc).await, 0, "nothing was deleted now");
    }
}

/// What a PUT, and a read that misses its key, send to check the bucket.
#[cfg(test)]
mod bucket_check_cost_tests {
    use super::*;
    use crate::api::handlers::AppState;
    use crate::s3_adapter_s3s::multipart::adapter_test_support::{request, service};
    use crate::storage::FakeS3;

    fn head_buckets(fake: &FakeS3) -> usize {
        // The SDK sends HeadBucket as `HEAD /b/`.
        fake.requests()
            .iter()
            .filter(|r| *r == "HEAD /b/" || r.starts_with("HEAD /b/?"))
            .count()
    }

    fn get(key: &str) -> s3s::dto::GetObjectInput {
        s3s::dto::GetObjectInput::builder()
            .bucket("b".into())
            .key(key.into())
            .build()
            .unwrap()
    }

    fn put(key: &str) -> s3s::dto::PutObjectInput {
        s3s::dto::PutObjectInput::builder()
            .bucket("b".into())
            .key(key.into())
            .body(Some(s3s::dto::StreamingBlob::from(s3s::Body::from(
                b"x".to_vec(),
            ))))
            .build()
            .unwrap()
    }

    /// A GET or HEAD of a missing key costs the two HEADs of the key in a
    /// bucket found in the last 5 seconds: the bucket check that tells
    /// `NoSuchKey` from `NoSuchBucket` uses the write paths' cache. It sent
    /// a HeadBucket after every miss.
    #[tokio::test]
    async fn a_get_of_a_missing_key_in_a_known_bucket_sends_two_requests() {
        let (engine, fake) = crate::deltaglider::s3_engine().await;
        let svc = service(AppState::for_tests(engine));
        let first = get_object(&svc, request(get("dir/missing.zip")))
            .await
            .err()
            .unwrap();
        assert_eq!(*first.code(), s3s::S3ErrorCode::NoSuchKey);
        fake.clear();
        let again = get_object(&svc, request(get("dir/missing.zip")))
            .await
            .err()
            .unwrap();
        assert_eq!(*again.code(), s3s::S3ErrorCode::NoSuchKey);
        let n = fake.requests().len();
        eprintln!("GET of a missing key in a known bucket: {n} requests");
        assert_eq!(n, 2, "{:?}", fake.requests());

        // A missing bucket is still asked every time.
        fake.fail("HEAD_BUCKET", "/nope", 404, "NoSuchBucket", u32::MAX);
        let input = s3s::dto::GetObjectInput::builder()
            .bucket("nope".into())
            .key("k".into())
            .build()
            .unwrap();
        let err = get_object(&svc, request(input)).await.err().unwrap();
        assert_eq!(*err.code(), s3s::S3ErrorCode::NoSuchBucket);
    }

    /// PutObject checks its bucket with the write paths' cache: one
    /// HeadBucket per bucket per 5 seconds, not one per PUT.
    #[tokio::test]
    async fn puts_check_their_bucket_once() {
        let (engine, fake) = crate::deltaglider::s3_engine().await;
        let svc = service(AppState::for_tests(engine));
        fake.clear();
        for key in ["a/1.jpg", "a/2.jpg", "a/3.jpg"] {
            put_object(&svc, request(put(key))).await.unwrap();
        }
        assert_eq!(head_buckets(&fake), 1, "{:?}", fake.requests());
    }

    /// A LIST that finds nothing checks the bucket (S3 answers NoSuchBucket
    /// there), through the same cache: the file browser lists a folder again
    /// after it deletes it, and on prod that HeadBucket was the one in ~30
    /// that took 7-10 s.
    #[tokio::test]
    async fn an_empty_list_uses_the_cached_bucket_check() {
        let (engine, fake) = crate::deltaglider::s3_engine().await;
        let svc = service(AppState::for_tests(engine));
        fake.clear();
        put_object(&svc, request(put("a/1.jpg"))).await.unwrap();
        for _ in 0..2 {
            let input = s3s::dto::ListObjectsV2Input::builder()
                .bucket("b".into())
                .prefix(Some("none/".into()))
                .build()
                .unwrap();
            let out = crate::s3_adapter_s3s::list::list_objects_v2(&svc, request(input))
                .await
                .unwrap();
            assert!(out.output.contents.unwrap_or_default().is_empty());
        }
        assert_eq!(head_buckets(&fake), 1, "{:?}", fake.requests());
    }

    /// The LIST path checks a bucket through the cached check
    /// (`crate::api::handlers::ensure_bucket_exists_on`), never with a
    /// HeadBucket of its own.
    #[test]
    fn lists_check_buckets_through_the_cache() {
        let text = std::fs::read_to_string("src/s3_adapter_s3s/list.rs").unwrap();
        let sites: Vec<usize> = crate::source_scan::prod_lines(&text)
            .into_iter()
            .filter(|(_, l)| l.contains("ensure_bucket_on(") || l.contains(".head_bucket("))
            .map(|(n, _)| n)
            .collect();
        assert!(sites.is_empty(), "list.rs lines {sites:?}");
    }

    /// Every S3 verb checks a bucket through the cached check
    /// (`crate::api::handlers::ensure_bucket_exists`), never with a
    /// HeadBucket of its own. The one live check is the HeadBucket verb
    /// (`ensure_bucket_on` in bucket.rs, defined in mod.rs).
    #[test]
    fn object_verbs_check_buckets_through_the_cache() {
        let mut live = Vec::new();
        for (file, text) in crate::source_scan::prod_sources("src/s3_adapter_s3s") {
            let mut current_fn = String::new();
            for (n, l) in crate::source_scan::prod_lines(&text) {
                if let Some(i) = l.find("fn ") {
                    current_fn = l[i + 3..].split('(').next().unwrap_or("").to_string();
                }
                let call = l.contains(".head_bucket(")
                    || (l.contains("ensure_bucket_on(") && !l.contains("fn ensure_bucket_on("));
                if call {
                    live.push(format!("{file}:{n} in fn {current_fn}"));
                }
            }
        }
        let allowed = |site: &String| {
            (site.starts_with("src/s3_adapter_s3s/bucket.rs:")
                && site.ends_with("in fn head_bucket"))
                || (site.starts_with("src/s3_adapter_s3s/mod.rs:")
                    && site.ends_with("in fn ensure_bucket_on"))
        };
        let wrong: Vec<_> = live.iter().filter(|s| !allowed(s)).collect();
        assert!(
            wrong.is_empty(),
            "live bucket checks outside HeadBucket: {wrong:?}"
        );
        assert_eq!(live.len(), 2, "{live:?}");
    }

    fn copy_from(bucket: &str, key: &str, dst: &str) -> s3s::dto::CopyObjectInput {
        s3s::dto::CopyObjectInput::builder()
            .bucket("b".into())
            .key(dst.into())
            .copy_source(s3s::dto::CopySource::Bucket {
                bucket: bucket.into(),
                key: key.into(),
                version_id: None,
            })
            .build()
            .unwrap()
    }

    /// CopyObject sent a HeadBucket for its source on every copy. The
    /// source HEAD answers NoSuchKey for a missing bucket too; only then the
    /// cached check tells NoSuchBucket.
    #[tokio::test]
    async fn copies_send_no_head_bucket_for_their_source() {
        use crate::s3_adapter_s3s::copy::copy_object;
        let (engine, fake) = crate::deltaglider::s3_engine().await;
        let svc = service(AppState::for_tests(engine));
        put_object(&svc, request(put("a/1.jpg"))).await.unwrap();
        fake.clear();
        for dst in ["a/2.jpg", "a/3.jpg", "a/4.jpg"] {
            copy_object(&svc, request(copy_from("b", "a/1.jpg", dst)))
                .await
                .unwrap();
        }
        assert_eq!(head_buckets(&fake), 0, "{:?}", fake.requests());

        let err = copy_object(&svc, request(copy_from("b", "a/none.jpg", "a/5.jpg")))
            .await
            .err()
            .unwrap();
        assert_eq!(*err.code(), s3s::S3ErrorCode::NoSuchKey);
        fake.fail("HEAD_BUCKET", "/nope", 404, "NoSuchBucket", u32::MAX);
        let err = copy_object(&svc, request(copy_from("nope", "k", "a/5.jpg")))
            .await
            .err()
            .unwrap();
        assert_eq!(*err.code(), s3s::S3ErrorCode::NoSuchBucket);
    }

    /// The multipart verbs and the bucket sub-resource verbs check through
    /// the cache; HeadBucket itself stays live.
    #[tokio::test]
    async fn bucket_and_multipart_verbs_check_through_the_cache() {
        use crate::s3_adapter_s3s::{bucket, multipart};
        let (engine, fake) = crate::deltaglider::s3_engine().await;
        let svc = service(AppState::for_tests(engine));
        fake.clear();
        for _ in 0..3 {
            let input = s3s::dto::GetBucketLocationInput::builder()
                .bucket("b".into())
                .build()
                .unwrap();
            bucket::get_bucket_location(&svc, request(input))
                .await
                .unwrap();
            let input = s3s::dto::CreateMultipartUploadInput::builder()
                .bucket("b".into())
                .key("a/big.bin".into())
                .build()
                .unwrap();
            multipart::create_multipart_upload(&svc, request(input))
                .await
                .unwrap();
            let input = s3s::dto::ListMultipartUploadsInput::builder()
                .bucket("b".into())
                .build()
                .unwrap();
            multipart::list_multipart_uploads(&svc, request(input))
                .await
                .unwrap();
        }
        assert_eq!(head_buckets(&fake), 1, "{:?}", fake.requests());
        fake.clear();
        for _ in 0..2 {
            let input = s3s::dto::HeadBucketInput::builder()
                .bucket("b".into())
                .build()
                .unwrap();
            bucket::head_bucket(&svc, request(input)).await.unwrap();
        }
        assert_eq!(head_buckets(&fake), 2, "HeadBucket is live");
    }
}
