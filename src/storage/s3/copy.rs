// SPDX-License-Identifier: BUSL-1.1

//! Server-side copy ([`StorageBackend::copy_within`]): CopyObject up to
//! 5 GiB, UploadPartCopy above. The object's bytes never pass through the
//! proxy.

use super::*;

/// S3's limit on the source of one CopyObject request.
pub(super) const COPY_OBJECT_MAX_BYTES: u64 = 5 * 1024 * 1024 * 1024;

/// Smallest part of a copy above [`COPY_OBJECT_MAX_BYTES`].
pub(super) const COPY_PART_BYTES: u64 = 512 * 1024 * 1024;

/// Pure: the part size of a multipart copy of `size` bytes: at least
/// [`COPY_PART_BYTES`], and large enough for at most 10 000 parts (the S3
/// maximum).
pub(super) fn copy_part_size(size: u64) -> u64 {
    COPY_PART_BYTES.max(size.div_ceil(10_000))
}

/// Pure: the inclusive byte ranges of the parts of a multipart copy of
/// `size` bytes.
pub(super) fn copy_part_ranges(size: u64, part: u64) -> Vec<(u64, u64)> {
    let mut ranges = Vec::new();
    let mut start = 0;
    while start < size {
        let end = (start + part).min(size) - 1;
        ranges.push((start, end));
        start = end + 1;
    }
    ranges
}

/// What a failed CopyObject or UploadPartCopy means for the caller.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum CopyFailure {
    /// The provider does not copy (501 NotImplemented, or a 400
    /// NotImplemented code): the caller copies the bytes.
    Unsupported,
    /// `x-amz-copy-source-if-match` failed: the source changed.
    SourceChanged,
    /// Anything else: the classified error.
    Other,
}

/// Pure: classify a copy failure from its status and S3 error code.
pub(super) fn copy_failure(status: Option<u16>, code: Option<&str>) -> CopyFailure {
    match (status, code) {
        (Some(501), _) | (_, Some("NotImplemented")) => CopyFailure::Unsupported,
        (Some(412), _) | (_, Some("PreconditionFailed")) => CopyFailure::SourceChanged,
        _ => CopyFailure::Other,
    }
}

fn failure_of<E: ProvideErrorMetadata>(e: &SdkError<E>) -> CopyFailure {
    let status = match e {
        SdkError::ServiceError(svc) => Some(svc.raw().status().as_u16()),
        _ => None,
    };
    copy_failure(status, e.code())
}

