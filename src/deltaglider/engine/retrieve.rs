// SPDX-License-Identifier: BUSL-1.1

//! Retrieve pipeline — delta reconstruction, streaming, and range requests.

use super::*;
use crate::storage::StorageBackend;
use bytes::Bytes;
use futures::stream::BoxStream;

impl<S: StorageBackend> DeltaGliderEngine<S> {
    pub async fn retrieve(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<(Vec<u8>, FileMetadata), EngineError> {
        use futures::TryStreamExt;

        match self.retrieve_stream(bucket, key).await? {
            RetrieveResponse::Buffered { data, metadata, .. } => Ok((data, metadata)),
            RetrieveResponse::Streamed {
                stream, metadata, ..
            } => {
                // Collect stream into contiguous buffer (pre-allocated to exact size).
                let chunks: Vec<Bytes> = stream.map_err(EngineError::Storage).try_collect().await?;
                let total_len: usize = chunks.iter().map(|b| b.len()).sum();
                let mut data = Vec::with_capacity(total_len);
                for chunk in &chunks {
                    data.extend_from_slice(chunk);
                }
                Ok((data, metadata))
            }
        }
    }

    /// Retrieve an object with streaming support for passthrough files.
    ///
    /// Passthrough files are streamed from the backend without buffering (constant memory).
    /// Delta/reference files are reconstructed in memory (buffering required by xdelta3).
    #[instrument(skip(self))]
    pub async fn retrieve_stream(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<RetrieveResponse, EngineError> {
        let (obj_key, deltaspace_id) = self.validated_key(bucket, key)?;

        // Check metadata cache first (avoids resolve_metadata_with_migration I/O)
        let (metadata, from_cache) = if let Some(cached) = self.metadata_cache.get(bucket, key) {
            (Some(cached), true)
        } else {
            let resolved = self
                .resolve_metadata_with_migration(bucket, &deltaspace_id, &obj_key)
                .await?;
            (resolved, false)
        };

        let metadata = match metadata {
            Some(m) => {
                // Populate on a fresh resolve only: re-inserting a cache hit
                // restarts its TTL, so a hot key never expired.
                if !from_cache {
                    self.metadata_cache.insert(bucket, key, m.clone());
                }
                m
            }
            None => {
                // No DG metadata — try streaming as an unmanaged passthrough object
                if key.ends_with(".delta") || key.contains("reference.bin") {
                    warn!(
                        "PATHOLOGICAL | {}/{} has no DG metadata but looks like a delta/reference file. \
                         Delta reconstruction disabled. Re-upload through the proxy or re-copy with --metadata.",
                        bucket, key
                    );
                } else {
                    info!(
                        "No DG metadata for {}/{}, attempting direct passthrough",
                        bucket, key
                    );
                }
                return self
                    .try_unmanaged_passthrough(bucket, &deltaspace_id, &obj_key)
                    .await;
            }
        };

        info!(
            "Retrieving {}/{} (stored as {})",
            bucket,
            key,
            metadata.storage_info.label()
        );

        match self
            .retrieve_with_metadata(bucket, key, &deltaspace_id, &obj_key, metadata.clone())
            .await
        {
            Ok(response) => Ok(response),
            // A stale cache entry surfaces as EITHER a top-level NotFound OR a
            // storage-level NotFound: when the cached type is stale (e.g. cached
            // passthrough but the object is now stored as a delta), the read
            // hits the wrong storage path and the backend returns
            // Storage(NotFound) — NOT EngineError::NotFound. Matching only the
            // latter made this whole retry dead code, so a stale entry became a
            // hard 500 instead of recovering (X-ray H11).
            Err(e) if from_cache && e.is_not_found() => {
                // Stale cache entry — the object's storage type may have changed
                // (e.g., passthrough → delta) during a concurrent PUT. Invalidate
                // the cache and retry with fresh metadata from storage.
                warn!(
                    "Stale metadata cache for {}/{}, retrying with fresh metadata",
                    bucket, key
                );
                self.metadata_cache.invalidate(bucket, key);
                let fresh = self
                    .resolve_metadata_with_migration(bucket, &deltaspace_id, &obj_key)
                    .await?;
                match fresh {
                    Some(m) => {
                        self.metadata_cache.insert(bucket, key, m.clone());
                        self.retrieve_with_metadata(bucket, key, &deltaspace_id, &obj_key, m)
                            .await
                    }
                    None => {
                        self.try_unmanaged_passthrough(bucket, &deltaspace_id, &obj_key)
                            .await
                    }
                }
            }
            Err(e) => Err(e),
        }
    }

    /// Inner retrieve that uses pre-resolved metadata.
    async fn retrieve_with_metadata(
        &self,
        bucket: &str,
        _key: &str,
        deltaspace_id: &str,
        obj_key: &super::ObjectKey,
        metadata: FileMetadata,
    ) -> Result<RetrieveResponse, EngineError> {
        match &metadata.storage_info {
            StorageInfo::Passthrough => {
                let stored_name = passthrough_stored_name(&obj_key.filename, &metadata);
                let stream = self
                    .storage
                    .get_passthrough_stream(bucket, deltaspace_id, stored_name)
                    .await?;
                debug!("Streaming passthrough file for {}", obj_key.full_key());
                Ok(RetrieveResponse::Streamed {
                    stream,
                    metadata,
                    cache_hit: None,
                })
            }
            StorageInfo::Delta { .. } if metadata.file_size > self.spool_threshold() => {
                // Large delta: reconstruct to a spool file (bounded memory) and
                // stream the file to the client. Integrity is verified BEFORE the
                // first byte ships (see retrieve_delta_spooled).
                self.retrieve_delta_spooled(bucket, deltaspace_id, obj_key, metadata)
                    .await
            }
            StorageInfo::Reference { .. } | StorageInfo::Delta { .. } => {
                let (data, cache_hit) = self
                    .retrieve_buffered(bucket, deltaspace_id, obj_key, &metadata)
                    .await?;
                debug!(
                    "Retrieved (buffered) {} bytes for {}",
                    data.len(),
                    obj_key.full_key()
                );
                Ok(RetrieveResponse::Buffered {
                    data,
                    metadata,
                    cache_hit,
                })
            }
        }
    }

    /// Reconstruct a large delta object to a quota'd spool file, verify its
    /// SHA-256 BEFORE returning, then stream the spool file to the client.
    ///
    /// Memory is bounded by the codec pump (Spike A: 73MB on a 2.5GB decode), not
    /// the object size. The integrity gate (blocker 2) is preserved: we hash the
    /// reconstruction as the codec writes it and compare to `metadata.file_sha256`
    /// before the first response byte — a mismatch returns a clean S3 error, never
    /// a truncated 200. The codec permit is released at decode-done (blocker 5),
    /// so a slow client draining the spool never pins a codec slot.
    ///
    /// The verified reconstruction is shared as the range reads' is
    /// (storage-11): concurrent GETs of one object decode it once.
    async fn retrieve_delta_spooled(
        &self,
        bucket: &str,
        deltaspace_id: &str,
        obj_key: &super::ObjectKey,
        metadata: FileMetadata,
    ) -> Result<RetrieveResponse, EngineError> {
        let key = RangeSpoolKey::object(bucket, obj_key.full_key(), metadata.file_sha256.clone());
        let out_spool = self
            .range_spools
            .get_or_fill(key, || {
                self.reconstruct_delta_to_spool(bucket, deltaspace_id, obj_key, &metadata)
            })
            .await?;

        // Stream the verified spool file whole. This share of `out_spool` is
        // moved into the stream, so the file lives until the last byte is
        // read; the cache entry and other readers may hold it longer.
        let file = tokio::fs::File::open(out_spool.path())
            .await
            .map_err(StorageError::from)?;
        let reader = tokio_util::io::ReaderStream::new(file);
        let stream =
            futures::stream::unfold((reader, out_spool), |(mut reader, spool)| async move {
                use futures::StreamExt;
                match reader.next().await {
                    Some(Ok(b)) => Some((Ok(b), (reader, spool))),
                    Some(Err(e)) => Some((Err(StorageError::from(e)), (reader, spool))),
                    // This reader's share of the spool drops here; the file
                    // goes (and its budget frees) with the last share.
                    None => None,
                }
            });

        debug!(
            "Retrieved (spooled) {} bytes for {}",
            metadata.file_size,
            obj_key.full_key()
        );
        Ok(RetrieveResponse::Streamed {
            stream: Box::pin(stream),
            metadata,
            cache_hit: None,
        })
    }

    /// Reconstruct a delta object to a quota'd spool file and verify its SHA-256
    /// BEFORE returning. Shared by the full-GET (`retrieve_delta_spooled`) and the
    /// range path. Returns the verified `Spool` (its `Drop` deletes the file +
    /// releases the budget). Bounded memory (codec pump); the codec permit is
    /// released here, at decode-done — never held across the client download.
    async fn reconstruct_delta_to_spool(
        &self,
        bucket: &str,
        deltaspace_id: &str,
        obj_key: &super::ObjectKey,
        metadata: &FileMetadata,
    ) -> Result<crate::deltaglider::spool::Spool, EngineError> {
        use std::io::Write;

        let (ref_file, out_spool) = self
            .reference_file_and_out_spool(bucket, deltaspace_id, metadata)
            .await?;

        // Fetch the delta (small — it's a delta).
        let delta = self
            .storage
            .get_delta(bucket, deltaspace_id, &obj_key.filename)
            .await?;

        let _permit = self
            .acquire_codec_timeout(std::time::Duration::from_secs(60))
            .await?;
        let codec = self.codec.clone();
        let ref_path = ref_file.path().to_path_buf();
        let out_path = out_spool.path().to_path_buf();
        let decode_start = Instant::now();
        let expected_sha = metadata.file_sha256.clone();
        let key_for_err = obj_key.full_key();
        // Decompression-bomb cap: a correct reconstruction is EXACTLY file_size
        // bytes. Abort the write the moment output exceeds it — a crafted delta
        // can otherwise inflate to fill the spool dir (ENOSPC) past the reserved
        // budget, BEFORE the SHA gate runs (x-ray blocker — the buffered path's
        // max_stdout guard was missing on the streaming path).
        let output_cap = metadata.file_size;

        // A Write sink that tees to the spool file AND a running SHA-256, capped.
        let actual_sha = tokio::task::spawn_blocking(move || -> Result<String, EngineError> {
            struct HashingWriter<W: Write> {
                inner: W,
                hasher: Sha256,
                written: u64,
                cap: u64,
            }
            impl<W: Write> Write for HashingWriter<W> {
                fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                    self.written = self.written.saturating_add(buf.len() as u64);
                    if self.written > self.cap {
                        return Err(std::io::Error::other(format!(
                            "delta reconstruction exceeded expected size ({} > {} bytes)",
                            self.written, self.cap
                        )));
                    }
                    self.hasher.update(buf);
                    self.inner.write_all(buf)?;
                    Ok(buf.len())
                }
                fn flush(&mut self) -> std::io::Result<()> {
                    self.inner.flush()
                }
            }
            let file = std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&out_path)
                .map_err(|e| EngineError::Storage(StorageError::from(e)))?;
            let mut sink = HashingWriter {
                inner: std::io::BufWriter::new(file),
                hasher: Sha256::new(),
                written: 0,
                cap: output_cap,
            };
            codec
                .decode_to_writer(&ref_path, &delta[..], &mut sink)
                .map_err(EngineError::Codec)?;
            sink.flush()
                .map_err(|e| EngineError::Storage(StorageError::from(e)))?;
            Ok(hex::encode(sink.hasher.finalize()))
        })
        .await
        .map_err(|e| {
            EngineError::Storage(StorageError::Other(format!("decode task panicked: {e}")))
        })??;

        let decode_secs = decode_start.elapsed().as_secs_f64();
        drop(_permit); // release codec slot at decode-done, NOT download-done
        self.with_metrics(|m| m.delta_decode_duration_seconds.observe(decode_secs));

        // PRE-FLIGHT INTEGRITY GATE — before any byte ships. A shared
        // reference file was checked against the delta's `ref_sha256`, so a
        // mismatch is this object's fault: the reference stays shared.
        if actual_sha != expected_sha {
            if super::metadata::reference_suspect_after_mismatch(metadata) {
                self.cache
                    .invalidate(&self.cache_key(bucket, deltaspace_id));
            }
            warn!(
                "Checksum mismatch (spooled) for {}: expected {}, got {}",
                key_for_err, expected_sha, actual_sha
            );
            return Err(EngineError::ChecksumMismatch {
                key: key_for_err,
                expected: expected_sha,
                actual: actual_sha,
            });
        }

        // ref_file drops here: an own reference file is deleted, a share of
        // a cached one is released.
        Ok(out_spool)
    }

