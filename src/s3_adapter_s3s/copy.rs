// SPDX-License-Identifier: BUSL-1.1

//! Server-side copy verbs: CopyObject and UploadPartCopy, with the copy
//! source parser, its access check and its conditionals.

use super::*;

pub(super) async fn copy_object(
    svc: &DeltaGliderS3Service,
    req: s3s::S3Request<s3s::dto::CopyObjectInput>,
) -> s3s::S3Result<s3s::S3Response<s3s::dto::CopyObjectOutput>> {
    let auth_user = req.extensions.get::<AuthenticatedUser>().cloned();
    let input = req.input;
    let (source_bucket, source_key) = copy_source_bucket_key(&input.copy_source)?;
    let client_ip = req
        .extensions
        .get::<crate::api::auth::RequestClientIp>()
        .map(|c| c.0);
    check_copy_source_access_s3s(
        svc.state.engine.load().bucket_policy_registry(),
        auth_user.as_ref(),
        &source_bucket,
        &source_key,
        client_ip,
        &req.headers,
    )?;
    // Gate on the DESTINATION bucket first — copy-into is a client write,
    // refused regardless of backend reachability (403-before-404).
    crate::api::handlers::object_helpers::check_client_write_allowed(&svc.state, &input.bucket)?;
    let directive = input
        .metadata_directive
        .as_ref()
        .map(|d| d.as_str())
        .unwrap_or(s3s::dto::MetadataDirective::COPY);
    let changes_attributes = input.storage_class.is_some()
        || input.server_side_encryption.is_some()
        || input.ssekms_key_id.is_some()
        || input.sse_customer_algorithm.is_some()
        || input.website_redirect_location.is_some();
    if is_illegal_self_copy(
        (&source_bucket, &source_key),
        (&input.bucket, &input.key),
        directive.eq_ignore_ascii_case("REPLACE") || changes_attributes,
    ) {
        return Err(s3s::s3_error!(
            InvalidRequest,
            "This copy request is illegal because it is trying to copy an object to \
             itself without changing the object's metadata, storage class, website \
             redirect location or encryption attributes."
        ));
    }
    ensure_bucket_exists_s3s(&svc.state, &source_bucket).await?;
    // The destination is written: the write paths' brief bucket cache.
    crate::api::handlers::ensure_bucket_exists(&svc.state, &input.bucket).await?;
    let engine = svc.state.engine.load();
    let source_meta = engine.head(&source_bucket, &source_key).await?;
    evaluate_copy_source_conditionals_s3s(
        &source_meta,
        input.copy_source_if_match.as_ref(),
        input.copy_source_if_none_match.as_ref(),
        input.copy_source_if_modified_since.as_ref(),
        input.copy_source_if_unmodified_since.as_ref(),
    )?;
    let dest_metadata = if directive.eq_ignore_ascii_case("REPLACE") {
        input.metadata.clone().unwrap_or_default()
    } else {
        source_meta.user_metadata.clone()
    };
    if source_meta.file_size > copy_size_limit(&engine, &input.bucket, &input.key, &dest_metadata) {
        return Err(s3s::s3_error!(EntityTooLarge));
    }
    let body = read_copy_source(
        &engine,
        &source_bucket,
        &source_key,
        source_meta.file_size,
        engine.spool_threshold(),
    )
    .await?;
    let source_meta = body.metadata();
    let (content_type, mut user_metadata) = if directive.eq_ignore_ascii_case("REPLACE") {
        check_user_metadata_size_s3s(input.metadata.as_ref())?;
        (input.content_type, input.metadata.unwrap_or_default())
    } else if directive.eq_ignore_ascii_case("COPY") {
        (
            source_meta.content_type.clone(),
            source_meta.user_metadata.clone(),
        )
    } else {
        return Err(s3s::s3_error!(
            InvalidArgument,
            "metadata-directive must be COPY or REPLACE"
        ));
    };
    // retrieve() decrypted the body; the source metadata still carries the
    // source's dg-encryption markers. Storing them onto a decrypted body
    // makes the destination unreadable (read path thinks it's encrypted).
    crate::storage::encrypting::strip_encryption_markers(&mut user_metadata);
    crate::transfer::strip_rule_provenance(&mut user_metadata);
    // A copy creates a new object at the destination: the same client
    // write as a PUT (quota on the destination, the object's write lock,
    // the ObjectCreated event). s3s models no conditional headers for it.
    let precondition = crate::deltaglider::Precondition::none();
    let result = match &body {
        CopySourceBody::Buffered(data, _) => {
            crate::api::handlers::object_helpers::store_client_write(
                &svc.state,
                crate::api::handlers::object_helpers::ClientWrite {
                    bucket: &input.bucket,
                    key: &input.key,
                    data,
                    content_type,
                    user_metadata,
                    precondition: &precondition,
                },
            )
            .await?
        }
        CopySourceBody::Spooled { spool, size, .. } => {
            crate::api::handlers::object_helpers::store_client_write_spooled(
                &svc.state,
                crate::api::handlers::object_helpers::SpooledClientWrite {
                    bucket: &input.bucket,
                    key: &input.key,
                    spool,
                    size: *size,
                    content_type,
                    user_metadata,
                    precondition: &precondition,
                },
            )
            .await?
        }
    };
    Ok(s3s::S3Response::new(s3s::dto::CopyObjectOutput {
        copy_object_result: Some(s3s::dto::CopyObjectResult {
            e_tag: Some(parse_s3s_etag(&result.metadata.etag())?),
            last_modified: Some(SystemTime::from(result.metadata.created_at).into()),
            ..Default::default()
        }),
        ..Default::default()
    }))
}

