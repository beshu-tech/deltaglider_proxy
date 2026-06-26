// SPDX-License-Identifier: GPL-3.0-only

//! Shared engine-routed object transfer primitives.
//!
//! Replication and lifecycle transitions both need the same copy semantics:
//! retrieve through the DeltaGlider engine, store through the engine, preserve
//! multipart ETags, stamp provenance metadata, and retry narrow transient
//! transport failures.

use crate::deltaglider::DynEngine;
use crate::metrics::{bump_peak, Metrics};
use crate::storage::UploadedPart;
use crate::transfer_plan::{self, PartSpan};
use bytes::{Bytes, BytesMut};
use futures::stream::{StreamExt, TryStreamExt};
use std::sync::Arc;
use tracing::{info, warn};

/// RAII guard for one in-flight streaming-copy part. Increments
/// `parts_inflight` (+peak) on construction; records resident part bytes via
/// [`PartGuard::resident`]; subtracts resident bytes and decrements
/// `parts_inflight` on drop so an early abort still settles the gauges.
struct PartGuard {
    metrics: Arc<Metrics>,
    resident: i64,
}

impl PartGuard {
    fn new(metrics: Arc<Metrics>) -> Self {
        metrics.replication_parts_inflight.inc();
        bump_peak(
            &metrics.replication_parts_inflight,
            &metrics.replication_parts_inflight_peak,
        );
        Self {
            metrics,
            resident: 0,
        }
    }

    /// Record `len` bytes now resident in this part's buffer.
    fn resident(&mut self, len: u64) {
        self.resident = len as i64;
        self.metrics
            .replication_part_bytes_resident
            .add(self.resident);
        bump_peak(
            &self.metrics.replication_part_bytes_resident,
            &self.metrics.replication_part_bytes_resident_peak,
        );
    }
}

impl Drop for PartGuard {
    fn drop(&mut self) {
        if self.resident != 0 {
            self.metrics
                .replication_part_bytes_resident
                .sub(self.resident);
        }
        self.metrics.replication_parts_inflight.dec();
    }
}

pub(crate) const DEFAULT_COPY_MAX_ATTEMPTS: u32 = 3;
pub(crate) const REPLICATION_RULE_METADATA_KEY: &str = "dg-replication-rule";
pub(crate) const LIFECYCLE_RULE_METADATA_KEY: &str = "dg-lifecycle-rule";

#[derive(Debug, Clone, Copy)]
pub(crate) struct TransferProvenance<'a> {
    pub metadata_key: &'a str,
    pub metadata_value: &'a str,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ObjectTransferRequest<'a> {
    pub source_bucket: &'a str,
    pub source_key: &'a str,
    pub destination_bucket: &'a str,
    pub destination_key: &'a str,
    pub provenance: Option<TransferProvenance<'a>>,
    /// User-metadata keys to DROP from the copied metadata before the
    /// destination store. The re-encryption job uses this to shed stale
    /// `dg-encrypted` / `dg-encryption-key-id` markers when rewriting
    /// toward plaintext — copying them verbatim would make every later
    /// read attempt AEAD decryption of plaintext and fail. The encrypting
    /// wrapper re-stamps fresh markers on the store when the destination
    /// backend encrypts, so stripping is always safe.
    pub strip_user_metadata_keys: &'a [&'a str],
    pub operation: &'a str,
    /// In-flight parts for the streaming multipart path (Phase B). `None`
    /// falls back to the env-resolved `transfer_plan::upload_concurrency()`.
    /// Only the replication worker overrides it (from config).
    pub upload_concurrency: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ObjectTransferOutcome {
    pub bytes_copied: usize,
}

