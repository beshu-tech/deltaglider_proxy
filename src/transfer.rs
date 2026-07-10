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

/// How one object was physically moved source→dest. Surfaced to the
/// run totals, per-object event, and jobs API so operators can see the
/// fast path working.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CopyStrategy {
    /// The `.delta` blob was shipped verbatim (no xdelta3, no full-body
    /// transfer). Saves `file_size - delta_size` egress bytes.
    DeltaPassthrough,
    /// A delta source was reconstructed (xdelta3) then re-stored.
    Reconstructed,
    /// A passthrough source was streamed via multipart (bounded memory).
    StreamedPassthrough,
    /// A passthrough source was buffered then re-stored.
    BufferedPassthrough,
}

impl CopyStrategy {
    pub fn as_str(self) -> &'static str {
        match self {
            CopyStrategy::DeltaPassthrough => "delta_passthrough",
            CopyStrategy::Reconstructed => "reconstructed",
            CopyStrategy::StreamedPassthrough => "streamed_passthrough",
            CopyStrategy::BufferedPassthrough => "buffered_passthrough",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ObjectTransferOutcome {
    pub bytes_copied: usize,
    pub strategy: CopyStrategy,
    /// At-rest storage label of the SOURCE object ("delta" / "passthrough").
    pub source_storage_label: &'static str,
    /// Logical (hydrated) source size — only meaningful on the fast path.
    pub source_file_size: u64,
    /// Egress bytes the fast path saved vs reconstruct (`file_size - delta`);
    /// 0 on every non-`DeltaPassthrough` path. Single source for metric + column.
    pub bytes_egress_saved: u64,
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

    // Delta fast path: when the source is delta-stored, try shipping the
    // `.delta` blob verbatim (seeding the dest reference if needed). Any
    // `Ok(None)` = the gate said fall back → the buffered reconstruct path
    // below runs unchanged (no duplication).
    if matches!(
        source_head.storage_info,
        crate::types::StorageInfo::Delta { .. }
    ) {
        if let Some(outcome) = delta_passthrough_copy(engine, request, &source_head).await? {
            return Ok(outcome);
        }
    }

    // Large objects: stream the source reconstruction to a spool file and store
    // from the spool — bounded memory end-to-end (no full-object Vec for copy/
    // replication). The x-ray flagged retrieve()→store() as a hidden OOM for big
    // deltas; this closes it now that the store side streams (Phase 4).
    let source_size = source_head.file_size;
    if source_size > engine.spool_store_threshold() {
        if let Some(outcome) = spooled_copy(engine, &request, &source_head, source_size).await? {
            return Ok(outcome);
        }
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
    // A delta source went through the reconstruct→re-store cycle; a
    // passthrough source was buffered then re-stored.
    let strategy = if matches!(
        source_head.storage_info,
        crate::types::StorageInfo::Delta { .. }
    ) {
        CopyStrategy::Reconstructed
    } else {
        CopyStrategy::BufferedPassthrough
    };
    Ok(ObjectTransferOutcome {
        bytes_copied: bytes,
        strategy,
        source_storage_label: source_head.storage_info.label(),
        source_file_size: source_head.file_size,
        bytes_egress_saved: 0,
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
    // Abort the upload if THIS future is dropped (killed) before complete/abort.
    let mut abort_guard = MultipartAbortGuard::new(engine.clone(), handle.clone());

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
    let pinned_head = source_head.clone();
    let metrics = engine.metrics().cloned();
    let results: Result<Vec<PartUploadResult>, String> =
        futures::stream::iter(spans.iter().copied())
            .map(|span| {
                let engine = engine.clone();
                let handle = handle.clone();
                let src_bucket = src_bucket.clone();
                let src_key = src_key.clone();
                let pinned_head = pinned_head.clone();
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
                        &pinned_head,
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
            // Explicit abort on a part failure — by ref, so it works at any
            // Arc strong count; disarm so the guard's Drop doesn't double-abort.
            engine.abort_passthrough_multipart_ref(&handle).await;
            drop(abort_guard.disarm());
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

    // Reached complete: disarm the guard (it holds a clone) so the Arc is sole-owned.
    drop(handle);
    let handle = match Arc::try_unwrap(abort_guard.disarm()) {
        Ok(h) => h,
        Err(shared) => {
            // Should be sole-owned here; if not, ABORT the upload before
            // erroring — the guard is already disarmed, so nobody else will.
            engine.abort_passthrough_multipart_ref(&shared).await;
            return Err("internal: multipart handle still shared at finish".into());
        }
    };
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
        strategy: CopyStrategy::StreamedPassthrough,
        source_storage_label: source_head.storage_info.label(),
        source_file_size: source_head.file_size,
        bytes_egress_saved: 0,
    })
}

/// Aborts the multipart upload on drop UNLESS disarmed. Covers the
/// cancellation path: when a kill drops the streaming-copy future mid-part,
/// neither the Ok nor the Err arm runs, so without this the upload is left
/// dangling on the destination (backends like B2 don't GC incomplete uploads).
/// On an armed drop we spawn a detached best-effort abort (Drop is sync).
struct MultipartAbortGuard {
    engine: Arc<DynEngine>,
    handle: Option<Arc<crate::deltaglider::PassthroughMultipartHandle>>,
}

impl MultipartAbortGuard {
    fn new(
        engine: Arc<DynEngine>,
        handle: Arc<crate::deltaglider::PassthroughMultipartHandle>,
    ) -> Self {
        Self {
            engine,
            handle: Some(handle),
        }
    }
    /// Hand the handle back; the normal complete/abort path now owns cleanup.
    fn disarm(&mut self) -> Arc<crate::deltaglider::PassthroughMultipartHandle> {
        self.handle.take().expect("disarm called twice")
    }
}

impl Drop for MultipartAbortGuard {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            let engine = self.engine.clone();
            // By-ref abort: correct even while the dropped future's other Arc
            // clones are still being torn down.
            tokio::spawn(async move { engine.abort_passthrough_multipart_ref(&handle).await });
        }
    }
}

/// Fetch one part via a native ranged GET, retrying transient failures by
/// re-issuing the GET (the range-resume). Returns the collected bytes.
async fn fetch_part_with_resume(
    engine: &Arc<DynEngine>,
    bucket: &str,
    key: &str,
    span: &PartSpan,
    source_head: &crate::types::FileMetadata,
    metrics: Option<&Arc<Metrics>>,
) -> Result<Bytes, Box<dyn std::error::Error + Send + Sync>> {
    const MAX_PART_ATTEMPTS: u32 = 4;
    let mut last_err: Option<String> = None;
    for attempt in 1..=MAX_PART_ATTEMPTS {
        match fetch_part_once(engine, bucket, key, span, source_head).await {
            Ok(bytes) => return Ok(bytes),
            Err(msg) => {
                // A generation-pin failure can never heal at the PART level
                // (the pin is fixed) — abort now; the whole-copy retry
                // re-HEADs and copies the NEW generation cleanly.
                if msg.contains(SOURCE_CHANGED_TOKEN) {
                    return Err(msg.into());
                }
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
    source_head: &crate::types::FileMetadata,
) -> Result<Bytes, String> {
    // Test-only fault injection (inert without the env var): fire a
    // transient-classified error exactly once for the named part so the
    // resume loop retries + range-resumes it.
    if let Some(e) = maybe_inject_part_failure(span.number) {
        return Err(e);
    }
    // Generation-pinned: the engine resolves the source FRESH (no cache) and
    // fails when it no longer matches `source_head` — a concurrent overwrite
    // mid-copy must abort the copy, never mix generations into the dest.
    let ranged = engine
        .retrieve_stream_range(
            bucket,
            key,
            span.start,
            span.end_inclusive,
            Some(source_head),
        )
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

/// Marker for a generation-pin failure (see retrieve_stream_range): fatal at
/// the part level, transient at the whole-copy level (retry re-HEADs).
pub(crate) const SOURCE_CHANGED_TOKEN: &str = "source changed during copy";

pub(crate) fn is_transient_copy_error(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    [
        SOURCE_CHANGED_TOKEN,
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
        // Spool budget exhaustion under concurrent large copies is a CONTENTION
        // signal, not a permanent failure — retry (with backoff) rather than
        // poisoning the object into the failure ring (finding #14).
        "spool budget exhausted",
    ]
    .iter()
    .any(|needle| m.contains(needle))
}

// ── Delta-passthrough fast path ──────────────────────────────────────
//
// Shipping a `.delta` blob verbatim only reconstructs correctly at the
// destination if the dest deltaspace holds the byte-identical reference
// the delta was encoded against. The gate `can_delta_passthrough` is the
// single decision point; corruption is impossible as long as it returns
// `Fallback` on any sha/enc doubt. v1 ships ONLY plaintext sources.

/// At-rest encryption fingerprint of a blob, derived from metadata markers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EncFingerprint {
    Plaintext,
    Encrypted { key_id: Option<String> },
}

/// Facts about the SOURCE delta needed to decide the fast path.
#[derive(Debug, Clone)]
pub(crate) struct SrcDeltaFacts {
    pub ref_sha256: String,
    pub enc: EncFingerprint,
}

/// Facts about the DEST deltaspace reference (when one exists).
#[derive(Debug, Clone)]
pub(crate) struct DestRefFacts {
    pub file_sha256: String,
    pub enc: EncFingerprint,
}

/// Gate verdict. `Fallback` carries a stable reason for diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DeltaPassthroughDecision {
    ShipVerbatim,
    SeedThenShip,
    Fallback { reason: &'static str },
}

/// True iff the two fingerprints can host the SAME verbatim blob: both
/// plaintext, or both encrypted with an equal KNOWN key_id. `Encrypted{None}`
/// is never compatible — we can't prove the two blobs share a key.
fn enc_compatible(a: &EncFingerprint, b: &EncFingerprint) -> bool {
    match (a, b) {
        (EncFingerprint::Plaintext, EncFingerprint::Plaintext) => true,
        (
            EncFingerprint::Encrypted { key_id: Some(ka) },
            EncFingerprint::Encrypted { key_id: Some(kb) },
        ) => ka == kb,
        _ => false,
    }
}

/// PURE decision: can we ship the source `.delta` verbatim to the dest?
///
/// Precedence is exact and load-bearing:
///   1. dest present AND sha differs → Fallback{ref_sha_mismatch}
///      UNCONDITIONALLY (before any enc check). A wrong reference is
///      silent corruption; nothing overrides this.
///   2. enc incompatible → Fallback{enc_incompatible}.
///   3. dest absent → SeedThenShip (we'll seed the matching reference).
///   4. dest present + sha equal + enc compatible → ShipVerbatim.
pub(crate) fn can_delta_passthrough(
    src: &SrcDeltaFacts,
    dest_ref: Option<&DestRefFacts>,
) -> DeltaPassthroughDecision {
    if let Some(dest) = dest_ref {
        if dest.file_sha256 != src.ref_sha256 {
            return DeltaPassthroughDecision::Fallback {
                reason: "ref_sha_mismatch",
            };
        }
        if !enc_compatible(&src.enc, &dest.enc) {
            return DeltaPassthroughDecision::Fallback {
                reason: "enc_incompatible",
            };
        }
        DeltaPassthroughDecision::ShipVerbatim
    } else {
        DeltaPassthroughDecision::SeedThenShip
    }
}

use crate::storage::encrypting::strip_encryption_markers;

/// Build an [`EncFingerprint`] from at-rest user-metadata markers.
fn enc_fingerprint(meta: &crate::types::FileMetadata) -> EncFingerprint {
    use crate::storage::encrypting::{ENCRYPTION_KEY_ID_KEY, ENCRYPTION_MARKER_KEY};
    if meta.user_metadata.contains_key(ENCRYPTION_MARKER_KEY) {
        EncFingerprint::Encrypted {
            key_id: meta.user_metadata.get(ENCRYPTION_KEY_ID_KEY).cloned(),
        }
    } else {
        EncFingerprint::Plaintext
    }
}

/// Try the delta fast path. `Ok(None)` = fell back; the caller runs the
/// existing reconstruct path. Enforces three corruption-defense layers
/// (gate sha-check, seed sha-assert, post-lock re-gate).
async fn delta_passthrough_copy(
    engine: &Arc<DynEngine>,
    request: ObjectTransferRequest<'_>,
    source_head: &crate::types::FileMetadata,
) -> Result<Option<ObjectTransferOutcome>, Box<dyn std::error::Error + Send + Sync>> {
    use crate::types::{ObjectKey, StorageInfo};

    // Source delta facts. `ref_sha256` comes from the HEAD; recover it via
    // delta_meta if a lite-list stub left it empty.
    let (src_prefix, src_filename, src_delta_size, mut src_ref_sha256) =
        match &source_head.storage_info {
            StorageInfo::Delta {
                ref_sha256,
                delta_size,
                ..
            } => {
                let key = ObjectKey::parse(request.source_bucket, request.source_key);
                (
                    key.prefix.clone(),
                    key.filename.clone(),
                    *delta_size,
                    ref_sha256.clone(),
                )
            }
            _ => return Ok(None),
        };
    // Safety net: HEAD normally populates ref_sha256, but if a future lite-list
    // path leaves it empty, recover via delta_meta. Empty after that → fall back
    // to reconstruct (never ship without a known reference hash).
    if src_ref_sha256.is_empty() {
        match engine
            .delta_meta(request.source_bucket, &src_prefix, &src_filename)
            .await
        {
            Ok(m) => {
                if let StorageInfo::Delta { ref_sha256, .. } = &m.storage_info {
                    src_ref_sha256 = ref_sha256.clone();
                }
            }
            Err(_) => return Ok(None),
        }
    }
    if src_ref_sha256.is_empty() {
        return Ok(None);
    }

    // ENCRYPTED sources are eligible: get_delta_raw / get_reference_raw DECRYPT
    // to plaintext, and put_delta_raw / put_reference_raw RE-ENCRYPT under the
    // DEST key (+re-stamp the dest key-id) — so a same-key-id ship is byte-
    // preserving (skips the whole-object xdelta3 reconstruct). The gate below
    // Fallbacks on enc-incompatible / Encrypted{None}. We MUST strip the source
    // encryption markers before the ship (see the `meta` strip below): a dest
    // wrapper in decrypt-only PassThrough mode (or no key) does NOT re-stamp, so
    // a surviving source marker on a now-plaintext body would make the read pick
    // the wrong key and hard-fail.
    let src_enc = enc_fingerprint(source_head);

    let src = SrcDeltaFacts {
        ref_sha256: src_ref_sha256.clone(),
        enc: src_enc,
    };

    let dest_key = ObjectKey::parse(request.destination_bucket, request.destination_key);
    let dest_prefix = dest_key.prefix.clone();
    let dest_filename = dest_key.filename.clone();

    // The shipped delta's metadata: clone the source delta metadata so the
    // LOGICAL fields (file_sha256/file_size/multipart_etag/original_name/
    // content_type/StorageInfo::Delta{}) survive; stamp provenance + strip.
    let mut meta = source_head.clone();
    if let Some(provenance) = request.provenance {
        meta.user_metadata.insert(
            provenance.metadata_key.to_string(),
            provenance.metadata_value.to_string(),
        );
    }
    for key in request.strip_user_metadata_keys {
        meta.user_metadata.remove(*key);
    }
    // Strip the SOURCE encryption markers: the dest write re-stamps them iff it
    // actually encrypts (encrypt mode), and leaves the body plaintext-with-no-
    // marker iff it doesn't (PassThrough shim / no key). Either way the stored
    // metadata matches the body — a surviving source marker on a re-encrypted or
    // passed-through body is silent corruption on read. Mirrors the reencrypt
    // job (maintenance/worker.rs strip_encryption_markers).
    strip_encryption_markers(&mut meta.user_metadata);

    // Decide AND ship under the dest prefix lock, so the gate's reference read
    // and the delta write are one critical section — mirrors the normal PUT
    // path (store.rs) and closes the gate→write TOCTOU. A concurrent reference
    // teardown/re-seed can't slip a wrong-sha reference under our delta.
    let dest_bucket = request.destination_bucket.to_string();
    let src_bucket = request.source_bucket.to_string();
    let src_prefix2 = src_prefix.clone();
    let src_filename2 = src_filename.clone();
    let dest_prefix2 = dest_prefix.clone();
    let dest_filename2 = dest_filename.clone();
    let engine2 = engine.clone();
    // Keep a copy of the shipped delta's metadata + dest bucket for the usage
    // counter after the lock is released (the originals are moved into the
    // closure). The counter must see this fast-path store too — it bypasses the
    // engine store() choke point via put_delta_raw.
    let counter_meta = meta.clone();
    let counter_dest_bucket = dest_bucket.clone();
    // Snapshot the dest's PRIOR metadata BEFORE the write — reading it after
    // returns the just-written delta and nets the overwrite to zero.
    let counter_prior = engine
        .fast_path_prior(&counter_dest_bucket, request.destination_key)
        .await;
    // `Some(ref_bytes)` = shipped (ref_bytes = bytes of a reference we SEEDED on
    // this copy, 0 if the dest already had one); `None` = fell back.
    let shipped: Result<Option<u64>, Box<dyn std::error::Error + Send + Sync>> = engine
        .with_dest_prefix_lock(&dest_prefix, || async move {
            // Re-read the dest reference UNDER the lock and re-run the SAME pure
            // gate — identical sha + enc precedence to the first read.
            let dest_ref = engine2
                .reference_meta(&dest_bucket, &dest_prefix2)
                .await
                .map(|m| DestRefFacts {
                    file_sha256: m.file_sha256.clone(),
                    enc: enc_fingerprint(&m),
                });
            let mut seeded_ref_bytes = 0u64;
            match can_delta_passthrough(&src, dest_ref.as_ref()) {
                DeltaPassthroughDecision::Fallback { .. } => return Ok(None),
                DeltaPassthroughDecision::ShipVerbatim => {}
                DeltaPassthroughDecision::SeedThenShip => {
                    // Seed the dest reference verbatim, asserting it matches
                    // src.ref_sha256 before writing (defense layer 2).
                    let ref_data = engine2.get_reference_raw(&src_bucket, &src_prefix2).await?;
                    let mut ref_meta = engine2
                        .reference_metadata_raw(&src_bucket, &src_prefix2)
                        .await?;
                    if ref_meta.file_sha256 != src.ref_sha256 {
                        return Ok(None);
                    }
                    // Same marker strip as the delta: the dest write re-stamps
                    // (encrypt) or leaves plaintext (passthrough); a surviving
                    // source marker would corrupt the reference read.
                    strip_encryption_markers(&mut ref_meta.user_metadata);
                    seeded_ref_bytes = ref_meta.file_size;
                    engine2
                        .put_reference_raw(&dest_bucket, &dest_prefix2, &ref_data, &ref_meta)
                        .await?;
                }
            }
            // Ship the delta blob verbatim — still under the lock.
            let delta_bytes = engine2
                .get_delta_raw(&src_bucket, &src_prefix2, &src_filename2)
                .await?;
            engine2
                .put_delta_raw(
                    &dest_bucket,
                    &dest_prefix2,
                    &dest_filename2,
                    &delta_bytes,
                    &meta,
                )
                .await?;
            Ok(Some(seeded_ref_bytes))
        })
        .await;
    let Some(seeded_ref_bytes) = shipped? else {
        return Ok(None);
    };

    // Record the destination contribution into the usage counter — the fast
    // path bypasses the engine store() choke point, so do it explicitly here.
    // Overwrite-aware (the dest key may already exist) + add a seeded reference.
    engine.record_fast_path_copy(
        &counter_dest_bucket,
        counter_prior.as_ref(),
        &counter_meta,
        seeded_ref_bytes,
    );

    // HEAD reports the LOGICAL size, not the delta size.
    verify_destination(
        engine,
        request,
        source_head.file_size as usize,
        source_head.multipart_etag.as_deref(),
    )
    .await?;

    let bytes_egress_saved = source_head.file_size.saturating_sub(src_delta_size);
    // Metric counts replication only — lifecycle transitions share this path
    // but shouldn't be attributed to replication egress savings.
    if request.operation == "replication" {
        if let Some(m) = engine.metrics() {
            m.replication_delta_passthrough_bytes_saved_total
                .inc_by(bytes_egress_saved);
        }
    }

    Ok(Some(ObjectTransferOutcome {
        bytes_copied: src_delta_size as usize,
        strategy: CopyStrategy::DeltaPassthrough,
        source_storage_label: "delta",
        source_file_size: source_head.file_size,
        bytes_egress_saved,
    }))
}

/// Bounded-memory copy for large objects (Phase 4.1): stream the source
/// reconstruction to a spool file, then store from the spool via
/// `store_spooled_delta`. Closes the retrieve()→store() full-RAM re-buffer the
/// x-ray flagged for copy/replication of big deltas. Returns `None` to fall back
/// to the buffered path when the source can't be streamed to a spool here.
async fn spooled_copy(
    engine: &Arc<DynEngine>,
    request: &ObjectTransferRequest<'_>,
    source_head: &crate::types::FileMetadata,
    source_size: u64,
) -> Result<Option<ObjectTransferOutcome>, Box<dyn std::error::Error + Send + Sync>> {
    use tokio::io::AsyncWriteExt;

    // Stream the (reconstructed) source to a spool file — bounded memory.
    let resp = engine
        .retrieve_stream(request.source_bucket, request.source_key)
        .await
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
            format!("source retrieve_stream failed: {e}").into()
        })?;
    let (mut stream, meta) = match resp {
        crate::deltaglider::RetrieveResponse::Streamed {
            stream, metadata, ..
        } => (stream, metadata),
        // Buffered (small) source — let the caller's buffered path handle it.
        crate::deltaglider::RetrieveResponse::Buffered { .. } => return Ok(None),
    };

    let spool = engine.spool_acquire(source_size).await?;
    {
        let mut file = tokio::fs::File::create(spool.path()).await?;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                format!("source stream error: {e}").into()
            })?;
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
    }

    let content_type = meta.content_type.clone();
    let mut user_metadata = meta.user_metadata.clone();
    if let Some(provenance) = request.provenance {
        user_metadata.insert(
            provenance.metadata_key.to_string(),
            provenance.metadata_value.to_string(),
        );
    }
    for key in request.strip_user_metadata_keys {
        user_metadata.remove(*key);
    }

    engine
        .store_spooled_delta(
            request.destination_bucket,
            request.destination_key,
            &spool,
            source_size,
            content_type,
            user_metadata,
            meta.multipart_etag.clone(),
        )
        .await
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
            format!("destination spooled store failed: {e}").into()
        })?;