    /// The reference of a large delta GET as a local file, and the spool for
    /// the reconstruction.
    ///
    /// A delta that names its reference by `ref_sha256` reads a SHARED file:
    /// one download, checked against that SHA-256, kept like a reconstruction
    /// (`range_spools`: same TTL, spool budget and eviction). Concurrent GETs
    /// wait for the one download, and later GETs of any delta of the
    /// deltaspace read the file instead of downloading the reference again
    /// (and sending a HEAD for its size). The output spool is reserved first,
    /// so a GET waits for budget holding nothing; the download then takes
    /// the reference's spool beside it, without waiting (on no room, the GET
    /// downloads its own copy as before).
    ///
    /// Without a `ref_sha256` (or with the cache off) nothing can verify a
    /// shared copy: the GET downloads its own, with ONE reservation for both
    /// spools (two concurrent GETs never hold one while waiting for the
    /// other).
    async fn reference_file_and_out_spool(
        &self,
        bucket: &str,
        deltaspace_id: &str,
        metadata: &FileMetadata,
    ) -> Result<(ReferenceFile, crate::deltaglider::spool::Spool), EngineError> {
        let ref_sha256 = match &metadata.storage_info {
            StorageInfo::Delta { ref_sha256, .. } => ref_sha256.as_str(),
            _ => "",
        };
        if !ref_sha256.is_empty() && self.range_spools.caches() {
            let out = self.spool_acquire(metadata.file_size).await?;
            let key = RangeSpoolKey::reference(
                self.cache_key(bucket, deltaspace_id),
                ref_sha256.to_string(),
            );
            let shared = self
                .range_spools
                .get_or_fill(key, || {
                    self.download_verified_reference(
                        bucket,
                        deltaspace_id,
                        ref_sha256,
                        metadata.file_size,
                        &out,
                    )
                })
                .await;
            match shared {
                Ok(file) => return Ok((ReferenceFile::Shared(file), out)),
                Err(SharedReferenceError::NoRoom) => drop(out),
                Err(SharedReferenceError::Engine(e)) => return Err(e),
            }
        }
        // The reference can be far larger than the object: reserve its spool
        // at its own size (a HEAD), or the byte budget under-counts the disk.
        let ref_size = self
            .storage
            .get_reference_metadata(bucket, deltaspace_id)
            .await
            .map(|m| m.file_size)
            .unwrap_or(metadata.file_size);
        let (ref_spool, out_spool) = self
            .spool_acquire_pair(ref_size, metadata.file_size)
            .await?;
        // Materialise the reference as a seekable file WITHOUT heap-loading it
        // (Phase 2: filesystem hardlink / S3 stream-to-file).
        self.storage
            .get_reference_to_file(bucket, deltaspace_id, ref_spool.path())
            .await?;
        Ok((ReferenceFile::Own(ref_spool), out_spool))
    }

