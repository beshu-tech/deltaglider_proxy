// SPDX-License-Identifier: BUSL-1.1

//! Raw deltaspace blob accessors (replication delta-passthrough).

use super::usage::CounterPrior;
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

    /// [`Self::get_delta_raw`] and the metadata stored with the blob, both
    /// from ONE read (the GET response on S3, the file and its xattr on
    /// the filesystem). The metadata names the generation of these exact
    /// bytes, so a caller that pins a generation needs no HEAD after the
    /// GET.
    pub async fn get_delta_raw_with_metadata(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<(Vec<u8>, FileMetadata), StorageError> {
        self.open_raw(
            bucket,
            prefix,
            crate::storage::StoredObject::Delta(filename),
        )
        .await
    }

    /// The reference blob verbatim and its stored metadata, from ONE read
    /// (see [`Self::get_delta_raw_with_metadata`]).
    pub async fn get_reference_raw_with_metadata(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<(Vec<u8>, FileMetadata), StorageError> {
        self.open_raw(bucket, prefix, crate::storage::StoredObject::Reference)
            .await
    }

    async fn open_raw(
        &self,
        bucket: &str,
        prefix: &str,
        object: crate::storage::StoredObject<'_>,
    ) -> Result<(Vec<u8>, FileMetadata), StorageError> {
        use futures::TryStreamExt;
        let (stream, meta) = self.storage.open_object(bucket, prefix, object).await?;
        let data = stream
            .try_fold(Vec::new(), |mut acc, chunk| async move {
                acc.extend_from_slice(&chunk);
                Ok(acc)
            })
            .await?;
        Ok((data, meta))
    }

    /// Write a delta blob + metadata verbatim into a deltaspace. Call it
    /// inside [`Self::with_dest_prefix_lock`]; outside it, multi-instance
    /// refuses. `prior`: what the key held, read before the write
    /// ([`Self::fast_path_prior`]); `CounterPrior::Unknown` when the
    /// caller did not read it.
    pub async fn put_delta_raw(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        data: Bytes,
        metadata: &FileMetadata,
        prior: &CounterPrior,
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
        self.drop_other_form(
            bucket,
            prefix,
            filename,
            crate::storage::ObjectVariant::Delta,
            prior,
        )
        .await;
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
        data: &Bytes,
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
        self.reference_metadata_if_present(bucket, prefix).await
    }

    /// Bytes `start..=end` of the delta object `bucket/key` whose HEAD is
    /// `source_head`, for an UploadPartCopy. Every part of one source
    /// reads the same verified reconstruction: the range-spool cache,
    /// matched on the content SHA-256 of `source_head` (single flight).
    /// Before, each part rebuilt the whole object. The HEAD is the caller's
    /// fresh read of this request, so a cached reconstruction of its SHA-256
    /// is the content that the request copies; a reconstruction that does
    /// not match that SHA-256 fails the integrity gate.
    pub async fn delta_copy_range(
        &self,
        bucket: &str,
        key: &str,
        source_head: &FileMetadata,
        start: u64,
        end: u64,
    ) -> Result<(BoxStream<'static, Result<Bytes, StorageError>>, u64), EngineError> {
        let (obj_key, deltaspace_id) = self.validated_key(bucket, key)?;
        self.retrieve_delta_range_spooled(bucket, &deltaspace_id, &obj_key, source_head, start, end)
            .await
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
