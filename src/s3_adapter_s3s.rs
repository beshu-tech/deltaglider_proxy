//! Experimental `s3s` protocol adapter.
//!
//! This module is intentionally feature-gated (`s3s-adapter`) and not wired
//! into the default server yet. The goal is to migrate the S3 protocol surface
//! gradually: implement one `s3s::S3` operation at a time while the existing
//! Axum handlers remain the production path.
//!
//! Boundary contract:
//! - `s3s` owns HTTP/S3 parsing, generated DTOs, XML/error rendering, and
//!   protocol validation.
//! - DeltaGlider keeps all product logic: admission/IAM policy, compression,
//!   encryption wrappers, metadata cache, replication, metrics, and storage.

use crate::api::handlers::{debug_headers_enabled, AppState};
use crate::deltaglider::RetrieveResponse;
use crate::iam::{
    user_can_see_common_prefix, user_can_see_listed_key, AuthenticatedUser, ListScope, S3Action,
};
use crate::storage::StorageError;
use crate::types::FileMetadata;
use futures::stream::BoxStream;
use futures::Stream;
use futures::StreamExt;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::task::{Context, Poll};
use std::time::SystemTime;

#[derive(Debug, Clone)]
pub struct ListMetadataXmlExtensions(
    pub std::collections::HashMap<String, std::collections::HashMap<String, String>>,
);

#[derive(Debug, Clone)]
pub struct RecursiveDeleteJson {
    pub deleted: u32,
    pub denied: u32,
}

/// Thin service object that will implement the `s3s::S3` trait operation by
/// operation. For now, the empty trait impl intentionally returns `s3s`'s
/// default NotImplemented responses for every operation.
#[derive(Clone)]
pub struct DeltaGliderS3Service {
    state: Arc<AppState>,
}

impl DeltaGliderS3Service {
    pub fn new(state: Arc<AppState>) -> Self {
        Self { state }
    }

    /// Exposed for adapter tests and for the future router builder.
    pub fn state(&self) -> &Arc<AppState> {
        &self.state
    }
}