    /// Download the reference to a spool file beside `held` (never waits for
    /// budget) and check it against `ref_sha256`. Only a file that passes is
    /// shared.
    async fn download_verified_reference(
        &self,
        bucket: &str,
        deltaspace_id: &str,
        ref_sha256: &str,
        fallback_size: u64,
        held: &crate::deltaglider::spool::Spool,
    ) -> Result<crate::deltaglider::spool::Spool, SharedReferenceError> {
        let ref_size = self
            .storage
            .get_reference_metadata(bucket, deltaspace_id)
            .await
            .map(|m| m.file_size)
            .unwrap_or(fallback_size);
        let spool = match self.spool.acquire_beside(Some(held), ref_size).await {
            Ok(spool) => spool,
            Err(e) if e.kind() == crate::deltaglider::spool::CONTENDED => {
                return Err(SharedReferenceError::NoRoom)
            }
            Err(e) => return Err(EngineError::Storage(StorageError::from(e)).into()),
        };
        self.storage
            .get_reference_to_file(bucket, deltaspace_id, spool.path())
            .await
            .map_err(EngineError::from)?;
        let (actual, _, _) = Self::hash_spool_file(spool.path()).await?;
        if actual != ref_sha256 {
            warn!(
                "Reference {bucket}/{deltaspace_id} has sha256 {actual}, its deltas name \
                 {ref_sha256}: not shared"
            );
            return Err(EngineError::ChecksumMismatch {
                key: format!("{deltaspace_id}/.dg/reference.bin"),
                expected: ref_sha256.to_string(),
                actual,
            }
            .into());
        }
        Ok(spool)
    }

