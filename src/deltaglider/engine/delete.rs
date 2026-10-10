// SPDX-License-Identifier: BUSL-1.1

//! Object delete paths and reference reclamation.
//!
//! [`DeltaGliderEngine::delete_batch`] is THE way to delete more than one
//! key: a per-key [`DeltaGliderEngine::delete`] in a loop checks the
//! folder for a reference to reclaim after every key (a source guard in
//! `src/lib.rs` refuses it).

use super::*;
use crate::storage::ObjectVariant;

/// Folders (deltaspaces) that one [`DeltaGliderEngine::delete_batch`]
/// works on at the same time. The keys of one folder go one after
/// another: the engine's per-deltaspace lock serialises them anyway.
pub const BULK_DELETE_CONCURRENCY: usize = 8;

/// The condition of a [`DeleteItem`]: `true` while the stored object is
/// still the one the caller means to delete.
pub type StillOurs<'a> = dyn Fn(&FileMetadata) -> bool + Send + Sync + 'a;

/// One key of a [`DeltaGliderEngine::delete_batch`].
pub struct DeleteItem<'a> {
    /// The object key, as a client names it.
    pub key: String,
    /// `Some`: delete the object only while this check accepts it, as read
    /// under the deltaspace lock (see [`DeltaGliderEngine::delete_if`]).
    pub still_ours: Option<Box<StillOurs<'a>>>,
}

impl<'a> DeleteItem<'a> {
    /// Delete `key` whatever it holds.
    pub fn new(key: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            still_ours: None,
        }
    }

    /// Delete `key` only while `still_ours` accepts the stored object
    /// (a lifecycle rule's snapshot, the source of a move).
    pub fn only_if(
        key: impl Into<String>,
        still_ours: impl Fn(&FileMetadata) -> bool + Send + Sync + 'a,
    ) -> Self {
        Self {
            key: key.into(),
            still_ours: Some(Box::new(still_ours)),
        }
    }
}

/// What [`DeltaGliderEngine::delete_batch`] did with one key.
#[derive(Debug)]
pub enum DeleteOutcome {
    /// Deleted. The metadata is the object's as it was.
    Deleted(Box<FileMetadata>),
    /// No such object: nothing to delete (S3 answers success). Its folder
    /// is still checked for a reference.bin left alone, so a retry of a
    /// cut batch reclaims what the cut skipped.
    NotFound,
    /// The item's condition refused the object: it is unchanged. A stale
    /// older variant of it may be gone.
    Changed,
    /// Not tried: [`DeleteHooks::proceed`] answered `false`.
    Skipped,
    /// The delete failed. The object may still exist.
    Failed(EngineError),
}

/// The [`DeleteHooks::on_outcome`] hook: the key, its outcome, and a
/// future that the batch awaits before the folder's next key.
pub type OnDeleteOutcome<'a> =
    dyn Fn(&str, &DeleteOutcome) -> futures::future::BoxFuture<'static, ()> + Send + Sync + 'a;

/// Optional hooks of a [`DeltaGliderEngine::delete_batch`].
#[derive(Default, Clone, Copy)]
pub struct DeleteHooks<'a> {
    /// Asked before each key and before each folder's reclaim. `false`:
    /// the key is [`DeleteOutcome::Skipped`] and the folder keeps its
    /// reference.bin (for example, a maintenance job armed on the bucket).
    pub proceed: Option<&'a (dyn Fn() -> bool + Send + Sync)>,
    /// Awaited as each key ends, before the next key of its folder starts,
    /// with the key and its outcome (not after the batch: a batch that is
    /// cut part-way has then published every key it deleted). Every key
    /// gets one call, in no particular order across folders.
    pub on_outcome: Option<&'a OnDeleteOutcome<'a>>,
}