pub(crate) async fn copy_object_with_retries(
    engine: &Arc<DynEngine>,
    request: ObjectTransferRequest<'_>,
) -> Result<ObjectTransferOutcome, Box<dyn std::error::Error + Send + Sync>> {
    let mut last_err: Option<String> = None;
    for attempt in 1..=DEFAULT_COPY_MAX_ATTEMPTS {
        match copy_object_once(engine, request).await {
            Ok(outcome) => return Ok(outcome),
            Err(err) => {
                let msg = err.to_string();
                if !is_transient_copy_error(&msg) || attempt == DEFAULT_COPY_MAX_ATTEMPTS {
                    return Err(if attempt > 1 {
                        format!("{} (after {} attempts)", msg, attempt).into()
                    } else {
                        msg.into()
                    });
                }
                warn!(
                    "{} transient copy failure attempt {}/{} src={}/{} dst={}/{}: {}",
                    request.operation,
                    attempt,
                    DEFAULT_COPY_MAX_ATTEMPTS,
                    request.source_bucket,
                    request.source_key,
                    request.destination_bucket,
                    request.destination_key,
                    msg
                );
                last_err = Some(msg);
                tokio::time::sleep(std::time::Duration::from_millis(250 * attempt as u64)).await;
            }
        }
    }
    Err(last_err
        .unwrap_or_else(|| "copy failed without error detail".to_string())
        .into())
}

async fn copy_object_once(
    engine: &Arc<DynEngine>,
    request: ObjectTransferRequest<'_>,
) -> Result<ObjectTransferOutcome, Box<dyn std::error::Error + Send + Sync>> {
    // HEAD first so callers get a crisp source-disappeared failure row.
    let source_head = engine
        .head(request.source_bucket, request.source_key)
        .await
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
            format!("source head failed: {}", e).into()
        })?;

    // Large passthrough on a native-multipart destination → stream via
    // multipart with per-part range-resume (bounded memory). Delta /
    // reference / small / proxy-AES-destination objects keep the buffered
    // path below (preserves all current ETag/delta semantics + tests).
    let label = source_head.storage_info.label();
    let threshold = transfer_plan::stream_copy_threshold();
    if transfer_plan::should_stream_copy(source_head.file_size, label, threshold)
        && engine.destination_supports_native_multipart(request.destination_bucket)
    {
        return stream_copy_passthrough(engine, request, &source_head).await;
    }

    let (data, meta) = engine
        .retrieve(request.source_bucket, request.source_key)
        .await
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
            format!("source retrieve failed: {}", e).into()
        })?;

    let content_type = meta.content_type.clone();
    let mut user_metadata = meta.user_metadata.clone();
    let bytes = data.len();

    if let Some(provenance) = request.provenance {
        user_metadata.insert(
            provenance.metadata_key.to_string(),
            provenance.metadata_value.to_string(),
        );
    }
    for key in request.strip_user_metadata_keys {
        user_metadata.remove(*key);
    }

    if let Some(mp_etag) = meta.multipart_etag.clone() {
        engine
            .store_with_multipart_etag(
                request.destination_bucket,
                request.destination_key,
                &data,
                content_type,
                user_metadata,
                mp_etag,
            )
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                format!("destination store failed: {}", e).into()
            })?;
    } else {
        engine
            .store(
                request.destination_bucket,
                request.destination_key,
                &data,
                content_type,
                user_metadata,
            )
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                format!("destination store failed: {}", e).into()
            })?;
    }

    verify_destination(
        engine,
        request,
        bytes,
        source_head.multipart_etag.as_deref(),
    )
    .await?;
    Ok(ObjectTransferOutcome {
        bytes_copied: bytes,
    })
}

/// One pipelined part result: the upload receipt + (for buffering backends
/// only) the part bytes retained for `complete`'s assembly.
type PartUploadResult = (UploadedPart, Option<(i32, Bytes)>);