    /// Serve a byte range of a large delta object from a reconstructed spool file
    /// (blocker 6). Reconstructs once (verified), then seeks to `start` and
    /// streams `end-start+1` bytes — no full-object re-buffer. The verified
    /// spool is cached briefly (storage-11): the other ranges of the object
    /// read it instead of decoding again, and a concurrent range waits for a
    /// running decode. Returns `(stream, content_length)`.
    async fn retrieve_delta_range_spooled(
        &self,
        bucket: &str,
        deltaspace_id: &str,
        obj_key: &super::ObjectKey,
        metadata: &FileMetadata,
        start: u64,
        end_inclusive: u64,
    ) -> Result<(BoxStream<'static, Result<Bytes, StorageError>>, u64), EngineError> {
        use tokio::io::{AsyncReadExt, AsyncSeekExt};

        let key = RangeSpoolKey::object(bucket, obj_key.full_key(), metadata.file_sha256.clone());
        let out_spool = self
            .range_spools
            .get_or_fill(key, || {
                self.reconstruct_delta_to_spool(bucket, deltaspace_id, obj_key, metadata)
            })
            .await?;

        // Clamp the range to the object size; compute the content length.
        let size = metadata.file_size;
        let start = start.min(size);
        let end_excl = end_inclusive.saturating_add(1).min(size);
        let content_length = end_excl.saturating_sub(start);

        let mut file = tokio::fs::File::open(out_spool.path())
            .await
            .map_err(StorageError::from)?;
        file.seek(std::io::SeekFrom::Start(start))
            .await
            .map_err(StorageError::from)?;
        // `take` bounds the read to exactly the range length.
        let reader = file.take(content_length);
        let inner = tokio_util::io::ReaderStream::new(reader);
        let stream = futures::stream::unfold((inner, out_spool), |(mut inner, spool)| async move {
            use futures::StreamExt;
            match inner.next().await {
                Some(Ok(b)) => Some((Ok(b), (inner, spool))),
                Some(Err(e)) => Some((Err(StorageError::from(e)), (inner, spool))),
                None => None,
            }
        });
        Ok((Box::pin(stream), content_length))
    }

    /// Retrieve a byte range of a passthrough object with streaming support.
    ///
    /// Only passthrough objects benefit from range passthrough (the backend streams
    /// just the requested bytes). Delta/reference objects need full reconstruction
    /// regardless, so this method falls back to `retrieve_stream` for those.
    ///
    /// Returns `Ok(Some((stream, content_length)))` when the range was handled
    /// natively by the backend (passthrough only). Returns `Ok(None)` when the
    /// caller should fall back to the buffered path (delta/reference, or
    /// unmanaged objects where we don't know the storage type up front).
    /// `expected_source`: generation pin for multi-part copies. When `Some`,
    /// the metadata CACHE is bypassed (fresh resolve) and the resolved object
    /// must still match the pinned size + content hash — a concurrent
    /// overwrite mid-copy fails the range instead of silently mixing
    /// generations into the destination ("frankenobject" guard).
    #[instrument(skip(self, expected_source))]
    #[allow(clippy::type_complexity)]
    pub async fn retrieve_stream_range(
        &self,
        bucket: &str,
        key: &str,
        start: u64,
        end: u64,
        expected_source: Option<&FileMetadata>,
    ) -> Result<
        Option<(
            BoxStream<'static, Result<Bytes, StorageError>>,
            u64,
            FileMetadata,
        )>,
        EngineError,
    > {
        let (obj_key, deltaspace_id) = self.validated_key(bucket, key)?;

        // Check metadata cache first. Track cache provenance so a stale
        // strategy (e.g. passthrough cached before a concurrent rewrite to
        // delta) can be invalidated and retried like `retrieve_stream()`.
        // A generation-pinned read never trusts the cache: the pin exists to
        // DETECT change, and the same-node cache would mask it.
        let (metadata, from_cache) = if expected_source.is_none() {
            if let Some(cached) = self.metadata_cache.get(bucket, key) {
                (Some(cached), true)
            } else {
                (
                    self.resolve_metadata_with_migration(bucket, &deltaspace_id, &obj_key)
                        .await?,
                    false,
                )
            }
        } else {
            (
                self.resolve_metadata_with_migration(bucket, &deltaspace_id, &obj_key)
                    .await?,
                false,
            )
        };

        if let (Some(expected), Some(actual)) = (expected_source, metadata.as_ref()) {
            if !Self::same_generation(expected, actual) {
                return Err(EngineError::Storage(StorageError::PreconditionFailed(
                    format!(
                        "source changed during copy: {}/{} (size {} -> {})",
                        bucket, key, expected.file_size, actual.file_size
                    ),
                )));
            }
        }

        let metadata = match metadata {
            Some(m) => {
                if !from_cache {
                    self.metadata_cache.insert(bucket, key, m.clone());
                }
                m
            }
            None => {
                // Unmanaged object — we don't know if it's passthrough.
                // Signal caller to use the non-range path.
                return Ok(None);
            }
        };

        match &metadata.storage_info {
            StorageInfo::Passthrough => {
                let stored_name = passthrough_stored_name(&obj_key.filename, &metadata);
                let range_result = self
                    .storage
                    .get_passthrough_stream_range(bucket, &deltaspace_id, stored_name, start, end)
                    .await;
                let (stream, content_length) = match range_result {
                    Ok(v) => v,
                    Err(StorageError::NotFound(_)) if from_cache => {
                        warn!(
                            "Stale range metadata cache for {}/{}, retrying with fresh metadata",
                            bucket, key
                        );
                        self.metadata_cache.invalidate(bucket, key);
                        let fresh = self
                            .resolve_metadata_with_migration(bucket, &deltaspace_id, &obj_key)
                            .await?;
                        let Some(fresh_meta) = fresh else {
                            return Ok(None);
                        };
                        self.metadata_cache.insert(bucket, key, fresh_meta.clone());
                        match &fresh_meta.storage_info {
                            StorageInfo::Passthrough => {
                                let (stream, content_length) = self
                                    .storage
                                    .get_passthrough_stream_range(
                                        bucket,
                                        &deltaspace_id,
                                        passthrough_stored_name(&obj_key.filename, &fresh_meta),
                                        start,
                                        end,
                                    )
                                    .await?;
                                return if content_length == 0 {
                                    Ok(None)
                                } else {
                                    Ok(Some((stream, content_length, fresh_meta)))
                                };
                            }
                            StorageInfo::Reference { .. } | StorageInfo::Delta { .. } => {
                                return Ok(None);
                            }
                        }
                    }
                    Err(e) => return Err(e.into()),
                };

                if content_length == 0 {
                    // Backend returned full stream (default impl), signal caller
                    // to fall back to the buffered slicing path.
                    return Ok(None);
                }

                debug!(
                    "Streaming passthrough range for {} (bytes {}-{}, {} bytes)",
                    obj_key.full_key(),
                    start,
                    end,
                    content_length
                );
                Ok(Some((stream, content_length, metadata)))
            }
            StorageInfo::Delta { .. } if metadata.file_size > self.spool_threshold() => {
                // Large delta range: reconstruct once to a spool file (verified),
                // then seek + stream just the requested bytes (blocker 6) — no
                // full-object re-buffer.
                let (stream, content_length) = self
                    .retrieve_delta_range_spooled(
                        bucket,
                        &deltaspace_id,
                        &obj_key,
                        &metadata,
                        start,
                        end,
                    )
                    .await?;
                Ok(Some((stream, content_length, metadata)))
            }
            StorageInfo::Reference { .. } | StorageInfo::Delta { .. } => {
                // Small delta/reference: signal caller to use the buffered slice
                // path (cheap — the whole object fits comfortably in RAM).
                Ok(None)
            }
        }
    }