impl DeleteHooks<'_> {
    fn proceeds(&self) -> bool {
        self.proceed.is_none_or(|proceed| proceed())
    }

    async fn report(&self, key: &str, outcome: &DeleteOutcome) {
        if let Some(on_outcome) = self.on_outcome {
            on_outcome(key, outcome).await;
        }
    }
}

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

    /// Delete one object, then reclaim its folder's reference.bin when
    /// nothing else is left in the folder. `NotFound` when there is no such
    /// object (the folder is still checked). For more than one key use
    /// [`Self::delete_batch`].
    #[instrument(skip(self))]
    pub async fn delete(&self, bucket: &str, key: &str) -> Result<FileMetadata, EngineError> {
        match self.delete_one(bucket, DeleteItem::new(key)).await {
            DeleteOutcome::Deleted(meta) => Ok(*meta),
            DeleteOutcome::Failed(e) => Err(e),
            // NotFound (Changed and Skipped need a condition or a hook).
            _ => Err(EngineError::NotFound(key.to_string())),
        }
    }

    /// Delete `key` only if `still_ours` accepts the object as read under
    /// the deltaspace lock. Every PUT holds that lock, so no overwrite can
    /// land between the check and the delete (a HEAD, then a delete by key,
    /// removed an overwrite that landed in between). A peer INSTANCE's PUT
    /// does not take this lock: where the backend has a conditional delete
    /// (S3 `If-Match`), the delete is also pinned to the stored version the
    /// check saw; elsewhere (filesystem, a backend answering 501) the
    /// in-process lock is the only guard. For more than one key use
    /// [`Self::delete_batch`] with [`DeleteItem::only_if`].
    #[instrument(skip(self, still_ours))]
    pub async fn delete_if(
        &self,
        bucket: &str,
        key: &str,
        still_ours: &(dyn Fn(&FileMetadata) -> bool + Send + Sync),
    ) -> Result<ConditionalDelete, EngineError> {
        Ok(
            match self
                .delete_one(bucket, DeleteItem::only_if(key, still_ours))
                .await
            {
                DeleteOutcome::Deleted(meta) => ConditionalDelete::Deleted(meta),
                DeleteOutcome::Changed => ConditionalDelete::Changed,
                DeleteOutcome::Failed(e) => return Err(e),
                DeleteOutcome::NotFound | DeleteOutcome::Skipped => ConditionalDelete::Gone,
            },
        )
    }

    /// One key through [`Self::delete_batch`].
    async fn delete_one(&self, bucket: &str, item: DeleteItem<'_>) -> DeleteOutcome {
        let mut outcomes = self
            .delete_batch(bucket, vec![item], DeleteHooks::default())
            .await;
        // One outcome per item.
        outcomes.pop().unwrap_or(DeleteOutcome::Skipped)
    }

    /// Delete `items` from `bucket`: THE way to delete more than one key
    /// (S3 DeleteObjects, admin bulk delete and move, and every job that
    /// deletes what it listed). Returns one outcome per item, in input
    /// order.
    ///
    /// - The keys are grouped by folder (deltaspace). The keys of one folder
    ///   go one after another; [`BULK_DELETE_CONCURRENCY`] folders run at
    ///   once.
    /// - Each key costs the delete alone. After a folder's last key the
    ///   folder is checked ONCE for a reference.bin left alone, and the
    ///   reference is reclaimed: one LIST page of the folder's own level,
    ///   plus a HEAD and a DELETE when the reference goes. A per-key
    ///   [`Self::delete`] pays that check after every key. A key found
    ///   already gone also marks its folder: a retry of a batch cut before
    ///   its reclaim repairs the folder.
    /// - A key with a condition ([`DeleteItem::only_if`]) is deleted as by
    ///   [`Self::delete_if`].
    /// - [`DeleteHooks::on_outcome`] runs as each key ends, so a caller
    ///   publishes events per key; [`DeleteHooks::proceed`] stops the batch
    ///   between keys.
    ///
    /// The counter forgets each object as soon as its delete lands, before
    /// any reclaim, so a caller dropped mid-batch never leaves a deleted
    /// object counted. A caller that must finish the batch whatever its
    /// own request does (a client disconnect, the request timeout) runs it
    /// in a spawned task and awaits that.
    pub async fn delete_batch(
        &self,
        bucket: &str,
        items: Vec<DeleteItem<'_>>,
        hooks: DeleteHooks<'_>,
    ) -> Vec<DeleteOutcome> {
        use futures::StreamExt;
        let mut outcomes: Vec<Option<DeleteOutcome>> =
            std::iter::repeat_with(|| None).take(items.len()).collect();
        let mut folders: std::collections::BTreeMap<String, Vec<usize>> = Default::default();
        for (i, item) in items.iter().enumerate() {
            match self.validated_key(bucket, &item.key) {
                Ok((_, deltaspace_id)) => folders.entry(deltaspace_id).or_default().push(i),
                Err(e) => {
                    let outcome = DeleteOutcome::Failed(e);
                    hooks.report(&item.key, &outcome).await;
                    outcomes[i] = Some(outcome);
                }
            }
        }
        let items = &items;
        let done: Vec<Vec<(usize, DeleteOutcome)>> = futures::stream::iter(folders)
            .map(|(deltaspace_id, indexes)| {
                self.delete_folder_items(bucket, deltaspace_id, indexes, items, hooks)
            })
            .buffer_unordered(BULK_DELETE_CONCURRENCY)
            .collect()
            .await;
        for (i, outcome) in done.into_iter().flatten() {
            outcomes[i] = Some(outcome);
        }
        outcomes
            .into_iter()
            // Every index got its outcome above.
            .map(|o| o.unwrap_or(DeleteOutcome::Skipped))
            .collect()
    }

    /// The keys `indexes` of `items`, all in `deltaspace_id`, one after
    /// another; then the folder's reclaim when a key went (or was gone).
    async fn delete_folder_items(
        &self,
        bucket: &str,
        deltaspace_id: String,
        indexes: Vec<usize>,
        items: &[DeleteItem<'_>],
        hooks: DeleteHooks<'_>,
    ) -> Vec<(usize, DeleteOutcome)> {
        let mut out = Vec::with_capacity(indexes.len());
        let mut reclaim = false;
        for i in indexes {
            let item = &items[i];
            let outcome = if hooks.proceeds() {
                self.delete_item(bucket, item).await
            } else {
                DeleteOutcome::Skipped
            };
            reclaim |= matches!(outcome, DeleteOutcome::Deleted(_) | DeleteOutcome::NotFound);
            hooks.report(&item.key, &outcome).await;
            out.push((i, outcome));
        }
        // Best effort: the objects are gone already, and an orphan
        // reference.bin goes with a later delete in its folder.
        if reclaim && hooks.proceeds() {
            match self.reclaim_empty_deltaspace(bucket, &deltaspace_id).await {
                Ok(()) => {}
                Err(e) if e.is_not_found() => {
                    debug!("reference reclaim skipped for {bucket}/{deltaspace_id}: {e}")
                }
                Err(e) => warn!("reference reclaim skipped for {bucket}/{deltaspace_id}: {e}"),
            }
        }
        out
    }

    async fn delete_item(&self, bucket: &str, item: &DeleteItem<'_>) -> DeleteOutcome {
        match self
            .delete_inner(bucket, &item.key, item.still_ours.as_deref())
            .await
        {
            Ok(ConditionalDelete::Deleted(meta)) => DeleteOutcome::Deleted(meta),
            Ok(ConditionalDelete::Changed) => DeleteOutcome::Changed,
            Ok(ConditionalDelete::Gone) => DeleteOutcome::NotFound,
            Err(e) if e.is_not_found() => DeleteOutcome::NotFound,
            Err(e) => DeleteOutcome::Failed(e),
        }
    }

    /// Delete one member of a prefix sweep with no reclaim check: the
    /// caller runs [`Self::reclaim_empty_deltaspace`] once per folder at
    /// the end, as [`Self::delete_batch`] does.
    pub async fn delete_in_sweep(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<FileMetadata, EngineError> {
        match self.delete_inner(bucket, key, None).await? {
            ConditionalDelete::Deleted(meta) => Ok(*meta),
            ConditionalDelete::Changed | ConditionalDelete::Gone => {
                Err(EngineError::NotFound(key.to_string()))
            }
        }
    }

    /// [`Self::delete_in_sweep`] with a condition, as [`Self::delete_if`].
    pub async fn delete_if_in_sweep(
        &self,
        bucket: &str,
        key: &str,
        still_ours: &(dyn Fn(&FileMetadata) -> bool + Send + Sync),
    ) -> Result<ConditionalDelete, EngineError> {
        self.delete_inner(bucket, key, Some(still_ours)).await
    }

    /// Reclaim a deltaspace's `reference.bin` if no non-reference object remains.
    /// [`Self::delete_batch`] runs it after the last key of each folder.
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
    /// lock. Multi-instance: the emptiness check runs again under the
    /// cross-instance lock, because a peer can write a delta against the
    /// reference between the first check and the delete. The first check
    /// runs unlocked so a delete in a non-empty deltaspace pays no lock
    /// requests.
    async fn reclaimable_reference(
        &self,
        bucket: &str,
        deltaspace_id: &str,
    ) -> Result<Option<(ReferenceLockGuard, u64)>, EngineError> {
        let Some(mut ref_bytes) = self.only_reference_left(bucket, deltaspace_id).await? else {
            return Ok(None);
        };
        let xnode = self.acquire_reference_lock(bucket, deltaspace_id).await?;
        if xnode.is_cross_instance() {
            match self.only_reference_left(bucket, deltaspace_id).await? {
                Some(b) => ref_bytes = b,
                None => return Ok(None),
            }
        }
        Ok(Some((xnode, ref_bytes)))
    }

    /// The bytes that the store of reference.bin added to the counter (its
    /// plaintext size) when the deltaspace holds the reference and nothing
    /// else; `None` while an object remains or without a reference.
    ///
    /// One listing of the deltaspace's own level, up to its first object
    /// ([`StorageBackend::holds_only_reference`]), then one HEAD of the
    /// reference only when it is alone. The LIST size of an encrypted
    /// reference is its ciphertext size, 28 bytes more than the store
    /// counted, so the size comes from the HEAD.
    async fn only_reference_left(
        &self,
        bucket: &str,
        deltaspace_id: &str,
    ) -> Result<Option<u64>, StorageError> {
        if !self
            .storage
            .holds_only_reference(bucket, deltaspace_id)
            .await?
        {
            return Ok(None);
        }
        match self
            .storage
            .get_reference_metadata(bucket, deltaspace_id)
            .await
        {
            Ok(meta) => Ok(Some(meta.file_size)),
            Err(StorageError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The object's metadata (the newest variant when both exist, as
    /// `resolve_object_metadata` picks it) and which variants the two
    /// parallel HEADs found, so a delete skips the DELETE of a variant that
    /// is not there. `None`: neither exists.
    async fn resolve_variants(
        &self,
        bucket: &str,
        deltaspace_id: &str,
        filename: &str,
    ) -> Result<Option<(FileMetadata, impl Fn(ObjectVariant) -> bool)>, StorageError> {
        let (delta, passthrough) = tokio::join!(
            self.storage
                .get_delta_metadata(bucket, deltaspace_id, filename),
            self.storage
                .get_passthrough_metadata(bucket, deltaspace_id, filename),
        );
        // Only NotFound means absent: an I/O error is an error.
        let found = |r: Result<FileMetadata, StorageError>| match r {
            Ok(meta) => Ok(Some(meta)),
            Err(StorageError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        };
        let (delta, passthrough) = (found(delta)?, found(passthrough)?);
        let (has_delta, has_passthrough) = (delta.is_some(), passthrough.is_some());
        let newest = match (delta, passthrough) {
            (Some(d), Some(p)) => Some(if d.created_at >= p.created_at { d } else { p }),
            (one, None) | (None, one) => one,
        };
        Ok(newest.map(|meta| {
            (meta, move |v: ObjectVariant| match v {
                ObjectVariant::Delta => has_delta,
                ObjectVariant::Passthrough => has_passthrough,
            })
        }))
    }

    /// Delete one object, with no reclaim check: `Gone` when there is no
    /// such object. The counter forgets the object before this returns.
    async fn delete_inner(
        &self,
        bucket: &str,
        key: &str,
        still_ours: Option<&(dyn Fn(&FileMetadata) -> bool + Send + Sync)>,
    ) -> Result<ConditionalDelete, EngineError> {
        let (obj_key, deltaspace_id) = self.validated_key(bucket, key)?;

        info!("Deleting {}/{}", bucket, key);

        // Acquire per-deltaspace lock to prevent races with concurrent store/delete
        // operations that may create or clean up the reference.
        let _guard = self.acquire_prefix_lock(bucket, &deltaspace_id).await;

        // No migration: we already hold the prefix lock, and
        // tokio::sync::Mutex is not reentrant, so resolve_metadata_with_migration
        // here would deadlock. Legacy objects that haven't been migrated yet will appear
        // as NotFound; a prior GET/HEAD on the key will have triggered migration.
        let Some((metadata, found)) = self
            .resolve_variants(bucket, &deltaspace_id, &obj_key.filename)
            .await?
        else {
            return Ok(ConditionalDelete::Gone);
        };
        let (variant, sibling) = match metadata.storage_info {
            StorageInfo::Delta { .. } => (ObjectVariant::Delta, ObjectVariant::Passthrough),
            _ => (ObjectVariant::Passthrough, ObjectVariant::Delta),
        };
        let sibling_found = found(sibling);
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
            // A sibling that its HEAD did not find gets no DELETE (and no
            // facts cleanup): under the prefix lock no PUT of this process
            // can add one.
            SiblingPin::Unpinned if !sibling_found => {}
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

        // The object is gone: the counter forgets it now, before any other
        // await, so a caller dropped later (in its reclaim) leaves no drift.
        self.record_delete(bucket, &metadata, 0);
        self.metadata_cache.invalidate(bucket, key);

        // Release the per-prefix lock before cleanup so strong_count drops to 1.
        drop(_guard);
        self.cleanup_prefix_locks();

        debug!("Deleted {}/{}", bucket, key);
        Ok(ConditionalDelete::Deleted(Box::new(metadata)))
    }
}
