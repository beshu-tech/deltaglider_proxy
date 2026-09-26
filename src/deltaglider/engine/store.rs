// SPDX-License-Identifier: BUSL-1.1

//! Store pipeline — delta encoding, passthrough, and baseline management.

use super::*;
use crate::deltaglider::spool::SpoolBudget;
use crate::storage::{MultipartUpload, StorageBackend, UploadedPart};
use md5::{Digest, Md5};
use sha2::Sha256;
use std::path::{Path, PathBuf};
use std::time::Instant;
use tokio::io::AsyncReadExt;

/// In-progress streaming passthrough multipart upload (Phase B). Holds the
/// per-deltaspace lock for the upload's lifetime. The handle is IMMUTABLE
/// during part uploads so the caller can drive parts concurrently; it
/// collects the small [`UploadedPart`] receipts itself. Whole-object hashes
/// are taken from the copy source (a copy doesn't recompute them), so no
/// in-order hashing is needed and memory stays O(in-flight parts).
pub struct PassthroughMultipartHandle {
    bucket: String,
    key: String,
    deltaspace_id: String,
    filename: String,
    total_size: u64,
    content_type: Option<String>,
    user_metadata: HashMap<String, String>,
    upload: MultipartUpload,
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

impl PassthroughMultipartHandle {
    /// Whether the backend writes parts durably & incrementally (S3) — the
    /// caller may drop part bytes after each `upload_passthrough_part`.
    pub fn native(&self) -> bool {
        self.upload.native
    }
}

/// Store an object with automatic delta compression
impl<S: StorageBackend> DeltaGliderEngine<S> {
    #[instrument(skip(self, data, user_metadata))]
    pub async fn store(
        &self,
        bucket: &str,
        key: &str,
        data: &[u8],
        content_type: Option<String>,
        user_metadata: std::collections::HashMap<String, String>,
    ) -> Result<StoreResult, EngineError> {
        let result = self
            .store_inner(bucket, key, data, content_type, user_metadata, None)
            .await?;
        self.record_store(bucket, &result);
        Ok(result)
    }

    /// Multipart-aware variant of [`Self::store`]. The `multipart_etag` is
    /// persisted alongside the object so HEAD/GET/LIST return it verbatim
    /// (H1 correctness fix). All other semantics are identical.
    #[instrument(skip(self, data, user_metadata, multipart_etag))]
    #[allow(clippy::too_many_arguments)]
    pub async fn store_with_multipart_etag(
        &self,
        bucket: &str,
        key: &str,
        data: &[u8],
        content_type: Option<String>,
        user_metadata: std::collections::HashMap<String, String>,
        multipart_etag: String,
    ) -> Result<StoreResult, EngineError> {
        let result = self
            .store_inner(
                bucket,
                key,
                data,
                content_type,
                user_metadata,
                Some(multipart_etag),
            )
            .await?;
        self.record_store(bucket, &result);
        Ok(result)
    }

    /// `PUT photos/` with an empty body (review D3): a zero-byte folder
    /// marker, the object S3 clients create for an empty folder. The backend
    /// stores it as-is (never encrypted, never a delta), so an S3 listing
    /// still shows it as a zero-byte `photos/`.
    async fn store_directory_marker(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<StoreResult, EngineError> {
        let (obj_key, deltaspace_id) = self.validated_key(bucket, key)?;
        let prior_for_counter = self.prior_for_counter(bucket, key).await;
        let _guard = self.acquire_prefix_lock(bucket, &deltaspace_id).await;
        self.storage
            .put_directory_marker(bucket, &obj_key.full_key())
            .await?;
        let metadata = FileMetadata::directory_marker(&obj_key.full_key());
        self.metadata_cache.insert(bucket, key, metadata.clone());
        Ok(StoreResult::new(metadata, 0).with_accounting(prior_for_counter, 0))
    }

    #[allow(clippy::too_many_arguments)]
    async fn store_inner(
        &self,
        bucket: &str,
        key: &str,
        data: &[u8],
        content_type: Option<String>,
        user_metadata: std::collections::HashMap<String, String>,
        multipart_etag: Option<String>,
    ) -> Result<StoreResult, EngineError> {
        // Invalidate stale metadata on overwrite (before the write, so concurrent
        // readers don't see outdated metadata during the write window).
        self.metadata_cache.invalidate(bucket, key);

        // Check size limit
        if data.len() as u64 > self.max_object_size {
            return Err(EngineError::TooLarge {
                size: data.len() as u64,
                max: self.max_object_size,
            });
        }

        if data.is_empty() && ObjectKey::parse(bucket, key).is_directory_marker() {
            return self.store_directory_marker(bucket, key).await;
        }
        let (obj_key, deltaspace_id) = self.validated_key_ingest(bucket, key)?;

        // Usage-counter accounting: capture the PRIOR object metadata (S3 PUT is
        // an upsert) so the counter nets an overwrite to +0 instead of double-
        // counting. Small TOCTOU window vs the write below is acceptable — the
        // counter is best-effort and reconciled by Refresh.
        let prior_for_counter = self.prior_for_counter(bucket, key).await;

        // Calculate hashes
        let sha256 = hex::encode(Sha256::digest(data));
        let md5 = hex::encode(Md5::digest(data));

        info!(
            "Storing {}/{} ({} bytes, sha256={})",
            bucket,
            key,
            data.len(),
            &sha256[..8]
        );

        // Check per-bucket compression policy + file type eligibility
        let compression_disabled = !self.bucket_policies.compression_enabled(bucket);
        if compression_disabled || !self.file_router.is_delta_eligible(&obj_key.filename) {
            if compression_disabled {
                debug!("Compression disabled for bucket '{bucket}', storing as passthrough");
            } else {
                debug!("File type not delta-eligible, storing as passthrough");
            }
            self.with_metrics(|m| {
                m.delta_decisions_total
                    .with_label_values(&["passthrough"])
                    .inc()
            });
            let _guard = self.acquire_prefix_lock(bucket, &deltaspace_id).await;
            let ctx = StoreContext {
                bucket,
                obj_key: &obj_key,
                deltaspace_id: &deltaspace_id,
                data,
                sha256,
                md5,
                content_type,
                user_metadata,
                multipart_etag: multipart_etag.clone(),
            };
            let result = self.store_passthrough(ctx).await?;
            // Write succeeded — now safe to clean up old delta variant
            if let Err(e) = self
                .delete_delta_idempotent(bucket, &deltaspace_id, &obj_key.filename)
                .await
            {
                warn!(
                    "Failed to clean up old delta after passthrough write: {}",
                    e
                );
            }
            self.metadata_cache
                .insert(bucket, key, result.metadata.clone());
            // Passthrough creates no reference baseline; only overwrite-net.
            return Ok(result.with_accounting(prior_for_counter, 0));
        }

        // Acquire per-deltaspace lock to prevent concurrent reference overwrites.
        // The critical section: has_reference check → set_reference → store_delta
        // must be atomic per-prefix to avoid two writers both creating a reference.
        // The in-process mutex serializes same-node threads; the cross-instance
        // lock (multi-instance only, inert otherwise) serializes across nodes.
        let _guard = self.acquire_prefix_lock(bucket, &deltaspace_id).await;
        let xnode = self.acquire_reference_lock(bucket, &deltaspace_id).await?;

        let ctx = StoreContext {
            bucket,
            obj_key: &obj_key,
            deltaspace_id: &deltaspace_id,
            data,
            sha256,
            md5,
            content_type,
            user_metadata,
            multipart_etag,
        };

        // Check if deltaspace already has a reference (existing deltaspace).
        // A backend error here must ABORT the PUT — never fall through to the
        // "create baseline" branch, which would overwrite a reference.bin that
        // may exist and orphan every sibling delta.
        let has_existing_reference = match xnode.observed_reference() {
            Some(seen) => seen,
            None => {
                self.storage
                    .has_reference(ctx.bucket, ctx.deltaspace_id)
                    .await?
            }
        };

        // Ensure deltaspace has an internal reference baseline.
        //
        // S-P1-2: when we CREATE the reference here, we own its
        // lifecycle. If the subsequent `encode_and_store` fails (codec
        // semaphore exhausted, codec panic, size cap, storage write
        // error), the reference would otherwise remain on disk with no
        // sibling delta — every future PUT to this prefix would anchor
        // against bytes the user never successfully stored, poisoning
        // the deltaspace permanently. Rollback on failure to restore
        // the "no reference yet" invariant.
        let ref_meta = if has_existing_reference {
            let read = self
                .storage
                .get_reference_metadata(ctx.bucket, ctx.deltaspace_id)
                .await?;
            // Heal a stripped-metadata reference in place (same bytes) so the
            // delta we write next carries a valid ref_sha256 and replication
            // stops re-copying this deltaspace. No-op (zero I/O) when healthy.
            self.heal_reference_if_corrupt(ctx.bucket, ctx.deltaspace_id, read, None, &xnode)
                .await?
        } else {
            debug!("No reference in deltaspace, creating baseline");
            self.set_reference_baseline(&ctx, &xnode).await?
        };

        // Encode delta and decide: keep as delta or fall back to direct storage
        let result = match self
            .encode_and_store(ctx, &ref_meta, has_existing_reference, &xnode)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                if !has_existing_reference {
                    // Best-effort: undo the reference we just created.
                    // Errors here are logged but do not mask the
                    // original encode failure.
                    let cache_key = self.cache_key(bucket, &deltaspace_id);
                    self.cache.invalidate(&cache_key);
                    if let Err(cleanup_err) = xnode
                        .delete_reference(&*self.storage, bucket, &deltaspace_id)
                        .await
                    {
                        warn!(
                            "S-P1-2: encode failed AND reference rollback failed for {}/{}: encode_err={}, rollback_err={}",
                            bucket, deltaspace_id, e, cleanup_err
                        );
                    } else {
                        debug!(
                            "S-P1-2: encode failed; rolled back fresh reference for {}/{}",
                            bucket, deltaspace_id
                        );
                    }
                }
                return Err(e);
            }
        };
        self.metadata_cache
            .insert(bucket, key, result.metadata.clone());
        // NB: the COUNTER is recorded in the public delegators (store /
        // store_with_multipart_etag), not here — store_inner is shared, so
        // recording here would double-count. We only attach the accounting the
        // delegators need: the prior object (overwrite-net) + a newly-seeded
        // reference's bytes (symmetric with delete's reclamation subtraction).
        let reference_created_bytes = if has_existing_reference {
            0
        } else {
            ref_meta.file_size
        };
        Ok(result.with_accounting(prior_for_counter, reference_created_bytes))
    }

