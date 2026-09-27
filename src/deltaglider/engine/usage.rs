// SPDX-License-Identifier: BUSL-1.1

//! Best-effort bucket usage counter updates.

use super::*;

impl<S: StorageBackend> DeltaGliderEngine<S> {
    /// Best-effort: fold a stored object into the bucket counter. Never fails
    /// the S3 path. Applies the NET delta the store path captured:
    /// - new object: +1 / +logical / +stored
    /// - overwrite (`result.replaced` set): subtract the prior version first so
    ///   the count nets to +0 objects (S3 PUT is an upsert — a blind +1 here is
    ///   the over-count bug the review caught)
    /// - a newly-seeded reference.bin: + its bytes into stored_bytes (symmetric
    ///   with `record_delete`'s reclamation subtraction, so inline == scan).
    pub(super) fn record_store(&self, bucket: &str, result: &StoreResult) {
        let Some(u) = &self.bucket_usage else { return };
        // net: -prior (if overwrite) + new object, + any newly-seeded reference.
        u.apply_net(
            bucket,
            result.replaced.as_deref(),
            Some(&result.metadata),
            result.reference_created_bytes as i64,
        );
    }

    /// Best-effort: fold a deleted object out of the bucket counter (-1), plus
    /// any reclaimed reference bytes (stored-only) so stored_bytes stays exact.
    pub(super) fn record_delete(
        &self,
        bucket: &str,
        meta: &FileMetadata,
        reclaimed_ref_bytes: u64,
    ) {
        let Some(u) = &self.bucket_usage else { return };
        u.apply_net(bucket, Some(meta), None, -(reclaimed_ref_bytes as i64));
    }

    /// Resolve the prior object at `bucket/key` for overwrite-net accounting —
    /// only when a counter is attached. `None` on miss / no counter.
    pub(super) async fn prior_for_counter(&self, bucket: &str, key: &str) -> Option<FileMetadata> {
        self.bucket_usage.as_ref()?;
        let (obj_key, deltaspace_id) = self.validated_key(bucket, key).ok()?;
        self.resolve_metadata(bucket, &deltaspace_id, &obj_key)
            .await
            .ok()
            .flatten()
    }

    /// Best-effort counter update for the delta-passthrough FAST PATH
    /// (`transfer.rs`), which ships a `.delta` verbatim via `put_delta_raw` and
    /// thus bypasses the `store()` choke point. Overwrite-aware + adds any
    /// reference the copy seeded. Mirrors `Self::record_store`.
    /// Snapshot the destination's PRIOR metadata for fast-path accounting.
    /// MUST be called BEFORE the fast-path write — calling `prior_for_counter`
    /// after the write returns the just-written delta, netting an overwrite to
    /// zero (the dest bucket usage counter then never grows).
    pub async fn fast_path_prior(&self, bucket: &str, dest_key: &str) -> Option<FileMetadata> {
        self.prior_for_counter(bucket, dest_key).await
    }

    pub fn record_fast_path_copy(
        &self,
        bucket: &str,
        prior: Option<&FileMetadata>,
        delta_meta: &FileMetadata,
        seeded_reference_bytes: u64,
    ) {
        let Some(u) = &self.bucket_usage else { return };
        u.apply_net(
            bucket,
            prior,
            Some(delta_meta),
            seeded_reference_bytes as i64,
        );
    }
}