impl S3Backend {
    /// See [`StorageBackend::copy_within`].
    pub(super) async fn copy_object_within(
        &self,
        variant: ObjectVariant,
        src: ObjectAt<'_>,
        dst: ObjectAt<'_>,
        source: &FileMetadata,
        metadata: &FileMetadata,
    ) -> Result<CopyOutcome, StorageError> {
        if self
            .copy_unsupported
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return Ok(CopyOutcome::Unsupported);
        }
        let key_of = |at: ObjectAt<'_>| match variant {
            ObjectVariant::Delta => self.delta_key(at.prefix, at.filename),
            ObjectVariant::Passthrough => self.passthrough_key(at.prefix, at.filename),
        };
        let (src_key, dst_key) = (key_of(src), key_of(dst));
        // The pin: the source still holds the caller's generation, and the
        // copy is conditional on the ETag of that HEAD.
        BACKEND_HEAD_REQUESTS.inc();
        let head = self
            .client
            .head_object()
            .bucket(src.bucket)
            .key(&src_key)
            .send()
            .await
            .map_err(|e| self.classify(src.bucket, &e, S3Op::HeadObject))?;
        let current = self.metadata_of_response(
            src.bucket,
            &src_key,
            &ObjectHeaders {
                user: head.metadata(),
                last_modified: head.last_modified(),
                e_tag: head.e_tag(),
                content_length: head.content_length(),
                content_type: head.content_type(),
            },
        );
        if !same_stored_generation(source, &current) {
            return Err(StorageError::PreconditionFailed(format!(
                "{}/{src_key} changed before the copy",
                src.bucket
            )));
        }
        let (Some(etag), Some(size)) = (head.e_tag(), head.content_length()) else {
            return Ok(CopyOutcome::Unsupported);
        };
        let size = u64::try_from(size).unwrap_or(0);
        let mut headers = self.metadata_to_headers(metadata);
        if let Some(marker) = self.native_encryption.marker() {
            headers.insert("dg-encrypted-native".to_string(), marker.to_string());
        }
        check_metadata_size(&headers, dst.bucket, &dst_key)?;
        let copy_source = format!("{}/{}", src.bucket, encode_copy_source_key(&src_key));
        let copied = if size <= COPY_OBJECT_MAX_BYTES {
            self.copy_single(dst.bucket, &dst_key, &copy_source, etag, &headers)
                .await
        } else {
            self.copy_parts(dst.bucket, &dst_key, &copy_source, etag, size, &headers)
                .await
        };
        match copied {
            Ok(new_etag) => {
                self.remember_listed_facts(dst.bucket, &dst_key, &new_etag, size, metadata);
                self.persist_listing_facts(dst.bucket, &dst_key, &new_etag, size, metadata);
                debug!(
                    "S3 server-side copy {}/{src_key} -> {}/{dst_key} ({size} bytes)",
                    src.bucket, dst.bucket
                );
                Ok(CopyOutcome::Copied)
            }
            Err((CopyFailure::Unsupported, _)) => {
                // Remembered for the life of this backend instance (one
                // backend definition): the next copy skips the attempt.
                self.copy_unsupported
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                warn!(
                    "S3 backend refuses server-side copy (CopyObject); copies of bucket {} \
                     move the bytes through the proxy",
                    dst.bucket
                );
                Ok(CopyOutcome::Unsupported)
            }
            Err((CopyFailure::SourceChanged, _)) => Err(StorageError::PreconditionFailed(format!(
                "{}/{src_key} changed during the copy",
                src.bucket
            ))),
            Err((CopyFailure::Other, e)) => Err(e),
        }
    }

    /// One CopyObject with `MetadataDirective: REPLACE`: the destination
    /// gets the DG metadata and the content type of a PUT. Returns the new
    /// ETag.
    async fn copy_single(
        &self,
        bucket: &str,
        key: &str,
        copy_source: &str,
        source_etag: &str,
        headers: &HashMap<String, String>,
    ) -> Result<String, (CopyFailure, StorageError)> {
        use aws_sdk_s3::types::ServerSideEncryption;
        let mut request = self
            .bulk_client
            .copy_object()
            .bucket(bucket)
            .key(key)
            .copy_source(copy_source)
            .copy_source_if_match(source_etag)
            .metadata_directive(aws_sdk_s3::types::MetadataDirective::Replace)
            .content_type("application/octet-stream");
        for (k, v) in headers {
            request = request.metadata(k.clone(), v.clone());
        }
        match &self.native_encryption {
            NativeEncryptionConfig::None => {}
            NativeEncryptionConfig::SseS3 => {
                request = request.server_side_encryption(ServerSideEncryption::Aes256);
            }
            NativeEncryptionConfig::SseKms {
                kms_key_id,
                bucket_key_enabled,
            } => {
                request = request
                    .server_side_encryption(ServerSideEncryption::AwsKms)
                    .ssekms_key_id(kms_key_id.clone())
                    .bucket_key_enabled(*bucket_key_enabled);
            }
        }
        match request.send().await {
            Ok(resp) => Ok(resp
                .copy_object_result()
                .and_then(|r| r.e_tag())
                .unwrap_or_default()
                .to_string()),
            Err(e) => Err((
                failure_of(&e),
                self.classify(bucket, &e, S3Op::Other("copy_object")),
            )),
        }
    }

    /// A copy above the CopyObject limit: a multipart upload whose parts
    /// are UploadPartCopy requests, each conditional on the source ETag.
    /// Aborted on any failure.
    async fn copy_parts(
        &self,
        bucket: &str,
        key: &str,
        copy_source: &str,
        source_etag: &str,
        size: u64,
        headers: &HashMap<String, String>,
    ) -> Result<String, (CopyFailure, StorageError)> {
        let mut create = self
            .bulk_client
            .create_multipart_upload()
            .bucket(bucket)
            .key(key)
            .content_type("application/octet-stream");
        for (k, v) in headers {
            create = create.metadata(k.clone(), v.clone());
        }
        create = apply_native_encryption_mpu(create, &self.native_encryption);
        let created = create.send().await.map_err(|e| {
            (
                failure_of(&e),
                self.classify(bucket, &e, S3Op::Other("create_multipart_upload (copy)")),
            )
        })?;
        let Some(upload_id) = created.upload_id().map(str::to_string) else {
            return Err((
                CopyFailure::Other,
                StorageError::Other("CreateMultipartUpload returned no upload id".into()),
            ));
        };
        let result = self
            .copy_part_list(bucket, key, copy_source, source_etag, size, &upload_id)
            .await;
        if result.is_err() {
            let _ = self
                .bulk_client
                .abort_multipart_upload()
                .bucket(bucket)
                .key(key)
                .upload_id(&upload_id)
                .send()
                .await;
        }
        result
    }

    async fn copy_part_list(
        &self,
        bucket: &str,
        key: &str,
        copy_source: &str,
        source_etag: &str,
        size: u64,
        upload_id: &str,
    ) -> Result<String, (CopyFailure, StorageError)> {
        use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
        let mut parts = Vec::new();
        for (i, (start, end)) in copy_part_ranges(size, copy_part_size(size))
            .into_iter()
            .enumerate()
        {
            let number = i as i32 + 1;
            let out = self
                .bulk_client
                .upload_part_copy()
                .bucket(bucket)
                .key(key)
                .upload_id(upload_id)
                .part_number(number)
                .copy_source(copy_source)
                .copy_source_if_match(source_etag)
                .copy_source_range(format!("bytes={start}-{end}"))
                .send()
                .await
                .map_err(|e| {
                    (
                        failure_of(&e),
                        self.classify(bucket, &e, S3Op::Other("upload_part_copy")),
                    )
                })?;
            parts.push(
                CompletedPart::builder()
                    .part_number(number)
                    .set_e_tag(
                        out.copy_part_result()
                            .and_then(|r| r.e_tag())
                            .map(str::to_string),
                    )
                    .build(),
            );
        }
        let done = self
            .bulk_client
            .complete_multipart_upload()
            .bucket(bucket)
            .key(key)
            .upload_id(upload_id)
            .multipart_upload(
                CompletedMultipartUpload::builder()
                    .set_parts(Some(parts))
                    .build(),
            )
            .send()
            .await
            .map_err(|e| {
                (
                    failure_of(&e),
                    self.classify(bucket, &e, S3Op::Other("complete_multipart_upload (copy)")),
                )
            })?;
        Ok(done.e_tag().unwrap_or_default().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_parts_cover_the_object_exactly() {
        assert!(copy_part_ranges(0, 10).is_empty());
        assert_eq!(copy_part_ranges(10, 10), vec![(0, 9)]);
        assert_eq!(copy_part_ranges(25, 10), vec![(0, 9), (10, 19), (20, 24)]);
        let big = 5 * 1024 * 1024 * 1024 * 1024u64;
        assert!(copy_part_ranges(big, copy_part_size(big)).len() <= 10_000);
        let six = 6 * 1024 * 1024 * 1024u64;
        assert_eq!(copy_part_ranges(six, copy_part_size(six)).len(), 12);
    }

    #[test]
    fn a_copy_failure_reads_its_status_and_code() {
        assert_eq!(copy_failure(Some(501), None), CopyFailure::Unsupported);
        assert_eq!(
            copy_failure(Some(400), Some("NotImplemented")),
            CopyFailure::Unsupported
        );
        assert_eq!(copy_failure(Some(412), None), CopyFailure::SourceChanged);
        assert_eq!(
            copy_failure(Some(503), Some("SlowDown")),
            CopyFailure::Other
        );
        assert_eq!(copy_failure(None, None), CopyFailure::Other);
    }
}
