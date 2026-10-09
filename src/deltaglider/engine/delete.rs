// SPDX-License-Identifier: BUSL-1.1

//! Object delete paths and reference reclamation.

use super::*;
use crate::storage::ObjectVariant;

/// The sibling variant's state, read before a conditional delete's check.
enum SiblingPin {
    /// The backend has no conditional delete: the in-process lock guards.
    Unpinned,
    /// No sibling: a sibling that appears later is a peer's new object.
    Absent,
    /// Delete the sibling only while it is still this version.
    Version(String),
}

impl<S: StorageBackend> DeltaGliderEngine<S> {
    /// Delete one stored variant of `filename`, unconditionally.
    async fn delete_variant(
        &self,
        bucket: &str,
        deltaspace_id: &str,
        filename: &str,
        variant: ObjectVariant,
    ) -> Result<(), StorageError> {
        match variant {
            ObjectVariant::Delta => {
                self.storage
                    .delete_delta(bucket, deltaspace_id, filename)
                    .await
            }
            ObjectVariant::Passthrough => {
                self.storage
                    .delete_passthrough(bucket, deltaspace_id, filename)
                    .await
            }
        }
    }

    /// Delete the sibling storage variant (the one NOT matched by
    /// resolve_metadata) so a stale passthrough/delta pair can't resurrect a
    /// deleted key. NotFound (the normal case: only one variant exists) is
    /// success; any other error fails the DELETE before the object changes
    /// (a retry finds the sibling and deletes it).
    async fn delete_sibling_variant(
        &self,
        bucket: &str,
        deltaspace_id: &str,
        filename: &str,
        sibling: ObjectVariant,
    ) -> Result<(), StorageError> {
        match self
            .delete_variant(bucket, deltaspace_id, filename, sibling)
            .await
        {
            Ok(()) | Err(StorageError::NotFound(_)) => Ok(()),
            Err(e) => {
                warn!(
                    "sibling-variant cleanup for {bucket}/{deltaspace_id}/{filename} \
                     ({sibling:?}) failed: {e}"
                );
                Err(e)
            }
        }
    }

    /// Delete an object
    #[instrument(skip(self))]
    pub async fn delete(&self, bucket: &str, key: &str) -> Result<FileMetadata, EngineError> {
        Self::deleted(
            key,
            self.delete_inner(bucket, key, /* reclaim_reference = */ true, None)
                .await?,
        )
    }

    /// Delete `key` only if `still_ours` accepts the object as read under
    /// the deltaspace lock. Every PUT holds that lock, so no overwrite can
    /// land between the check and the delete (a HEAD, then a delete by key,
    /// removed an overwrite that landed in between). A peer INSTANCE's PUT
    /// does not take this lock: where the backend has a conditional delete
    /// (S3 `If-Match`), the delete is also pinned to the stored version the
    /// check saw; elsewhere (filesystem, a backend answering 501) the
    /// in-process lock is the only guard.
    #[instrument(skip(self, still_ours))]
    pub async fn delete_if(
        &self,
        bucket: &str,
        key: &str,
        still_ours: &(dyn Fn(&FileMetadata) -> bool + Send + Sync),
    ) -> Result<ConditionalDelete, EngineError> {
        self.delete_inner(bucket, key, true, Some(still_ours)).await
    }

    fn deleted(key: &str, outcome: ConditionalDelete) -> Result<FileMetadata, EngineError> {
        match outcome {
            ConditionalDelete::Deleted(meta) => Ok(*meta),
            // Unconditional deletes report a missing object as NotFound.
            ConditionalDelete::Changed | ConditionalDelete::Gone => {
                Err(EngineError::NotFound(key.to_string()))
            }
        }
    }