    /// Encode a delta against the reference, evaluate the compression ratio,
    /// and either commit as delta or fall back to passthrough storage.
    async fn encode_and_store(
        &self,
        ctx: StoreContext<'_>,
        ref_meta: &FileMetadata,
        has_existing_reference: bool,
        xnode: &super::ReferenceLockGuard,
    ) -> Result<StoreResult, EngineError> {
        let (reference, _cache_hit) = self
            .get_reference_cached(ctx.bucket, ctx.deltaspace_id, &ref_meta.file_sha256)
            .await?;
        // PERF: try_acquire instead of acquire — fail fast with 503 when all codec
        // slots are busy rather than queuing unbounded requests in memory (each
        // holding a full object body while waiting for a permit).
        // The source file first (spool before codec slot, as on every path).
        let source_file = self.codec_source_spool_now(reference.len())?;
        let _codec_permit = self.try_acquire_codec()?;
        // spawn_blocking: xdelta3 is CPU-bound; data must be owned ('static).
        let ref_clone = reference.clone();
        let data_owned = ctx.data.to_vec();
        let codec = self.codec.clone();
        let encode_start = Instant::now();
        let delta = tokio::task::spawn_blocking(move || {
            codec.encode_spooled(&source_file, &ref_clone, &data_owned)
        })
        .await
        .map_err(|e| {
            tracing::error!("Delta encode task panicked: {}", e);
            EngineError::Storage(StorageError::Other(format!("codec task panicked: {}", e)))
        })??;
        let encode_secs = encode_start.elapsed().as_secs_f64();
        drop(_codec_permit);

        let ratio = DeltaCodec::compression_ratio(ctx.data.len(), delta.len());

        self.with_metrics(|m| {
            m.delta_encode_duration_seconds.observe(encode_secs);
            m.delta_compression_ratio.observe(ratio as f64);
        });

        info!(
            "Delta computed: {} bytes -> {} bytes (ratio: {:.2}%)",
            ctx.data.len(),
            delta.len(),
            ratio * 100.0
        );

        self.commit_delta_or_passthrough(ctx, ref_meta, has_existing_reference, delta, ratio, xnode)
            .await
    }

    /// STREAMING delta PUT (Phase 4): store a large delta-eligible object whose
    /// body is already on a seekable spool file, WITHOUT buffering it in RAM.
    ///
    /// Memory is bounded by the codec pump (Spike C: 2MB RSS on a 1.5GB target).
    /// Flow:
    /// 1. Hash the body by streaming the spool (sha256 + md5) — no full-RAM read.
    /// 2. If not delta-eligible / compression off / no reference yet → passthrough
    ///    straight from the body spool (store_passthrough_file).
    /// 3. Else: materialise the reference to a spool, encode_from_reader(body →
    ///    delta spool) capped at `ratio_threshold × size`. If the cap trips or the
    ///    ratio loses → passthrough from the body spool (we still have it). Else →
    ///    commit the delta.
    ///
    /// The caller owns `body` (a `Spool`); it lives until this returns.
    #[allow(clippy::too_many_arguments)]
    pub async fn store_spooled_delta(
        &self,
        bucket: &str,
        key: &str,
        body: &crate::deltaglider::spool::Spool,
        size: u64,
        content_type: Option<String>,
        user_metadata: std::collections::HashMap<String, String>,
        multipart_etag: Option<String>,
    ) -> Result<StoreResult, EngineError> {
        let result = self
            .store_spooled_delta_inner(
                bucket,
                key,
                body,
                size,
                content_type,
                user_metadata,
                multipart_etag,
            )
            .await?;
        self.record_store(bucket, &result);
        Ok(result)
    }

    /// Body of [`Self::store_spooled_delta`]. Does NOT record the counter (the
    /// public entry point does, exactly once); it attaches the accounting.
    #[allow(clippy::too_many_arguments)]
    async fn store_spooled_delta_inner(
        &self,
        bucket: &str,
        key: &str,
        body: &crate::deltaglider::spool::Spool,
        size: u64,
        content_type: Option<String>,
        user_metadata: std::collections::HashMap<String, String>,
        multipart_etag: Option<String>,
    ) -> Result<StoreResult, EngineError> {
        use tokio::io::AsyncReadExt;

        self.metadata_cache.invalidate(bucket, key);
        let (obj_key, deltaspace_id) = self.validated_key_ingest(bucket, key)?;
        // Size ceiling depends on the STRATEGY: a delta-eligible object is
        // bounded by max_object_size (it will be xdelta3-encoded in RAM); a
        // passthrough object streams from the spool and is bounded by the far
        // larger max_passthrough_object_size. Applying the delta limit to
        // passthrough objects made every spooled passthrough copy fail
        // TooLarge under default config (finding #3).
        let compression_disabled = !self.bucket_policies.compression_enabled(bucket);
        let is_passthrough =
            compression_disabled || !self.file_router.is_delta_eligible(&obj_key.filename);
        let ceiling = if is_passthrough {
            self.max_passthrough_object_size
        } else {
            self.max_object_size
        };
        if size > ceiling {
            return Err(EngineError::TooLarge { size, max: ceiling });
        }
        let prior_for_counter = self.prior_for_counter(bucket, key).await;
        let mpe = multipart_etag.clone().unwrap_or_default();

        // (1) Hash the body by streaming the spool — bounded memory. Also count
        // the observed bytes and reject a spool that doesn't match the declared
        // `size` (source overwritten mid-stream, or a stale/corrupt xattr
        // file_size on a filesystem source) — else we'd stamp file_size=declared
        // over a sha256 of DIFFERENT bytes. Matches the multipart-relay guard.
        let (sha256, md5, observed) = Self::hash_spool_file(body.path()).await?;
        if observed != size {
            return Err(EngineError::Storage(StorageError::Other(format!(
                "Spooled object size mismatch: declared {size}, observed {observed}"
            ))));
        }

        let etag = if mpe.is_empty() {
            format!("\"{md5}\"")
        } else {
            mpe.clone()
        };

        // (2) Not delta-eligible → passthrough from the body spool.
        if is_passthrough {
            let result = self
                .store_passthrough_file_inner(
                    bucket,
                    key,
                    body.path(),
                    Some(body),
                    size,
                    content_type.clone(),
                    user_metadata.clone(),
                    etag.clone(),
                )
                .await?;
            self.metadata_cache
                .insert(bucket, key, result.metadata.clone());
            return Ok(result.with_accounting(prior_for_counter, 0));
        }

        // B1: the in-process mutex serializes same-node threads; the
        // cross-instance lock (multi-instance only, inert single-instance)
        // serializes across NODES, so two instances can no longer both create a
        // baseline and corrupt reference.bin (see CLAUDE.md HA contract).
        let _guard = self.acquire_prefix_lock(bucket, &deltaspace_id).await;
        let xnode = self.acquire_reference_lock(bucket, &deltaspace_id).await?;
        // Write path: a backend error must abort, not read as "no reference".
        let has_existing_reference = match xnode.observed_reference() {
            Some(seen) => seen,
            None => self.storage.has_reference(bucket, &deltaspace_id).await?,
        };
        // A fresh baseline stays in place on both branches below (even when the
        // ratio loses — see the NOTE there), so its bytes are always counted.
        let reference_created_bytes = if has_existing_reference { 0 } else { size };

        // No reference yet → this object becomes the deltaspace baseline. Stream
        // the reference into place from the spool (put_reference_from_file: no
        // heap-load — M1.4 fix). Then FALL THROUGH to the encode-against-reference
        // path below: the first member self-deltas (tiny) against its own fresh
        // reference, exactly like the buffered baseline branch.
        if !has_existing_reference {
            let ref_meta = FileMetadata::new_reference(
                Self::INTERNAL_REFERENCE_NAME.to_string(),
                obj_key.full_key(),
                sha256.clone(),
                md5.clone(),
                size,
                content_type.clone(),
            );
            xnode
                .put_reference_from_file(
                    &*self.storage,
                    bucket,
                    &deltaspace_id,
                    body.path(),
                    &ref_meta,
                )
                .await?;
            self.with_metrics(|m| {
                m.delta_decisions_total
                    .with_label_values(&["reference"])
                    .inc()
            });
            // The streaming path doesn't pre-cache the reference bytes; next GET
            // loads fresh.
            self.cache
                .invalidate(&self.cache_key(bucket, &deltaspace_id));
            // Fall through — the encode block below now sees has_existing_reference
            // effectively true (the reference is on disk).
        }

        // Existing-reference metadata, carried forward for the spool reservation
        // below (avoids a redundant re-read). `None` on the fresh-baseline path.
        let existing_ref_meta = if has_existing_reference {
            // Heal it in place (same bytes) if its DG metadata was stripped, so
            // the delta we encode next carries a valid ref_sha256 and
            // replication stops re-copying this deltaspace. No-op (zero extra
            // I/O) when the reference is healthy; returns the current metadata.
            let read = self
                .storage
                .get_reference_metadata(bucket, &deltaspace_id)
                .await?;
            Some(
                self.heal_reference_if_corrupt(bucket, &deltaspace_id, read, Some(body), &xnode)
                    .await?,
            )
        } else {
            None
        };

        // (3) Encode from the body spool against the reference, capped. Reaches
        // here both for an existing reference AND a freshly-created baseline
        // (the first member self-deltas against its own reference).
        // ONE timed, combined reservation for both spools (ref + delta) — two raw
        // sequential acquire()s self-deadlock when 2×size > budget, the exact
        // class the GET path uses acquire_pair to prevent (mega-review finding).
        // The ref spool holds the REFERENCE, which can be larger than this object
        // — reserve it at the reference's actual size so the byte-budget isn't
        // under-accounted under concurrency (→ ENOSPC). Falls back to `size` for
        // a freshly-created baseline (no reference metadata yet).
        let ref_size = existing_ref_meta.map(|m| m.file_size).unwrap_or(size);
        // Clamped beside the body spool this op already holds (else body +
        // pair > budget waited on itself for the whole acquire timeout).
        let pair = self.spool_acquire_pair_beside(body, ref_size, size).await?;
        let Some((ref_spool, delta_spool)) = pair else {
            // No budget free now. Waiting while this PUT holds its body could
            // deadlock with another PUT that waits on ours. Store it as
            // passthrough, exactly as when the ratio loses (a fresh baseline
            // stays: see the NOTE in that branch).
            tracing::debug!("streaming PUT {bucket}/{key}: spool contended, storing passthrough");
            drop((_guard, xnode));
            return self
                .store_spooled_body_as_passthrough(
                    bucket,
                    key,
                    body,
                    size,
                    content_type,
                    user_metadata,
                    etag,
                )
                .await
                .map(|r| r.with_accounting(prior_for_counter, reference_created_bytes));
        };
        self.storage
            .get_reference_to_file(bucket, &deltaspace_id, ref_spool.path())
            .await?;

        let effective_ratio = self.bucket_policies.max_delta_ratio(bucket);
        let cap = ((size as f64) * (effective_ratio as f64)).ceil() as u64;
        let _permit = self.try_acquire_codec()?;
        let codec = self.codec.clone();
        let ref_path = ref_spool.path().to_path_buf();
        let body_path = body.path().to_path_buf();
        let delta_path = delta_spool.path().to_path_buf();
        let encode_start = Instant::now();

        // Encode body→delta spool, aborting if the delta exceeds the cap (ratio
        // loses — Spike C). A capped-write error signals "passthrough wins".
        let encode_res =
            tokio::task::spawn_blocking(move || -> Result<Option<u64>, EngineError> {
                use std::io::Write;
                struct CapWriter<W: Write> {
                    inner: W,
                    written: u64,
                    cap: u64,
                    capped: bool,
                }
                impl<W: Write> Write for CapWriter<W> {
                    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                        self.written = self.written.saturating_add(buf.len() as u64);
                        if self.written > self.cap {
                            self.capped = true;
                            return Err(std::io::Error::other("delta exceeded ratio cap"));
                        }
                        self.inner.write_all(buf)?;
                        Ok(buf.len())
                    }
                    fn flush(&mut self) -> std::io::Result<()> {
                        self.inner.flush()
                    }
                }
                let body = std::fs::File::open(&body_path)
                    .map_err(|e| EngineError::Storage(StorageError::from(e)))?;
                let out = std::fs::OpenOptions::new()
                    .write(true)
                    .truncate(true)
                    .open(&delta_path)
                    .map_err(|e| EngineError::Storage(StorageError::from(e)))?;
                let mut sink = CapWriter {
                    inner: std::io::BufWriter::new(out),
                    written: 0,
                    cap,
                    capped: false,
                };
                match codec.encode_from_reader(&ref_path, body, &mut sink) {
                    Ok(n) => {
                        sink.flush()
                            .map_err(|e| EngineError::Storage(StorageError::from(e)))?;
                        Ok(Some(n))
                    }
                    // Cap tripped → ratio loses; signal passthrough (None).
                    Err(_) if sink.capped => Ok(None),
                    Err(e) => Err(EngineError::Codec(e)),
                }
            })
            .await
            .map_err(|e| {
                EngineError::Storage(StorageError::Other(format!("encode task: {e}")))
            })??;
        drop(_permit);
        self.with_metrics(|m| {
            m.delta_encode_duration_seconds
                .observe(encode_start.elapsed().as_secs_f64())
        });

