// SPDX-License-Identifier: BUSL-1.1

//! Bucket verbs: HEAD/create/delete bucket, ListBuckets, and the bucket
//! ACL, location, versioning and tagging answers.

use super::*;

/// HeadBucket — `HEAD /<bucket>`
///
/// The s3s default returns `501 NotImplemented`, which broke the
/// `error_test::test_nosuchbucket_xml_response` and
/// `test_entitytoolarge_response` integration tests after the legacy
/// axum HEAD-bucket handler was retired in `2f8e483`. Real AWS / MinIO
/// return `200` when the bucket exists and `404 NoSuchBucket` when it
/// doesn't, so we mirror that contract via `ensure_bucket_exists_s3s`.
///
/// The `x-amz-bucket-region` header is conventionally returned on
/// HeadBucket; s3s emits it from `bucket_region` on the output, and
/// we hard-code `us-east-1` to match the legacy axum handler and the
/// `get_bucket_location` constraint.
pub(super) async fn head_bucket(
    svc: &DeltaGliderS3Service,
    req: s3s::S3Request<s3s::dto::HeadBucketInput>,
) -> s3s::S3Result<s3s::S3Response<s3s::dto::HeadBucketOutput>> {
    ensure_bucket_exists_s3s(&svc.state, &req.input.bucket).await?;
    Ok(s3s::S3Response::new(s3s::dto::HeadBucketOutput {
        bucket_region: Some("us-east-1".to_string()),
        ..Default::default()
    }))
}

pub(super) async fn list_buckets(
    svc: &DeltaGliderS3Service,
    req: s3s::S3Request<s3s::dto::ListBucketsInput>,
) -> s3s::S3Result<s3s::S3Response<s3s::dto::ListBucketsOutput>> {
    let auth_user = req.extensions.get::<AuthenticatedUser>().cloned();
    let input = req.input;
    let engine = svc.state.engine.load();
    let mut buckets = engine.list_buckets_with_dates().await?;
    // The coordination bucket is not a client bucket (see
    // `reserved_bucket_refusal`).
    let registry = engine.bucket_policy_registry();
    buckets.retain(|(name, _)| !registry.is_reserved(name));
    if let Some(user) = auth_user {
        buckets.retain(|(name, _)| user.can_see_bucket(name));
    }
    Ok(s3s::S3Response::new(list_buckets_output_from_rows(
        buckets,
        input.prefix.as_deref(),
        input.max_buckets,
        input.continuation_token.as_deref(),
    )))
}

pub(super) async fn get_bucket_acl(
    svc: &DeltaGliderS3Service,
    req: s3s::S3Request<s3s::dto::GetBucketAclInput>,
) -> s3s::S3Result<s3s::S3Response<s3s::dto::GetBucketAclOutput>> {
    ensure_bucket_exists_s3s(&svc.state, &req.input.bucket).await?;
    Ok(s3s::S3Response::new(s3s::dto::GetBucketAclOutput {
        owner: Some(default_acl_owner()),
        grants: Some(vec![default_full_control_grant()]),
    }))
}

pub(super) async fn get_bucket_location(
    svc: &DeltaGliderS3Service,
    req: s3s::S3Request<s3s::dto::GetBucketLocationInput>,
) -> s3s::S3Result<s3s::S3Response<s3s::dto::GetBucketLocationOutput>> {
    ensure_bucket_exists_s3s(&svc.state, &req.input.bucket).await?;
    Ok(s3s::S3Response::new(s3s::dto::GetBucketLocationOutput {
        location_constraint: None,
    }))
}

pub(super) async fn get_bucket_versioning(
    svc: &DeltaGliderS3Service,
    req: s3s::S3Request<s3s::dto::GetBucketVersioningInput>,
) -> s3s::S3Result<s3s::S3Response<s3s::dto::GetBucketVersioningOutput>> {
    ensure_bucket_exists_s3s(&svc.state, &req.input.bucket).await?;
    Ok(s3s::S3Response::new(
        s3s::dto::GetBucketVersioningOutput::default(),
    ))
}

pub(super) async fn get_bucket_tagging(
    svc: &DeltaGliderS3Service,
    req: s3s::S3Request<s3s::dto::GetBucketTaggingInput>,
) -> s3s::S3Result<s3s::S3Response<s3s::dto::GetBucketTaggingOutput>> {
    ensure_bucket_exists_s3s(&svc.state, &req.input.bucket).await?;
    Err(s3s::s3_error!(
        NotImplemented,
        "Bucket tagging is not supported by this proxy"
    ))
}

pub(super) async fn put_bucket_tagging(
    svc: &DeltaGliderS3Service,
    req: s3s::S3Request<s3s::dto::PutBucketTaggingInput>,
) -> s3s::S3Result<s3s::S3Response<s3s::dto::PutBucketTaggingOutput>> {
    ensure_bucket_exists_s3s(&svc.state, &req.input.bucket).await?;
    Err(s3s::s3_error!(
        NotImplemented,
        "Bucket tagging is not supported by this proxy"
    ))
}

