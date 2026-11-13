// SPDX-License-Identifier: BUSL-1.1

//! A file body above the single-PUT limit, stored as a backend multipart
//! upload (CreateMultipartUpload, UploadPart, CompleteMultipartUpload).
//!
//! S3 takes at most 5 GiB in one PutObject request (the same limit holds on
//! Ceph RGW). Every file-backed write goes through
//! [`S3Backend::put_object_file_with_metadata_fenced_with`]: relayed client
//! parts, a delta or reference from a spool file, the ciphertext temp file
//! of a proxy-encrypted bucket. That one method switches to this module
//! when the body is larger than [`SINGLE_PUT_MAX_BYTES`], so each of those
//! writes can reach the object size that S3 itself allows (5 TiB).
//!
//! The parts stream from file ranges ([`FileBody::slice`]); no part is held
//! in memory. The upload is aborted on every error and when the future is
//! dropped ([`MpuAbortGuard`]).

use super::file_body::FileBody;
use super::*;
use crate::coordination::if_match::IfMatchRequest;
use crate::storage::mpu_guard::MpuAbortGuard;
use futures::{StreamExt, TryStreamExt};

pub(super) use crate::config::limits::S3_SINGLE_PUT_MAX_BYTES as SINGLE_PUT_MAX_BYTES;

/// The part size of an upload, unless the body is so large that more
/// parts than [`MAX_PARTS`] would be needed.
pub(super) const PART_BYTES: u64 = 64 * 1024 * 1024;

/// S3's smallest part (the last part may be smaller).
pub(super) const MIN_PART_BYTES: u64 = 5 * 1024 * 1024;

/// S3's largest part.
pub(super) const MAX_PART_BYTES: u64 = 5 * 1024 * 1024 * 1024;

/// S3's most parts in one upload.
pub(super) const MAX_PARTS: u64 = 10_000;

/// Parts of one upload that are in flight together.
pub(super) const PARALLEL_PARTS: usize = 4;

/// The size above which a file body is a multipart upload:
/// [`SINGLE_PUT_MAX_BYTES`], or the debug test seam
/// `DGP_TEST_PUT_SINGLE_MAX_BYTES`.
pub(super) fn single_put_max_bytes() -> u64 {
    crate::config::test_seams::test_seams()
        .put_single_max_bytes
        .unwrap_or(SINGLE_PUT_MAX_BYTES)
}

/// The preferred part size: [`PART_BYTES`], or the debug test seam
/// `DGP_TEST_PUT_PART_BYTES`.
pub(super) fn part_bytes() -> u64 {
    crate::config::test_seams::test_seams()
        .put_part_bytes
        .unwrap_or(PART_BYTES)
}

/// Pure: the part size for a body of `total` bytes. At least `preferred`
/// (kept between S3's smallest and largest part) and large enough for at
/// most [`MAX_PARTS`] parts, in whole MiB. `None`: no part size fits the
/// body (it is above what S3 stores in one object).
pub(super) fn part_size(total: u64, preferred: u64) -> Option<u64> {
    const MIB: u64 = 1024 * 1024;
    let for_count = total.div_ceil(MAX_PARTS).div_ceil(MIB) * MIB;
    let size = preferred
        .clamp(MIN_PART_BYTES, MAX_PART_BYTES)
        .max(for_count);
    (size <= MAX_PART_BYTES).then_some(size)
}

/// Pure: the `(offset, length)` of each part of a body of `total` bytes,
/// in order. Every part but the last has `part` bytes; the parts cover the
/// body exactly. Empty for an empty body.
pub(super) fn part_ranges(total: u64, part: u64) -> Vec<(u64, u64)> {
    let part = part.max(1);
    let mut ranges = Vec::new();
    let mut offset = 0;
    while offset < total {
        let len = part.min(total - offset);
        ranges.push((offset, len));
        offset += len;
    }
    ranges
}

/// Pure: the parts of an upload of `total` bytes with the preferred part
/// size, or `None` when the body is too large for one object.
pub(super) fn plan_parts(total: u64, preferred: u64) -> Option<Vec<(u64, u64)>> {
    let size = part_size(total, preferred)?;
    let ranges = part_ranges(total, size);
    (ranges.len() as u64 <= MAX_PARTS).then_some(ranges)
}