        match encode_res {
            None => {
                // Ratio lost → passthrough from the body spool.
                // Drop BOTH locks FIRST — store_passthrough_file re-acquires the
                // prefix lock internally, so holding it here would re-entrant-
                // deadlock; the cross-node lock is released too (passthrough does
                // not touch reference.bin, so it needs no cross-node exclusion).
                drop((ref_spool, delta_spool, _guard, xnode));
                let result = self
                    .store_spooled_body_as_passthrough(
                        bucket,
                        key,
                        body,
                        size,
                        content_type.clone(),
                        user_metadata.clone(),
                        etag.clone(),
                    )
                    .await?;
                // NOTE: a fresh baseline whose first member lost the ratio is
                // LEFT IN PLACE — we deliberately do NOT tear it down here.
                // The buffered path deletes it inside its prefix-lock scope; the
                // streaming path had to DROP the lock before the passthrough
                // store (which re-acquires it), so an unguarded delete_reference
                // here would race a concurrent PUT B that, between our drop and
                // our delete, sees the reference, deltas against it, and commits —
                // we'd then delete the reference B needs (MissingReference on B's
                // GET). A reference with no delta pointing at it is harmless: a
                // later sibling PUT may delta against it, and it's reclaimed when
                // the deltaspace empties. Correctness over a minor cleanup.
                Ok(result.with_accounting(prior_for_counter, reference_created_bytes))
            }
            Some(delta_size) => {
                // Delta wins → read the delta spool (small, < cap) + commit it.
                let mut df = tokio::fs::File::open(delta_spool.path())
                    .await
                    .map_err(StorageError::from)?;
                let mut delta_bytes = Vec::with_capacity(delta_size as usize);
                df.read_to_end(&mut delta_bytes)
                    .await
                    .map_err(StorageError::from)?;
                let result = self
                    .commit_streamed_delta(
                        bucket,
                        &deltaspace_id,
                        &obj_key,
                        delta_bytes,
                        size,
                        sha256,
                        md5,
                        content_type.clone(),
                        user_metadata.clone(),
                        multipart_etag.clone(),
                        &xnode,
                    )
                    .await?;
                drop((ref_spool, delta_spool, _guard, xnode));
                self.metadata_cache
                    .insert(bucket, key, result.metadata.clone());
                Ok(result.with_accounting(prior_for_counter, reference_created_bytes))
            }
        }
    }

    /// The streaming PUT's body, stored as passthrough (the ratio lost, or no
    /// spool for the encode). The caller holds no deltaspace lock:
    /// `store_passthrough_file_inner` takes it.
    #[allow(clippy::too_many_arguments)]
    async fn store_spooled_body_as_passthrough(
        &self,
        bucket: &str,
        key: &str,
        body: &crate::deltaglider::spool::Spool,
        size: u64,
        content_type: Option<String>,
        user_metadata: std::collections::HashMap<String, String>,
        etag: String,
    ) -> Result<StoreResult, EngineError> {
        let result = self
            .store_passthrough_file_inner(
                bucket,
                key,
                body.path(),
                Some(body),
                size,
                content_type,
                user_metadata,
                etag,
            )
            .await?;
        self.metadata_cache
            .insert(bucket, key, result.metadata.clone());
        Ok(result)
    }

    /// Persist a pre-computed delta (from the streaming PUT path) as a delta
    /// object. Mirrors the delta-commit tail of `commit_delta_or_passthrough`,
    /// but takes the original `size` explicitly (the body isn't in RAM) and the
    /// already-encoded `delta` bytes.
    #[allow(clippy::too_many_arguments)]
    async fn commit_streamed_delta(
        &self,
        bucket: &str,
        deltaspace_id: &str,
        obj_key: &ObjectKey,
        delta: Vec<u8>,
        size: u64,
        sha256: String,
        md5: String,
        content_type: Option<String>,
        user_metadata: std::collections::HashMap<String, String>,
        multipart_etag: Option<String>,
        xnode: &super::ReferenceLockGuard,
    ) -> Result<StoreResult, EngineError> {
        let ref_meta = self
            .storage
            .get_reference_metadata(bucket, deltaspace_id)
            .await?;
        self.with_metrics(|m| {
            m.delta_decisions_total.with_label_values(&["delta"]).inc();
            let saved = size.saturating_sub(delta.len() as u64);
            m.delta_bytes_saved_total.inc_by(saved);
        });
        let mut metadata = FileMetadata::new_delta(
            obj_key.filename.clone(),
            sha256,
            md5,
            size,
            "reference.bin".to_string(),
            ref_meta.file_sha256.clone(),
            delta.len() as u64,
            content_type,
        );
        metadata.user_metadata = user_metadata;
        metadata.multipart_etag = multipart_etag;
        let stored_size = delta.len() as u64;
        // The delta is only valid against the reference we locked.
        xnode.ensure_held().await?;
        self.storage
            .put_delta(bucket, deltaspace_id, &obj_key.filename, &delta, &metadata)
            .await?;
        // Clean up any prior passthrough variant at this key.
        if let Err(e) = self
            .delete_passthrough_idempotent(bucket, deltaspace_id, &obj_key.filename)
            .await
        {
            warn!("Failed to clean up passthrough after delta write: {}", e);
        }
        Ok(StoreResult::new(metadata, stored_size))
    }

    /// Decide whether to commit the encoded delta or fall back to passthrough,
    /// then persist the chosen storage strategy.
    async fn commit_delta_or_passthrough(
        &self,
        ctx: StoreContext<'_>,
        ref_meta: &FileMetadata,
        has_existing_reference: bool,
        delta: Vec<u8>,
        ratio: f32,
        xnode: &super::ReferenceLockGuard,
    ) -> Result<StoreResult, EngineError> {
        // S-P1-1: re-evaluate the ratio on every PUT, not just the
        // first one in the deltaspace. Pre-fix, the threshold gate
        // was `!has_existing_reference && ratio >= effective_ratio` —
        // once any file pinned the reference, every subsequent file
        // was forced into delta storage regardless of cost. A 1 KB
        // sentinel followed by a 50 MB unrelated file produced a 50
        // MB delta + 1 KB reference (worse than the 50 MB plain
        // passthrough would have been). When the deltas were against
        // unrelated bytes, storage grew without bound.
        //
        // Post-fix: the ratio is checked unconditionally. When the
        // delta is poor, we store passthrough. Three sub-cases:
        //
        //   1. `!has_existing_reference` AND poor ratio — same as
        //      before, except now we also tear down the just-written
        //      reference (next file may benefit; the heuristic is
        //      "don't pin a reference for a deltaspace whose first
        //      file proves we don't have a useful baseline").
        //   2. `has_existing_reference` AND poor ratio — NEW
        //      behaviour. Other delta files in this deltaspace need
        //      the reference, so we KEEP the reference and only
        //      store this single file as passthrough.
        //   3. Good ratio — commit as delta as before.
        let effective_ratio = self.bucket_policies.max_delta_ratio(ctx.bucket);
        if ratio >= effective_ratio {
            debug!(
                "Delta ratio {:.2} >= {:.2} (has_existing_reference={}), storing as passthrough",
                ratio, effective_ratio, has_existing_reference
            );
            self.with_metrics(|m| {
                m.delta_decisions_total
                    .with_label_values(&["passthrough"])
                    .inc()
            });
            let del_bucket = ctx.bucket.to_string();
            let del_dsid = ctx.deltaspace_id.to_string();
            let del_filename = ctx.obj_key.filename.clone();
            // Write passthrough FIRST, then clean up. This prevents
            // transient 404s on concurrent GETs during strategy
            // transition.
            let result = self.store_passthrough(ctx).await?;
            // Always tear down any prior delta for THIS key (we just
            // overwrote it with passthrough at the same logical key).
            if let Err(e) = self
                .delete_delta_idempotent(&del_bucket, &del_dsid, &del_filename)
                .await
            {
                warn!("Failed to clean up delta after passthrough write: {}", e);
            }
            // Reference cleanup ONLY when we just minted the reference
            // for this PUT (case 1). If the reference pre-existed, it
            // belongs to other delta siblings and must stay.
            if !has_existing_reference {
                let cache_key = self.cache_key(&del_bucket, &del_dsid);
                self.cache.invalidate(&cache_key);
                if let Err(e) = xnode
                    .delete_reference(&*self.storage, &del_bucket, &del_dsid)
                    .await
                {
                    warn!(
                        "Failed to clean up reference after passthrough write: {}",
                        e
                    );
                }
            }
            return Ok(result);
        }

        // Commit as delta
        self.with_metrics(|m| {
            m.delta_decisions_total.with_label_values(&["delta"]).inc();
            let saved = ctx.data.len().saturating_sub(delta.len()) as u64;
            m.delta_bytes_saved_total.inc_by(saved);
        });
        let mut metadata = FileMetadata::new_delta(
            ctx.obj_key.filename.clone(),
            ctx.sha256,
            ctx.md5,
            ctx.data.len() as u64,
            "reference.bin".to_string(),
            ref_meta.file_sha256.clone(),
            delta.len() as u64,
            ctx.content_type,
        );
        metadata.user_metadata = ctx.user_metadata;
        metadata.multipart_etag = ctx.multipart_etag;

        // Write delta first, then clean up old passthrough variant. The delta
        // is only valid against the reference we locked.
        xnode.ensure_held().await?;
        self.storage
            .put_delta(
                ctx.bucket,
                ctx.deltaspace_id,
                &ctx.obj_key.filename,
                &delta,
                &metadata,
            )
            .await?;
        if let Err(e) = self
            .delete_passthrough_idempotent(ctx.bucket, ctx.deltaspace_id, &ctx.obj_key.filename)
            .await
        {
            warn!(
                "Failed to clean up old passthrough after delta write: {}",
                e
            );
        }

        Ok(StoreResult::new(metadata, delta.len() as u64))
    }

    /// Store the internal deltaspace reference baseline.
    async fn set_reference_baseline(
        &self,
        ctx: &StoreContext<'_>,
        xnode: &super::ReferenceLockGuard,
    ) -> Result<FileMetadata, EngineError> {
        let metadata = FileMetadata::new_reference(
            Self::INTERNAL_REFERENCE_NAME.to_string(),
            ctx.obj_key.full_key(),
            ctx.sha256.clone(),
            ctx.md5.clone(),
            ctx.data.len() as u64,
            ctx.content_type.clone(),
        );

        xnode
            .put_reference(
                &*self.storage,
                ctx.bucket,
                ctx.deltaspace_id,
                ctx.data,
                &metadata,
            )
            .await?;

        self.with_metrics(|m| {
            m.delta_decisions_total
                .with_label_values(&["reference"])
                .inc()
        });

        let cache_key = self.cache_key(ctx.bucket, ctx.deltaspace_id);
        self.cache
            .put(&cache_key, Bytes::copy_from_slice(ctx.data), &ctx.sha256);

        Ok(metadata)
    }

    /// True iff a read reference's DG metadata is missing/corrupt (its S3
    /// `x-amz-meta-dg-*` headers / xattr were stripped — the classic "copied
    /// without --metadata" damage). Both conditions matter: a fully-stripped
    /// reference reads back as a `Passthrough` fallback (fails `is_reference`),
    /// and a partially-stripped one that kept `dg-note=reference` but lost its
    /// SHA reads as a Reference with an empty `file_sha256`. A HEALTHY reference
    /// always carries a non-empty `file_sha256`. NB: a transient backend error
    /// surfaces as `Err` from `get_reference_metadata`, never as this shape —
    /// so this can never mistake a 503/timeout for corruption.
    fn reference_metadata_is_corrupt(m: &FileMetadata) -> bool {
        !m.is_reference() || m.file_sha256.is_empty()
    }

    /// Heal a reference whose DG metadata was stripped, RE-STAMPING it (same
    /// bytes, correct headers) so future reads resolve it and content-diff
    /// replication stops re-copying the whole deltaspace every run. Returns the
    /// (possibly re-stamped) reference metadata to encode against.
    ///
    /// Bytes are UNCHANGED — every existing sibling delta was encoded against
    /// these bytes and must still reconstruct (the decode uses the reference
    /// bytes, not its `ref_sha256`). Caller MUST hold the per-deltaspace prefix
    /// lock. `original_name` is pinned to `INTERNAL_REFERENCE_NAME` — the
    /// legacy-reference migrator keys off that to skip already-internal
    /// references; any other value would trigger a spurious migration.
    async fn heal_reference_if_corrupt(
        &self,
        bucket: &str,
        deltaspace_id: &str,
        ref_meta: FileMetadata,
        // A spool the caller already holds (the streaming PUT body).
        held: Option<&crate::deltaglider::spool::Spool>,
        xnode: &super::ReferenceLockGuard,
    ) -> Result<FileMetadata, EngineError> {
        if !Self::reference_metadata_is_corrupt(&ref_meta) {
            return Ok(ref_meta); // healthy — zero extra I/O
        }
        warn!(
            "Reference {}/{} has stripped DG metadata — re-stamping (bytes unchanged) \
             so replication stops re-copying this deltaspace",
            bucket, deltaspace_id
        );
        // Materialise the intact reference bytes to a spool, re-hash, re-put
        // with correct metadata. Reserve the spool at the reference's on-disk
        // size (fall back to the fallback-reported size, which is the object's
        // content length — accurate for the reference object).
        let spool = self
            .spool_acquire_beside(held, ref_meta.file_size.max(1))
            .await?;
        self.storage
            .get_reference_to_file(bucket, deltaspace_id, spool.path())
            .await?;
        let (sha256, md5, size) = Self::hash_spool_file(spool.path()).await?;
        let healed = FileMetadata::new_reference(
            Self::INTERNAL_REFERENCE_NAME.to_string(),
            // source_name is cosmetic (display/label only; not used by decode,
            // verify, or replication compare). A stable placeholder that equals
            // the read-side default keeps re-reads idempotent.
            Self::INTERNAL_REFERENCE_NAME.to_string(),
            sha256,
            md5,
            size,
            ref_meta.content_type.clone(),
        );
        xnode
            .put_reference_from_file(&*self.storage, bucket, deltaspace_id, spool.path(), &healed)
            .await?;
        self.cache
            .invalidate(&self.cache_key(bucket, deltaspace_id));
        Ok(healed)
    }

    /// Stream-hash a spool file → (sha256_hex, md5_hex, byte_len). Bounded
    /// memory (256KiB chunks). Shared by the store hash path and the reference
    /// heal.
    async fn hash_spool_file(path: &Path) -> Result<(String, String, u64), EngineError> {
        let path = path.to_path_buf();
        tokio::task::spawn_blocking(move || -> std::io::Result<(String, String, u64)> {
            use std::io::Read;
            let mut f = std::fs::File::open(&path)?;
            let mut sh = Sha256::new();
            let mut mh = Md5::new();
            let mut observed: u64 = 0;
            let mut buf = vec![0u8; 256 * 1024];
            loop {
                let n = f.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                observed = observed.saturating_add(n as u64);
                sh.update(&buf[..n]);
                mh.update(&buf[..n]);
            }
            Ok((
                hex::encode(sh.finalize()),
                hex::encode(mh.finalize()),
                observed,
            ))
        })
        .await
        .map_err(|e| EngineError::Storage(StorageError::Other(format!("hash task: {e}"))))?
        .map_err(|e| EngineError::Storage(StorageError::from(e)))
    }

    /// Check if a key's filename is eligible for delta compression.
    pub fn is_delta_eligible(&self, key: &str) -> bool {
        let obj_key = ObjectKey::parse("_", key);
        self.file_router.is_delta_eligible(&obj_key.filename)
    }

    /// Store a non-delta-eligible object from pre-split chunks without assembling
    /// into a contiguous buffer. Computes SHA256 and MD5 incrementally.
    #[instrument(skip(self, chunks, user_metadata))]
    pub async fn store_passthrough_chunked(
        &self,
        bucket: &str,
        key: &str,
        chunks: &[Bytes],
        total_size: u64,
        content_type: Option<String>,
        user_metadata: HashMap<String, String>,
    ) -> Result<StoreResult, EngineError> {
        self.store_recorded(
            bucket,
            key,
            self.store_passthrough_chunked_inner(
                bucket,
                key,
                chunks,
                total_size,
                content_type,
                user_metadata,
                None,
            ),
        )
        .await
    }

    /// Multipart-aware variant of [`Self::store_passthrough_chunked`]. The
    /// `multipart_etag` is persisted on metadata so HEAD/GET/LIST return
    /// it verbatim (H1 correctness fix).
    #[instrument(skip(self, chunks, user_metadata, multipart_etag))]
    #[allow(clippy::too_many_arguments)]
    pub async fn store_passthrough_chunked_with_multipart_etag(
        &self,
        bucket: &str,
        key: &str,
        chunks: &[Bytes],
        total_size: u64,
        content_type: Option<String>,
        user_metadata: HashMap<String, String>,
        multipart_etag: String,
    ) -> Result<StoreResult, EngineError> {
        self.store_recorded(
            bucket,
            key,
            self.store_passthrough_chunked_inner(
                bucket,
                key,
                chunks,
                total_size,
                content_type,
                user_metadata,
                Some(multipart_etag),
            ),
        )
        .await
    }

    /// Run a non-recording passthrough store and record it in the usage
    /// counter exactly once. The prior object is resolved BEFORE `store` is
    /// polled (futures are lazy), so an overwrite nets to +0 objects.
    async fn store_recorded(
        &self,
        bucket: &str,
        key: &str,
        store: impl std::future::Future<Output = Result<StoreResult, EngineError>>,
    ) -> Result<StoreResult, EngineError> {
        let prior = self.prior_for_counter(bucket, key).await;
        let result = store.await?.with_accounting(prior, 0);
        self.record_store(bucket, &result);
        Ok(result)
    }

    /// Guard a PASSTHROUGH store against the passthrough ceiling (64 GiB
    /// default), NOT the much smaller delta `max_object_size`. Every
    /// passthrough sink shares this — a new one that forgets the check would
    /// let an object past the wrong limit. (The delta path uses its own
    /// strategy-aware ceiling in `store_spooled_delta`; don't route it here.)
    fn ensure_within_passthrough_ceiling(&self, total_size: u64) -> Result<(), EngineError> {
        if total_size > self.max_passthrough_object_size {
            return Err(EngineError::TooLarge {
                size: total_size,
                max: self.max_passthrough_object_size,
            });
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn store_passthrough_chunked_inner(
        &self,
        bucket: &str,
        key: &str,
        chunks: &[Bytes],
        total_size: u64,
        content_type: Option<String>,
        user_metadata: HashMap<String, String>,
        multipart_etag: Option<String>,
    ) -> Result<StoreResult, EngineError> {
        self.ensure_within_passthrough_ceiling(total_size)?;

        // Invalidate stale metadata on overwrite
        self.metadata_cache.invalidate(bucket, key);

        let (obj_key, deltaspace_id) = self.validated_key_ingest(bucket, key)?;

        // Compute SHA256 + MD5 incrementally across chunks
        let mut sha256_hasher = Sha256::new();
        let mut md5_hasher = Md5::new();
        for chunk in chunks {
            sha256_hasher.update(chunk);
            md5_hasher.update(chunk);
        }
        let sha256 = hex::encode(sha256_hasher.finalize());
        let md5 = hex::encode(md5_hasher.finalize());

        info!(
            "Storing chunked {}/{} ({} bytes, {} chunks, sha256={})",
            bucket,
            key,
            total_size,
            chunks.len(),
            &sha256[..8]
        );

        let _guard = self.acquire_prefix_lock(bucket, &deltaspace_id).await;

        let mut metadata = FileMetadata::new_passthrough(
            obj_key.filename.clone(),
            sha256,
            md5,
            total_size,
            content_type,
        );
        metadata.user_metadata = user_metadata;
        metadata.multipart_etag = multipart_etag;

        self.storage
            .put_passthrough_chunked(bucket, &deltaspace_id, &obj_key.filename, chunks, &metadata)
            .await?;
        // Write succeeded — now safe to clean up old delta variant
        if let Err(e) = self
            .delete_delta_idempotent(bucket, &deltaspace_id, &obj_key.filename)
            .await
        {
            warn!(
                "Failed to clean up old delta after chunked passthrough write: {}",
                e
            );
        }

        let result = StoreResult::new(metadata, total_size);
        self.metadata_cache
            .insert(bucket, key, result.metadata.clone());
        // NB: recorded in the public delegators, not here (shared inner).
        Ok(result)
    }

    /// Store a passthrough object from relayed multipart part files without
    /// materializing an assembled temporary file.
    #[instrument(skip(self, part_paths, user_metadata, multipart_etag))]
    #[allow(clippy::too_many_arguments)]
    pub async fn store_passthrough_relayed_parts_with_multipart_etag(
        &self,
        bucket: &str,
        key: &str,
        part_paths: &[PathBuf],
        total_size: u64,
        content_type: Option<String>,
        user_metadata: HashMap<String, String>,
        multipart_etag: String,
    ) -> Result<StoreResult, EngineError> {
        self.store_recorded(
            bucket,
            key,
            self.store_passthrough_relayed_parts_inner(
                bucket,
                key,
                part_paths,
                total_size,
                content_type,
                user_metadata,
                multipart_etag,
            ),
        )
        .await
    }

    /// Body of [`Self::store_passthrough_relayed_parts_with_multipart_etag`]; does not record the counter.
    #[allow(clippy::too_many_arguments)]
    async fn store_passthrough_relayed_parts_inner(
        &self,
        bucket: &str,
        key: &str,
        part_paths: &[PathBuf],
        total_size: u64,
        content_type: Option<String>,
        user_metadata: HashMap<String, String>,
        multipart_etag: String,
    ) -> Result<StoreResult, EngineError> {
        self.ensure_within_passthrough_ceiling(total_size)?;

        self.metadata_cache.invalidate(bucket, key);
        let (obj_key, deltaspace_id) = self.validated_key_ingest(bucket, key)?;

        let mut sha256_hasher = Sha256::new();
        let mut md5_hasher = Md5::new();
        let mut observed = 0u64;
        let mut buf = vec![0u8; 1024 * 1024];
        for path in part_paths {
            let mut file = tokio::fs::File::open(path)
                .await
                .map_err(StorageError::from)?;
            loop {
                let n = file.read(&mut buf).await.map_err(StorageError::from)?;
                if n == 0 {
                    break;
                }
                observed = observed.saturating_add(n as u64);
                sha256_hasher.update(&buf[..n]);
                md5_hasher.update(&buf[..n]);
            }
        }
        if observed != total_size {
            return Err(EngineError::Storage(StorageError::Other(format!(
                "Multipart relay size mismatch: expected {}, observed {}",
                total_size, observed
            ))));
        }
        let sha256 = hex::encode(sha256_hasher.finalize());
        let md5 = hex::encode(md5_hasher.finalize());
        // The relay parts hold spool budget (the multipart store reserves it
        // per part), so this op is a holder and never waits.
        let parts_mib = crate::deltaglider::spool::mib_ceil(total_size);
        let reserved = self
            .reserve_storage_spool(bucket, total_size, true, parts_mib)
            .await?;
        let _guard = self.acquire_prefix_lock(bucket, &deltaspace_id).await;

        let mut metadata = FileMetadata::new_passthrough(
            obj_key.filename.clone(),
            sha256,
            md5,
            total_size,
            content_type,
        );
        metadata.user_metadata = user_metadata;
        metadata.multipart_etag = Some(multipart_etag);

        self.storage
            .put_passthrough_parts(
                bucket,
                &deltaspace_id,
                &obj_key.filename,
                part_paths,
                &metadata,
                SpoolBudget::new(&self.spool, None, reserved.as_ref()),
            )
            .await?;
        if let Err(e) = self
            .delete_delta_idempotent(bucket, &deltaspace_id, &obj_key.filename)
            .await
        {
            warn!(
                "Failed to clean up old delta after relayed passthrough write: {}",
                e
            );
        }

        let result = StoreResult::new(metadata, total_size);
        self.metadata_cache
            .insert(bucket, key, result.metadata.clone());
        // NB: recorded in the public wrapper, not here (shared inner).
        Ok(result)
    }

    /// Store a passthrough object from a local file path, computing hashes
    /// incrementally to avoid reconstructing large multipart payloads in memory.
    #[instrument(skip(self, user_metadata, multipart_etag))]
    #[allow(clippy::too_many_arguments)]
    pub async fn store_passthrough_file_with_multipart_etag(
        &self,
        bucket: &str,
        key: &str,
        source_path: &Path,
        total_size: u64,
        content_type: Option<String>,
        user_metadata: HashMap<String, String>,
        multipart_etag: String,
    ) -> Result<StoreResult, EngineError> {
        self.store_recorded(
            bucket,
            key,
            self.store_passthrough_file_inner(
                bucket,
                key,
                source_path,
                None,
                total_size,
                content_type,
                user_metadata,
                multipart_etag,
            ),
        )
        .await
    }

    /// Body of [`Self::store_passthrough_file_with_multipart_etag`]; does not record the counter.
    #[allow(clippy::too_many_arguments)]
    async fn store_passthrough_file_inner(
        &self,
        bucket: &str,
        key: &str,
        source_path: &Path,
        held: Option<&crate::deltaglider::spool::Spool>,
        total_size: u64,
        content_type: Option<String>,
        user_metadata: HashMap<String, String>,
        multipart_etag: String,
    ) -> Result<StoreResult, EngineError> {
        self.ensure_within_passthrough_ceiling(total_size)?;

        self.metadata_cache.invalidate(bucket, key);
        let (obj_key, deltaspace_id) = self.validated_key_ingest(bucket, key)?;

        let mut file = tokio::fs::File::open(source_path)
            .await
            .map_err(StorageError::from)?;
        let mut buf = vec![0u8; 1024 * 1024];
        let mut sha256_hasher = Sha256::new();
        let mut md5_hasher = Md5::new();
        let mut observed = 0u64;
        loop {
            let n = file.read(&mut buf).await.map_err(StorageError::from)?;
            if n == 0 {
                break;
            }
            observed = observed.saturating_add(n as u64);
            sha256_hasher.update(&buf[..n]);
            md5_hasher.update(&buf[..n]);
        }
        if observed != total_size {
            return Err(EngineError::Storage(StorageError::Other(format!(
                "Multipart relay size mismatch: expected {}, observed {}",
                total_size, observed
            ))));
        }
        let sha256 = hex::encode(sha256_hasher.finalize());
        let md5 = hex::encode(md5_hasher.finalize());
        let reserved = self
            .reserve_storage_spool(
                bucket,
                total_size,
                false,
                held.map_or(0, crate::deltaglider::spool::Spool::reserved_mib),
            )
            .await?;
        let _guard = self.acquire_prefix_lock(bucket, &deltaspace_id).await;

        let mut metadata = FileMetadata::new_passthrough(
            obj_key.filename.clone(),
            sha256,
            md5,
            total_size,
            content_type,
        );
        metadata.user_metadata = user_metadata;
        metadata.multipart_etag = Some(multipart_etag);

        self.storage
            .put_passthrough_file(
                bucket,
                &deltaspace_id,
                &obj_key.filename,
                source_path,
                &metadata,
                SpoolBudget::new(&self.spool, held, reserved.as_ref()),
            )
            .await?;
        if let Err(e) = self
            .delete_delta_idempotent(bucket, &deltaspace_id, &obj_key.filename)
            .await
        {
            warn!(
                "Failed to clean up old delta after relay passthrough write: {}",
                e
            );
        }

        let result = StoreResult::new(metadata, total_size);
        self.metadata_cache
            .insert(bucket, key, result.metadata.clone());
        // NB: recorded in the public wrapper, not here (shared inner).
        Ok(result)
    }

    /// Begin a streaming passthrough multipart upload (Phase B). Gated on
    /// `max_passthrough_object_size`. Acquires the per-deltaspace lock and
    /// holds it for the lifetime of the returned handle, mirroring
    /// `store_passthrough_chunked_inner`. The caller drives parts via
    /// [`Self::upload_passthrough_part`] then finalizes with
    /// [`Self::finish_passthrough_multipart`] (or aborts).
    #[allow(clippy::too_many_arguments)]
    pub async fn begin_passthrough_multipart(
        &self,
        bucket: &str,
        key: &str,
        total_size: u64,
        content_type: Option<String>,
        user_metadata: HashMap<String, String>,
    ) -> Result<PassthroughMultipartHandle, EngineError> {
        self.ensure_within_passthrough_ceiling(total_size)?;

        self.metadata_cache.invalidate(bucket, key);
        let (obj_key, deltaspace_id) = self.validated_key_ingest(bucket, key)?;
        let guard = self.acquire_prefix_lock(bucket, &deltaspace_id).await;

        // The create call needs metadata headers (content-type, user
        // metadata) so the backend stamps them at create time.
        let mut create_meta = FileMetadata::new_passthrough(
            obj_key.filename.clone(),
            String::new(),
            String::new(),
            total_size,
            content_type.clone(),
        );
        create_meta.user_metadata = user_metadata.clone();

        let upload = self
            .storage
            .create_multipart_upload(bucket, &deltaspace_id, &obj_key.filename, &create_meta)
            .await?;

        Ok(PassthroughMultipartHandle {
            bucket: bucket.to_string(),
            key: key.to_string(),
            deltaspace_id,
            filename: obj_key.filename,
            total_size,
            content_type,
            user_metadata,
            upload,
            _guard: guard,
        })
    }

    /// Upload one part of an in-progress passthrough multipart upload.
    /// IMMUTABLE in the handle so the caller can drive parts CONCURRENTLY.
    /// Returns the [`UploadedPart`] receipt (the caller collects + sorts
    /// them). Memory stays O(in-flight parts) — the bytes are owned by the
    /// caller's bounded `buffer_unordered` and dropped after this returns.
    pub async fn upload_passthrough_part(
        &self,
        handle: &PassthroughMultipartHandle,
        part_number: i32,
        data: Bytes,
    ) -> Result<UploadedPart, EngineError> {
        let part = self
            .storage
            .upload_part(
                &handle.upload,
                &handle.deltaspace_id,
                &handle.filename,
                part_number,
                data,
            )
            .await?;
        Ok(part)
    }

    /// Finalize a passthrough multipart upload: complete on the backend,
    /// write FileMetadata (with the supplied source hashes + multipart ETag),
    /// and clean the old delta variant. Consumes the handle (releases the
    /// lock). `parts` and `assembled` must be in part-number order;
    /// `assembled` is empty for native backends.
    #[allow(clippy::too_many_arguments)]
    pub async fn finish_passthrough_multipart(
        &self,
        mut handle: PassthroughMultipartHandle,
        mut parts: Vec<UploadedPart>,
        assembled: Vec<Bytes>,
        sha256: String,
        md5: String,
        multipart_etag: Option<String>,
    ) -> Result<StoreResult, EngineError> {
        // `parts` must be part-number-ordered for the multipart complete;
        // `assembled` (buffering backends only) is already caller-ordered.
        parts.sort_by_key(|p| p.part_number);

        // Overwrite-net accounting for the usage counter (see store_inner). The
        // handle already holds the per-deltaspace lock, so this is race-safe.
        let prior_for_counter = self.prior_for_counter(&handle.bucket, &handle.key).await;

        let mut metadata = FileMetadata::new_passthrough(
            handle.filename.clone(),
            sha256,
            md5,
            handle.total_size,
            handle.content_type.clone(),
        );
        metadata.user_metadata = std::mem::take(&mut handle.user_metadata);
        metadata.multipart_etag = multipart_etag;

        if let Err(e) = self
            .storage
            .complete_multipart_upload(
                &handle.upload,
                &handle.deltaspace_id,
                &handle.filename,
                &parts,
                &assembled,
                &metadata,
            )
            .await
        {
            // Complete failed: abort so the fully-uploaded part set doesn't
            // dangle on backends (B2) that never GC incomplete uploads.
            self.abort_passthrough_multipart_ref(&handle).await;
            return Err(e.into());
        }

        if let Err(e) = self
            .delete_delta_idempotent(&handle.bucket, &handle.deltaspace_id, &handle.filename)
            .await
        {
            warn!(
                "Failed to clean up old delta after multipart passthrough write: {}",
                e
            );
        }

        let result =
            StoreResult::new(metadata, handle.total_size).with_accounting(prior_for_counter, 0);
        self.metadata_cache
            .insert(&handle.bucket, &handle.key, result.metadata.clone());
        // Streaming multipart (large replication/lifecycle copies). Every
        // public store entry point records exactly once; `counter_tests`
        // pins the accounting against the buffered `store()` oracle.
        self.record_store(&handle.bucket, &result);
        Ok(result)
    }

    /// Abort an in-progress passthrough multipart upload (best-effort)
    /// WITHOUT consuming the handle — callers holding it behind an Arc can
    /// abort at any strong count (the prefix lock releases when the last
    /// clone drops).
    pub async fn abort_passthrough_multipart_ref(&self, handle: &PassthroughMultipartHandle) {
        if let Err(e) = self
            .storage
            .abort_multipart_upload(&handle.upload, &handle.deltaspace_id, &handle.filename)
            .await
        {
            warn!(
                "Failed to abort multipart upload {}/{}: {}",
                handle.bucket, handle.key, e
            );
        }
    }

    /// Store as passthrough without delta compression
    async fn store_passthrough(&self, ctx: StoreContext<'_>) -> Result<StoreResult, EngineError> {
        let mut metadata = FileMetadata::new_passthrough(
            ctx.obj_key.filename.clone(),
            ctx.sha256,
            ctx.md5,
            ctx.data.len() as u64,
            ctx.content_type,
        );
        metadata.user_metadata = ctx.user_metadata;
        metadata.multipart_etag = ctx.multipart_etag;

        self.storage
            .put_passthrough(
                ctx.bucket,
                ctx.deltaspace_id,
                &ctx.obj_key.filename,
                ctx.data,
                &metadata,
            )
            .await?;

        Ok(StoreResult::new(metadata, ctx.data.len() as u64))
    }

    /// Delete a storage object, ignoring NotFound errors (idempotent delete).
    /// Swallow NotFound errors from a storage delete — the object is already gone.
    fn delete_ignoring_not_found(result: Result<(), StorageError>) -> Result<(), EngineError> {
        match result {
            Ok(()) | Err(StorageError::NotFound(_)) => Ok(()),
            Err(other) => Err(other.into()),
        }
    }

    /// Delete a delta file, ignoring NotFound (idempotent).
    async fn delete_delta_idempotent(
        &self,
        bucket: &str,
        deltaspace_id: &str,
        filename: &str,
    ) -> Result<(), EngineError> {
        Self::delete_ignoring_not_found(
            self.storage
                .delete_delta(bucket, deltaspace_id, filename)
                .await,
        )
    }

    /// Delete a passthrough file, ignoring NotFound (idempotent).
    pub(crate) async fn delete_passthrough_idempotent(
        &self,
        bucket: &str,
        deltaspace_id: &str,
        filename: &str,
    ) -> Result<(), EngineError> {
        Self::delete_ignoring_not_found(
            self.storage
                .delete_passthrough(bucket, deltaspace_id, filename)
                .await,
        )
    }

    pub(super) async fn migrate_legacy_reference_object_if_needed(
        &self,
        bucket: &str,
        deltaspace_id: &str,
        filename: &str,
        xnode: &super::ReferenceLockGuard,
    ) -> Result<bool, EngineError> {
        if !self.storage.has_reference(bucket, deltaspace_id).await? {
            return Ok(false);
        }

        let mut ref_meta = self
            .storage
            .get_reference_metadata(bucket, deltaspace_id)
            .await?;
        if ref_meta.original_name == Self::INTERNAL_REFERENCE_NAME {
            return Ok(false);
        }
        if ref_meta.original_name != filename {
            return Ok(false);
        }

        let (reference, _cache_hit) = self
            .get_reference_cached(bucket, deltaspace_id, &ref_meta.file_sha256)
            .await?;
        let source_file = self.codec_source_spool_now(reference.len())?;
        let _codec_permit = self.codec_semaphore.acquire().await.map_err(|_| {
            EngineError::Storage(StorageError::Other("codec semaphore closed".into()))
        })?;
        let delta = self
            .codec
            .encode_spooled(&source_file, &reference, &reference)?;
        drop(_codec_permit);

        let delta_meta = FileMetadata::new_delta(
            filename.to_string(),
            ref_meta.file_sha256.clone(),
            ref_meta.md5.clone(),
            ref_meta.file_size,
            "reference.bin".to_string(),
            ref_meta.file_sha256.clone(),
            delta.len() as u64,
            ref_meta.content_type.clone(),
        );

        // Write the delta BEFORE deleting the passthrough. If put_delta fails,
        // the passthrough still exists and the object remains accessible.
        self.storage
            .put_delta(bucket, deltaspace_id, filename, &delta, &delta_meta)
            .await?;
        self.delete_passthrough_idempotent(bucket, deltaspace_id, filename)
            .await?;

        ref_meta.original_name = Self::INTERNAL_REFERENCE_NAME.to_string();
        xnode
            .put_reference_metadata(&*self.storage, bucket, deltaspace_id, &ref_meta)
            .await?;

        // Invalidate cache — reference metadata changed (though data is unchanged,
        // the cached Bytes doesn't include metadata, so this is precautionary).
        let cache_key = self.cache_key(bucket, deltaspace_id);
        self.cache.invalidate(&cache_key);

        Ok(true)
    }

    /// Batch-migrate all legacy reference objects in a bucket.
    /// Returns (migrated_count, skipped_count, error_count).
    pub async fn migrate_legacy_references(
        &self,
        bucket: &str,
    ) -> Result<(u32, u32, u32), EngineError> {
        let deltaspaces = self.storage.list_deltaspaces(bucket).await?;
        let mut migrated = 0u32;
        let mut skipped = 0u32;
        let mut errors = 0u32;

        for ds in &deltaspaces {
            // Check if reference exists and needs migration
            if !self.storage.has_reference(bucket, ds).await? {
                skipped += 1;
                continue;
            }

            let ref_meta = match self.storage.get_reference_metadata(bucket, ds).await {
                Ok(m) => m,
                Err(_) => {
                    skipped += 1;
                    continue;
                }
            };

            if ref_meta.original_name == Self::INTERNAL_REFERENCE_NAME {
                skipped += 1;
                continue;
            }

            // This is a legacy reference — migrate it
            let filename = ref_meta.original_name.clone();
            let _guard = self.acquire_prefix_lock(bucket, ds).await;
            let xnode = match self.acquire_reference_lock(bucket, ds).await {
                Ok(g) => g,
                Err(e) => {
                    tracing::warn!("Failed to migrate {}/{}: {}", bucket, ds, e);
                    errors += 1;
                    continue;
                }
            };
            match self
                .migrate_legacy_reference_object_if_needed(bucket, ds, &filename, &xnode)
                .await
            {
                Ok(true) => {
                    tracing::info!("Migrated legacy reference in {}/{}", bucket, ds);
                    migrated += 1;
                }
                Ok(false) => {
                    skipped += 1;
                }
                Err(e) => {
                    tracing::warn!("Failed to migrate {}/{}: {}", bucket, ds, e);
                    errors += 1;
                }
            }
        }

        Ok((migrated, skipped, errors))
    }
}

/// Usage-counter accounting across every public store entry point. The
/// buffered `store()` path is the oracle: any other path that stores the same
/// bytes under the same keys must leave the counter in the same state.
#[cfg(test)]
mod counter_tests {
    use super::*;
    use crate::bucket_usage::{BucketUsage, BucketUsageRow};
    use crate::config::Config;
    use crate::storage::FilesystemBackend;

    const BUCKET: &str = "counter-bkt";

    struct Harness {
        _tmp: tempfile::TempDir,
        usage: Arc<BucketUsage>,
        engine: DeltaGliderEngine<FilesystemBackend>,
    }

    async fn harness() -> Harness {
        let tmp = tempfile::tempdir().unwrap();
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .unwrap();
        backend.create_bucket(BUCKET).await.unwrap();
        let usage = Arc::new(BucketUsage::in_memory().unwrap());
        let engine =
            DeltaGliderEngine::new_with_backend(Arc::new(backend), &Config::default(), None)
                .with_bucket_usage(Some(usage.clone()));
        Harness {
            _tmp: tmp,
            usage,
            engine,
        }
    }

    fn row(h: &Harness) -> (u64, u64, u64) {
        h.usage.flush_pending();
        let r: BucketUsageRow = h.usage.read(BUCKET).unwrap().unwrap_or(BucketUsageRow {
            object_count: 0,
            logical_bytes: 0,
            stored_bytes: 0,
            last_scan_at: None,
        });
        (r.object_count, r.logical_bytes, r.stored_bytes)
    }

    /// Two versions of a delta-eligible artifact: v2 is v1 with a small edit.
    fn versions() -> (Vec<u8>, Vec<u8>) {
        let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
        let v1: Vec<u8> = (0..200_000)
            .map(|_| {
                x ^= x >> 12;
                x ^= x << 25;
                x ^= x >> 27;
                (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 56) as u8
            })
            .collect();
        let mut v2 = v1.clone();
        v2[100_000..100_100].fill(0xAB);
        (v1, v2)
    }

    async fn buffered(h: &Harness, key: &str, data: &[u8]) {
        h.engine
            .store(BUCKET, key, data, None, HashMap::new())
            .await
            .unwrap();
    }

    async fn spooled(h: &Harness, key: &str, data: &[u8]) {
        let spool = h.engine.spool_acquire(data.len() as u64).await.unwrap();
        tokio::fs::write(spool.path(), data).await.unwrap();
        h.engine
            .store_spooled_delta(
                BUCKET,
                key,
                &spool,
                data.len() as u64,
                None,
                HashMap::new(),
                None,
            )
            .await
            .unwrap();
    }

    async fn relayed(h: &Harness, key: &str, data: &[u8]) {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = data.split_at(data.len() / 2);
        let paths = vec![dir.path().join("p1"), dir.path().join("p2")];
        tokio::fs::write(&paths[0], a).await.unwrap();
        tokio::fs::write(&paths[1], b).await.unwrap();
        h.engine
            .store_passthrough_relayed_parts_with_multipart_etag(
                BUCKET,
                key,
                &paths,
                data.len() as u64,
                None,
                HashMap::new(),
                "\"0123456789abcdef0123456789abcdef-2\"".to_string(),
            )
            .await
            .unwrap();
    }

    async fn file(h: &Harness, key: &str, data: &[u8]) {
        let f = tempfile::NamedTempFile::new().unwrap();
        tokio::fs::write(f.path(), data).await.unwrap();
        h.engine
            .store_passthrough_file_with_multipart_etag(
                BUCKET,
                key,
                f.path(),
                data.len() as u64,
                None,
                HashMap::new(),
                "\"0123456789abcdef0123456789abcdef-1\"".to_string(),
            )
            .await
            .unwrap();
    }

    async fn chunked(h: &Harness, key: &str, data: &[u8]) {
        let chunks: Vec<Bytes> = data.chunks(64 * 1024).map(Bytes::copy_from_slice).collect();
        h.engine
            .store_passthrough_chunked_with_multipart_etag(
                BUCKET,
                key,
                &chunks,
                data.len() as u64,
                None,
                HashMap::new(),
                "\"0123456789abcdef0123456789abcdef-3\"".to_string(),
            )
            .await
            .unwrap();
    }

    /// Streaming delta PUTs: a fresh baseline, a sibling delta, then an
    /// overwrite. Pre-fix the delta-win branch never recorded (count 0).
    #[tokio::test]
    async fn spooled_delta_matches_buffered() {
        let (v1, v2) = versions();
        let (oracle, h) = (harness().await, harness().await);
        for (key, data) in [("rel/a.zip", &v1), ("rel/b.zip", &v2), ("rel/b.zip", &v1)] {
            buffered(&oracle, key, data).await;
            spooled(&h, key, data).await;
        }
        assert_eq!(row(&oracle).0, 2, "oracle: two keys");
        assert_eq!(row(&h), row(&oracle));
    }

    /// Streaming PUT of a non-delta-eligible key, then an overwrite. Pre-fix
    /// the overwrite counted a second object.
    #[tokio::test]
    async fn spooled_passthrough_overwrite_matches_buffered() {
        let (v1, v2) = versions();
        let (oracle, h) = (harness().await, harness().await);
        for data in [&v1, &v2] {
            buffered(&oracle, "img/a.jpg", data).await;
            spooled(&h, "img/a.jpg", data).await;
        }
        assert_eq!(row(&oracle).0, 1);
        assert_eq!(row(&h), row(&oracle));
    }

    /// Every multipart passthrough sink must net an overwrite to +0 objects.
    #[tokio::test]
    async fn multipart_sinks_overwrite_counts_one_object() {
        let (v1, v2) = versions();
        for sink in ["relayed", "file", "chunked"] {
            let h = harness().await;
            for data in [&v1, &v2] {
                match sink {
                    "relayed" => relayed(&h, "img/a.jpg", data).await,
                    "file" => file(&h, "img/a.jpg", data).await,
                    _ => chunked(&h, "img/a.jpg", data).await,
                }
            }
            let (count, logical, _) = row(&h);
            assert_eq!(count, 1, "{sink}: overwrite must not add an object");
            assert_eq!(logical, v2.len() as u64, "{sink}: logical bytes of v2 only");
        }
    }
}

/// D8: the buffered encode must not trust a cached reference that no longer
/// matches the stored one (a peer node reseeded the deltaspace).
#[cfg(test)]
mod stale_reference_cache_tests {
    use super::*;
    use crate::config::Config;
    use crate::storage::FilesystemBackend;

    fn noise(seed: u64, n: usize) -> Vec<u8> {
        let mut x = seed;
        (0..n)
            .map(|_| {
                x ^= x >> 12;
                x ^= x << 25;
                x ^= x >> 27;
                (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 56) as u8
            })
            .collect()
    }

    #[tokio::test]
    async fn buffered_store_reloads_a_reseeded_reference() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = Arc::new(
            FilesystemBackend::new(tmp.path().to_path_buf())
                .await
                .unwrap(),
        );
        backend.create_bucket("b").await.unwrap();
        let engine = DeltaGliderEngine::new_with_backend(backend.clone(), &Config::default(), None);

        // v1 seeds the reference and the cache.
        let v1 = noise(1, 200_000);
        engine
            .store("b", "rel/a.zip", &v1, None, HashMap::new())
            .await
            .unwrap();

        // A peer node reseeds reference.bin with different bytes + metadata.
        let r2 = noise(2, 200_000);
        let r2_meta = FileMetadata::new_reference(
            DeltaGliderEngine::<FilesystemBackend>::INTERNAL_REFERENCE_NAME.to_string(),
            "rel/other.zip".into(),
            hex::encode(Sha256::digest(&r2)),
            hex::encode(Md5::digest(&r2)),
            r2.len() as u64,
            None,
        );
        backend
            .put_reference("b", "rel", &r2, &r2_meta)
            .await
            .unwrap();

        // v2 is close to v1: against the STALE cached v1 it deltas well and
        // gets committed as a delta, stamped with r2's sha.
        let mut v2 = v1.clone();
        v2[1000..1100].fill(0xAB);
        engine
            .store("b", "rel/b.zip", &v2, None, HashMap::new())
            .await
            .unwrap();
        // Another node (or this one after a restart) has no cached copy and
        // decodes against the stored reference.
        let fresh = DeltaGliderEngine::new_with_backend(backend, &Config::default(), None);
        let (back, _) = fresh.retrieve("b", "rel/b.zip").await.expect("readable");
        assert_eq!(back, v2);
    }
}