    /// Fetch and reconstruct a reference or delta object, with checksum verification.
    /// Returns `(data, cache_hit)` where `cache_hit` is `Some(bool)` for delta objects.
    async fn retrieve_buffered(
        &self,
        bucket: &str,
        deltaspace_id: &str,
        obj_key: &ObjectKey,
        metadata: &FileMetadata,
    ) -> Result<(Vec<u8>, Option<bool>), EngineError> {
        let (data, cache_hit) = match &metadata.storage_info {
            StorageInfo::Reference { .. } => (
                self.storage.get_reference(bucket, deltaspace_id).await?,
                None,
            ),
            StorageInfo::Delta { ref_sha256, .. } => {
                // Fetch reference and delta in parallel — saves one S3 round-trip.
                // The reference is sibling to the delta: same parent directory + "reference.bin".
                let (ref_result, delta_result) = tokio::join!(
                    self.get_reference_cached(bucket, deltaspace_id, ref_sha256),
                    self.storage
                        .get_delta(bucket, deltaspace_id, &obj_key.filename)
                );

                // Fallback: if get_reference fails (uses internal deltaspace key),
                // try get_passthrough which goes through the full routing+aliasing pipeline.
                // This covers the case where reference.bin was uploaded via the Python CLI
                // to the real S3 path, but the proxy's internal deltaspace key differs
                // from the aliased path that the routing layer resolves.
                let (reference, cache_hit) = match ref_result {
                    Ok(r) => r,
                    Err(EngineError::MissingReference(_)) => {
                        tracing::info!(
                            "Reference not found via internal key — trying passthrough fallback: {}/reference.bin",
                            deltaspace_id
                        );
                        match self
                            .storage
                            .get_passthrough(bucket, deltaspace_id, "reference.bin")
                            .await
                        {
                            Ok(data) => {
                                tracing::info!(
                                    "Reference passthrough fallback succeeded ({} bytes)",
                                    data.len()
                                );
                                let sha = hex::encode(Sha256::digest(&data));
                                let bytes = bytes::Bytes::from(data);
                                let cache_key = self.cache_key(bucket, deltaspace_id);
                                self.cache.put(&cache_key, bytes.clone(), &sha);
                                (bytes, false)
                            }
                            Err(_) => {
                                return Err(EngineError::MissingReference(format!(
                                    "{} (reference.bin not found via any method)",
                                    deltaspace_id
                                )));
                            }
                        }
                    }
                    Err(e) => return Err(e),
                };
                let delta = delta_result?;

                // Decompression-bomb guard: reject if the object's DECLARED
                // reconstruction size exceeds max. Using ref.len()+delta.len()
                // here was WRONG — that is not a lower bound for the output
                // (a small delta against a large reference can exceed it), so
                // it falsely rejected legitimately-stored objects on every GET,
                // making them permanently unreadable. The true reconstruction
                // size is metadata.file_size (already ≤ max at store time), so
                // a correctly-stored object never trips this; a tampered
                // metadata claiming an oversized output still does.
                if metadata.file_size > self.max_object_size {
                    return Err(EngineError::TooLarge {
                        size: metadata.file_size,
                        max: self.max_object_size,
                    });
                }

                // The source file first (spool before codec slot, as on the
                // streaming GET). A GET holds no spool and no lock here, so it
                // may wait for budget, under the spool acquire timeout.
                let source_file = self.spool_acquire(reference.len() as u64).await?;
                // Wait up to 60s for a codec slot (GET should queue, not fail fast)
                let _codec_permit = self
                    .acquire_codec_timeout(std::time::Duration::from_secs(60))
                    .await?;
                let ref_clone = reference.clone();
                let codec = self.codec.clone();
                let decode_start = Instant::now();
                let result = tokio::task::spawn_blocking(move || {
                    codec.decode_spooled(&source_file, &ref_clone, &delta)
                })
                .await
                .map_err(|e| {
                    tracing::error!("Delta decode task panicked: {}", e);
                    EngineError::Storage(StorageError::Other(format!("codec task panicked: {}", e)))
                })??;
                let decode_secs = decode_start.elapsed().as_secs_f64();
                drop(_codec_permit);
                self.with_metrics(|m| m.delta_decode_duration_seconds.observe(decode_secs));
                (result, Some(cache_hit))
            }
            StorageInfo::Passthrough => {
                // Callers route Passthrough to the streaming path in retrieve_stream().
                // This arm is kept as a safe fallback rather than panicking.
                debug_assert!(
                    false,
                    "retrieve_buffered called for Passthrough — should use streaming path"
                );
                (
                    self.storage
                        .get_passthrough(bucket, deltaspace_id, &obj_key.filename)
                        .await?,
                    None,
                )
            }
        };

        // Always verify checksum on read — detect corruption or delta reconstruction bugs
        let actual_sha256 = hex::encode(Sha256::digest(&data));
        if actual_sha256 != metadata.file_sha256 {
            // Evict the cached reference only when nothing verified it for
            // this delta (`reference_suspect_after_mismatch`): one bad object
            // must not make every GET of the deltaspace download it again.
            let cache_key = self.cache_key(bucket, deltaspace_id);
            let evict = super::metadata::reference_suspect_after_mismatch(metadata);
            if evict {
                self.cache.invalidate(&cache_key);
            }
            warn!(
                "Checksum mismatch for {} (cached reference of {} evicted: {}): expected {}, got {}",
                obj_key.full_key(),
                cache_key,
                evict,
                metadata.file_sha256,
                actual_sha256
            );
            return Err(EngineError::ChecksumMismatch {
                key: obj_key.full_key(),
                expected: metadata.file_sha256.clone(),
                actual: actual_sha256,
            });
        }

        Ok((data, cache_hit))
    }