    /// Delete one member of a prefix sweep, SKIPPING the per-object
    /// "is the deltaspace empty now?" reference-reclamation scan.
    ///
    /// That scan lists the WHOLE deltaspace, so running it per object makes a
    /// prefix sweep O(N²) in directory reads (1100 objects ≈ 600k entry reads —
    /// enough to blow past the request timeout). A sweep is deleting everything
    /// anyway, so the caller runs [`Self::reclaim_empty_deltaspace`] ONCE when
    /// the sweep finishes. Semantics are otherwise identical to [`Self::delete`].
    pub async fn delete_in_sweep(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<FileMetadata, EngineError> {
        Self::deleted(
            key,
            self.delete_inner(bucket, key, /* reclaim_reference = */ false, None)
                .await?,
        )
    }

    /// Reclaim a deltaspace's `reference.bin` if no non-reference object remains.
    /// The tail half of [`Self::delete`], callable once after a prefix sweep.
    /// Idempotent and safe when the deltaspace still holds objects (no-op).
    pub async fn reclaim_empty_deltaspace(
        &self,
        bucket: &str,
        deltaspace_id: &str,
    ) -> Result<(), EngineError> {
        let _guard = self.acquire_prefix_lock(bucket, deltaspace_id).await;
        let Some((xnode, reclaimed_ref_bytes)) =
            self.reclaimable_reference(bucket, deltaspace_id).await?
        else {
            return Ok(());
        };
        xnode
            .delete_reference(&*self.storage, bucket, deltaspace_id)
            .await?;
        self.cache
            .invalidate(&self.cache_key(bucket, deltaspace_id));
        // Mirror `delete`'s accounting: the reclaimed reference bytes leave
        // stored_bytes (no object count change — the objects were counted as
        // they were individually deleted).
        if let Some(u) = &self.bucket_usage {
            u.apply_net(bucket, None, None, -(reclaimed_ref_bytes as i64));
        }
        Ok(())
    }

    /// `Some((lock, reference bytes))` when the deltaspace holds a reference
    /// and nothing else, so the reference can go. Caller holds the prefix
    /// lock. Multi-instance: the emptiness scan runs again under the
    /// cross-instance lock, because a peer can write a delta against the
    /// reference between the first scan and the delete. The first scan runs
    /// unlocked so a delete in a non-empty deltaspace pays no lock requests.
    async fn reclaimable_reference(
        &self,
        bucket: &str,
        deltaspace_id: &str,
    ) -> Result<Option<(ReferenceLockGuard, u64)>, EngineError> {
        let only_reference = |remaining: &[FileMetadata]| -> Option<u64> {
            let mut ref_bytes = None;
            for m in remaining {
                match m.storage_info {
                    StorageInfo::Reference { .. } => ref_bytes = Some(m.file_size),
                    _ => return None,
                }
            }
            Some(ref_bytes.unwrap_or(0))
        };
        let remaining = self.storage.scan_deltaspace(bucket, deltaspace_id).await?;
        let Some(mut ref_bytes) = only_reference(&remaining) else {
            return Ok(None);
        };
        if !self.storage.has_reference(bucket, deltaspace_id).await? {
            return Ok(None);
        }
        let xnode = self.acquire_reference_lock(bucket, deltaspace_id).await?;
        if xnode.is_cross_instance() {
            let remaining = self.storage.scan_deltaspace(bucket, deltaspace_id).await?;
            match only_reference(&remaining) {
                Some(b) if self.storage.has_reference(bucket, deltaspace_id).await? => {
                    ref_bytes = b
                }
                _ => return Ok(None),
            }
        }
        Ok(Some((xnode, ref_bytes)))
    }

    async fn delete_inner(
        &self,
        bucket: &str,
        key: &str,
        reclaim_reference: bool,
        still_ours: Option<&(dyn Fn(&FileMetadata) -> bool + Send + Sync)>,
    ) -> Result<ConditionalDelete, EngineError> {
        let (obj_key, deltaspace_id) = self.validated_key(bucket, key)?;

        info!("Deleting {}/{}", bucket, key);

        // Acquire per-deltaspace lock to prevent races with concurrent store/delete
        // operations that may create or clean up the reference.
        let _guard = self.acquire_prefix_lock(bucket, &deltaspace_id).await;

        // Use resolve_metadata (no migration) — we already hold the prefix lock, and
        // tokio::sync::Mutex is not reentrant, so calling resolve_metadata_with_migration
        // here would deadlock. Legacy objects that haven't been migrated yet will appear
        // as NotFound; a prior GET/HEAD on the key will have triggered migration.
        let Some(metadata) = self
            .resolve_metadata(bucket, &deltaspace_id, &obj_key)
            .await?
        else {
            return match still_ours {
                Some(_) => Ok(ConditionalDelete::Gone),
                None => Err(EngineError::NotFound(obj_key.full_key())),
            };
        };
        let (variant, sibling) = match metadata.storage_info {
            StorageInfo::Delta { .. } => (ObjectVariant::Delta, ObjectVariant::Passthrough),
            _ => (ObjectVariant::Passthrough, ObjectVariant::Delta),
        };
        // A conditional delete also pins the stored versions where the
        // backend can (S3 If-Match): a peer INSTANCE's PUT does not take our
        // in-process lock. Both versions are read BEFORE the check's read, so
        // any overwrite after it fails the delete. The sibling is pinned too
        // (review A6): a peer's PUT of the sibling kind after the check is
        // a new object, not a stale variant.
        let mut pinned: Option<String> = None;
        let mut sibling_pin = SiblingPin::Unpinned;
        let metadata = match still_ours {
            None => metadata,
            Some(ours) => {
                sibling_pin = match self
                    .storage
                    .variant_version(bucket, &deltaspace_id, &obj_key.filename, sibling)
                    .await
                {
                    Ok(None) => SiblingPin::Unpinned,
                    Ok(Some(version)) => SiblingPin::Version(version),
                    Err(StorageError::NotFound(_)) => SiblingPin::Absent,
                    Err(e) => return Err(e.into()),
                };
                let checked = match self
                    .storage
                    .variant_version(bucket, &deltaspace_id, &obj_key.filename, variant)
                    .await
                {
                    Ok(None) => metadata,
                    Ok(Some(version)) => {
                        pinned = Some(version);
                        match self
                            .resolve_metadata(bucket, &deltaspace_id, &obj_key)
                            .await?
                        {
                            Some(m)
                                if std::mem::discriminant(&m.storage_info)
                                    == std::mem::discriminant(&metadata.storage_info) =>
                            {
                                m
                            }
                            Some(_) => return Ok(ConditionalDelete::Changed),
                            None => return Ok(ConditionalDelete::Gone),
                        }
                    }
                    Err(StorageError::NotFound(_)) => return Ok(ConditionalDelete::Gone),
                    Err(e) => return Err(e.into()),
                };
                if !ours(&checked) {
                    return Ok(ConditionalDelete::Changed);
                }
                checked
            }
        };

        if matches!(metadata.storage_info, StorageInfo::Reference { .. }) {
            return Err(EngineError::InvalidArgument(
                "Reference objects are internal and cannot be deleted directly".to_string(),
            ));
        }
        // A key can transiently have BOTH a passthrough and a delta variant
        // (a PUT whose cleanup of the other kind failed). resolve_metadata
        // picks the newest; the stale sibling must go too, or a later GET
        // resolves it and the deleted object comes back (H33). The sibling
        // goes FIRST (review A6): a failure at either step then leaves the
        // newest variant, so a failed DELETE changes nothing visible and a
        // retry converges.
        match &sibling_pin {
            SiblingPin::Absent => {}
            SiblingPin::Version(version) => {
                if !self
                    .storage
                    .delete_variant_if(bucket, &deltaspace_id, &obj_key.filename, sibling, version)
                    .await?
                {
                    return Ok(ConditionalDelete::Changed);
                }
            }
            SiblingPin::Unpinned => {
                self.delete_sibling_variant(bucket, &deltaspace_id, &obj_key.filename, sibling)
                    .await?
            }
        }
        match &pinned {
            Some(version) => {
                if !self
                    .storage
                    .delete_variant_if(bucket, &deltaspace_id, &obj_key.filename, variant, version)
                    .await?
                {
                    return Ok(ConditionalDelete::Changed);
                }
            }
            None => {
                self.delete_variant(bucket, &deltaspace_id, &obj_key.filename, variant)
                    .await?
            }
        }

        // If this deltaspace no longer has any objects, clean up its reference
        // baseline. SKIPPED for prefix sweeps (`delete_in_sweep`): this scan
        // lists the entire deltaspace, so running it per object makes a sweep
        // O(N²) in directory reads. The sweep caller reclaims once at the end
        // via `reclaim_empty_deltaspace`.
        // Bytes of a reclaimed reference.bin (stored-only) — subtracted from the
        // counter so stored_bytes stays exact when the last delta is removed.
        let mut reclaimed_ref_bytes = 0u64;
        // The object is already gone: a failed reclaim check (a peer holds
        // the reference lock, a listing error) must not fail the DELETE.
        // The orphan reference is harmless and reclaimed on a later delete.
        let reclaimable = if reclaim_reference {
            match self.reclaimable_reference(bucket, &deltaspace_id).await {
                Ok(r) => r,
                Err(e) => {
                    warn!("reference reclaim skipped for {bucket}/{deltaspace_id}: {e}");
                    None
                }
            }
        } else {
            None
        };
        if let Some((xnode, ref_bytes)) = reclaimable {
            // Delete storage BEFORE invalidating cache — prevents stale cache entries
            // from a concurrent GET loading between invalidation and deletion.
            // Best-effort like the check above: the object is gone, so a lost
            // lock or a transient error must not turn the DELETE into a 500.
            match xnode
                .delete_reference(&*self.storage, bucket, &deltaspace_id)
                .await
            {
                Ok(()) => {
                    reclaimed_ref_bytes = ref_bytes;
                    let cache_key = self.cache_key(bucket, &deltaspace_id);
                    self.cache.invalidate(&cache_key);
                }
                Err(e) => warn!("reference reclaim failed for {bucket}/{deltaspace_id}: {e}"),
            }
        }

        // Invalidate metadata cache for the deleted key
        self.metadata_cache.invalidate(bucket, key);

        // Release the per-prefix lock before cleanup so strong_count drops to 1.
        drop(_guard);
        self.cleanup_prefix_locks();

        // Best-effort counter update: -1 object + reclaimed reference bytes.
        self.record_delete(bucket, &metadata, reclaimed_ref_bytes);

        debug!("Deleted {}/{}", bucket, key);
        Ok(ConditionalDelete::Deleted(Box::new(metadata)))
    }
}