/// Tier 4: a streaming PUT holds its body spool while it needs the ref +
/// delta pair. Under a small budget it waited for its own budget until the
/// acquire timeout (120 s) and then failed.
#[cfg(test)]
mod spool_budget_tests {
    use super::*;
    use crate::config::Config;
    use crate::storage::FilesystemBackend;

    #[tokio::test]
    async fn streaming_put_does_not_wait_on_its_own_body_spool() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = FilesystemBackend::new(tmp.path().join("data"))
            .await
            .unwrap();
        backend.create_bucket("b").await.unwrap();
        let mut engine =
            DeltaGliderEngine::new_with_backend(Arc::new(backend), &Config::default(), None);
        // 2 MiB budget; each object is 1.5 MiB, so body + pair > budget.
        engine.spool = Arc::new(
            crate::deltaglider::spool::SpoolDir::new(tmp.path().join("spool"), 2 * 1024 * 1024)
                .unwrap(),
        );
        let v1: Vec<u8> = (0..1_500_000u32).map(|n| (n % 251) as u8).collect();
        let mut v2 = v1.clone();
        v2[700_000..700_100].fill(0xAB);
        for (key, data) in [("rel/a.zip", &v1), ("rel/b.zip", &v2)] {
            let body = engine.spool_acquire(data.len() as u64).await.unwrap();
            tokio::fs::write(body.path(), data).await.unwrap();
            let put = engine.store_spooled_delta(
                "b",
                key,
                &body,
                data.len() as u64,
                None,
                HashMap::new(),
                None,
            );
            tokio::time::timeout(std::time::Duration::from_secs(10), put)
                .await
                .unwrap_or_else(|_| panic!("{key}: streaming PUT stalled on its own spool"))
                .unwrap();
        }
        let (back, _) = engine.retrieve("b", "rel/b.zip").await.unwrap();
        assert_eq!(back, v2);
    }
}