/// S3's limit on the source of one CopyObject request.
pub(super) const COPY_OBJECT_MAX_BYTES: u64 = 5 * 1024 * 1024 * 1024;

/// The largest source that CopyObject copies into `bucket/key` with
/// `user_metadata`. A destination that stores the object as it is
/// (passthrough) takes S3's 5 GiB copy limit (capped at
/// `max_passthrough_object_size`): the source streams through a spool file,
/// so its size costs disk, not memory. A destination that tries a delta keeps
/// `max_object_size`, the store's limit for a delta. Every copy had the
/// delta limit (100 MiB by default), so rclone and mc copies of larger
/// objects failed with EntityTooLarge.
pub(super) fn copy_size_limit(
    engine: &crate::deltaglider::DynEngine,
    bucket: &str,
    key: &str,
    user_metadata: &std::collections::HashMap<String, String>,
) -> u64 {
    if engine.write_tries_delta(bucket, key, user_metadata) {
        engine.max_object_size()
    } else {
        COPY_OBJECT_MAX_BYTES.min(engine.max_passthrough_object_size())
    }
}

/// The body of a CopyObject source, read with bounded memory.
pub(super) enum CopySourceBody {
    /// At most the spool threshold (or a delta the engine reconstructed in
    /// memory): the same buffer a PutObject body is.
    Buffered(Vec<u8>, FileMetadata),
    /// Larger: streamed into a spool file on the spool budget.
    Spooled {
        spool: crate::deltaglider::spool::Spool,
        size: u64,
        metadata: FileMetadata,
    },
}

impl CopySourceBody {
    fn metadata(&self) -> FileMetadata {
        match self {
            CopySourceBody::Buffered(_, m) | CopySourceBody::Spooled { metadata: m, .. } => {
                m.clone()
            }
        }
    }
}

/// Read a copy source. Up to `spool_threshold` bytes it is buffered, as a
/// PutObject body is; above it the source streams into a spool file, so a
/// CopyObject never holds a large object in memory. `head_size` is the
/// size the conditionals judged: the spool is reserved for it, and a
/// source that grew since answers 503 SlowDown (the client retries and the
/// retry judges the new object).
pub(super) async fn read_copy_source(
    engine: &crate::deltaglider::DynEngine,
    bucket: &str,
    key: &str,
    head_size: u64,
    spool_threshold: u64,
) -> s3s::S3Result<CopySourceBody> {
    let too_large = |size: u64| size > engine.max_object_size();
    if head_size <= spool_threshold {
        let (data, metadata) = engine.retrieve(bucket, key).await?;
        if too_large(data.len() as u64) {
            return Err(s3s::s3_error!(EntityTooLarge));
        }
        return Ok(CopySourceBody::Buffered(data, metadata));
    }
    let (mut stream, metadata) = match engine.retrieve_stream(bucket, key).await? {
        RetrieveResponse::Buffered { data, metadata, .. } => {
            if too_large(data.len() as u64) {
                return Err(s3s::s3_error!(EntityTooLarge));
            }
            return Ok(CopySourceBody::Buffered(data, metadata));
        }
        RetrieveResponse::Streamed {
            stream, metadata, ..
        } => (stream, metadata),
    };
    let spool = engine.spool_acquire(head_size).await?;
    use crate::deltaglider::spool::SpoolFillError;
    let fill = spool
        .fill_from_stream(&mut stream, head_size)
        .await
        .map_err(|e| match e {
            SpoolFillError::Source(e) => s3s::S3Error::from(e),
            SpoolFillError::Write(e) => crate::api::S3Error::from(StorageError::from(e)).into(),
            SpoolFillError::Overrun { .. } => crate::api::S3Error::SlowDown(
                "the copy source changed during the copy; retry the request".to_string(),
            )
            .into(),
        })?;
    Ok(CopySourceBody::Spooled {
        spool,
        size: fill.written,
        metadata,
    })
}

