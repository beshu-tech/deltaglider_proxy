// SPDX-License-Identifier: BUSL-1.1

//! Key validation, object metadata resolution, HEAD, and the verified
//! reference cache.

use super::*;
use crate::storage::ObjectVariant;

/// Pure integrity check for a freshly-loaded reference baseline.
///
/// `expected_sha256` is the reference's own recorded checksum (from its
/// stored `FileMetadata.file_sha256`). When it is empty we cannot verify
/// — references uploaded out-of-band (e.g. the Python CLI, or fallback
/// metadata with no DG xattrs) carry no checksum — so we treat that as a
/// pass and let the downstream per-object checksum be the safety net.
///
/// When the checksum IS present and disagrees with the actual data, the
/// reference on disk is corrupt; returning the `(expected, actual)` pair
/// lets the caller fail fast WITHOUT caching the bad bytes. Without this,
/// a corrupted reference would be cached on the first miss and poison
/// every subsequent delta GET in the deltaspace until natural eviction
/// (the downstream checksum-mismatch path in `retrieve.rs` only evicts
/// after a reconstruction has already failed).
pub(super) fn reference_integrity_ok(
    actual_sha256: &str,
    expected_sha256: &str,
) -> Result<(), String> {
    if expected_sha256.is_empty() || actual_sha256 == expected_sha256 {
        Ok(())
    } else {
        Err(expected_sha256.to_string())
    }
}

/// Which stored forms of an object exist (`resolve_stored_forms`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoredForms {
    pub(super) delta: bool,
    pub(super) passthrough: bool,
}

/// The stand-in metadata of a stored form whose facts the read rule
/// refuses: its kind only, and the oldest creation time, so a form that
/// opens is the newest.
fn unopened_form(filename: &str, form: ObjectVariant) -> FileMetadata {
    let mut meta = match form {
        ObjectVariant::Delta => FileMetadata::new_delta(
            filename.to_string(),
            String::new(),
            String::new(),
            0,
            String::new(),
            String::new(),
            0,
            None,
        ),
        ObjectVariant::Passthrough => FileMetadata::new_passthrough(
            filename.to_string(),
            String::new(),
            String::new(),
            0,
            None,
        ),
    };
    meta.created_at = chrono::DateTime::UNIX_EPOCH;
    meta
}

impl StoredForms {
    pub(super) fn has(self, form: ObjectVariant) -> bool {
        match form {
            ObjectVariant::Delta => self.delta,
            ObjectVariant::Passthrough => self.passthrough,
        }
    }
}

/// PURE: newest-wins between the two stored forms of one object (a crash
/// between a write and its `drop_other_form` leaves both). A tie goes to
/// the delta.
pub(super) fn delta_is_newest(delta: &FileMetadata, passthrough: &FileMetadata) -> bool {
    delta.created_at >= passthrough.created_at
}

impl StoredForms {
    /// Exactly the one stored form `form`.
    fn only(form: crate::storage::ObjectVariant) -> Self {
        Self {
            delta: form == crate::storage::ObjectVariant::Delta,
            passthrough: form == crate::storage::ObjectVariant::Passthrough,
        }
    }
}

/// The one stored form of each key of a job's listing page, where the page
/// proved it ([`crate::storage::ListedForms`]).
///
/// Only a maintenance job may HEAD one form on this proof: its bucket's
/// write gate holds every client write (and defers replication and
/// lifecycle writes) from before the listing until after the HEAD, so no
/// form appears between the two. A client HEAD reads both forms (plan
/// decision D4: a crash between a write and its other-form DELETE leaves
/// both, and newest-wins needs both reads).
#[derive(Debug, Clone, Default)]
pub struct ListedPageForms(std::collections::HashMap<String, crate::storage::ObjectVariant>);

impl ListedPageForms {
    pub(super) fn of_page(
        objects: &[(String, FileMetadata)],
        forms: &crate::storage::ListedForms,
    ) -> Self {
        Self(
            objects
                .iter()
                .filter_map(|(k, meta)| Some((k.clone(), forms.single_form(k, meta)?)))
                .collect(),
        )
    }

    /// The one stored form of `key`; `None`: not proved (HEAD both).
    pub fn of(&self, key: &str) -> Option<crate::storage::ObjectVariant> {
        self.0.get(key).copied()
    }
}

