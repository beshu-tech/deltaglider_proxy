// SPDX-License-Identifier: BUSL-1.1

//! Bucket operations (delegate to storage).

use super::delete::ReferenceHint;
use super::*;

impl<S: StorageBackend> DeltaGliderEngine<S> {
    // === Bucket operations (delegate to storage) ===

    /// Make durable every object write that the storage deferred
    /// (`storage::with_deferred_fsync`).
    pub async fn flush_pending(&self) -> Result<(), EngineError> {
        Ok(self.storage.flush_pending().await?)
    }

    /// Create a real bucket on the storage backend.
    pub async fn create_bucket(&self, bucket: &str) -> Result<(), EngineError> {
        Ok(self.storage.create_bucket(bucket).await?)
    }

    /// Delete a real bucket on the storage backend (must be empty).
    ///
    /// An orphan reference.bin (its deltas are gone, but a delete was cut
    /// before its reclaim) is invisible to clients, so it must not keep the
    /// bucket. Each one goes through the reclaim of a delete: it takes the
    /// deltaspace lock (and the cross-instance lock), checks again that the
    /// reference is alone, and only then deletes it. A PUT in flight holds
    /// that lock while it encodes against the reference, so the reclaim
    /// waits for it, finds its delta, and keeps the reference; the storage
    /// then answers BucketNotEmpty. The storage never deletes a reference
    /// on its own.
    pub async fn delete_bucket(&self, bucket: &str) -> Result<(), EngineError> {
        for prefix in self.storage.list_reference_prefixes(bucket, "").await? {
            match self
                .reclaim_empty_deltaspace(bucket, &prefix, ReferenceHint::Present)
                .await
            {
                Ok(()) => {}
                Err(e) if e.is_not_found() => {}
                Err(e) => return Err(e),
            }
        }
        Ok(self.storage.delete_bucket(bucket).await?)
    }

    /// List all real buckets from the storage backend.
    pub async fn list_buckets(&self) -> Result<Vec<String>, EngineError> {
        Ok(self.storage.list_buckets().await?)
    }

    /// List all real buckets with their creation dates.
    pub async fn list_buckets_with_dates(
        &self,
    ) -> Result<Vec<(String, chrono::DateTime<chrono::Utc>)>, EngineError> {
        Ok(self.storage.list_buckets_with_dates().await?)
    }

    /// List buckets with optional backend-origin metadata.
    pub async fn list_bucket_origins(
        &self,
    ) -> Result<Vec<crate::storage::BucketListing>, EngineError> {
        Ok(self.storage.list_bucket_origins().await?)
    }

    /// Check if a real bucket exists on the storage backend.
    pub async fn head_bucket(&self, bucket: &str) -> Result<bool, EngineError> {
        Ok(self.storage.head_bucket(bucket).await?)
    }

    /// Issue #120: a sample of `bucket` (the first `sample` objects that a
    /// listing gives, one HEAD each): the first object that the read rule
    /// refuses because it is not encrypted, and whether any sampled object
    /// is encrypted. The boot starts the conversion of a bucket that a
    /// restart made encrypt only when the sample holds plaintext and no
    /// encrypted object (`maintenance::conversion::start_at_boot`).
    pub async fn plaintext_sample(
        &self,
        bucket: &str,
        sample: u32,
    ) -> Result<PlaintextSample, EngineError> {
        let page = self
            .list_objects(bucket, "", None, sample, None, false)
            .await?;
        let mut out = PlaintextSample::default();
        for (key, _) in page.objects {
            if key.ends_with('/') {
                continue;
            }
            match self.head(bucket, &key).await {
                Ok(meta) => {
                    out.encrypted |= meta
                        .user_metadata
                        .contains_key(crate::storage::encrypting::ENCRYPTION_MARKER_KEY);
                }
                Err(EngineError::Storage(e))
                    if crate::storage::encrypting::seal::is_plaintext_refusal(&e) =>
                {
                    out.first_refused.get_or_insert(key);
                }
                Err(_) => {}
            }
        }
        Ok(out)
    }
}

/// What [`DeltaGliderEngine::plaintext_sample`](super::DeltaGliderEngine::plaintext_sample) found.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PlaintextSample {
    /// The first sampled object that a client cannot read because it is
    /// not encrypted.
    pub first_refused: Option<String>,
    /// A sampled object is encrypted (presented with `dg-encrypted`).
    pub encrypted: bool,
}
