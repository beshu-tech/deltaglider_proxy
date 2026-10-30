// SPDX-License-Identifier: BUSL-1.1

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

/// Drop the rule provenance markers from metadata a CLIENT copy carries
/// over. A marker says "this rule wrote the object", and the rule's delete
/// paths act on it; a copy made by a client (CopyObject, the CLI) was not
/// written by the rule.
pub(crate) fn strip_rule_provenance(meta: &mut std::collections::HashMap<String, String>) {
    for key in [REPLICATION_RULE_METADATA_KEY, LIFECYCLE_RULE_METADATA_KEY] {
        meta.remove(key);
    }
}

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
    /// falls back to `RuntimeTuning::upload_concurrency` (`DGP_UPLOAD_CONCURRENCY`).
    /// Only the replication worker overrides it (from config).
    pub upload_concurrency: Option<usize>,
    /// Keep the source's created-at on the destination instead of the copy
    /// time. True for every copy that stands for the same object: migrate,
    /// re-encrypt, replication (a replica must not look newer than its
    /// source, or newer-wins and destination ages go wrong) and lifecycle
    /// transition (the archived object keeps its age). False only for the
    /// admin bulk copy, which makes a new object, as S3 CopyObject does.
    pub keep_created_at: bool,
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
    /// The backend copied a passthrough source itself
    /// ([`crate::storage::StorageBackend::copy_within`]): no byte passed
    /// through the proxy.
    ServerSide,
}