    let label = source_head.storage_info.label();
    Ok(Some(ObjectTransferOutcome {
        bytes_copied: source_size as usize,
        strategy: CopyStrategy::Reconstructed,
        source_storage_label: label,
        source_file_size: source_size,
        bytes_egress_saved: 0,
    }))
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

        // Spool budget exhaustion is contention, not permanent (finding #14):
        // the Overloaded Display string must classify transient so the copy
        // retries instead of poisoning the object into the failure ring.
        assert!(is_transient_copy_error(
            "source retrieve failed: Service overloaded: spool budget exhausted; retry shortly"
        ));

        // A generation-pin change is FATAL at the part level (part loop returns
        // it immediately) but the whole-copy retry re-HEADs → transient here.
        assert!(is_transient_copy_error(&format!(
            "{}: b/k (size 10 -> 11)",
            SOURCE_CHANGED_TOKEN
        )));
    }

    #[test]
    fn copy_strategy_as_str_is_snake_case() {
        assert_eq!(CopyStrategy::DeltaPassthrough.as_str(), "delta_passthrough");
        assert_eq!(CopyStrategy::Reconstructed.as_str(), "reconstructed");
        assert_eq!(
            CopyStrategy::StreamedPassthrough.as_str(),
            "streamed_passthrough"
        );
        assert_eq!(
            CopyStrategy::BufferedPassthrough.as_str(),
            "buffered_passthrough"
        );
    }

    // ── can_delta_passthrough truth table (one named test per row) ──

    fn plain_src(ref_sha: &str) -> SrcDeltaFacts {
        SrcDeltaFacts {
            ref_sha256: ref_sha.to_string(),
            enc: EncFingerprint::Plaintext,
        }
    }
    fn enc_src(ref_sha: &str, kid: Option<&str>) -> SrcDeltaFacts {
        SrcDeltaFacts {
            ref_sha256: ref_sha.to_string(),
            enc: EncFingerprint::Encrypted {
                key_id: kid.map(str::to_string),
            },
        }
    }
    fn plain_dest(sha: &str) -> DestRefFacts {
        DestRefFacts {
            file_sha256: sha.to_string(),
            enc: EncFingerprint::Plaintext,
        }
    }
    fn enc_dest(sha: &str, kid: Option<&str>) -> DestRefFacts {
        DestRefFacts {
            file_sha256: sha.to_string(),
            enc: EncFingerprint::Encrypted {
                key_id: kid.map(str::to_string),
            },
        }
    }

    #[test]
    fn row_plaintext_dest_absent_seeds() {
        assert_eq!(
            can_delta_passthrough(&plain_src("aaa"), None),
            DeltaPassthroughDecision::SeedThenShip
        );
    }

    #[test]
    fn row_plaintext_match_plaintext_ships() {
        assert_eq!(
            can_delta_passthrough(&plain_src("aaa"), Some(&plain_dest("aaa"))),
            DeltaPassthroughDecision::ShipVerbatim
        );
    }

    #[test]
    fn row_plaintext_differ_fallback_ref_sha_mismatch() {
        assert_eq!(
            can_delta_passthrough(&plain_src("aaa"), Some(&plain_dest("bbb"))),
            DeltaPassthroughDecision::Fallback {
                reason: "ref_sha_mismatch"
            }
        );
    }

    #[test]
    fn row_plaintext_match_encrypted_fallback_enc_incompatible() {
        assert_eq!(
            can_delta_passthrough(&plain_src("aaa"), Some(&enc_dest("aaa", Some("k")))),
            DeltaPassthroughDecision::Fallback {
                reason: "enc_incompatible"
            }
        );
    }

    #[test]
    fn row_encrypted_k_match_encrypted_k_ships() {
        // Same key-id both sides → ship. The copy fn strips source markers so
        // the dest re-stamps its own key-id (byte-preserving after decrypt).
        assert_eq!(
            can_delta_passthrough(
                &enc_src("aaa", Some("k")),
                Some(&enc_dest("aaa", Some("k")))
            ),
            DeltaPassthroughDecision::ShipVerbatim
        );
    }

    #[test]
    fn row_encrypted_k_dest_absent_seeds() {
        assert_eq!(
            can_delta_passthrough(&enc_src("aaa", Some("k")), None),
            DeltaPassthroughDecision::SeedThenShip
        );
    }

    #[test]
    fn row_encrypted_k_match_encrypted_j_fallback() {
        assert_eq!(
            can_delta_passthrough(
                &enc_src("aaa", Some("k")),
                Some(&enc_dest("aaa", Some("j")))
            ),
            DeltaPassthroughDecision::Fallback {
                reason: "enc_incompatible"
            }
        );
    }

    #[test]
    fn row_encrypted_none_both_fallback_enc_incompatible() {
        assert_eq!(
            can_delta_passthrough(&enc_src("aaa", None), Some(&enc_dest("aaa", None))),
            DeltaPassthroughDecision::Fallback {
                reason: "enc_incompatible"
            }
        );
    }

    #[test]
    fn row_any_differ_fallback_before_enc_check() {
        // sha differs AND enc differs → ref_sha_mismatch wins (checked first).
        assert_eq!(
            can_delta_passthrough(&enc_src("aaa", Some("k")), Some(&enc_dest("bbb", None))),
            DeltaPassthroughDecision::Fallback {
                reason: "ref_sha_mismatch"
            }
        );
    }
}

