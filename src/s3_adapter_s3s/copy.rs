// SPDX-License-Identifier: BUSL-1.1

//! Server-side copy verbs: CopyObject and UploadPartCopy, with the copy
//! source parser, its access check and its conditionals.

use super::*;

pub(super) async fn copy_object(
    svc: &DeltaGliderS3Service,
    req: s3s::S3Request<s3s::dto::CopyObjectInput>,
) -> s3s::S3Result<s3s::S3Response<s3s::dto::CopyObjectOutput>> {
    let auth_user = crate::iam::request_principal(&req.extensions).cloned();
    let writer = Reader::for_object(&req.extensions, &req.input.bucket, &req.input.key);
    let input = req.input;
    let (source_bucket, source_key) = copy_source_bucket_key(&input.copy_source)?;
    let client_ip = req
        .extensions
        .get::<crate::api::auth::RequestClientIp>()
        .map(|c| c.0);
    check_copy_source_access_s3s(
        svc.state.engine.load().bucket_policy_registry(),
        auth_user.as_deref(),
        &source_bucket,
        &source_key,
        client_ip,
        &req.headers,
    )?;
    // The destination's `If-Match` / `If-None-Match`: s3s models them for
    // no copy request, so they come from the raw headers (A1, #171).
    let precondition = destination_precondition(&req.headers)?;
    // Gate on the DESTINATION bucket first — copy-into is a client write,
    // refused regardless of backend reachability (403-before-404).
    crate::api::handlers::object_helpers::check_client_write_allowed(&svc.state, &input.bucket)
        .await?;
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
    // The destination is written: the write paths' brief bucket cache.
    crate::api::handlers::ensure_bucket_exists(&svc.state, &input.bucket).await?;
    let engine = svc.state.engine.load();
    // No HeadBucket for the source: its HEAD answers NoSuchKey for a
    // missing bucket too, and only then the cached check tells
    // NoSuchBucket (as a read miss does).
    let source_meta = match engine.head(&source_bucket, &source_key).await {
        Ok(meta) => meta,
        Err(e) => {
            let err = s3s::S3Error::from(e);
            return Err(no_such_key_or_bucket(&svc.state, &source_bucket, err).await);
        }
    };
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
    if source_meta.file_size > copy_size_limit(&engine) {
        return Err(s3s::s3_error!(EntityTooLarge));
    }
    // The stored object moves as it is when the destination stores it in
    // the same form: a delta into a key that stores deltas (the `.delta`
    // ships, no reconstruction, no re-encode), a passthrough object into a
    // key that does not (the backend copies it server-side). Otherwise, or
    // when that is not possible, the copy goes through the store.
    let tries_delta = engine.write_tries_delta(
        &input.bucket,
        &input.key,
        &dest_metadata,
        source_meta.file_size,
    );
    let as_stored = source_meta.is_delta() == tries_delta
        && (source_meta.is_delta()
            || matches!(
                source_meta.storage_info,
                crate::types::StorageInfo::Passthrough
            ));
    let fast = if as_stored {
        let (content_type, user_metadata) = copy_destination_fields(
            directive,
            input.content_type.clone(),
            input.metadata.clone(),
            &source_meta,
        )?;
        let copy = crate::api::handlers::object_helpers::ClientStoredCopy {
            source: (&source_bucket, &source_key),
            bucket: &input.bucket,
            key: &input.key,
            source_head: &source_meta,
            content_type,
            user_metadata,
            precondition: &precondition,
        };
        if tries_delta {
            crate::api::handlers::object_helpers::store_client_delta_copy(&svc.state, copy).await?
        } else {
            crate::api::handlers::object_helpers::store_client_server_copy(&svc.state, copy).await?
        }
    } else {
        None
    };
    let result = match fast {
        Some(result) => result,
        None => {
            copy_through_store(
                svc,
                &engine,
                (&source_bucket, &source_key),
                &input,
                directive,
                &source_meta,
                &precondition,
            )
            .await?
        }
    };
    let mut resp = s3s::S3Response::new(s3s::dto::CopyObjectOutput {
        copy_object_result: Some(s3s::dto::CopyObjectResult {
            e_tag: Some(parse_s3s_etag(&result.metadata.etag())?),
            last_modified: Some(SystemTime::from(result.metadata.created_at).into()),
            ..Default::default()
        }),
        ..Default::default()
    });
    add_write_fact_headers(&mut resp.headers, &result, writer);
    Ok(resp)
}