impl CopyStrategy {
    pub fn as_str(self) -> &'static str {
        match self {
            CopyStrategy::DeltaPassthrough => "delta_passthrough",
            CopyStrategy::Reconstructed => "reconstructed",
            CopyStrategy::StreamedPassthrough => "streamed_passthrough",
            CopyStrategy::BufferedPassthrough => "buffered_passthrough",
            CopyStrategy::ServerSide => "server_side",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ObjectTransferOutcome {
    pub bytes_copied: usize,
    pub strategy: CopyStrategy,
    /// At-rest storage label of the SOURCE object ("delta" / "passthrough").
    pub source_storage_label: &'static str,
    /// Logical (hydrated) source size: what a client downloads. Every copy
    /// path sets it.
    pub source_file_size: u64,
    /// Egress bytes the fast path saved vs reconstruct (`file_size - delta`);
    /// 0 on every non-`DeltaPassthrough` path. Single source for metric + column.
    pub bytes_egress_saved: u64,
}

impl ObjectTransferOutcome {
    /// The `content_length` an object event reports: the object's logical
    /// size. NOT `bytes_copied`, which counts transferred bytes and is the
    /// stored delta size on the delta fast path (43 B for a 1 MB object).
    pub fn content_length(&self) -> u64 {
        self.source_file_size
    }
}

/// Copy one object; the source HEAD comes from `source` when the caller
/// holds one (its listing or HEAD), else from one HEAD here. The copy keeps
/// the source's created-at from that same metadata
/// ([`ObjectTransferRequest::keep_created_at`]).
///
/// Retries only what the SDK does not retry itself
/// ([`CopyClass::copy_retry_helps`]): the source changed under the copy (the
/// retry HEADs it again), or the proxy itself was busy. A throttle, a 5xx
/// or a timeout already went through the SDK's own attempts; sending the
/// whole copy again multiplied them (3 copy attempts of 3 SDK attempts).
pub(crate) async fn copy_object_from(
    engine: &Arc<DynEngine>,
    request: ObjectTransferRequest<'_>,
    source: Option<&crate::types::FileMetadata>,
) -> Result<ObjectTransferOutcome, CopyError> {
    copy_object_known(engine, request, source, None).await
}

/// [`copy_object_from`], and what the destination key holds when the
/// caller read it already (`destination_prior`): the write then reads no
/// prior. A caller whose destination no one else writes in between (a
/// maintenance job behind its write gate) passes a plain prior: the write
/// also deletes no stored form that the read found absent. The replication
/// event consumer HEADs the destination before the copy takes the key's
/// write lock ([`DynEngine::head_with_counter_prior`]), so its prior is a
/// [`crate::deltaglider::CounterPrior::ReadAhead`]: it counts, but a client
/// PUT in that window may have stored the other form, so the write still
/// deletes that form. That write skews only the usage counter (the next
/// usage scan corrects it). The first attempt alone uses the given source
/// and prior: a retry follows a failed attempt that may have written the
/// destination, so it reads both again.
pub(crate) async fn copy_object_known(
    engine: &Arc<DynEngine>,
    request: ObjectTransferRequest<'_>,
    source: Option<&crate::types::FileMetadata>,
    destination_prior: Option<&crate::deltaglider::CounterPrior>,
) -> Result<ObjectTransferOutcome, CopyError> {
    let mut source = source.cloned();
    let mut prior = destination_prior;
    let mut attempt = 1;
    loop {
        let attempt_result = copy_attempt(engine, request, source.take(), prior.take()).await;
        let err = match attempt_result {
            Ok(outcome) => return Ok(outcome),
            Err(err) => err,
        };
        if !err.class.copy_retry_helps() || attempt == DEFAULT_COPY_MAX_ATTEMPTS {
            return Err(if attempt > 1 {
                let class = err.class;
                CopyError::new(class, format!("{err} (after {attempt} attempts)"))
            } else {
                err
            });
        }
        warn!(
            "{} copy attempt {}/{} failed, retrying src={}/{} dst={}/{}: {}",
            request.operation,
            attempt,
            DEFAULT_COPY_MAX_ATTEMPTS,
            request.source_bucket,
            request.source_key,
            request.destination_bucket,
            request.destination_key,
            err
        );
        tokio::time::sleep(std::time::Duration::from_millis(250 * attempt as u64)).await;
        attempt += 1;
    }
}

/// [`copy_object_from`] for a caller without the source's metadata.
pub(crate) async fn copy_object_with_retries(
    engine: &Arc<DynEngine>,
    request: ObjectTransferRequest<'_>,
) -> Result<ObjectTransferOutcome, CopyError> {
    copy_object_from(engine, request, None).await
}

/// One copy attempt: HEAD the source unless `source` is given, then copy,
/// stamping the source's created-at on the destination when the request
/// keeps it ([`crate::types::with_created_at`]).
async fn copy_attempt(
    engine: &Arc<DynEngine>,
    request: ObjectTransferRequest<'_>,
    source: Option<crate::types::FileMetadata>,
    prior: Option<&crate::deltaglider::CounterPrior>,
) -> Result<ObjectTransferOutcome, CopyError> {
    // A HEAD first so callers get a crisp source-disappeared failure row.
    let source_head = match source {
        Some(meta) => meta,
        None => engine
            .head(request.source_bucket, request.source_key)
            .await
            .map_err(|e| CopyError::engine("source head failed", e))?,
    };
    // The destination's write lock, as every write of an object takes it:
    // no other write of this key on this process interleaves its stored
    // forms with the copy's. Per attempt, so a retry's back-off holds none.
    let lock = engine
        .lock_object_write(request.destination_bucket, request.destination_key)
        .await;
    if request.keep_created_at {
        crate::types::with_created_at(
            source_head.created_at,
            copy_object_once(engine, &lock, request, &source_head, prior),
        )
        .await
    } else {
        copy_object_once(engine, &lock, request, &source_head, prior).await
    }
}

/// `prior`: see [`copy_object_known`]; the buffered path and the delta
/// fast path use it, the streamed paths read their own.
async fn copy_object_once(
    engine: &Arc<DynEngine>,
    lock: &crate::deltaglider::ObjectWriteGuard,
    request: ObjectTransferRequest<'_>,
    source_head: &crate::types::FileMetadata,
    prior: Option<&crate::deltaglider::CounterPrior>,
) -> Result<ObjectTransferOutcome, CopyError> {
    // A passthrough source that the destination stores as it is: the
    // backend copies it itself when it can (same backend instance, same
    // encryption key); no byte passes through the proxy.
    if matches!(
        source_head.storage_info,
        crate::types::StorageInfo::Passthrough
    ) {
        if let Some(outcome) = server_side_copy(engine, lock, request, source_head, prior).await? {
            return Ok(outcome);
        }
    }

    // Large passthrough on a native-multipart destination → stream via
    // multipart with per-part range-resume (bounded memory). Delta /
    // reference / small / proxy-AES-destination objects keep the buffered
    // path below (preserves all current ETag/delta semantics + tests).
    let label = source_head.storage_info.label();
    let threshold = engine.tuning().stream_copy_threshold;
    if transfer_plan::should_stream_copy(source_head.file_size, label, threshold)
        && engine.destination_supports_native_multipart(request.destination_bucket)
    {
        return stream_copy_passthrough(engine, lock, request, source_head).await;
    }

    // Delta fast path: when the source is delta-stored, try shipping the
    // `.delta` blob verbatim (seeding the dest reference if needed). Any
    // `Ok(None)` = the gate said fall back → the buffered reconstruct path
    // below runs unchanged (no duplication). Only when the destination
    // would store this object as a delta itself: a bucket with compression
    // off (or a key that is not delta-eligible) gets the plain object.
    let fields = DestinationFields::of_transfer(&request, source_head);
    if matches!(
        source_head.storage_info,
        crate::types::StorageInfo::Delta { .. }
    ) && engine.write_tries_delta(
        request.destination_bucket,
        request.destination_key,
        &fields.user_metadata,
        source_head.file_size,
    ) {
        if let Some(shipped) =
            delta_passthrough_copy(engine, lock, request, source_head, prior, fields).await?
        {
            return Ok(shipped.outcome);
        }
    }

    // Large objects: stream the source reconstruction to a spool file and store
    // from the spool — bounded memory end-to-end (no full-object Vec for copy/
    // replication). The x-ray flagged retrieve()→store() as a hidden OOM for big
    // deltas; this closes it now that the store side streams (Phase 4).
    let source_size = source_head.file_size;
    if source_size > engine.spool_threshold() {
        if let Some(outcome) =
            spooled_copy(engine, lock, &request, source_head, source_size).await?
        {
            return Ok(outcome);
        }
    }

    let (data, meta) = engine
        .retrieve(request.source_bucket, request.source_key)
        .await
        .map_err(|e| CopyError::engine("source retrieve failed", e))?;
    // The HEAD may be the caller's, taken a while ago: bytes of another
    // generation would land under its multipart ETag and created-at.
    if !source_head.file_sha256.is_empty()
        && !meta.file_sha256.is_empty()
        && source_head.file_sha256 != meta.file_sha256
    {
        return Err(CopyError::source_changed(format_args!(
            "{}/{} was overwritten after HEAD",
            request.source_bucket, request.source_key
        )));
    }

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
    // The source's `dg-encrypted` / `dg-encryption-key-id` markers describe the
    // SOURCE object's at-rest encryption, which is meaningless for the freshly
    // re-stored destination: `engine.retrieve` already returned plaintext bytes.
    // If the destination backend encrypts, its wrapper re-stamps the correct
    // marker on store; if it does NOT (a plaintext backend, or a decrypt-only
    // PassThrough shim during a migration), a stale marker makes the replica
    // undecryptable on read. Strip unconditionally — matching the client-facing
    // CopyObject handler and the delta fast path.
    crate::storage::encrypting::strip_encryption_markers(&mut user_metadata);

    let stored = engine
        .store_known_prior(
            lock,
            request.destination_bucket,
            request.destination_key,
            // The retrieved Vec moves into the body: no copy.
            &Bytes::from(data),
            content_type,
            user_metadata,
            meta.multipart_etag.clone(),
            prior.cloned(),
        )
        .await
        .map_err(|e| CopyError::engine("destination store failed", e))?;

    verify_destination(
        engine,
        request,
        &stored.metadata,
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
    lock: &crate::deltaglider::ObjectWriteGuard,
    request: ObjectTransferRequest<'_>,
    source_head: &crate::types::FileMetadata,
) -> Result<ObjectTransferOutcome, CopyError> {
    let total = source_head.file_size;
    let tuning = engine.tuning();
    let part_size = tuning.multipart_part_size;
    let concurrency = request
        .upload_concurrency
        .unwrap_or(tuning.upload_concurrency)
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
    // Strip the source's at-rest encryption markers (see `copy_object_once`):
    // the destination re-stamps its own on store, and a stale marker on a
    // non-encrypting destination yields an undecryptable replica.
    crate::storage::encrypting::strip_encryption_markers(&mut user_metadata);

    // Hashes come from the COPY source (a copy doesn't recompute them) so the
    // streaming path never holds the whole object to hash it. They go in at
    // create: a native S3 complete stores no metadata.
    let handle = engine
        .begin_passthrough_multipart(
            lock,
            request.destination_bucket,
            request.destination_key,
            crate::deltaglider::MultipartObjectFacts {
                total_size: total,
                content_type: source_head.content_type.clone(),
                user_metadata,
                sha256: source_head.file_sha256.clone(),
                md5: source_head.md5.clone(),
                multipart_etag: source_head.multipart_etag.clone(),
            },
        )
        .await
        .map_err(|e| CopyError::engine("multipart create failed", e))?;
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
    let results: Result<Vec<PartUploadResult>, CopyError> =
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
                    .map_err(|e| e.context(format_args!("part {} fetch failed", span.number)))?;
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
                        .map_err(|e| {
                            CopyError::engine(&format!("upload_part {} failed", span.number), e)
                        })?;
                    if let Some(m) = metrics.as_ref() {
                        m.replication_multipart_parts_total.inc();
                        m.replication_bytes_streamed_total.inc_by(len);
                    }
                    Ok::<PartUploadResult, CopyError>((part, retained))
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
            return Err(e);
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

    // Reached complete: disarm the guard (it holds a clone) so the Arc is sole-owned.
    drop(handle);
    let handle = match Arc::try_unwrap(abort_guard.disarm()) {
        Ok(h) => h,
        Err(shared) => {
            // Should be sole-owned here; if not, ABORT the upload before
            // erroring — the guard is already disarmed, so nobody else will.
            engine.abort_passthrough_multipart_ref(&shared).await;
            return Err(CopyError::new(
                CopyClass::Permanent,
                "internal: multipart handle still shared at finish",
            ));
        }
    };
    let result = engine
        .finish_passthrough_multipart(lock, handle, parts, assembled)
        .await
        .map_err(|e| CopyError::engine("multipart complete failed", e))?;

    let bytes = result.metadata.file_size as usize;
    verify_destination(
        engine,
        request,
        &result.metadata,
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

/// Fetch one part via a native ranged GET; a body that broke off part-way
/// is fetched again (the range-resume). Returns the collected bytes.
async fn fetch_part_with_resume(
    engine: &Arc<DynEngine>,
    bucket: &str,
    key: &str,
    span: &PartSpan,
    source_head: &crate::types::FileMetadata,
    metrics: Option<&Arc<Metrics>>,
) -> Result<Bytes, CopyError> {
    const MAX_PART_ATTEMPTS: u32 = 4;
    let mut attempt = 1;
    loop {
        let err = match fetch_part_once(engine, bucket, key, span, source_head).await {
            Ok(bytes) => return Ok(bytes),
            Err(err) => err,
        };
        // Only a body that broke off part-way is fetched again: the SDK
        // already retried the GET request itself (a throttle, a 5xx, a
        // timeout), so another GET multiplied its attempts. A
        // generation-pin failure never heals at the PART level (the pin is
        // fixed); the whole copy re-HEADs and copies the new generation.
        if !err.body_broke || err.class == CopyClass::SourceChanged || attempt == MAX_PART_ATTEMPTS
        {
            return Err(err);
        }
        if let Some(m) = metrics {
            m.replication_part_retries_total.inc();
        }
        warn!(
            "transient part {} fetch failure attempt {}/{} ({}-{}): {}",
            span.number, attempt, MAX_PART_ATTEMPTS, span.start, span.end_inclusive, err
        );
        tokio::time::sleep(std::time::Duration::from_millis(200 * attempt as u64)).await;
        attempt += 1;
    }
}

/// One ranged GET → collect into a single `Bytes` (≤ part_size).
async fn fetch_part_once(
    engine: &Arc<DynEngine>,
    bucket: &str,
    key: &str,
    span: &PartSpan,
    source_head: &crate::types::FileMetadata,
) -> Result<Bytes, CopyError> {
    // Test-only fault injection (inert without the env var): fire a
    // transient error exactly once for the named part so the resume loop
    // retries + range-resumes it.
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
        .map_err(|e| CopyError::engine("ranged retrieve failed", e))?;
    let (stream, content_length, _meta) = ranged.ok_or_else(|| {
        // None means the object isn't natively range-able (delta/unmanaged).
        // The caller only enters the streaming path for passthrough objects,
        // so this is a genuine error (concurrent strategy flip).
        CopyError::new(
            CopyClass::Permanent,
            "ranged retrieve unavailable for object",
        )
    })?;

    let expected = span.len();
    let mut buf = BytesMut::with_capacity(expected.min(64 * 1024 * 1024) as usize);
    let mut stream = stream;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| CopyError::body("part body stream error", e))?;
        buf.extend_from_slice(&chunk);
    }
    let got = buf.len() as u64;
    // content_length==0 signals "full stream, not range" — a backend that
    // didn't honour the Range. Validate we got exactly the span length.
    if got != expected {
        return Err(CopyError::new(
            CopyClass::Permanent,
            format!(
                "part {} short read: expected {} bytes, got {} (content_length={})",
                span.number, expected, got, content_length
            ),
        ));
    }
    Ok(buf.freeze())
}

/// Verify the copy's destination with ONE HEAD: of the stored form that
/// the write produced (`written`, its result), not both forms
/// (`engine.head`): the write deleted the other form, or knew it absent.
async fn verify_destination(
    engine: &Arc<DynEngine>,
    request: ObjectTransferRequest<'_>,
    written: &crate::types::FileMetadata,
    expected_bytes: usize,
    expected_multipart_etag: Option<&str>,
) -> Result<(), CopyError> {
    let key = crate::types::ObjectKey::parse(request.destination_bucket, request.destination_key);
    let dest = if written.is_delta() {
        engine
            .delta_meta(request.destination_bucket, &key.prefix, &key.filename)
            .await
    } else {
        engine
            .passthrough_meta(request.destination_bucket, &key.prefix, &key.filename)
            .await
    }
    .map_err(|e| CopyError::storage("destination verify head failed", e))?;
    verify_verdict(&dest, expected_bytes, None, expected_multipart_etag).map(|_| ())
}

/// Which way the verify accepted the destination.
#[derive(Debug, PartialEq, Eq)]
enum VerifyAccepted {
    /// HEAD returned the LOGICAL size — DG metadata intact, nothing to heal.
    Intact,
    /// HEAD returned the raw delta-blob size — the backend stripped the metadata;
    /// the object is present + correctly sized, but the caller must re-stamp it.
    StrippedDelta,
}

/// The verify decision on the destination's metadata, as a [`CopyError`].
fn verify_verdict(
    dest: &crate::types::FileMetadata,
    expected_bytes: usize,
    stored_delta_size: Option<u64>,
    expected_multipart_etag: Option<&str>,
) -> Result<VerifyAccepted, CopyError> {
    match verify_size_verdict(
        dest.file_size,
        expected_bytes as u64,
        stored_delta_size,
        dest.multipart_etag.as_deref(),
        expected_multipart_etag,
    ) {
        // Distinguish the two accept paths so the caller's heal can skip its own
        // redundant HEAD: logical size → intact; stored-delta size → stripped.
        Ok(()) if dest.file_size == expected_bytes as u64 => Ok(VerifyAccepted::Intact),
        Ok(()) => Ok(VerifyAccepted::StrippedDelta),
        Err(VerifyReject::Size { found }) => Err(CopyError::new(
            CopyClass::Permanent,
            format!("destination verify failed: expected {expected_bytes} bytes, found {found}"),
        )),
        Err(VerifyReject::Etag { found }) => Err(CopyError::new(
            CopyClass::Permanent,
            format!(
                "destination verify failed: expected multipart etag {:?}, found {:?}",
                expected_multipart_etag, found
            ),
        )),
    }
}

#[derive(Debug, PartialEq)]
enum VerifyReject {
    Size { found: u64 },
    Etag { found: Option<String> },
}

/// Pure verify decision. A dest is accepted when its HEAD size equals the
/// LOGICAL size (metadata intact — then the etag must also match), OR when it
/// equals the raw delta blob we just shipped (the backend stripped the dg
/// metadata so HEAD fell back to the stored size — accept, caller heals; the
/// etag is meaningless on a stripped/passthrough-resolved object, so it's not
/// checked in that case).
fn verify_size_verdict(
    dest_size: u64,
    logical_size: u64,
    stored_delta_size: Option<u64>,
    dest_etag: Option<&str>,
    expected_etag: Option<&str>,
) -> Result<(), VerifyReject> {
    if dest_size == logical_size {
        if let Some(expected) = expected_etag {
            if dest_etag != Some(expected) {
                return Err(VerifyReject::Etag {
                    found: dest_etag.map(String::from),
                });
            }
        }
        return Ok(());
    }
    if stored_delta_size == Some(dest_size) {
        return Ok(());
    }
    Err(VerifyReject::Size { found: dest_size })
}

/// Test seam: when `DGP_TEST_FAIL_PART_ONCE=<part#>` is set, return a
/// transient error the FIRST time that part is fetched (once per process
/// via `compare_exchange`). Inert in prod (env unset → None).
fn maybe_inject_part_failure(part_number: i32) -> Option<CopyError> {
    use std::sync::atomic::{AtomicI32, Ordering};
    static FIRED: AtomicI32 = AtomicI32::new(-1);
    if crate::config::test_seams::test_seams().fail_part_once != Some(part_number) {
        return None;
    }
    // compare_exchange(-1 → part#) succeeds for exactly one caller.
    if FIRED
        .compare_exchange(-1, part_number, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        return Some(CopyError {
            body_broke: true,
            ..CopyError::new(CopyClass::Transient, "connection reset (injected)")
        });
    }
    None
}

/// Test seam: when `DGP_TEST_PART_BARRIER=1`, async-sleep a small fixed delay
/// (`DGP_TEST_PART_DELAY_MS`, default 150ms) AFTER a part's inflight gauge is
/// bumped so >=concurrency parts are co-resident — making the inflight peak
/// DETERMINISTICALLY reach the configured concurrency. Inert in prod.
async fn maybe_part_barrier() {
    if let Some(ms) = crate::config::test_seams::test_seams().part_delay_ms {
        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
    }
}

/// The first words of a generation-pin failure: the source changed after
/// the copy's HEAD. Kept as text so the failure rows read as before.
const SOURCE_CHANGED: &str = "source changed during copy";

/// What a failed copy means for the caller. Set from the typed error where
/// the failure happens, never read back out of the message text: a key
/// such as `SlowDown-q3.pdf` or `quota-report.pdf` cannot change it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CopyClass {
    /// A retry can succeed: the backend was slow, broke off, or answered
    /// a 5xx; a peer held a lock; the proxy was out of codec or spool room.
    Transient,
    /// The backend throttles (503 SlowDown, 429). Retryable, and the
    /// replication run counts it apart (a throttle is not about the object).
    Throttled,
    /// This node refuses writes to the destination (the multi-instance
    /// encryption witness). Not about the object: a replication or
    /// lifecycle run stops as deferred (cursor kept) at the first one.
    Refused,
    /// The source changed under the copy's generation pin. Fatal for one
    /// part (the pin is fixed); the whole-copy retry re-HEADs.
    SourceChanged,
    /// The proxy itself was busy: a peer held the reference lock, or the
    /// codec slots or the spool budget were used up. No backend request
    /// failed, so the SDK retried nothing.
    Busy,
    /// No object can land: the bucket is gone, access is refused, or a
    /// storage cap is used up.
    DestinationFatal,
    /// Anything else: about this object, and a retry does not help.
    Permanent,
}

impl CopyClass {
    /// May a later attempt succeed (the caller holds the object for a later
    /// run or drain)?
    pub(crate) fn retryable(self) -> bool {
        matches!(
            self,
            CopyClass::Transient
                | CopyClass::Throttled
                | CopyClass::Refused
                | CopyClass::SourceChanged
                | CopyClass::Busy
        )
    }

    /// May the copy try again AT ONCE ([`copy_object_from`])? Only for what
    /// the SDK does not retry itself: a source that changed under the copy,
    /// or a proxy that was busy. A throttle, a 5xx or a timeout already had
    /// the SDK's attempts; another copy attempt multiplied them.
    pub(crate) fn copy_retry_helps(self) -> bool {
        matches!(self, CopyClass::SourceChanged | CopyClass::Busy)
    }

    pub(crate) fn of_storage(e: &crate::storage::StorageError) -> Self {
        use crate::storage::StorageError as S;
        match e {
            S::Throttled(_) => CopyClass::Throttled,
            S::WritesRefused(_) => CopyClass::Refused,
            S::Unavailable(_) | S::Transient(_) => CopyClass::Transient,
            S::Contended(_) => CopyClass::Busy,
            S::PreconditionFailed(_) => CopyClass::SourceChanged,
            S::BucketNotFound(_) | S::AccessDenied(_) | S::QuotaExceeded(_) | S::DiskFull => {
                CopyClass::DestinationFatal
            }
            S::Io(io) => Self::of_io(io),
            _ => CopyClass::Permanent,
        }
    }

    pub(crate) fn of_engine(e: &crate::deltaglider::EngineError) -> Self {
        use crate::deltaglider::{CodecError, EngineError as E};
        match e {
            E::Storage(s) => Self::of_storage(s),
            // Out of codec slots or spool budget: contention, not a fault of
            // the object (finding #14).
            E::Overloaded(_) | E::Codec(CodecError::TimedOut(_)) => CopyClass::Busy,
            E::Codec(CodecError::Io(io)) => Self::of_io(io),
            _ => CopyClass::Permanent,
        }
    }

    fn of_io(e: &std::io::Error) -> Self {
        use std::io::ErrorKind as K;
        match e.kind() {
            K::ConnectionReset
            | K::ConnectionAborted
            | K::BrokenPipe
            | K::TimedOut
            | K::UnexpectedEof
            | K::Interrupted => CopyClass::Transient,
            K::StorageFull | K::QuotaExceeded => CopyClass::DestinationFatal,
            _ => CopyClass::Permanent,
        }
    }
}

/// A failed copy: the class the caller acts on, and the text the failure
/// rows show (the same text as before the class existed).
#[derive(Debug)]
pub(crate) struct CopyError {
    pub class: CopyClass,
    message: String,
    /// A response body broke off part-way (a reset, a stall): the SDK
    /// retries a request, never a body it already began to read, so the
    /// part fetch resumes it ([`fetch_part_with_resume`]).
    body_broke: bool,
    /// The engine or storage error behind it, for a client answer
    /// ([`CopyError::into_s3_error`]).
    cause: Option<Box<CopyCause>>,
}

/// The error a [`CopyError`] came from.
#[derive(Debug)]
enum CopyCause {
    Engine(crate::deltaglider::EngineError),
    Storage(crate::storage::StorageError),
}

impl std::fmt::Display for CopyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CopyError {}

impl CopyError {
    pub(crate) fn new(class: CopyClass, message: impl Into<String>) -> Self {
        Self {
            class,
            message: message.into(),
            body_broke: false,
            cause: None,
        }
    }

    /// The S3 answer of a failed client copy: the answer of the error
    /// behind it, as the store path gives it; a source that changed under
    /// the copy is 503 SlowDown (the client retries, and the retry judges
    /// the new object).
    pub(crate) fn into_s3_error(self) -> crate::api::S3Error {
        if self.class == CopyClass::SourceChanged {
            return crate::api::S3Error::SlowDown(
                "the copy source changed during the copy; retry the request".to_string(),
            );
        }
        match self.cause.map(|c| *c) {
            Some(CopyCause::Engine(e)) => e.into(),
            Some(CopyCause::Storage(e)) => e.into(),
            None => crate::api::S3Error::InternalError(self.message),
        }
    }

    /// A body that broke off part-way: `"{context}: {e}"`, classed by the
    /// storage error.
    fn body(context: &str, e: crate::storage::StorageError) -> Self {
        Self {
            body_broke: true,
            ..Self::storage(context, e)
        }
    }

    /// `"{context}: {e}"`, classed by the engine error.
    pub(crate) fn engine(context: &str, e: crate::deltaglider::EngineError) -> Self {
        Self {
            cause: Some(Box::new(CopyCause::Engine(e))),
            ..Self::new(CopyClass::Permanent, String::new())
        }
        .described(context)
    }

    /// `"{context}: {e}"`, classed by the storage error.
    pub(crate) fn storage(context: &str, e: crate::storage::StorageError) -> Self {
        Self {
            cause: Some(Box::new(CopyCause::Storage(e))),
            ..Self::new(CopyClass::Permanent, String::new())
        }
        .described(context)
    }

    /// Class and message from the cause: `"{context}: {cause}"`, or the
    /// cause alone for an empty context.
    fn described(mut self, context: &str) -> Self {
        let (class, text) = match self.cause.as_deref() {
            Some(CopyCause::Engine(e)) => (CopyClass::of_engine(e), e.to_string()),
            Some(CopyCause::Storage(e)) => (CopyClass::of_storage(e), e.to_string()),
            None => (self.class, self.message.clone()),
        };
        self.class = class;
        self.message = if context.is_empty() {
            text
        } else {
            format!("{context}: {text}")
        };
        self
    }

    /// The source is not the generation the copy's HEAD saw.
    fn source_changed(what: std::fmt::Arguments<'_>) -> Self {
        Self::new(
            CopyClass::SourceChanged,
            format!("{SOURCE_CHANGED}: {what}"),
        )
    }

    /// The same error, the message behind `context`.
    fn context(self, context: std::fmt::Arguments<'_>) -> Self {
        Self {
            message: format!("{context}: {}", self.message),
            ..self
        }
    }
}

impl From<crate::deltaglider::EngineError> for CopyError {
    fn from(e: crate::deltaglider::EngineError) -> Self {
        Self::engine("", e)
    }
}

impl From<crate::storage::StorageError> for CopyError {
    fn from(e: crate::storage::StorageError) -> Self {
        Self::storage("", e)
    }
}

impl From<std::io::Error> for CopyError {
    fn from(e: std::io::Error) -> Self {
        Self::new(CopyClass::of_io(&e), e.to_string())
    }
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

/// Does the target object hold the same content as the source?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContentVerdict {
    /// Same size and the same SHA-256 (or MD5) on both sides.
    Same,
    /// Size or fingerprint differs: a stale copy.
    Differs,
    /// No fingerprint both sides carry (e.g. a foreign object without DG
    /// metadata): cannot prove a match.
    Unknown,
    /// Not on the target (or the source HEAD failed).
    Missing,
}

/// Pure: compare source and target metadata. Only `Same` lets a caller skip
/// a copy (migrate, lifecycle transition); a false `Differs`/`Unknown` only
/// costs a re-copy.
pub(crate) fn content_verdict(
    src: &crate::types::FileMetadata,
    dst: Option<&crate::types::FileMetadata>,
) -> ContentVerdict {
    let Some(dst) = dst else {
        return ContentVerdict::Missing;
    };
    if src.file_size != dst.file_size {
        return ContentVerdict::Differs;
    }
    if !src.file_sha256.is_empty() && !dst.file_sha256.is_empty() {
        return if src.file_sha256 == dst.file_sha256 {
            ContentVerdict::Same
        } else {
            ContentVerdict::Differs
        };
    }
    if !src.md5.is_empty() && src.md5 == dst.md5 {
        return ContentVerdict::Same;
    }
    // Two different simple MD5s are NOT `Differs`: this path only runs when a
    // side has no SHA-256, i.e. a foreign object whose `md5` is the backend
    // ETag, and an SSE-KMS / SSE-C ETag is not the MD5 of the content. A
    // `Differs` here would fail a migrate verify for a correct copy.
    ContentVerdict::Unknown
}

/// Pure: does `fresh` (source delta metadata read AFTER the blob) still name
/// the generation of `head`, and does the blob have that generation's size?
/// Missing metadata (deleted, or now passthrough) is "changed".
fn same_delta_generation(
    head: &crate::types::FileMetadata,
    fresh: Option<&crate::types::FileMetadata>,
    blob_len: usize,
) -> bool {
    use crate::types::StorageInfo;
    let Some(fresh) = fresh else {
        return false;
    };
    let (
        StorageInfo::Delta {
            ref_sha256: head_ref,
            delta_size: head_size,
            ..
        },
        StorageInfo::Delta {
            ref_sha256: fresh_ref,
            delta_size: fresh_size,
            ..
        },
    ) = (&head.storage_info, &fresh.storage_info)
    else {
        return false;
    };
    head.file_sha256 == fresh.file_sha256
        && head.created_at == fresh.created_at
        && (head_ref.is_empty() || head_ref == fresh_ref)
        && head_size == fresh_size
        && *fresh_size as usize == blob_len
}

/// The client-visible fields of a copy's destination that the copy may
/// change; [`destination_metadata`] derives every other field from the
/// source.
#[derive(Debug, Clone)]
pub(crate) struct DestinationFields {
    pub content_type: Option<String>,
    pub user_metadata: std::collections::HashMap<String, String>,
    /// The multipart ETag the destination keeps (`None`: its ETag is the
    /// MD5 of the content).
    pub multipart_etag: Option<String>,
}

impl DestinationFields {
    /// A background copy (replication, lifecycle, migrate, re-encrypt,
    /// admin bulk copy): the source's fields, the rule's provenance stamp
    /// added, the request's strip list removed.
    fn of_transfer(
        request: &ObjectTransferRequest<'_>,
        source: &crate::types::FileMetadata,
    ) -> Self {
        let mut user_metadata = source.user_metadata.clone();
        if let Some(provenance) = request.provenance {
            user_metadata.insert(
                provenance.metadata_key.to_string(),
                provenance.metadata_value.to_string(),
            );
        }
        for key in request.strip_user_metadata_keys {
            user_metadata.remove(*key);
        }
        Self {
            content_type: source.content_type.clone(),
            user_metadata,
            multipart_etag: source.multipart_etag.clone(),
        }
    }
}

/// THE metadata of an object that a copy writes to `dest_filename` with
/// the source's stored bytes (the delta fast path ships the `.delta`, a
/// server-side copy copies the stored file): the logical facts of the
/// source (SHA-256, size, MD5, and for a delta its reference), the
/// destination's own name, the caller's `fields`, a created-at as a new
/// store gives it (now, or the time [`crate::types::with_created_at`]
/// pins), and no source encryption marker (the destination write stamps
/// its own). The result equals what a store of the same content under the
/// same name writes. `None` for a reference, and for a source without
/// the DG content facts (a foreign object: its MD5 may be a backend ETag
/// that is not the MD5 of the content); only a store computes those.
pub(crate) fn destination_metadata(
    source: &crate::types::FileMetadata,
    dest_filename: &str,
    fields: DestinationFields,
) -> Option<crate::types::FileMetadata> {
    use crate::types::{FileMetadata, StorageInfo};
    if source.file_sha256.is_empty() || source.md5.is_empty() {
        return None;
    }
    let mut meta = match &source.storage_info {
        StorageInfo::Delta {
            ref_path,
            ref_sha256,
            delta_size,
            ..
        } => FileMetadata::new_delta(
            dest_filename.to_string(),
            source.file_sha256.clone(),
            source.md5.clone(),
            source.file_size,
            ref_path.clone(),
            ref_sha256.clone(),
            *delta_size,
            fields.content_type,
        ),
        StorageInfo::Passthrough => FileMetadata::new_passthrough(
            dest_filename.to_string(),
            source.file_sha256.clone(),
            source.md5.clone(),
            source.file_size,
            fields.content_type,
        ),
        StorageInfo::Reference { .. } => return None,
    };
    meta.set_user_metadata(fields.user_metadata);
    meta.multipart_etag = fields.multipart_etag;
    strip_encryption_markers(&mut meta.user_metadata);
    Some(meta)
}

/// Copy a passthrough source with [`DynEngine::copy_passthrough_within`]
/// when the destination stores it as it is. `Ok(None)`: not possible here
/// (a key that tries a delta, a foreign source, another backend or key, a
/// provider without CopyObject); nothing was written.
async fn server_side_copy(
    engine: &Arc<DynEngine>,
    lock: &crate::deltaglider::ObjectWriteGuard,
    request: ObjectTransferRequest<'_>,
    source_head: &crate::types::FileMetadata,
    prior: Option<&crate::deltaglider::CounterPrior>,
) -> Result<Option<ObjectTransferOutcome>, CopyError> {
    let fields = DestinationFields::of_transfer(&request, source_head);
    if engine.write_tries_delta(
        request.destination_bucket,
        request.destination_key,
        &fields.user_metadata,
        source_head.file_size,
    ) {
        return Ok(None);
    }
    let dest = crate::types::ObjectKey::parse(request.destination_bucket, request.destination_key);
    let Some(metadata) = destination_metadata(source_head, &dest.filename, fields) else {
        return Ok(None);
    };
    let copied = engine
        .copy_passthrough_within(
            lock,
            (request.source_bucket, request.source_key),
            (request.destination_bucket, request.destination_key),
            source_head,
            metadata,
            prior.cloned(),
        )
        .await
        .map_err(|e| CopyError::engine("server-side copy failed", e))?;
    Ok(copied.map(|_| ObjectTransferOutcome {
        bytes_copied: 0,
        strategy: CopyStrategy::ServerSide,
        source_storage_label: source_head.storage_info.label(),
        source_file_size: source_head.file_size,
        bytes_egress_saved: 0,
    }))
}

/// A delta the fast path shipped.
pub(crate) struct ShippedDelta {
    pub outcome: ObjectTransferOutcome,
    /// The metadata the destination got.
    pub metadata: crate::types::FileMetadata,
    /// The object the copy replaced (overwrite accounting).
    pub replaced: Option<crate::types::FileMetadata>,
    /// Bytes of a reference.bin the copy seeded at the destination.
    pub seeded_reference_bytes: u64,
}

/// A client CopyObject of a delta source through the delta fast path: the
/// `.delta` blob ships verbatim (no xdelta3, no reference download when the
/// destination folder holds the same reference). `fields` are the
/// destination's content type and user metadata as the metadata directive
/// chose them; the copy gets a new created-at and no multipart ETag (S3
/// CopyObject answers a single-part ETag). `Ok(None)`: the gate fell back,
/// the caller copies through the store. The caller holds the object's
/// write lock (`lock`) and ran the quota gate.
pub(crate) async fn client_delta_copy(
    engine: &Arc<DynEngine>,
    lock: &crate::deltaglider::ObjectWriteGuard,
    source: (&str, &str),
    destination: (&str, &str),
    source_head: &crate::types::FileMetadata,
    content_type: Option<String>,
    mut user_metadata: std::collections::HashMap<String, String>,
) -> Result<Option<crate::types::StoreResult>, CopyError> {
    strip_rule_provenance(&mut user_metadata);
    let request = ObjectTransferRequest {
        source_bucket: source.0,
        source_key: source.1,
        destination_bucket: destination.0,
        destination_key: destination.1,
        provenance: None,
        strip_user_metadata_keys: &[],
        operation: "client copy",
        upload_concurrency: None,
        keep_created_at: false,
    };
    let fields = DestinationFields {
        content_type,
        user_metadata,
        multipart_etag: None,
    };
    let Some(shipped) =
        delta_passthrough_copy(engine, lock, request, source_head, None, fields).await?
    else {
        return Ok(None);
    };
    Ok(Some(
        crate::types::StoreResult::new(shipped.metadata, shipped.outcome.bytes_copied as u64)
            .with_accounting(shipped.replaced, shipped.seeded_reference_bytes)
            // A copy that seeded the destination folder's reference.bin
            // created its baseline, as a first delta PUT does.
            .with_baseline_created(shipped.seeded_reference_bytes > 0),
    ))
}

/// Try the delta fast path. `Ok(None)` = fell back; the caller runs the
/// existing reconstruct path. Enforces three corruption-defense layers
/// (gate sha-check, seed sha-assert, post-lock re-gate).
async fn delta_passthrough_copy(
    engine: &Arc<DynEngine>,
    lock: &crate::deltaglider::ObjectWriteGuard,
    request: ObjectTransferRequest<'_>,
    source_head: &crate::types::FileMetadata,
    known_prior: Option<&crate::deltaglider::CounterPrior>,
    fields: DestinationFields,
) -> Result<Option<ShippedDelta>, CopyError> {
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

    // The shipped delta's metadata: the LOGICAL fields of the source, the
    // destination's own name (review D4), the caller's fields. The source
    // encryption markers go: the dest write re-stamps them iff it actually
    // encrypts, and leaves the body plaintext-with-no-marker iff it doesn't
    // (PassThrough shim / no key). A surviving source marker on a
    // re-encrypted or passed-through body is silent corruption on read.
    let mut source_facts = source_head.clone();
    if let StorageInfo::Delta { ref_sha256, .. } = &mut source_facts.storage_info {
        ref_sha256.clone_from(&src_ref_sha256);
    }
    let Some(meta) = destination_metadata(&source_facts, &dest_filename, fields) else {
        return Ok(None);
    };

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
    // The shipped delta's metadata, for the heal after the lock (the
    // original moves into the closure).
    let counter_meta = meta.clone();
    let meta_generation = source_head.clone();
    let counter_dest_bucket = dest_bucket.clone();
    // Snapshot the dest's PRIOR metadata BEFORE the write — reading it after
    // returns the just-written delta and nets the overwrite to zero.
    let counter_prior = match known_prior {
        Some(prior) => prior.clone(),
        None => {
            engine
                .fast_path_prior(&counter_dest_bucket, request.destination_key)
                .await
        }
    };
    let counter_replaced = counter_prior.replaced();
    // `Some(seeded reference bytes)` = shipped; `None` = fell back.
    let shipped: Result<Option<u64>, CopyError> = engine
        .with_dest_prefix_lock(&counter_dest_bucket, &dest_prefix, || async move {
            // Re-read the dest reference UNDER the lock and re-run the SAME pure
            // gate — identical sha + enc precedence to the first read. A read
            // that fails is not "no reference": seeding on it would overwrite
            // a live reference.bin. Fall back to reconstruct + store, which
            // surfaces the backend error itself.
            let dest_ref = match engine2.reference_meta(&dest_bucket, &dest_prefix2).await {
                Ok(meta) => meta.map(|m| DestRefFacts {
                    file_sha256: m.file_sha256.clone(),
                    enc: enc_fingerprint(&m),
                }),
                Err(_) => return Ok(None),
            };
            let decision = can_delta_passthrough(&src, dest_ref.as_ref());
            if matches!(decision, DeltaPassthroughDecision::Fallback { .. }) {
                return Ok(None);
            }
            // Read the source delta BEFORE any destination write: a failed
            // read then leaves no seeded reference.bin behind (its bytes
            // were added to no counter).
            // Generation pin: `meta` comes from the HEAD taken before this
            // read. The GET returns the metadata stored with the bytes it
            // returns; if that still names the HEAD's generation, the bytes
            // are that generation. Otherwise the dest would get new bytes
            // with the old sha. (A HEAD after the GET pinned it before.)
            let (delta_bytes, fresh) = engine2
                .get_delta_raw_with_metadata(&src_bucket, &src_prefix2, &src_filename2)
                .await?;
            if !same_delta_generation(&meta_generation, Some(&fresh), delta_bytes.len()) {
                return Err(CopyError::source_changed(format_args!(
                    "{src_bucket}/{src_prefix2}/{src_filename2} was overwritten after HEAD"
                )));
            }
            let mut seeded = 0;
            if decision == DeltaPassthroughDecision::SeedThenShip {
                // The seed moves the reference through the proxy and writes
                // it with the fenced PUT. A server-side copy is NOT used:
                // the seed must be create-only, and MinIO/silo ignores
                // `If-None-Match: *` on CopyObject (verified 2026-10-13: the
                // copy overwrote an existing key and answered 200), so a
                // conditional copy cannot stand in for the fenced write.
                // Seed the dest reference verbatim, asserting it matches
                // src.ref_sha256 before writing (defense layer 2).
                // The bytes and their metadata from one GET: a HEAD after
                // the GET could describe a reference re-seeded in between.
                let (ref_data, mut ref_meta) = engine2
                    .get_reference_raw_with_metadata(&src_bucket, &src_prefix2)
                    .await?;
                if ref_meta.file_sha256 != src.ref_sha256 {
                    return Ok(None);
                }
                // Same marker strip as the delta: the dest write re-stamps
                // (encrypt) or leaves plaintext (passthrough); a surviving
                // source marker would corrupt the reference read.
                strip_encryption_markers(&mut ref_meta.user_metadata);
                engine2
                    .put_reference_raw(
                        &dest_bucket,
                        &dest_prefix2,
                        &Bytes::from(ref_data),
                        &ref_meta,
                    )
                    .await?;
                // Counted at its write: a ship that fails below leaves it.
                engine2.record_seeded_reference(&dest_bucket, ref_meta.file_size);
                seeded = ref_meta.file_size;
            }
            engine2
                .put_delta_raw(
                    lock,
                    &dest_bucket,
                    &dest_prefix2,
                    &dest_filename2,
                    Bytes::from(delta_bytes),
                    &meta,
                    &counter_prior,
                )
                .await?;
            // The fast path bypasses the engine's store, so it records its
            // commit itself, at the commit (before the next await): the
            // delta, overwrite-aware.
            engine2.record_fast_path_copy(&dest_bucket, &counter_prior, &meta, 0);
            Ok(Some(seeded))
        })
        .await;
    let Some(seeded_reference_bytes) = shipped? else {
        return Ok(None);
    };

    // HEAD reports the LOGICAL size when the dest delta's DG metadata is intact.
    // If the backend stripped that metadata, HEAD falls back to the raw delta
    // size — accept that (== the blob we just shipped) instead of false-failing,
    // then heal the metadata so the object stays restorable + verifies cleanly.
    // Verify already HEADed the dest; its verdict tells us whether a heal is
    // needed, so heal only fires (and re-HEADs under the lock) on the stripped path.
    // The fast path wrote the `.delta` form and deleted the other one (or
    // knew it absent): HEAD that form alone, not both (`engine.head`).
    let dest = engine
        .delta_meta(request.destination_bucket, &dest_prefix, &dest_filename)
        .await
        .map_err(|e| CopyError::storage("destination verify head failed", e))?;
    let accepted = verify_verdict(
        &dest,
        source_head.file_size as usize,
        Some(src_delta_size),
        counter_meta.multipart_etag.as_deref(),
    )?;
    if accepted == VerifyAccepted::StrippedDelta {
        heal_stripped_dest_delta(engine, lock, request, &counter_meta, src_delta_size).await;
    }

    let bytes_egress_saved = source_head.file_size.saturating_sub(src_delta_size);
    // Metric counts replication only — lifecycle transitions share this path
    // but shouldn't be attributed to replication egress savings.
    if request.operation == "replication" {
        if let Some(m) = engine.metrics() {
            m.replication_delta_passthrough_bytes_saved_total
                .inc_by(bytes_egress_saved);
        }
    }

    Ok(Some(ShippedDelta {
        outcome: ObjectTransferOutcome {
            bytes_copied: src_delta_size as usize,
            strategy: CopyStrategy::DeltaPassthrough,
            source_storage_label: "delta",
            source_file_size: source_head.file_size,
            bytes_egress_saved,
        },
        metadata: counter_meta,
        replaced: counter_replaced,
        seeded_reference_bytes,
    }))
}

/// Heal a destination delta object whose DG metadata the backend stripped.
///
/// Called ONLY when the fast path's verify already accepted the dest via the
/// stripped-delta path (HEAD returned the raw delta size, not the logical size),
/// so no redundant pre-check HEAD here — verify's verdict is the gate. A stripped
/// object would make a client GET mis-detect it as passthrough and serve the raw
/// delta bytes (corruption), and every re-verify false-fail. Re-PUT the same delta
/// bytes with the correct metadata (`meta`, the logical StorageInfo::Delta fields)
/// UNDER the deltaspace lock. Best-effort: a failure here doesn't fail the copy —
/// the object is present and byte-correct; the next run retries.
async fn heal_stripped_dest_delta(
    engine: &Arc<DynEngine>,
    lock: &crate::deltaglider::ObjectWriteGuard,
    request: ObjectTransferRequest<'_>,
    meta: &crate::types::FileMetadata,
    expected_delta_size: u64,
) {
    use crate::types::ObjectKey;
    let dest_key = ObjectKey::parse(request.destination_bucket, request.destination_key);
    let bucket = request.destination_bucket;
    let key = request.destination_key;
    // Re-fetch + re-PUT UNDER the deltaspace prefix lock so a concurrent same-key
    // writer can't slip a newer blob between our read and re-stamp (which would
    // re-stamp the newer bytes with the older source's metadata → unreconstructable).
    // Re-assert the stripped signature INSIDE the lock: if the object changed since
    // the pre-check, another writer already re-stamped it — nothing to heal.
    engine
        .with_dest_prefix_lock(bucket, &dest_key.prefix, || async {
            match engine.head(bucket, key).await {
                Ok(m) if m.file_size != expected_delta_size => return, // healed/changed under us
                Ok(_) => {}
                Err(_) => return,
            }
            let delta_bytes = match engine
                .get_delta_raw(bucket, &dest_key.prefix, &dest_key.filename)
                .await
            {
                Ok(b) => b,
                Err(e) => {
                    warn!("heal: could not read stripped dest delta {bucket}/{key}: {e}");
                    return;
                }
            };
            match engine
                .put_delta_raw(
                    lock,
                    bucket,
                    &dest_key.prefix,
                    &dest_key.filename,
                    Bytes::from(delta_bytes),
                    meta,
                    // A re-stamp of a stored delta: no read says the
                    // passthrough form is absent.
                    &crate::deltaglider::CounterPrior::Unknown,
                )
                .await
            {
                Ok(_) => info!("heal: re-stamped stripped DG metadata on {bucket}/{key}"),
                Err(e) => warn!("heal: could not re-stamp stripped dest delta {bucket}/{key}: {e}"),
            }
        })
        .await;
}

/// Bounded-memory copy for large objects (Phase 4.1): stream the source
/// reconstruction to a spool file, then store from the spool via
/// `store_spooled_delta`. Closes the retrieve()→store() full-RAM re-buffer the
/// x-ray flagged for copy/replication of big deltas. Returns `None` to fall back
/// to the buffered path when the source can't be streamed to a spool here.
async fn spooled_copy(
    engine: &Arc<DynEngine>,
    lock: &crate::deltaglider::ObjectWriteGuard,
    request: &ObjectTransferRequest<'_>,
    source_head: &crate::types::FileMetadata,
    source_size: u64,
) -> Result<Option<ObjectTransferOutcome>, CopyError> {
    // Stream the (reconstructed) source to a spool file — bounded memory.
    let resp = engine
        .retrieve_stream(request.source_bucket, request.source_key)
        .await
        .map_err(|e| CopyError::engine("source retrieve_stream failed", e))?;
    let (mut stream, meta) = match resp {
        crate::deltaglider::RetrieveResponse::Streamed {
            stream, metadata, ..
        } => (stream, metadata),
        // Buffered (small) source — let the caller's buffered path handle it.
        crate::deltaglider::RetrieveResponse::Buffered { .. } => return Ok(None),
    };

    // Same generation as the HEAD? (The size below is the HEAD's.)
    let changed = || {
        CopyError::source_changed(format_args!(
            "{}/{} was overwritten after HEAD",
            request.source_bucket, request.source_key
        ))
    };
    if !source_head.file_sha256.is_empty()
        && !meta.file_sha256.is_empty()
        && source_head.file_sha256 != meta.file_sha256
    {
        return Err(changed());
    }

    let spool = engine.spool_acquire(source_size).await?;
    // Capped at the reservation, hashed on the way: the store must get
    // exactly the bytes the metadata names.
    use crate::deltaglider::spool::SpoolFillError;
    let fill = spool
        .fill_from_stream(&mut stream, source_size)
        .await
        .map_err(|e| match e {
            SpoolFillError::Source(e) => CopyError::storage("source stream error", e),
            SpoolFillError::Write(e) => e.into(),
            SpoolFillError::Overrun { .. } => changed(),
        })?;
    if fill.written != source_size
        || (!meta.file_sha256.is_empty() && fill.sha256 != meta.file_sha256)
    {
        return Err(changed());
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
    // Strip the source's at-rest encryption markers (see `copy_object_once`):
    // the destination re-stamps its own on store, and a stale marker on a
    // non-encrypting destination yields an undecryptable replica.
    crate::storage::encrypting::strip_encryption_markers(&mut user_metadata);

    let stored = engine
        .store_spooled_delta(
            lock,
            request.destination_bucket,
            request.destination_key,
            &spool,
            source_size,
            content_type,
            user_metadata,
            meta.multipart_etag.clone(),
        )
        .await
        .map_err(|e| CopyError::engine("destination spooled store failed", e))?;
    // Same post-store check as the buffered path.
    verify_destination(
        engine,
        *request,
        &stored.metadata,
        source_size as usize,
        source_head.multipart_etag.as_deref(),
    )
    .await?;

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
    use crate::config::Config;
    use crate::deltaglider::DeltaGliderEngine;
    use crate::storage::DynStorageBackend;

    async fn fs_engine_with(dir: &std::path::Path, buckets: &[&str]) -> Arc<DynEngine> {
        let backend: Box<DynStorageBackend<'static>> = DynStorageBackend::new_box(
            crate::storage::FilesystemBackend::new(dir.to_path_buf())
                .await
                .unwrap(),
        );
        let engine: Arc<DynEngine> = Arc::new(DeltaGliderEngine::new_with_backend(
            Arc::new(backend),
            &Config::default(),
            None,
        ));
        for b in buckets {
            engine.create_bucket(b).await.unwrap();
        }
        engine
    }

    /// A background copy of a delta into a bucket with compression off
    /// stores the plain object: the delta fast path shipped the stored
    /// delta and seeded a reference there, so tools that read that bucket
    /// without the proxy found `.delta` files.
    #[tokio::test]
    async fn a_delta_copied_into_a_bucket_without_compression_is_stored_plain() {
        let dir = tempfile::tempdir().unwrap();
        let backend: Box<DynStorageBackend<'static>> = DynStorageBackend::new_box(
            crate::storage::FilesystemBackend::new(dir.path().to_path_buf())
                .await
                .unwrap(),
        );
        let mut config = Config::default();
        config.buckets.insert(
            "plain".into(),
            crate::bucket_policy::BucketPolicyConfig {
                compression: Some(false),
                ..Default::default()
            },
        );
        let engine: Arc<DynEngine> = Arc::new(DeltaGliderEngine::new_with_backend(
            Arc::new(backend),
            &config,
            None,
        ));
        for b in ["b", "plain"] {
            engine.create_bucket(b).await.unwrap();
        }
        let keys = crate::deltaglider::store_deltas(&engine, "run", 2).await;
        let request = ObjectTransferRequest {
            source_bucket: "b",
            source_key: &keys[1],
            destination_bucket: "plain",
            destination_key: &keys[1],
            provenance: None,
            strip_user_metadata_keys: &[],
            operation: "replication",
            upload_concurrency: None,
            keep_created_at: true,
        };

        let outcome = copy_object_with_retries(&engine, request).await.unwrap();

        assert_ne!(outcome.strategy, CopyStrategy::DeltaPassthrough);
        let (body, meta) = engine.retrieve("plain", &keys[1]).await.unwrap();
        assert!(!meta.is_delta(), "a bucket without compression got a delta");
        let (source, _) = engine.retrieve("b", &keys[1]).await.unwrap();
        assert_eq!(body, source);
    }

    /// The spooled copy trusted the HEAD's size: a source overwritten with a
    /// bigger object after the HEAD overran the spool reservation, and the
    /// store claimed the old size for the new bytes. The copy must write
    /// exactly one generation or fail as SOURCE_CHANGED (the retry re-HEADs).
    #[tokio::test]
    async fn spooled_copy_refuses_a_source_that_grew_after_head() {
        let dir = tempfile::tempdir().unwrap();
        let engine = fs_engine_with(dir.path(), &["src", "dst"]).await;
        let old = vec![1u8; 64 * 1024];
        let new = vec![2u8; 96 * 1024];
        engine
            .store("src", "big.bin", &old, None, Default::default())
            .await
            .unwrap();
        let stale = engine.head("src", "big.bin").await.unwrap();
        engine
            .store("src", "big.bin", &new, None, Default::default())
            .await
            .unwrap();
        let request = ObjectTransferRequest {
            source_bucket: "src",
            source_key: "big.bin",
            destination_bucket: "dst",
            destination_key: "big.bin",
            provenance: None,
            strip_user_metadata_keys: &[],
            operation: "replication",
            upload_concurrency: None,
            keep_created_at: false,
        };
        let lock = engine
            .lock_object_write(request.destination_bucket, request.destination_key)
            .await;
        match spooled_copy(&engine, &lock, &request, &stale, stale.file_size).await {
            Err(e) => assert_eq!(
                e.class,
                CopyClass::SourceChanged,
                "a changed source must be retryable: {e}"
            ),
            Ok(None) => {}
            Ok(Some(_)) => {
                let (got, _) = engine.retrieve("dst", "big.bin").await.unwrap();
                assert!(
                    got == old || got == new,
                    "dest holds a mixed/truncated object"
                );
            }
        }
    }

    #[test]
    fn content_verdict_truth_table() {
        use crate::types::{FileMetadata, StorageInfo};
        let meta = |size: u64, sha: &str, md5: &str| {
            let mut m = FileMetadata::fallback(
                "k".into(),
                size,
                md5.into(),
                chrono::Utc::now(),
                None,
                StorageInfo::Passthrough,
            );
            m.file_sha256 = sha.into();
            m
        };
        let src = meta(3, "aa", "m1");
        assert_eq!(content_verdict(&src, None), ContentVerdict::Missing);
        assert_eq!(
            content_verdict(&src, Some(&meta(3, "aa", "zz"))),
            ContentVerdict::Same
        );
        assert_eq!(
            content_verdict(&src, Some(&meta(3, "bb", "m1"))),
            ContentVerdict::Differs
        );
        assert_eq!(
            content_verdict(&src, Some(&meta(4, "aa", "m1"))),
            ContentVerdict::Differs
        );
        // No SHA on one side: fall back to MD5.
        assert_eq!(
            content_verdict(&src, Some(&meta(3, "", "m1"))),
            ContentVerdict::Same
        );
        assert_eq!(
            content_verdict(&src, Some(&meta(3, "", "m2"))),
            ContentVerdict::Unknown
        );
        assert_eq!(
            content_verdict(&meta(3, "", ""), Some(&meta(3, "", ""))),
            ContentVerdict::Unknown
        );
    }

    fn versioned_bytes(seed: u8, n: usize) -> Vec<u8> {
        // Pseudo-random base (poorly compressible) + a small per-version edit,
        // so the second version stores as a delta against the first.
        let mut x: u32 = 0x1234_5678;
        let mut v: Vec<u8> = (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect();
        for b in v.iter_mut().take(64) {
            *b = seed;
        }
        v
    }

    /// D7: the fast path ships the CURRENT delta blob with the metadata of the
    /// HEAD it took earlier. If the source is overwritten in between, the dest
    /// gets new bytes with the old sha and is unreadable. The copy must fail
    /// (transient → the retry re-HEADs) instead of writing a broken object.
    #[tokio::test]
    async fn delta_fast_path_refuses_a_source_overwritten_after_head() {
        let dir = tempfile::tempdir().unwrap();
        let backend: Box<DynStorageBackend<'static>> = DynStorageBackend::new_box(
            crate::storage::FilesystemBackend::new(dir.path().to_path_buf())
                .await
                .unwrap(),
        );
        let engine: Arc<DynEngine> = Arc::new(DeltaGliderEngine::new_with_backend(
            Arc::new(backend),
            &Config::default(),
            None,
        ));
        engine.create_bucket("src").await.unwrap();
        engine.create_bucket("dst").await.unwrap();
        let n = 256 * 1024;
        for (key, seed) in [("p/a.zip", 1u8), ("p/b.zip", 2u8)] {
            engine
                .store(
                    "src",
                    key,
                    &versioned_bytes(seed, n),
                    None,
                    Default::default(),
                )
                .await
                .unwrap();
        }
        let stale_head = engine.head("src", "p/b.zip").await.unwrap();
        assert!(
            matches!(
                stale_head.storage_info,
                crate::types::StorageInfo::Delta { .. }
            ),
            "fixture must store b.zip as a delta, got {:?}",
            stale_head.storage_info
        );
        // Overwrite AFTER the HEAD (the race window).
        engine
            .store(
                "src",
                "p/b.zip",
                &versioned_bytes(3, n),
                None,
                Default::default(),
            )
            .await
            .unwrap();

        let request = ObjectTransferRequest {
            source_bucket: "src",
            source_key: "p/b.zip",
            destination_bucket: "dst",
            destination_key: "p/b.zip",
            provenance: None,
            strip_user_metadata_keys: &[],
            operation: "replication",
            upload_concurrency: None,
            keep_created_at: false,
        };
        let lock = engine
            .lock_object_write(request.destination_bucket, request.destination_key)
            .await;
        let result = delta_passthrough_copy(
            &engine,
            &lock,
            request,
            &stale_head,
            None,
            DestinationFields::of_transfer(&request, &stale_head),
        )
        .await;
        match result {
            Err(e) => assert_eq!(
                e.class,
                CopyClass::SourceChanged,
                "a changed source must be a retryable error, got: {e}"
            ),
            Ok(outcome) => {
                // Either a fallback (None) or a ship: a ship must read back.
                if outcome.is_some() {
                    let read = engine.retrieve("dst", "p/b.zip").await;
                    assert!(
                        read.is_ok(),
                        "dest is unreadable after the fast path: {:?}",
                        read.err()
                    );
                    assert_eq!(read.unwrap().0, versioned_bytes(3, n));
                }
            }
        }
        // With a current HEAD the fast path ships, and the dest reads back.
        let fresh_head = engine.head("src", "p/b.zip").await.unwrap();
        let shipped = delta_passthrough_copy(
            &engine,
            &lock,
            request,
            &fresh_head,
            None,
            DestinationFields::of_transfer(&request, &fresh_head),
        )
        .await
        .expect("fresh HEAD must not error");
        assert!(
            shipped.is_some(),
            "unchanged source must take the fast path"
        );
        assert_eq!(
            engine.retrieve("dst", "p/b.zip").await.unwrap().0,
            versioned_bytes(3, n)
        );
    }

    /// B003: under the dest lock, a failed read of the dest reference (an S3
    /// HEAD that answers 503) is not "no reference". Seeding on it overwrote
    /// the live reference.bin, and the dest deltas against it stopped
    /// decoding.
    #[tokio::test]
    async fn delta_fast_path_never_seeds_over_an_unreadable_dest_reference() {
        let dir = tempfile::tempdir().unwrap();
        let faulty = crate::storage::FaultyFs::new(
            crate::storage::FilesystemBackend::new(dir.path().to_path_buf())
                .await
                .unwrap(),
        );
        let faults = faulty.faults.clone();
        let backend: Box<DynStorageBackend<'static>> = DynStorageBackend::new_box(faulty);
        let engine: Arc<DynEngine> = Arc::new(DeltaGliderEngine::new_with_backend(
            Arc::new(backend),
            &Config::default(),
            None,
        ));
        engine.create_bucket("src").await.unwrap();
        engine.create_bucket("dst").await.unwrap();
        let n = 256 * 1024;
        // Two deltaspaces with different references: dst X (seed 1), src Y
        // (seed 4); each has one delta against its own reference.
        for (bucket, key, seed) in [
            ("dst", "p/a.zip", 1u8),
            ("dst", "p/a2.zip", 2),
            ("src", "p/b.zip", 4),
            ("src", "p/b2.zip", 5),
        ] {
            engine
                .store(
                    bucket,
                    key,
                    &versioned_bytes(seed, n),
                    None,
                    Default::default(),
                )
                .await
                .unwrap();
        }
        let head = engine.head("src", "p/b2.zip").await.unwrap();
        assert!(
            matches!(head.storage_info, crate::types::StorageInfo::Delta { .. }),
            "fixture must store b2.zip as a delta"
        );

        let x_sha = engine
            .reference_metadata_raw("dst", "p")
            .await
            .unwrap()
            .file_sha256;
        faults.arm(
            crate::storage::FaultPoint::GetReferenceMetadata,
            "dst/p/",
            crate::storage::Fault::Throttled,
        );
        let request = ObjectTransferRequest {
            source_bucket: "src",
            source_key: "p/b2.zip",
            destination_bucket: "dst",
            destination_key: "p/c.zip",
            provenance: None,
            strip_user_metadata_keys: &[],
            operation: "replication",
            upload_concurrency: None,
            keep_created_at: false,
        };
        let _ = delta_passthrough_copy(
            &engine,
            &engine
                .lock_object_write(request.destination_bucket, request.destination_key)
                .await,
            request,
            &head,
            None,
            DestinationFields::of_transfer(&request, &head),
        )
        .await;
        faults.disarm_all();
        assert!(faults.fired(crate::storage::FaultPoint::GetReferenceMetadata, "dst/p/") > 0);
        let after = engine
            .reference_metadata_raw("dst", "p")
            .await
            .unwrap()
            .file_sha256;
        assert_eq!(after, x_sha, "the copy replaced the live dest reference");

        // A fresh engine has no cached reference: it reads reference.bin.
        let fresh = fs_engine_with(dir.path(), &[]).await;
        let read = fresh.retrieve("dst", "p/a2.zip").await;
        assert!(
            read.is_ok(),
            "a dest delta no longer decodes: {:?}",
            read.err()
        );
        assert_eq!(read.unwrap().0, versioned_bytes(2, n));
    }

    /// Round-2 review: `ReplicationObjectCopied` reported the stored delta
    /// size (43 B for a 1 MB object) as `content_length`.
    #[test]
    fn event_content_length_is_the_logical_size_not_the_transfer() {
        let fast = ObjectTransferOutcome {
            bytes_copied: 43,
            strategy: CopyStrategy::DeltaPassthrough,
            source_storage_label: "delta",
            source_file_size: 1_048_576,
            bytes_egress_saved: 1_048_533,
        };
        assert_eq!(fast.content_length(), 1_048_576);
    }

    #[test]
    fn verify_size_verdict_truth_table() {
        // logical match + etag match → OK
        assert_eq!(
            verify_size_verdict(1000, 1000, Some(400), Some("e"), Some("e")),
            Ok(())
        );
        // logical match + etag MISMATCH → reject on etag
        assert_eq!(
            verify_size_verdict(1000, 1000, Some(400), Some("bad"), Some("e")),
            Err(VerifyReject::Etag {
                found: Some("bad".into())
            })
        );
        // logical match + no expected etag → OK (etag unchecked)
        assert_eq!(verify_size_verdict(1000, 1000, None, None, None), Ok(()));
        // stripped delta: HEAD returns the raw delta size we shipped → ACCEPT,
        // even though it ≠ logical, and WITHOUT checking the (meaningless) etag.
        assert_eq!(
            verify_size_verdict(400, 1000, Some(400), None, Some("e")),
            Ok(())
        );
        // genuinely wrong size (not logical, not the shipped delta) → reject.
        assert_eq!(
            verify_size_verdict(777, 1000, Some(400), None, None),
            Err(VerifyReject::Size { found: 777 })
        );
        // no stored_delta_size hint (buffered path) + size mismatch → reject.
        assert_eq!(
            verify_size_verdict(400, 1000, None, None, None),
            Err(VerifyReject::Size { found: 400 })
        );
    }

    /// The copy retry, the replication run and the event consumer act on the
    /// class, and the class comes from the error variant: the text (which
    /// names the key) plays no part.
    #[test]
    fn copy_class_follows_the_error_variant() {
        use crate::deltaglider::{CodecError, EngineError as E};
        use crate::storage::StorageError as S;
        use CopyClass::*;
        let io = |k: std::io::ErrorKind| S::Io(std::io::Error::new(k, "x"));
        let cases: Vec<(E, CopyClass)> = vec![
            (E::Storage(S::Throttled("SlowDown".into())), Throttled),
            (E::Storage(S::Unavailable("timed out".into())), Transient),
            (E::Storage(S::Transient("status=502".into())), Transient),
            (E::Storage(S::Contended("reference lock".into())), Busy),
            (E::Overloaded("spool budget exhausted".into()), Busy),
            (E::Overloaded("all delta codec slots busy".into()), Busy),
            (E::Codec(CodecError::TimedOut("xdelta3".into())), Busy),
            (
                E::Storage(io(std::io::ErrorKind::ConnectionReset)),
                Transient,
            ),
            (E::Storage(io(std::io::ErrorKind::BrokenPipe)), Transient),
            (
                E::Storage(S::PreconditionFailed("gen".into())),
                SourceChanged,
            ),
            (E::Storage(S::BucketNotFound("b".into())), DestinationFatal),
            (E::Storage(S::AccessDenied("403".into())), DestinationFatal),
            (E::Storage(S::QuotaExceeded("cap".into())), DestinationFatal),
            (E::Storage(S::DiskFull), DestinationFatal),
            (
                E::Storage(io(std::io::ErrorKind::StorageFull)),
                DestinationFatal,
            ),
            // Marker words in a key never change the class.
            (E::NotFound("SlowDown-q3.pdf".into()), Permanent),
            (
                E::Storage(S::NotFound("logs/timeout.txt".into())),
                Permanent,
            ),
            (
                E::Storage(S::S3("quota-report.pdf status=503".into())),
                Permanent,
            ),
            (E::Storage(S::Other("connection reset".into())), Permanent),
            (E::Codec(CodecError::DecodeFailed("bad".into())), Permanent),
            (E::InvalidArgument("x".into()), Permanent),
        ];
        for (e, want) in cases {
            let msg = e.to_string();
            let err = CopyError::engine("source retrieve failed", e);
            assert_eq!(err.class, want, "{msg}");
            assert_eq!(err.to_string(), format!("source retrieve failed: {msg}"));
        }
        assert!(Transient.retryable() && Throttled.retryable() && SourceChanged.retryable());
        assert!(Busy.retryable());
        assert!(!DestinationFatal.retryable() && !Permanent.retryable());
        // The copy itself tries again only for what the SDK did not retry.
        assert!(SourceChanged.copy_retry_helps() && Busy.copy_retry_helps());
        for sdk_retried in [Transient, Throttled, DestinationFatal, Permanent] {
            assert!(!sdk_retried.copy_retry_helps(), "{sdk_retried:?}");
        }
        // The generation-pin text reads as before.
        let changed = CopyError::source_changed(format_args!("b/k was overwritten after HEAD"));
        assert_eq!(
            changed.to_string(),
            "source changed during copy: b/k was overwritten after HEAD"
        );
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
    use crate::storage::DynStorageBackend;
    use crate::storage::{MultipartUpload, StorageBackend, StorageError, UploadedPart};
    use crate::types::FileMetadata;
    use bytes::Bytes;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// Spy backend: serves one in-memory passthrough source object, fakes a
    /// native multipart destination, and records every abort. Failure modes
    /// are toggled per test.
    struct AbortSpy {
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

    impl StorageBackend for AbortSpy {
        async fn copy_within(
            &self,
            v: crate::storage::ObjectVariant,
            s: crate::storage::ObjectAt<'_>,
            d: crate::storage::ObjectAt<'_>,
            src: &crate::types::FileMetadata,
            m: &crate::types::FileMetadata,
        ) -> Result<crate::storage::CopyOutcome, crate::storage::StorageError> {
            let _ = (v, s, d, src, m);
            Ok(crate::storage::CopyOutcome::Unsupported)
        }

        async fn reference_fence(
            &self,
            b: &str,
            p: &str,
        ) -> Result<crate::storage::RefFence, crate::storage::StorageError> {
            crate::storage::unfenced_reference_fence(self, b, p).await
        }
        async fn reference_fence_with_metadata(
            &self,
            b: &str,
            p: &str,
        ) -> Result<
            (crate::storage::RefFence, Option<crate::types::FileMetadata>),
            crate::storage::StorageError,
        > {
            crate::storage::unfenced_reference_fence_with_metadata(self, b, p).await
        }
        async fn write_reference(
            &self,
            _: &str,
            _: &str,
            _: crate::storage::RefWrite<'_>,
            _proof: &crate::deltaglider::RefWriteProof,
        ) -> Result<(), StorageError> {
            Err(nope())
        }
        async fn write_reference_fenced(
            &self,
            b: &str,
            p: &str,
            op: crate::storage::RefWrite<'_>,
            _: &crate::storage::RefFence,
            proof: &crate::deltaglider::RefWriteProof,
        ) -> Result<crate::storage::RefFence, crate::storage::StorageError> {
            crate::storage::unfenced_reference_write(self, b, p, op, proof).await
        }
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
        async fn get_passthrough_metadata_for_rewrite(
            &self,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<(FileMetadata, crate::storage::RewriteBasis), StorageError> {
            Ok((self.src_meta.clone(), crate::storage::RewriteBasis::none()))
        }
        async fn get_delta_metadata(
            &self,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<FileMetadata, StorageError> {
            Err(StorageError::NotFound("no delta".into()))
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
        async fn flush_pending(&self) -> Result<(), StorageError> {
            Ok(())
        }
        async fn get_delta(&self, _: &str, _: &str, _: &str) -> Result<Vec<u8>, StorageError> {
            Err(nope())
        }
        async fn put_delta(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: bytes::Bytes,
            _: &FileMetadata,
            _proof: &crate::deltaglider::RefWriteProof,
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
            _: bytes::Bytes,
            _: &FileMetadata,
        ) -> Result<(), StorageError> {
            Err(nope())
        }
        async fn delete_passthrough(&self, _: &str, _: &str, _: &str) -> Result<(), StorageError> {
            Err(nope())
        }
        async fn open_object(
            &self,
            b: &str,
            p: &str,
            o: crate::storage::StoredObject<'_>,
        ) -> Result<crate::storage::ObjectRead, crate::storage::StorageError> {
            let _ = (b, p, o);
            Err(nope())
        }

        async fn open_reference_source(
            &self,
            _: &str,
            _: &str,
        ) -> Result<crate::storage::ReferenceSource, crate::storage::StorageError> {
            Err(nope())
        }

        async fn open_passthrough_range(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: u64,
            _: u64,
        ) -> Result<crate::storage::RangeRead, crate::storage::StorageError> {
            Err(nope())
        }
        async fn listing_tokens(
            &self,
            _: &str,
            objects: &[(String, crate::types::FileMetadata)],
        ) -> Vec<Option<crate::storage::ListedToken>> {
            vec![None; objects.len()]
        }
        async fn read_key_claim(
            &self,
            _: &str,
        ) -> Result<Option<crate::storage::KeyClaim>, crate::storage::StorageError> {
            Ok(None)
        }
        async fn write_key_claim(
            &self,
            _: &str,
            _: crate::storage::ClaimWrite<'_>,
        ) -> Result<crate::storage::ClaimOutcome, crate::storage::StorageError> {
            Ok(crate::storage::ClaimOutcome::Written)
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
        async fn list_reference_prefixes(
            &self,
            _: &str,
            _: &str,
        ) -> Result<Vec<String>, StorageError> {
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
        let backend: Box<DynStorageBackend<'static>> =
            DynStorageBackend::new_box(SpyRef(spy.clone()));
        let engine =
            DeltaGliderEngine::new_with_backend(Arc::new(backend), &Config::default(), None);
        (Arc::new(engine), spy)
    }

    /// Thin forwarding wrapper so the test keeps an `Arc<AbortSpy>` for
    /// assertions while the engine owns the boxed backend.
    struct SpyRef(Arc<AbortSpy>);

    impl StorageBackend for SpyRef {
        async fn copy_within(
            &self,
            v: crate::storage::ObjectVariant,
            s: crate::storage::ObjectAt<'_>,
            d: crate::storage::ObjectAt<'_>,
            src: &crate::types::FileMetadata,
            m: &crate::types::FileMetadata,
        ) -> Result<crate::storage::CopyOutcome, crate::storage::StorageError> {
            self.0.copy_within(v, s, d, src, m).await
        }

        async fn reference_fence(
            &self,
            b: &str,
            p: &str,
        ) -> Result<crate::storage::RefFence, crate::storage::StorageError> {
            self.0.reference_fence(b, p).await
        }
        async fn reference_fence_with_metadata(
            &self,
            b: &str,
            p: &str,
        ) -> Result<
            (crate::storage::RefFence, Option<crate::types::FileMetadata>),
            crate::storage::StorageError,
        > {
            self.0.reference_fence_with_metadata(b, p).await
        }
        async fn write_reference(
            &self,
            b: &str,
            p: &str,
            op: crate::storage::RefWrite<'_>,
            proof: &crate::deltaglider::RefWriteProof,
        ) -> Result<(), StorageError> {
            self.0.write_reference(b, p, op, proof).await
        }
        async fn write_reference_fenced(
            &self,
            b: &str,
            p: &str,
            op: crate::storage::RefWrite<'_>,
            f: &crate::storage::RefFence,
            proof: &crate::deltaglider::RefWriteProof,
        ) -> Result<crate::storage::RefFence, crate::storage::StorageError> {
            self.0.write_reference_fenced(b, p, op, f, proof).await
        }
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
        async fn get_passthrough_metadata_for_rewrite(
            &self,
            b: &str,
            p: &str,
            f: &str,
        ) -> Result<(FileMetadata, crate::storage::RewriteBasis), StorageError> {
            self.0.get_passthrough_metadata_for_rewrite(b, p, f).await
        }
        async fn get_delta_metadata(
            &self,
            b: &str,
            p: &str,
            f: &str,
        ) -> Result<FileMetadata, StorageError> {
            self.0.get_delta_metadata(b, p, f).await
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
        async fn flush_pending(&self) -> Result<(), StorageError> {
            self.0.flush_pending().await
        }
        async fn get_delta(&self, b: &str, p: &str, f: &str) -> Result<Vec<u8>, StorageError> {
            self.0.get_delta(b, p, f).await
        }
        async fn put_delta(
            &self,
            b: &str,
            p: &str,
            f: &str,
            d: bytes::Bytes,
            m: &FileMetadata,
            proof: &crate::deltaglider::RefWriteProof,
        ) -> Result<(), StorageError> {
            self.0.put_delta(b, p, f, d, m, proof).await
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
            d: bytes::Bytes,
            m: &FileMetadata,
        ) -> Result<(), StorageError> {
            self.0.put_passthrough(b, p, f, d, m).await
        }
        async fn delete_passthrough(&self, b: &str, p: &str, f: &str) -> Result<(), StorageError> {
            self.0.delete_passthrough(b, p, f).await
        }
        async fn open_object(
            &self,
            b: &str,
            p: &str,
            o: crate::storage::StoredObject<'_>,
        ) -> Result<crate::storage::ObjectRead, crate::storage::StorageError> {
            self.0.open_object(b, p, o).await
        }

        async fn open_reference_source(
            &self,
            b: &str,
            p: &str,
        ) -> Result<crate::storage::ReferenceSource, crate::storage::StorageError> {
            self.0.open_reference_source(b, p).await
        }

        async fn open_passthrough_range(
            &self,
            b: &str,
            p: &str,
            f: &str,
            start: u64,
            end: u64,
        ) -> Result<crate::storage::RangeRead, crate::storage::StorageError> {
            self.0.open_passthrough_range(b, p, f, start, end).await
        }
        async fn listing_tokens(
            &self,
            b: &str,
            objects: &[(String, crate::types::FileMetadata)],
        ) -> Vec<Option<crate::storage::ListedToken>> {
            self.0.listing_tokens(b, objects).await
        }
        async fn read_key_claim(
            &self,
            _: &str,
        ) -> Result<Option<crate::storage::KeyClaim>, crate::storage::StorageError> {
            Ok(None)
        }
        async fn write_key_claim(
            &self,
            _: &str,
            _: crate::storage::ClaimWrite<'_>,
        ) -> Result<crate::storage::ClaimOutcome, crate::storage::StorageError> {
            Ok(crate::storage::ClaimOutcome::Written)
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
        async fn list_reference_prefixes(
            &self,
            b: &str,
            s: &str,
        ) -> Result<Vec<String>, StorageError> {
            self.0.list_reference_prefixes(b, s).await
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
            keep_created_at: false,
        }
    }

    #[tokio::test]
    async fn part_failure_aborts_the_multipart_upload() {
        let spy = AbortSpy::new(vec![7u8; 64]);
        spy.fail_part.store(true, Ordering::Relaxed);
        let (engine, spy) = spy_engine(spy);
        let meta = spy.src_meta.clone();

        let req = copy_request();
        let lock = engine
            .lock_object_write(req.destination_bucket, req.destination_key)
            .await;
        let res = stream_copy_passthrough(&engine, &lock, req, &meta).await;
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

        let req = copy_request();
        let lock = engine
            .lock_object_write(req.destination_bucket, req.destination_key)
            .await;
        let res = stream_copy_passthrough(&engine, &lock, req, &meta).await;
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
            let req = copy_request();
            let lock = eng
                .lock_object_write(req.destination_bucket, req.destination_key)
                .await;
            let _ = stream_copy_passthrough(&eng, &lock, req, &meta).await;
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

/// Requests one copy sends, on a fake S3: the source HEADs, the retries on
/// top of the SDK's, and the counter of a reference the fast path seeds.
#[cfg(test)]
mod request_count_tests {
    use super::*;
    use crate::bucket_usage::BucketUsage;
    use crate::config::Config;
    use crate::deltaglider::DeltaGliderEngine;
    use crate::storage::{DynStorageBackend, FakeS3, StorageBackend};

    async fn engine_on(
        backend: crate::storage::S3Backend,
        usage: Option<Arc<BucketUsage>>,
        buckets: &[&str],
    ) -> Arc<DynEngine> {
        let backend: Box<DynStorageBackend<'static>> = DynStorageBackend::new_box(backend);
        let engine =
            DeltaGliderEngine::new_with_backend(Arc::new(backend), &Config::default(), None)
                .with_bucket_usage(usage);
        for b in buckets {
            engine.create_bucket(b).await.unwrap();
        }
        Arc::new(engine)
    }

    async fn s3_engine(usage: Option<Arc<BucketUsage>>) -> (Arc<DynEngine>, Arc<FakeS3>) {
        let (s3, fake) = crate::storage::fake_s3_backend().await;
        (engine_on(s3, usage, &["src", "dst", "dst2"]).await, fake)
    }

    fn request(key: &str) -> ObjectTransferRequest<'_> {
        ObjectTransferRequest {
            source_bucket: "src",
            source_key: key,
            destination_bucket: "dst",
            destination_key: key,
            provenance: None,
            strip_user_metadata_keys: &[],
            operation: "replication",
            upload_concurrency: None,
            keep_created_at: true,
        }
    }

    /// Requests that start with `prefix`, without the background listing
    /// facts upkeep under `.dg/` (it races the assertions).
    fn count(fake: &FakeS3, prefix: &str) -> usize {
        fake.requests()
            .iter()
            .filter(|r| r.starts_with(prefix) && !r.contains("/.dg/"))
            .count()
    }

    /// Poorly compressible bytes with a per-version head, so a second
    /// version stores as a delta against the first.
    fn versioned(seed: u8, n: usize) -> Vec<u8> {
        let mut x: u32 = 0x9E37_79B9;
        let mut v: Vec<u8> = (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect();
        for b in v.iter_mut().take(64) {
            *b = seed;
        }
        v
    }

    /// Jobs finding 15: each copy attempt HEADed the source twice (once for
    /// its created-at, once more in the copy), though the caller held the
    /// source's metadata from its own HEAD: 8 HEAD requests or more for one
    /// object. With the caller's metadata the copy sends no source HEAD;
    /// without it, one source HEAD (two requests on S3).
    #[tokio::test]
    async fn a_copy_heads_the_source_only_when_the_caller_has_no_metadata() {
        let (engine, fake) = s3_engine(None).await;
        for key in ["k.dat", "j.dat"] {
            engine
                .store("src", key, b"payload", None, Default::default())
                .await
                .unwrap();
        }
        let meta = engine.head("src", "k.dat").await.unwrap();
        fake.clear();
        copy_object_from(&engine, request("k.dat"), Some(&meta))
            .await
            .unwrap();
        let heads = count(&fake, "HEAD ");
        assert!(
            heads <= 4,
            "{heads} HEAD requests for one copy: {:?}",
            fake.requests()
        );
        // The one source HEAD is the server-side copy's generation pin,
        // sent instead of a GET of the body; the caller's read is not
        // repeated.
        assert_eq!(
            (count(&fake, "HEAD /src/"), count(&fake, "GET /src/")),
            (1, 0),
            "the copy HEADed a source the caller had read: {:?}",
            fake.requests()
        );
        fake.clear();
        copy_object_from(&engine, request("j.dat"), None)
            .await
            .unwrap();
        assert!(
            count(&fake, "HEAD /src/") <= 3,
            "one source HEAD per copy, and the pin: {:?}",
            fake.requests()
        );
    }

    /// Copy `p/b.zip` (a delta) into `dst` while `op` requests whose path
    /// holds `path` fail once, retry the copy once if it failed, and compare
    /// the counter of `dst` with a clean copy into `dst2`.
    async fn seeded_reference_counted_despite(op: &str, path: &str) {
        let usage = Arc::new(BucketUsage::in_memory().unwrap());
        let (engine, fake) = s3_engine(Some(usage.clone())).await;
        let n = 256 * 1024;
        for (key, seed) in [("p/a.zip", 1u8), ("p/b.zip", 2u8)] {
            engine
                .store("src", key, &versioned(seed, n), None, Default::default())
                .await
                .unwrap();
        }
        let head = engine.head("src", "p/b.zip").await.unwrap();
        assert!(
            matches!(head.storage_info, crate::types::StorageInfo::Delta { .. }),
            "fixture must store b.zip as a delta"
        );
        // A clean copy into dst2 gives the numbers dst must end with.
        let clean = ObjectTransferRequest {
            destination_bucket: "dst2",
            ..request("p/b.zip")
        };
        copy_object_with_retries(&engine, clean).await.unwrap();
        fake.fail(op, path, 503, "SlowDown", 1);
        if copy_object_with_retries(&engine, request("p/b.zip"))
            .await
            .is_err()
        {
            copy_object_with_retries(&engine, request("p/b.zip"))
                .await
                .unwrap();
        }
        assert!(
            fake.requests()
                .iter()
                .any(|r| r.starts_with(&format!("{op} /{path}"))),
            "the fault never fired: {:?}",
            fake.requests()
        );
        usage.flush_pending();
        let dst = usage.read("dst").unwrap().unwrap();
        let dst2 = usage.read("dst2").unwrap().unwrap();
        assert_eq!(
            (dst.object_count, dst.stored_bytes),
            (dst2.object_count, dst2.stored_bytes),
            "the seeded reference.bin is missing from the counter"
        );
    }

    /// Accounting finding: the fast path seeded the destination's
    /// reference.bin, then the ship failed (here: a 503 on the GET of the
    /// source delta). The retry found the seeded reference and shipped the
    /// delta alone, so the counter never got the reference's bytes.
    #[tokio::test]
    async fn a_seeded_reference_is_counted_when_the_ship_after_it_fails() {
        seeded_reference_counted_despite("GET", "src/p/b.zip.delta").await;
    }

    /// The same, with the failure after the seed: the PUT of the delta.
    /// The seeded reference.bin stays at the destination, so the counter
    /// records it as soon as its write returns.
    #[tokio::test]
    async fn a_seeded_reference_is_counted_when_the_delta_put_after_it_fails() {
        seeded_reference_counted_despite("PUT", "dst/p/b.zip.delta").await;
    }

    /// Two delta versions under `src/p/`, and `dst/p/` seeded with their
    /// reference by a first copy. Returns the HEAD of `src/p/b.zip`.
    async fn seeded_delta_pair(engine: &Arc<DynEngine>) -> crate::types::FileMetadata {
        for (key, seed) in [("p/a.zip", 1u8), ("p/b.zip", 2u8), ("p/c.zip", 3u8)] {
            engine
                .store(
                    "src",
                    key,
                    &versioned(seed, 64 * 1024),
                    None,
                    Default::default(),
                )
                .await
                .unwrap();
        }
        let first = copy_object_with_retries(engine, request("p/c.zip"))
            .await
            .unwrap();
        assert_eq!(first.strategy, CopyStrategy::DeltaPassthrough);
        let head = engine.head("src", "p/b.zip").await.unwrap();
        assert!(head.is_delta(), "fixture must store b.zip as a delta");
        head
    }

    /// P5.4: the delta fast path HEADed the source delta again after its
    /// GET (the generation pin; the GET response carries that metadata)
    /// and verified the destination with both forms (2 HEADs) though it
    /// wrote one. Into a folder that holds the reference, without a usage
    /// counter: 4 HEADs -> 2 (reference + verify).
    #[tokio::test]
    async fn the_delta_fast_path_heads_only_what_no_response_told_it() {
        let (engine, fake) = s3_engine(None).await;
        let head = seeded_delta_pair(&engine).await;
        fake.clear();
        let outcome = copy_object_from(&engine, request("p/b.zip"), Some(&head))
            .await
            .unwrap();
        assert_eq!(outcome.strategy, CopyStrategy::DeltaPassthrough);
        let r = fake.requests();
        assert_eq!(count(&fake, "HEAD /src/"), 0, "the pin is the GET: {r:#?}");
        assert_eq!(count(&fake, "HEAD /dst/p/b.zip.delta"), 1, "{r:#?}");
        assert!(!r.iter().any(|x| x == "HEAD /dst/p/b.zip"), "{r:#?}");
        assert_eq!(count(&fake, "HEAD "), 2, "{r:#?}");
        assert_eq!(count(&fake, "GET "), 1, "{r:#?}");
        assert_eq!(count(&fake, "PUT "), 1, "{r:#?}");
        // The delta moves verbatim: what comes down goes up, no reference.
        let (down, up) = fake.bytes();
        assert_eq!((down, up), (outcome.bytes_copied as u64, down));
        assert_eq!(
            engine.retrieve("dst", "p/b.zip").await.unwrap().0,
            engine.retrieve("src", "p/b.zip").await.unwrap().0
        );
    }

    /// P5.4: a caller that HEADed the destination (the replication event
    /// consumer) hands that read in as the counter prior: the copy sends no
    /// HEAD of the destination key before its write, and the usage counter
    /// still nets an overwrite to +0 objects. The read came before the
    /// key's write lock, so the copy still deletes the other stored form
    /// (`a_form_written_after_the_read_ahead_is_dropped_by_the_copy`).
    #[tokio::test]
    async fn the_callers_destination_head_is_the_counter_prior() {
        let usage = Arc::new(BucketUsage::in_memory().unwrap());
        let (engine, fake) = s3_engine(Some(usage.clone())).await;
        let head = seeded_delta_pair(&engine).await;
        for round in 0..2 {
            let (dest, prior) = engine
                .head_with_counter_prior("dst", "p/b.zip")
                .await
                .unwrap();
            assert_eq!(dest.is_some(), round == 1);
            fake.clear();
            copy_object_known(&engine, request("p/b.zip"), Some(&head), Some(&prior))
                .await
                .unwrap();
            let r = fake.requests();
            // Reference HEAD + the verify HEAD of the written form.
            assert_eq!(count(&fake, "HEAD "), 2, "round {round}: {r:#?}");
            assert!(!r.iter().any(|x| x == "HEAD /dst/p/b.zip"), "{r:#?}");
            assert_eq!(count(&fake, "DELETE "), 1, "{r:#?}");
            assert_eq!(count(&fake, "DELETE /dst/p/b.zip?"), 1, "{r:#?}");
        }
        usage.flush_pending();
        let row = usage.read("dst").unwrap().unwrap();
        assert_eq!(row.object_count, 2, "c.zip + b.zip once");
    }

    /// The replication event consumer reads the destination before the
    /// copy takes the key's write lock. A client PUT in that window stored
    /// the other form (passthrough); the copy (a delta) skipped its DELETE
    /// because the read had seen no such form. Both forms stayed, and the
    /// client's object, newer than the replica's kept created-at, hid it.
    #[tokio::test]
    async fn a_form_written_after_the_read_ahead_is_dropped_by_the_copy() {
        let usage = Arc::new(BucketUsage::in_memory().unwrap());
        let (engine, _fake) = s3_engine(Some(usage)).await;
        let head = seeded_delta_pair(&engine).await;
        let (dest, prior) = engine
            .head_with_counter_prior("dst", "p/b.zip")
            .await
            .unwrap();
        assert!(dest.is_none());
        engine
            .store(
                "dst",
                "p/b.zip",
                &versioned(7, 64 * 1024),
                None,
                std::collections::HashMap::from([("dg-no-delta".to_string(), "true".to_string())]),
            )
            .await
            .unwrap();
        let outcome = copy_object_known(&engine, request("p/b.zip"), Some(&head), Some(&prior))
            .await
            .unwrap();
        assert_eq!(outcome.strategy, CopyStrategy::DeltaPassthrough);
        let left = engine
            .storage()
            .get_passthrough_metadata("dst", "p", "b.zip")
            .await;
        assert!(
            left.is_err(),
            "the client's passthrough form stayed beside the replica: {left:?}"
        );
        assert_eq!(
            engine.retrieve("dst", "p/b.zip").await.unwrap().0,
            engine.retrieve("src", "p/b.zip").await.unwrap().0
        );
    }

    /// P5.2: a background copy of a passthrough object inside one backend
    /// (replication, lifecycle, migrate, re-encrypt with the same key, the
    /// admin bulk copy) is a server-side copy: no byte passes through the
    /// proxy, and the destination is the object the store path writes
    /// (here a replica: the same metadata as its source, created-at kept).
    #[tokio::test]
    async fn a_passthrough_copy_inside_one_backend_moves_no_bytes() {
        let usage = Arc::new(BucketUsage::in_memory().unwrap());
        let (engine, fake) = s3_engine(Some(usage.clone())).await;
        let body = versioned(9, 128 * 1024);
        engine
            .store(
                "src",
                "pics/a.jpg",
                &body,
                Some("image/jpeg".into()),
                Default::default(),
            )
            .await
            .unwrap();
        let source = engine.head("src", "pics/a.jpg").await.unwrap();
        fake.clear();
        let outcome = copy_object_from(&engine, request("pics/a.jpg"), Some(&source))
            .await
            .unwrap();
        assert_eq!(outcome.strategy, CopyStrategy::ServerSide);
        assert_eq!(outcome.content_length(), body.len() as u64);
        assert_eq!(fake.bytes(), (0, 0), "{:#?}", fake.requests());
        let copied = engine.head("dst", "pics/a.jpg").await.unwrap();
        let fields = |m: &crate::types::FileMetadata| serde_json::to_value(m).unwrap();
        assert_eq!(fields(&copied), fields(&source));
        assert_eq!(engine.retrieve("dst", "pics/a.jpg").await.unwrap().0, body);
        usage.flush_pending();
        let row = usage.read("dst").unwrap().unwrap();
        assert_eq!(
            (row.object_count, row.logical_bytes),
            (1, body.len() as u64)
        );
    }

    /// Blast finding 6: the SDK already sends a throttled PUT three times;
    /// the copy loop sent the whole copy three times on top: 9 PUTs of the
    /// body to a backend that asked to slow down.
    #[tokio::test]
    async fn a_throttled_copy_sends_its_put_at_most_three_times() {
        let (endpoint, fake) = crate::storage::fake_s3::start().await;
        let config = crate::config::BackendConfig::S3 {
            endpoint: Some(endpoint),
            region: "us-east-1".into(),
            force_path_style: true,
            access_key_id: Some("k".into()),
            secret_access_key: Some("s".into()),
            allow_local: true,
            session_token: None,
        };
        let s3 =
            crate::storage::S3Backend::new(&config, crate::storage::NativeEncryptionConfig::None)
                .await
                .unwrap();
        let engine = engine_on(s3, None, &["src", "dst"]).await;
        engine
            .store("src", "k.bin", b"payload", None, Default::default())
            .await
            .unwrap();
        fake.fail("PUT", "dst/", 503, "SlowDown", u32::MAX);
        fake.fail("COPY", "dst/", 503, "SlowDown", u32::MAX);
        fake.clear();
        assert!(copy_object_with_retries(&engine, request("k.bin"))
            .await
            .is_err());
        // A PUT or a server-side copy (also a PUT request).
        let puts = count(&fake, "PUT /dst/");
        assert!(puts <= 3, "{puts} writes for one throttled copy");
    }
}