pub(super) async fn upload_part_copy(
    svc: &DeltaGliderS3Service,
    req: s3s::S3Request<s3s::dto::UploadPartCopyInput>,
) -> s3s::S3Result<s3s::S3Response<s3s::dto::UploadPartCopyOutput>> {
    let auth_user = req.extensions.get::<AuthenticatedUser>().cloned();
    let input = req.input;
    let (source_bucket, source_key) = copy_source_bucket_key(&input.copy_source)?;
    let client_ip = req
        .extensions
        .get::<crate::api::auth::RequestClientIp>()
        .map(|c| c.0);
    check_copy_source_access_s3s(
        svc.state.engine.load().bucket_policy_registry(),
        auth_user.as_ref(),
        &source_bucket,
        &source_key,
        client_ip,
        &req.headers,
    )?;
    // Gate on the DESTINATION bucket — part-copy feeds a client write
    // (403-before-404, no backend I/O for a doomed request).
    crate::api::handlers::object_helpers::check_client_write_allowed(&svc.state, &input.bucket)?;
    ensure_bucket_exists_s3s(&svc.state, &source_bucket).await?;
    // A part of the destination upload: the write paths' brief bucket cache.
    crate::api::handlers::ensure_bucket_exists(&svc.state, &input.bucket).await?;
    let engine = svc.state.engine.load();
    let source_meta = engine.head(&source_bucket, &source_key).await?;
    evaluate_copy_source_conditionals_s3s(
        &source_meta,
        input.copy_source_if_match.as_ref(),
        input.copy_source_if_none_match.as_ref(),
        input.copy_source_if_modified_since.as_ref(),
        input.copy_source_if_unmodified_since.as_ref(),
    )?;
    svc.state
        .multipart
        .set_max_object_size(engine.max_object_size());
    // A ranged part of a passthrough source is read as that range only,
    // pinned to the generation the conditionals judged: an SDK managed copy
    // sends one UploadPartCopy per part, and each used to buffer the whole
    // source (s3surface-10). The part itself is bounded like an UploadPart.
    let ranged = match input.copy_source_range.as_deref() {
        Some(range) => {
            let len = usize::try_from(source_meta.file_size).unwrap_or(usize::MAX);
            let (start, end) = parse_copy_range(range, len)?;
            if (end - start) as u64 >= engine.max_object_size() {
                return Err(s3s::s3_error!(EntityTooLarge));
            }
            engine
                .retrieve_stream_range(
                    &source_bucket,
                    &source_key,
                    start as u64,
                    end as u64,
                    Some(&source_meta),
                )
                .await?
        }
        None => None,
    };
    let part = if let Some((stream, content_length, _)) = ranged {
        collect_exact(stream, content_length).await?
    } else {
        // A delta source at most the spool threshold (a larger one is read
        // as its range above), or a whole-object part: `engine.retrieve`
        // buffers the ENTIRE source, and the range is sliced only after.
        // Reject a source over `max_object_size` before and after the
        // buffering read.
        if source_meta.file_size > engine.max_object_size() {
            return Err(s3s::s3_error!(EntityTooLarge));
        }
        let (data, _) = engine.retrieve(&source_bucket, &source_key).await?;
        if data.len() as u64 > engine.max_object_size() {
            return Err(s3s::s3_error!(EntityTooLarge));
        }
        if let Some(range) = input.copy_source_range.as_deref() {
            let (start, end) = parse_copy_range(range, data.len())?;
            bytes::Bytes::from(data[start..=end].to_vec())
        } else {
            bytes::Bytes::from(data)
        }
    };
    let etag = svc.state.multipart.upload_part(
        &input.upload_id,
        &input.bucket,
        &input.key,
        input.part_number as u32,
        part,
    )?;
    Ok(s3s::S3Response::new(s3s::dto::UploadPartCopyOutput {
        copy_part_result: Some(s3s::dto::CopyPartResult {
            e_tag: Some(parse_s3s_etag(&etag)?),
            last_modified: Some(SystemTime::now().into()),
            ..Default::default()
        }),
        ..Default::default()
    }))
}