#[cfg(test)]
mod gate_proptests {
    use super::{
        can_delta_passthrough, DeltaPassthroughDecision, DestRefFacts, EncFingerprint,
        SrcDeltaFacts,
    };
    use proptest::prelude::*;

    fn enc_strategy() -> impl Strategy<Value = EncFingerprint> {
        prop_oneof![
            Just(EncFingerprint::Plaintext),
            Just(EncFingerprint::Encrypted {
                key_id: Some("a".to_string())
            }),
            Just(EncFingerprint::Encrypted {
                key_id: Some("b".to_string())
            }),
            Just(EncFingerprint::Encrypted { key_id: None }),
        ]
    }

    fn compatible(a: &EncFingerprint, b: &EncFingerprint) -> bool {
        matches!(
            (a, b),
            (EncFingerprint::Plaintext, EncFingerprint::Plaintext)
        ) || matches!(
            (a, b),
            (
                EncFingerprint::Encrypted { key_id: Some(x) },
                EncFingerprint::Encrypted { key_id: Some(y) },
            ) if x == y
        )
    }

    proptest! {
        #[test]
        fn invariants_hold(
            src_enc in enc_strategy(),
            dest in proptest::option::of((proptest::bool::ANY, enc_strategy())),
        ) {
            // sha space is just {match, nomatch}.
            let src = SrcDeltaFacts { ref_sha256: "REF".to_string(), enc: src_enc.clone() };
            let dest_ref = dest.as_ref().map(|(sha_match, denc)| DestRefFacts {
                file_sha256: (if *sha_match { "REF" } else { "OTHER" }).to_string(),
                enc: denc.clone(),
            });
            let decision = can_delta_passthrough(&src, dest_ref.as_ref());
            // Bound to a local so the `{ .. }` pattern stays out of the
            // prop_assert format-string parser.
            let is_fallback = matches!(decision, DeltaPassthroughDecision::Fallback { .. });
            let is_ship = matches!(decision, DeltaPassthroughDecision::ShipVerbatim);
            let is_seed = matches!(decision, DeltaPassthroughDecision::SeedThenShip);

            // Invariant 1: sha-differ ⇒ always Fallback.
            if let Some(d) = dest_ref.as_ref() {
                if d.file_sha256 != src.ref_sha256 {
                    prop_assert!(is_fallback);
                }
            }
            // Invariant 2: ShipVerbatim ⇒ dest present ∧ sha equal ∧ enc compatible.
            if is_ship {
                let d = dest_ref.as_ref().expect("ship requires a dest ref");
                prop_assert_eq!(&d.file_sha256, &src.ref_sha256);
                prop_assert!(compatible(&src.enc, &d.enc));
            }
            // Invariant 3: SeedThenShip ⇒ dest absent.
            if is_seed {
                prop_assert!(dest_ref.is_none());
            }
            // Invariant 4: never Ship/Seed when enc incompatible (present dest).
            if let Some(d) = dest_ref.as_ref() {
                if !compatible(&src.enc, &d.enc) {
                    prop_assert!(is_fallback);
                }
            }
        }
    }
}

