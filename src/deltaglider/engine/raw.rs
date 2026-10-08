// SPDX-License-Identifier: BUSL-1.1

//! Raw deltaspace blob accessors (replication delta-passthrough).

use super::*;

impl<S: StorageBackend> DeltaGliderEngine<S> {
    // === Raw deltaspace blob accessors (replication delta-passthrough) ===
    //
    // These read/write the LITERAL stored blob + metadata through the
    // routed+wrapped storage top. For a plaintext object the encrypting
    // wrapper is a no-op so the round-trip is byte-verbatim; markers on
    // the returned metadata reflect AT-REST state (the wrapper encrypts
    // bodies, not metadata). Policy lives in `transfer.rs`; the engine
    // only exposes the routed raw I/O + the per-deltaspace lock.

    /// Read a delta blob verbatim from a deltaspace.
    pub async fn get_delta_raw(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<Vec<u8>, StorageError> {
        self.storage.get_delta(bucket, prefix, filename).await
    }

    /// Write a delta blob + metadata verbatim into a deltaspace. Call it
    /// inside [`Self::with_dest_prefix_lock`]; outside it, multi-instance
    /// refuses.
    pub async fn put_delta_raw(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        // Inside `with_dest_prefix_lock`: the delta is only valid against the
        // reference the held cross-instance lock protects.
        let held = self.held_reference_lock(bucket, prefix, "delta")?;
        held.put_delta(&*self.storage, bucket, prefix, filename, data, metadata)
            .await
            .map_err(engine_to_storage)?;
        // Mirror every engine store path: a delta write supersedes any stale
        // PASSTHROUGH variant of the same key — leaving it behind lets a
        // later delta delete resurrect old content. Cache must drop too.
        if let Err(e) = self
            .delete_passthrough_idempotent(bucket, prefix, filename)
            .await
        {
            tracing::warn!(
                "fast-path delta write {}/{}/{}: stale passthrough cleanup failed: {}",
                bucket,
                prefix,
                filename,
                e
            );
        }
        let full_key = if prefix.is_empty() {
            filename.to_string()
        } else {
            format!("{}/{}", prefix, filename)
        };
        self.metadata_cache.invalidate(bucket, &full_key);
        Ok(())
    }

    /// Read a deltaspace reference blob verbatim.
    pub async fn get_reference_raw(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<Vec<u8>, StorageError> {
        self.storage.get_reference(bucket, prefix).await
    }

    /// Write a deltaspace reference blob + metadata verbatim. Call it inside
    /// [`Self::with_dest_prefix_lock`]: that holds the cross-instance lock
    /// the write goes through. Outside it, multi-instance refuses.
    pub async fn put_reference_raw(
        &self,
        bucket: &str,
        prefix: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        let held = self.held_reference_lock(bucket, prefix, "reference")?;
        held.put_reference(&*self.storage, bucket, prefix, data, metadata)
            .await
            .map_err(engine_to_storage)
    }

    /// Reference metadata as a `Result` (errors propagate) — for callers that
    /// must distinguish "no reference" from a read failure during seeding.
    pub async fn reference_metadata_raw(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<FileMetadata, StorageError> {
        self.storage.get_reference_metadata(bucket, prefix).await
    }

    /// Reference metadata for a deltaspace. `Ok(None)` only when the backend
    /// confirms that no reference exists: a failed read (an S3 HEAD that
    /// answers 503) is an `Err`, never an absence, because a caller that
    /// writes on "no reference" would overwrite a live reference.bin.
    pub async fn reference_meta(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<Option<FileMetadata>, StorageError> {
        if !self.storage.has_reference(bucket, prefix).await? {
            return Ok(None);
        }
        match self.storage.get_reference_metadata(bucket, prefix).await {
            Ok(meta) => Ok(Some(meta)),
            Err(StorageError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Delta metadata for one object (full Delta info incl. `ref_sha256`).
    pub async fn delta_meta(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<FileMetadata, StorageError> {
        self.storage
            .get_delta_metadata(bucket, prefix, filename)
            .await
    }
}