pub(crate) fn copy_source_bucket_key(
    source: &s3s::dto::CopySource,
) -> s3s::S3Result<(String, String)> {
    match source {
        s3s::dto::CopySource::Bucket {
            bucket,
            key,
            version_id,
        } => {
            if version_id.is_some() {
                return Err(s3s::s3_error!(
                    InvalidArgument,
                    "copy source versionId is not supported"
                ));
            }
            // s3s keeps `b//k` as key `/k`; the engine (`ObjectKey::parse`)
            // serves it as `k`. Authorize the key the engine will read, or a
            // Deny on `b/k*` is escaped through `x-amz-copy-source: b//k`.
            Ok((bucket.to_string(), key.trim_start_matches('/').to_string()))
        }
        s3s::dto::CopySource::AccessPoint { .. } | s3s::dto::CopySource::Outpost { .. } => {
            Err(s3s::s3_error!(
                NotImplemented,
                "copy source access points / outposts are not supported"
            ))
        }
    }
}

pub(super) fn check_copy_source_access_s3s(
    registry: &crate::bucket_policy::BucketPolicyRegistry,
    auth_user: Option<&AuthenticatedUser>,
    source_bucket: &str,
    source_key: &str,
    client_ip: Option<std::net::IpAddr>,
    headers: &axum::http::HeaderMap,
) -> s3s::S3Result<()> {
    // The request gate refuses the coordination bucket as the PATH bucket;
    // a copy names a second bucket, refused here for every identity.
    if let Some(reason) = registry.reserved_bucket_reason(source_bucket) {
        return Err(crate::api::errors::S3Error::AccessDeniedReason(reason).into());
    }
    let Some(user) = auth_user else {
        return Ok(());
    };
    let context = policy_context_for_ip(client_ip);
    if user.can_with_context(S3Action::Read, source_bucket, source_key, &context) {
        return Ok(());
    }
    crate::audit::audit_log(
        "access_denied",
        &user.name,
        "CopySourceRead",
        headers,
        source_bucket,
        source_key,
    );
    Err(s3s::s3_error!(AccessDenied))
}

pub(super) fn evaluate_copy_source_conditionals_s3s(
    source_meta: &FileMetadata,
    if_match: Option<&s3s::dto::ETagCondition>,
    if_none_match: Option<&s3s::dto::ETagCondition>,
    if_modified_since: Option<&s3s::dto::Timestamp>,
    if_unmodified_since: Option<&s3s::dto::Timestamp>,
) -> s3s::S3Result<()> {
    let current = parse_s3s_etag(&source_meta.etag())?;
    let last_modified = http_last_modified(source_meta);

    if let Some(cond) = if_match {
        let matches = cond.is_any()
            || cond
                .as_etag()
                .map(|wanted| wanted.weak_cmp(&current))
                .unwrap_or(false);
        if !matches {
            return Err(s3s::s3_error!(PreconditionFailed));
        }
        // AWS CopyObject: a passing ETag condition wins over date guard.
    } else if let Some(date) = if_unmodified_since {
        if last_modified > *date {
            return Err(s3s::s3_error!(PreconditionFailed));
        }
    }

    if let Some(cond) = if_none_match {
        let matches = cond.is_any()
            || cond
                .as_etag()
                .map(|wanted| wanted.weak_cmp(&current))
                .unwrap_or(false);
        if matches {
            return Err(s3s::s3_error!(PreconditionFailed));
        }
        // AWS CopyObject: a passing negative ETag condition wins over date guard.
    } else if let Some(date) = if_modified_since {
        if last_modified <= *date {
            return Err(s3s::s3_error!(PreconditionFailed));
        }
    }

    Ok(())
}

/// Pure: whether a CopyObject copies an object onto itself and changes
/// nothing. S3 refuses it with 400 InvalidRequest; the proxy re-encoded the
/// object instead (s3surface-16). The engine trims leading `/` from keys, so
/// `k` and `/k` are one object.
pub(super) fn is_illegal_self_copy(
    source: (&str, &str),
    dest: (&str, &str),
    changes: bool,
) -> bool {
    !changes
        && source.0 == dest.0
        && source.1.trim_start_matches('/') == dest.1.trim_start_matches('/')
}

