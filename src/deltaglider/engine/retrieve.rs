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
        let (obj_key, deltaspace_id) = Self::validated_key(bucket, key)?;

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
                // Populate metadata cache on resolve
                self.metadata_cache.insert(bucket, key, m.clone());
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
            Err(EngineError::NotFound(_)) if from_cache => {
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
                // Use the stored original_name (may differ from obj_key.filename if the file
                // was copied with a .delta suffix from another deployment)
                let stored_name = &metadata.original_name;
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
    #[instrument(skip(self))]
    #[allow(clippy::type_complexity)]
    pub async fn retrieve_stream_range(
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
        let (obj_key, deltaspace_id) = Self::validated_key(bucket, key)?;

        // Check metadata cache first. Track cache provenance so a stale
        // strategy (e.g. passthrough cached before a concurrent rewrite to
        // delta) can be invalidated and retried like `retrieve_stream()`.
        let (metadata, from_cache) = if let Some(cached) = self.metadata_cache.get(bucket, key) {
            (Some(cached), true)
        } else {
            (
                self.resolve_metadata_with_migration(bucket, &deltaspace_id, &obj_key)
                    .await?,
                false,
            )
        };

        let metadata = match metadata {
            Some(m) => {
                self.metadata_cache.insert(bucket, key, m.clone());
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
                let stored_name = &metadata.original_name;
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
                                        &fresh_meta.original_name,
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
            StorageInfo::Reference { .. } | StorageInfo::Delta { .. } => {
                // Delta/reference objects need full reconstruction — signal
                // caller to use the buffered path.
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
            StorageInfo::Delta { .. } => {
                // Fetch reference and delta in parallel — saves one S3 round-trip.
                // The reference is sibling to the delta: same parent directory + "reference.bin".
                let (ref_result, delta_result) = tokio::join!(
                    self.get_reference_cached(bucket, deltaspace_id),
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
                                let bytes = bytes::Bytes::from(data);
                                let cache_key = Self::cache_key(bucket, deltaspace_id);
                                self.cache.put(&cache_key, bytes.clone());
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

                // Guard against oversized inputs before spawning the codec task.
                // The reference + delta combined size is a lower bound for the
                // reconstructed object; reject early to avoid OOM.
                let combined_size = reference.len() as u64 + delta.len() as u64;
                if combined_size > self.max_object_size {
                    return Err(EngineError::TooLarge {
                        size: combined_size,
                        max: self.max_object_size,
                    });
                }

                // Wait up to 60s for a codec slot (GET should queue, not fail fast)
                let _codec_permit = self
                    .acquire_codec_timeout(std::time::Duration::from_secs(60))
                    .await?;
                let ref_clone = reference.clone();
                let codec = self.codec.clone();
                let decode_start = Instant::now();
                let result = tokio::task::spawn_blocking(move || codec.decode(&ref_clone, &delta))
                    .await
                    .map_err(|e| {
                        tracing::error!("Delta decode task panicked: {}", e);
                        EngineError::Storage(StorageError::Other(format!(
                            "codec task panicked: {}",
                            e
                        )))
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
            // Evict the cached reference for this deltaspace — it may be the
            // source of corruption. Without this, a corrupted reference loaded
            // from storage would poison the cache indefinitely, causing every
            // subsequent delta GET in this deltaspace to fail until the cache
            // entry is naturally evicted or the process restarts.
            let cache_key = Self::cache_key(bucket, deltaspace_id);
            self.cache.invalidate(&cache_key);
            warn!(
                "Checksum mismatch for {} (cache evicted for {}): expected {}, got {}",
                obj_key.full_key(),
                cache_key,
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
}