/// Stream a large passthrough object source→dest via multipart, with
/// per-part range-resume and bounded memory.
///
/// Each part is an independent ranged GET (`engine.retrieve_stream_range`)
/// collected into one `Bytes` then `upload_part`-ed. Up to
/// `upload_concurrency` parts run concurrently via `buffer_unordered`, so
/// peak memory is O(upload_concurrency × part_size), NOT O(object_size).
/// A transient per-part failure retries JUST that part by re-issuing the
/// ranged GET. Any unrecoverable error aborts the multipart upload.
async fn stream_copy_passthrough(
    engine: &Arc<DynEngine>,
    request: ObjectTransferRequest<'_>,
    source_head: &crate::types::FileMetadata,
) -> Result<ObjectTransferOutcome, Box<dyn std::error::Error + Send + Sync>> {
    let total = source_head.file_size;
    let part_size = transfer_plan::multipart_part_size();
    let concurrency = request
        .upload_concurrency
        .unwrap_or_else(transfer_plan::upload_concurrency)
        .clamp(1, 16);
    let spans = transfer_plan::plan_parts(total, part_size);

    let mut user_metadata = source_head.user_metadata.clone();
    if let Some(provenance) = request.provenance {
        user_metadata.insert(
            provenance.metadata_key.to_string(),
            provenance.metadata_value.to_string(),
        );
    }
    for key in request.strip_user_metadata_keys {
        user_metadata.remove(*key);
    }

    let handle = engine
        .begin_passthrough_multipart(
            request.destination_bucket,
            request.destination_key,
            total,
            source_head.content_type.clone(),
            user_metadata,
        )
        .await
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
            format!("multipart create failed: {}", e).into()
        })?;
    let native = handle.native();
    let handle = Arc::new(handle);

    info!(
        "{} streaming multipart copy src={}/{} dst={}/{} ({} bytes, {} parts, concurrency={})",
        request.operation,
        request.source_bucket,
        request.source_key,
        request.destination_bucket,
        request.destination_key,
        total,
        spans.len(),
        concurrency,
    );

    // Pipeline each part: ranged GET (range-resume on transient failure) →
    // upload_part → drop the bytes (native backends). buffer_unordered bounds
    // BOTH the in-flight GETs AND the held bytes to O(concurrency × part),
    // NOT O(object). Non-native (filesystem) backends retain the bytes so
    // `finish` can assemble them; that path isn't memory-critical (local).
    let src_bucket = request.source_bucket.to_string();
    let src_key = request.source_key.to_string();
    let metrics = engine.metrics().cloned();
    let results: Result<Vec<PartUploadResult>, String> =
        futures::stream::iter(spans.iter().copied())
            .map(|span| {
                let engine = engine.clone();
                let handle = handle.clone();
                let src_bucket = src_bucket.clone();
                let src_key = src_key.clone();
                let metrics = metrics.clone();
                async move {
                    // Guard increments parts_inflight on entry, holds resident
                    // bytes, and decrements both on drop (covers early abort).
                    let mut guard = metrics.clone().map(PartGuard::new);
                    maybe_part_barrier().await;
                    let bytes = fetch_part_with_resume(
                        &engine,
                        &src_bucket,
                        &src_key,
                        &span,
                        metrics.as_ref(),
                    )
                    .await
                    .map_err(|e| format!("part {} fetch failed: {}", span.number, e))?;
                    let len = bytes.len() as u64;
                    if let Some(g) = guard.as_mut() {
                        g.resident(len);
                    }
                    let retained = if native {
                        None
                    } else {
                        Some((span.number, bytes.clone()))
                    };
                    let part = engine
                        .upload_passthrough_part(&handle, span.number, bytes)
                        .await
                        .map_err(|e| format!("upload_part {} failed: {}", span.number, e))?;
                    if let Some(m) = metrics.as_ref() {
                        m.replication_multipart_parts_total.inc();
                        m.replication_bytes_streamed_total.inc_by(len);
                    }
                    Ok::<PartUploadResult, String>((part, retained))
                }
            })
            .buffer_unordered(concurrency)
            .try_collect()
            .await;

    let collected = match results {
        Ok(v) => v,
        Err(e) => {
            abort_shared_handle(engine, handle).await;
            return Err(e.into());
        }
    };

    let mut parts: Vec<UploadedPart> = Vec::with_capacity(collected.len());
    let mut retained: Vec<(i32, Bytes)> = Vec::new();
    for (part, keep) in collected {
        parts.push(part);
        if let Some(r) = keep {
            retained.push(r);
        }
    }
    retained.sort_by_key(|(n, _)| *n);
    let assembled: Vec<Bytes> = retained.into_iter().map(|(_, b)| b).collect();

    // Hashes come from the COPY source (a copy doesn't recompute them) so the
    // streaming path never holds the whole object to hash it.
    let sha256 = source_head.file_sha256.clone();
    let md5 = source_head.md5.clone();
    let multipart_etag = source_head.multipart_etag.clone();

    let handle =
        Arc::try_unwrap(handle).map_err(|_| -> Box<dyn std::error::Error + Send + Sync> {
            "internal: multipart handle still shared at finish".into()
        })?;
    let result = engine
        .finish_passthrough_multipart(handle, parts, assembled, sha256, md5, multipart_etag)
        .await
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
            format!("multipart complete failed: {}", e).into()
        })?;

    let bytes = result.metadata.file_size as usize;
    verify_destination(
        engine,
        request,
        bytes,
        source_head.multipart_etag.as_deref(),
    )
    .await?;
    Ok(ObjectTransferOutcome {
        bytes_copied: bytes,
    })
}