/// Regression tests for the multipart-abort paths: a dangling multipart
/// upload on backends without incomplete-upload GC (B2) accrues storage
/// forever, so EVERY failure path must reach `abort_multipart_upload`.
#[cfg(test)]
mod multipart_abort_tests {
    use super::*;
    use crate::config::Config;
    use crate::deltaglider::DeltaGliderEngine;
    use crate::storage::{MultipartUpload, StorageBackend, StorageError, UploadedPart};
    use crate::types::FileMetadata;
    use async_trait::async_trait;
    use bytes::Bytes;
    use futures::stream::BoxStream;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// Spy backend: serves one in-memory passthrough source object, fakes a
    /// native multipart destination, and records every abort. Failure modes
    /// are toggled per test.
    struct AbortSpy {
        src_bytes: Vec<u8>,
        src_meta: FileMetadata,
        fail_part: AtomicBool,
        slow_parts: AtomicBool,
        fail_complete: AtomicBool,
        aborts: Mutex<Vec<String>>,
    }

    impl AbortSpy {
        fn new(src_bytes: Vec<u8>) -> Self {
            let meta = FileMetadata::new_passthrough(
                "src.bin".to_string(),
                "0".repeat(64),
                "0".repeat(32),
                src_bytes.len() as u64,
                Some("application/octet-stream".to_string()),
            );
            Self {
                src_bytes,
                src_meta: meta,
                fail_part: AtomicBool::new(false),
                slow_parts: AtomicBool::new(false),
                fail_complete: AtomicBool::new(false),
                aborts: Mutex::new(Vec::new()),
            }
        }
    }

