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

    /// Issue #120: the first object of `bucket`, among the first `sample`
    /// that a listing gives, that the read rule refuses because it is not
    /// encrypted. The boot check warns with it about a bucket that encrypts
    /// with `plaintext_reads: refuse` and still holds plaintext objects
    /// (a restart from an edited file passes no apply gate). One listing
    /// page and one HEAD per sampled object.
    pub async fn first_refused_plaintext(
        &self,
        bucket: &str,
        sample: u32,
    ) -> Result<Option<String>, EngineError> {
        let page = self
            .list_objects(bucket, "", None, sample, None, false)
            .await?;
        for (key, _) in page.objects {
            if key.ends_with('/') {
                continue;
            }
            if let Err(EngineError::Storage(e)) = self.head(bucket, &key).await {
                if crate::storage::encrypting::seal::is_plaintext_refusal(&e) {
                    return Ok(Some(key));
                }
            }
        }
        Ok(None)
    }
}