#[async_trait::async_trait]
impl s3s::S3 for DeltaGliderS3Service {
    async fn head_object(
        &self,
        req: s3s::S3Request<s3s::dto::HeadObjectInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::HeadObjectOutput>> {
        let input = req.input;
        let meta = self
            .state
            .engine
            .load()
            .head(&input.bucket, &input.key)
            .await
            .map_err(engine_error_to_s3s)?;
        evaluate_read_conditionals_s3s(
            &meta,
            input.if_match.as_ref(),
            input.if_none_match.as_ref(),
            input.if_modified_since.as_ref(),
            input.if_unmodified_since.as_ref(),
        )?;

        let mut output = head_object_output_from_metadata(&meta)?;
        let mut status = None;
        if let Some(range) = input.range.as_ref() {
            let checked = range
                .check(meta.file_size)
                .map_err(|_| s3s::s3_error!(InvalidRange))?;
            let range_len = checked.end.saturating_sub(checked.start);
            output.content_length = Some(i64::try_from(range_len).unwrap_or(i64::MAX));
            output.content_range = Some(format!(
                "bytes {}-{}/{}",
                checked.start,
                checked.end.saturating_sub(1),
                meta.file_size
            ));
            status = Some(axum::http::StatusCode::PARTIAL_CONTENT);
        }

        let mut resp = s3s::S3Response::new(output);
        resp.status = status;
        add_storage_debug_headers(&mut resp.headers, &meta);
        Ok(resp)
    }

    async fn get_object(
        &self,
        req: s3s::S3Request<s3s::dto::GetObjectInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::GetObjectOutput>> {
        let input = req.input;
        let engine = self.state.engine.load();
        let head = engine
            .head(&input.bucket, &input.key)
            .await
            .map_err(engine_error_to_s3s)?;
        evaluate_read_conditionals_s3s(
            &head,
            input.if_match.as_ref(),
            input.if_none_match.as_ref(),
            input.if_modified_since.as_ref(),
            input.if_unmodified_since.as_ref(),
        )?;

        if let Some(range) = input.range.as_ref() {
            let checked = range
                .check(head.file_size)
                .map_err(|_| s3s::s3_error!(InvalidRange))?;
            let start = checked.start;
            let end_inclusive = checked.end.saturating_sub(1);
            let content_range = format!("bytes {start}-{end_inclusive}/{}", head.file_size);

            if let Some((stream, content_length, metadata)) = engine
                .retrieve_stream_range(&input.bucket, &input.key, start, end_inclusive)
                .await
                .map_err(engine_error_to_s3s)?
            {
                let body = s3s::dto::StreamingBlob::new(SyncStorageStream::new(stream));
                let mut output = get_object_output_from_metadata(&metadata, body)?;
                output.content_length = Some(i64::try_from(content_length).unwrap_or(i64::MAX));
                output.content_range = Some(content_range);
                apply_get_response_overrides(&input, &mut output);
                let mut resp =
                    s3s::S3Response::with_status(output, axum::http::StatusCode::PARTIAL_CONTENT);
                add_storage_debug_headers(&mut resp.headers, &metadata);
                return Ok(resp);
            }

            let (data, metadata) = engine
                .retrieve(&input.bucket, &input.key)
                .await
                .map_err(engine_error_to_s3s)?;
            let sliced = bytes::Bytes::copy_from_slice(
                &data[usize::try_from(start).unwrap_or(usize::MAX)
                    ..usize::try_from(checked.end).unwrap_or(usize::MAX)],
            );
            let body = s3s::dto::StreamingBlob::from(s3s::Body::from(sliced));
            let mut output = get_object_output_from_metadata(&metadata, body)?;
            let range_len = checked.end.saturating_sub(checked.start);
            output.content_length = Some(i64::try_from(range_len).unwrap_or(i64::MAX));
            output.content_range = Some(content_range);
            apply_get_response_overrides(&input, &mut output);
            let mut resp =
                s3s::S3Response::with_status(output, axum::http::StatusCode::PARTIAL_CONTENT);
            add_storage_debug_headers(&mut resp.headers, &metadata);
            return Ok(resp);
        }

        let response = self
            .state
            .engine
            .load()
            .retrieve_stream(&input.bucket, &input.key)
            .await
            .map_err(engine_error_to_s3s)?;
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
        let mut output = get_object_output_from_metadata(&metadata, body)?;
        apply_get_response_overrides(&input, &mut output);
        let mut resp = s3s::S3Response::new(output);
        add_storage_debug_headers(&mut resp.headers, &metadata);
        Ok(resp)
    }

    async fn list_objects_v2(
        &self,
        req: s3s::S3Request<s3s::dto::ListObjectsV2Input>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::ListObjectsV2Output>> {
        let list_scope = req.extensions.get::<ListScope>().cloned();
        let include_metadata = query_flag(&req.uri, "metadata", "true");
        let input = req.input;
        let max_keys = input.max_keys.unwrap_or(1000).clamp(1, 1000) as u32;
        let mut page = self
            .state
            .engine
            .load()
            .list_objects(
                &input.bucket,
                input.prefix.as_deref().unwrap_or(""),
                input.delimiter.as_deref(),
                max_keys,
                input.continuation_token.as_deref(),
                false,
            )
            .await
            .map_err(engine_error_to_s3s)?;
        if let Some(ListScope::Filtered { user }) = list_scope {
            let requested_prefix = input.prefix.as_deref().unwrap_or("");
            page.objects.retain(|(key, _)| {
                user_can_see_listed_key(&user, &input.bucket, key, requested_prefix)
            });
            page.common_prefixes
                .retain(|prefix| user_can_see_common_prefix(&user, &input.bucket, prefix));
        }
        let metadata_ext = include_metadata.then(|| {
            ListMetadataXmlExtensions(
                page.objects
                    .iter()
                    .map(|(key, meta)| (key.clone(), meta.all_amz_metadata()))
                    .collect(),
            )
        });
        let mut resp =
            s3s::S3Response::new(list_objects_v2_output_from_page(&input, max_keys, page)?);
        if let Some(metadata_ext) = metadata_ext {
            resp.extensions.insert(metadata_ext);
        }
        Ok(resp)
    }

    async fn list_buckets(
        &self,
        req: s3s::S3Request<s3s::dto::ListBucketsInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::ListBucketsOutput>> {
        let auth_user = req.extensions.get::<AuthenticatedUser>().cloned();
        let input = req.input;
        let mut buckets = self
            .state
            .engine
            .load()
            .list_buckets_with_dates()
            .await
            .map_err(engine_error_to_s3s)?;
        if let Some(user) = auth_user {
            buckets.retain(|(name, _)| user.can_see_bucket(name));
        }
        Ok(s3s::S3Response::new(list_buckets_output_from_rows(
            buckets,
            input.prefix.as_deref(),
            input.max_buckets,
        )))
    }

    async fn get_bucket_acl(
        &self,
        req: s3s::S3Request<s3s::dto::GetBucketAclInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::GetBucketAclOutput>> {
        ensure_bucket_exists_s3s(&self.state, &req.input.bucket).await?;
        Ok(s3s::S3Response::new(s3s::dto::GetBucketAclOutput {
            owner: Some(default_acl_owner()),
            grants: Some(vec![default_full_control_grant()]),
        }))
    }

    async fn get_bucket_location(
        &self,
        req: s3s::S3Request<s3s::dto::GetBucketLocationInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::GetBucketLocationOutput>> {
        ensure_bucket_exists_s3s(&self.state, &req.input.bucket).await?;
        Ok(s3s::S3Response::new(s3s::dto::GetBucketLocationOutput {
            location_constraint: None,
        }))
    }

    async fn get_bucket_versioning(
        &self,
        req: s3s::S3Request<s3s::dto::GetBucketVersioningInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::GetBucketVersioningOutput>> {
        ensure_bucket_exists_s3s(&self.state, &req.input.bucket).await?;
        Ok(s3s::S3Response::new(
            s3s::dto::GetBucketVersioningOutput::default(),
        ))
    }

    async fn get_bucket_tagging(
        &self,
        req: s3s::S3Request<s3s::dto::GetBucketTaggingInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::GetBucketTaggingOutput>> {
        ensure_bucket_exists_s3s(&self.state, &req.input.bucket).await?;
        Err(s3s::s3_error!(
            NotImplemented,
            "Bucket tagging is not supported by this proxy"
        ))
    }

    async fn put_bucket_tagging(
        &self,
        req: s3s::S3Request<s3s::dto::PutBucketTaggingInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::PutBucketTaggingOutput>> {
        ensure_bucket_exists_s3s(&self.state, &req.input.bucket).await?;
        Err(s3s::s3_error!(
            NotImplemented,
            "Bucket tagging is not supported by this proxy"
        ))
    }

    async fn put_bucket_acl(
        &self,
        req: s3s::S3Request<s3s::dto::PutBucketAclInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::PutBucketAclOutput>> {
        ensure_bucket_exists_s3s(&self.state, &req.input.bucket).await?;
        Err(s3s::s3_error!(
            NotImplemented,
            "Bucket ACL mutation is not supported by this proxy"
        ))
    }

    async fn get_object_acl(
        &self,
        req: s3s::S3Request<s3s::dto::GetObjectAclInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::GetObjectAclOutput>> {
        let input = req.input;
        self.state
            .engine
            .load()
            .head(&input.bucket, &input.key)
            .await
            .map_err(engine_error_to_s3s)?;
        Ok(s3s::S3Response::new(s3s::dto::GetObjectAclOutput {
            owner: Some(default_acl_owner()),
            grants: Some(vec![default_full_control_grant()]),
            ..Default::default()
        }))
    }

    async fn put_object_acl(
        &self,
        req: s3s::S3Request<s3s::dto::PutObjectAclInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::PutObjectAclOutput>> {
        let input = req.input;
        self.state
            .engine
            .load()
            .head(&input.bucket, &input.key)
            .await
            .map_err(engine_error_to_s3s)?;
        Err(s3s::s3_error!(
            NotImplemented,
            "Object ACL mutation is not supported by this proxy"
        ))
    }

    async fn get_object_tagging(
        &self,
        req: s3s::S3Request<s3s::dto::GetObjectTaggingInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::GetObjectTaggingOutput>> {
        let input = req.input;
        self.state
            .engine
            .load()
            .head(&input.bucket, &input.key)
            .await
            .map_err(engine_error_to_s3s)?;
        Err(s3s::s3_error!(
            NotImplemented,
            "Object tagging is not supported by this proxy"
        ))
    }

    async fn put_object_tagging(
        &self,
        req: s3s::S3Request<s3s::dto::PutObjectTaggingInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::PutObjectTaggingOutput>> {
        let input = req.input;
        self.state
            .engine
            .load()
            .head(&input.bucket, &input.key)
            .await
            .map_err(engine_error_to_s3s)?;
        Err(s3s::s3_error!(
            NotImplemented,
            "Object tagging is not supported by this proxy"
        ))
    }

    async fn delete_object_tagging(
        &self,
        req: s3s::S3Request<s3s::dto::DeleteObjectTaggingInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::DeleteObjectTaggingOutput>> {
        let input = req.input;
        self.state
            .engine
            .load()
            .head(&input.bucket, &input.key)
            .await
            .map_err(engine_error_to_s3s)?;
        Err(s3s::s3_error!(
            NotImplemented,
            "Object tagging is not supported by this proxy"
        ))
    }

    async fn create_bucket(
        &self,
        req: s3s::S3Request<s3s::dto::CreateBucketInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::CreateBucketOutput>> {
        let bucket = req.input.bucket;
        self.state
            .engine
            .load()
            .create_bucket(&bucket)
            .await
            .map_err(engine_error_to_s3s)?;
        Ok(s3s::S3Response::new(s3s::dto::CreateBucketOutput {
            location: Some(format!("/{bucket}")),
        }))
    }

    async fn delete_bucket(
        &self,
        req: s3s::S3Request<s3s::dto::DeleteBucketInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::DeleteBucketOutput>> {
        let bucket = req.input.bucket;
        let engine = self.state.engine.load();
        let page = engine
            .list_objects(&bucket, "", None, 1, None, false)
            .await
            .map_err(engine_error_to_s3s)?;
        if !page.objects.is_empty() || self.state.multipart.count_uploads_for_bucket(&bucket) > 0 {
            return Err(s3s::s3_error!(
                BucketNotEmpty,
                "Bucket is not empty (objects or active multipart uploads exist)"
            ));
        }
        engine
            .delete_bucket(&bucket)
            .await
            .map_err(engine_error_to_s3s)?;
        Ok(s3s::S3Response::new(s3s::dto::DeleteBucketOutput::default()))
    }

    async fn delete_object(
        &self,
        req: s3s::S3Request<s3s::dto::DeleteObjectInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::DeleteObjectOutput>> {
        let auth_user = req.extensions.get::<AuthenticatedUser>().cloned();
        let input = req.input;
        if input.key.ends_with('/') {
            let (deleted, denied) = recursive_delete_prefix_s3s(
                &self.state,
                auth_user.as_ref(),
                &input.bucket,
                &input.key,
            )
            .await?;
            let mut resp = s3s::S3Response::with_status(
                s3s::dto::DeleteObjectOutput::default(),
                axum::http::StatusCode::OK,
            );
            resp.extensions
                .insert(RecursiveDeleteJson { deleted, denied });
            return Ok(resp);
        }
        match self
            .state
            .engine
            .load()
            .delete(&input.bucket, &input.key)
            .await
        {
            Ok(()) | Err(crate::deltaglider::EngineError::NotFound(_)) => {
                Ok(s3s::S3Response::new(s3s::dto::DeleteObjectOutput::default()))
            }
            Err(e) => Err(engine_error_to_s3s(e)),
        }
    }

    async fn delete_objects(
        &self,
        req: s3s::S3Request<s3s::dto::DeleteObjectsInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::DeleteObjectsOutput>> {
        let input = req.input;
        validate_delete_objects_count(input.delete.objects.len())?;
        let quiet = input.delete.quiet.unwrap_or(false);
        let mut deleted = Vec::new();
        let mut errors = Vec::new();
        for obj in input.delete.objects {
            let key = obj.key.trim_start_matches('/').to_string();
            match self.state.engine.load().delete(&input.bucket, &key).await {
                Ok(()) | Err(crate::deltaglider::EngineError::NotFound(_)) => {
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
        Ok(s3s::S3Response::new(s3s::dto::DeleteObjectsOutput {
            deleted: (!deleted.is_empty()).then_some(deleted),
            errors: (!errors.is_empty()).then_some(errors),
            ..Default::default()
        }))
    }

    async fn put_object(
        &self,
        req: s3s::S3Request<s3s::dto::PutObjectInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::PutObjectOutput>> {
        let headers = req.headers.clone();
        let signed_payload_hash = req
            .extensions
            .get::<crate::api::auth::SignedPayloadHash>()
            .cloned();
        let input = req.input;
        let engine = self.state.engine.load();
        if !engine
            .head_bucket(&input.bucket)
            .await
            .map_err(engine_error_to_s3s)?
        {
            return Err(s3s::s3_error!(NoSuchBucket));
        }

        let data =
            collect_blob_limited(input.body, engine.max_object_size(), Some(&headers)).await?;
        verify_signed_payload_hash_s3s(signed_payload_hash.as_ref(), &data)?;
        validate_content_md5_s3s(input.content_md5.as_deref(), &data)?;
        evaluate_put_etag_conditionals_s3s(
            engine.as_ref(),
            &input.bucket,
            &input.key,
            input.if_match.as_ref(),
            input.if_none_match.as_ref(),
        )
        .await?;
        let content_type = input.content_type;
        let user_metadata = input.metadata.unwrap_or_default();
        let result = engine
            .store(
                &input.bucket,
                &input.key,
                &data,
                content_type,
                user_metadata,
            )
            .await
            .map_err(engine_error_to_s3s)?;
        let mut resp = s3s::S3Response::new(s3s::dto::PutObjectOutput {
            e_tag: Some(parse_s3s_etag(&result.metadata.etag())?),
            ..Default::default()
        });
        add_storage_debug_headers(&mut resp.headers, &result.metadata);
        Ok(resp)
    }

    async fn copy_object(
        &self,
        req: s3s::S3Request<s3s::dto::CopyObjectInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::CopyObjectOutput>> {
        let auth_user = req.extensions.get::<AuthenticatedUser>().cloned();
        let input = req.input;
        let (source_bucket, source_key) = copy_source_bucket_key(&input.copy_source)?;
        check_copy_source_access_s3s(auth_user.as_ref(), &source_bucket, &source_key)?;
        ensure_bucket_exists_s3s(&self.state, &source_bucket).await?;
        ensure_bucket_exists_s3s(&self.state, &input.bucket).await?;
        let engine = self.state.engine.load();
        let source_meta = engine
            .head(&source_bucket, &source_key)
            .await
            .map_err(engine_error_to_s3s)?;
        evaluate_copy_source_conditionals_s3s(
            &source_meta,
            input.copy_source_if_match.as_ref(),
            input.copy_source_if_none_match.as_ref(),
            input.copy_source_if_modified_since.as_ref(),
            input.copy_source_if_unmodified_since.as_ref(),
        )?;
        if source_meta.file_size > engine.max_object_size() {
            return Err(s3s::s3_error!(EntityTooLarge));
        }
        let (data, source_meta) = engine
            .retrieve(&source_bucket, &source_key)
            .await
            .map_err(engine_error_to_s3s)?;
        if data.len() as u64 > engine.max_object_size() {
            return Err(s3s::s3_error!(EntityTooLarge));
        }
        let directive = input
            .metadata_directive
            .as_ref()
            .map(|d| d.as_str())
            .unwrap_or(s3s::dto::MetadataDirective::COPY);
        let (content_type, user_metadata) = if directive.eq_ignore_ascii_case("REPLACE") {
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
        let result = engine
            .store(
                &input.bucket,
                &input.key,
                &data,
                content_type,
                user_metadata,
            )
            .await
            .map_err(engine_error_to_s3s)?;
        Ok(s3s::S3Response::new(s3s::dto::CopyObjectOutput {
            copy_object_result: Some(s3s::dto::CopyObjectResult {
                e_tag: Some(parse_s3s_etag(&result.metadata.etag())?),
                last_modified: Some(SystemTime::from(result.metadata.created_at).into()),
                ..Default::default()
            }),
            ..Default::default()
        }))
    }

    async fn create_multipart_upload(
        &self,
        req: s3s::S3Request<s3s::dto::CreateMultipartUploadInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::CreateMultipartUploadOutput>> {
        let input = req.input;
        ensure_bucket_exists_s3s(&self.state, &input.bucket).await?;
        let upload_id = self
            .state
            .multipart
            .create(
                &input.bucket,
                &input.key,
                input.content_type.clone(),
                input.metadata.unwrap_or_default(),
            )
            .map_err(engine_error_to_s3s)?;
        Ok(s3s::S3Response::new(
            s3s::dto::CreateMultipartUploadOutput {
                bucket: Some(input.bucket),
                key: Some(input.key),
                upload_id: Some(upload_id),
                ..Default::default()
            },
        ))
    }

    async fn upload_part(
        &self,
        req: s3s::S3Request<s3s::dto::UploadPartInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::UploadPartOutput>> {
        let headers = req.headers.clone();
        let input = req.input;
        ensure_bucket_exists_s3s(&self.state, &input.bucket).await?;
        let data = collect_blob_limited(
            input.body,
            self.state.engine.load().max_object_size(),
            Some(&headers),
        )
        .await?;
        validate_content_md5_s3s(input.content_md5.as_deref(), &data)?;
        let etag = self
            .state
            .multipart
            .upload_part(
                &input.upload_id,
                &input.bucket,
                &input.key,
                input.part_number as u32,
                data,
            )
            .map_err(engine_error_to_s3s)?;
        Ok(s3s::S3Response::new(s3s::dto::UploadPartOutput {
            e_tag: Some(parse_s3s_etag(&etag)?),
            ..Default::default()
        }))
    }

    async fn abort_multipart_upload(
        &self,
        req: s3s::S3Request<s3s::dto::AbortMultipartUploadInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::AbortMultipartUploadOutput>> {
        let input = req.input;
        self.state
            .multipart
            .abort(&input.upload_id, &input.bucket, &input.key)
            .map_err(engine_error_to_s3s)?;
        Ok(s3s::S3Response::new(
            s3s::dto::AbortMultipartUploadOutput::default(),
        ))
    }

    async fn list_parts(
        &self,
        req: s3s::S3Request<s3s::dto::ListPartsInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::ListPartsOutput>> {
        let input = req.input;
        let max_parts = input.max_parts.unwrap_or(1000).clamp(1, 1000) as u32;
        let marker = input.part_number_marker.unwrap_or(0) as u32;
        let (parts, is_truncated, next_marker) = self
            .state
            .multipart
            .list_parts_paginated(
                &input.upload_id,
                &input.bucket,
                &input.key,
                marker,
                max_parts,
            )
            .map_err(engine_error_to_s3s)?;
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

    async fn complete_multipart_upload(
        &self,
        req: s3s::S3Request<s3s::dto::CompleteMultipartUploadInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::CompleteMultipartUploadOutput>> {
        let input = req.input;
        ensure_bucket_exists_s3s(&self.state, &input.bucket).await?;
        let requested_parts = completed_parts_to_request(input.multipart_upload.as_ref())?;
        let engine = self.state.engine.load();
        let multipart_etag = if !engine.is_delta_eligible(&input.key) {
            let completed = self
                .state
                .multipart
                .complete_parts(
                    &input.upload_id,
                    &input.bucket,
                    &input.key,
                    &requested_parts,
                )
                .map_err(engine_error_to_s3s)?;
            let etag = completed.etag.clone();
            match engine
                .store_passthrough_chunked_with_multipart_etag(
                    &input.bucket,
                    &input.key,
                    &completed.parts,
                    completed.total_size,
                    completed.content_type,
                    completed.user_metadata,
                    etag.clone(),
                )
                .await
            {
                Ok(_) => etag,
                Err(e) => {
                    self.state.multipart.rollback_upload(&input.upload_id);
                    return Err(engine_error_to_s3s(e));
                }
            }
        } else {
            let completed = self
                .state
                .multipart
                .complete(
                    &input.upload_id,
                    &input.bucket,
                    &input.key,
                    &requested_parts,
                )
                .map_err(engine_error_to_s3s)?;
            let etag = completed.etag.clone();
            match engine
                .store_with_multipart_etag(
                    &input.bucket,
                    &input.key,
                    &completed.data,
                    completed.content_type,
                    completed.user_metadata,
                    etag.clone(),
                )
                .await
            {
                Ok(_) => etag,
                Err(e) => {
                    self.state.multipart.rollback_upload(&input.upload_id);
                    return Err(engine_error_to_s3s(e));
                }
            }
        };
        self.state.multipart.finish_upload(&input.upload_id);
        let location = format!("/{}/{}", input.bucket, input.key);
        Ok(s3s::S3Response::new(
            s3s::dto::CompleteMultipartUploadOutput {
                bucket: Some(input.bucket),
                key: Some(input.key),
                e_tag: Some(parse_s3s_etag(&multipart_etag)?),
                location: Some(location),
                ..Default::default()
            },
        ))
    }

    async fn list_multipart_uploads(
        &self,
        req: s3s::S3Request<s3s::dto::ListMultipartUploadsInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::ListMultipartUploadsOutput>> {
        let input = req.input;
        ensure_bucket_exists_s3s(&self.state, &input.bucket).await?;
        let max_uploads = input.max_uploads.unwrap_or(1000).clamp(1, 1000) as u32;
        let (uploads, is_truncated, next_key, next_upload_id) =
            self.state.multipart.list_uploads_paginated(
                Some(&input.bucket),
                input.prefix.as_deref(),
                input.key_marker.as_deref().unwrap_or(""),
                input.upload_id_marker.as_deref().unwrap_or(""),
                max_uploads,
            );
        let uploads = uploads
            .into_iter()
            .map(|u| s3s::dto::MultipartUpload {
                key: Some(u.key),
                upload_id: Some(u.upload_id),
                initiated: Some(SystemTime::from(u.initiated).into()),
                ..Default::default()
            })
            .collect();
        Ok(s3s::S3Response::new(s3s::dto::ListMultipartUploadsOutput {
            bucket: Some(input.bucket),
            delimiter: input.delimiter,
            encoding_type: input.encoding_type,
            is_truncated: Some(is_truncated),
            key_marker: input.key_marker,
            max_uploads: Some(max_uploads as i32),
            next_key_marker: (!next_key.is_empty()).then_some(next_key),
            next_upload_id_marker: (!next_upload_id.is_empty()).then_some(next_upload_id),
            prefix: input.prefix,
            upload_id_marker: input.upload_id_marker,
            uploads: Some(uploads),
            ..Default::default()
        }))
    }

    async fn upload_part_copy(
        &self,
        req: s3s::S3Request<s3s::dto::UploadPartCopyInput>,
    ) -> s3s::S3Result<s3s::S3Response<s3s::dto::UploadPartCopyOutput>> {
        let auth_user = req.extensions.get::<AuthenticatedUser>().cloned();
        let input = req.input;
        let (source_bucket, source_key) = copy_source_bucket_key(&input.copy_source)?;
        check_copy_source_access_s3s(auth_user.as_ref(), &source_bucket, &source_key)?;
        ensure_bucket_exists_s3s(&self.state, &source_bucket).await?;
        ensure_bucket_exists_s3s(&self.state, &input.bucket).await?;
        let engine = self.state.engine.load();
        let source_meta = engine
            .head(&source_bucket, &source_key)
            .await
            .map_err(engine_error_to_s3s)?;
        evaluate_copy_source_conditionals_s3s(
            &source_meta,
            input.copy_source_if_match.as_ref(),
            input.copy_source_if_none_match.as_ref(),
            input.copy_source_if_modified_since.as_ref(),
            input.copy_source_if_unmodified_since.as_ref(),
        )?;
        let (data, _) = engine
            .retrieve(&source_bucket, &source_key)
            .await
            .map_err(engine_error_to_s3s)?;
        let part = if let Some(range) = input.copy_source_range.as_deref() {
            let (start, end) = parse_copy_range(range, data.len())?;
            bytes::Bytes::from(data[start..=end].to_vec())
        } else {
            bytes::Bytes::from(data)
        };
        let etag = self
            .state
            .multipart
            .upload_part(
                &input.upload_id,
                &input.bucket,
                &input.key,
                input.part_number as u32,
                part,
            )
            .map_err(engine_error_to_s3s)?;
        Ok(s3s::S3Response::new(s3s::dto::UploadPartCopyOutput {
            copy_part_result: Some(s3s::dto::CopyPartResult {
                e_tag: Some(parse_s3s_etag(&etag)?),
                last_modified: Some(SystemTime::now().into()),
                ..Default::default()
            }),
            ..Default::default()
        }))
    }
}

struct SyncStorageStream {
    inner: Mutex<BoxStream<'static, Result<bytes::Bytes, StorageError>>>,
}

impl SyncStorageStream {
    fn new(inner: BoxStream<'static, Result<bytes::Bytes, StorageError>>) -> Self {
        Self {
            inner: Mutex::new(inner),
        }
    }
}

impl Stream for SyncStorageStream {
    type Item = Result<bytes::Bytes, s3s::StdError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut guard = self.inner.lock().expect("stream mutex poisoned");
        Pin::new(&mut *guard)
            .poll_next(cx)
            .map_err(|e| Box::new(e) as s3s::StdError)
    }
}

impl s3s::stream::ByteStream for SyncStorageStream {}

async fn collect_blob_limited(
    body: Option<s3s::dto::StreamingBlob>,
    limit: u64,
    headers: Option<&axum::http::HeaderMap>,
) -> s3s::S3Result<bytes::Bytes> {
    let Some(mut body) = body else {
        return Ok(bytes::Bytes::new());
    };
    let mut out = Vec::new();
    while let Some(chunk) = body.next().await {
        let chunk =
            chunk.map_err(|_| s3s::s3_error!(InternalError, "failed to read request body"))?;
        let next_len = out
            .len()
            .checked_add(chunk.len())
            .ok_or_else(|| s3s::s3_error!(EntityTooLarge))?;
        if next_len as u64 > limit {
            return Err(s3s::s3_error!(EntityTooLarge));
        }
        out.extend_from_slice(&chunk);
    }
    let body = bytes::Bytes::from(out);
    if let Some(headers) = headers {
        if crate::api::aws_chunked::is_aws_chunked(headers) {
            let expected_len = crate::api::aws_chunked::get_decoded_content_length(headers);
            if expected_len.is_some_and(|len| len as u64 > limit) {
                return Err(s3s::s3_error!(EntityTooLarge));
            }
            return crate::api::aws_chunked::decode_aws_chunked(&body, expected_len).ok_or_else(
                || {
                    s3s::s3_error!(
                        InvalidArgument,
                        "Failed to decode AWS chunked transfer encoding"
                    )
                },
            );
        }
    }
    Ok(body)
}

async fn ensure_bucket_exists_s3s(state: &Arc<AppState>, bucket: &str) -> s3s::S3Result<()> {
    if state
        .engine
        .load()
        .head_bucket(bucket)
        .await
        .map_err(engine_error_to_s3s)?
    {
        Ok(())
    } else {
        Err(s3s::s3_error!(NoSuchBucket))
    }
}

async fn recursive_delete_prefix_s3s(
    state: &Arc<AppState>,
    auth_user: Option<&AuthenticatedUser>,
    bucket: &str,
    prefix: &str,
) -> s3s::S3Result<(u32, u32)> {
    const DELETE_PAGE_SIZE: u32 = 1000;

    let engine = state.engine.load();
    let mut deleted = 0u32;
    let mut denied = 0u32;
    let mut next_token: Option<String> = None;

    loop {
        let page = engine
            .list_objects(
                bucket,
                prefix,
                None,
                DELETE_PAGE_SIZE,
                next_token.as_deref(),
                false,
            )
            .await
            .map_err(engine_error_to_s3s)?;

        for (obj_key, _) in &page.objects {
            if let Some(user) = auth_user {
                if !user.can(S3Action::Delete, bucket, obj_key) {
                    denied = denied.saturating_add(1);
                    continue;
                }
            }
            match engine.delete(bucket, obj_key).await {
                Ok(()) | Err(crate::deltaglider::EngineError::NotFound(_)) => {
                    deleted = deleted.saturating_add(1);
                }
                Err(e) => return Err(engine_error_to_s3s(e)),
            }
        }

        if !page.is_truncated {
            break;
        }
        next_token = page.next_continuation_token;
        if next_token.is_none() {
            break;
        }
    }

    Ok((deleted, denied))
}

fn copy_source_bucket_key(source: &s3s::dto::CopySource) -> s3s::S3Result<(String, String)> {
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
            Ok((bucket.to_string(), key.to_string()))
        }
        s3s::dto::CopySource::AccessPoint { .. } => Err(s3s::s3_error!(
            NotImplemented,
            "copy source access points are not supported"
        )),
    }
}

fn check_copy_source_access_s3s(
    auth_user: Option<&AuthenticatedUser>,
    source_bucket: &str,
    source_key: &str,
) -> s3s::S3Result<()> {
    let Some(user) = auth_user else {
        return Ok(());
    };
    if user.can(S3Action::Read, source_bucket, source_key) {
        return Ok(());
    }
    crate::audit::audit_log(
        "access_denied",
        &user.name,
        "CopySourceRead",
        &axum::http::HeaderMap::new(),
        source_bucket,
        source_key,
    );
    Err(s3s::s3_error!(AccessDenied))
}

fn evaluate_copy_source_conditionals_s3s(
    source_meta: &FileMetadata,
    if_match: Option<&s3s::dto::ETagCondition>,
    if_none_match: Option<&s3s::dto::ETagCondition>,
    if_modified_since: Option<&s3s::dto::Timestamp>,
    if_unmodified_since: Option<&s3s::dto::Timestamp>,
) -> s3s::S3Result<()> {
    let current = parse_s3s_etag(&source_meta.etag())?;
    let last_modified: s3s::dto::Timestamp = SystemTime::from(source_meta.created_at).into();

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

fn parse_copy_range(range: &str, len: usize) -> s3s::S3Result<(usize, usize)> {
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

fn validate_content_md5_s3s(content_md5: Option<&str>, body: &[u8]) -> s3s::S3Result<()> {
    let Some(content_md5) = content_md5 else {
        return Ok(());
    };
    use base64::Engine as _;
    use md5::Digest as _;
    let expected = base64::engine::general_purpose::STANDARD
        .decode(content_md5.trim())
        .map_err(|_| s3s::s3_error!(InvalidDigest))?;
    let actual = md5::Md5::digest(body);
    if actual.as_slice() != expected.as_slice() {
        return Err(s3s::s3_error!(BadDigest));
    }
    Ok(())
}

fn query_flag(uri: &axum::http::Uri, key: &str, expected: &str) -> bool {
    uri.query()
        .map(|query| {
            query.split('&').any(|part| {
                part.split_once('=')
                    .map(|(k, v)| k == key && v.eq_ignore_ascii_case(expected))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

fn evaluate_read_conditionals_s3s(
    meta: &FileMetadata,
    if_match: Option<&s3s::dto::ETagCondition>,
    if_none_match: Option<&s3s::dto::ETagCondition>,
    if_modified_since: Option<&s3s::dto::Timestamp>,
    if_unmodified_since: Option<&s3s::dto::Timestamp>,
) -> s3s::S3Result<()> {
    let current = parse_s3s_etag(&meta.etag())?;
    let last_modified: s3s::dto::Timestamp = SystemTime::from(meta.created_at).into();

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
            return Err(s3s::s3_error!(NotModified));
        }
        // AWS/S3: a passing If-None-Match wins over If-Modified-Since.
    } else if let Some(since) = if_modified_since {
        if last_modified <= *since {
            return Err(s3s::s3_error!(NotModified));
        }
    }

    Ok(())
}

fn apply_get_response_overrides(
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

fn verify_signed_payload_hash_s3s(
    signed: Option<&crate::api::auth::SignedPayloadHash>,
    body: &[u8],
) -> s3s::S3Result<()> {
    let Some(claimed) = signed else {
        return Ok(());
    };
    if claimed.requires_chunk_signature_verification() {
        return Err(s3s::s3_error!(
            NotImplemented,
            "Signed AWS streaming payloads are not supported; use UNSIGNED-PAYLOAD or non-streaming SHA-256 payloads"
        ));
    }
    if claimed.is_verifiable_hex() {
        use sha2::Digest as _;
        let actual = hex::encode(sha2::Sha256::digest(body));
        let matches: bool =
            subtle::ConstantTimeEq::ct_eq(actual.as_bytes(), claimed.as_str().as_bytes()).into();
        if !matches {
            return Err(s3s::s3_error!(BadDigest));
        }
    }
    Ok(())
}

async fn evaluate_put_etag_conditionals_s3s(
    engine: &crate::deltaglider::DynEngine,
    bucket: &str,
    key: &str,
    if_match: Option<&s3s::dto::ETagCondition>,
    if_none_match: Option<&s3s::dto::ETagCondition>,
) -> s3s::S3Result<()> {
    if if_match.is_none() && if_none_match.is_none() {
        return Ok(());
    }
    let existing = engine.head(bucket, key).await.ok();
    if let Some(cond) = if_match {
        let Some(meta) = existing.as_ref() else {
            return Err(s3s::s3_error!(PreconditionFailed));
        };
        let current = parse_s3s_etag(&meta.etag())?;
        let matches = cond.is_any()
            || cond
                .as_etag()
                .map(|wanted| wanted.weak_cmp(&current))
                .unwrap_or(false);
        if !matches {
            return Err(s3s::s3_error!(PreconditionFailed));
        }
    }
    if let Some(cond) = if_none_match {
        if let Some(meta) = existing.as_ref() {
            let current = parse_s3s_etag(&meta.etag())?;
            let matches = cond.is_any()
                || cond
                    .as_etag()
                    .map(|wanted| wanted.weak_cmp(&current))
                    .unwrap_or(false);
            if matches {
                return Err(s3s::s3_error!(PreconditionFailed));
            }
        }
    }
    Ok(())
}

fn engine_error_to_s3s(err: impl Into<crate::api::S3Error>) -> s3s::S3Error {
    match err.into() {
        crate::api::S3Error::NoSuchKey(_) => s3s::s3_error!(NoSuchKey),
        crate::api::S3Error::NoSuchBucket(_) => s3s::s3_error!(NoSuchBucket),
        crate::api::S3Error::BucketAlreadyExists(_) => s3s::s3_error!(BucketAlreadyExists),
        crate::api::S3Error::BucketNotEmpty(_) => s3s::s3_error!(BucketNotEmpty),
        crate::api::S3Error::EntityTooLarge { .. } => s3s::s3_error!(EntityTooLarge),
        crate::api::S3Error::InvalidArgument(msg) => s3s::s3_error!(InvalidArgument, "{}", msg),
        crate::api::S3Error::InvalidRequest(msg) => s3s::s3_error!(InvalidRequest, "{}", msg),
        crate::api::S3Error::NoSuchUpload(_) => s3s::s3_error!(NoSuchUpload),
        crate::api::S3Error::InvalidPart(msg) => s3s::s3_error!(InvalidPart, "{}", msg),
        crate::api::S3Error::InvalidPartOrder => s3s::s3_error!(InvalidPartOrder),
        crate::api::S3Error::InvalidBucketName(msg) => {
            s3s::s3_error!(InvalidBucketName, "{}", msg)
        }
        crate::api::S3Error::AccessDenied => s3s::s3_error!(AccessDenied),
        crate::api::S3Error::PreconditionFailed => s3s::s3_error!(PreconditionFailed),
        crate::api::S3Error::InvalidRange => s3s::s3_error!(InvalidRange),
        other => s3s::s3_error!(InternalError, "{}", other.code()),
    }
}

fn completed_parts_to_request(
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

fn validate_delete_objects_count(count: usize) -> s3s::S3Result<()> {
    if count > 1000 {
        return Err(s3s::s3_error!(
            InvalidArgument,
            "DeleteObjects supports at most 1000 keys per request"
        ));
    }
    Ok(())
}

fn head_object_output_from_metadata(
    meta: &FileMetadata,
) -> s3s::S3Result<s3s::dto::HeadObjectOutput> {
    let e_tag = parse_s3s_etag(&meta.etag())?;
    let last_modified: s3s::dto::Timestamp = SystemTime::from(meta.created_at).into();
    let content_length = i64::try_from(meta.file_size).unwrap_or(i64::MAX);
    let content_type = meta
        .content_type
        .clone()
        .unwrap_or_else(|| "application/octet-stream".to_string());

    Ok(s3s::dto::HeadObjectOutput {
        accept_ranges: Some("bytes".to_string()),
        content_length: Some(content_length),
        content_type: Some(content_type),
        e_tag: Some(e_tag),
        last_modified: Some(last_modified),
        metadata: Some(response_metadata_map(meta)),
        ..Default::default()
    })
}

fn response_metadata_map(meta: &FileMetadata) -> std::collections::HashMap<String, String> {
    let mut map = meta.to_bare_metadata_map();
    map.remove("content-type");
    map.retain(|key, _| !key.starts_with("user-"));
    for (key, value) in &meta.user_metadata {
        if !key.to_lowercase().starts_with("dg-") {
            map.insert(key.clone(), value.clone());
        }
    }
    map
}

fn add_storage_debug_headers(headers: &mut axum::http::HeaderMap, meta: &FileMetadata) {
    if !debug_headers_enabled() {
        return;
    }
    if let Ok(value) = axum::http::HeaderValue::from_str(meta.storage_info.label()) {
        headers.insert("x-amz-storage-type", value);
    }
    let stored_size = meta.delta_size().unwrap_or(meta.file_size);
    if let Ok(value) = axum::http::HeaderValue::from_str(&stored_size.to_string()) {
        headers.insert("x-deltaglider-stored-size", value);
    }
}

fn default_acl_owner() -> s3s::dto::Owner {
    s3s::dto::Owner {
        id: Some("dgp".to_string()),
        display_name: Some("deltaglider".to_string()),
    }
}

fn default_full_control_grant() -> s3s::dto::Grant {
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

fn get_object_output_from_metadata(
    meta: &FileMetadata,
    body: s3s::dto::StreamingBlob,
) -> s3s::S3Result<s3s::dto::GetObjectOutput> {
    let head = head_object_output_from_metadata(meta)?;
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

fn list_objects_v2_output_from_page(
    input: &s3s::dto::ListObjectsV2Input,
    max_keys: u32,
    page: crate::deltaglider::ListObjectsPage,
) -> s3s::S3Result<s3s::dto::ListObjectsV2Output> {
    let contents: s3s::dto::ObjectList = page
        .objects
        .into_iter()
        .map(|(key, meta)| object_from_metadata(key, &meta))
        .collect::<s3s::S3Result<_>>()?;
    let common_prefixes: s3s::dto::CommonPrefixList = page
        .common_prefixes
        .into_iter()
        .map(|prefix| s3s::dto::CommonPrefix {
            prefix: Some(prefix),
        })
        .collect();
    let key_count = contents.len().saturating_add(common_prefixes.len());
    Ok(s3s::dto::ListObjectsV2Output {
        name: Some(input.bucket.clone()),
        prefix: input.prefix.clone(),
        delimiter: input.delimiter.clone(),
        max_keys: Some(max_keys as i32),
        key_count: Some(i32::try_from(key_count).unwrap_or(i32::MAX)),
        continuation_token: input.continuation_token.clone(),
        is_truncated: Some(page.is_truncated),
        next_continuation_token: page.next_continuation_token,
        contents: Some(contents),
        common_prefixes: Some(common_prefixes),
        encoding_type: input.encoding_type.clone(),
        start_after: input.start_after.clone(),
        ..Default::default()
    })
}

fn object_from_metadata(key: String, meta: &FileMetadata) -> s3s::S3Result<s3s::dto::Object> {
    Ok(s3s::dto::Object {
        key: Some(key),
        e_tag: Some(parse_s3s_etag(&meta.etag())?),
        last_modified: Some(SystemTime::from(meta.created_at).into()),
        size: Some(i64::try_from(meta.file_size).unwrap_or(i64::MAX)),
        ..Default::default()
    })
}

fn list_buckets_output_from_rows(
    mut rows: Vec<(String, chrono::DateTime<chrono::Utc>)>,
    prefix: Option<&str>,
    max_buckets: Option<i32>,
) -> s3s::dto::ListBucketsOutput {
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    if let Some(prefix) = prefix {
        rows.retain(|(name, _)| name.starts_with(prefix));
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

fn parse_s3s_etag(etag: &str) -> s3s::S3Result<s3s::dto::ETag> {
    etag.parse::<s3s::dto::ETag>()
        .map_err(|_| s3s::s3_error!(InternalError, "invalid metadata ETag"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_s3_service<T: s3s::S3>() {}

    #[test]
    fn adapter_type_implements_s3_trait() {
        assert_s3_service::<DeltaGliderS3Service>();
    }

    #[test]
    fn head_output_preserves_visible_metadata() {
        let mut meta = FileMetadata::new_passthrough(
            "file.bin".to_string(),
            "sha".to_string(),
            "0123456789abcdef0123456789abcdef".to_string(),
            42,
            Some("application/octet-stream".to_string()),
        );
        meta.user_metadata
            .insert("owner".to_string(), "alice".to_string());
        meta.multipart_etag = Some("\"abc-2\"".to_string());

        let out = head_object_output_from_metadata(&meta).expect("head output");
        assert_eq!(out.content_length, Some(42));
        assert_eq!(
            out.content_type.as_deref(),
            Some("application/octet-stream")
        );
        assert_eq!(out.e_tag.as_ref().map(s3s::dto::ETag::value), Some("abc-2"));
        let metadata = out.metadata.expect("metadata map");
        assert_eq!(metadata.get("owner").map(String::as_str), Some("alice"));
        assert_eq!(
            metadata.get("dg-multipart-etag").map(String::as_str),
            Some("\"abc-2\"")
        );
    }

    #[test]
    fn get_output_reuses_head_metadata_and_sets_body() {
        let meta = FileMetadata::new_passthrough(
            "file.bin".to_string(),
            "sha".to_string(),
            "0123456789abcdef0123456789abcdef".to_string(),
            3,
            Some("application/octet-stream".to_string()),
        );

        let out = get_object_output_from_metadata(
            &meta,
            s3s::dto::StreamingBlob::from(s3s::Body::from(bytes::Bytes::from_static(b"abc"))),
        )
        .unwrap();
        assert!(out.body.is_some());
        assert_eq!(out.content_length, Some(3));
        assert_eq!(
            out.e_tag.as_ref().map(s3s::dto::ETag::value),
            Some("0123456789abcdef0123456789abcdef")
        );
    }

    #[test]
    fn list_output_maps_objects_and_common_prefixes() {
        let meta = FileMetadata::new_passthrough(
            "a.txt".to_string(),
            "sha".to_string(),
            "0123456789abcdef0123456789abcdef".to_string(),
            10,
            None,
        );
        let input = s3s::dto::ListObjectsV2Input {
            bucket: "bucket".to_string(),
            prefix: Some("p/".to_string()),
            delimiter: Some("/".to_string()),
            max_keys: Some(100),
            ..Default::default()
        };
        let page = crate::deltaglider::ListObjectsPage {
            objects: vec![("p/a.txt".to_string(), meta)],
            common_prefixes: vec!["p/sub/".to_string()],
            is_truncated: true,
            next_continuation_token: Some("p/a.txt".to_string()),
        };

        let out = list_objects_v2_output_from_page(&input, 100, page).unwrap();
        assert_eq!(out.name.as_deref(), Some("bucket"));
        assert_eq!(out.key_count, Some(2));
        assert_eq!(out.is_truncated, Some(true));
        assert_eq!(out.next_continuation_token.as_deref(), Some("p/a.txt"));
        assert_eq!(out.contents.as_ref().map(Vec::len), Some(1));
        assert_eq!(out.common_prefixes.as_ref().map(Vec::len), Some(1));
    }

    #[test]
    fn list_buckets_output_filters_sorts_and_paginates() {
        let ts = chrono::Utc::now();
        let out = list_buckets_output_from_rows(
            vec![
                ("beta".to_string(), ts),
                ("alpha".to_string(), ts),
                ("archive".to_string(), ts),
            ],
            Some("a"),
            Some(1),
        );
        let buckets = out.buckets.expect("buckets");
        assert_eq!(buckets.len(), 1);
        assert_eq!(buckets[0].name.as_deref(), Some("alpha"));
        assert_eq!(out.continuation_token.as_deref(), Some("alpha"));
        assert_eq!(out.prefix.as_deref(), Some("a"));
    }

    #[test]
    fn delete_objects_count_limit_matches_s3_cap() {
        assert!(validate_delete_objects_count(1000).is_ok());
        assert_eq!(
            validate_delete_objects_count(1001).unwrap_err().code(),
            &s3s::S3ErrorCode::InvalidArgument
        );
    }

    #[tokio::test]
    async fn collect_blob_limited_rejects_oversize_body() {
        let blob =
            s3s::dto::StreamingBlob::from(s3s::Body::from(bytes::Bytes::from_static(b"abcd")));
        let err = collect_blob_limited(Some(blob), 3, None).await.unwrap_err();
        assert_eq!(err.code(), &s3s::S3ErrorCode::EntityTooLarge);
    }

    #[test]
    fn content_md5_validation_detects_mismatch() {
        use base64::Engine as _;
        use md5::Digest as _;
        let good = base64::engine::general_purpose::STANDARD.encode(md5::Md5::digest(b"abc"));
        assert!(validate_content_md5_s3s(Some(&good), b"abc").is_ok());
        assert_eq!(
            validate_content_md5_s3s(Some(&good), b"xyz")
                .unwrap_err()
                .code(),
            &s3s::S3ErrorCode::BadDigest
        );
    }

    #[test]
    fn signed_payload_hash_validation_detects_mismatch() {
        use sha2::Digest as _;
        let good = hex::encode(sha2::Sha256::digest(b"abc"));
        assert!(verify_signed_payload_hash_s3s(
            Some(&crate::api::auth::SignedPayloadHash(good)),
            b"abc"
        )
        .is_ok());
        let bad = "0".repeat(64);
        assert_eq!(
            verify_signed_payload_hash_s3s(Some(&crate::api::auth::SignedPayloadHash(bad)), b"abc")
                .unwrap_err()
                .code(),
            &s3s::S3ErrorCode::BadDigest
        );
    }

    #[test]
    fn completed_parts_conversion_requires_etags() {
        let upload = s3s::dto::CompletedMultipartUpload {
            parts: Some(vec![s3s::dto::CompletedPart {
                part_number: Some(1),
                e_tag: Some("\"abc\"".parse().unwrap()),
                ..Default::default()
            }]),
        };
        assert_eq!(
            completed_parts_to_request(Some(&upload)).unwrap(),
            vec![(1, "\"abc\"".to_string())]
        );
        let bad = s3s::dto::CompletedMultipartUpload {
            parts: Some(vec![s3s::dto::CompletedPart {
                part_number: Some(1),
                e_tag: None,
                ..Default::default()
            }]),
        };
        assert_eq!(
            completed_parts_to_request(Some(&bad)).unwrap_err().code(),
            &s3s::S3ErrorCode::InvalidPart
        );
    }
}
