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
//!
//! Time limits. The client's read timeout (`DGP_S3_READ_TIMEOUT_SECS`,
//! 60 s) is a deadline for the whole request, body included: a part of
//! 64 MiB that needs longer to leave never gets an answer, and each retry
//! sends it again (measured against Hetzner Object Storage, where all 20
//! UploadPart requests of a 5.5 GiB upload ended with no status). So a part
//! carries [`upload_timeouts`] without a deadline for the operation: an
//! attempt and its read timeout may last [`super::client::LONG_TIMEOUT`].
//! The stalled-stream protection (`DGP_S3_STALL_GRACE_SECS`) ends a part
//! whose body does not move (the body says that it is at its end as soon as
//! its last byte is out, `FileBody`: a wait for the answer is no stall); the
//! wait for the answer of a part is bounded by its attempt timeout. The
//! Complete answers after the backend joined the parts, which takes minutes
//! for an object of many GiB: an attempt waits
//! `DGP_S3_COMPLETE_TIMEOUT_SECS` (900 s) for it, and the SDK sends at most
//! 3 attempts.
//!
//! A write with a condition (`If-None-Match`, `If-Match`) is stored with a
//! multipart upload only on a backend whose probe proved a conditional
//! Complete: see [`fence_is_conditional`].

use super::client::upload_timeouts;
use super::file_body::FileBody;
use super::*;
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

/// Pure: does a write carry a condition? Such a write is stored with a
/// multipart upload only when the backend is proven to enforce a condition
/// on CompleteMultipartUpload ([`S3Backend::conditional_complete_proven`],
/// the probe `cas_probe::probe_conditional_complete`): a backend that
/// ignores the header there would store a baseline over a peer's. Without
/// the proof nothing is sent and the caller gets
/// [`StorageError::ConditionalMultipartRefused`], never an unconditional
/// write where a conditional one was required.
pub(super) fn fence_is_conditional(fence: &RefFence) -> bool {
    match fence {
        RefFence::Unfenced => false,
        RefFence::Absent => true,
        RefFence::ETag(e) => !e.is_empty(),
    }
}

/// Pure: the ETag that S3 gives a multipart object of parts with these
/// ETags: the MD5 of their MD5s, a dash and the part count. `None` when a
/// part ETag is not an MD5 (an object that SSE-KMS or SSE-C encrypts has
/// ETags that are not): then the ETag of the object proves nothing.
pub(super) fn multipart_etag(part_etags: &[String]) -> Option<String> {
    use md5::Digest;
    let mut digests = Vec::with_capacity(part_etags.len() * 16);
    for etag in part_etags {
        let bare = etag.trim().trim_matches('"');
        if bare.len() != 32 {
            return None;
        }
        digests.extend_from_slice(&hex::decode(bare).ok()?);
    }
    Some(format!(
        "{}-{}",
        hex::encode(md5::Md5::digest(&digests)),
        part_etags.len()
    ))
}

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
/// the backend's dialect for an ETag). Only a backend that the probe proved
/// ([`S3Backend::conditional_complete_proven`]) gets them.
fn apply_complete_fence(
    req: aws_sdk_s3::operation::complete_multipart_upload::builders::CompleteMultipartUploadFluentBuilder,
    fence: &RefFence,
    dialect: crate::coordination::if_match::IfMatchDialect,
) -> aws_sdk_s3::operation::complete_multipart_upload::builders::CompleteMultipartUploadFluentBuilder
{
    use crate::coordination::if_match::IfMatchRequest;
    match fence {
        RefFence::Absent => req.if_none_match("*"),
        RefFence::ETag(e) if !e.is_empty() => req.if_match_etag(e, dialect),
        _ => req,
    }
}

