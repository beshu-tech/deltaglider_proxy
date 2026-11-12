// SPDX-License-Identifier: BUSL-1.1

//! Multipart upload verbs: create, upload part, abort, list parts, list
//! uploads, and the detached CompleteMultipartUpload pipeline.

use super::*;

pub(super) async fn create_multipart_upload(
    svc: &DeltaGliderS3Service,
    req: s3s::S3Request<s3s::dto::CreateMultipartUploadInput>,
) -> s3s::S3Result<s3s::S3Response<s3s::dto::CreateMultipartUploadOutput>> {
    let input = req.input;
    // Earliest reject: refusing at create saves the client uploading the
    // parts at all (403-before-404).
    crate::api::handlers::object_helpers::check_client_write_allowed(&svc.state, &input.bucket)
        .await?;
    check_user_metadata_size_s3s(input.metadata.as_ref())?;
    // The key rule of PutObject, at create: a refused key (`.dg` segment, `//`,
    // `.delta` name, folder marker) fails here, not after all parts.
    crate::types::ObjectKey::parse(&input.bucket, &input.key)
        .validate_data_ingest()
        .map_err(|e| {
            s3s::S3Error::from(crate::api::errors::S3Error::InvalidArgument(e.to_string()))
        })?;
    crate::api::handlers::ensure_bucket_exists(&svc.state, &input.bucket).await?;
    let upload_id = svc.state.multipart.create(
        &input.bucket,
        &input.key,
        input.content_type.clone(),
        input.metadata.unwrap_or_default(),
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

/// S3's limit on one request body that is not a PutObject: a part
/// (UploadPart, UploadPartCopy) and the source of a CopyObject.
pub(super) const S3_MAX_SINGLE_TRANSFER_BYTES: u64 = 5 * 1024 * 1024 * 1024;

/// Pure: the largest part, S3's 5 GiB, and never above `max_object_size`
/// (no object may be larger). A part above it is `EntityTooLarge`, from
/// its declared length, before a byte is read.
pub(super) fn part_size_limit(max_object_size: u64) -> u64 {
    S3_MAX_SINGLE_TRANSFER_BYTES.min(max_object_size)
}

pub(super) async fn upload_part(
    svc: &DeltaGliderS3Service,
    req: s3s::S3Request<s3s::dto::UploadPartInput>,
) -> s3s::S3Result<s3s::S3Response<s3s::dto::UploadPartOutput>> {
    let body_ctx = crate::api::body_ingest::BodyContext::of(&req);
    let input = req.input;
    // Cached briefly per bucket: one HeadBucket per upload, not per part.
    crate::api::handlers::ensure_bucket_exists(&svc.state, &input.bucket).await?;
    let max_object_size = svc.state.engine.load().max_object_size();
    // Review C9: the store's per-upload cap follows the live config.
    svc.state.multipart.set_max_object_size(max_object_size);
    let part_number = u32::try_from(input.part_number).unwrap_or(0);
    // The part goes straight to a spool file, read once and checked, and
    // the multipart store keeps that file as the part.
    let (body, admission) = crate::api::body_ingest::ingest_part_body(
        &svc.state,
        input.body,
        &body_ctx,
        crate::api::body_ingest::PartTarget {
            upload_id: &input.upload_id,
            bucket: &input.bucket,
            key: &input.key,
            part_number,
        },
        part_size_limit(max_object_size),
        input.content_md5.as_deref(),
    )
    .await?;
    let etag = svc.state.multipart.upload_part(
        &input.upload_id,
        &input.bucket,
        &input.key,
        part_number,
        body,
        admission,
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

/// Pure: an integer argument of a multipart LIST (`max-parts`,
/// `part-number-marker`, `max-uploads`), `default` when absent. A negative
/// value is 400 InvalidArgument, as on S3 and MinIO: a clamp gave the
/// client a page it did not ask for. The LIST twin is `client_max_keys`
/// (its 0 means an empty page, so it is not shared).
fn non_negative_arg(value: Option<i32>, default: i32, name: &str) -> s3s::S3Result<u32> {
    let value = value.unwrap_or(default);
    u32::try_from(value).map_err(|_| {
        s3s::s3_error!(
            InvalidArgument,
            "Argument {} must be an integer between 0 and 2147483647",
            name
        )
    })
}

pub(super) async fn list_parts(
    svc: &DeltaGliderS3Service,
    req: s3s::S3Request<s3s::dto::ListPartsInput>,
) -> s3s::S3Result<s3s::S3Response<s3s::dto::ListPartsOutput>> {
    let input = req.input;
    let max_parts = non_negative_arg(input.max_parts, 1000, "max-parts")?.clamp(1, 1000);
    let marker = non_negative_arg(input.part_number_marker, 0, "part-number-marker")?;
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
    let writer = Reader::for_object(&req.extensions, &req.input.bucket, &req.input.key);
    let extensions = req.extensions;
    let input = req.input;
    // Defense-in-depth: the marker can be hot-applied mid-upload, so
    // Complete (the moment bytes reach the backend) re-checks.
    crate::api::handlers::object_helpers::check_client_write_allowed(&svc.state, &input.bucket)
        .await?;
    crate::api::handlers::ensure_bucket_exists(&svc.state, &input.bucket).await?;
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
         stored: Option<&crate::types::StoreResult>|
         -> s3s::S3Result<s3s::S3Response<s3s::dto::CompleteMultipartUploadOutput>> {
            let location = format!("/{}/{}", input.bucket, input.key);
            let mut resp = s3s::S3Response::new(s3s::dto::CompleteMultipartUploadOutput {
                bucket: Some(input.bucket.clone()),
                key: Some(input.key.clone()),
                e_tag: Some(parse_s3s_etag(etag)?),
                location: Some(location),
                ..Default::default()
            });
            // The storage facts are known only to the request that ran the
            // store; a joined retry or a tombstone hit answers without them.
            if let Some(stored) = stored {
                add_write_fact_headers(&mut resp.headers, stored, writer);
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
            // The slot of the detached store (F56) BEFORE the object lock:
            // a detached DeleteObjects batch holds a slot while it takes
            // each key's object lock, so holding this key's lock while
            // waiting for a slot deadlocked with it. A Complete that the
            // quota or a precondition then refuses waited for the slot for
            // nothing: acceptable, the wait is bounded by its deadline.
            let slot = crate::api::handlers::DetachedWork::reserve(&extensions).await;
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
            .await;
            // A refusal before the store (quota, precondition) is the
            // owner's answer: publish it, so a joined retry gets the same
            // status, not an internal error from the dropped publisher.
            let write_lock = match write_lock {
                Ok(lock) => lock,
                Err(refusal) => {
                    let refusal = s3s::S3Error::from(refusal);
                    publisher.publish(Err(crate::multipart::joiner_failure(Some(&refusal))));
                    return Err(refusal);
                }
            };
            let user_metadata = svc
                .state
                .multipart
                .user_metadata(&input.upload_id)
                .unwrap_or_default();
            let stores_whole = completion_stores_from_parts(
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
            // The detached store is a write of the bucket until it ends, also
            // after the request is gone (client disconnect, request timeout):
            // the maintenance drain (`drain_inflight_writes`) must wait for it.
            let gate_write = svc
                .state
                .maintenance_gate
                .begin_detached_write(&input.bucket);
            let handle = slot.spawn_detached(async move {
                let _gate_write = gate_write;
                let result = run_multipart_completion(
                    &write_lock,
                    state,
                    bucket,
                    key,
                    upload_id,
                    parts,
                    stores_whole,
                )
                .await
                .map_err(s3s::S3Error::from);
                match &result {
                    Ok((etag, _)) => publisher.publish(Ok(etag.clone())),
                    Err(e) => publisher.publish(Err(crate::multipart::joiner_failure(Some(e)))),
                }
                result
            });
            match handle.await {
                Ok(Ok((etag, stored))) => complete_response(&etag, Some(&stored)),
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
/// joined into one file: the write tries no delta, or it is larger than
/// `DGP_MPU_DELTA_RECONSTRUCT_MAX_BYTES`.
pub(super) fn completion_stores_from_parts(
    engine: &crate::deltaglider::DynEngine,
    bucket: &str,
    key: &str,
    user_metadata: &std::collections::HashMap<String, String>,
    total_parts_size: u64,
) -> bool {
    !engine.write_tries_delta(bucket, key, user_metadata, total_parts_size)
        || total_parts_size > engine.tuning().mpu_delta_reconstruct_max_bytes
}

pub(super) async fn list_multipart_uploads(
    svc: &DeltaGliderS3Service,
    req: s3s::S3Request<s3s::dto::ListMultipartUploadsInput>,
) -> s3s::S3Result<s3s::S3Response<s3s::dto::ListMultipartUploadsOutput>> {
    let list_scope = req.extensions.get::<ListScope>().cloned();
    let input = req.input;
    crate::api::handlers::ensure_bucket_exists(&svc.state, &input.bucket).await?;
    // `max-uploads=0` lists no upload and says 0, as `max-keys=0` does.
    let max_uploads = non_negative_arg(input.max_uploads, 1000, "max-uploads")?.min(1000);
    let enc = ListKeyEncoding::of(input.encoding_type.as_ref());
    // A bucket-level LIST like ListObjects: a Filtered caller sees only the
    // uploads of keys it may list.
    let visible = |key: &str| match &list_scope {
        Some(ListScope::Filtered { user, context }) => {
            crate::iam::permissions::user_can_see_listed_key(user, &input.bucket, key, context)
        }
        _ => true,
    };
    let (uploads, is_truncated, next_key, next_upload_id) = if max_uploads == 0 {
        (Vec::new(), false, String::new(), String::new())
    } else {
        svc.state.multipart.list_uploads_paginated(
            Some(&input.bucket),
            input.prefix.as_deref(),
            input.key_marker.as_deref().unwrap_or(""),
            input.upload_id_marker.as_deref().unwrap_or(""),
            max_uploads,
            &visible,
        )
    };
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
/// `write_lock`: the object's write lock, held until the store ends.
pub(super) async fn run_multipart_completion(
    write_lock: &crate::deltaglider::ObjectWriteGuard,
    state: Arc<AppState>,
    bucket: String,
    key: String,
    upload_id: String,
    requested_parts: Vec<(u32, String)>,
    stores_whole: bool,
) -> Result<(String, crate::types::StoreResult), crate::api::S3Error> {
    // Deterministic chaos hook for tests: hold the store window open.
    let stall_ms = crate::config::test_seams::test_seams().complete_stall_ms;
    if stall_ms > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(stall_ms)).await;
    }
    let engine = state.engine.load();
    let completed = state
        .multipart
        .complete_parts(&upload_id, &bucket, &key, &requested_parts)?;
    let etag = completed.etag.clone();
    // Protect the relay part files from the idle-TTL sweeper while the store
    // reads them (H18): a slow remote store can exceed completing_timeout.
    let store_guard = state.multipart.store_guard(&upload_id);
    // Armed from here: a pre-commit error, panic or drop rolls the upload
    // back; once the write committed, an exit completes it instead.
    let committed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let completing = state
        .multipart
        .completing_guard(&upload_id, &etag, committed.clone());
    let store = async {
        if stores_whole {
            engine
                .store_passthrough_relayed_parts_with_multipart_etag(
                    write_lock,
                    &bucket,
                    &key,
                    &completed.parts,
                    completed.total_size,
                    completed.content_type,
                    completed.user_metadata,
                    etag.clone(),
                )
                .await
        } else {
            // A delta candidate: the parts are joined into one spool file and
            // delta-encoded as a stream, whatever the size of the upload.
            engine
                .store_relayed_parts_delta_with_multipart_etag(
                    write_lock,
                    &bucket,
                    &key,
                    &completed.parts,
                    completed.total_size,
                    completed.content_type,
                    completed.user_metadata,
                    etag.clone(),
                )
                .await
        }
    };
    let stored = crate::deltaglider::commit_signal::watching(committed, store).await;
    drop(store_guard);
    let result = match stored {
        Ok(result) => result,
        Err(e) => return Err(e.into()),
    };
    state.multipart.finish_upload(&upload_id);
    completing.finish();
    crate::api::handlers::object_helpers::store_client_multipart_commit(
        &state, &bucket, &key, &result,
    )
    .await;
    Ok((etag, result))
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

#[cfg(test)]
pub(super) mod adapter_test_support {
    use super::*;

    /// An s3s request around `input`, as the router hands it over.
    pub(in crate::s3_adapter_s3s) fn request<T>(input: T) -> s3s::S3Request<T> {
        s3s::S3Request {
            input,
            method: axum::http::Method::PUT,
            uri: "/".parse().unwrap(),
            headers: Default::default(),
            extensions: Default::default(),
            credentials: None,
            region: None,
            service: None,
            trailing_headers: None,
        }
    }

    /// The adapter over `state`, with the default config.
    pub(in crate::s3_adapter_s3s) fn service(state: Arc<AppState>) -> DeltaGliderS3Service {
        DeltaGliderS3Service::new(
            state,
            Arc::new(tokio::sync::RwLock::new(crate::config::Config::default())),
        )
    }
}

#[cfg(test)]
mod multipart_cost_tests {
    use super::adapter_test_support::{request, service};
    use super::*;

    fn part(upload_id: &str, n: i32) -> s3s::dto::UploadPartInput {
        s3s::dto::UploadPartInput::builder()
            .bucket("b".into())
            .key("a/photo.jpg".into())
            .upload_id(upload_id.into())
            .part_number(n)
            .body(Some(s3s::dto::StreamingBlob::from(s3s::Body::from(
                vec![n as u8; 1024],
            ))))
            .build()
            .unwrap()
    }

    /// A negative `max-parts`, `part-number-marker` or `max-uploads` is
    /// 400 InvalidArgument, as on S3 and MinIO. It was clamped to 1 (or cast
    /// to a huge marker), so the client got a page it did not ask for.
    #[tokio::test]
    async fn a_negative_page_argument_is_invalid() {
        let (engine, _fake) = crate::deltaglider::s3_engine().await;
        let state = AppState::for_tests(engine);
        let svc = service(state.clone());
        let id = state
            .multipart
            .create("b", "a/photo.jpg", None, Default::default())
            .unwrap();
        let parts = |max: Option<i32>, marker: Option<i32>| {
            s3s::dto::ListPartsInput::builder()
                .bucket("b".into())
                .key("a/photo.jpg".into())
                .upload_id(id.clone())
                .max_parts(max)
                .part_number_marker(marker)
                .build()
                .unwrap()
        };
        for (max, marker) in [(Some(-1), None), (Some(i32::MIN), None), (None, Some(-1))] {
            let err = list_parts(&svc, request(parts(max, marker)))
                .await
                .expect_err("a negative argument is refused");
            assert_eq!(
                *err.code(),
                s3s::S3ErrorCode::InvalidArgument,
                "{max:?} {marker:?}"
            );
        }
        list_parts(&svc, request(parts(Some(0), Some(0))))
            .await
            .expect("zero is a valid value");
        let uploads = s3s::dto::ListMultipartUploadsInput::builder()
            .bucket("b".into())
            .max_uploads(Some(-1))
            .build()
            .unwrap();
        let err = list_multipart_uploads(&svc, request(uploads))
            .await
            .expect_err("a negative max-uploads is refused");
        assert_eq!(*err.code(), s3s::S3ErrorCode::InvalidArgument);
    }

    /// UploadPart checks its bucket with one HeadBucket per bucket per short
    /// interval: a 1,000-part upload sent 1,000.
    #[tokio::test]
    async fn upload_parts_check_their_bucket_once() {
        let (engine, fake) = crate::deltaglider::s3_engine().await;
        let state = AppState::for_tests(engine);
        let svc = service(state.clone());
        let id = state
            .multipart
            .create("b", "a/photo.jpg", None, Default::default())
            .unwrap();
        fake.clear();
        for n in 1..=3 {
            upload_part(&svc, request(part(&id, n))).await.unwrap();
        }
        let head_buckets = fake
            .requests()
            .iter()
            .filter(|r| *r == "HEAD /b/" || r.starts_with("HEAD /b/?"))
            .count();
        assert_eq!(head_buckets, 1, "{:?}", fake.requests());
    }

    /// The precondition of a Complete is the OWNER's: a retry that joined
    /// it matched on bucket, key and parts only, so it must not get the
    /// owner's 412. It gets a retryable 503, becomes the owner on its retry,
    /// and gets its own answer: a Complete without the precondition
    /// succeeds, one with the same precondition gets its own 412.
    #[tokio::test]
    async fn a_joined_complete_never_gets_the_preconditions_of_its_owner() {
        let (engine, _fake) = crate::deltaglider::s3_engine().await;
        engine
            .store(
                "b",
                "a/photo.jpg",
                b"already here",
                None,
                Default::default(),
            )
            .await
            .unwrap();
        let state = AppState::for_tests(engine);
        let svc = Arc::new(service(state.clone()));
        let id = state
            .multipart
            .create("b", "a/photo.jpg", None, Default::default())
            .unwrap();
        let etag = upload_part(&svc, request(part(&id, 1)))
            .await
            .unwrap()
            .output
            .e_tag
            .unwrap();
        let input = |create_only: bool| {
            s3s::dto::CompleteMultipartUploadInput::builder()
                .bucket("b".into())
                .key("a/photo.jpg".into())
                .upload_id(id.clone())
                .if_none_match(create_only.then_some(s3s::dto::ETagCondition::Any))
                .multipart_upload(Some(s3s::dto::CompletedMultipartUpload {
                    parts: Some(vec![s3s::dto::CompletedPart {
                        part_number: Some(1),
                        e_tag: Some(etag.clone()),
                        ..Default::default()
                    }]),
                }))
                .build()
                .unwrap()
        };
        // The object's lock is held: the owner waits in its admission.
        let lock = state
            .engine
            .load()
            .lock_object_write("b", "a/photo.jpg")
            .await;
        let owner = tokio::spawn({
            let (svc, input) = (svc.clone(), input(true));
            async move { complete_multipart_upload(&svc, request(input)).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let joiner = tokio::spawn({
            let (svc, input) = (svc.clone(), input(false));
            async move { complete_multipart_upload(&svc, request(input)).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        drop(lock);
        let owner = owner.await.unwrap().expect_err("the precondition refuses");
        assert_eq!(*owner.code(), s3s::S3ErrorCode::PreconditionFailed);
        let joiner = joiner
            .await
            .unwrap()
            .expect_err("the joiner is told to retry");
        assert_eq!(*joiner.code(), s3s::S3ErrorCode::SlowDown, "{joiner:?}");
        assert_eq!(
            joiner.status_code(),
            Some(axum::http::StatusCode::SERVICE_UNAVAILABLE)
        );

        // Its retry is a fresh owner: with the same precondition it gets
        // its own 412; without it, the Complete succeeds.
        let again = complete_multipart_upload(&svc, request(input(true)))
            .await
            .expect_err("the same precondition refuses again");
        assert_eq!(*again.code(), s3s::S3ErrorCode::PreconditionFailed);
        complete_multipart_upload(&svc, request(input(false)))
            .await
            .expect("without the precondition the retry completes");
    }

    /// A CompleteMultipartUpload whose request is gone (client disconnect,
    /// request timeout) still counts as an in-flight write of its bucket
    /// while its detached store runs: the maintenance drain waits for it.
    /// It counted as none once the request future was dropped.
    #[tokio::test]
    async fn a_detached_completion_is_an_inflight_write() {
        let (engine, fake) = crate::deltaglider::s3_engine().await;
        let state = AppState::for_tests(engine);
        let svc = service(state.clone());
        let id = state
            .multipart
            .create("b", "a/photo.jpg", None, Default::default())
            .unwrap();
        let etag = upload_part(&svc, request(part(&id, 1)))
            .await
            .unwrap()
            .output
            .e_tag
            .unwrap();
        let input = s3s::dto::CompleteMultipartUploadInput::builder()
            .bucket("b".into())
            .key("a/photo.jpg".into())
            .upload_id(id)
            .multipart_upload(Some(s3s::dto::CompletedMultipartUpload {
                parts: Some(vec![s3s::dto::CompletedPart {
                    part_number: Some(1),
                    e_tag: Some(etag),
                    ..Default::default()
                }]),
            }))
            .build()
            .unwrap();
        // Cut while the object's PUT is in flight; it stays in flight long
        // enough for the next assertion.
        fake.set_delay_ms("PUT", 2_000);
        let cut = fake
            .cut_at(complete_multipart_upload(&svc, request(input)), |r| {
                r.starts_with("PUT /b/a/photo.jpg")
            })
            .await;
        assert!(cut, "the store ended before the request was cut");
        assert_eq!(state.maintenance_gate.inflight_writes("b"), 1);
        // No request deadline bounds it: a drain counts it as background
        // (a timeout on it defers the job, never fails it).
        assert_eq!(state.maintenance_gate.inflight_background_writes("b"), 1);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while state.maintenance_gate.inflight_writes("b") > 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "the write never ended"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(fake
            .requests()
            .iter()
            .any(|r| r.starts_with("PUT /b/a/photo.jpg")));
    }

    /// F10 (epic #143): parallel UploadParts of 24 MiB each (and the
    /// Completes that read them back) never hold the `uploads` lock across
    /// file I/O, and never stall the async workers: the part body streams
    /// into its spool file before the lock, the lock covers the map only.
    /// A thread probes the lock the whole time (ListMultipartUploads and
    /// every other multipart request take it), a task ticks on the runtime.
    /// Measured at 69690c8d (part written and fsynced under the lock): 557
    /// ms timer gap for 6 parts, 2.8 s ListMultipartUploads stall for 100
    /// uploads of 64 MB. The source guard of the same rule is
    /// `the_uploads_lock_is_never_held_for_file_io`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn parallel_parts_never_hold_the_lock_or_the_reactor() {
        const PART: usize = 24 * 1024 * 1024;
        const CHUNK: usize = 64 * 1024;
        const PARTS: i32 = 6;
        let (engine, _fake) = crate::deltaglider::s3_engine().await;
        let state = AppState::for_tests(engine);
        let svc = Arc::new(service(state.clone()));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let probe = {
            let (state, stop) = (state.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut worst = std::time::Duration::ZERO;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let t = std::time::Instant::now();
                    let _ = state.multipart.count_uploads();
                    worst = worst.max(t.elapsed());
                    std::thread::sleep(std::time::Duration::from_micros(200));
                }
                worst
            })
        };
        let gap = Arc::new(parking_lot::Mutex::new(std::time::Duration::ZERO));
        let ticker = {
            let (stop, gap) = (stop.clone(), gap.clone());
            tokio::spawn(async move {
                let mut last = std::time::Instant::now();
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                    let g = last.elapsed();
                    let mut w = gap.lock();
                    *w = (*w).max(g);
                    last = std::time::Instant::now();
                }
            })
        };
        // One part per upload: six uploads at once, then six Completes.
        let uploads: Vec<_> = (1..=PARTS)
            .map(|n| {
                let (svc, state) = (svc.clone(), state.clone());
                tokio::spawn(async move {
                    let key = format!("big-{n}.bin");
                    let id = state
                        .multipart
                        .create("b", &key, None, Default::default())
                        .unwrap();
                    let input = s3s::dto::UploadPartInput::builder()
                        .bucket("b".into())
                        .key(key.clone())
                        .upload_id(id.clone())
                        .part_number(1)
                        // 64 KiB frames, as a client's body arrives.
                        .body(Some(s3s::dto::StreamingBlob::wrap(futures::stream::iter(
                            (0..PART / CHUNK).map(move |_| {
                                Ok::<_, std::io::Error>(bytes::Bytes::from(vec![n as u8; CHUNK]))
                            }),
                        ))))
                        .build()
                        .unwrap();
                    let mut req = request(input);
                    req.headers
                        .insert(axum::http::header::CONTENT_LENGTH, PART.into());
                    let etag = upload_part(&svc, req).await.unwrap().output.e_tag.unwrap();
                    (key, id, etag)
                })
            })
            .collect();
        let mut done = Vec::new();
        for u in uploads {
            done.push(u.await.unwrap());
        }
        let parts_gap = std::mem::take(&mut *gap.lock());
        let mut completes = Vec::new();
        for (key, id, etag) in done {
            let svc = svc.clone();
            completes.push(tokio::spawn(async move {
                let input = s3s::dto::CompleteMultipartUploadInput::builder()
                    .bucket("b".into())
                    .key(key)
                    .upload_id(id)
                    .multipart_upload(Some(s3s::dto::CompletedMultipartUpload {
                        parts: Some(vec![s3s::dto::CompletedPart {
                            part_number: Some(1),
                            e_tag: Some(etag),
                            ..Default::default()
                        }]),
                    }))
                    .build()
                    .unwrap();
                complete_multipart_upload(&svc, request(input))
                    .await
                    .unwrap();
            }));
        }
        for c in completes {
            c.await.unwrap();
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let lock_worst = probe.join().unwrap();
        ticker.await.unwrap();
        let tick_worst = parts_gap.max(*gap.lock());
        eprintln!(
            "F10: worst uploads-lock wait {lock_worst:?}, worst runtime tick gap: \
             UploadPart {parts_gap:?}, Complete {:?}",
            *gap.lock()
        );
        assert!(
            lock_worst < std::time::Duration::from_millis(100),
            "the uploads lock was held {lock_worst:?} (file I/O under the lock)"
        );
        // A debug build hashes ~20x slower (MD5 + SHA-256 of each body
        // chunk on the worker), so only an optimised build judges the
        // runtime ticks; the lock bound above holds in both.
        if !cfg!(debug_assertions) {
            assert!(
                tick_worst < std::time::Duration::from_millis(250),
                "a runtime worker stalled {tick_worst:?} (blocking I/O on the reactor)"
            );
        }
    }

    /// Review of #166: CompleteMultipartUpload took the key's object lock
    /// and THEN waited for a detached slot, while a detached DeleteObjects
    /// batch held the only slot and waited for that key's object lock: both
    /// hung. The Complete reserves its slot before the lock now.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_complete_and_a_delete_batch_share_one_slot_without_deadlock() {
        let (engine, fake) = crate::deltaglider::s3_engine().await;
        engine
            .store("b", "a/k0.bin", b"x", None, Default::default())
            .await
            .unwrap();
        let state = AppState::for_tests(engine);
        let svc = Arc::new(service(state.clone()));
        let work = crate::api::handlers::DetachedWork::new(1);
        fn with<T>(work: &crate::api::handlers::DetachedWork, input: T) -> s3s::S3Request<T> {
            let mut req = request(input);
            req.extensions.insert(work.clone());
            req
        }
        let id = state
            .multipart
            .create("b", "a/photo.jpg", None, Default::default())
            .unwrap();
        let etag = upload_part(&svc, request(part(&id, 1)))
            .await
            .unwrap()
            .output
            .e_tag
            .unwrap();
        // The batch takes the slot and stalls on its first key; its second
        // key is the upload's.
        fake.stall("DELETE", "b/a/k0");
        let delete = s3s::dto::DeleteObjectsInput::builder()
            .bucket("b".into())
            .delete(s3s::dto::Delete {
                objects: ["a/k0.bin", "a/photo.jpg"]
                    .into_iter()
                    .map(|k| s3s::dto::ObjectIdentifier {
                        key: k.into(),
                        ..Default::default()
                    })
                    .collect(),
                quiet: None,
            })
            .build()
            .unwrap();
        let batch = {
            let (svc, req) = (svc.clone(), with(&work, delete));
            tokio::spawn(async move { super::super::object::delete_objects(&svc, req).await })
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !fake
            .requests()
            .iter()
            .any(|r| r.starts_with("DELETE /b/a/k0"))
        {
            assert!(std::time::Instant::now() < deadline, "the batch never sent");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let input = s3s::dto::CompleteMultipartUploadInput::builder()
            .bucket("b".into())
            .key("a/photo.jpg".into())
            .upload_id(id)
            .multipart_upload(Some(s3s::dto::CompletedMultipartUpload {
                parts: Some(vec![s3s::dto::CompletedPart {
                    part_number: Some(1),
                    e_tag: Some(etag),
                    ..Default::default()
                }]),
            }))
            .build()
            .unwrap();
        let complete = {
            let (svc, req) = (svc.clone(), with(&work, input));
            tokio::spawn(async move { complete_multipart_upload(&svc, req).await })
        };
        // Let the Complete reach its wait (for the slot, or with the bug,
        // for the slot while it holds the key's lock).
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        fake.release_stalls();
        let both = async { (batch.await.unwrap(), complete.await.unwrap()) };
        let (deleted, completed) = tokio::time::timeout(std::time::Duration::from_secs(10), both)
            .await
            .expect("the Complete and the delete batch deadlocked on the slot and the object lock");
        deleted.unwrap();
        completed.unwrap();
    }
}
