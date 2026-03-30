//! Store pipeline — delta encoding, passthrough, and baseline management.

use super::*;
use crate::storage::StorageBackend;
use md5::{Digest, Md5};
use sha2::Sha256;
use std::time::Instant;

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

        let (obj_key, deltaspace_id) = Self::validated_key(bucket, key)?;

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

        // Check if file type is eligible for delta compression
        if !self.file_router.is_delta_eligible(&obj_key.filename) {
            debug!("File type not delta-eligible, storing as passthrough");
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
            return Ok(result);
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
        };

        // Check if deltaspace already has a reference (existing deltaspace)
        let has_existing_reference = self
            .storage
            .has_reference(ctx.bucket, ctx.deltaspace_id)
            .await;

        // Ensure deltaspace has an internal reference baseline.
        let ref_meta = if has_existing_reference {
            self.storage
                .get_reference_metadata(ctx.bucket, ctx.deltaspace_id)
                .await?
        } else {
            debug!("No reference in deltaspace, creating baseline");
            self.set_reference_baseline(&ctx).await?
        };

        // Encode delta and decide: keep as delta or fall back to direct storage
        let result = self
            .encode_and_store(ctx, &ref_meta, has_existing_reference)
            .await?;
        self.metadata_cache
            .insert(bucket, key, result.metadata.clone());
        Ok(result)
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
        // Only apply the threshold when NO reference exists yet (first file in deltaspace).
        // Once a reference exists, ALWAYS store as delta — the deltaspace is committed to
        // delta storage and we want all related files to benefit from the shared reference.
        if !has_existing_reference && ratio >= self.max_delta_ratio {
            debug!(
                "First file in deltaspace with poor delta ratio {:.2} >= {:.2}, storing as passthrough",
                ratio, self.max_delta_ratio
            );
            self.with_metrics(|m| {
                m.delta_decisions_total
                    .with_label_values(&["passthrough"])
                    .inc()
            });
            // Write passthrough first, then clean up old delta/reference.
            // This prevents transient 404s on concurrent GETs during strategy transition.
            let del_bucket = ctx.bucket.to_string();
            let del_dsid = ctx.deltaspace_id.to_string();
            let del_filename = ctx.obj_key.filename.clone();
            let result = self.store_passthrough(ctx).await?;
            let cache_key = Self::cache_key(&del_bucket, &del_dsid);
            self.cache.invalidate(&cache_key);
            if let Err(e) = self.storage.delete_reference(&del_bucket, &del_dsid).await {
                warn!(
                    "Failed to clean up reference after passthrough write: {}",
                    e
                );
            }
            if let Err(e) = self
                .delete_delta_idempotent(&del_bucket, &del_dsid, &del_filename)
                .await
            {
                warn!("Failed to clean up delta after passthrough write: {}", e);
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

        Ok(StoreResult {
            metadata,
            stored_size: delta.len() as u64,
        })
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
        if total_size > self.max_object_size {
            return Err(EngineError::TooLarge {
                size: total_size,
                max: self.max_object_size,
            });
        }

        // Invalidate stale metadata on overwrite
        self.metadata_cache.invalidate(bucket, key);

        let (obj_key, deltaspace_id) = Self::validated_key(bucket, key)?;

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

        let result = StoreResult {
            metadata,
            stored_size: total_size,
        };
        self.metadata_cache
            .insert(bucket, key, result.metadata.clone());
        Ok(result)
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

        self.storage
            .put_passthrough(
                ctx.bucket,
                ctx.deltaspace_id,
                &ctx.obj_key.filename,
                ctx.data,
                &metadata,
            )
            .await?;

        Ok(StoreResult {
            metadata,
            stored_size: ctx.data.len() as u64,
        })
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
    async fn delete_passthrough_idempotent(
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
        if !self.storage.has_reference(bucket, deltaspace_id).await {
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
            if !self.storage.has_reference(bucket, ds).await {
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