/// Pure: is the object that a HEAD found the one that this write made? Its
/// ETag is the composite of this write's part ETags (`expected`: the same
/// BYTES), and its user metadata is exactly what this write sent (`wanted`:
/// the same write: `dg-created-at` is the time of this store). The bytes
/// alone prove nothing: an older object with the same bytes and other
/// metadata is at the key when this Complete really failed, and taking it
/// for ours would report a write that the backend did not make (and cache
/// the new metadata over the old object).
pub(super) fn is_own_multipart_write(
    head_etag: &str,
    head_metadata: Option<&HashMap<String, String>>,
    expected: &str,
    wanted: &HashMap<String, String>,
) -> bool {
    head_etag.trim().trim_matches('"') == expected && user_metadata_equal(head_metadata, wanted)
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
        // A write with a condition needs a backend that enforces one on
        // CompleteMultipartUpload; nothing is sent otherwise (never an
        // unconditional write where a conditional one was required).
        if fence_is_conditional(put.fence) && !self.conditional_complete_proven() {
            let backend = self
                .health_key
                .as_ref()
                .map_or(String::new(), |(name, _)| format!(" '{name}'"));
            let why = format!(
                "{bucket}/{key}: a write with a condition (If-None-Match or If-Match) of \
                 {total} bytes, more than the 5 GiB of one PUT request, needs a multipart \
                 upload, and the storage backend{backend} is not proven to enforce a condition \
                 on CompleteMultipartUpload (the capability probe checks it at start and at \
                 \"Check again\"; GET /_/api/admin/backends shows `conditional_complete`)"
            );
            warn!("S3 PUT: {why}");
            return Err(StorageError::ConditionalMultipartRefused(why));
        }
        let preferred = self.put_limits().1;
        let Some(ranges) = plan_parts(total, preferred) else {
            return Err(StorageError::TooLarge {
                size: total,
                max: MAX_PARTS * MAX_PART_BYTES,
            });
        };
        let native = self.native_encryption.for_bucket(bucket);
        let created = retry_unidentified_400(
            || {
                let mut create = self
                    .bulk_client
                    .create_multipart_upload()
                    .bucket(bucket)
                    .key(key)
                    .content_type("application/octet-stream");
                for (k, v) in headers {
                    create = create.metadata(k.clone(), v.clone());
                }
                apply_native_encryption(create, &native).send()
            },
            |e| self.classify(bucket, &e, S3Op::CreateMpu),
        )
        .await?;
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
            // The earlier parts are on the backend by now: the abort has
            // something to remove.
            crate::config::test_seams::let_earlier_parts_finish().await;
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
                // No deadline of the whole operation: see the module docs.
                request
                    .customize()
                    .config_override(Self::part_config())
                    .send()
                    .await
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
        let part_etags: Vec<String> = completed
            .iter()
            .filter_map(|p| p.e_tag().map(str::to_string))
            .collect();
        let expected = multipart_etag(&part_etags);
        let drop_first_answer = crate::config::test_seams::test_seams().put_drop_complete_answer;
        let mut backoff = UNIDENTIFIED_400_BACKOFF_MS.iter();
        for attempt in 0.. {
            // The write's condition (only a proven backend gets one: see
            // `put_file_multipart`).
            let request = apply_complete_fence(
                self.bulk_client
                    .complete_multipart_upload()
                    .bucket(bucket)
                    .key(key)
                    .upload_id(upload_id)
                    .multipart_upload(
                        CompletedMultipartUpload::builder()
                            .set_parts(Some(completed.clone()))
                            .build(),
                    ),
                put.fence,
                self.if_match_dialect(),
            );
            let sent = request
                .customize()
                .config_override(self.complete_config())
                .send()
                .await;
            // A debug seam: the backend completed the upload, and its
            // answer is lost (a dropped connection, a timeout).
            let failure = match sent {
                Ok(resp) if !(drop_first_answer && attempt == 0) => {
                    let etag = resp.e_tag().unwrap_or_default();
                    self.remember_listed_facts(bucket, key, etag, stored_size, metadata);
                    self.persist_listing_facts(bucket, key, etag, stored_size, metadata);
                    return Ok(FencedPut::Stored(resp.e_tag().map(str::to_string)));
                }
                Ok(_) => None,
                Err(e) => Some(e),
            };
            // The Complete may have landed although it failed here: a
            // retry of it answers `NoSuchUpload`, a late answer is a
            // timeout. The object of this upload is the one at the key, so
            // that is a success, and the client does not upload every part
            // again.
            if let Some(etag) = self
                .landed_multipart(bucket, key, expected.as_deref(), put.headers)
                .await
            {
                self.remember_listed_facts(bucket, key, &etag, stored_size, metadata);
                self.persist_listing_facts(bucket, key, &etag, stored_size, metadata);
                return Ok(FencedPut::Stored(Some(etag)));
            }
            let Some(e) = failure else {
                return Err(StorageError::Transient(
                    "injected: the answer of CompleteMultipartUpload was lost \
                     (DGP_TEST_PUT_DROP_COMPLETE_ANSWER)"
                        .into(),
                ));
            };
            // A refused condition: another writer changed the object (the
            // backend proved it enforces the condition). Never an overwrite.
            let signal = crate::coordination::cas::sdk_error_signal(&e);
            match fenced_write_verdict(put.fence, &signal) {
                FencedWriteVerdict::Lost => return Ok(FencedPut::Lost),
                FencedWriteVerdict::Other
                    if put.on_unsupported == OnUnsupported::Report
                        && conditional_target_gone(put.fence, &signal) =>
                {
                    return Ok(FencedPut::Lost);
                }
                FencedWriteVerdict::Unsupported | FencedWriteVerdict::Other => {}
            }
            // A 400 without a request id (Hetzner): sent again, now that
            // the object is known not to be there.
            if let Some(ms) = is_unidentified_400(&e).then(|| backoff.next()).flatten() {
                warn!("S3 CompleteMultipartUpload {bucket}/{key}: a 400 without a request id, retrying in {ms}ms: {e:?}");
                tokio::time::sleep(std::time::Duration::from_millis(*ms)).await;
                continue;
            }
            return Err(self.classify(bucket, &e, S3Op::CompleteMpu));
        }
        unreachable!("the loop returns on every path")
    }

    /// The config of a part: no deadline for the operation, a long attempt
    /// and read timeout ([`upload_timeouts`]); the connect timeout stays.
    /// The stalled-stream protection ends a part that does not move.
    pub(super) fn part_config() -> aws_sdk_s3::config::Builder {
        aws_sdk_s3::config::Builder::new().timeout_config(upload_timeouts(None))
    }

    /// The config of a CompleteMultipartUpload (also of a copy's): an
    /// attempt waits `DGP_S3_COMPLETE_TIMEOUT_SECS` for its answer
    /// ([`complete_timeouts`]).
    pub(super) fn complete_config(&self) -> aws_sdk_s3::config::Builder {
        aws_sdk_s3::config::Builder::new()
            .timeout_config(super::client::complete_timeouts(self.complete_timeout))
    }

    /// The ETag of the object at `key` when it is the object that THIS
    /// write made: see [`is_own_multipart_write`]. `None` for no object,
    /// for another one, for an ETag that proves nothing (`expected` is
    /// `None`) and for a failed HEAD.
    async fn landed_multipart(
        &self,
        bucket: &str,
        key: &str,
        expected: Option<&str>,
        wanted: &HashMap<String, String>,
    ) -> Option<String> {
        let expected = expected?;
        BACKEND_HEAD_REQUESTS.inc();
        let head = self
            .client
            .head_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .ok()?;
        let etag = head.e_tag()?;
        is_own_multipart_write(etag, head.metadata(), expected, wanted).then(|| etag.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{backend_with_short_read_timeout, production_backend};
    use super::*;
    use proptest::prelude::*;

    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;
    const TIB: u64 = 1024 * GIB;

    /// A listener that answers every request with an empty `200` (and an
    /// ETag) `delay` after it read the request head, and sends each head
    /// (the request line and headers, lower-cased) to the channel.
    async fn head_recorder(
        delay: std::time::Duration,
    ) -> (String, tokio::sync::mpsc::UnboundedReceiver<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let tx = tx.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let end = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
                    let head = String::from_utf8_lossy(&buf[..end])
                        .to_lowercase()
                        .replace('\r', "");
                    let _ = tx.send(head);
                    tokio::time::sleep(delay).await;
                    let _ = sock
                        .write_all(b"HTTP/1.1 200 OK\r\netag: \"abc\"\r\ncontent-length: 0\r\n\r\n")
                        .await;
                });
            }
        });
        (endpoint, rx)
    }

    /// A listener that answers its n-th request with `responses[n]` (a whole
    /// HTTP response; an empty `200` after the last) and sends each request
    /// line to the channel.
    async fn scripted_server(
        responses: Vec<String>,
    ) -> (String, tokio::sync::mpsc::UnboundedReceiver<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let next = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let responses = std::sync::Arc::new(responses);
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let (tx, next, responses) = (tx.clone(), next.clone(), responses.clone());
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let end = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
                    let head = String::from_utf8_lossy(&buf[..end]).to_lowercase();
                    let line = head.lines().next().unwrap_or_default().to_string();
                    // Take the whole body before the answer: a reply that
                    // leaves a body unread resets the connection, and the SDK
                    // sends the request again.
                    let len: usize = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length: "))
                        .and_then(|v| v.trim().parse().ok())
                        .unwrap_or(0);
                    let mut got = buf.len() - end;
                    while got < len {
                        match sock.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => got += n,
                        }
                    }
                    // A HEAD or a DELETE gets the plain answer and takes no
                    // place in the script: how many attempts the SDK makes
                    // is not the test's business.
                    let scripted = !(line.starts_with("head ") || line.starts_with("delete "));
                    let n = if scripted {
                        next.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                    } else {
                        usize::MAX
                    };
                    let _ = tx.send(line);
                    let reply = responses.get(n).cloned().unwrap_or_else(|| {
                        "HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n".to_string()
                    });
                    // `@<ms>@HTTP/1.1 ...`: answer after `<ms>`.
                    let reply = match reply.strip_prefix('@').and_then(|r| r.split_once('@')) {
                        Some((ms, rest)) => {
                            tokio::time::sleep(std::time::Duration::from_millis(
                                ms.parse().unwrap(),
                            ))
                            .await;
                            rest.to_string()
                        }
                        None => reply,
                    };
                    let _ = sock.write_all(reply.as_bytes()).await;
                });
            }
        });
        (endpoint, rx)
    }

    /// A Complete that the backend does not answer ends at
    /// `DGP_S3_COMPLETE_TIMEOUT_SECS` (here 1 s; the backend answers after
    /// 20 s), not at the hour of an upload without a deadline: a hung Complete
    /// would hold the object lock, the detached slot and the spool.
    #[tokio::test]
    async fn a_complete_that_is_not_answered_ends_at_the_complete_timeout() {
        let xml = "<?xml version=\"1.0\"?><InitiateMultipartUploadResult><Bucket>b</Bucket>\
                   <Key>k</Key><UploadId>uid1</UploadId></InitiateMultipartUploadResult>";
        let created = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/xml\r\ncontent-length: {}\r\n\r\n{xml}",
            xml.len()
        );
        let part = "HTTP/1.1 200 OK\r\netag: \"900150983cd24fb0d6963f7d28e17f72\"\r\ncontent-length: 0\r\n\r\n";
        let (endpoint, mut lines) = scripted_server(vec![
            created,
            part.to_string(),
            part.to_string(),
            format!("@20000@{part}"),
            format!("@20000@{part}"),
            format!("@20000@{part}"),
        ])
        .await;
        let mut s3 = production_backend(&endpoint)
            .await
            .with_put_limits(MIN_PART_BYTES, MIN_PART_BYTES);
        s3.complete_timeout = std::time::Duration::from_secs(1);
        let (_dir, path, meta) = file_and_meta(MIN_PART_BYTES as usize + 1);
        let started = std::time::Instant::now();
        let err = s3
            .put_object_file_with_metadata("b", "k", &[path.as_path()], &meta)
            .await
            .expect_err("no answer");
        let mut completes = 0;
        while let Ok(l) = lines.try_recv() {
            completes += usize::from(l.starts_with("post /b/k?uploadid="));
        }
        // A timeout is a 503 for the client; the SDK's attempts (up to 3)
        // are each cut at 1 s.
        assert!(matches!(err, StorageError::Unavailable(_)), "{err:?}");
        assert!(
            (1..=3).contains(&completes),
            "{completes} Complete attempts"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(15),
            "ended after {:?}",
            started.elapsed()
        );
    }

    /// A Hetzner 400 without a request id is sent again by the
    /// CreateMultipartUpload (as by the PutObject and the UploadPart): the
    /// next request is a second Create, and the upload goes on to its parts.
    #[tokio::test]
    async fn a_400_without_a_request_id_is_retried_by_the_create() {
        let no_id = "HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\n\r\n".to_string();
        let xml = "<?xml version=\"1.0\"?><InitiateMultipartUploadResult><Bucket>b</Bucket>\
                   <Key>k</Key><UploadId>uid1</UploadId></InitiateMultipartUploadResult>";
        let created = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/xml\r\ncontent-length: {}\r\n\r\n{xml}",
            xml.len()
        );
        // The part is refused for good (a request id): the upload ends.
        let refused =
            "HTTP/1.1 400 Bad Request\r\nx-amz-request-id: r1\r\ncontent-length: 0\r\n\r\n"
                .to_string();
        let (endpoint, mut lines) = scripted_server(vec![no_id, created, refused]).await;
        let s3 = production_backend(&endpoint)
            .await
            .with_put_limits(MIN_PART_BYTES, MIN_PART_BYTES);
        let (_dir, path, meta) = file_and_meta(MIN_PART_BYTES as usize + 1);
        let _ = s3
            .put_object_file_with_metadata("b", "k", &[path.as_path()], &meta)
            .await;
        let mut seen = Vec::new();
        while let Ok(Some(l)) =
            tokio::time::timeout(std::time::Duration::from_millis(300), lines.recv()).await
        {
            seen.push(l);
        }
        assert!(seen[0].starts_with("post /b/k?uploads"), "{seen:?}");
        assert!(seen[1].starts_with("post /b/k?uploads"), "{seen:?}");
        assert!(seen[2].starts_with("put /b/k?x-id=uploadpart"), "{seen:?}");
    }

    /// A listener that takes a request body with a read of at most 4 KiB
    /// every `read_delay_ms` (40 ms: about 100 KB/s) and answers `200`
    /// `answer_delay_ms` after it has it all.
    async fn slow_upload_server(read_delay_ms: u64, answer_delay_ms: u64) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                    }
                    let end = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
                    let head = String::from_utf8_lossy(&buf[..end]).to_lowercase();
                    let len: usize = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length: "))
                        .unwrap()
                        .trim()
                        .parse()
                        .unwrap();
                    let mut got = buf.len() - end;
                    while got < len {
                        tokio::time::sleep(std::time::Duration::from_millis(read_delay_ms)).await;
                        match sock.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => got += n,
                        }
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(answer_delay_ms)).await;
                    let _ = sock
                        .write_all(b"HTTP/1.1 200 OK\r\netag: \"abc\"\r\ncontent-length: 0\r\n\r\n")
                        .await;
                });
            }
        });
        endpoint
    }

    /// A part whose body takes longer than the read timeout to leave (about
    /// 3 s against 1 s) still gets its answer. The read timeout of the client
    /// is a deadline for the whole request, body included: with it, a part
    /// of 64 MiB over a link that moves less than 1 MiB/s per connection
    /// never completed, and every retry sent it again (Hetzner, 20 of 20
    /// parts).
    #[tokio::test]
    async fn a_part_that_outlasts_the_read_timeout_completes() {
        let endpoint = slow_upload_server(40, 0).await;
        let s3 = backend_with_short_read_timeout(&endpoint).await;
        let (_dir, path, meta) = file_and_meta(300_000);
        upload_part_with(&s3, &path, &meta, 300_000, 300_000)
            .await
            .expect("the slow body is not cut by the read timeout");
    }

    /// The same for the PutObject of a file, and of bytes in memory: one
    /// rule (`upload_config`) for every upload.
    #[tokio::test]
    async fn a_put_that_outlasts_the_read_timeout_completes() {
        let endpoint = slow_upload_server(40, 0).await;
        let s3 = backend_with_short_read_timeout(&endpoint).await;
        let (_dir, path, meta) = file_and_meta(300_000);
        s3.put_object_file_with_metadata("b", "k", &[path.as_path()], &meta)
            .await
            .expect("a PUT of a file");
        s3.put_object_with_metadata("b", "k2", bytes::Bytes::from(vec![1u8; 300_000]), &meta)
            .await
            .expect("a PUT of bytes");
    }

    /// A file of `len` bytes and the metadata of a passthrough object.
    fn file_and_meta(len: usize) -> (tempfile::TempDir, std::path::PathBuf, FileMetadata) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, vec![7u8; len]).unwrap();
        let meta = FileMetadata::new_passthrough(
            "o".into(),
            "0".repeat(64),
            "0".repeat(32),
            len as u64,
            None,
        );
        (dir, path, meta)
    }

    /// The request head of one UploadPart of `part_len` bytes of a file of
    /// `len` bytes, sent by `s3` to the recorder.
    async fn upload_part_with(
        s3: &S3Backend,
        path: &std::path::Path,
        meta: &FileMetadata,
        len: u64,
        part_len: u64,
    ) -> Result<aws_sdk_s3::types::CompletedPart, StorageError> {
        let files = FileBody::open(&[path]).await.unwrap();
        let headers = HashMap::new();
        let put = MultipartPut {
            bucket: "b",
            key: "k",
            files: &files,
            headers: &headers,
            metadata: meta,
            fence: &RefFence::Unfenced,
            on_unsupported: OnUnsupported::Unfenced,
        };
        assert!(part_len <= len);
        s3.upload_one_part(&put, "uid", 1, 0, part_len).await
    }

    /// An UploadPart is sent like the PutObject of the same bytes: a
    /// `Content-Length` of the exact body, an unsigned payload, and no
    /// `aws-chunked` encoding, trailing checksum, checksum header or
    /// `Expect`. A body that the SDK wraps in `aws-chunked` with a trailing
    /// checksum is one that a backend may not take, and its length is not
    /// the length of the file range. The client is built as production
    /// builds it (`S3Backend::new`: request checksums only when required).
    #[tokio::test]
    async fn an_upload_part_is_sent_like_a_put_object() {
        let (endpoint, mut heads) = head_recorder(std::time::Duration::ZERO).await;
        let s3 = production_backend(&endpoint).await;
        let (_dir, path, meta) = file_and_meta(100_000);
        s3.put_object_file_with_metadata("b", "k", &[path.as_path()], &meta)
            .await
            .unwrap();
        let put = heads.recv().await.unwrap();
        upload_part_with(&s3, &path, &meta, 100_000, 100_000)
            .await
            .unwrap();
        let part = heads.recv().await.unwrap();
        assert!(part.starts_with("put /b/k?x-id=uploadpart&partnumber=1&uploadid=uid "));
        for (what, head) in [("PutObject", &put), ("UploadPart", &part)] {
            assert!(
                head.contains("\ncontent-length: 100000\n"),
                "{what}: {head}"
            );
            assert!(
                head.contains("\nx-amz-content-sha256: unsigned-payload\n"),
                "{what}: {head}"
            );
            for absent in [
                "aws-chunked",
                "x-amz-trailer",
                "x-amz-checksum",
                "x-amz-sdk-checksum-algorithm",
                "x-amz-decoded-content-length",
                "transfer-encoding",
                "content-encoding",
                "expect:",
            ] {
                assert!(!head.contains(absent), "{what} sends {absent}: {head}");
            }
        }
    }

    /// A part has no deadline for the whole operation. The deadline of a
    /// single PUT (`DGP_BACKEND_REQUEST_TIMEOUT_SECS` plus one second per
    /// MiB, retries included) was the only limit of a part that moves:
    /// 94 s for a part of 64 MiB, and every retry spent a share of it. The
    /// client here has an operation deadline of 0.3 s (the production
    /// client has 30 s), and the backend answers after 1.5 s.
    #[tokio::test]
    async fn a_part_has_no_deadline_for_the_whole_operation() {
        let (endpoint, _heads) = head_recorder(std::time::Duration::from_millis(1500)).await;
        let mut s3 = production_backend(&endpoint).await;
        let cfg = crate::config::BackendConfig::S3 {
            session_token: None,
            endpoint: Some(endpoint.clone()),
            region: "us-east-1".to_string(),
            force_path_style: true,
            access_key_id: Some("AKIA".to_string()),
            secret_access_key: Some("secret".to_string()),
            allow_local: true,
        };
        s3.client = S3Backend::build_client_with(&cfg, Some(std::time::Duration::from_millis(300)))
            .await
            .unwrap();
        let (_dir, path, meta) = file_and_meta(10);
        upload_part_with(&s3, &path, &meta, 10, 10)
            .await
            .expect("the part waits for the backend: no deadline of the whole operation");
    }

    /// A write with a condition above the single-PUT limit, on a backend
    /// whose conditional Complete is not proven, sends no request and gets a
    /// true error (`ConditionalMultipartRefused`, naming the 5 GiB and the
    /// gap) from every caller: never an unconditional write where a
    /// conditional one was required, never a false "no conditional writes".
    /// An unconditional write of the same body is stored with a multipart
    /// upload (its first request is the Create), and so is a conditional one
    /// on a backend that the probe proved.
    #[tokio::test]
    async fn a_conditional_write_above_the_limit_is_refused_before_any_request() {
        let (endpoint, mut heads) = head_recorder(std::time::Duration::ZERO).await;
        let s3 = production_backend(&endpoint)
            .await
            .with_put_limits(MIN_PART_BYTES, MIN_PART_BYTES);
        let (_dir, path, meta) = file_and_meta(MIN_PART_BYTES as usize + 1);
        let files = [path.as_path()];
        for fence in [RefFence::Absent, RefFence::ETag("\"abc\"".into())] {
            for on_unsupported in [OnUnsupported::Report, OnUnsupported::Unfenced] {
                let err = s3
                    .put_object_file_with_metadata_fenced_with(
                        "b",
                        "k",
                        &files,
                        &meta,
                        &fence,
                        on_unsupported,
                    )
                    .await
                    .expect_err("a conditional multipart write is refused");
                let StorageError::ConditionalMultipartRefused(why) = &err else {
                    panic!("{err:?}");
                };
                assert!(
                    why.contains("5 GiB") && why.contains("CompleteMultipartUpload"),
                    "{why}"
                );
            }
        }
        assert!(heads.try_recv().is_err(), "no request was sent");
        // A probe that proved the conditional Complete opens the path: the
        // first request is the Create (the answer is no upload, so it ends).
        let proven = production_backend(&endpoint)
            .await
            .with_put_limits(MIN_PART_BYTES, MIN_PART_BYTES)
            .with_conditional_profile(crate::coordination::if_match::ConditionalProfile {
                conditional_complete: Some(true),
                ..Default::default()
            });
        let _ = proven
            .put_object_file_with_metadata_fenced_with(
                "b",
                "k",
                &files,
                &meta,
                &RefFence::Absent,
                OnUnsupported::Report,
            )
            .await;
        let first = heads.recv().await.unwrap();
        assert!(first.starts_with("post /b/k?uploads"), "{first}");
        // Unknown (the probe could not tell) is no proof either.
        let unknown = production_backend(&endpoint)
            .await
            .with_put_limits(MIN_PART_BYTES, MIN_PART_BYTES)
            .with_conditional_profile(crate::coordination::if_match::ConditionalProfile {
                conditional_complete: None,
                ..Default::default()
            });
        assert!(!unknown.conditional_complete_proven());
        while heads.try_recv().is_ok() {}
        // An empty ETag is no condition (a backend that does not fence).
        assert!(!fence_is_conditional(&RefFence::ETag(String::new())));
        assert!(!fence_is_conditional(&RefFence::Unfenced));
        // Without a condition the write goes to the backend as a multipart
        // upload: its first request is CreateMultipartUpload.
        let _ = s3
            .put_object_file_with_metadata_fenced_with(
                "b",
                "k",
                &files,
                &meta,
                &RefFence::Unfenced,
                OnUnsupported::Unfenced,
            )
            .await;
        let first = heads.recv().await.unwrap();
        assert!(first.starts_with("post /b/k?uploads"), "{first}");
    }

    /// The object that a HEAD finds is this write's only when its bytes AND
    /// its metadata are this write's: an older object with the same bytes
    /// and another `dg-created-at` is not (a real Complete failure would
    /// then be reported as a success).
    #[test]
    fn an_older_object_with_the_same_bytes_is_not_this_write() {
        let m = |created: &str| -> HashMap<String, String> {
            [
                ("dg-created-at".to_string(), created.to_string()),
                ("dg-file-sha256".to_string(), "abc".to_string()),
            ]
            .into()
        };
        let part = hex::encode(<md5::Md5 as md5::Digest>::digest(b"p"));
        let expected = multipart_etag(&[part]).unwrap();
        let quoted = format!("\"{expected}\"");
        let ours = m("2026-11-13T10:00:00Z");
        // This write's object.
        assert!(is_own_multipart_write(
            &quoted,
            Some(&ours),
            &expected,
            &ours
        ));
        // The same bytes from an earlier write.
        let older = m("2026-11-12T10:00:00Z");
        assert!(!is_own_multipart_write(
            &quoted,
            Some(&older),
            &expected,
            &ours
        ));
        // Other bytes, or no metadata at all.
        assert!(!is_own_multipart_write(
            "\"other-1\"",
            Some(&ours),
            &expected,
            &ours
        ));
        assert!(!is_own_multipart_write(&quoted, None, &expected, &ours));
    }

    /// The ETag of a multipart object: the MD5 of the parts' MD5s and the
    /// part count (a known vector), and nothing for an ETag that is no MD5.
    #[test]
    fn the_etag_of_a_multipart_object_is_the_md5_of_the_part_md5s() {
        // md5(md5("a") ++ md5("b")) with the quotes that S3 puts around a
        // part ETag.
        let a = format!(
            "\"{}\"",
            hex::encode(<md5::Md5 as md5::Digest>::digest(b"a"))
        );
        let b = hex::encode(<md5::Md5 as md5::Digest>::digest(b"b"));
        let mut both = Vec::new();
        both.extend_from_slice(&<md5::Md5 as md5::Digest>::digest(b"a"));
        both.extend_from_slice(&<md5::Md5 as md5::Digest>::digest(b"b"));
        let want = format!(
            "{}-2",
            hex::encode(<md5::Md5 as md5::Digest>::digest(&both))
        );
        assert_eq!(multipart_etag(&[a.clone(), b]), Some(want));
        assert_eq!(multipart_etag(&[a, "\"not-an-md5\"".into()]), None);
        assert_eq!(multipart_etag(&["zz".repeat(16)]), None);
        assert!(multipart_etag(&[]).is_some());
    }

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
