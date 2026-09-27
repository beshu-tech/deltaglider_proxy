// SPDX-License-Identifier: BUSL-1.1

//! Bucket operations (delegate to storage).

use super::*;

impl<S: StorageBackend> DeltaGliderEngine<S> {
    // === Bucket operations (delegate to storage) ===

    /// Create a real bucket on the storage backend.
    /// Make durable every object write that the storage deferred
    /// (`storage::with_deferred_fsync`).
    pub async fn flush_pending(&self) -> Result<(), EngineError> {
        Ok(self.storage.flush_pending().await?)
    }

    pub async fn create_bucket(&self, bucket: &str) -> Result<(), EngineError> {
        Ok(self.storage.create_bucket(bucket).await?)
    }

    /// Delete a real bucket on the storage backend (must be empty).
    pub async fn delete_bucket(&self, bucket: &str) -> Result<(), EngineError> {
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
}
