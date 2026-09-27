// SPDX-License-Identifier: BUSL-1.1

//! Object verbs: HEAD/GET/PUT/DELETE of one object, DeleteObjects, and the
//! object ACL and tagging stubs.

use super::*;

pub(super) async fn head_object(
    svc: &DeltaGliderS3Service,
    req: s3s::S3Request<s3s::dto::HeadObjectInput>,
) -> s3s::S3Result<s3s::S3Response<s3s::dto::HeadObjectOutput>> {
    let reader = Reader::of(&req.extensions);
    let if_range = req.headers.get(axum::http::header::IF_RANGE).cloned();
    let input = req.input;
    let engine = svc.state.engine.load();
    let meta = match engine.head(&input.bucket, &input.key).await {
        Ok(meta) => meta,
        Err(e) => {
            return Err(no_such_key_or_bucket(&engine, &input.bucket, s3s::S3Error::from(e)).await)
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
    add_storage_debug_headers(&mut resp.headers, &meta);
    Ok(resp)
}

pub(super) async fn get_object(
    svc: &DeltaGliderS3Service,
    req: s3s::S3Request<s3s::dto::GetObjectInput>,
) -> s3s::S3Result<s3s::S3Response<s3s::dto::GetObjectOutput>> {
    let reader = Reader::of(&req.extensions);
    let anonymous_principal = req
        .extensions
        .get::<AuthenticatedUser>()
        .is_some_and(|u| u.is_anonymous());
    let if_range = req.headers.get(axum::http::header::IF_RANGE).cloned();
    let input = req.input;
    check_response_overrides_allowed(&input, anonymous_principal)?;
    let engine = svc.state.engine.load();
    let conditional = input.if_match.is_some()
        || input.if_none_match.is_some()
        || input.if_modified_since.is_some()
        || input.if_unmodified_since.is_some();
    let read = match get_metadata_source(conditional, input.range.is_some()) {
        GetMetadata::Fresh => Some(engine.head(&input.bucket, &input.key).await),
        GetMetadata::Cached => Some(engine.read_metadata(&input.bucket, &input.key).await),
        GetMetadata::None => None,
    };
    let head = match read.transpose() {
        Ok(head) => head,
        Err(e) => {
            return Err(no_such_key_or_bucket(&engine, &input.bucket, s3s::S3Error::from(e)).await)
        }
    };
    if let Some(head) = &head {
        evaluate_read_conditionals_s3s(
            head,
            input.if_match.as_ref(),
            input.if_none_match.as_ref(),
            input.if_modified_since.as_ref(),
            input.if_unmodified_since.as_ref(),
        )?;
    }
    let checked = match &head {
        Some(head) => served_range(input.range.as_ref(), if_range.as_ref(), head)?
            .map(|checked| (checked, head.file_size)),
        None => None,
    };

    if let Some((checked, file_size)) = checked {
        let start = checked.start;
        let end_inclusive = checked.end.saturating_sub(1);
        let content_range = format!("bytes {start}-{end_inclusive}/{file_size}");

        if let Some((stream, content_length, metadata)) = engine
            .retrieve_stream_range(&input.bucket, &input.key, start, end_inclusive, None)
            .await?
        {
            let body = s3s::dto::StreamingBlob::new(SyncStorageStream::new(stream));
            let mut output = get_object_output_from_metadata(&metadata, body, reader)?;
            output.content_length = Some(i64::try_from(content_length).unwrap_or(i64::MAX));
            output.content_range = Some(content_range);
            apply_get_response_overrides(&input, &mut output);
            // s3s answers 206 because `content_range` is set.
            let mut resp = s3s::S3Response::new(output);
            add_storage_debug_headers(&mut resp.headers, &metadata);
            add_get_object_security_headers(&mut resp);
            return Ok(resp);
        }

        let (data, metadata) = engine.retrieve(&input.bucket, &input.key).await?;
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
        add_storage_debug_headers(&mut resp.headers, &metadata);
        add_get_object_security_headers(&mut resp);
        return Ok(resp);
    }

    let response = match engine.retrieve_stream(&input.bucket, &input.key).await {
        Ok(r) => r,
        Err(e) => {
            return Err(no_such_key_or_bucket(&engine, &input.bucket, s3s::S3Error::from(e)).await)
        }
    };
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
    apply_get_response_overrides(&input, &mut output);
    let mut resp = s3s::S3Response::new(output);
    add_storage_debug_headers(&mut resp.headers, &metadata);
    add_get_object_security_headers(&mut resp);
    Ok(resp)
}

/// Where a GET reads the metadata it judges before the body.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(super) enum GetMetadata {
    /// A fresh HEAD of storage: a condition must not pass on a stale cache
    /// (another node may have written the key).
    Fresh,
    /// The metadata cache (else storage): a range needs the object size.
    Cached,
    /// No lookup: `retrieve_stream` resolves the metadata it serves.
    None,
}

/// Pure: the metadata source of a GET with or without a condition header
/// (`If-Match`, `If-None-Match`, `If-Modified-Since`, `If-Unmodified-Since`)
/// and a `Range`.
pub(super) fn get_metadata_source(conditional: bool, ranged: bool) -> GetMetadata {
    match (conditional, ranged) {
        (true, _) => GetMetadata::Fresh,
        (false, true) => GetMetadata::Cached,
        (false, false) => GetMetadata::None,
    }
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
            ensure_bucket_exists_s3s(&svc.state, &input.bucket).await?;
            Ok(s3s::S3Response::new(s3s::dto::DeleteObjectOutput::default()))
        }
        Err(e) => Err(s3s::S3Error::from(e)),
    }
}