/// Run one request, and again after a Hetzner 400 without a request id
/// ([`is_unidentified_400`]; the SDK retries no 400).
/// An error that ends the request is `classify`'d.
async fn retry_unidentified_400<T, E, F, Fut>(
    mut attempt: F,
    classify: impl FnOnce(SdkError<E>) -> StorageError,
) -> Result<T, StorageError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, SdkError<E>>>,
    E: std::fmt::Debug,
{
    let mut backoff = UNIDENTIFIED_400_BACKOFF_MS.iter();
    loop {
        match attempt().await {
            Ok(done) => return Ok(done),
            Err(e) if is_unidentified_400(&e) => match backoff.next() {
                Some(ms) => {
                    warn!("S3 multipart upload: a 400 without a request id, retrying in {ms}ms: {e:?}");
                    tokio::time::sleep(std::time::Duration::from_millis(*ms)).await;
                }
                None => return Err(classify(e)),
            },
            Err(e) => return Err(classify(e)),
        }
    }
}

/// `CompleteMultipartUpload` with the write condition of a PUT: the same
/// two headers (`If-None-Match: *` for [`RefFence::Absent`], `If-Match` in
/// the backend's dialect for an ETag).
fn apply_complete_fence(
    req: aws_sdk_s3::operation::complete_multipart_upload::builders::CompleteMultipartUploadFluentBuilder,
    fence: &RefFence,
    dialect: crate::coordination::if_match::IfMatchDialect,
) -> aws_sdk_s3::operation::complete_multipart_upload::builders::CompleteMultipartUploadFluentBuilder
{
    match fence {
        RefFence::Absent => req.if_none_match("*"),
        RefFence::ETag(e) if !e.is_empty() => req.if_match_etag(e, dialect),
        _ => req,
    }
}

/// What a multipart store of a file body is given.
pub(super) struct MultipartPut<'a> {
    pub bucket: &'a str,
    pub key: &'a str,
    pub files: &'a FileBody,
    /// The metadata headers of the object, `dg-*` included.
    pub headers: &'a HashMap<String, String>,
    pub metadata: &'a FileMetadata,
    pub fence: &'a RefFence,
    pub on_unsupported: OnUnsupported,
}

impl S3Backend {
    /// The single-PUT limit of this backend: the instance's own
    /// ([`S3Backend::with_put_limits`]), else [`single_put_max_bytes`].
    pub(super) fn put_limits(&self) -> (u64, u64) {
        self.put_limits
            .unwrap_or_else(|| (single_put_max_bytes(), part_bytes()))
    }