/// A CopyObject through the store: read (and reconstruct) the source, then
/// store it as a client write of the destination.
async fn copy_through_store(
    svc: &DeltaGliderS3Service,
    engine: &crate::deltaglider::DynEngine,
    (source_bucket, source_key): (&str, &str),
    input: &s3s::dto::CopyObjectInput,
    directive: &str,
    source_meta: &FileMetadata,
    precondition: &crate::deltaglider::Precondition,
) -> s3s::S3Result<crate::types::StoreResult> {
    let (body, metadata) = read_copy_source(
        engine,
        source_bucket,
        source_key,
        &input.bucket,
        source_meta.file_size,
    )
    .await?;
    let (content_type, user_metadata) = copy_destination_fields(
        directive,
        input.content_type.clone(),
        input.metadata.clone(),
        &metadata,
    )?;
    // A copy creates a new object at the destination: the same client
    // write as a PUT (quota on the destination, the object's write lock,
    // the ObjectCreated event), and the destination's conditional headers.
    Ok(crate::api::handlers::object_helpers::store_client_write(
        &svc.state,
        crate::api::handlers::object_helpers::ClientWrite {
            bucket: &input.bucket,
            key: &input.key,
            body,
            content_type,
            user_metadata,
            precondition,
            early_quota: None,
        },
    )
    .await?)
}

/// Pure: the write preconditions of a CopyObject destination, from the raw
/// request headers. S3 judges `If-Match` and `If-None-Match` of a CopyObject
/// against the destination key, as it does for a PutObject (`412
/// PreconditionFailed`, `404 NoSuchKey` for `If-Match` on a missing key).
/// s3s 0.14 reads only the `x-amz-copy-source-if-*` headers into the input,
/// so these two would be dropped silently. A header that does not parse is an
/// `InvalidArgument`, as for a PutObject.
pub(super) fn destination_precondition(
    headers: &axum::http::HeaderMap,
) -> s3s::S3Result<crate::deltaglider::Precondition> {
    let read = |name: &'static str| -> s3s::S3Result<Option<s3s::dto::ETagCondition>> {
        headers
            .get(name)
            .map(|value| {
                s3s::dto::ETagCondition::parse_http_header(value.as_bytes()).map_err(|_| {
                    s3s::s3_error!(
                        InvalidArgument,
                        "the {name} header is not a valid ETag condition"
                    )
                })
            })
            .transpose()
    };
    Ok(crate::deltaglider::Precondition {
        if_match: read("if-match")?,
        if_none_match: read("if-none-match")?,
    })
}

/// The content type and user metadata of a CopyObject destination: the
/// request's under `REPLACE`, the source's under `COPY`. The source's
/// encryption markers and rule provenance stamps never carry over: the
/// destination write stamps its own markers, and a client copy was not
/// written by a rule.
fn copy_destination_fields(
    directive: &str,
    content_type: Option<String>,
    metadata: Option<std::collections::HashMap<String, String>>,
    source: &FileMetadata,
) -> s3s::S3Result<(Option<String>, std::collections::HashMap<String, String>)> {
    let (content_type, mut user_metadata) = if directive.eq_ignore_ascii_case("REPLACE") {
        check_user_metadata_size_s3s(metadata.as_ref())?;
        (content_type, metadata.unwrap_or_default())
    } else if directive.eq_ignore_ascii_case("COPY") {
        (source.content_type.clone(), source.user_metadata.clone())
    } else {
        return Err(s3s::s3_error!(
            InvalidArgument,
            "metadata-directive must be COPY or REPLACE"
        ));
    };
    crate::storage::encrypting::strip_encryption_markers(&mut user_metadata);
    crate::transfer::strip_rule_provenance(&mut user_metadata);
    Ok((content_type, user_metadata))
}

/// The largest CopyObject source: S3's 5 GiB copy limit, capped at
/// `max_passthrough_object_size`. The source streams through a spool file,
/// so its size costs disk, not memory. A source above `max_object_size` is
/// stored whole (`StorePlan::decline_reason`), so the delta limit never refuses
/// a copy; with it, rclone and mc copies of larger objects failed with
/// EntityTooLarge.
pub(super) fn copy_size_limit(engine: &crate::deltaglider::DynEngine) -> u64 {
    super::multipart::S3_MAX_SINGLE_TRANSFER_BYTES.min(engine.max_passthrough_object_size())
}

