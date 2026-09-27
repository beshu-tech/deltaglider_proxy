// SPDX-License-Identifier: BUSL-1.1

//! Engine accessors, spool acquisition, codec permits and cache stats.

use super::*;

impl<S: StorageBackend> DeltaGliderEngine<S> {
    /// Access the underlying storage backend (for operations that bypass the delta engine)
    pub fn storage(&self) -> &S {
        &self.storage
    }

    /// Access the bucket policy registry (for quota checks, compression settings, etc.)
    pub fn bucket_policy_registry(&self) -> &crate::bucket_policy::BucketPolicyRegistry {
        &self.bucket_policies
    }

    /// Return a reference to the metadata cache (for handler-level access).
    pub fn metadata_cache(&self) -> &MetadataCache {
        &self.metadata_cache
    }

    /// Returns whether the xdelta3 CLI binary is available for legacy delta decoding.
    pub fn is_cli_available(&self) -> bool {
        self.codec.is_cli_available()
    }

    /// The installed xdelta3 version line (e.g. "Xdelta version 3.0.11..."), if any.
    pub fn cli_version(&self) -> Option<&str> {
        self.codec.cli_version()
    }

    /// Bytes above which a delta-eligible PUT routes through the streaming spool
    /// store (`store_spooled_delta`). Tied to `max_object_size`; overridable via
    /// `DGP_SPOOL_THRESHOLD_BYTES` (shared with the GET-side threshold).
    pub fn spool_store_threshold(&self) -> u64 {
        crate::config::env_parse_with_default("DGP_SPOOL_THRESHOLD_BYTES", self.max_object_size)
    }

    /// Whether `key`'s filename is delta-eligible (used by the adapter to decide
    /// the streaming-store route before constructing a spool).
    pub fn is_delta_eligible_key(&self, key: &str) -> bool {
        let filename = key.rsplit('/').next().unwrap_or(key);
        self.file_router.is_delta_eligible(filename)
    }

    /// Run a spool acquisition under the configured timeout, mapping a timeout to
    /// SlowDown (don't park the request + its budget forever under contention).
    /// The ONE place the timeout/Overloaded policy lives — both PUT/POST
    /// (`spool_acquire`) and GET (`spool_acquire_pair`) go through it.
    async fn with_spool_timeout<T, F>(fut: F) -> Result<T, EngineError>
    where
        F: std::future::Future<Output = std::io::Result<T>>,
    {
        Self::with_spool_timeout_io(fut).await?.map_err(|e| {
            // A holder refused a wait (hold-and-wait guard): retryable.
            if e.kind() == crate::deltaglider::spool::CONTENDED {
                EngineError::Overloaded(e.to_string())
            } else {
                EngineError::Storage(StorageError::from(e))
            }
        })
    }

    /// [`Self::with_spool_timeout`] that hands back the acquisition's own
    /// `io::Result`, for a caller that acts on its error kind.
    async fn with_spool_timeout_io<T, F>(fut: F) -> Result<std::io::Result<T>, EngineError>
    where
        F: std::future::Future<Output = std::io::Result<T>>,
    {
        let secs = crate::config::env_parse_with_default("DGP_SPOOL_ACQUIRE_TIMEOUT_SECS", 120u64);
        tokio::time::timeout(std::time::Duration::from_secs(secs), fut)
            .await
            .map_err(|_| {
                EngineError::Overloaded("spool budget exhausted; retry shortly".to_string())
            })
    }

    /// Acquire a spool file (timed). For the adapter to stage a large PUT/POST
    /// body before `store_spooled_delta`. Both ingest paths share it (B1.1).
    pub async fn spool_acquire(
        &self,
        bytes: u64,
    ) -> Result<crate::deltaglider::spool::Spool, EngineError> {
        Self::with_spool_timeout(self.spool.acquire(bytes)).await
    }

    /// Acquire a deadlock-safe spool PAIR (timed) — the GET reconstruct path.
    pub async fn spool_acquire_pair(
        &self,
        a: u64,
        b: u64,
    ) -> Result<
        (
            crate::deltaglider::spool::Spool,
            crate::deltaglider::spool::Spool,
        ),
        EngineError,
    > {
        Self::with_spool_timeout(self.spool.acquire_pair(a, b)).await
    }

    /// Reserve, BEFORE the deltaspace lock, the spool that a file-streaming
    /// storage write needs for its own temp files (the encrypting wrapper's
    /// ciphertext). Waiting for budget under the lock is hold-and-wait: a
    /// streaming PUT that holds its body spool can wait for the same lock.
    /// `held_mib`: spool budget the op holds already (its body spool, or its
    /// relay parts); a holder never waits. `None`: no spool needed.
    pub(crate) async fn reserve_storage_spool(
        &self,
        bucket: &str,
        bytes: u64,
        parts: bool,
        held_mib: usize,
    ) -> Result<Option<crate::deltaglider::spool::SpoolReservation>, EngineError> {
        let need = self
            .storage
            .file_put_spool_bytes(bucket, bytes, parts)
            .await;
        if need == 0 {
            return Ok(None);
        }
        Self::with_spool_timeout(self.spool.reserve_beside(held_mib, need))
            .await
            .map(Some)
    }

    /// The spool file for the buffered codec's source, taken WITHOUT waiting:
    /// the buffered PUT encodes under the deltaspace lock. A full budget is a
    /// retryable SlowDown.
    pub(crate) fn codec_source_spool_now(
        &self,
        bytes: usize,
    ) -> Result<crate::deltaglider::spool::Spool, EngineError> {
        self.spool.try_acquire(bytes as u64).map_err(|e| {
            if e.kind() == crate::deltaglider::spool::CONTENDED {
                EngineError::Overloaded(e.to_string())
            } else {
                EngineError::Storage(StorageError::from(e))
            }
        })
    }