/// Abort a shared multipart handle (best-effort). Unwraps the Arc when
/// it's the sole owner; otherwise the in-flight tasks have already failed
/// and the upload is abandoned (the backend GCs incomplete uploads).
async fn abort_shared_handle(
    engine: &Arc<DynEngine>,
    handle: Arc<crate::deltaglider::PassthroughMultipartHandle>,
) {
    if let Ok(h) = Arc::try_unwrap(handle) {
        engine.abort_passthrough_multipart(h).await;
    }
}

/// Fetch one part via a native ranged GET, retrying transient failures by
/// re-issuing the GET (the range-resume). Returns the collected bytes.
async fn fetch_part_with_resume(
    engine: &Arc<DynEngine>,
    bucket: &str,
    key: &str,
    span: &PartSpan,
    metrics: Option<&Arc<Metrics>>,
) -> Result<Bytes, Box<dyn std::error::Error + Send + Sync>> {
    const MAX_PART_ATTEMPTS: u32 = 4;
    let mut last_err: Option<String> = None;
    for attempt in 1..=MAX_PART_ATTEMPTS {
        match fetch_part_once(engine, bucket, key, span).await {
            Ok(bytes) => return Ok(bytes),
            Err(msg) => {
                if !is_transient_copy_error(&msg) || attempt == MAX_PART_ATTEMPTS {
                    return Err(msg.into());
                }
                if let Some(m) = metrics {
                    m.replication_part_retries_total.inc();
                }
                warn!(
                    "transient part {} fetch failure attempt {}/{} ({}-{}): {}",
                    span.number, attempt, MAX_PART_ATTEMPTS, span.start, span.end_inclusive, msg
                );
                last_err = Some(msg);
                tokio::time::sleep(std::time::Duration::from_millis(200 * attempt as u64)).await;
            }
        }
    }
    Err(last_err
        .unwrap_or_else(|| "part fetch failed".to_string())
        .into())
}

/// One ranged GET → collect into a single `Bytes` (≤ part_size). Errors are
/// stringified so `is_transient_copy_error` can classify them.
async fn fetch_part_once(
    engine: &Arc<DynEngine>,
    bucket: &str,
    key: &str,
    span: &PartSpan,
) -> Result<Bytes, String> {
    // Test-only fault injection (inert without the env var): fire a
    // transient-classified error exactly once for the named part so the
    // resume loop retries + range-resumes it.
    if let Some(e) = maybe_inject_part_failure(span.number) {
        return Err(e);
    }
    let ranged = engine
        .retrieve_stream_range(bucket, key, span.start, span.end_inclusive)
        .await
        .map_err(|e| format!("ranged retrieve failed: {}", e))?;
    let (stream, content_length, _meta) = ranged.ok_or_else(|| {
        // None means the object isn't natively range-able (delta/unmanaged).
        // The caller only enters the streaming path for passthrough objects,
        // so this is a genuine error (concurrent strategy flip).
        "ranged retrieve unavailable for object".to_string()
    })?;

    let expected = span.len();
    let mut buf = BytesMut::with_capacity(expected.min(64 * 1024 * 1024) as usize);
    let mut stream = stream;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("part body stream error: {}", e))?;
        buf.extend_from_slice(&chunk);
    }
    let got = buf.len() as u64;
    // content_length==0 signals "full stream, not range" — a backend that
    // didn't honour the Range. Validate we got exactly the span length.
    if got != expected {
        return Err(format!(
            "part {} short read: expected {} bytes, got {} (content_length={})",
            span.number, expected, got, content_length
        ));
    }
    Ok(buf.freeze())
}