/// Read a copy source through the same ingest as a PutObject body: up to
/// the spool threshold into memory (when the RAM budget has room), above it
/// into a spool file, so a CopyObject never holds a large object in memory.
/// `head_size` is the size the conditionals judged: the declared length of
/// the ingest. A source that changed since answers 503 SlowDown (the client
/// retries and the retry judges the new object). `dest_bucket`: where the
/// copy is stored (its storage writes are reserved with the body).
pub(super) async fn read_copy_source(
    engine: &crate::deltaglider::DynEngine,
    bucket: &str,
    key: &str,
    dest_bucket: &str,
    head_size: u64,
) -> s3s::S3Result<(crate::deltaglider::ingest::IngestedBody, FileMetadata)> {
    use crate::deltaglider::ingest::{ingest, Destination, IngestPlan};
    let (stream, metadata) = if head_size <= engine.spool_threshold() {
        // small: the HEAD size is at most the spool threshold.
        let (data, metadata) = engine.retrieve(bucket, key).await?;
        let one: BoxStream<'static, Result<bytes::Bytes, StorageError>> =
            futures::stream::iter([Ok(bytes::Bytes::from(data))]).boxed();
        (one, metadata)
    } else {
        match engine.retrieve_stream(bucket, key).await? {
            RetrieveResponse::Buffered { data, metadata, .. } => (
                futures::stream::iter([Ok(bytes::Bytes::from(data))]).boxed(),
                metadata,
            ),
            RetrieveResponse::Streamed {
                stream, metadata, ..
            } => (stream, metadata),
        }
    };
    let plan = IngestPlan {
        declared: head_size,
        limit: copy_size_limit(engine),
        sha256: crate::deltaglider::ingest::Sha256Need::Compute,
        // A large delta or reference source streams from a spool file that
        // the stream holds: the ingest of the copy holds that budget, so
        // it never waits for more (a wait there could wait on itself, or
        // on another copy that waits on it).
        held_spool_mib: engine.stream_spool_mib(&metadata),
        strict_length: true,
        destination: Destination::Object,
    };
    let body = ingest(engine, dest_bucket, stream, plan)
        .await
        .map_err(copy_source_ingest_failure)?;
    Ok((body, metadata))
}

/// The S3 answer to a failed ingest of a copy source: a source longer or
/// shorter than its HEAD said changed since (503 SlowDown: the client
/// retries and the retry judges the new object).
fn copy_source_ingest_failure(
    e: crate::deltaglider::ingest::IngestError<StorageError>,
) -> crate::api::S3Error {
    use crate::deltaglider::ingest::IngestError;
    match e {
        IngestError::TooLong { .. } | IngestError::Incomplete { .. } => {
            crate::api::S3Error::copy_source_changed()
        }
        e => crate::api::body_ingest::ingest_failure(e),
    }
}

