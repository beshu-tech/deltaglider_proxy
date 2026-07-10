// SPDX-License-Identifier: GPL-3.0-only

//! Store pipeline — delta encoding, passthrough, and baseline management.

use super::*;
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

        let (obj_key, deltaspace_id) = Self::validated_key_ingest(bucket, key)?;

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
            let _guard = self.acquire_prefix_lock(&deltaspace_id).await;
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
        let _guard = self.acquire_prefix_lock(&deltaspace_id).await;

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
        let has_existing_reference = self
            .storage
            .has_reference(ctx.bucket, ctx.deltaspace_id)
            .await?;

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
            self.storage
                .get_reference_metadata(ctx.bucket, ctx.deltaspace_id)
                .await?
        } else {
            debug!("No reference in deltaspace, creating baseline");
            self.set_reference_baseline(&ctx).await?
        };

        // Encode delta and decide: keep as delta or fall back to direct storage
        let result = match self
            .encode_and_store(ctx, &ref_meta, has_existing_reference)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                if !has_existing_reference {
                    // Best-effort: undo the reference we just created.
                    // Errors here are logged but do not mask the
                    // original encode failure.
                    let cache_key = Self::cache_key(bucket, &deltaspace_id);
                    self.cache.invalidate(&cache_key);
                    if let Err(cleanup_err) =
                        self.storage.delete_reference(bucket, &deltaspace_id).await
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
    ) -> Result<StoreResult, EngineError> {
        let (reference, _cache_hit) = self
            .get_reference_cached(ctx.bucket, ctx.deltaspace_id)
            .await?;
        // PERF: try_acquire instead of acquire — fail fast with 503 when all codec
        // slots are busy rather than queuing unbounded requests in memory (each
        // holding a full object body while waiting for a permit).
        let _codec_permit = self.try_acquire_codec()?;
        // spawn_blocking: xdelta3 is CPU-bound; data must be owned ('static).
        let ref_clone = reference.clone();
        let data_owned = ctx.data.to_vec();
        let codec = self.codec.clone();
        let encode_start = Instant::now();
        let delta = tokio::task::spawn_blocking(move || codec.encode(&ref_clone, &data_owned))
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

        self.commit_delta_or_passthrough(ctx, ref_meta, has_existing_reference, delta, ratio)
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
        use tokio::io::AsyncReadExt;

        self.metadata_cache.invalidate(bucket, key);
        let (obj_key, deltaspace_id) = Self::validated_key_ingest(bucket, key)?;
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
        let (sha256, md5, observed) = {
            let path = body.path().to_path_buf();
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
            .map_err(|e| EngineError::Storage(StorageError::from(e)))?
        };
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
                .store_passthrough_file_with_multipart_etag(
                    bucket,
                    key,
                    body.path(),
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

        // B1 (open): this lock is IN-PROCESS only — two instances writing the
        // same deltaspace can corrupt reference.bin (see CLAUDE.md HA contract).
        let _guard = self.acquire_prefix_lock(&deltaspace_id).await;
        // Write path: a backend error must abort, not read as "no reference".
        let has_existing_reference = self.storage.has_reference(bucket, &deltaspace_id).await?;

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
            self.storage
                .put_reference_from_file(bucket, &deltaspace_id, body.path(), &ref_meta)
                .await?;
            self.with_metrics(|m| {
                m.delta_decisions_total
                    .with_label_values(&["reference"])
                    .inc()
            });
            // The streaming path doesn't pre-cache the reference bytes; next GET
            // loads fresh.
            self.cache
                .invalidate(&Self::cache_key(bucket, &deltaspace_id));
            // Fall through — the encode block below now sees has_existing_reference
            // effectively true (the reference is on disk).
        }

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
        let ref_size = self
            .storage
            .get_reference_metadata(bucket, &deltaspace_id)
            .await
            .map(|m| m.file_size)
            .unwrap_or(size);
        let (ref_spool, delta_spool) = self.spool_acquire_pair(ref_size, size).await?;
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
                // Drop the prefix lock FIRST — store_passthrough_file re-acquires
                // it internally, so holding it here would re-entrant-deadlock.
                drop((ref_spool, delta_spool, _guard));
                let result = self
                    .store_passthrough_file_with_multipart_etag(
                        bucket,
                        key,
                        body.path(),
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
                self.metadata_cache
                    .insert(bucket, key, result.metadata.clone());
                Ok(result.with_accounting(prior_for_counter, 0))
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
                    )
                    .await?;
                drop((ref_spool, delta_spool, _guard));
                self.metadata_cache
                    .insert(bucket, key, result.metadata.clone());
                Ok(result.with_accounting(prior_for_counter, 0))
            }
        }
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
                let cache_key = Self::cache_key(&del_bucket, &del_dsid);
                self.cache.invalidate(&cache_key);
                if let Err(e) = self.storage.delete_reference(&del_bucket, &del_dsid).await {
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

        // Write delta first, then clean up old passthrough variant
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
    ) -> Result<FileMetadata, EngineError> {
        let metadata = FileMetadata::new_reference(
            Self::INTERNAL_REFERENCE_NAME.to_string(),
            ctx.obj_key.full_key(),
            ctx.sha256.clone(),
            ctx.md5.clone(),
            ctx.data.len() as u64,
            ctx.content_type.clone(),
        );

        self.storage
            .put_reference(ctx.bucket, ctx.deltaspace_id, ctx.data, &metadata)
            .await?;

        self.with_metrics(|m| {
            m.delta_decisions_total
                .with_label_values(&["reference"])
                .inc()
        });

        let cache_key = Self::cache_key(ctx.bucket, ctx.deltaspace_id);
        self.cache.put(&cache_key, Bytes::copy_from_slice(ctx.data));

        Ok(metadata)
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
        let result = self
            .store_passthrough_chunked_inner(
                bucket,
                key,
                chunks,
                total_size,
                content_type,
                user_metadata,
                None,
            )
            .await?;
        self.record_store(bucket, &result);
        Ok(result)
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
        let result = self
            .store_passthrough_chunked_inner(
                bucket,
                key,
                chunks,
                total_size,
                content_type,
                user_metadata,
                Some(multipart_etag),
            )
            .await?;
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

        let (obj_key, deltaspace_id) = Self::validated_key_ingest(bucket, key)?;

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

        let _guard = self.acquire_prefix_lock(&deltaspace_id).await;

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
        self.ensure_within_passthrough_ceiling(total_size)?;

        self.metadata_cache.invalidate(bucket, key);
        let (obj_key, deltaspace_id) = Self::validated_key_ingest(bucket, key)?;

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
        let _guard = self.acquire_prefix_lock(&deltaspace_id).await;

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
        self.record_store(bucket, &result);
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
        self.ensure_within_passthrough_ceiling(total_size)?;

        self.metadata_cache.invalidate(bucket, key);
        let (obj_key, deltaspace_id) = Self::validated_key_ingest(bucket, key)?;

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
        let _guard = self.acquire_prefix_lock(&deltaspace_id).await;

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
        self.record_store(bucket, &result);
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
        let (obj_key, deltaspace_id) = Self::validated_key_ingest(bucket, key)?;
        let guard = self.acquire_prefix_lock(&deltaspace_id).await;

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
        // This is the 7th store-completing path (streaming multipart, used by
        // large replication/lifecycle copies) — record like the other 6.
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

        let (reference, _cache_hit) = self.get_reference_cached(bucket, deltaspace_id).await?;
        let _codec_permit = self.codec_semaphore.acquire().await.map_err(|_| {
            EngineError::Storage(StorageError::Other("codec semaphore closed".into()))
        })?;
        let delta = self.codec.encode(&reference, &reference)?;
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
        self.storage
            .put_reference_metadata(bucket, deltaspace_id, &ref_meta)
            .await?;

        // Invalidate cache — reference metadata changed (though data is unchanged,
        // the cached Bytes doesn't include metadata, so this is precautionary).
        let cache_key = Self::cache_key(bucket, deltaspace_id);
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
            let _guard = self.acquire_prefix_lock(ds).await;
            match self
                .migrate_legacy_reference_object_if_needed(bucket, ds, &filename)
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