    /// Try to stream an unmanaged object (no DG metadata) with best-effort metadata.
    /// First tries `get_passthrough_metadata` for proper size/etag, then falls back
    /// to streaming with minimal metadata if the metadata lookup fails.
    async fn try_unmanaged_passthrough(
        &self,
        bucket: &str,
        deltaspace_id: &str,
        obj_key: &ObjectKey,
    ) -> Result<RetrieveResponse, EngineError> {
        // Try metadata first (same source as HEAD) for consistent Content-Length/ETag
        let meta = match self
            .storage
            .get_passthrough_metadata(bucket, deltaspace_id, &obj_key.filename)
            .await
        {
            Ok(m) => m,
            Err(StorageError::NotFound(_)) => {
                // No metadata at all — use minimal fallback
                FileMetadata::new_passthrough(
                    obj_key.filename.clone(),
                    String::new(),
                    String::new(),
                    0,
                    None,
                )
            }
            Err(e) => return Err(EngineError::Storage(e)),
        };

        // Inject warning into metadata for UI display if this looks like a delta artifact
        let mut meta = meta;
        if obj_key.filename.ends_with(".delta") || obj_key.filename == "reference.bin" {
            meta.user_metadata.insert(
                "dg-warning".to_string(),
                "Missing DG metadata — delta features disabled. Re-copy with --metadata flag."
                    .to_string(),
            );
        }

        // Stream the object
        match self
            .storage
            .get_passthrough_stream(bucket, deltaspace_id, &obj_key.filename)
            .await
        {
            Ok(stream) => Ok(RetrieveResponse::Streamed {
                stream,
                metadata: meta,
                cache_hit: None,
            }),
            Err(StorageError::NotFound(_)) => Err(EngineError::NotFound(obj_key.full_key())),
            Err(e) => Err(EngineError::Storage(e)),
        }
    }

    /// PURE: do two metadata snapshots describe the same object generation?
    /// Size must match; then the strongest hash both sides carry decides
    /// (sha256 > md5 > multipart_etag). Same size with NO shared hash is
    /// conservatively DIFFERENT — a pinned copy must never guess.
    pub(crate) fn same_generation(expected: &FileMetadata, actual: &FileMetadata) -> bool {
        if expected.file_size != actual.file_size {
            return false;
        }
        let pairs = [
            (&expected.file_sha256, &actual.file_sha256),
            (&expected.md5, &actual.md5),
        ];
        for (e, a) in pairs {
            if !e.is_empty() && !a.is_empty() {
                return e == a;
            }
        }
        match (&expected.multipart_etag, &actual.multipart_etag) {
            (Some(e), Some(a)) if !e.is_empty() && !a.is_empty() => e == a,
            _ => false,
        }
    }
}

use crate::deltaglider::range_spool::RangeSpoolKey;

/// The reference file a large delta GET decodes against.
enum ReferenceFile {
    /// A verified download shared through `range_spools`.
    Shared(Arc<crate::deltaglider::spool::Spool>),
    /// This GET's own download.
    Own(crate::deltaglider::spool::Spool),
}

impl ReferenceFile {
    fn path(&self) -> &std::path::Path {
        match self {
            ReferenceFile::Shared(s) => s.path(),
            ReferenceFile::Own(s) => s.path(),
        }
    }
}

/// Why a shared reference download did not happen.
enum SharedReferenceError {
    /// No spool budget beside the GET's own: it downloads its own copy.
    NoRoom,
    Engine(EngineError),
}

impl From<EngineError> for SharedReferenceError {
    fn from(e: EngineError) -> Self {
        SharedReferenceError::Engine(e)
    }
}