pub(super) async fn delete_objects(
    svc: &DeltaGliderS3Service,
    req: s3s::S3Request<s3s::dto::DeleteObjectsInput>,
) -> s3s::S3Result<s3s::S3Response<s3s::dto::DeleteObjectsOutput>> {
    let headers = req.headers.clone();
    let input = req.input;
    crate::api::handlers::object_helpers::check_client_write_allowed(&svc.state, &input.bucket)?;
    validate_delete_objects_count(input.delete.objects.len())?;
    // Per-key misses are successes, so a missing bucket is asked here.
    ensure_bucket_exists_s3s(&svc.state, &input.bucket).await?;
    let quiet = input.delete.quiet.unwrap_or(false);
    // Per-key IAM authorization. The middleware only authorized the
    // bucket-level POST ?delete; each key in the batch body must be
    // checked individually, or a `Deny delete bucket/protected/*`
    // carve-out (or a prefix-scoped Allow) is silently ignored and
    // protected objects get batch-deleted. (X-ray H6/H26.)
    let auth_user = req.extensions.get::<AuthenticatedUser>().cloned();
    let policy_context = request_policy_context(&req.extensions);
    let mut deleted = Vec::new();
    let mut errors = Vec::new();
    // Collect ObjectDeleted events for keys that were ACTUALLY deleted
    // (Ok, not NotFound), filtered to user objects, and batch-insert once
    // after the loop (single DB lock).
    let mut delete_events: Vec<crate::event_outbox::NewEvent> = Vec::new();
    // Review C9: a plain `delete` lists the whole deltaspace after each
    // key to decide reference reclamation, so a 1000-key batch in one
    // deltaspace was O(N²). Delete in sweep mode and reclaim once per
    // touched deltaspace.
    let engine = svc.state.engine.load();
    let mut touched_deltaspaces: std::collections::BTreeSet<String> = Default::default();
    for obj in input.delete.objects {
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
                let s3_err: crate::api::S3Error = crate::api::S3Error::AccessDenied;
                errors.push(s3s::dto::Error {
                    key: Some(obj.key),
                    version_id: obj.version_id,
                    code: Some(s3_err.code().to_string()),
                    message: Some(s3_err.to_string()),
                });
                continue;
            }
        }
        match engine.delete_in_sweep(&input.bucket, &key).await {
            Ok(_) => {
                touched_deltaspaces
                    .insert(crate::types::ObjectKey::parse(&input.bucket, &key).deltaspace_id());
                if crate::replication::event_consumer::is_user_object_key(&key) {
                    delete_events.push(crate::event_outbox::NewEvent::new(
                        crate::event_outbox::EventKind::ObjectDeleted,
                        input.bucket.clone(),
                        key.clone(),
                        crate::event_outbox::EventSource::S3Api,
                        crate::replication::current_unix_seconds(),
                        serde_json::json!({}),
                    ));
                }
                if !quiet {
                    deleted.push(s3s::dto::DeletedObject {
                        key: Some(obj.key),
                        version_id: obj.version_id,
                        ..Default::default()
                    });
                }
            }
            Err(crate::deltaglider::EngineError::NotFound(_)) => {
                if !quiet {
                    deleted.push(s3s::dto::DeletedObject {
                        key: Some(obj.key),
                        version_id: obj.version_id,
                        ..Default::default()
                    });
                }
            }
            Err(e) => {
                let s3_err: crate::api::S3Error = e.into();
                errors.push(s3s::dto::Error {
                    key: Some(obj.key),
                    version_id: obj.version_id,
                    code: Some(s3_err.code().to_string()),
                    message: Some(s3_err.to_string()),
                });
            }
        }
    }
    for ds in &touched_deltaspaces {
        // Best effort: the objects are gone already.
        if let Err(e) = engine.reclaim_empty_deltaspace(&input.bucket, ds).await {
            tracing::warn!(
                "DeleteObjects reference reclaim failed for {}/{ds}: {e}",
                input.bucket
            );
        }
    }
    crate::api::handlers::object_helpers::enqueue_object_events(&svc.state, &delete_events).await;
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
    let chunked_headers = headers_if_still_aws_chunked(&req);
    let signed_payload_hash = req
        .extensions
        .get::<crate::api::auth::SignedPayloadHash>()
        .cloned();
    let input = req.input;
    let engine = svc.state.engine.load();
    // Gate BEFORE head_bucket: a marked bucket refuses client bytes
    // regardless of backend reachability (403-before-404, no backend I/O
    // for a doomed request).
    crate::api::handlers::object_helpers::check_client_write_allowed(&svc.state, &input.bucket)?;
    check_user_metadata_size_s3s(input.metadata.as_ref())?;
    if !engine.head_bucket(&input.bucket).await? {
        return Err(s3s::s3_error!(NoSuchBucket));
    }

    let data = collect_blob_limited(
        input.body,
        engine.max_object_size(),
        chunked_headers.as_ref(),
    )
    .await?;
    verify_signed_payload_hash_s3s(signed_payload_hash.as_ref(), &data)?;
    validate_content_md5_s3s(input.content_md5.as_deref(), &data)?;
    // The body is already collected here (SigV4 payload-hash verification
    // needs it); the engine spools a large delta-eligible body.
    let result = crate::api::handlers::object_helpers::store_client_write(
        &svc.state,
        crate::api::handlers::object_helpers::ClientWrite {
            bucket: &input.bucket,
            key: &input.key,
            data: &data,
            content_type: input.content_type,
            user_metadata: input.metadata.unwrap_or_default(),
            precondition: &crate::deltaglider::Precondition {
                if_match: input.if_match,
                if_none_match: input.if_none_match,
            },
        },
    )
    .await?;
    let mut resp = s3s::S3Response::new(s3s::dto::PutObjectOutput {
        e_tag: Some(parse_s3s_etag(&result.metadata.etag())?),
        ..Default::default()
    });
    add_storage_debug_headers(&mut resp.headers, &result.metadata);
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
        _ => Err(s3s::s3_error!(InvalidRange)),
    }
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
/// §15.4.5). s3s writes an error's own headers IN PLACE of its XML ones, so
/// the 304 also loses the `Content-Type`/`Content-Length` of a body that is
/// never sent (s3surface-7).
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
    let mut err = s3s::s3_error!(NotModified);
    err.set_headers(headers);
    err
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

pub(super) fn verify_signed_payload_hash_s3s(
    signed: Option<&crate::api::auth::SignedPayloadHash>,
    body: &[u8],
) -> s3s::S3Result<()> {
    let Some(claimed) = signed else {
        return Ok(());
    };
    // Delegate to the canonical verifier so axum + s3s can never desync
    // on the H1 integrity contract (BadDigest / NotImplemented).
    Ok(claimed.verify_against_body(body)?)
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