/// Tier 4: a GET served from the metadata cache must not re-insert the entry
/// (each insert restarts moka's TTL, so a hot key never expired).
#[cfg(test)]
mod metadata_cache_ttl_tests {
    use super::*;
    use crate::config::Config;
    use crate::storage::FilesystemBackend;

    #[tokio::test]
    async fn cache_hits_do_not_restart_the_ttl() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .unwrap();
        backend.create_bucket("b").await.unwrap();
        let engine =
            DeltaGliderEngine::new_with_backend(Arc::new(backend), &Config::default(), None);
        for key in ["rel/a.zip", "img/a.jpg"] {
            engine
                .store("b", key, b"payload bytes", None, HashMap::new())
                .await
                .unwrap();
            let after_put = engine.metadata_cache().insert_count();
            for _ in 0..3 {
                engine.retrieve("b", key).await.unwrap();
                let _ = engine.retrieve_stream_range("b", key, 0, 3, None).await;
            }
            assert_eq!(
                engine.metadata_cache().insert_count(),
                after_put,
                "{key}: cache hits re-inserted the entry"
            );
        }
    }
}

/// Tier 3: the per-deltaspace lock is per BUCKET too. Keyed by the prefix
/// alone, `a/releases` and `b/releases` shared one mutex.
#[cfg(test)]
mod prefix_lock_scope_tests {
    use super::*;
    use crate::config::Config;
    use crate::storage::FilesystemBackend;