/// The stored name of the passthrough object that answers a request for
/// `filename`. The object lives at the request key. The one exception is a
/// `.delta` file copied in without its DG metadata (rclone between
/// deployments): it reads as passthrough and lives at `filename.delta`.
/// Never any other name from the metadata: the S3 SDK trims header values
/// and a backend copy keeps the source's `dg-original-name`, so a GET that
/// trusted it served the bytes of another key.
fn passthrough_stored_name<'a>(filename: &'a str, metadata: &'a FileMetadata) -> &'a str {
    if metadata.original_name.strip_suffix(".delta") == Some(filename) {
        &metadata.original_name
    } else {
        filename
    }
}

#[cfg(test)]
mod passthrough_name_tests {
    use super::*;

    fn meta(original_name: &str) -> FileMetadata {
        FileMetadata::fallback(
            original_name.to_string(),
            1,
            String::new(),
            chrono::Utc::now(),
            None,
            StorageInfo::Passthrough,
        )
    }

    #[test]
    fn the_request_key_names_the_stored_object() {
        assert_eq!(passthrough_stored_name("a.csv", &meta("a.csv")), "a.csv");
        // Trimmed by the SDK, copied from another key, or encoded: the
        // request key wins.
        assert_eq!(passthrough_stored_name(" a.csv", &meta("a.csv")), " a.csv");
        assert_eq!(passthrough_stored_name("b.csv", &meta("a.csv")), "b.csv");
        assert_eq!(
            passthrough_stored_name("é.csv", &meta("=?UTF-8?Q?=C3=A9.csv?=")),
            "é.csv"
        );
        // A metadata-less `.delta` copy is stored under its suffix.
        assert_eq!(
            passthrough_stored_name("x.zip", &meta("x.zip.delta")),
            "x.zip.delta"
        );
    }
}

#[cfg(test)]
mod generation_tests {
    use super::*;
    use crate::types::FileMetadata;

    fn meta(size: u64, sha: &str, md5: &str, metag: Option<&str>) -> FileMetadata {
        let mut m =
            FileMetadata::new_passthrough("f.bin".into(), sha.into(), md5.into(), size, None);
        m.multipart_etag = metag.map(|s| s.to_string());
        m
    }

    #[test]
    fn same_generation_truth_table() {
        type E = DeltaGliderEngine<Box<crate::storage::DynStorageBackend<'static>>>;
        // Identical sha → same.
        assert!(E::same_generation(
            &meta(10, "aa", "m1", None),
            &meta(10, "aa", "m2", None)
        ));
        // Size differs → different, regardless of hashes.
        assert!(!E::same_generation(
            &meta(10, "aa", "m1", None),
            &meta(11, "aa", "m1", None)
        ));
        // Sha differs → different (md5 agreement is NOT consulted).
        assert!(!E::same_generation(
            &meta(10, "aa", "m1", None),
            &meta(10, "bb", "m1", None)
        ));
        // No sha on one side → falls to md5.
        assert!(E::same_generation(
            &meta(10, "", "m1", None),
            &meta(10, "bb", "m1", None)
        ));
        assert!(!E::same_generation(
            &meta(10, "", "m1", None),
            &meta(10, "", "m2", None)
        ));
        // Only multipart etags shared → compared.
        assert!(E::same_generation(
            &meta(10, "", "", Some("e-3")),
            &meta(10, "", "", Some("e-3"))
        ));
        // Same size, NO shared hash at all → conservatively different.
        assert!(!E::same_generation(
            &meta(10, "", "", None),
            &meta(10, "", "", None)
        ));
    }
}

/// What a cold read of deltas costs in reference downloads. K concurrent
/// cold GETs of one deltaspace each downloaded its reference.bin (and a
/// large delta GET downloaded it on every request), so 20 CI runners that
/// fetched one 500 MB artifact pulled 10 GB of reference from the backend.
#[cfg(test)]
mod reference_download_tests {
    use super::*;
    use crate::config::Config;
    use crate::storage::{DynStorageBackend, FakeS3};
    use futures::TryStreamExt;

    /// A writer engine and a cold reader engine (`config`, `metrics`) on one
    /// fake S3 with bucket `b`.
    async fn writer_and_reader(
        config: &Config,
        metrics: Option<Arc<crate::metrics::Metrics>>,
    ) -> (DynEngine, DynEngine, Arc<FakeS3>, String) {
        let (endpoint, fake) = crate::storage::fake_s3::start().await;
        let engine = |config: &Config, metrics| {
            let s3 = crate::storage::s3_test_support::for_test_endpoint(&endpoint);
            DeltaGliderEngine::new_with_backend(
                Arc::new(DynStorageBackend::new_box(s3)),
                config,
                metrics,
            )
        };
        let writer = engine(&Config::default(), None);
        writer.create_bucket("b").await.unwrap();
        let reader = engine(config, metrics);
        (writer, reader, fake, endpoint)
    }

    fn count(fake: &FakeS3, request: &str) -> usize {
        fake.requests()
            .iter()
            .filter(|r| r.as_str() == request || r.starts_with(&format!("{request}?")))
            .count()
    }

    async fn read_all(engine: &DynEngine, key: &str) -> Result<Vec<u8>, EngineError> {
        match engine.retrieve_stream("b", key).await? {
            RetrieveResponse::Buffered { data, .. } => Ok(data),
            RetrieveResponse::Streamed { stream, .. } => {
                let chunks: Vec<Bytes> = stream.try_collect().await?;
                Ok(chunks.concat())
            }
        }
    }

    /// Eight concurrent cold GETs of eight small deltas of one deltaspace
    /// download its reference once (it was eight times).
    #[tokio::test]
    async fn concurrent_cold_gets_download_the_reference_once() {
        let (writer, reader, fake, _) = writer_and_reader(&Config::default(), None).await;
        let keys = store_deltas(&writer, "run", 8).await;
        fake.set_delay_ms("GET", 50);
        fake.clear();
        let got = futures::future::join_all(keys.iter().map(|k| read_all(&reader, k))).await;
        for (k, r) in keys.iter().zip(&got) {
            assert_eq!(
                r.as_ref().unwrap(),
                &writer.retrieve("b", k).await.unwrap().0
            );
        }
        assert_eq!(count(&fake, "GET /b/run/reference.bin"), 1);
    }