    fn nope() -> StorageError {
        StorageError::Other("AbortSpy: not implemented".into())
    }

    #[async_trait]
    impl StorageBackend for AbortSpy {
        async fn create_multipart_upload(
            &self,
            bucket: &str,
            _: &str,
            _: &str,
            _: &FileMetadata,
        ) -> Result<MultipartUpload, StorageError> {
            Ok(MultipartUpload {
                bucket: bucket.to_string(),
                upload_id: "spy-upload-1".to_string(),
                native: true,
                backend: None,
            })
        }
        async fn upload_part(
            &self,
            _: &MultipartUpload,
            _: &str,
            _: &str,
            part_number: i32,
            _: Bytes,
        ) -> Result<UploadedPart, StorageError> {
            if self.slow_parts.load(Ordering::Relaxed) {
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
            if self.fail_part.load(Ordering::Relaxed) {
                return Err(StorageError::Other("injected part failure".into()));
            }
            Ok(UploadedPart {
                part_number,
                etag: format!("etag-{part_number}"),
            })
        }
        async fn complete_multipart_upload(
            &self,
            _: &MultipartUpload,
            _: &str,
            _: &str,
            _: &[UploadedPart],
            _: &[Bytes],
            _: &FileMetadata,
        ) -> Result<String, StorageError> {
            if self.fail_complete.load(Ordering::Relaxed) {
                return Err(StorageError::Other("injected complete failure".into()));
            }
            Ok("final-etag".to_string())
        }
        async fn abort_multipart_upload(
            &self,
            upload: &MultipartUpload,
            _: &str,
            _: &str,
        ) -> Result<(), StorageError> {
            self.aborts.lock().unwrap().push(upload.upload_id.clone());
            Ok(())
        }

        // Source-object reads used by the streaming copy path.
        async fn get_passthrough_metadata(
            &self,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<FileMetadata, StorageError> {
            Ok(self.src_meta.clone())
        }
        async fn get_delta_metadata(
            &self,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<FileMetadata, StorageError> {
            Err(StorageError::NotFound("no delta".into()))
        }
        async fn get_passthrough_stream_range(
            &self,
            _: &str,
            _: &str,
            _: &str,
            start: u64,
            end: u64,
        ) -> Result<(BoxStream<'static, Result<Bytes, StorageError>>, u64), StorageError> {
            let end = std::cmp::min(end, self.src_bytes.len() as u64 - 1);
            let slice = self.src_bytes[start as usize..=end as usize].to_vec();
            let len = slice.len() as u64;
            Ok((
                Box::pin(futures::stream::once(async move { Ok(Bytes::from(slice)) })),
                len,
            ))
        }

        // Required by the trait; irrelevant to these tests.
        async fn create_bucket(&self, _: &str) -> Result<(), StorageError> {
            Err(nope())
        }
        async fn delete_bucket(&self, _: &str) -> Result<(), StorageError> {
            Err(nope())
        }
        async fn list_buckets(&self) -> Result<Vec<String>, StorageError> {
            Err(nope())
        }
        async fn head_bucket(&self, _: &str) -> Result<bool, StorageError> {
            Err(nope())
        }
        async fn get_reference(&self, _: &str, _: &str) -> Result<Vec<u8>, StorageError> {
            Err(nope())
        }
        async fn put_reference(
            &self,
            _: &str,
            _: &str,
            _: &[u8],
            _: &FileMetadata,
        ) -> Result<(), StorageError> {
            Err(nope())
        }
        async fn put_reference_metadata(
            &self,
            _: &str,
            _: &str,
            _: &FileMetadata,
        ) -> Result<(), StorageError> {
            Err(nope())
        }
        async fn get_reference_metadata(
            &self,
            _: &str,
            _: &str,
        ) -> Result<FileMetadata, StorageError> {
            Err(nope())
        }
        async fn has_reference(&self, _: &str, _: &str) -> Result<bool, StorageError> {
            Ok(false)
        }
        async fn delete_reference(&self, _: &str, _: &str) -> Result<(), StorageError> {
            Err(nope())
        }
        async fn get_delta(&self, _: &str, _: &str, _: &str) -> Result<Vec<u8>, StorageError> {
            Err(nope())
        }
        async fn put_delta(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &[u8],
            _: &FileMetadata,
        ) -> Result<(), StorageError> {
            Err(nope())
        }
        async fn delete_delta(&self, _: &str, _: &str, _: &str) -> Result<(), StorageError> {
            Ok(())
        }
        async fn get_passthrough(
            &self,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<Vec<u8>, StorageError> {
            Err(nope())
        }
        async fn put_passthrough(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &[u8],
            _: &FileMetadata,
        ) -> Result<(), StorageError> {
            Err(nope())
        }
        async fn delete_passthrough(&self, _: &str, _: &str, _: &str) -> Result<(), StorageError> {
            Err(nope())
        }
        async fn get_passthrough_stream(
            &self,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<BoxStream<'static, Result<Bytes, StorageError>>, StorageError> {
            Err(nope())
        }
        async fn scan_deltaspace(
            &self,
            _: &str,
            _: &str,
        ) -> Result<Vec<FileMetadata>, StorageError> {
            Err(nope())
        }
        async fn list_deltaspaces(&self, _: &str) -> Result<Vec<String>, StorageError> {
            Err(nope())
        }
        async fn total_size(&self, _: Option<&str>) -> Result<u64, StorageError> {
            Err(nope())
        }
        async fn bulk_list_objects(
            &self,
            _: &str,
            _: &str,
        ) -> Result<Vec<(String, FileMetadata)>, StorageError> {
            Err(nope())
        }
    }

    fn spy_engine(spy: AbortSpy) -> (Arc<DynEngine>, Arc<AbortSpy>) {
        let spy = Arc::new(spy);
        let backend: Box<dyn StorageBackend> = Box::new(SpyRef(spy.clone()));
        let engine =
            DeltaGliderEngine::new_with_backend(Arc::new(backend), &Config::default(), None);
        (Arc::new(engine), spy)
    }

    /// Thin forwarding wrapper so the test keeps an `Arc<AbortSpy>` for
    /// assertions while the engine owns the boxed backend.
    struct SpyRef(Arc<AbortSpy>);

    #[async_trait]
    impl StorageBackend for SpyRef {
        async fn create_multipart_upload(
            &self,
            b: &str,
            p: &str,
            f: &str,
            m: &FileMetadata,
        ) -> Result<MultipartUpload, StorageError> {
            self.0.create_multipart_upload(b, p, f, m).await
        }
        async fn upload_part(
            &self,
            u: &MultipartUpload,
            p: &str,
            f: &str,
            n: i32,
            d: Bytes,
        ) -> Result<UploadedPart, StorageError> {
            self.0.upload_part(u, p, f, n, d).await
        }
        async fn complete_multipart_upload(
            &self,
            u: &MultipartUpload,
            p: &str,
            f: &str,
            parts: &[UploadedPart],
            a: &[Bytes],
            m: &FileMetadata,
        ) -> Result<String, StorageError> {
            self.0.complete_multipart_upload(u, p, f, parts, a, m).await
        }
        async fn abort_multipart_upload(
            &self,
            u: &MultipartUpload,
            p: &str,
            f: &str,
        ) -> Result<(), StorageError> {
            self.0.abort_multipart_upload(u, p, f).await
        }
        async fn get_passthrough_metadata(
            &self,
            b: &str,
            p: &str,
            f: &str,
        ) -> Result<FileMetadata, StorageError> {
            self.0.get_passthrough_metadata(b, p, f).await
        }
        async fn get_delta_metadata(
            &self,
            b: &str,
            p: &str,
            f: &str,
        ) -> Result<FileMetadata, StorageError> {
            self.0.get_delta_metadata(b, p, f).await
        }
        async fn get_passthrough_stream_range(
            &self,
            b: &str,
            p: &str,
            f: &str,
            s: u64,
            e: u64,
        ) -> Result<(BoxStream<'static, Result<Bytes, StorageError>>, u64), StorageError> {
            self.0.get_passthrough_stream_range(b, p, f, s, e).await
        }
        async fn create_bucket(&self, b: &str) -> Result<(), StorageError> {
            self.0.create_bucket(b).await
        }
        async fn delete_bucket(&self, b: &str) -> Result<(), StorageError> {
            self.0.delete_bucket(b).await
        }
        async fn list_buckets(&self) -> Result<Vec<String>, StorageError> {
            self.0.list_buckets().await
        }
        async fn head_bucket(&self, b: &str) -> Result<bool, StorageError> {
            self.0.head_bucket(b).await
        }
        async fn get_reference(&self, b: &str, p: &str) -> Result<Vec<u8>, StorageError> {
            self.0.get_reference(b, p).await
        }
        async fn put_reference(
            &self,
            b: &str,
            p: &str,
            d: &[u8],
            m: &FileMetadata,
        ) -> Result<(), StorageError> {
            self.0.put_reference(b, p, d, m).await
        }
        async fn put_reference_metadata(
            &self,
            b: &str,
            p: &str,
            m: &FileMetadata,
        ) -> Result<(), StorageError> {
            self.0.put_reference_metadata(b, p, m).await
        }
        async fn get_reference_metadata(
            &self,
            b: &str,
            p: &str,
        ) -> Result<FileMetadata, StorageError> {
            self.0.get_reference_metadata(b, p).await
        }
        async fn has_reference(&self, b: &str, p: &str) -> Result<bool, StorageError> {
            self.0.has_reference(b, p).await
        }
        async fn delete_reference(&self, b: &str, p: &str) -> Result<(), StorageError> {
            self.0.delete_reference(b, p).await
        }
        async fn get_delta(&self, b: &str, p: &str, f: &str) -> Result<Vec<u8>, StorageError> {
            self.0.get_delta(b, p, f).await
        }
        async fn put_delta(
            &self,
            b: &str,
            p: &str,
            f: &str,
            d: &[u8],
            m: &FileMetadata,
        ) -> Result<(), StorageError> {
            self.0.put_delta(b, p, f, d, m).await
        }
        async fn delete_delta(&self, b: &str, p: &str, f: &str) -> Result<(), StorageError> {
            self.0.delete_delta(b, p, f).await
        }
        async fn get_passthrough(
            &self,
            b: &str,
            p: &str,
            f: &str,
        ) -> Result<Vec<u8>, StorageError> {
            self.0.get_passthrough(b, p, f).await
        }
        async fn put_passthrough(
            &self,
            b: &str,
            p: &str,
            f: &str,
            d: &[u8],
            m: &FileMetadata,
        ) -> Result<(), StorageError> {
            self.0.put_passthrough(b, p, f, d, m).await
        }
        async fn delete_passthrough(&self, b: &str, p: &str, f: &str) -> Result<(), StorageError> {
            self.0.delete_passthrough(b, p, f).await
        }
        async fn get_passthrough_stream(
            &self,
            b: &str,
            p: &str,
            f: &str,
        ) -> Result<BoxStream<'static, Result<Bytes, StorageError>>, StorageError> {
            self.0.get_passthrough_stream(b, p, f).await
        }
        async fn scan_deltaspace(
            &self,
            b: &str,
            p: &str,
        ) -> Result<Vec<FileMetadata>, StorageError> {
            self.0.scan_deltaspace(b, p).await
        }
        async fn list_deltaspaces(&self, b: &str) -> Result<Vec<String>, StorageError> {
            self.0.list_deltaspaces(b).await
        }
        async fn total_size(&self, b: Option<&str>) -> Result<u64, StorageError> {
            self.0.total_size(b).await
        }
        async fn bulk_list_objects(
            &self,
            b: &str,
            p: &str,
        ) -> Result<Vec<(String, FileMetadata)>, StorageError> {
            self.0.bulk_list_objects(b, p).await
        }
    }

    fn copy_request() -> ObjectTransferRequest<'static> {
        ObjectTransferRequest {
            source_bucket: "srcb",
            source_key: "src.bin",
            destination_bucket: "dstb",
            destination_key: "dst.bin",
            provenance: None,
            strip_user_metadata_keys: &[],
            operation: "test",
            upload_concurrency: Some(2),
        }
    }

    #[tokio::test]
    async fn part_failure_aborts_the_multipart_upload() {
        let spy = AbortSpy::new(vec![7u8; 64]);
        spy.fail_part.store(true, Ordering::Relaxed);
        let (engine, spy) = spy_engine(spy);
        let meta = spy.src_meta.clone();

        let res = stream_copy_passthrough(&engine, copy_request(), &meta).await;
        assert!(res.is_err(), "part failure must surface");
        assert_eq!(
            spy.aborts.lock().unwrap().as_slice(),
            &["spy-upload-1".to_string()],
            "the Err arm must abort the multipart upload exactly once"
        );
    }

    #[tokio::test]
    async fn complete_failure_aborts_the_multipart_upload() {
        let spy = AbortSpy::new(vec![7u8; 64]);
        spy.fail_complete.store(true, Ordering::Relaxed);
        let (engine, spy) = spy_engine(spy);
        let meta = spy.src_meta.clone();

        let res = stream_copy_passthrough(&engine, copy_request(), &meta).await;
        assert!(res.is_err(), "complete failure must surface");
        assert_eq!(
            spy.aborts.lock().unwrap().as_slice(),
            &["spy-upload-1".to_string()],
            "finish_passthrough_multipart must abort when complete fails"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropped_copy_future_aborts_the_multipart_upload() {
        let spy = AbortSpy::new(vec![7u8; 64]);
        spy.slow_parts.store(true, Ordering::Relaxed);
        let (engine, spy) = spy_engine(spy);
        let meta = spy.src_meta.clone();

        let eng = engine.clone();
        let task = tokio::spawn(async move {
            let _ = stream_copy_passthrough(&eng, copy_request(), &meta).await;
        });
        // Let the copy reach the (stalled) upload_part, then kill it — the
        // abort guard's Drop must abort even while part-task Arcs unwind.
        tokio::time::sleep(Duration::from_millis(200)).await;
        task.abort();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if spy.aborts.lock().unwrap().len() == 1 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "guard-drop abort never reached the backend"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}