    /// THE abort request of an upload that this backend opened. Every
    /// opener binds it as `MpuAbortGuard::new(self.mpu_abort_request(..))`
    /// (the source guard in `storage/mpu_guard.rs` checks the binding).
    pub(super) fn mpu_abort_request(
        &self,
        bucket: &str,
        key: &str,
        upload_id: &str,
    ) -> impl Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
           + Send
           + Sync
           + 'static {
        let client = self.bulk_client.clone();
        let (bucket, key, upload_id) = (bucket.to_string(), key.to_string(), upload_id.to_string());
        move || {
            let (client, bucket, key, upload_id) = (
                client.clone(),
                bucket.clone(),
                key.clone(),
                upload_id.clone(),
            );
            Box::pin(async move {
                if let Err(e) = client
                    .abort_multipart_upload()
                    .bucket(&bucket)
                    .key(&key)
                    .upload_id(&upload_id)
                    .send()
                    .await
                {
                    tracing::warn!(
                        "Failed to abort multipart upload {bucket}/{key} ({upload_id}): {}",
                        aws_sdk_s3::error::DisplayErrorContext(&e)
                    );
                }
            })
        }
    }

    /// Store a file body as a backend multipart upload. The result reads
    /// like the one of a single PUT ([`FencedPut`]).
    pub(super) async fn put_file_multipart(
        &self,
        put: &MultipartPut<'_>,
    ) -> Result<FencedPut, StorageError> {
        let MultipartPut {
            bucket,
            key,
            files,
            headers,
            ..
        } = *put;
        let total = files.len();
        let preferred = self.put_limits().1;
        let Some(ranges) = plan_parts(total, preferred) else {
            return Err(StorageError::TooLarge {
                size: total,
                max: MAX_PARTS * MAX_PART_BYTES,
            });
        };
        let native = self.native_encryption.for_bucket(bucket);
        let mut create = self
            .bulk_client
            .create_multipart_upload()
            .bucket(bucket)
            .key(key)
            .content_type("application/octet-stream");
        for (k, v) in headers {
            create = create.metadata(k.clone(), v.clone());
        }
        create = apply_native_encryption(create, &native);
        let created = create
            .send()
            .await
            .map_err(|e| self.classify(bucket, &e, S3Op::CreateMpu))?;
        let Some(upload_id) = created.upload_id().map(str::to_string) else {
            return Err(StorageError::Other(
                "CreateMultipartUpload returned no upload id".into(),
            ));
        };
        let guard = MpuAbortGuard::new(self.mpu_abort_request(bucket, key, &upload_id));
        debug!(
            "S3 PUT {bucket}/{key}: {total} bytes in {} parts (upload {upload_id})",
            ranges.len()
        );
        match self.upload_and_complete(put, &upload_id, &ranges).await {
            Ok(put @ FencedPut::Stored(_)) => {
                guard.disarm();
                Ok(put)
            }
            // A refused condition and an error leave the upload open: abort.
            Ok(other) => {
                guard.abort().await;
                Ok(other)
            }
            Err(e) => {
                guard.abort().await;
                Err(e)
            }
        }
    }

    /// One part: the bytes `offset..offset + len` of the body, streamed
    /// from the files.
    async fn upload_one_part(
        &self,
        put: &MultipartPut<'_>,
        upload_id: &str,
        number: i32,
        offset: u64,
        len: u64,
    ) -> Result<aws_sdk_s3::types::CompletedPart, StorageError> {
        let (bucket, key) = (put.bucket, put.key);
        if crate::config::test_seams::test_seams().put_fail_part == Some(number) {
            return Err(StorageError::Transient(format!(
                "injected failure of part {number} (DGP_TEST_PUT_FAIL_PART)"
            )));
        }
        let part = put.files.slice(offset, len);
        let out = retry_unidentified_400(
            || async {
                let request = self
                    .bulk_client
                    .upload_part()
                    .bucket(bucket)
                    .key(key)
                    .upload_id(upload_id)
                    .part_number(number)
                    .body(part.byte_stream());
                match self.upload_config(len) {
                    Some(c) => request.customize().config_override(c).send().await,
                    None => request.send().await,
                }
            },
            |e| self.classify(bucket, &e, S3Op::UploadPart),
        )
        .await?;
        Ok(aws_sdk_s3::types::CompletedPart::builder()
            .part_number(number)
            .set_e_tag(out.e_tag().map(str::to_string))
            .build())
    }

    async fn upload_and_complete(
        &self,
        put: &MultipartPut<'_>,
        upload_id: &str,
        ranges: &[(u64, u64)],
    ) -> Result<FencedPut, StorageError> {
        use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
        let MultipartPut {
            bucket,
            key,
            files,
            metadata,
            on_unsupported,
            ..
        } = *put;
        let completed: Vec<CompletedPart> = futures::stream::iter(
            ranges
                .iter()
                .copied()
                .enumerate()
                .map(|(i, (offset, len))| {
                    self.upload_one_part(put, upload_id, i as i32 + 1, offset, len)
                }),
        )
        .buffered(PARALLEL_PARTS)
        // small: the part tags of one upload, at most 10 000 entries.
        .try_collect()
        .await?;

        let stored_size = files.len();
        let mut fence = put.fence.clone();
        loop {
            let request = self
                .bulk_client
                .complete_multipart_upload()
                .bucket(bucket)
                .key(key)
                .upload_id(upload_id)
                .multipart_upload(
                    CompletedMultipartUpload::builder()
                        .set_parts(Some(completed.clone()))
                        .build(),
                );
            let request = apply_complete_fence(request, &fence, self.if_match_dialect());
            match request.send().await {
                Ok(resp) => {
                    let etag = resp.e_tag().unwrap_or_default();
                    self.remember_listed_facts(bucket, key, etag, stored_size, metadata);
                    self.persist_listing_facts(bucket, key, etag, stored_size, metadata);
                    return Ok(FencedPut::Stored(resp.e_tag().map(str::to_string)));
                }
                Err(e) => {
                    let signal = crate::coordination::cas::sdk_error_signal(&e);
                    match fenced_write_verdict(&fence, &signal) {
                        // The ETag of a multipart object is no MD5 of its
                        // body, so a refused condition cannot be proven to
                        // be our own landed write (as a single PUT does):
                        // it reads as lost.
                        FencedWriteVerdict::Lost => return Ok(FencedPut::Lost),
                        FencedWriteVerdict::Unsupported
                            if on_unsupported == OnUnsupported::Report =>
                        {
                            return Ok(FencedPut::Unsupported);
                        }
                        FencedWriteVerdict::Unsupported => {
                            warn!("S3 PUT {bucket}/{key}: the backend has no conditional writes (501); completing without the fence");
                            fence = RefFence::Unfenced;
                            continue;
                        }
                        FencedWriteVerdict::Other
                            if on_unsupported == OnUnsupported::Report
                                && conditional_target_gone(&fence, &signal) =>
                        {
                            return Ok(FencedPut::Lost);
                        }
                        FencedWriteVerdict::Other => {}
                    }
                    return Err(self.classify(bucket, &e, S3Op::CompleteMpu));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;
    const TIB: u64 = 1024 * GIB;

    #[test]
    fn part_plans_of_known_sizes() {
        // 5.5 GiB in 64 MiB parts: 88 parts.
        let plan = plan_parts(5 * GIB + GIB / 2, PART_BYTES).unwrap();
        assert_eq!(plan.len(), 88);
        assert!(plan.iter().all(|&(_, len)| len == PART_BYTES));
        // A partial last part.
        let plan = plan_parts(PART_BYTES + 1, PART_BYTES).unwrap();
        assert_eq!(plan, vec![(0, PART_BYTES), (PART_BYTES, 1)]);
        // One part, and none.
        assert_eq!(plan_parts(7, PART_BYTES).unwrap(), vec![(0, 7)]);
        assert!(plan_parts(0, PART_BYTES).unwrap().is_empty());
    }

    #[test]
    fn the_largest_object_fits_in_ten_thousand_parts() {
        let plan = plan_parts(5 * TIB, PART_BYTES).unwrap();
        assert!(plan.len() as u64 <= MAX_PARTS);
        assert!(plan.iter().all(|&(_, len)| len <= MAX_PART_BYTES));
        assert_eq!(plan.iter().map(|&(_, l)| l).sum::<u64>(), 5 * TIB);
        // Above what 10 000 parts of 5 GiB hold: no plan.
        assert!(plan_parts(MAX_PARTS * MAX_PART_BYTES + 1, PART_BYTES).is_none());
        // A preferred size under S3's smallest part is raised to it.
        assert_eq!(part_size(GIB, 1), Some(MIN_PART_BYTES));
    }

    proptest! {
        /// Any body size and preferred part size: the parts are in order,
        /// contiguous, cover the body exactly, stay within S3's part
        /// limits (the last part may be smaller than the smallest) and
        /// number at most 10 000.
        #[test]
        fn plans_cover_the_body_within_the_limits(
            total in 0u64..(6 * TIB),
            preferred in 0u64..(8 * GIB),
        ) {
            let plan = plan_parts(total, preferred).expect("a body below 10 000 x 5 GiB has a plan");
            prop_assert!(plan.len() as u64 <= MAX_PARTS);
            let mut next = 0;
            for (i, &(offset, len)) in plan.iter().enumerate() {
                prop_assert_eq!(offset, next);
                prop_assert!(len > 0 && len <= MAX_PART_BYTES);
                if i + 1 < plan.len() {
                    prop_assert!(len >= MIN_PART_BYTES);
                }
                next = offset + len;
            }
            prop_assert_eq!(next, total);
        }
    }
}