/// Collect a ranged read of `len` bytes. More or fewer bytes than the backend
/// announced is a backend fault (500), never a short part.
pub(super) async fn collect_exact(
    mut stream: BoxStream<'static, Result<bytes::Bytes, StorageError>>,
    len: u64,
) -> s3s::S3Result<bytes::Bytes> {
    let mut buf = bytes::BytesMut::with_capacity(usize::try_from(len).unwrap_or(0));
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if (buf.len() + chunk.len()) as u64 > len {
            return Err(s3s::s3_error!(
                InternalError,
                "copy source range is longer than announced"
            ));
        }
        buf.extend_from_slice(&chunk);
    }
    if buf.len() as u64 != len {
        return Err(s3s::s3_error!(
            InternalError,
            "copy source range is shorter than announced"
        ));
    }
    Ok(buf.freeze())
}

pub(super) fn parse_copy_range(range: &str, len: usize) -> s3s::S3Result<(usize, usize)> {
    let range = range
        .strip_prefix("bytes=")
        .ok_or_else(|| s3s::s3_error!(InvalidArgument, "invalid copy-source-range"))?;
    let (start, end) = range
        .split_once('-')
        .ok_or_else(|| s3s::s3_error!(InvalidArgument, "invalid copy-source-range"))?;
    let start: usize = start
        .parse()
        .map_err(|_| s3s::s3_error!(InvalidArgument, "invalid copy-source-range"))?;
    let end: usize = end
        .parse()
        .map_err(|_| s3s::s3_error!(InvalidArgument, "invalid copy-source-range"))?;
    if start > end || end >= len {
        return Err(s3s::s3_error!(InvalidRange));
    }
    Ok((start, end))
}

#[cfg(test)]
mod large_copy_tests {
    use super::super::multipart::adapter_test_support::{request, service};
    use super::*;
    use crate::deltaglider::{DeltaGliderEngine, DynEngine};
    use crate::storage::DynStorageBackend;

    fn copy(dst_key: &str) -> s3s::dto::CopyObjectInput {
        s3s::dto::CopyObjectInput::builder()
            .bucket("dst".into())
            .key(dst_key.into())
            .copy_source(s3s::dto::CopySource::Bucket {
                bucket: "src".into(),
                key: "a/photo.jpg".into(),
                version_id: None,
            })
            .build()
            .unwrap()
    }

    /// CopyObject of a passthrough source larger than `max_object_size`
    /// streams through the spool, as a passthrough store takes objects up to
    /// `max_passthrough_object_size` (S3 copies up to 5 GiB). It answered
    /// EntityTooLarge, so rclone and mc copies of large objects failed. A
    /// destination that tries a delta keeps the delta limit.
    #[tokio::test]
    async fn a_large_passthrough_source_is_copied() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let engine_with = |config: crate::config::Config| {
            let root = root.clone();
            async move {
                let backend: Box<DynStorageBackend<'static>> = DynStorageBackend::new_box(
                    crate::storage::FilesystemBackend::new(root).await.unwrap(),
                );
                let engine: DynEngine =
                    DeltaGliderEngine::new_with_backend(Arc::new(backend), &config, None);
                engine
            }
        };
        let writer = engine_with(crate::config::Config::default()).await;
        for b in ["src", "dst"] {
            writer.create_bucket(b).await.unwrap();
        }
        let body: Vec<u8> = (0..300 * 1024u32).map(|i| (i * 7 % 253) as u8).collect();
        writer
            .store("src", "a/photo.jpg", &body, None, Default::default())
            .await
            .unwrap();
        let small = crate::config::Config {
            max_object_size: 64 * 1024,
            ..Default::default()
        };
        let state = AppState::for_tests(engine_with(small).await);
        let svc = service(state.clone());
        copy_object(&svc, request(copy("b/photo.jpg")))
            .await
            .unwrap();
        let (copied, meta) = state
            .engine
            .load()
            .retrieve("dst", "b/photo.jpg")
            .await
            .unwrap();
        assert_eq!(copied, body);
        assert!(!meta.is_delta());
        // Into a key that tries a delta, the delta limit holds.
        let err = copy_object(&svc, request(copy("b/app.zip")))
            .await
            .unwrap_err();
        assert_eq!(err.code(), &s3s::S3ErrorCode::EntityTooLarge);
    }
}
