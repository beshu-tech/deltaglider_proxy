// SPDX-License-Identifier: BUSL-1.1

//! Retrieve pipeline — delta reconstruction, streaming, and range requests.

use super::*;
use crate::storage::{ObjectVariant, StorageBackend};
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
        // Boxed: the GET future is deep enough to hit the type recursion limit.
        self.retry_missing_during_writes(bucket, key, || {
            Box::pin(self.retrieve_stream_once(bucket, key))
        })
        .await
    }

    async fn retrieve_stream_once(
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
            .retrieve_with_metadata(bucket, &deltaspace_id, &obj_key, metadata.clone(), None)
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
                        self.retrieve_with_metadata(bucket, &deltaspace_id, &obj_key, m, None)
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

    /// A plain GET (no range, no conditional headers, no `partNumber`): the
    /// object's metadata and its bytes from ONE round trip, and both from
    /// one response, so the ETag and the body describe one version.
    ///
    /// - Metadata cache warm: open the cached form only (1 request). Its
    ///   GET response refreshes the cache; a 404 (the form changed) falls
    ///   through to the cold path.
    /// - Cold: open the form that the key's file type and the bucket's
    ///   compression setting make likely, and in parallel HEAD the other
    ///   form. Newest-wins decides as in `resolve_stored_forms`. A wrong
    ///   guess (or a losing form) drops the opened stream unread and opens
    ///   the winner, whose GET gives the metadata served with its bytes:
    ///   the cost of the HEAD-first path.
    /// - Opened metadata that names another layout (a `.delta` copied in
    ///   without its DG metadata, a passthrough object whose metadata says
    ///   delta), a folder marker: the HEAD-first path.
    /// - A delta above the spool threshold drops its opened body unread
    ///   (bounded memory): the shared fill reads the delta after its spool
    ///   reservation, and the SHA-256 check against the opened metadata
    ///   refuses a body of another version.
    ///
    /// The conditional and ranged reads keep HEAD-first, so a 304/412/416
    /// never opens a body.
    #[instrument(skip(self))]
    pub async fn retrieve_stream_fresh(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<RetrieveResponse, EngineError> {
        self.retry_missing_during_writes(bucket, key, || {
            Box::pin(self.retrieve_stream_fresh_once(bucket, key))
        })
        .await
    }

    async fn retrieve_stream_fresh_once(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<RetrieveResponse, EngineError> {
        let (obj_key, deltaspace_id) = self.validated_key(bucket, key)?;
        if obj_key.is_directory_marker() {
            return self.retrieve_stream_headed(bucket, key).await;
        }
        let filename = obj_key.filename.as_str();
        if let Some(cached) = self.metadata_cache.get(bucket, key) {
            if let Some(form) = opened_form(filename, &cached) {
                match self
                    .storage
                    .open_object(bucket, &deltaspace_id, stored_object(form, filename))
                    .await
                {
                    Ok(read) => {
                        let (stream, meta) = (read.stream, read.metadata);
                        return self
                            .serve_opened(bucket, key, &deltaspace_id, &obj_key, form, stream, meta)
                            .await;
                    }
                    Err(StorageError::NotFound(_)) => self.metadata_cache.invalidate(bucket, key),
                    Err(e) => return Err(e.into()),
                }
            }
        }
        let guess = if self.bucket_policies.compression_enabled(bucket)
            && self.bucket_policies.is_delta_eligible(bucket, filename)
        {
            ObjectVariant::Delta
        } else {
            ObjectVariant::Passthrough
        };
        let other_form = match guess {
            ObjectVariant::Delta => ObjectVariant::Passthrough,
            ObjectVariant::Passthrough => ObjectVariant::Delta,
        };
        let (opened, other) = tokio::join!(
            self.storage
                .open_object(bucket, &deltaspace_id, stored_object(guess, filename)),
            async {
                let stall = crate::config::test_seams::test_seams().read_other_form_stall_ms;
                if stall > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(stall)).await;
                }
                self.stored_form_metadata(bucket, &deltaspace_id, filename, other_form)
                    .await
            },
        );
        let opened = match opened {
            Ok(opened) => Some(opened),
            Err(StorageError::NotFound(_)) => None,
            Err(e) => return Err(e.into()),
        };
        let other = other?;
        let opened_wins = match (&opened, &other) {
            (None, _) => false,
            (Some(_), None) => true,
            (Some(read), Some(x)) => match guess {
                ObjectVariant::Delta => super::metadata::delta_is_newest(&read.metadata, x),
                ObjectVariant::Passthrough => !super::metadata::delta_is_newest(x, &read.metadata),
            },
        };
        if let (true, Some(read)) = (opened_wins, opened) {
            return self
                .serve_opened(
                    bucket,
                    key,
                    &deltaspace_id,
                    &obj_key,
                    guess,
                    read.stream,
                    read.metadata,
                )
                .await;
        }
        // The other form wins (or the guess is absent): the opened stream,
        // if any, dropped unread above. Open the other form: its GET
        // carries the metadata that is served with its bytes, so a write
        // that replaced it after the HEAD cannot pair the HEAD's ETag
        // with the new bytes.
        if other.is_none() {
            return Err(EngineError::NotFound(obj_key.full_key()));
        }
        match self
            .storage
            .open_object(bucket, &deltaspace_id, stored_object(other_form, filename))
            .await
        {
            Ok(read) => {
                let (stream, meta) = (read.stream, read.metadata);
                self.serve_opened(
                    bucket,
                    key,
                    &deltaspace_id,
                    &obj_key,
                    other_form,
                    stream,
                    meta,
                )
                .await
            }
            // Gone since the HEAD: the HEAD-first read decides again.
            Err(StorageError::NotFound(_)) => self.retrieve_stream_headed(bucket, key).await,
            Err(e) => Err(e.into()),
        }
    }

    /// A ranged GET (no If-Range, no conditional headers) of a passthrough
    /// object in one round trip: the ranged read of the plain object, whose
    /// response carries its metadata, in parallel with the HEAD of the
    /// `.delta` form. `Ok(None)`: the HEAD-first path answers instead,
    /// because the plain object is absent, a `.delta` form is newer (a
    /// crash leftover), its metadata names another layout, or the backend
    /// refused the range. The backend's refusal does not carry the object
    /// length, and the HEAD-first path answers the 416 with
    /// `Content-Range: bytes */<len>` (one HEAD, on the error path only).
    /// The ranged body is then dropped unread.
    #[instrument(skip(self))]
    #[allow(clippy::type_complexity)]
    pub async fn retrieve_range_fresh(
        &self,
        bucket: &str,
        key: &str,
        start: u64,
        end: u64,
    ) -> Result<
        Option<(
            BoxStream<'static, Result<Bytes, StorageError>>,
            u64,
            FileMetadata,
        )>,
        EngineError,
    > {
        let (obj_key, deltaspace_id) = self.validated_key(bucket, key)?;
        if obj_key.is_directory_marker() {
            return Ok(None);
        }
        let filename = obj_key.filename.as_str();
        let (ranged, delta) = tokio::join!(
            self.storage
                .open_passthrough_range(bucket, &deltaspace_id, filename, start, end),
            self.stored_form_metadata(bucket, &deltaspace_id, filename, ObjectVariant::Delta),
        );
        let delta = delta?;
        match ranged {
            Ok(crate::storage::RangeRead {
                stream,
                len,
                metadata: meta,
                ..
            }) => {
                let passthrough_wins = delta
                    .as_ref()
                    .is_none_or(|d| !super::metadata::delta_is_newest(d, &meta));
                if !passthrough_wins
                    || opened_form(filename, &meta) != Some(ObjectVariant::Passthrough)
                {
                    return Ok(None);
                }
                self.metadata_cache.insert(bucket, key, meta.clone());
                Ok(Some((stream, len, meta)))
            }
            Err(StorageError::NotFound(_)) => Ok(None),
            Err(StorageError::InvalidRange(_)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Serve the form that `retrieve_stream_fresh` opened, with the
    /// metadata from the same response.
    #[allow(clippy::too_many_arguments)]
    async fn serve_opened(
        &self,
        bucket: &str,
        key: &str,
        deltaspace_id: &str,
        obj_key: &super::ObjectKey,
        form: ObjectVariant,
        stream: crate::storage::ByteStream,
        meta: FileMetadata,
    ) -> Result<RetrieveResponse, EngineError> {
        if opened_form(&obj_key.filename, &meta) != Some(form) {
            // The stored metadata names another layout: read it the
            // HEAD-first way, which knows every layout.
            drop(stream);
            return self.retrieve_stream_headed(bucket, key).await;
        }
        info!(
            "Retrieving {}/{} (stored as {}, one read)",
            bucket,
            key,
            meta.storage_info.label()
        );
        self.metadata_cache.insert(bucket, key, meta.clone());
        match form {
            ObjectVariant::Passthrough => Ok(RetrieveResponse::Streamed {
                stream,
                metadata: meta,
                cache_hit: None,
            }),
            ObjectVariant::Delta => {
                // A large delta (spool path) drops its opened body unread:
                // collecting it here would hold one delta in RAM per
                // concurrent GET before the spool and codec admission, and
                // a waiter of the shared fill would throw its copy away.
                // The fill reads the delta after it reserved its spool, or
                // the verified reconstruction is cached already.
                let prefetched = if meta.file_size > self.spool_threshold() {
                    drop(stream);
                    None
                } else {
                    use futures::TryStreamExt;
                    let chunks: Vec<Bytes> = stream.try_collect().await?;
                    Some(chunks.concat())
                };
                self.retrieve_with_metadata(bucket, deltaspace_id, obj_key, meta, prefetched)
                    .await
            }
        }
    }

    /// The HEAD-first read: fresh metadata of both forms, then the bytes.
    async fn retrieve_stream_headed(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<RetrieveResponse, EngineError> {
        // `head` refreshes the cache that `retrieve_stream` reads.
        self.head(bucket, key).await?;
        self.retrieve_stream(bucket, key).await
    }

    /// Inner retrieve that uses pre-resolved metadata. `prefetched`: the
    /// delta body, when the caller already read it with its metadata
    /// (`retrieve_stream_fresh`), so it is not read again.
    async fn retrieve_with_metadata(
        &self,
        bucket: &str,
        deltaspace_id: &str,
        obj_key: &super::ObjectKey,
        metadata: FileMetadata,
        prefetched: Option<Vec<u8>>,
    ) -> Result<RetrieveResponse, EngineError> {
        match &metadata.storage_info {
            StorageInfo::Passthrough => {
                // The metadata served is the one of the response that
                // carries the bytes, not the earlier read's: a write between
                // the two would pair one version's ETag with another's body.
                let stored_name = passthrough_stored_name(&obj_key.filename, &metadata);
                let crate::storage::ObjectRead {
                    stream,
                    metadata: served,
                    ..
                } = self
                    .storage
                    .open_object(
                        bucket,
                        deltaspace_id,
                        crate::storage::StoredObject::Passthrough(stored_name),
                    )
                    .await?;
                if !matches!(served.storage_info, StorageInfo::Passthrough) {
                    return Err(changed_while_read(bucket, obj_key));
                }
                debug!("Streaming passthrough file for {}", obj_key.full_key());
                Ok(RetrieveResponse::Streamed {
                    stream,
                    metadata: served,
                    cache_hit: None,
                })
            }
            StorageInfo::Delta { .. } if metadata.file_size > self.spool_threshold() => {
                // Large delta: reconstruct to a spool file (bounded memory) and
                // stream the file to the client. Integrity is verified BEFORE the
                // first byte ships (see retrieve_delta_spooled).
                self.retrieve_delta_spooled(bucket, deltaspace_id, obj_key, metadata, prefetched)
                    .await
            }
            StorageInfo::Reference { .. } | StorageInfo::Delta { .. } => {
                let (data, cache_hit) = self
                    .retrieve_buffered(bucket, deltaspace_id, obj_key, &metadata, prefetched)
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
        prefetched: Option<Vec<u8>>,
    ) -> Result<RetrieveResponse, EngineError> {
        let key = RangeSpoolKey::object(bucket, obj_key.full_key(), metadata.file_sha256.clone());
        let out_spool = self
            .range_spools
            .get_or_fill(key, || {
                self.reconstruct_delta_to_spool(
                    bucket,
                    deltaspace_id,
                    obj_key,
                    &metadata,
                    prefetched,
                )
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
        prefetched: Option<Vec<u8>>,
    ) -> Result<crate::deltaglider::spool::Spool, EngineError> {
        use std::io::Write;

        let (ref_file, out_spool) = self
            .reference_file_and_out_spool(bucket, deltaspace_id, metadata)
            .await?;

        // Fetch the delta (small — it's a delta), unless the caller has it.
        let delta = match prefetched {
            Some(delta) => delta,
            None => {
                self.storage
                    .get_delta(bucket, deltaspace_id, &obj_key.filename)
                    .await?
            }
        };

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
    /// deltaspace read the file instead of downloading the reference again. The output spool is reserved first,
    /// so a GET waits for budget holding nothing; the download then takes
    /// the reference's spool beside it, without waiting (on no room, the GET
    /// downloads its own copy as before).
    ///
    /// Without a `ref_sha256` (or with the cache off) nothing can verify a
    /// shared copy: the GET downloads its own. It waits for budget only
    /// while it holds nothing but its output spool reservation, or for ONE
    /// reservation of both spools (two concurrent GETs never hold one while
    /// waiting for the other).
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
                    self.download_verified_reference(bucket, deltaspace_id, ref_sha256, &out)
                })
                .await;
            match shared {
                Ok(file) => return Ok((ReferenceFile::Shared(file), out)),
                Err(SharedReferenceError::NoRoom) => drop(out),
                Err(SharedReferenceError::Engine(e)) => return Err(e),
            }
        }
        // This GET's own copy. The reference can be far larger than the
        // object: its spool is reserved at its own size, which the GET of
        // the reference answers (no HEAD). The output spool first (a GET
        // that waits for budget holds nothing), then the reference's beside
        // it without waiting, so the open response is never held while
        // waiting. No room beside it: drop both, take ONE reservation for
        // the pair at the known size, and read the reference again.
        let out = self.spool_acquire(metadata.file_size).await?;
        let source = self
            .storage
            .open_reference_source(bucket, deltaspace_id)
            .await?;
        match self.spool.acquire_beside(Some(&out), source.size).await {
            Ok(ref_spool) => {
                source.write_to(ref_spool.path()).await?;
                Ok((ReferenceFile::Own(ref_spool), out))
            }
            Err(e) if e.kind() == crate::deltaglider::spool::CONTENDED => {
                let ref_size = source.size;
                drop(source);
                drop(out);
                let (ref_spool, out_spool) = self
                    .spool_acquire_pair(ref_size, metadata.file_size)
                    .await?;
                // Materialise the reference as a seekable file WITHOUT
                // heap-loading it (filesystem hardlink / S3 stream-to-file).
                self.storage
                    .get_reference_to_file(bucket, deltaspace_id, ref_spool.path())
                    .await?;
                Ok((ReferenceFile::Own(ref_spool), out_spool))
            }
            Err(e) => Err(EngineError::Storage(StorageError::from(e))),
        }
    }

    /// Download the reference to a spool file beside `held` (never waits for
    /// budget; the size comes from the reference's own GET) and check it
    /// against `ref_sha256`. Only a file that passes is shared. The GET
    /// path and the streaming PUT (`encode_spooled`) fill `range_spools`
    /// with it.
    pub(super) async fn download_verified_reference(
        &self,
        bucket: &str,
        deltaspace_id: &str,
        ref_sha256: &str,
        held: &crate::deltaglider::spool::Spool,
    ) -> Result<crate::deltaglider::spool::Spool, SharedReferenceError> {
        let source = self
            .storage
            .open_reference_source(bucket, deltaspace_id)
            .await
            .map_err(EngineError::from)?;
        let spool = match self.spool.acquire_beside(Some(held), source.size).await {
            Ok(spool) => spool,
            Err(e) if e.kind() == crate::deltaglider::spool::CONTENDED => {
                return Err(SharedReferenceError::NoRoom)
            }
            Err(e) => return Err(EngineError::Storage(StorageError::from(e)).into()),
        };
        source
            .write_to(spool.path())
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
    pub(super) async fn retrieve_delta_range_spooled(
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
                self.reconstruct_delta_to_spool(bucket, deltaspace_id, obj_key, metadata, None)
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
                // One read gives the range and the metadata of the version
                // it carries; that metadata is returned (and checked
                // against the pin), never the earlier read's.
                let stored_name = passthrough_stored_name(&obj_key.filename, &metadata);
                let range_result = self
                    .storage
                    .open_passthrough_range(bucket, &deltaspace_id, stored_name, start, end)
                    .await;
                let crate::storage::RangeRead {
                    stream,
                    len: content_length,
                    metadata: served,
                    ..
                } = match range_result {
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
                        if !matches!(fresh_meta.storage_info, StorageInfo::Passthrough) {
                            return Ok(None);
                        }
                        self.storage
                            .open_passthrough_range(
                                bucket,
                                &deltaspace_id,
                                passthrough_stored_name(&obj_key.filename, &fresh_meta),
                                start,
                                end,
                            )
                            .await?
                    }
                    Err(e) => return Err(e.into()),
                };
                if !matches!(served.storage_info, StorageInfo::Passthrough) {
                    return Err(changed_while_read(bucket, &obj_key));
                }
                if let Some(expected) = expected_source {
                    if !Self::same_generation(expected, &served) {
                        return Err(EngineError::Storage(StorageError::PreconditionFailed(
                            format!(
                                "source changed during copy: {}/{} (size {} -> {})",
                                bucket, key, expected.file_size, served.file_size
                            ),
                        )));
                    }
                }
                debug!(
                    "Streaming passthrough range for {} (bytes {}-{}, {} bytes)",
                    obj_key.full_key(),
                    start,
                    end,
                    content_length
                );
                Ok(Some((stream, content_length, served)))
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

    /// P3.6: decode a small delta against the deltaspace's verified shared
    /// reference FILE (`range_spools`, matched on `ref_sha256`), not a new
    /// spool copy of the reference per GET. The file is written once per
    /// cache TTL: from the RAM reference cache when it holds the verified
    /// bytes (no request), else by one verified download. Every decode that
    /// reads it holds a share (`Arc<Spool>`), so an eviction or the TTL
    /// never deletes it under a running decode, and its budget is released
    /// when the last share drops. The output is capped at the object's
    /// size (the decompression-bomb guard); the caller checks its SHA-256.
    ///
    /// `Fallback(delta)`: no shared file (no spool room, or no reference
    /// under the internal key): the caller decodes the buffered way, with
    /// the delta this read already fetched.
    async fn decode_against_shared_reference(
        &self,
        bucket: &str,
        deltaspace_id: &str,
        obj_key: &ObjectKey,
        metadata: &FileMetadata,
        ref_sha256: &str,
        prefetched: Option<Vec<u8>>,
    ) -> Result<SharedDecode, EngineError> {
        let key = RangeSpoolKey::reference(
            self.cache_key(bucket, deltaspace_id),
            ref_sha256.to_string(),
        );
        let filled = std::sync::atomic::AtomicBool::new(false);
        let (shared, delta) = tokio::join!(
            self.range_spools.get_or_fill(key, || {
                filled.store(true, std::sync::atomic::Ordering::Relaxed);
                self.fill_shared_reference(bucket, deltaspace_id, ref_sha256)
            }),
            async {
                match prefetched {
                    Some(delta) => Ok(delta),
                    None => {
                        self.storage
                            .get_delta(bucket, deltaspace_id, &obj_key.filename)
                            .await
                    }
                }
            }
        );
        let delta = delta?;
        let reference = match shared {
            Ok(file) => file,
            Err(SharedReferenceError::NoRoom)
            | Err(SharedReferenceError::Engine(EngineError::MissingReference(_)))
            | Err(SharedReferenceError::Engine(EngineError::Storage(StorageError::NotFound(_)))) => {
                return Ok(SharedDecode::Fallback(delta))
            }
            Err(SharedReferenceError::Engine(e)) => return Err(e),
        };
        let hit = !filled.load(std::sync::atomic::Ordering::Relaxed);
        self.with_metrics(|m| {
            if hit {
                m.cache_hits_total.inc()
            } else {
                m.cache_misses_total.inc()
            }
        });
        // Decompression-bomb guard: the declared size is at most the max
        // (see the buffered branch below).
        if metadata.file_size > self.max_object_size {
            return Err(EngineError::TooLarge {
                size: metadata.file_size,
                max: self.max_object_size,
            });
        }
        let _codec_permit = self
            .acquire_codec_timeout(std::time::Duration::from_secs(60))
            .await?;
        let codec = self.codec.clone();
        let cap = metadata.file_size;
        let decode_start = Instant::now();
        let data = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, EngineError> {
            let mut out = CappedVec {
                buf: Vec::with_capacity(cap as usize),
                cap,
            };
            codec
                .decode_to_writer(reference.path(), &delta[..], &mut out)
                .map_err(EngineError::Codec)?;
            // `reference` (this decode's share of the file) drops here.
            Ok(out.buf)
        })
        .await
        .map_err(|e| {
            EngineError::Storage(StorageError::Other(format!("decode task panicked: {e}")))
        })??;
        drop(_codec_permit);
        self.with_metrics(|m| {
            m.delta_decode_duration_seconds
                .observe(decode_start.elapsed().as_secs_f64())
        });
        Ok(SharedDecode::Decoded(data, hit))
    }

    /// The verified reference file of a deltaspace, for `range_spools`:
    /// the bytes come through the RAM reference cache (`get_reference_cached`:
    /// a hit sends no request, a miss is the one verified download that the
    /// buffered GET sent before, and it fills the RAM cache that PUT
    /// encodes use), and only bytes whose SHA-256 is `ref_sha256` are
    /// written. Never waits for spool budget (`NoRoom`). Bytes that the RAM
    /// cache did not keep, or that do not have `ref_sha256`, are `NoRoom`
    /// too: the caller decodes the buffered way, which checks the result.
    async fn fill_shared_reference(
        &self,
        bucket: &str,
        deltaspace_id: &str,
        ref_sha256: &str,
    ) -> Result<crate::deltaglider::spool::Spool, SharedReferenceError> {
        self.get_reference_cached(bucket, deltaspace_id, ref_sha256)
            .await?;
        let cache_key = self.cache_key(bucket, deltaspace_id);
        let Some(bytes) = self.cache.get_matching(&cache_key, ref_sha256) else {
            return Err(SharedReferenceError::NoRoom);
        };
        let spool = match self.spool.try_acquire(bytes.len() as u64) {
            Ok(spool) => spool,
            Err(e) if e.kind() == crate::deltaglider::spool::CONTENDED => {
                return Err(SharedReferenceError::NoRoom)
            }
            Err(e) => return Err(EngineError::Storage(StorageError::from(e)).into()),
        };
        tokio::fs::write(spool.path(), &bytes)
            .await
            .map_err(|e| EngineError::Storage(StorageError::from(e)))?;
        Ok(spool)
    }

    /// Fetch and reconstruct a reference or delta object, with checksum verification.
    /// Returns `(data, cache_hit)` where `cache_hit` is `Some(bool)` for delta objects.
    async fn retrieve_buffered(
        &self,
        bucket: &str,
        deltaspace_id: &str,
        obj_key: &ObjectKey,
        metadata: &FileMetadata,
        prefetched: Option<Vec<u8>>,
    ) -> Result<(Vec<u8>, Option<bool>), EngineError> {
        let mut prefetched = prefetched;
        let shared = match &metadata.storage_info {
            StorageInfo::Delta { ref_sha256, .. }
                if !ref_sha256.is_empty() && self.range_spools.caches() =>
            {
                match self
                    .decode_against_shared_reference(
                        bucket,
                        deltaspace_id,
                        obj_key,
                        metadata,
                        ref_sha256,
                        prefetched.take(),
                    )
                    .await?
                {
                    SharedDecode::Decoded(data, hit) => Some((data, Some(hit))),
                    SharedDecode::Fallback(delta) => {
                        prefetched = Some(delta);
                        None
                    }
                }
            }
            _ => None,
        };
        let (data, cache_hit) = match (shared, &metadata.storage_info) {
            (Some(done), _) => done,
            (None, StorageInfo::Reference { .. }) => (
                self.storage.get_reference(bucket, deltaspace_id).await?,
                None,
            ),
            (None, StorageInfo::Delta { ref_sha256, .. }) => {
                // Fetch reference and delta in parallel — saves one S3 round-trip.
                // The reference is sibling to the delta: same parent directory + "reference.bin".
                let (ref_result, delta_result) = tokio::join!(
                    self.get_reference_cached(bucket, deltaspace_id, ref_sha256),
                    async {
                        match prefetched {
                            Some(delta) => Ok(delta),
                            None => {
                                self.storage
                                    .get_delta(bucket, deltaspace_id, &obj_key.filename)
                                    .await
                            }
                        }
                    }
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
            (None, StorageInfo::Passthrough) => {
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

    /// Stream an unmanaged object (no DG metadata): one read gives the
    /// bytes and the metadata of the same version (its size and ETag).
    async fn try_unmanaged_passthrough(
        &self,
        bucket: &str,
        deltaspace_id: &str,
        obj_key: &ObjectKey,
    ) -> Result<RetrieveResponse, EngineError> {
        let (stream, mut meta) = match self
            .storage
            .open_object(
                bucket,
                deltaspace_id,
                crate::storage::StoredObject::Passthrough(&obj_key.filename),
            )
            .await
        {
            Ok(read) => (read.stream, read.metadata),
            Err(StorageError::NotFound(_)) => {
                return Err(EngineError::NotFound(obj_key.full_key()))
            }
            Err(e) => return Err(EngineError::Storage(e)),
        };
        // Inject warning into metadata for UI display if this looks like a delta artifact
        if obj_key.filename.ends_with(".delta") || obj_key.filename == "reference.bin" {
            meta.user_metadata.insert(
                "dg-warning".to_string(),
                "Missing DG metadata — delta features disabled. Re-copy with --metadata flag."
                    .to_string(),
            );
        }
        Ok(RetrieveResponse::Streamed {
            stream,
            metadata: meta,
            cache_hit: None,
        })
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
/// What `decode_against_shared_reference` did.
enum SharedDecode {
    /// The reconstruction, and whether the shared file was there already.
    Decoded(Vec<u8>, bool),
    /// No shared file: decode the buffered way with this delta.
    Fallback(Vec<u8>),
}

/// A `Write` sink into memory that fails past `cap` bytes: a correct
/// reconstruction is exactly the object's size (decompression-bomb guard).
struct CappedVec {
    buf: Vec<u8>,
    cap: u64,
}

impl std::io::Write for CappedVec {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if self.buf.len() as u64 + data.len() as u64 > self.cap {
            return Err(std::io::Error::other(format!(
                "delta reconstruction exceeded expected size ({} bytes)",
                self.cap
            )));
        }
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(super) enum SharedReferenceError {
    /// No spool budget beside the GET's own: it downloads its own copy.
    NoRoom,
    Engine(EngineError),
}

impl From<EngineError> for SharedReferenceError {
    fn from(e: EngineError) -> Self {
        SharedReferenceError::Engine(e)
    }
}

/// The stored form that `retrieve_stream_fresh` can serve from one read of
/// it, given its metadata: a delta, or a passthrough stored under the
/// request key. `None`: another layout (read HEAD-first).
fn opened_form(filename: &str, metadata: &FileMetadata) -> Option<ObjectVariant> {
    match &metadata.storage_info {
        StorageInfo::Delta { .. } => Some(ObjectVariant::Delta),
        StorageInfo::Passthrough if passthrough_stored_name(filename, metadata) == filename => {
            Some(ObjectVariant::Passthrough)
        }
        _ => None,
    }
}

/// The 503 SlowDown of a read whose byte read found another version
/// than its metadata read (the text of the storage layer's
/// `object_changed_while_read`, which `crate::storage` does not export).
fn changed_while_read(bucket: &str, obj_key: &ObjectKey) -> EngineError {
    EngineError::Storage(StorageError::Throttled(format!(
        "{bucket}/{} changed while it was read; retry the request",
        obj_key.full_key()
    )))
}

fn stored_object(form: ObjectVariant, filename: &str) -> crate::storage::StoredObject<'_> {
    match form {
        ObjectVariant::Delta => crate::storage::StoredObject::Delta(filename),
        ObjectVariant::Passthrough => crate::storage::StoredObject::Passthrough(filename),
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

    /// P3.6: small-delta GETs decode against the deltaspace's verified
    /// shared reference file (`range_spools`), written to disk once per
    /// cache TTL. Each GET wrote the whole reference to a new spool file
    /// for xdelta3: 6 GETs of deltas of a 64 KiB reference wrote 384 KiB of
    /// source files; now 0, and the one shared file is 64 KiB. The
    /// requests stay: one reference GET for the cold reader.
    #[tokio::test]
    async fn small_delta_gets_decode_against_the_shared_reference_file() {
        let (writer, reader, fake, _) = writer_and_reader(&Config::default(), None).await;
        let keys =
            crate::deltaglider::engine::delete_cost_tests::store_deltas(&writer, "rel", 2).await;
        let mut expected = Vec::new();
        for k in &keys {
            expected.push(writer.retrieve("b", k).await.unwrap().0);
        }
        fake.clear();
        let before = reader.codec.source_bytes_written();
        for _ in 0..3 {
            for (k, want) in keys.iter().zip(&expected) {
                let (got, meta) = reader.retrieve("b", k).await.unwrap();
                assert_eq!(&got, want);
                assert_eq!(meta.storage_info.label(), "delta");
            }
        }
        assert_eq!(
            reader.codec.source_bytes_written() - before,
            0,
            "source bytes written by 6 decodes"
        );
        // Concurrent decodes share the one file too.
        let reads =
            futures::future::join_all((0..8).map(|i| reader.retrieve("b", &keys[i % keys.len()])))
                .await;
        for (i, r) in reads.into_iter().enumerate() {
            assert_eq!(r.unwrap().0, expected[i % keys.len()]);
        }
        assert_eq!(reader.codec.source_bytes_written() - before, 0);
        assert_eq!(
            reader.range_spools.fill_count(),
            1,
            "one shared reference file"
        );
        let ref_gets = fake
            .requests()
            .iter()
            .filter(|r| r.starts_with("GET ") && r.contains("reference.bin"))
            .count();
        assert_eq!(ref_gets, 1, "{:#?}", fake.requests());
    }

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

    /// N concurrent cold GETs of one delta object above the spool threshold
    /// read no delta body before the spool admission: each opened stream is
    /// dropped unread, and the one fill reads the delta after it reserved
    /// its spool. Before, each GET collected the whole delta into RAM before
    /// it joined the fill, so N GETs held N deltas (and the fill read none).
    #[tokio::test]
    async fn cold_gets_of_a_large_delta_read_its_body_only_in_the_fill() {
        let config = Config {
            max_object_size: 32 * 1024,
            ..Config::default()
        };
        let metrics = Arc::new(crate::metrics::Metrics::new());
        let (writer, reader, fake, _) = writer_and_reader(&config, Some(metrics.clone())).await;
        let keys = store_deltas(&writer, "run", 2).await;
        let want = writer.retrieve("b", &keys[1]).await.unwrap().0;
        fake.set_delay_ms("GET", 50);
        fake.clear();
        const N: usize = 6;
        let (reader, key) = (&reader, &keys[1]);
        let got = futures::future::join_all((0..N).map(|_| async move {
            match reader.retrieve_stream_fresh("b", key).await.unwrap() {
                RetrieveResponse::Streamed { stream, .. } => {
                    let chunks: Vec<Bytes> = stream.try_collect().await.unwrap();
                    chunks.concat()
                }
                RetrieveResponse::Buffered { data, .. } => data,
            }
        }))
        .await;
        for g in got {
            assert_eq!(g, want);
        }
        assert_eq!(
            metrics.delta_decode_duration_seconds.get_sample_count(),
            1,
            "one decode"
        );
        // N opens (dropped unread) and the one read of the fill.
        assert_eq!(
            count(&fake, &format!("GET /b/{}.delta", keys[1])),
            N + 1,
            "{:#?}",
            fake.requests()
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

    /// A corrupt xattr on reference.bin (metadata that is not JSON) only
    /// skips the reference's own checksum: a delta GET still serves, and
    /// the object's SHA-256 still checks the reconstruction. Small deltas
    /// (buffered) and large ones (spooled), from cold readers. Before, every
    /// delta GET of the deltaspace failed.
    #[tokio::test]
    async fn a_corrupt_reference_xattr_does_not_fail_delta_gets() {
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
        let reference = walkdir::WalkDir::new(tmp.path())
            .into_iter()
            .filter_map(Result::ok)
            .find(|e| e.file_name() == "reference.bin")
            .expect("reference.bin")
            .into_path();
        xattr::set(
            &reference,
            crate::storage::xattr_meta::XATTR_NAME,
            b"this is not valid JSON {{{",
        )
        .unwrap();
        for max_object_size in [Config::default().max_object_size, 64 * 1024] {
            let config = Config {
                max_object_size,
                ..Config::default()
            };
            for fresh in [true, false] {
                let reader = DeltaGliderEngine::new_with_backend(backend.clone(), &config, None);
                let response = if fresh {
                    reader.retrieve_stream_fresh("b", "v/b.zip").await
                } else {
                    reader.retrieve_stream("b", "v/b.zip").await
                };
                let got = match response.unwrap_or_else(|e| {
                    panic!("max_object_size {max_object_size} fresh {fresh}: {e}")
                }) {
                    RetrieveResponse::Buffered { data, .. } => data,
                    RetrieveResponse::Streamed { stream, .. } => {
                        let chunks: Vec<Bytes> = stream.try_collect().await.unwrap();
                        chunks.concat()
                    }
                };
                assert!(got == v2, "max_object_size {max_object_size} fresh {fresh}");
            }
        }
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