    /// `spool_acquire` for an op that may already hold a spool (`held`).
    pub(crate) async fn spool_acquire_beside(
        &self,
        held: Option<&crate::deltaglider::spool::Spool>,
        bytes: u64,
    ) -> Result<crate::deltaglider::spool::Spool, EngineError> {
        Self::with_spool_timeout(self.spool.acquire_beside(held, bytes)).await
    }

    /// `spool_acquire_pair` for an op that already holds `held` (the streaming
    /// PUT's body spool): the pair is clamped so the op never waits for budget
    /// it holds itself. `None`: the budget is not free now, and a holder
    /// never waits (hold-and-wait deadlock, see `SpoolDir::reserve_within`);
    /// the caller goes on without the pair.
    pub(crate) async fn spool_acquire_pair_beside(
        &self,
        held: &crate::deltaglider::spool::Spool,
        a: u64,
        b: u64,
    ) -> Result<
        Option<(
            crate::deltaglider::spool::Spool,
            crate::deltaglider::spool::Spool,
        )>,
        EngineError,
    > {
        match Self::with_spool_timeout_io(self.spool.acquire_pair_beside(Some(held), a, b)).await? {
            Ok(pair) => Ok(Some(pair)),
            Err(e) if e.kind() == crate::deltaglider::spool::CONTENDED => Ok(None),
            Err(e) => Err(EngineError::Storage(StorageError::from(e))),
        }
    }

    /// Whether the codec passes `-a` (armor disabled) to xdelta3 (3.1+ only).
    pub fn codec_armor_disabled(&self) -> bool {
        self.codec.armor_disabled()
    }

    /// Returns the maximum object size in bytes.
    pub fn max_object_size(&self) -> u64 {
        self.max_object_size
    }

    /// Streaming-passthrough size ceiling (Phase B).
    pub fn max_passthrough_object_size(&self) -> u64 {
        self.max_passthrough_object_size
    }

    /// Encryption-mode label of the backend serving `bucket`
    /// (`transfer_plan::backend_supports_native_multipart` consumes this).
    pub fn multipart_storage_label(&self, bucket: &str) -> &'static str {
        self.storage.multipart_storage_label(bucket)
    }

    /// True when a streaming multipart copy to `bucket` stays memory-bounded:
    /// the backend must (a) NOT be a whole-object proxy-AES backend (the label
    /// gate) AND (b) write parts durably+incrementally (native multipart).
    /// A filesystem/buffering destination fails (b) and would otherwise retain
    /// the whole object in RAM on the "streaming" path — route it to spool.
    pub fn destination_supports_native_multipart(&self, bucket: &str) -> bool {
        crate::transfer_plan::backend_supports_native_multipart(
            self.storage.multipart_storage_label(bucket),
        ) && self.storage.supports_native_multipart(bucket)
    }

    /// True when a lite LIST of `bucket` carries trustworthy logical facts
    /// (real user_metadata + plaintext size/etag). False → parity must HEAD
    /// every key for ownership + logical size (S3, or an encrypting backend).
    pub fn lite_list_carries_logical_facts(&self, bucket: &str) -> bool {
        self.storage.lite_list_carries_logical_facts(bucket)
    }

    /// Return the number of entries in the reference cache (O(1) atomic read).
    pub fn cache_entry_count(&self) -> u64 {
        self.cache.entry_count()
    }

    /// Return the weighted size of the reference cache in bytes (O(1) atomic read).
    pub fn cache_weighted_size(&self) -> u64 {
        self.cache.weighted_size()
    }

    /// Return the configured maximum cache capacity in bytes.
    pub fn cache_max_capacity(&self) -> u64 {
        self.cache.max_capacity_bytes()
    }

    /// Return available codec semaphore permits.
    pub fn codec_available_permits(&self) -> usize {
        self.codec_semaphore.available_permits()
    }

    /// Borrow the metrics handle (None in tests). Lets transfer/replication
    /// code clone the `Arc<Metrics>` into part/object closures for counters.
    #[inline]
    pub fn metrics(&self) -> Option<&Arc<Metrics>> {
        self.metrics.as_ref()
    }

    /// Run a closure with the metrics if enabled (no-op in tests).
    #[inline]
    pub(super) fn with_metrics(&self, f: impl FnOnce(&Metrics)) {
        if let Some(m) = &self.metrics {
            f(m);
        }
    }

    /// Try to acquire a codec permit, returning `Overloaded` if all slots are busy.
    /// Use for PUT (fail fast — don't queue uploads holding large bodies in memory).
    pub(super) fn try_acquire_codec(
        &self,
    ) -> Result<tokio::sync::SemaphorePermit<'_>, EngineError> {
        self.codec_semaphore.try_acquire().map_err(|_| {
            EngineError::Overloaded("all delta codec slots busy — try again later".into())
        })
    }

    /// Wait for a codec permit with a timeout. Use for GET (users expect downloads to
    /// work even if they queue briefly behind other reconstructions).
    pub(super) async fn acquire_codec_timeout(
        &self,
        timeout: std::time::Duration,
    ) -> Result<tokio::sync::SemaphorePermit<'_>, EngineError> {
        match tokio::time::timeout(timeout, self.codec_semaphore.acquire()).await {
            Ok(Ok(permit)) => Ok(permit),
            Ok(Err(_closed)) => Err(EngineError::Overloaded("codec semaphore closed".into())),
            Err(_elapsed) => Err(EngineError::Overloaded(
                "timed out waiting for codec slot — server too busy".into(),
            )),
        }
    }
}