    #[tokio::test]
    async fn same_prefix_in_two_buckets_does_not_contend() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .unwrap();
        let engine =
            DeltaGliderEngine::new_with_backend(Arc::new(backend), &Config::default(), None);
        let _held = engine.acquire_prefix_lock("bucket-a", "releases").await;
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            engine.acquire_prefix_lock("bucket-b", "releases"),
        )
        .await
        .expect("another bucket's deltaspace must not wait");
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(100),
                engine.acquire_prefix_lock("bucket-a", "releases"),
            )
            .await
            .is_err(),
            "the same deltaspace must still serialise"
        );
    }
}

/// A rebuilt engine (config reload) serves while the old one drains its
/// in-flight PUTs. Each built its own prefix-lock map, so the two engines'
/// writers to one deltaspace did not exclude each other.
#[cfg(test)]
mod prefix_lock_rebuild_tests {
    use super::*;
    use crate::config::Config;
    use crate::storage::FilesystemBackend;

    #[tokio::test]
    async fn rebuilt_engines_share_prefix_locks() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = Arc::new(
            FilesystemBackend::new(tmp.path().to_path_buf())
                .await
                .unwrap(),
        );
        let old = DeltaGliderEngine::new_with_backend(backend.clone(), &Config::default(), None);
        let new = DeltaGliderEngine::new_with_backend(backend, &Config::default(), None);
        let bucket = format!("rebuild-{}", uuid::Uuid::new_v4());
        let _held = old.acquire_prefix_lock(&bucket, "releases").await;
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(100),
                new.acquire_prefix_lock(&bucket, "releases"),
            )
            .await
            .is_err(),
            "the new engine must wait for the old engine's writer"
        );
    }
}