pub(super) async fn put_bucket_acl(
    svc: &DeltaGliderS3Service,
    req: s3s::S3Request<s3s::dto::PutBucketAclInput>,
) -> s3s::S3Result<s3s::S3Response<s3s::dto::PutBucketAclOutput>> {
    ensure_bucket_exists_s3s(&svc.state, &req.input.bucket).await?;
    Err(s3s::s3_error!(
        NotImplemented,
        "Bucket ACL mutation is not supported by this proxy"
    ))
}

pub(super) async fn create_bucket(
    svc: &DeltaGliderS3Service,
    req: s3s::S3Request<s3s::dto::CreateBucketInput>,
) -> s3s::S3Result<s3s::S3Response<s3s::dto::CreateBucketOutput>> {
    let bucket = req.input.bucket;
    // NOTE: CreateBucket is deliberately NOT gated by the
    // replication_target_only marker — an empty destination bucket must be
    // creatable (that's how a replication target is bootstrapped). The
    // marker gates OBJECT writes and bucket DELETION; a bare CreateBucket
    // corrupts nothing (an existing bucket returns BucketAlreadyOwnedByYou).
    svc.state.engine.load().create_bucket(&bucket).await?;
    Ok(s3s::S3Response::new(s3s::dto::CreateBucketOutput {
        location: Some(format!("/{bucket}")),
    }))
}

pub(super) async fn delete_bucket(
    svc: &DeltaGliderS3Service,
    req: s3s::S3Request<s3s::dto::DeleteBucketInput>,
) -> s3s::S3Result<s3s::S3Response<s3s::dto::DeleteBucketOutput>> {
    let bucket = req.input.bucket;
    // A client must not delete a replication mirror (an object-empty or
    // freshly-marked destination would otherwise be destructible).
    crate::api::handlers::object_helpers::check_client_write_allowed(&svc.state, &bucket)?;
    let engine = svc.state.engine.load();

    // Check object emptiness first: only visible objects are hard blockers.
    let first_object = first_visible_key(engine.as_ref(), &bucket).await?;

    let mpu_count = svc.state.multipart.count_uploads_for_bucket(&bucket);
    if let Some(sample) = first_object {
        return Err(s3s::s3_error!(
            BucketNotEmpty,
            "{} (blocked: visible object remains, example_key={}, multipart_uploads={}; action: delete user objects first)",
            bucket,
            sample,
            mpu_count
        ));
    }

    // For object-empty buckets, MPU state is internal residue: purge it
    // deterministically so deletion is self-healing and frictionless.
    //
    // C-P0-1 (mirror of axum handler): refuse while any upload is
    // `Completing` so we don't tear down state the in-flight
    // `engine.store_*` is still holding borrowed paths for.
    if mpu_count > 0 {
        match svc.state.multipart.purge_uploads_for_bucket(&bucket) {
            Ok(purged) => tracing::info!(
                "DELETE bucket {} purged {} multipart upload residues before deletion",
                bucket,
                purged
            ),
            Err(completing) => {
                return Err(s3s::s3_error!(
                    BucketNotEmpty,
                    "{} (blocked: {} multipart upload(s) finalising; retry in a few seconds)",
                    bucket,
                    completing
                ));
            }
        }
    }

    engine.delete_bucket(&bucket).await?;
    // The usage row goes with the bucket: `/_/stats` sums every row, and a
    // bucket created again under this name must start at zero.
    if let Some(usage) = &svc.state.bucket_usage {
        if let Err(e) = usage.forget(&bucket) {
            tracing::warn!("DELETE bucket {bucket}: its usage row stays: {e}");
        }
    }
    Ok(s3s::S3Response::new(s3s::dto::DeleteBucketOutput::default()))
}

/// The first visible key of `bucket` in key order, or `None` when it holds
/// none. It descends one `/`-delimited level at a time: a listing without a
/// delimiter walks the whole bucket on the filesystem backend just to find
/// one key (s3surface-12). The first entry of each level is the prefix of
/// the first key, so the result is the first key of the flat listing.
pub(super) async fn first_visible_key<L: crate::iam::listing::Lister>(
    lister: &L,
    bucket: &str,
) -> Result<Option<String>, crate::deltaglider::EngineError> {
    let mut prefix = String::new();
    loop {
        let page = lister
            .list(bucket, &prefix, Some("/"), 1, None, false)
            .await?;
        if let Some((key, _)) = page.objects.first() {
            return Ok(Some(key.clone()));
        }
        match page.common_prefixes.first() {
            Some(next) => prefix = next.clone(),
            None if prefix.is_empty() => return Ok(None),
            // A prefix with nothing visible under it (on S3, one that holds
            // only proxy-internal keys): the flat listing decides.
            None => {
                let flat = lister.list(bucket, "", None, 1, None, false).await?;
                return Ok(flat.objects.first().map(|(key, _)| key.clone()));
            }
        }
    }
}