    /// A delta GET above the spool threshold reads a verified reference that
    /// a GET before it downloaded, instead of downloading it again; and
    /// concurrent GETs of one large object decode it once.
    #[tokio::test]
    async fn large_delta_gets_reuse_one_reference_download() {
        // The spool threshold is capped at max_object_size: 64 KiB deltas
        // take the spool path.
        let config = Config {
            max_object_size: 32 * 1024,
            ..Config::default()
        };
        let metrics = Arc::new(crate::metrics::Metrics::new());
        let (writer, reader, fake, _) = writer_and_reader(&config, Some(metrics.clone())).await;
        let keys = store_deltas(&writer, "run", 5).await;
        let mut want = HashMap::new();
        for k in &keys {
            want.insert(k.clone(), writer.retrieve("b", k).await.unwrap().0);
        }
        fake.set_delay_ms("GET", 50);
        fake.clear();
        // Four objects, each twice, all at once.
        let first = &keys[..4];
        let reader = &reader;
        let got = futures::future::join_all(
            first
                .iter()
                .chain(first.iter())
                .map(|k| async move { (k, read_all(reader, k).await) }),
        )
        .await;
        for (k, r) in got {
            assert_eq!(r.unwrap(), want[k], "{k}");
        }
        // A later GET of the fifth object reuses the cached reference.
        assert_eq!(read_all(reader, &keys[4]).await.unwrap(), want[&keys[4]]);
        assert_eq!(count(&fake, "GET /b/run/reference.bin"), 1);
        assert!(count(&fake, "HEAD /b/run/reference.bin") <= 1);
        assert_eq!(
            metrics.delta_decode_duration_seconds.get_sample_count(),
            5,
            "eight concurrent GETs of four objects decode each once"
        );
    }

    /// A delta whose reconstruction fails its checksum does not evict the
    /// reference that the other deltas of the folder decode against: three
    /// GETs of the bad object and one of a good one download it once (it was
    /// four times).
    #[tokio::test]
    async fn a_bad_object_does_not_evict_a_good_reference() {
        let (writer, reader, fake, endpoint) = writer_and_reader(&Config::default(), None).await;
        let keys = store_deltas(&writer, "run", 2).await;
        // The stored delta of object 0 now holds the bytes of object 1: it
        // decodes, and the result fails object 0's checksum.
        let http = reqwest::Client::new();
        let url = |k: &str| format!("{endpoint}/b/{k}.delta");
        let bad = http.get(url(&keys[0])).send().await.unwrap();
        let headers = bad.headers().clone();
        let other = http
            .get(url(&keys[1]))
            .send()
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        let mut put = http.put(url(&keys[0])).body(other);
        for (name, value) in headers.iter() {
            if name.as_str().starts_with("x-amz-meta-") {
                put = put.header(name, value);
            }
        }
        put.send().await.unwrap();
        fake.clear();
        for _ in 0..3 {
            let err = read_all(&reader, &keys[0]).await.unwrap_err();
            assert!(matches!(err, EngineError::ChecksumMismatch { .. }), "{err}");
        }
        read_all(&reader, &keys[1]).await.unwrap();
        assert_eq!(count(&fake, "GET /b/run/reference.bin"), 1);
    }
}

/// storage-11: range reads of a large delta object share one verified
/// reconstruction.
#[cfg(test)]
mod range_spool_tests {
    use super::*;
    use crate::config::Config;
    use crate::storage::FilesystemBackend;
    use futures::TryStreamExt;
    use std::collections::HashMap;

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

    /// Eight concurrent ranges and four later ones decode the object once.
    #[tokio::test]
    async fn many_ranges_of_a_large_delta_decode_it_once() {
        let tmp = tempfile::tempdir().unwrap();
        let backend = Arc::new(
            FilesystemBackend::new(tmp.path().to_path_buf())
                .await
                .unwrap(),
        );
        backend.create_bucket("b").await.unwrap();
        let (v1, v2) = versions();
        let writer = DeltaGliderEngine::new_with_backend(backend.clone(), &Config::default(), None);
        for (k, body) in [("v/a.zip", &v1), ("v/b.zip", &v2)] {
            writer
                .store("b", k, body, None, HashMap::new())
                .await
                .unwrap();
        }
        assert!(writer.head("b", "v/b.zip").await.unwrap().is_delta());

        // Objects above the spool threshold (here max_object_size, which
        // caps it) take the spooled range path.
        let config = Config {
            max_object_size: 64 * 1024,
            ..Config::default()
        };
        let metrics = Arc::new(crate::metrics::Metrics::new());
        let reader = DeltaGliderEngine::new_with_backend(backend, &config, Some(metrics.clone()));
        let range = |start: u64| {
            let reader = &reader;
            async move {
                let (stream, len, _) = reader
                    .retrieve_stream_range("b", "v/b.zip", start, start + 999, None)
                    .await
                    .unwrap()
                    .expect("spooled range");
                let got: Vec<Bytes> = stream.try_collect().await.unwrap();
                (start, len, got.concat())
            }
        };
        let concurrent = futures::future::join_all((0..8).map(|i| range(i * 20_000))).await;
        let mut all = concurrent;
        for i in 8..12 {
            all.push(range(i * 15_000).await);
        }
        for (start, len, got) in all {
            let s = start as usize;
            assert_eq!(len, 1000);
            assert_eq!(got, v2[s..s + 1000], "range at {start}");
        }
        assert_eq!(
            metrics.delta_decode_duration_seconds.get_sample_count(),
            1,
            "twelve ranges, one decode"
        );
    }
}
