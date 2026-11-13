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
//! and the Complete carry [`upload_timeouts`] without a deadline for the
//! operation: an attempt and its read timeout may last
//! [`super::client::LONG_TIMEOUT`], and the stalled-stream protection
//! (`DGP_S3_STALL_GRACE_SECS`) ends a part that does not move. The
//! Complete answers after the backend joined the parts, which takes
//! minutes for an object of many GiB.
//!
//! A write with a condition (`If-None-Match`, `If-Match`) is not stored
//! with a multipart upload: see [`fence_is_conditional`].

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

/// Pure: does a write carry a condition? A conditional write is not stored
/// with a multipart upload. The proxy knows that a backend enforces a
/// condition on PutObject (the CAS probe, `coordination/cas_probe.rs`),
/// but it has no proof for CompleteMultipartUpload, and a backend that
/// ignores the header there would store a baseline over a peer's. So an
/// object above the single-PUT limit is written with a condition on no
/// backend: a caller that asks for `Report` gets "no conditional writes",
/// and any other caller gets `EntityTooLarge`.
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
        if fence_is_conditional(put.fence) {
            warn!(
                "S3 PUT {bucket}/{key}: {total} bytes need a multipart upload, which is stored \
                 without a condition on no backend; refusing the conditional write"
            );
            return match put.on_unsupported {
                OnUnsupported::Report => Ok(FencedPut::Unsupported),
                OnUnsupported::Unfenced => Err(StorageError::EntityTooLarge(format!(
                    "{bucket}/{key}: an object of {total} bytes needs a multipart upload on the \
                     storage backend, and a write with a condition (a delta baseline, a rewrite \
                     of a stored version) cannot use one"
                ))),
            };
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
                    .config_override(Self::long_upload_config())
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
            let sent = self
                .bulk_client
                .complete_multipart_upload()
                .bucket(bucket)
                .key(key)
                .upload_id(upload_id)
                .multipart_upload(
                    CompletedMultipartUpload::builder()
                        .set_parts(Some(completed.clone()))
                        .build(),
                )
                .customize()
                .config_override(Self::long_upload_config())
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
                .landed_multipart(bucket, key, expected.as_deref())
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

    /// The config of a part and of a CompleteMultipartUpload (also of a
    /// copy): no deadline for the operation, a long attempt and read timeout
    /// ([`upload_timeouts`]); the connect timeout stays.
    pub(super) fn long_upload_config() -> aws_sdk_s3::config::Builder {
        aws_sdk_s3::config::Builder::new().timeout_config(upload_timeouts(None))
    }

    /// The ETag of the object at `key` when it is the object that a
    /// Complete of parts with the ETag `expected` makes (the ETag of the
    /// object is [`multipart_etag`] of its parts); `None` for no object, for
    /// another ETag, for an ETag that proves nothing (`expected` is `None`)
    /// and for a failed HEAD.
    async fn landed_multipart(
        &self,
        bucket: &str,
        key: &str,
        expected: Option<&str>,
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
        (etag.trim().trim_matches('"') == expected).then(|| etag.to_string())
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
                    let line = String::from_utf8_lossy(&buf)
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .to_lowercase();
                    let n = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let _ = tx.send(line);
                    let reply = responses.get(n).cloned().unwrap_or_else(|| {
                        "HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n".to_string()
                    });
                    let _ = sock.write_all(reply.as_bytes()).await;
                });
            }
        });
        (endpoint, rx)
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

    /// A listener that takes a request body at about 100 KB/s (a read of at
    /// most 4 KiB every 40 ms) and answers `200` when it has it all.
    async fn slow_upload_server() -> String {
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
                        // 100 KB/s
                        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
                        match sock.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => got += n,
                        }
                    }
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
        let endpoint = slow_upload_server().await;
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
        let endpoint = slow_upload_server().await;
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

    /// A write with a condition above the single-PUT limit sends no
    /// request: `Report` callers read "no conditional writes", the others
    /// get `EntityTooLarge`. An unconditional write of the same body is
    /// stored with a multipart upload (its first request is the Create).
    #[tokio::test]
    async fn a_conditional_write_above_the_limit_is_refused_before_any_request() {
        let (endpoint, mut heads) = head_recorder(std::time::Duration::ZERO).await;
        let s3 = production_backend(&endpoint)
            .await
            .with_put_limits(MIN_PART_BYTES, MIN_PART_BYTES);
        let (_dir, path, meta) = file_and_meta(MIN_PART_BYTES as usize + 1);
        let files = [path.as_path()];
        for fence in [RefFence::Absent, RefFence::ETag("\"abc\"".into())] {
            let reported = s3
                .put_object_file_with_metadata_fenced_with(
                    "b",
                    "k",
                    &files,
                    &meta,
                    &fence,
                    OnUnsupported::Report,
                )
                .await
                .unwrap();
            assert_eq!(reported, FencedPut::Unsupported, "{fence:?}");
            let err = s3
                .put_object_file_with_metadata_fenced_with(
                    "b",
                    "k",
                    &files,
                    &meta,
                    &fence,
                    OnUnsupported::Unfenced,
                )
                .await
                .expect_err("a conditional multipart write is refused");
            assert!(matches!(err, StorageError::EntityTooLarge(_)), "{err:?}");
        }
        assert!(heads.try_recv().is_err(), "no request was sent");
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