/// A rebuilt engine (config reload) must share the spool budget with the
/// engine it replaces: each built its own, so a reload doubled the budget.
#[cfg(test)]
mod spool_singleton_tests {
    use super::*;
    use crate::config::Config;
    use crate::storage::FilesystemBackend;

    #[tokio::test]
    async fn rebuilt_engines_share_one_spool_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = Arc::new(
            FilesystemBackend::new(tmp.path().to_path_buf())
                .await
                .unwrap(),
        );
        let a = DeltaGliderEngine::new_with_backend(backend.clone(), &Config::default(), None);
        let b = DeltaGliderEngine::new_with_backend(backend, &Config::default(), None);
        assert!(a.spool.same_budget(&b.spool));
    }
}

#[cfg(test)]
mod review2_tests {
    use super::*;
    use crate::config::Config;
    use crate::storage::FilesystemBackend;

    /// Review-2 (7692271b): a streaming PUT no longer waits on its OWN body
    /// spool, but two PUTs still wait on EACH OTHER's (hold-and-wait): both
    /// hold a body reservation and ask for a pair that only fits once the
    /// other releases. Both stall until DGP_SPOOL_ACQUIRE_TIMEOUT_SECS, then 503.
    #[tokio::test]
    async fn review2_two_streaming_puts_do_not_deadlock_on_each_others_body_spool() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = FilesystemBackend::new(tmp.path().join("data"))
            .await
            .unwrap();
        backend.create_bucket("b").await.unwrap();
        let mut engine =
            DeltaGliderEngine::new_with_backend(Arc::new(backend), &Config::default(), None);
        engine.spool = Arc::new(
            crate::deltaglider::spool::SpoolDir::new(tmp.path().join("spool"), 4 * 1024 * 1024)
                .unwrap(),
        );
        let v: Vec<u8> = (0..1_500_000u32).map(|n| (n % 251) as u8).collect();
        let body_a = engine.spool_acquire(v.len() as u64).await.unwrap();
        let body_b = engine.spool_acquire(v.len() as u64).await.unwrap();
        tokio::fs::write(body_a.path(), &v).await.unwrap();
        tokio::fs::write(body_b.path(), &v).await.unwrap();
        let a = engine.store_spooled_delta(
            "b",
            "x/a.zip",
            &body_a,
            v.len() as u64,
            None,
            HashMap::new(),
            None,
        );
        let b = engine.store_spooled_delta(
            "b",
            "y/b.zip",
            &body_b,
            v.len() as u64,
            None,
            HashMap::new(),
            None,
        );
        let (ra, rb) = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            futures::future::join(a, b),
        )
        .await
        .expect("two streaming PUTs deadlocked on each other's body spool");
        ra.unwrap();
        rb.unwrap();
    }

    /// An engine over an encrypting filesystem backend with a small spool.
    async fn encrypted_engine(
        tmp: &tempfile::TempDir,
        spool_bytes: u64,
    ) -> DeltaGliderEngine<crate::storage::EncryptingBackend<FilesystemBackend>> {
        let backend = FilesystemBackend::new(tmp.path().join("data"))
            .await
            .unwrap();
        backend.create_bucket("b").await.unwrap();
        let cfg = Arc::new(arc_swap::ArcSwap::new(Arc::new(
            crate::storage::EncryptionConfig {
                key: Some(crate::storage::EncryptionKey::from_hex(&"ab".repeat(32)).unwrap()),
                key_id: Some("kid".into()),
                ..Default::default()
            },
        )));
        let wrapper = crate::storage::EncryptingBackend::new(backend, cfg);
        let mut engine =
            DeltaGliderEngine::new_with_backend(Arc::new(wrapper), &Config::default(), None);
        engine.spool = Arc::new(
            crate::deltaglider::spool::SpoolDir::new(tmp.path().join("spool"), spool_bytes)
                .unwrap(),
        );
        engine
    }

    /// Two streaming passthrough PUTs to an encrypting backend: each holds
    /// its body spool, and the encrypt needs as much again. Neither may wait
    /// for the other's body (hold-and-wait): each stores, or fails at once
    /// with a retryable SlowDown.
    #[tokio::test]
    async fn encrypted_streaming_puts_do_not_deadlock_on_each_others_body_spool() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = encrypted_engine(&tmp, 4 * 1024 * 1024).await;
        let v = vec![3u8; 1_500_000];
        let body_a = engine.spool_acquire(v.len() as u64).await.unwrap();
        let body_b = engine.spool_acquire(v.len() as u64).await.unwrap();
        tokio::fs::write(body_a.path(), &v).await.unwrap();
        tokio::fs::write(body_b.path(), &v).await.unwrap();
        {
            let put = |key: &'static str, body| {
                engine.store_spooled_delta(
                    "b",
                    key,
                    body,
                    v.len() as u64,
                    None,
                    HashMap::new(),
                    None,
                )
            };
            let (ra, rb) = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                futures::future::join(put("x/a.png", &body_a), put("y/b.png", &body_b)),
            )
            .await
            .expect("two encrypted streaming PUTs deadlocked on each other's body spool");
            for r in [ra, rb] {
                match r {
                    Ok(_) | Err(EngineError::Overloaded(_)) => {}
                    Err(e) => panic!("unexpected error: {e:?}"),
                }
            }
        }
        // With room for both, both store and read back.
        drop((body_a, body_b));
        let body = engine.spool_acquire(v.len() as u64).await.unwrap();
        tokio::fs::write(body.path(), &v).await.unwrap();
        engine
            .store_spooled_delta(
                "b",
                "x/a.png",
                &body,
                v.len() as u64,
                None,
                HashMap::new(),
                None,
            )
            .await
            .unwrap();
        let (got, _) = engine.retrieve("b", "x/a.png").await.unwrap();
        assert_eq!(got, v);
    }

    /// The buffered codec's source file is a spool file. A buffered PUT
    /// encodes under the deltaspace lock, so it never waits for budget (a
    /// full budget is SlowDown at once); a GET holds nothing and waits.
    #[tokio::test]
    async fn buffered_codec_source_is_a_spool_file() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = FilesystemBackend::new(tmp.path().join("data"))
            .await
            .unwrap();
        backend.create_bucket("b").await.unwrap();
        let mut engine =
            DeltaGliderEngine::new_with_backend(Arc::new(backend), &Config::default(), None);
        engine.spool = Arc::new(
            crate::deltaglider::spool::SpoolDir::new(tmp.path().join("spool"), 4 * 1024 * 1024)
                .unwrap(),
        );
        let engine = Arc::new(engine);
        let v1: Vec<u8> = (0..300_000u32).map(|n| (n % 251) as u8).collect();
        let mut v2 = v1.clone();
        v2[1000] ^= 0xff;
        engine
            .store("b", "rel/a.zip", &v1, None, HashMap::new())
            .await
            .unwrap();

        let full = engine.spool_acquire(4 * 1024 * 1024).await.unwrap();
        let r = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            engine.store("b", "rel/b.zip", &v2, None, HashMap::new()),
        )
        .await
        .expect("a buffered PUT must not wait for spool budget under the lock");
        assert!(matches!(r, Err(EngineError::Overloaded(_))), "got {r:?}");
        drop(full);
        engine
            .store("b", "rel/b.zip", &v2, None, HashMap::new())
            .await
            .unwrap();

        // GET: waits for the budget, then decodes.
        let full = engine.spool_acquire(4 * 1024 * 1024).await.unwrap();
        let e = engine.clone();
        let get = tokio::spawn(async move { e.retrieve("b", "rel/b.zip").await });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(!get.is_finished(), "the GET waits for spool budget");
        drop(full);
        let (got, _) = tokio::time::timeout(std::time::Duration::from_secs(10), get)
            .await
            .expect("the GET goes on once the budget is free")
            .unwrap()
            .unwrap();
        assert_eq!(got, v2);
    }

    /// A relayed multipart store holds spool budget: its relay parts. So it
    /// never waits for more (two completing uploads would each wait for the
    /// other's parts); with the budget in use it is a SlowDown at once.
    #[tokio::test]
    async fn encrypted_relayed_store_never_waits_for_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let engine = encrypted_engine(&tmp, 4 * 1024 * 1024).await;
        let part = tmp.path().join("part1");
        let v = vec![5u8; 1024 * 1024];
        tokio::fs::write(&part, &v).await.unwrap();
        let store = || {
            engine.store_passthrough_relayed_parts_with_multipart_etag(
                "b",
                "x/a.png",
                std::slice::from_ref(&part),
                v.len() as u64,
                None,
                HashMap::new(),
                "\"0123456789abcdef0123456789abcdef-1\"".to_string(),
            )
        };
        let full = engine.spool_acquire(4 * 1024 * 1024).await.unwrap();
        let r = tokio::time::timeout(std::time::Duration::from_secs(5), store())
            .await
            .expect("a relayed store must not wait for spool budget");
        assert!(matches!(r, Err(EngineError::Overloaded(_))), "got {r:?}");
        drop(full);
        store().await.unwrap();
        let (got, _) = engine.retrieve("b", "x/a.png").await.unwrap();
        assert_eq!(got, v);
    }

    /// Review-2 (144b303b): the deltaspace lock is keyed by the VIRTUAL
    /// bucket. Two bucket names aliased onto one real bucket share one
    /// `reference.bin`, but take different locks, so two first PUTs can both
    /// create a baseline.
    #[tokio::test]
    async fn review2_two_virtual_names_of_one_storage_share_the_deltaspace_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let yaml = format!(
            r#"
storage:
  backends:
    - name: local-disk
      type: filesystem
      path: {}
  buckets:
    releases:
      backend: local-disk
      alias: shared
    downloads:
      backend: local-disk
      alias: shared
"#,
            tmp.path().display()
        );
        let cfg = Config::from_yaml_str(&yaml).unwrap();
        let engine = DeltaGliderEngine::new(&cfg, None).await.unwrap();
        // The reference cache too: one entry for one reference.bin, so an
        // invalidation through one name reaches a read through the other.
        assert_eq!(
            engine.cache_key("releases", "fw"),
            engine.cache_key("downloads", "fw")
        );
        assert_ne!(
            engine.cache_key("releases", "fw"),
            engine.cache_key("releases", "fx")
        );
        let _held = engine.acquire_prefix_lock("releases", "fw").await;
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(200),
                engine.acquire_prefix_lock("downloads", "fw"),
            )
            .await
            .is_err(),
            "the same real deltaspace through two names must serialise"
        );
    }
}