impl<S: StorageBackend> DeltaGliderEngine<S> {
    /// Build the cache key for a deltaspace's reference.
    /// THE per-deltaspace key: the reference cache and the in-process
    /// deltaspace lock. Keyed by the STORAGE, so two alias names of one real
    /// bucket (one reference.bin) share the entry and the lock.
    pub(super) fn cache_key(&self, bucket: &str, deltaspace_id: &str) -> String {
        format!(
            "{}/{}",
            self.storage.storage_identity(bucket),
            deltaspace_id
        )
    }

    /// Parse and validate an S3 key, returning the parsed key and deltaspace ID.
    pub(super) fn validated_key(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<(ObjectKey, String), EngineError> {
        let obj_key = ObjectKey::parse(bucket, key);
        obj_key
            .validate_object()
            .map_err(|e| EngineError::InvalidArgument(e.to_string()))?;
        let deltaspace_id = obj_key.deltaspace_id();
        Ok((obj_key, deltaspace_id))
    }

    /// Like `validated_key` but stricter — the INGEST (PUT) gate. Rejects `//`
    /// so a malformed key can't be STORED; reads/deletes keep using
    /// `validated_key` so pre-existing `//` objects stay reachable for cleanup
    /// on S3. (The filesystem backend refuses `.`/empty segments on every
    /// path it builds: `filesystem::check_path_segments`.)
    /// `lock`: the write lock of `key`; a guard of another key is refused,
    /// so every store writes under its own key's lock.
    pub(super) fn validated_key_ingest(
        &self,
        lock: &super::conditional::ObjectWriteGuard,
        bucket: &str,
        key: &str,
    ) -> Result<(ObjectKey, String), EngineError> {
        let obj_key = ObjectKey::parse(bucket, key);
        obj_key
            .validate_ingest()
            .map_err(|e| EngineError::InvalidArgument(e.to_string()))?;
        // A folder marker is stored only by `store` (zero bytes, through
        // `put_directory_marker`). Every other ingest path would write a data
        // object under the marker's name, so they all refuse it here.
        if obj_key.is_directory_marker() {
            return Err(EngineError::InvalidArgument(
                "A key that ends in '/' is a folder marker and must have an empty body".to_string(),
            ));
        }
        let deltaspace_id = obj_key.deltaspace_id();
        lock.covers(bucket, &deltaspace_id, &obj_key.filename)?;
        Ok((obj_key, deltaspace_id))
    }

    /// Look up object metadata by checking both delta and passthrough storage,
    /// returning the most recent version if both exist.
    pub(super) async fn resolve_object_metadata(
        &self,
        bucket: &str,
        prefix: &str,
        original_name: &str,
    ) -> Result<Option<FileMetadata>, StorageError> {
        let filename = original_name.rsplit('/').next().unwrap_or(original_name);
        Ok(self
            .resolve_stored_forms(bucket, prefix, filename)
            .await?
            .map(|(meta, _)| meta))
    }

    /// THE lookup of an object's two stored forms (`filename.delta` and
    /// `filename`), with one HEAD each, in parallel: the object's metadata
    /// (the newest form when both exist) and which forms exist (a delete
    /// sends no DELETE for a form that is not there). `None`: neither.
    pub(super) async fn resolve_stored_forms(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<Option<(FileMetadata, StoredForms)>, StorageError> {
        Ok(self
            .resolve_forms(bucket, prefix, filename, false)
            .await?
            .map(|(meta, forms, _)| (meta, forms)))
    }

    /// [`Self::resolve_stored_forms`] for a DELETE. A form that the read
    /// rule refuses (`StorageError::Encryption` or `KeyNotHeld`: its seal does not open, or
    /// it is plaintext where the bucket refuses plaintext) counts as present
    /// with unknown facts, and the refusal comes back with it: an object
    /// that cannot be read can still be deleted.
    pub(super) async fn resolve_forms_to_delete(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<Option<(FileMetadata, StoredForms, Option<StorageError>)>, StorageError> {
        self.resolve_forms(bucket, prefix, filename, true).await
    }

    async fn resolve_forms(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        refused_is_present: bool,
    ) -> Result<Option<(FileMetadata, StoredForms, Option<StorageError>)>, StorageError> {
        // Fetch delta and passthrough metadata in parallel — saves one S3
        // round-trip. A folder marker (`dir/`: no file name) has no `.delta`
        // form: only `put_directory_marker` stores it. Its delta HEAD
        // (`dir/.delta`) was a request that could only answer 404.
        let (delta, passthrough) = tokio::join!(
            async {
                if filename.is_empty() {
                    return Ok(None);
                }
                self.stored_form_metadata(bucket, prefix, filename, ObjectVariant::Delta)
                    .await
            },
            self.stored_form_metadata(bucket, prefix, filename, ObjectVariant::Passthrough),
        );
        let mut refusal = None;
        let mut present = |read: Result<Option<FileMetadata>, StorageError>, form| match read {
            Err(e @ (StorageError::Encryption(_) | StorageError::KeyNotHeld { .. }))
                if refused_is_present =>
            {
                refusal = Some(e);
                Ok(Some(unopened_form(filename, form)))
            }
            other => other,
        };
        let delta = present(delta, ObjectVariant::Delta)?;
        let passthrough = present(passthrough, ObjectVariant::Passthrough)?;
        let forms = StoredForms {
            delta: delta.is_some(),
            passthrough: passthrough.is_some(),
        };
        let newest = match (delta, passthrough) {
            (Some(d), Some(p)) => Some(if delta_is_newest(&d, &p) { d } else { p }),
            (one, None) | (None, one) => one,
        };
        Ok(newest.map(|meta| (meta, forms, refusal)))
    }

    /// The metadata of ONE stored form of an object (a HEAD on S3), `None`
    /// when it is absent. Only NotFound means absent. An I/O error (EACCES,
    /// EIO) is an error: read as absent, HEAD answered 404 for a live
    /// object, DELETE reported success without deleting, and
    /// `If-None-Match: *` overwrote it.
    pub(super) async fn stored_form_metadata(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        form: ObjectVariant,
    ) -> Result<Option<FileMetadata>, StorageError> {
        let read = match form {
            ObjectVariant::Delta => {
                self.storage
                    .get_delta_metadata(bucket, prefix, filename)
                    .await
            }
            ObjectVariant::Passthrough => {
                self.storage
                    .get_passthrough_metadata(bucket, prefix, filename)
                    .await
            }
        };
        match read {
            Ok(meta) => Ok(Some(meta)),
            Err(StorageError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Resolve metadata for an object key, with no migration attempt.
    ///
    /// Use this from callers that **already hold** the per-deltaspace prefix lock
    /// (e.g. `delete()`). Calling `resolve_metadata_with_migration` from such a
    /// caller would deadlock because tokio's async Mutex is not reentrant.
    pub(super) async fn resolve_metadata(
        &self,
        bucket: &str,
        deltaspace_id: &str,
        obj_key: &ObjectKey,
    ) -> Result<Option<FileMetadata>, EngineError> {
        Ok(self
            .resolve_object_metadata(bucket, deltaspace_id, &obj_key.full_key())
            .await?)
    }

    /// Resolve metadata with legacy migration fallback, acquiring the per-deltaspace
    /// prefix lock before migration to prevent races with concurrent `store()` calls.
    ///
    /// Uses double-checked locking:
    /// 1. Fast path: look up metadata without the lock.
    /// 2. If not found, acquire the prefix lock.
    /// 3. Re-check under the lock (a concurrent writer may have already migrated).
    /// 4. If still not found, attempt migration under the lock.
    ///
    /// **Do not call this from a caller that already holds the prefix lock** — use
    /// `resolve_metadata` instead to avoid a deadlock.
    pub(super) async fn resolve_metadata_with_migration(
        &self,
        bucket: &str,
        deltaspace_id: &str,
        obj_key: &ObjectKey,
    ) -> Result<Option<FileMetadata>, EngineError> {
        // Fast path: most objects are found immediately without acquiring the lock.
        let metadata = self
            .resolve_object_metadata(bucket, deltaspace_id, &obj_key.full_key())
            .await?;
        if metadata.is_some() {
            return Ok(metadata);
        }

        // Legacy migration removed from GET hot path — it was blocking downloads
        // for 60+ seconds on large reference files. Migration is now batch-only
        // via the /_/api/admin/migrate endpoint.
        //
        // If the object still isn't found, return None and let the caller
        // fall through to the unmanaged passthrough path.
        Ok(None)
    }

    /// Drop the cached metadata for one key. Used by the metadata-backfill
    /// job after an in-place metadata rewrite — the 10-minute cache would
    /// otherwise keep serving the pre-backfill (fallback) metadata on LIST.
    pub fn invalidate_metadata_cache(&self, bucket: &str, key: &str) {
        self.metadata_cache.invalidate(bucket, key);
    }

    pub async fn head(&self, bucket: &str, key: &str) -> Result<FileMetadata, EngineError> {
        let found = self
            .retry_missing_during_writes(bucket, key, || self.head_with_forms(bucket, key))
            .await?;
        Ok(found.0)
    }

    /// [`Self::head`], and which stored forms of the key exist (the
    /// precondition check of a conditional write keeps them for the write).
    pub(super) async fn head_with_forms(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<(FileMetadata, StoredForms), EngineError> {
        // Note: we do NOT use the metadata cache for HEAD. The cache is used for
        // LIST enrichment and file_size correction, but HEAD must always verify
        // the object exists on storage to handle out-of-band deletions correctly.
        // The cost is one storage call per HEAD, but HEAD is already a storage call.

        let (obj_key, deltaspace_id) = self.validated_key(bucket, key)?;

        // The lookup's passthrough read returns fallback metadata for an
        // object without DG metadata (both backends), so `None` means both
        // forms are absent. (A second passthrough HEAD here cost a missing
        // key 3 requests instead of 2.)
        let (meta, forms) = self
            .resolve_stored_forms(bucket, &deltaspace_id, &obj_key.filename)
            .await?
            .ok_or_else(|| EngineError::NotFound(obj_key.full_key()))?;

        // Populate metadata cache on successful backend lookup
        self.metadata_cache.insert(bucket, key, meta.clone());
        Ok((meta, forms))
    }

    /// `head_with_forms` (a HEAD of both forms) for a key of a maintenance job's listing
    /// page: one HEAD of `listed`, the one stored form that the page proved
    /// ([`ListedPageForms`]), instead of one per form. `None`, or a form
    /// that is gone (the object changed outside the proxy), reads both.
    pub async fn head_listed(
        &self,
        bucket: &str,
        key: &str,
        listed: Option<crate::storage::ObjectVariant>,
    ) -> Result<(FileMetadata, StoredForms), EngineError> {
        use crate::storage::ObjectVariant;
        let Some(form) = listed else {
            return self.head_with_forms(bucket, key).await;
        };
        let (obj_key, deltaspace_id) = self.validated_key(bucket, key)?;
        let read = match form {
            ObjectVariant::Delta if !obj_key.filename.is_empty() => {
                self.storage
                    .get_delta_metadata(bucket, &deltaspace_id, &obj_key.filename)
                    .await
            }
            ObjectVariant::Delta => return self.head_with_forms(bucket, key).await,
            ObjectVariant::Passthrough => {
                self.storage
                    .get_passthrough_metadata(bucket, &deltaspace_id, &obj_key.filename)
                    .await
            }
        };
        match read {
            Ok(meta) => {
                self.metadata_cache.insert(bucket, key, meta.clone());
                Ok((meta, StoredForms::only(form)))
            }
            Err(StorageError::NotFound(_)) => self.head_with_forms(bucket, key).await,
            Err(e) => Err(e.into()),
        }
    }

    /// The metadata of the deltaspace's reference.bin from ONE read (a HEAD
    /// on S3). `None` only when the backend says it is absent: a failed read
    /// is an error, because a caller that writes on "no reference" would
    /// overwrite a live baseline. (`has_reference` then
    /// `get_reference_metadata` sent two HEADs of one object.)
    pub(super) async fn reference_metadata_if_present(
        &self,
        bucket: &str,
        deltaspace_id: &str,
    ) -> Result<Option<FileMetadata>, StorageError> {
        match self
            .storage
            .get_reference_metadata(bucket, deltaspace_id)
            .await
        {
            Ok(meta) => Ok(Some(meta)),
            Err(StorageError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Get reference with caching. Returns `Bytes` for zero-copy sharing.
    /// Returns `(reference_data, cache_hit)`.
    /// `expected_sha256` is the reference sha the caller is about to rely on
    /// (the stored reference metadata on PUT, the delta's `ref_sha256` on
    /// GET; empty = cannot verify). A cached copy with another sha is stale:
    /// a peer node reseeded the deltaspace. Encoding against it wrote deltas
    /// that no other node could decode.
    ///
    /// Single flight: concurrent cold reads of one deltaspace wait for one
    /// download (`ReferenceCache::get_or_load`) instead of each sending its
    /// own GET of the whole reference.
    pub(super) async fn get_reference_cached(
        &self,
        bucket: &str,
        deltaspace_id: &str,
        expected_sha256: &str,
    ) -> Result<(bytes::Bytes, bool), EngineError> {
        let cache_key = self.cache_key(bucket, deltaspace_id);
        let (data, hit) = self
            .cache
            .get_or_load(&cache_key, expected_sha256, || {
                self.load_verified_reference(bucket, deltaspace_id)
            })
            .await?;
        self.with_metrics(|m| {
            if hit {
                m.cache_hits_total.inc()
            } else {
                m.cache_misses_total.inc()
            }
        });
        Ok((data, hit))
    }

    /// Load a reference and check it against its own recorded SHA-256.
    /// Returns the bytes and their SHA-256, for the cache.
    async fn load_verified_reference(
        &self,
        bucket: &str,
        deltaspace_id: &str,
    ) -> Result<(bytes::Bytes, String), EngineError> {
        // ONE read: the GET response carries the reference's recorded
        // metadata in its headers (`open_object`). We use the recorded
        // checksum to verify the bytes BEFORE caching — a reference that's
        // corrupt on disk would otherwise be cached on the first miss and
        // poison every subsequent delta GET in the deltaspace.
        let crate::storage::ObjectRead {
            stream,
            metadata: recorded,
            ..
        } = self
            .storage
            .open_object(
                bucket,
                deltaspace_id,
                crate::storage::StoredObject::Reference,
            )
            .await
            .map_err(|e| match e {
                StorageError::NotFound(_) => {
                    EngineError::MissingReference(deltaspace_id.to_string())
                }
                other => EngineError::Storage(other),
            })?;
        let data = {
            use futures::TryStreamExt;
            let chunks: Vec<Bytes> = stream.try_collect().await?;
            chunks.concat()
        };

        // Validate against the reference's own recorded checksum when present.
        // An empty checksum (out-of-band / CLI-uploaded references) is
        // treated as "cannot verify" — we proceed and let the downstream
        // per-object checksum in retrieve.rs catch any corruption.
        let actual = hex::encode(Sha256::digest(&data));
        if let Err(expected_sha256) = reference_integrity_ok(&actual, &recorded.file_sha256) {
            // Do NOT cache corrupt bytes — fail fast so a single bad
            // reference doesn't fan out into repeated reconstruction
            // failures across the deltaspace.
            warn!(
                "Reference integrity check failed for {}/{}: expected {}, got {} — not caching",
                bucket, deltaspace_id, expected_sha256, actual
            );
            return Err(EngineError::ChecksumMismatch {
                key: format!("{}/.dg/reference.bin", deltaspace_id),
                expected: expected_sha256,
                actual,
            });
        }

        // PERF: Convert Vec→Bytes once (zero-copy ownership transfer); the
        // cache keeps a clone (refcount increment, no memcpy).
        Ok((Bytes::from(data), actual))
    }
}

/// Pure: after the reconstruction of a delta failed its checksum, can the
/// cached reference of its deltaspace be the cause? Only when the delta
/// names no reference checksum: then nothing tied that reference to this
/// delta. A delta that names one was decoded against the reference the
/// cache holds under that checksum, or against the stored reference, read
/// and checked against its own recorded checksum. Either way the reference
/// is good and the delta is bad: evicting the reference made each GET of
/// the bad object (and the next GET of every good one) download it again.
pub(super) fn reference_suspect_after_mismatch(metadata: &FileMetadata) -> bool {
    match &metadata.storage_info {
        StorageInfo::Delta { ref_sha256, .. } => ref_sha256.is_empty(),
        _ => false,
    }
}

/// Which checksum failure evicts the cached reference.
#[cfg(test)]
mod reference_eviction_tests {
    use super::*;

    #[test]
    fn only_a_delta_without_a_reference_sha_suspects_the_reference() {
        let delta = |ref_sha: &str| {
            FileMetadata::new_delta(
                "a.zip".into(),
                "s".into(),
                "m".into(),
                10,
                "reference.bin".into(),
                ref_sha.into(),
                3,
                None,
            )
        };
        assert!(reference_suspect_after_mismatch(&delta("")));
        assert!(!reference_suspect_after_mismatch(&delta("abc")));
        let plain = FileMetadata::new_passthrough("a.bin".into(), "s".into(), "m".into(), 1, None);
        assert!(!reference_suspect_after_mismatch(&plain));
    }
}

/// Fixed request costs of a HEAD.
#[cfg(test)]
mod head_cost_tests {
    use super::*;

    /// A HEAD of a missing key costs the two variant HEADs. It sent a third:
    /// the passthrough HEAD of the lookup, again.
    #[tokio::test]
    async fn a_head_of_a_missing_key_sends_two_requests() {
        let (engine, fake) = s3_engine().await;
        fake.clear();
        let err = engine.head("b", "dir/missing.zip").await.unwrap_err();
        assert!(err.is_not_found(), "{err}");
        assert_eq!(fake.requests().len(), 2, "{:?}", fake.requests());
    }

    /// Line `i` reads the metadata of one form: by its `StorageBackendExt`
    /// name, or by `head_object` with `form` in the call.
    fn reads_metadata(lines: &[(usize, &str)], i: usize, name: &str, form: &str) -> bool {
        let line = lines[i].1;
        line.contains(name)
            || (line.contains(".head_object(")
                && lines[i..lines.len().min(i + 4)]
                    .iter()
                    .any(|(_, l)| l.contains(form)))
    }

    /// Both stored forms of an object are read together in one place
    /// (`resolve_stored_forms`): `delete.rs` kept its own copy of the
    /// parallel HEADs and the newest-wins rule.
    #[test]
    fn the_two_stored_forms_are_read_together_in_one_place() {
        let mut sites = Vec::new();
        for (file, text) in crate::source_scan::prod_sources("src/deltaglider/engine") {
            let lines = crate::source_scan::prod_lines(&text);
            for (i, (n, _)) in lines.iter().enumerate() {
                if !reads_metadata(&lines, i, ".get_delta_metadata(", "StoredObject::Delta") {
                    continue;
                }
                let both = (i.saturating_sub(6)..lines.len().min(i + 6)).any(|j| {
                    reads_metadata(
                        &lines,
                        j,
                        ".get_passthrough_metadata(",
                        "StoredObject::Passthrough",
                    )
                });
                if both {
                    sites.push((file.clone(), *n));
                }
            }
        }
        let files: Vec<&str> = sites.iter().map(|(f, _)| f.as_str()).collect();
        assert_eq!(
            files,
            ["src/deltaglider/engine/metadata.rs"],
            "both forms read at {sites:?}: use resolve_stored_forms"
        );
    }

    /// One read answers whether a deltaspace has a reference and what it is
    /// (`reference_metadata_if_present`, `reference_fence_with_metadata`):
    /// engine code does not ask `has_reference` or `reference_fence` and
    /// then read the metadata (two HEADs on S3).
    #[test]
    fn engine_code_reads_a_reference_once() {
        const ALLOWED: [&str; 0] = [];
        let mut sites = Vec::new();
        for (file, text) in crate::source_scan::prod_sources("src/deltaglider/engine") {
            let lines = crate::source_scan::prod_lines(&text);
            for (i, (n, line)) in lines.iter().enumerate() {
                // `reference_fence` then the metadata: the shape the
                // cross-instance acquire had (reference_fence_with_metadata).
                if !line.contains(".has_reference(") && !line.contains(".reference_fence(") {
                    continue;
                }
                let then_reads = (i..lines.len().min(i + 15)).any(|j| {
                    reads_metadata(
                        &lines,
                        j,
                        ".get_reference_metadata(",
                        "StoredObject::Reference",
                    )
                });
                if then_reads {
                    sites.push((file.clone(), *n));
                }
            }
        }
        let files: Vec<&str> = sites.iter().map(|(f, _)| f.as_str()).collect();
        assert_eq!(
            files, ALLOWED,
            "has_reference then get_reference_metadata ({sites:?}): use \
             reference_metadata_if_present"
        );
    }
}