async fn verify_destination(
    engine: &Arc<DynEngine>,
    request: ObjectTransferRequest<'_>,
    expected_bytes: usize,
    expected_multipart_etag: Option<&str>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let dest = engine
        .head(request.destination_bucket, request.destination_key)
        .await
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
            format!("destination verify head failed: {}", e).into()
        })?;
    if dest.file_size != expected_bytes as u64 {
        return Err(format!(
            "destination verify failed: expected {} bytes, found {}",
            expected_bytes, dest.file_size
        )
        .into());
    }
    if let Some(expected) = expected_multipart_etag {
        if dest.multipart_etag.as_deref() != Some(expected) {
            return Err(format!(
                "destination verify failed: expected multipart etag {:?}, found {:?}",
                expected, dest.multipart_etag
            )
            .into());
        }
    }
    Ok(())
}

/// Test seam: when `DGP_TEST_FAIL_PART_ONCE=<part#>` is set, return a
/// transient-classified error the FIRST time that part is fetched (once per
/// process via `compare_exchange`). Inert in prod (env unset → None).
fn maybe_inject_part_failure(part_number: i32) -> Option<String> {
    use std::sync::atomic::{AtomicI32, Ordering};
    static FIRED: AtomicI32 = AtomicI32::new(-1);
    let target: i32 = crate::config::env_parse_with_default("DGP_TEST_FAIL_PART_ONCE", -1);
    if target < 0 || target != part_number {
        return None;
    }
    // compare_exchange(-1 → part#) succeeds for exactly one caller.
    if FIRED
        .compare_exchange(-1, part_number, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        return Some("connection reset (injected)".to_string());
    }
    None
}

/// Test seam: when `DGP_TEST_PART_BARRIER=1`, async-sleep a small fixed delay
/// (`DGP_TEST_PART_DELAY_MS`, default 150ms) AFTER a part's inflight gauge is
/// bumped so >=concurrency parts are co-resident — making the inflight peak
/// DETERMINISTICALLY reach the configured concurrency. Inert in prod.
async fn maybe_part_barrier() {
    if crate::config::env_bool("DGP_TEST_PART_BARRIER", false) {
        let ms: u64 = crate::config::env_parse_with_default("DGP_TEST_PART_DELAY_MS", 150);
        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
    }
}

pub(crate) fn is_transient_copy_error(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    [
        "failed to read response body",
        "streaming error",
        "connection reset",
        "connection closed",
        "connection aborted",
        "timed out",
        "timeout",
        "temporary failure",
        "service unavailable",
        "slowdown",
        "internalerror",
        "broken pipe",
        // Transient source-backend 5xx. Hetzner phrases 503 as "throttled" and
        // 502/504 as "failed (status=50x)" — none match the words above, so
        // they were landing in the failure ring instead of being retried.
        "throttled",
        "status=502",
        "status=503",
        "status=504",
        "bad gateway",
        "gateway timeout",
    ]
    .iter()
    .any(|needle| m.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_copy_error_classification_is_narrow() {
        assert!(is_transient_copy_error(
            "source retrieve failed: Storage error: S3 error: Failed to read response body: streaming error"
        ));
        assert!(is_transient_copy_error("connection reset by peer"));
        assert!(is_transient_copy_error("503 SlowDown"));

        // Exact source-backend (Hetzner) transient strings from the failure ring.
        assert!(is_transient_copy_error(
            "source retrieve failed: Storage error: Backend throttled: head_object throttled (status=503): service error"
        ));
        assert!(is_transient_copy_error(
            "source retrieve failed: Storage error: S3 error: head_object failed (status=502): service error"
        ));
        assert!(is_transient_copy_error(
            "source retrieve failed: Storage error: S3 error: get_object failed (status=504): service error"
        ));

        assert!(!is_transient_copy_error(
            "destination store failed: Storage error: Bucket not found: test-bucket"
        ));
        assert!(!is_transient_copy_error("AccessDenied"));
        assert!(!is_transient_copy_error("NoSuchKey"));
    }
}