pub(super) fn default_acl_owner() -> s3s::dto::Owner {
    s3s::dto::Owner {
        id: Some("dgp".to_string()),
        display_name: Some("deltaglider".to_string()),
    }
}

pub(super) fn default_full_control_grant() -> s3s::dto::Grant {
    s3s::dto::Grant {
        grantee: Some(s3s::dto::Grantee {
            display_name: Some("deltaglider".to_string()),
            id: Some("dgp".to_string()),
            type_: s3s::dto::Type::from_static(s3s::dto::Type::CANONICAL_USER),
            email_address: None,
            uri: None,
        }),
        permission: Some(s3s::dto::Permission::from_static(
            s3s::dto::Permission::FULL_CONTROL,
        )),
    }
}

pub(super) fn list_buckets_output_from_rows(
    mut rows: Vec<(String, chrono::DateTime<chrono::Utc>)>,
    prefix: Option<&str>,
    max_buckets: Option<i32>,
    continuation_token: Option<&str>,
) -> s3s::dto::ListBucketsOutput {
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    if let Some(prefix) = prefix {
        rows.retain(|(name, _)| name.starts_with(prefix));
    }
    // Resume past the previous page: the token is the last bucket name we
    // returned, so drop everything <= it. Without this, a paginating client
    // gets page 1 forever and buckets beyond the first page are unreachable
    // (X-ray H25).
    if let Some(token) = continuation_token {
        rows.retain(|(name, _)| name.as_str() > token);
    }
    let cap = max_buckets
        .and_then(|n| usize::try_from(n.max(0)).ok())
        .unwrap_or(10_000);
    let is_truncated = rows.len() > cap;
    if is_truncated {
        rows.truncate(cap);
    }
    let continuation_token = if is_truncated {
        rows.last().map(|(name, _)| name.clone())
    } else {
        None
    };
    let buckets = rows
        .into_iter()
        .map(|(name, created_at)| s3s::dto::Bucket {
            name: Some(name),
            creation_date: Some(SystemTime::from(created_at).into()),
            bucket_region: Some("us-east-1".to_string()),
        })
        .collect();
    s3s::dto::ListBucketsOutput {
        buckets: Some(buckets),
        continuation_token,
        owner: Some(s3s::dto::Owner {
            display_name: Some("DeltaGlider Proxy".to_string()),
            id: Some("deltaglider_proxy".to_string()),
        }),
        prefix: prefix.map(str::to_string),
    }
}

#[cfg(test)]
mod delete_bucket_usage_tests {
    use super::*;
    use crate::bucket_usage::BucketUsage;
    use crate::storage::DynStorageBackend;

    /// DeleteBucket forgets the bucket's usage row: `/_/stats` sums every
    /// row, and a bucket created again under the same name started with the
    /// old row (its drift and its "scanned" stamp).
    #[tokio::test]
    async fn delete_bucket_forgets_the_usage_row() {
        let dir = tempfile::tempdir().unwrap();
        let backend: Box<DynStorageBackend<'static>> = DynStorageBackend::new_box(
            crate::storage::FilesystemBackend::new(dir.path().to_path_buf())
                .await
                .unwrap(),
        );
        let usage = Arc::new(BucketUsage::in_memory().unwrap());
        let engine: crate::deltaglider::DynEngine =
            crate::deltaglider::DeltaGliderEngine::new_with_backend(
                Arc::new(backend),
                &crate::config::Config::default(),
                None,
            )
            .with_bucket_usage(Some(usage.clone()));
        engine.create_bucket("b").await.unwrap();
        engine
            .store("b", "k.txt", b"hello", None, Default::default())
            .await
            .unwrap();
        engine.delete("b", "k.txt").await.unwrap();
        // Residual drift (e.g. an orphan baseline): the row stays non-zero.
        usage.apply_delta("b", 0, 0, 28);
        usage.flush_pending();
        assert!(usage.read("b").unwrap().is_some());

        let mut state = Arc::try_unwrap(AppState::for_tests(engine)).ok().unwrap();
        state.bucket_usage = Some(usage.clone());
        let svc = DeltaGliderS3Service::new(
            Arc::new(state),
            Arc::new(tokio::sync::RwLock::new(crate::config::Config::default())),
        );
        let req = s3s::S3Request {
            input: s3s::dto::DeleteBucketInput {
                bucket: "b".to_string(),
                expected_bucket_owner: None,
            },
            method: http::Method::DELETE,
            uri: "/b".parse().unwrap(),
            headers: Default::default(),
            extensions: Default::default(),
            credentials: None,
            region: None,
            service: None,
            trailing_headers: None,
        };
        delete_bucket(&svc, req).await.unwrap();

        assert_eq!(usage.read("b").unwrap(), None, "the row survived");
        assert!(usage.read_all().unwrap().iter().all(|(b, _)| b != "b"));
        usage.flush_pending();
        assert_eq!(usage.read("b").unwrap(), None, "a flush brought it back");
    }
}
