// SPDX-License-Identifier: BUSL-1.1

//! Key validation, object metadata resolution, HEAD, and the verified
//! reference cache.

use super::*;

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
    pub(super) fn validated_key_ingest(
        &self,
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

        // Fetch delta and passthrough metadata in parallel — saves one S3 round-trip
        let (delta_result, passthrough_result) = tokio::join!(
            self.storage.get_delta_metadata(bucket, prefix, filename),
            self.storage
                .get_passthrough_metadata(bucket, prefix, filename),
        );

        // Only NotFound means absent. An I/O error (EACCES, EIO) is an
        // error: read as absent, HEAD answered 404 for a live object, DELETE
        // reported success without deleting, and `If-None-Match: *`
        // overwrote it.
        let delta = match delta_result {
            Ok(meta) => Some(meta),
            Err(StorageError::NotFound(_)) => None,
            Err(e) => return Err(e),
        };
        let passthrough = match passthrough_result {
            Ok(meta) => Some(meta),
            Err(StorageError::NotFound(_)) => None,
            Err(e) => return Err(e),
        };
        match (delta, passthrough) {
            (Some(d), Some(p)) => Ok(Some(if d.created_at >= p.created_at { d } else { p })),
            (Some(meta), None) | (None, Some(meta)) => Ok(Some(meta)),
            (None, None) => Ok(None),
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
        // Note: we do NOT use the metadata cache for HEAD. The cache is used for
        // LIST enrichment and file_size correction, but HEAD must always verify
        // the object exists on storage to handle out-of-band deletions correctly.
        // The cost is one storage call per HEAD, but HEAD is already a storage call.

        let (obj_key, deltaspace_id) = self.validated_key(bucket, key)?;

        let meta = match self
            .resolve_metadata_with_migration(bucket, &deltaspace_id, &obj_key)
            .await?
        {
            Some(meta) => meta,
            None => {
                // No DG metadata — try reading passthrough metadata (lightweight HEAD).
                // If that also fails (unmanaged file with no DG headers), return NotFound.
                // Both S3 and filesystem backends now return fallback metadata for files
                // that exist without DG metadata, so this should succeed for any existing file.
                self.storage
                    .get_passthrough_metadata(bucket, &deltaspace_id, &obj_key.filename)
                    .await
                    .map_err(|e| match e {
                        StorageError::NotFound(_) => EngineError::NotFound(obj_key.full_key()),
                        other => EngineError::Storage(other),
                    })?
            }
        };

        // Populate metadata cache on successful backend lookup
        self.metadata_cache.insert(bucket, key, meta.clone());
        Ok(meta)
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
        // Load the reference data and its recorded metadata together. The
        // metadata read is cheap (xattr / S3 HEAD) and runs in parallel so it
        // doesn't add a serial round-trip to the miss path. We use the
        // recorded checksum to verify the bytes BEFORE caching — a reference
        // that's corrupt on disk would otherwise be cached on the first miss
        // and poison every subsequent delta GET in the deltaspace.
        let (data_result, meta_result) = tokio::join!(
            self.storage.get_reference(bucket, deltaspace_id),
            self.storage.get_reference_metadata(bucket, deltaspace_id),
        );
        let data = data_result.map_err(|e| match e {
            StorageError::NotFound(_) => EngineError::MissingReference(deltaspace_id.to_string()),
            other => EngineError::Storage(other),
        })?;

        // Validate against the reference's own recorded checksum when present.
        // A missing metadata read or empty checksum (out-of-band / CLI-uploaded
        // references) is treated as "cannot verify" — we proceed and let the
        // downstream per-object checksum in retrieve.rs catch any corruption.
        let actual = hex::encode(Sha256::digest(&data));
        if let Ok(expected) = meta_result {
            if !expected.file_sha256.is_empty() {
                if let Err(expected_sha256) = reference_integrity_ok(&actual, &expected.file_sha256)
                {
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
            }
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