pub(super) async fn upload_part_copy(
    svc: &DeltaGliderS3Service,
    req: s3s::S3Request<s3s::dto::UploadPartCopyInput>,
) -> s3s::S3Result<s3s::S3Response<s3s::dto::UploadPartCopyOutput>> {
    let auth_user = crate::iam::request_principal(&req.extensions).cloned();
    let input = req.input;
    let (source_bucket, source_key) = copy_source_bucket_key(&input.copy_source)?;
    let client_ip = req
        .extensions
        .get::<crate::api::auth::RequestClientIp>()
        .map(|c| c.0);
    check_copy_source_access_s3s(
        svc.state.engine.load().bucket_policy_registry(),
        auth_user.as_deref(),
        &source_bucket,
        &source_key,
        client_ip,
        &req.headers,
    )?;
    // Gate on the DESTINATION bucket — part-copy feeds a client write
    // (403-before-404, no backend I/O for a doomed request).
    crate::api::handlers::object_helpers::check_client_write_allowed(&svc.state, &input.bucket)
        .await?;
    // A part of the destination upload: the write paths' brief bucket cache.
    crate::api::handlers::ensure_bucket_exists(&svc.state, &input.bucket).await?;
    let engine = svc.state.engine.load();
    // No HeadBucket for the source: its HEAD answers NoSuchKey for a
    // missing bucket too, and only then the cached check tells
    // NoSuchBucket (as a read miss does).
    let source_meta = match engine.head(&source_bucket, &source_key).await {
        Ok(meta) => meta,
        Err(e) => {
            let err = s3s::S3Error::from(e);
            return Err(no_such_key_or_bucket(&svc.state, &source_bucket, err).await);
        }
    };
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
    let part_number = u32::try_from(input.part_number).unwrap_or(0);
    // A part is read as its range only, pinned to the generation the
    // conditionals judged: an SDK managed copy sends one UploadPartCopy per
    // part, so a whole-source read per part is N times the object. A part
    // with no range is the whole source as one range, so it reads the same
    // way. The part itself is bounded like an UploadPart.
    let len = usize::try_from(source_meta.file_size).unwrap_or(usize::MAX);
    let range = match input.copy_source_range.as_deref() {
        Some(range) => Some(parse_copy_range(range, len)?),
        None => len.checked_sub(1).map(|last| (0, last)),
    };
    let part_len = range.map_or(0, |(start, end)| (end - start) as u64 + 1);
    if part_len > super::multipart::part_size_limit(engine.max_object_size()) {
        return Err(s3s::s3_error!(EntityTooLarge));
    }
    // The upload admits the part before a byte of the source is read.
    let admission = svc.state.multipart.admit_part(
        &input.upload_id,
        &input.bucket,
        &input.key,
        part_number,
        part_len,
    )?;
    let ranged = match range {
        Some((start, end)) if source_meta.is_delta() => {
            // Every part of a delta source reads one verified
            // reconstruction (it rebuilt the whole object per part).
            let (stream, len) = engine
                .delta_copy_range(
                    &source_bucket,
                    &source_key,
                    &source_meta,
                    start as u64,
                    end as u64,
                )
                .await?;
            Some((stream, len))
        }
        Some((start, end)) => engine
            .retrieve_stream_range(
                &source_bucket,
                &source_key,
                start as u64,
                end as u64,
                Some(&source_meta),
            )
            .await?
            .map(|(stream, len, _)| (stream, len)),
        None => None,
    };
    let stream: BoxStream<'static, Result<bytes::Bytes, StorageError>> = match ranged {
        Some((stream, content_length)) => {
            if content_length != part_len {
                return Err(s3s::s3_error!(
                    InternalError,
                    "copy source range is not the length announced"
                ));
            }
            stream
        }
        None => {
            // What `retrieve_stream_range` does not serve as a range: an
            // empty source, a delta or reference object at most the spool
            // threshold, or a source gone or rewritten since its HEAD (a
            // retry judges the new object).
            if source_meta.file_size > engine.spool_threshold() {
                return Err(crate::api::S3Error::copy_source_changed().into());
            }
            // small: at most the spool threshold (see above).
            let (data, _) = engine.retrieve(&source_bucket, &source_key).await?;
            let part = match range {
                Some((start, end)) if end < data.len() => {
                    bytes::Bytes::copy_from_slice(&data[start..=end])
                }
                Some(_) => return Err(crate::api::S3Error::copy_source_changed().into()),
                None => bytes::Bytes::new(),
            };
            futures::stream::iter([Ok(part)]).boxed()
        }
    };
    // The same ingest as an UploadPart body: into a spool file, hashed on
    // the way (no Content-MD5 or checksum: the source is the proxy's own).
    let plan = crate::deltaglider::ingest::IngestPlan {
        declared: part_len,
        limit: super::multipart::part_size_limit(engine.max_object_size()),
        // No signed payload hash to check: only the part's MD5.
        sha256: crate::deltaglider::ingest::Sha256Need::decide(
            crate::deltaglider::ingest::Destination::MultipartPart,
            None,
            false,
        ),
        held_spool_mib: admission.held_spool_mib,
        strict_length: true,
        destination: crate::deltaglider::ingest::Destination::MultipartPart,
    };
    let part = crate::deltaglider::ingest::ingest(&**engine, &input.bucket, stream, plan)
        .await
        .map_err(copy_source_ingest_failure)?;
    let etag = svc.state.multipart.upload_part(
        &input.upload_id,
        &input.bucket,
        &input.key,
        part_number,
        part,
        admission,
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
#[path = "copy_cost_tests.rs"]
mod copy_cost_tests;

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
    /// delta-eligible destination stores a source above the delta limit
    /// whole.
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
        // Into a delta-eligible key: above the delta limit, stored whole.
        copy_object(&svc, request(copy("b/app.zip"))).await.unwrap();
        let meta = state.engine.load().head("dst", "b/app.zip").await.unwrap();
        assert!(!meta.is_delta());
    }
}

#[cfg(test)]
mod destination_precondition_tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> axum::http::HeaderMap {
        let mut h = axum::http::HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, v.parse().unwrap());
        }
        h
    }

    /// A1 (#171): the two destination headers, from the raw request.
    #[test]
    fn the_destination_headers_become_a_precondition() {
        let none = destination_precondition(&headers(&[])).unwrap();
        assert!(none.if_match.is_none() && none.if_none_match.is_none());
        let star = destination_precondition(&headers(&[("if-none-match", "*")])).unwrap();
        assert!(star.if_none_match.as_ref().unwrap().is_any() && star.if_match.is_none());
        let etag = destination_precondition(&headers(&[("if-match", "\"abc\"")])).unwrap();
        assert!(etag.if_match.is_some() && etag.if_none_match.is_none());
        // The copy SOURCE conditionals are another pair of headers.
        let src = destination_precondition(&headers(&[("x-amz-copy-source-if-match", "\"abc\"")]))
            .unwrap();
        assert!(src.if_match.is_none() && src.if_none_match.is_none());
        let e = destination_precondition(&headers(&[("if-match", "\"abc")])).unwrap_err();
        assert_eq!(*e.code(), s3s::S3ErrorCode::InvalidArgument);
    }
}
