// SPDX-License-Identifier: BUSL-1.1

//! S3 storage backend implementation using AWS SDK
//!
//! This backend stores metadata in S3 object metadata headers (x-amz-meta-dg-*)
//! for compatibility with the original DeltaGlider CLI (beshultd/deltaglider).
//!
//! Each API bucket maps 1:1 to a real S3 bucket on the backend.
//!
//! Layout (split 2026-09):
//!   - `mod.rs`      — `S3Backend`, its constructor, key helpers, the
//!     `StorageBackend` impl, and the request counters
//!   - `client.rs`   — client construction and the SSRF endpoint guard
//!   - `errors.rs`   — SDK error classification and write/delete verdicts
//!   - `metadata.rs` — DG metadata headers, self-copy plan, native SSE
//!   - `objects.rs`  — object reads, writes and deletes (fenced and plain)
//!   - `facts.rs`    — durable listing facts
//!   - `listing.rs`  — listing classification, HEAD enrichment, anchors
//!   - `tests.rs`    — unit tests

mod client;
mod errors;
mod facts;
mod listing;
mod metadata;
mod objects;
#[cfg(test)]
mod tests;
#[cfg(test)]
pub(crate) use tests::test_support;

pub(crate) use client::{backend_request_timeout, check_s3_endpoint, guard_s3_endpoint};
use errors::*;
pub(in crate::storage) use facts::put_facts_object;
use listing::*;
use metadata::*;
use objects::*;

use super::list_size_cache::{self, ListedSize, LogicalFacts, StoredObjectId};
use super::listing_facts;
use super::traits::{
    reference_fence_lost, BulkListing, DelegatedListResult, LiteScanResult, MultipartUpload,
    RefFence, RefWrite, StorageBackend, StorageError, StoredObject, UploadedPart,
};
use crate::config::BackendConfig;
use crate::types::{FileMetadata, StorageInfo};
use aws_credential_types::Credentials;
use aws_sdk_s3::config::BehaviorVersion;
use aws_sdk_s3::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures::stream::BoxStream;
use futures::StreamExt;
use std::collections::{HashMap, HashSet};

use tracing::{debug, instrument, warn};

/// Upstream `ListObjectsV2` pages fetched by `list_objects_delegated`'s loop.
/// Observable request-amplification counter (issue #82): a healthy anchored
/// listing costs ~1 page per served page; a subtree drain costs O(subtree).
/// Registered into the Prometheus registry by `Metrics::new()`; a static so
/// the backend needs no `Metrics` handle. Tests gate on counts, never
/// wall-clock (same doctrine as `replication_list_calls_total`).
pub static DELEGATED_LIST_UPSTREAM_PAGES: std::sync::LazyLock<prometheus::IntCounter> =
    std::sync::LazyLock::new(|| {
        prometheus::IntCounter::new(
            "deltaglider_delegated_list_upstream_pages_total",
            "Upstream ListObjectsV2 pages fetched by delegated listings",
        )
        .expect("valid metric")
    });

/// Every object HEAD request the proxy sends to S3: object metadata reads,
/// baseline existence checks, and the config-sync poll. Lets an operator see
/// a HEAD burst (the Hetzner 503 SlowDown RCA) and lets tests prove that a
/// path sends none. Every `head_object()` call in the server must increment
/// it (guarded by a source test).
pub static BACKEND_HEAD_REQUESTS: std::sync::LazyLock<prometheus::IntCounter> =
    std::sync::LazyLock::new(|| {
        prometheus::IntCounter::new(
            "deltaglider_backend_head_requests_total",
            "Object HEAD requests sent to S3 (storage backends and the config-sync bucket)",
        )
        .expect("valid metric")
    });

/// Exact-key confirmation probes issued after an anchored early exit
/// (`confirmable_candidates`). Bounded per page by the anchor's length.
pub static DELEGATED_LIST_PROBE_REQUESTS: std::sync::LazyLock<prometheus::IntCounter> =
    std::sync::LazyLock::new(|| {
        prometheus::IntCounter::new(
            "deltaglider_delegated_list_probe_requests_total",
            "Exact-key confirmation probes issued by delegated listings",
        )
        .expect("valid metric")
    });

/// Requests the proxy sends to S3 to keep the listing facts (see
/// `storage::listing_facts`), by kind: `list` (a LIST page reads the facts of
/// its stored keys), `put` (a PUT or a lazy backfill writes one), `delete`
/// (cleanup of an overwritten or deleted object's facts).
pub static LISTING_FACTS_REQUESTS: std::sync::LazyLock<prometheus::IntCounterVec> =
    std::sync::LazyLock::new(|| {
        prometheus::IntCounterVec::new(
            prometheus::Opts::new(
                "deltaglider_listing_facts_requests_total",
                "Requests sent to S3 to read, write or clean up listing facts",
            ),
            &["kind"],
        )
        .expect("valid metric")
    });

/// Listed objects whose listing facts a LIST looked up and did not find:
/// the entry keeps its stored size (a delta's `.delta`, a ciphertext) until
/// a HEAD backfills the facts.
pub static LISTING_FACTS_MISSES: std::sync::LazyLock<prometheus::IntCounter> =
    std::sync::LazyLock::new(|| {
        prometheus::IntCounter::new(
            "deltaglider_listing_facts_misses_total",
            "Listed objects whose listing facts were not found (the LIST shows the stored size)",
        )
        .expect("valid metric")
    });

/// S3 storage backend for DeltaGlider objects
/// Native S3 server-side encryption mode applied per PutObject.
///
/// Distinct from the proxy's `EncryptingBackend` wrapper (which does
/// AES-256-GCM in-process before the bytes reach the backend).
/// Native modes delegate encryption to AWS: the proxy sends the
/// appropriate headers, AWS encrypts on write, AWS decrypts on read
/// for callers with KMS permission.
///
/// Stamped onto the object's `dg-encrypted-native` user-metadata so
/// reads can distinguish "native-encrypted" from "proxy-encrypted"
/// from "plaintext" — only proxy-encrypted objects need the
/// `EncryptingBackend` decrypt pass; native ones come back already-
/// decrypted from the SDK and the wrapper's `dg-encrypted` marker
/// check is (correctly) false.
#[derive(Debug, Clone, PartialEq)]
pub enum NativeEncryptionConfig {
    /// No S3-side encryption headers — proxy-mode encryption or
    /// plaintext. Default.
    None,
    /// SSE-S3 (AES256, AWS-managed keys). No KMS cost; minimal
    /// control. Stamps `dg-encrypted-native: sse-s3`.
    SseS3,
    /// SSE-KMS with a specific KMS key ARN/alias. `bucket_key_enabled`
    /// enables S3 bucket keys to amortise KMS API calls on bursty
    /// traffic. Stamps `dg-encrypted-native: sse-kms`.
    SseKms {
        kms_key_id: String,
        bucket_key_enabled: bool,
    },
}

impl NativeEncryptionConfig {
    /// Short machine-readable marker value written to
    /// `dg-encrypted-native`. Matches what read-side sniffers look
    /// for. Returns `None` for the plaintext case — callers skip
    /// stamping entirely when no native encryption is configured.
    fn marker(&self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::SseS3 => Some("sse-s3"),
            Self::SseKms { .. } => Some("sse-kms"),
        }
    }
}

pub struct S3Backend {
    /// Client for requests without a large body: every operation is capped
    /// at [`backend_request_timeout`], so a hung backend fails fast.
    client: Client,
    /// Client for uploads and server-side copies (PutObject, UploadPart,
    /// CopyObject, CompleteMultipartUpload): their duration grows with the
    /// object size, so only the per-attempt and read timeouts apply.
    bulk_client: Client,
    /// `(backend name, definition fingerprint)` for passive health marking
    /// (`coordination::health::note_unavailable`). `None` for a backend that
    /// the engine did not name (tests, CLI).
    health_key: Option<(String, String)>,
    /// Per-backend native S3 server-side encryption mode. Applied to
    /// every `put_object`/`put_directory_marker` call.
    native_encryption: NativeEncryptionConfig,
    /// Scope of this backend's entries in the listing-size cache: the
    /// endpoint that holds the buckets (see `list_size_cache`).
    list_cache_scope: String,
    /// Batched removal of deleted objects' listing facts.
    facts_cleanup: super::facts_cleanup::FactsCleanupQueue,
}

impl S3Backend {
    /// Create a new S3 backend from configuration + native encryption
    /// policy. Pass `NativeEncryptionConfig::None` for plaintext or for
    /// backends using proxy-side AES-256-GCM (the `EncryptingBackend`
    /// wrapper handles those at a layer above us).
    pub async fn new(
        config: &BackendConfig,
        native_encryption: NativeEncryptionConfig,
    ) -> Result<Self, StorageError> {
        let client = Self::build_client_with(config, backend_request_timeout()).await?;
        let bulk_client = Self::build_client(config).await?;
        debug!(
            "S3Backend initialized (multi-bucket mode, native encryption: {:?})",
            native_encryption
        );
        let list_cache_scope = match config {
            BackendConfig::S3 {
                endpoint: Some(endpoint),
                ..
            } => endpoint.trim_end_matches('/').to_string(),
            BackendConfig::S3 { region, .. } => format!("aws:{region}"),
            _ => String::new(),
        };
        let facts_cleanup = super::facts_cleanup::FactsCleanupQueue::start(
            client.clone(),
            native_encryption.clone(),
        );
        Ok(Self {
            client,
            bulk_client,
            health_key: None,
            native_encryption,
            list_cache_scope,
            facts_cleanup,
        })
    }

    /// Name this backend for passive health marking: a request that finds
    /// it unavailable marks it unhealthy at once (the request gate then
    /// answers a fast 503), and error messages name it.
    pub fn with_health_name(mut self, name: &str, config: &BackendConfig) -> Self {
        self.health_key = Some((
            name.to_string(),
            crate::coordination::capability::fingerprint(config),
        ));
        self
    }

    // === Key generation helpers ===

    /// Join a prefix and filename into an S3 key, omitting the prefix if empty.
    fn prefixed_key(prefix: &str, filename: &str) -> String {
        if prefix.is_empty() {
            filename.to_string()
        } else {
            format!("{}/{}", prefix, filename)
        }
    }

    /// Get the S3 key for a reference file
    fn reference_key(&self, prefix: &str) -> String {
        Self::prefixed_key(prefix, "reference.bin")
    }

    /// Get the S3 key for a delta file
    fn delta_key(&self, prefix: &str, filename: &str) -> String {
        Self::prefixed_key(prefix, &format!("{}.delta", filename))
    }

    fn variant_key(
        &self,
        prefix: &str,
        filename: &str,
        variant: crate::storage::ObjectVariant,
    ) -> String {
        match variant {
            crate::storage::ObjectVariant::Delta => self.delta_key(prefix, filename),
            crate::storage::ObjectVariant::Passthrough => self.passthrough_key(prefix, filename),
        }
    }

    /// Get the S3 key for a passthrough file (stored with original filename, no suffix)
    fn passthrough_key(&self, prefix: &str, filename: &str) -> String {
        Self::prefixed_key(prefix, filename)
    }

    // === Metadata conversion helpers ===

    // === Internal helpers ===

    // (object_exists removed: it mapped every HEAD error to `false`, which is
    // the has_reference transient-error corruption bug. has_reference now
    // classifies the error directly — NotFound → absent, else propagate.)

    // === Listing classification helpers ===
    //
    // Both `bulk_list_objects` and `list_objects_delegated` need to:
    //   1. Classify raw S3 keys into user-visible objects vs internal files
    //   2. Fire parallel HEAD calls for delta files (listing size != original size)
    //   3. Build FileMetadata from HEAD results or listing fallback
    //   4. Dedup by user key, keeping the latest version
    //
    // These helpers centralise that logic so changes only need to happen once.
}

impl StorageBackend for S3Backend {
    // === Bucket operations ===

    #[instrument(skip(self))]
    async fn create_bucket(&self, bucket: &str) -> Result<(), StorageError> {
        let result = self.client.create_bucket().bucket(bucket).send().await;
        if let Err(e) = result {
            return match classify_create_bucket_conflict(bucket, e.code()) {
                Some(outcome) => outcome,
                None => Err(self.classify(bucket, &e, S3Op::CreateBucket)),
            };
        }
        debug!("Created S3 bucket: {}", bucket);
        Ok(())
    }

    /// A bucket declared in config is created at boot when the backend
    /// does not have it, as on the filesystem backend: its first write
    /// must not fail with NoSuchBucket. HEAD first, so a present bucket
    /// costs one request and a key without CreateBucket rights only warns
    /// when the bucket is really missing.
    async fn ensure_declared_bucket(&self, bucket: &str) -> Result<(), StorageError> {
        if self.head_bucket(bucket).await? {
            return Ok(());
        }
        self.create_bucket(bucket).await?;
        tracing::info!("created declared bucket '{bucket}' on its S3 backend");
        Ok(())
    }

    #[instrument(skip(self))]
    async fn delete_bucket(&self, bucket: &str) -> Result<(), StorageError> {
        // Listing facts are internal: they must not keep an empty bucket.
        self.purge_listing_facts_if_only_ones(bucket).await?;
        self.client
            .delete_bucket()
            .bucket(bucket)
            .send()
            .await
            .map_err(|e| self.classify(bucket, &e, S3Op::Other("delete_bucket")))?;
        debug!("Deleted S3 bucket: {}", bucket);
        Ok(())
    }

    #[instrument(skip(self))]
    async fn list_buckets(&self) -> Result<Vec<String>, StorageError> {
        let dated = self.list_buckets_with_dates().await?;
        Ok(dated.into_iter().map(|(name, _)| name).collect())
    }

    #[instrument(skip(self))]
    async fn list_buckets_with_dates(&self) -> Result<Vec<(String, DateTime<Utc>)>, StorageError> {
        let response = self
            .client
            .list_buckets()
            .send()
            .await
            // classify (not a bare S3(...)) so a 503 throttle surfaces as
            // Throttled → SlowDown, not a retry-storm-inducing 500.
            .map_err(|e| self.classify("", &e, S3Op::Other("list_buckets")))?;

        let mut buckets: Vec<(String, DateTime<Utc>)> = response
            .buckets()
            .iter()
            .filter_map(|b| {
                b.name().map(|n| {
                    let created = b
                        .creation_date()
                        .and_then(|d| {
                            let secs = d.secs();
                            let nanos = d.subsec_nanos();
                            chrono::DateTime::from_timestamp(secs, nanos)
                        })
                        .unwrap_or_else(Utc::now);
                    (n.to_string(), created)
                })
            })
            .collect();
        buckets.sort_by(|a, b| a.0.cmp(&b.0));
        debug!("Listed {} S3 buckets", buckets.len());
        Ok(buckets)
    }

    #[instrument(skip(self))]
    async fn head_bucket(&self, bucket: &str) -> Result<bool, StorageError> {
        match self.client.head_bucket().bucket(bucket).send().await {
            Ok(_) => Ok(true),
            // Only a genuine bucket-404 → absent. A 503 SlowDown / timeout /
            // 5xx must NOT read as absent — routing uses this to place a bucket
            // on a backend; a transient error must not silently reroute writes
            // to the wrong (default) backend. (Same class as has_reference.)
            Err(e) => match self.classify(bucket, &e, S3Op::HeadBucket) {
                StorageError::BucketNotFound(_) | StorageError::NotFound(_) => Ok(false),
                other => Err(other),
            },
        }
    }

    // === Reference file operations ===

    #[instrument(skip(self, data, metadata))]
    async fn put_reference(
        &self,
        bucket: &str,
        prefix: &str,
        data: &[u8],
        metadata: &FileMetadata,
        proof: &crate::deltaglider::RefWriteProof,
    ) -> Result<(), StorageError> {
        let key = self.reference_key(prefix);
        self.put_object_with_metadata(bucket, &key, data, metadata)
            .await?;
        debug!(
            "Stored reference for {}/{} ({} bytes)",
            bucket,
            prefix,
            data.len()
        );
        Ok(())
    }

    async fn put_reference_from_file(
        &self,
        bucket: &str,
        prefix: &str,
        source_path: &std::path::Path,
        metadata: &FileMetadata,
        _proof: &crate::deltaglider::RefWriteProof,
    ) -> Result<(), StorageError> {
        // Stream the file body to S3 (ByteStream::from_path) — never heap-load it.
        let key = self.reference_key(prefix);
        self.put_object_file_with_metadata(bucket, &key, source_path, metadata)
            .await?;
        debug!("Stored reference from file for {}/{}", bucket, prefix);
        Ok(())
    }

    #[instrument(skip(self, metadata))]
    async fn put_reference_metadata(
        &self,
        bucket: &str,
        prefix: &str,
        metadata: &FileMetadata,
        proof: &crate::deltaglider::RefWriteProof,
    ) -> Result<(), StorageError> {
        let key = self.reference_key(prefix);
        self.replace_metadata_in_place(bucket, &key, metadata)
            .await?;
        debug!("Updated reference metadata for {}/{}", bucket, prefix);
        Ok(())
    }

    #[instrument(skip(self, metadata))]
    async fn put_passthrough_metadata(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        let key = self.passthrough_key(prefix, filename);
        self.replace_metadata_in_place(bucket, &key, metadata)
            .await?;
        debug!("Updated passthrough metadata for {}/{}", bucket, key);
        Ok(())
    }

    #[instrument(skip(self))]
    async fn get_reference(&self, bucket: &str, prefix: &str) -> Result<Vec<u8>, StorageError> {
        let key = self.reference_key(prefix);
        self.get_object(bucket, &key).await
    }

    async fn get_reference_to_file(
        &self,
        bucket: &str,
        prefix: &str,
        dest: &std::path::Path,
    ) -> Result<u64, StorageError> {
        use tokio::io::AsyncWriteExt;
        let key = self.reference_key(prefix);
        // Stream the GET body straight to the dest file — never collect the
        // (possibly multi-GB) reference into a Vec (blocker 10).
        let response = self
            .client
            .get_object()
            .bucket(bucket)
            .key(&key)
            .send()
            .await
            .map_err(|e| self.classify_get(bucket, &key, &e))?;

        let mut stream = Self::s3_body_to_stream(response.body);
        let mut file = tokio::fs::File::create(dest).await?;
        let mut written: u64 = 0;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            file.write_all(&chunk).await?;
            written += chunk.len() as u64;
        }
        file.flush().await?;
        debug!(
            "S3 GET reference→file {}/{} ({} bytes)",
            bucket, key, written
        );
        Ok(written)
    }

    #[instrument(skip(self))]
    async fn get_reference_metadata(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<FileMetadata, StorageError> {
        let key = self.reference_key(prefix);
        self.get_object_metadata(bucket, &key).await
    }

    #[instrument(skip(self))]
    async fn has_reference(&self, bucket: &str, prefix: &str) -> Result<bool, StorageError> {
        let key = self.reference_key(prefix);
        BACKEND_HEAD_REQUESTS.inc();
        match self
            .client
            .head_object()
            .bucket(bucket)
            .key(&key)
            .send()
            .await
        {
            Ok(_) => Ok(true),
            // A genuine object-level 404 → absent. Anything else (503 SlowDown,
            // timeout, 5xx, connection reset) must NOT read as absent — else a
            // write path overwrites a live reference.bin on a backend hiccup.
            Err(e) => match self.classify(bucket, &e, S3Op::HeadObject) {
                StorageError::NotFound(_) => Ok(false),
                other => Err(other),
            },
        }
    }

    /// A PUT is durable when S3 answers it: nothing is deferred.
    async fn flush_pending(&self) -> Result<(), StorageError> {
        Ok(())
    }

    #[instrument(skip(self))]
    async fn delete_reference(
        &self,
        bucket: &str,
        prefix: &str,
        proof: &crate::deltaglider::RefWriteProof,
    ) -> Result<(), StorageError> {
        let key = self.reference_key(prefix);
        self.delete_s3_object(bucket, &key).await?;
        debug!("Deleted reference for {}/{}", bucket, prefix);
        Ok(())
    }

    #[instrument(skip(self))]
    async fn reference_fence(&self, bucket: &str, prefix: &str) -> Result<RefFence, StorageError> {
        let key = self.reference_key(prefix);
        BACKEND_HEAD_REQUESTS.inc();
        match self
            .client
            .head_object()
            .bucket(bucket)
            .key(&key)
            .send()
            .await
        {
            Ok(head) => Ok(fence_from_head_etag(head.e_tag(), bucket, &key)),
            // Same contract as has_reference: only a real 404 is "absent".
            Err(e) => match self.classify(bucket, &e, S3Op::HeadObject) {
                StorageError::NotFound(_) => Ok(RefFence::Absent),
                other => Err(other),
            },
        }
    }

    #[instrument(skip(self, op))]
    async fn write_reference_fenced(
        &self,
        bucket: &str,
        prefix: &str,
        op: RefWrite<'_>,
        fence: &RefFence,
        proof: &crate::deltaglider::RefWriteProof,
    ) -> Result<RefFence, StorageError> {
        let key = self.reference_key(prefix);
        let etag = match op {
            RefWrite::Put { data, metadata } => {
                self.put_object_with_metadata_fenced(bucket, &key, data, metadata, fence)
                    .await?
            }
            RefWrite::PutFile { path, metadata } => {
                self.put_object_file_with_metadata_fenced(bucket, &key, path, metadata, fence)
                    .await?
            }
            RefWrite::Metadata { metadata } => {
                self.replace_metadata_in_place_fenced(bucket, &key, metadata, fence)
                    .await?
            }
            RefWrite::Delete => {
                self.delete_s3_object_fenced(bucket, &key, fence).await?;
                return Ok(RefFence::Absent);
            }
        };
        // No ETag in the response: the next write of this hold is unfenced
        // rather than fenced on a value that can never match.
        Ok(etag.map(RefFence::ETag).unwrap_or(RefFence::Unfenced))
    }

    // === Delta file operations ===

    #[instrument(skip(self, data, metadata))]
    async fn put_delta(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        data: &[u8],
        metadata: &FileMetadata,
        proof: &crate::deltaglider::RefWriteProof,
    ) -> Result<(), StorageError> {
        let key = self.delta_key(prefix, filename);
        self.put_object_with_metadata(bucket, &key, data, metadata)
            .await?;
        debug!(
            "Stored delta for {}/{}/{} ({} bytes)",
            bucket,
            prefix,
            filename,
            data.len()
        );
        Ok(())
    }

    #[instrument(skip(self))]
    async fn get_delta(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<Vec<u8>, StorageError> {
        let key = self.delta_key(prefix, filename);
        self.get_object(bucket, &key).await
    }

    #[instrument(skip(self))]
    async fn get_delta_metadata(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<FileMetadata, StorageError> {
        let key = self.delta_key(prefix, filename);
        self.get_object_metadata(bucket, &key).await
    }

    #[instrument(skip(self))]
    async fn delete_delta(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<(), StorageError> {
        let key = self.delta_key(prefix, filename);
        let at = self.delete_s3_object_dated(bucket, &key).await?;
        self.facts_cleanup.enqueue(bucket, &key, at);
        debug!("Deleted delta for {}/{}/{}", bucket, prefix, filename);
        Ok(())
    }

    // === Passthrough file operations (stored with original filename) ===

    #[instrument(skip(self, data, metadata))]
    async fn put_passthrough(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        let key = self.passthrough_key(prefix, filename);
        self.put_object_with_metadata(bucket, &key, data, metadata)
            .await?;
        debug!(
            "Stored passthrough for {}/{}/{} ({} bytes)",
            bucket,
            prefix,
            filename,
            data.len()
        );
        Ok(())
    }

    #[instrument(skip(self, metadata, _spool))]
    async fn put_passthrough_file(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        source_path: &std::path::Path,
        metadata: &FileMetadata,
        _spool: crate::deltaglider::spool::SpoolBudget<'_>,
    ) -> Result<(), StorageError> {
        let key = self.passthrough_key(prefix, filename);
        self.put_object_file_with_metadata(bucket, &key, source_path, metadata)
            .await?;
        debug!(
            "Stored passthrough from file for {}/{}/{} ({:?})",
            bucket, prefix, filename, source_path
        );
        Ok(())
    }

    #[instrument(skip(self))]
    async fn get_passthrough(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<Vec<u8>, StorageError> {
        let key = self.passthrough_key(prefix, filename);
        self.get_object(bucket, &key).await
    }

    #[instrument(skip(self))]
    async fn get_passthrough_metadata(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<FileMetadata, StorageError> {
        let key = self.passthrough_key(prefix, filename);
        self.get_object_metadata(bucket, &key).await
    }

    #[instrument(skip(self))]
    async fn delete_passthrough(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<(), StorageError> {
        let key = self.passthrough_key(prefix, filename);
        let at = self.delete_s3_object_dated(bucket, &key).await?;
        // A plain passthrough has facts too when its stored ETag is not the
        // logical one (a proxy-assembled multipart upload), and a ciphertext
        // always: queue the cleanup for every passthrough delete.
        self.facts_cleanup.enqueue(bucket, &key, at);
        debug!("Deleted passthrough for {}/{}/{}", bucket, prefix, filename);
        Ok(())
    }

    async fn variant_version(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        variant: crate::storage::ObjectVariant,
    ) -> Result<Option<String>, StorageError> {
        let key = self.variant_key(prefix, filename, variant);
        BACKEND_HEAD_REQUESTS.inc();
        let head = self
            .client
            .head_object()
            .bucket(bucket)
            .key(&key)
            .send()
            .await
            .map_err(|e| Self::classify_s3_error(bucket, &e, S3Op::HeadObject))?;
        Ok(head.e_tag().map(str::to_string))
    }

    /// `DeleteObject` with `If-Match`. A 412/409 means a peer overwrote it:
    /// nothing deleted. A backend that answers 501 to a conditional delete
    /// gets a plain one (the in-process lock is then the only guard).
    async fn delete_variant_if(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        variant: crate::storage::ObjectVariant,
        version: &str,
    ) -> Result<bool, StorageError> {
        let key = self.variant_key(prefix, filename, variant);
        let date = crate::coordination::server_clock::ServerDate::default();
        let sent = self
            .client
            .delete_object()
            .bucket(bucket)
            .key(&key)
            .if_match(version)
            .customize()
            .interceptor(date.clone())
            .send()
            .await;
        match sent {
            Ok(_) => {}
            Err(e) => {
                match conditional_delete_verdict(&crate::coordination::cas::sdk_error_signal(&e)) {
                    ConditionalDeleteVerdict::Changed => return Ok(false),
                    ConditionalDeleteVerdict::Unsupported => {
                        let at = self.delete_s3_object_dated(bucket, &key).await?;
                        self.facts_cleanup.enqueue(bucket, &key, at);
                        return Ok(true);
                    }
                    ConditionalDeleteVerdict::Other => {
                        return match Self::classify_s3_error(bucket, &e, S3Op::DeleteObject) {
                            StorageError::NotFound(_) => Ok(false),
                            other => Err(other),
                        }
                    }
                }
            }
        }
        self.facts_cleanup.enqueue(bucket, &key, date.get());
        debug!("Deleted {key} in {bucket} if still {version}");
        Ok(true)
    }

    // === Streaming operations ===

    #[instrument(skip(self))]
    async fn get_passthrough_stream(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<BoxStream<'static, Result<Bytes, StorageError>>, StorageError> {
        let key = self.passthrough_key(prefix, filename);
        let response = self
            .client
            .get_object()
            .bucket(bucket)
            .key(&key)
            .send()
            .await
            .map_err(|e| self.classify_get(bucket, &key, &e))?;

        debug!("S3 GET stream {}/{}", bucket, key);

        Ok(Box::pin(Self::s3_body_to_stream(response.body)))
    }

    /// One GET: the body, and the metadata from the response's own headers.
    async fn open_object(
        &self,
        bucket: &str,
        prefix: &str,
        object: StoredObject<'_>,
    ) -> Result<(super::ByteStream, FileMetadata), StorageError> {
        let key = match object {
            StoredObject::Reference => self.reference_key(prefix),
            StoredObject::Delta(f) => self.delta_key(prefix, f),
            StoredObject::Passthrough(f) => self.passthrough_key(prefix, f),
        };
        let response = self
            .client
            .get_object()
            .bucket(bucket)
            .key(&key)
            .send()
            .await
            .map_err(|e| self.classify_get(bucket, &key, &e))?;
        let meta = self.metadata_of_response(
            bucket,
            &key,
            &ObjectHeaders {
                user: response.metadata(),
                last_modified: response.last_modified(),
                e_tag: response.e_tag(),
                content_length: response.content_length(),
                content_type: response.content_type(),
            },
        );
        debug!("S3 GET stream {}/{} (with its metadata)", bucket, key);
        Ok((Self::s3_body_to_stream(response.body), meta))
    }

    #[instrument(skip(self))]
    async fn get_passthrough_stream_range(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        start: u64,
        end: u64,
    ) -> Result<(BoxStream<'static, Result<Bytes, StorageError>>, u64), StorageError> {
        let key = self.passthrough_key(prefix, filename);
        let range_header = format!("bytes={}-{}", start, end);
        let response = self
            .client
            .get_object()
            .bucket(bucket)
            .key(&key)
            .range(&range_header)
            .send()
            .await
            .map_err(|e| self.classify_get(bucket, &key, &e))?;

        let content_length = response.content_length.unwrap_or(0) as u64;
        debug!(
            "S3 GET range stream {}/{} ({}, {} bytes)",
            bucket, key, range_header, content_length
        );

        Ok((
            Box::pin(Self::s3_body_to_stream(response.body)),
            content_length,
        ))
    }

    // === Multipart upload (Phase B native streaming copy) ===

    fn supports_native_multipart(&self, _bucket: &str) -> bool {
        true
    }

    fn lite_list_carries_logical_facts(&self, _bucket: &str) -> bool {
        // S3 LIST returns NO user metadata (so replication-provenance ownership
        // can't be read from a lite entry) — parity must HEAD to know ownership.
        false
    }

    #[instrument(skip(self, metadata))]
    async fn create_multipart_upload(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        metadata: &FileMetadata,
    ) -> Result<MultipartUpload, StorageError> {
        let key = self.passthrough_key(prefix, filename);
        let mut headers = self.metadata_to_headers(metadata);
        if let Some(marker) = self.native_encryption.marker() {
            headers.insert("dg-encrypted-native".to_string(), marker.to_string());
        }
        check_metadata_size(&headers, bucket, &key)?;

        let mut request = self
            .client
            .create_multipart_upload()
            .bucket(bucket)
            .key(&key)
            .content_type("application/octet-stream");
        for (k, v) in &headers {
            request = request.metadata(k.clone(), v.clone());
        }
        request = apply_native_encryption_mpu(request, &self.native_encryption);

        let resp = request
            .send()
            .await
            .map_err(|e| self.classify(bucket, &e, S3Op::CreateMpu))?;
        let upload_id = resp.upload_id().ok_or_else(|| {
            StorageError::S3(format!(
                "create_multipart_upload returned no upload id for {}/{}",
                bucket, key
            ))
        })?;
        Ok(MultipartUpload {
            bucket: bucket.to_string(),
            upload_id: upload_id.to_string(),
            native: true,
            backend: None,
        })
    }

    #[instrument(skip(self, upload, data))]
    async fn upload_part(
        &self,
        upload: &MultipartUpload,
        prefix: &str,
        filename: &str,
        part_number: i32,
        data: Bytes,
    ) -> Result<UploadedPart, StorageError> {
        let key = self.passthrough_key(prefix, filename);
        let backoff_ms = [100u64, 200, 400];
        for attempt in 0..=backoff_ms.len() {
            let body = ByteStream::from(data.clone());
            let result = self
                .bulk_client
                .upload_part()
                .bucket(&upload.bucket)
                .key(&key)
                .upload_id(&upload.upload_id)
                .part_number(part_number)
                .body(body)
                .send()
                .await;
            match result {
                Ok(resp) => {
                    let etag = resp.e_tag().unwrap_or_default().to_string();
                    return Ok(UploadedPart { part_number, etag });
                }
                Err(e) => {
                    let is_retryable = if let SdkError::ServiceError(ref svc) = e {
                        let status = svc.raw().status().as_u16();
                        status == 400 || status == 500 || status == 503
                    } else {
                        matches!(e, SdkError::DispatchFailure(_) | SdkError::TimeoutError(_))
                    };
                    if is_retryable && attempt < backoff_ms.len() {
                        warn!(
                            "S3 upload_part {}/{} part {} failed (attempt {}), retrying in {}ms: {:?}",
                            upload.bucket, key, part_number, attempt + 1, backoff_ms[attempt], e
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(backoff_ms[attempt]))
                            .await;
                        continue;
                    }
                    return Err(self.classify(&upload.bucket, &e, S3Op::UploadPart));
                }
            }
        }
        unreachable!("retry loop must return on every path")
    }

    #[instrument(skip(self, upload, parts, _assembled, _metadata))]
    async fn complete_multipart_upload(
        &self,
        upload: &MultipartUpload,
        prefix: &str,
        filename: &str,
        parts: &[UploadedPart],
        _assembled: &[Bytes],
        _metadata: &FileMetadata,
    ) -> Result<String, StorageError> {
        use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
        let key = self.passthrough_key(prefix, filename);
        let completed_parts: Vec<CompletedPart> = parts
            .iter()
            .map(|p| {
                CompletedPart::builder()
                    .part_number(p.part_number)
                    .e_tag(p.etag.clone())
                    .build()
            })
            .collect();
        let completed = CompletedMultipartUpload::builder()
            .set_parts(Some(completed_parts))
            .build();
        let resp = self
            .bulk_client
            .complete_multipart_upload()
            .bucket(&upload.bucket)
            .key(&key)
            .upload_id(&upload.upload_id)
            .multipart_upload(completed)
            .send()
            .await
            .map_err(|e| self.classify(&upload.bucket, &e, S3Op::CompleteMpu))?;
        // The S3 multipart ETag is `<md5-of-concatenated-part-md5s>-<n>`.
        let etag = resp
            .e_tag()
            .map(|e| e.trim_matches('"').to_string())
            .unwrap_or_default();
        Ok(etag)
    }

    #[instrument(skip(self, upload))]
    async fn abort_multipart_upload(
        &self,
        upload: &MultipartUpload,
        prefix: &str,
        filename: &str,
    ) -> Result<(), StorageError> {
        let key = self.passthrough_key(prefix, filename);
        self.client
            .abort_multipart_upload()
            .bucket(&upload.bucket)
            .key(&key)
            .upload_id(&upload.upload_id)
            .send()
            .await
            .map_err(|e| self.classify(&upload.bucket, &e, S3Op::AbortMpu))?;
        Ok(())
    }

    // === Scanning operations ===

    #[instrument(skip(self))]
    async fn scan_deltaspace(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<Vec<FileMetadata>, StorageError> {
        let listed = self.list_deltaspace_eligible(bucket, prefix).await?;

        // For delta files, ListObjectsV2 Size is the delta size, not the
        // original. HEAD each one (bounded parallel) to recover the real
        // original size from user-metadata. Passthrough and reference
        // entries: listing Size == real file size, no HEAD needed.
        let delta_keys: Vec<String> = listed
            .iter()
            .filter(|obj| obj.key.ends_with(".delta"))
            .map(|obj| obj.key.clone())
            .collect();
        // Deltaspace scans want completeness (savings accounting), so no
        // throttle bail here — the SDK's adaptive rate limiter paces the
        // sweep instead. The count is logged for observability only.
        let (head_results, throttled) = self
            .bounded_head_calls(bucket, delta_keys.iter().map(|k| k.as_str()))
            .await;
        if throttled > 0 {
            warn!("deltaspace scan of {bucket}: {throttled} HEADs throttled (503 SlowDown)");
        }

        // `head_results` only contains the delta keys we HEAD'd above —
        // non-delta entries fall through to the no-HEAD builder.
        let metadata_list: Vec<FileMetadata> = listed
            .into_iter()
            .map(|obj| {
                if let Some(head_meta) = head_results.get(&obj.key) {
                    head_meta.clone()
                } else {
                    Self::lite_metadata_from_listed(&obj)
                }
            })
            .collect();

        debug!(
            "Scanned {} objects in deltaspace {}/{}",
            metadata_list.len(),
            bucket,
            prefix
        );
        Ok(metadata_list)
    }

    /// HEAD-free variant. Suitable for diagnostics callers that only
    /// need delta sizes (which are already in the listing).
    ///
    /// PERF: For a bucket with 141 prefixes × ~500 deltas, the regular
    /// `scan_deltaspace` fires ~70k HEAD calls. This variant fires
    /// zero HEADs and is ~300× faster end-to-end.
    ///
    /// Reports `originals_estimated: true` because, without HEAD, we
    /// don't recover the original-file size of `.delta` entries — the
    /// `file_size` field on delta `FileMetadata` is the on-disk delta
    /// size in this shape. Callers MUST honour the flag and suppress
    /// "savings" / "original total" displays.
    async fn scan_deltaspace_lite(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<LiteScanResult, StorageError> {
        let listed = self.list_deltaspace_eligible(bucket, prefix).await?;
        let metadata: Vec<FileMetadata> =
            listed.iter().map(Self::lite_metadata_from_listed).collect();
        debug!(
            "Lite-scanned {} objects in deltaspace {}/{}",
            metadata.len(),
            bucket,
            prefix
        );
        Ok(LiteScanResult {
            metadata,
            originals_estimated: true,
        })
    }

    #[instrument(skip(self))]
    async fn list_deltaspaces(&self, bucket: &str) -> Result<Vec<String>, StorageError> {
        let keys = self.list_objects_with_prefix(bucket, "").await?;
        let mut prefixes = HashSet::new();

        for key in keys {
            // Every file in the bucket belongs to a deltaspace.
            // Delta files end with .delta, references are reference.bin,
            // passthrough files keep their original names.
            if let Some(idx) = key.rfind('/') {
                let prefix = &key[..idx];
                prefixes.insert(prefix.to_string());
            } else {
                prefixes.insert(String::new());
            }
        }

        let result: Vec<String> = prefixes.into_iter().collect();
        debug!("Found {} deltaspaces in bucket {}", result.len(), bucket);
        Ok(result)
    }

    #[instrument(skip(self))]
    async fn total_size(&self, bucket: Option<&str>) -> Result<u64, StorageError> {
        let buckets_to_scan = if let Some(b) = bucket {
            vec![b.to_string()]
        } else {
            self.list_buckets().await?
        };

        let mut total = 0u64;
        for b in &buckets_to_scan {
            let objects = self.list_objects_full(b, "").await?;
            total += objects.iter().map(|o| o.size).sum::<u64>();
        }

        debug!("Total S3 storage size: {} bytes", total);
        Ok(total)
    }

    /// Enrich listed objects with full metadata from bounded HEAD calls.
    /// Maps user-visible keys back to actual S3 keys (appending `.delta` for
    /// delta files) and fires parallel HEAD requests with concurrency control.
    async fn enrich_list_metadata(
        &self,
        bucket: &str,
        objects: Vec<(String, FileMetadata)>,
    ) -> Result<Vec<(String, FileMetadata)>, StorageError> {
        // Build a mapping from S3 key -> user key so we can HEAD the right
        // objects and map results back.
        let s3_keys: Vec<String> = objects
            .iter()
            .map(|(user_key, meta)| {
                if meta.is_delta() {
                    // Delta files are stored with .delta suffix
                    let obj = crate::types::ObjectKey::parse("_", user_key);
                    let prefix = obj.prefix;
                    let filename = obj.filename;
                    if prefix.is_empty() {
                        format!("{}.delta", filename)
                    } else {
                        format!("{}/{}.delta", prefix, filename)
                    }
                } else {
                    user_key.clone()
                }
            })
            .collect();

        // Partition the HEADs into batches and stop the sweep if a batch comes
        // back mostly throttled — the remaining objects fall back to their
        // (perfectly serviceable) lite listing metadata instead of bombing a
        // backend that is already answering 503 SlowDown. Enrichment here is
        // cosmetic (original sizes for delta entries in listings), never worth
        // grinding a struggling backend for.
        let mut head_results: HashMap<String, FileMetadata> = HashMap::new();
        for batch in s3_keys.chunks(Self::MAX_CONCURRENT_HEADS) {
            let (ok, throttled) = self
                .bounded_head_calls(bucket, batch.iter().map(|s| s.as_str()))
                .await;
            head_results.extend(ok);
            if throttled * 2 >= batch.len() {
                warn!(
                    "HEAD enrichment aborted for {}: {}/{} throttled in one batch — \
                     serving lite metadata for the remaining {} objects",
                    bucket,
                    throttled,
                    batch.len(),
                    s3_keys.len().saturating_sub(head_results.len()),
                );
                break;
            }
        }

        let enriched: Vec<(String, FileMetadata)> = objects
            .into_iter()
            .zip(s3_keys.iter())
            .map(|((user_key, fallback_meta), s3_key)| {
                if let Some(head_meta) = head_results.get(s3_key) {
                    (user_key, head_meta.clone())
                } else {
                    (user_key, fallback_meta)
                }
            })
            .collect();

        debug!(
            "Enriched {} objects with HEAD metadata in {}",
            enriched.len(),
            bucket
        );
        Ok(enriched)
    }

    async fn bulk_list_objects(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<Vec<(String, FileMetadata)>, StorageError> {
        Ok(self
            .bulk_list_objects_with_baselines(bucket, prefix)
            .await?
            .objects)
    }

    async fn bulk_list_objects_with_baselines(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<BulkListing, StorageError> {
        let listed = self.list_objects_full(bucket, prefix).await?;
        let listing = Self::classify_listed_objects(listed);

        // Build FileMetadata from LIST data only — no HEAD calls. A delta
        // entry is a stub carrying its STORED size and ETag;
        // `resolve_listed_sizes` swaps in the logical ones when the
        // listing-size cache knows this exact stored object. Full metadata
        // (storage type, SHA) is fetched by HEAD only where a caller asks for
        // it (metadata=true listings, the inspector).
        let objects = Self::resolve_classified_lite(listing.classified, listing.dir_markers);

        debug!(
            "Bulk listed {} objects + {} baselines (lite, no HEAD) in {}/{}",
            objects.len(),
            listing.baselines.len(),
            bucket,
            prefix
        );
        Ok(BulkListing {
            objects,
            baselines: listing.baselines,
        })
    }

    async fn resolve_listed_sizes(
        &self,
        bucket: &str,
        objects: &mut [(String, FileMetadata)],
        passthrough_may_differ: bool,
    ) -> Vec<ListedSize> {
        let mut sizes: Vec<ListedSize> = objects
            .iter_mut()
            .map(|(key, meta)| Self::resolve_one_listed(&self.list_cache_scope, bucket, key, meta))
            .collect();
        // What the process cache did not know, the durable facts may.
        let candidates: Vec<(usize, String)> = objects
            .iter()
            .zip(&sizes)
            .enumerate()
            .filter_map(|(i, ((key, meta), size))| match size {
                ListedSize::StoredOnly => Some((i, format!("{key}.delta"))),
                ListedSize::Listed
                    if passthrough_may_differ && !key.ends_with('/') && !meta.is_delta() =>
                {
                    Some((i, key.clone()))
                }
                _ => None,
            })
            .collect();
        if !candidates.is_empty() {
            for i in self
                .apply_durable_listing_facts(bucket, objects, &candidates)
                .await
            {
                sizes[i] = ListedSize::Cached;
            }
        }
        sizes
    }

    /// Optimised listing that delegates delimiter collapsing to upstream S3.
    ///
    /// Instead of fetching *every* object and collapsing in-memory, we ask S3
    /// to handle the delimiter, which means S3 returns CommonPrefixes directly
    /// and only the objects at the current level appear in Contents.
    #[instrument(skip(self))]
    async fn list_objects_delegated(
        &self,
        bucket: &str,
        prefix: &str,
        delimiter: Option<&str>,
        max_keys: u32,
        continuation_token: Option<&str>,
    ) -> Result<Option<DelegatedListResult>, StorageError> {
        // We need to over-fetch from upstream because internal files
        // (reference.bin, .delta suffixes) inflate the key count — but only
        // until the page is provably complete (see `list_anchor` below), not
        // until the prefix is exhausted (the old behaviour materialised the
        // whole subtree on delimiter-less lists — the audit's memory HIGH).
        let mut all_common_prefixes = std::collections::BTreeSet::new();
        let mut raw_objects: Vec<S3ListedObject> = Vec::new();
        let mut upstream_token: Option<String> = None;
        let mut first_page = true;
        // Set when the loop stops early at the anchor. The bounded set of raw
        // keys that can still sort above it is confirmed after the loop.
        let mut settled_anchor: Option<String> = None;
        let mut skip_to: Option<&'static str> = None;

        // When the engine gives us a continuation_token it's a *user-visible* key.
        // We use start_after to skip past it on upstream S3.
        let start_after = continuation_token.map(|s| s.to_string());

        loop {
            let mut request = self
                .client
                .list_objects_v2()
                .bucket(bucket)
                .prefix(prefix)
                .set_delimiter(delimiter.map(String::from));

            // On the first page use start_after; on subsequent pages use
            // the upstream continuation token (or jump past the facts).
            if first_page {
                if let Some(ref sa) = start_after {
                    request = request.start_after(sa);
                }
                first_page = false;
            } else if let Some(past) = skip_to.take() {
                request = request.start_after(past);
            } else if let Some(ref token) = upstream_token {
                request = request.continuation_token(token);
            }

            let response = request
                .send()
                .await
                .map_err(|e| self.classify(bucket, &e, S3Op::ListObjects))?;
            DELEGATED_LIST_UPSTREAM_PAGES.inc();

            // Collect CommonPrefixes, skipping ONLY the `.dg/` internal deltaspace
            // directory (never a user-visible key). Deliberately narrow: an
            // earlier version skipped ANY dot-prefixed segment, which also hid
            // legitimate folders like `.well-known/` from user-facing listings.
            if let Some(cps) = response.common_prefixes {
                for cp in cps {
                    if let Some(p) = cp.prefix {
                        let last_seg = p.trim_end_matches('/').rsplit('/').next().unwrap_or("");
                        if last_seg == ".dg" || listing_facts::is_internal_common_prefix(&p) {
                            continue;
                        }
                        all_common_prefixes.insert(p);
                    }
                }
            }

            // Collect direct objects at this level
            let page_last = last_listed_key(response.contents.as_deref());
            if let Some(contents) = response.contents {
                raw_objects.extend(
                    contents
                        .into_iter()
                        .filter_map(S3ListedObject::from_s3_object),
                );
            }

            if response.is_truncated.unwrap_or(false) {
                upstream_token = response.next_continuation_token;
                skip_to = page_last
                    .as_deref()
                    .and_then(listing_facts::skip_past_facts);
            } else {
                break;
            }

            // Early exit: stop fetching once we are past the anchor — the
            // (max_keys+1)-th user key — instead of draining the whole prefix
            // into memory. The few raw keys that can still sort above the
            // anchor and belong on the page are confirmed after the loop.
            if let Some(anchor) = list_anchor(
                raw_objects.iter().map(|o| o.key.as_str()),
                all_common_prefixes.iter().map(|p| p.as_str()),
                max_keys,
                continuation_token,
            ) {
                let past_anchor = raw_objects
                    .last()
                    .map(|o| o.key.as_str() > anchor.as_str())
                    .unwrap_or(false);
                if past_anchor {
                    settled_anchor = Some(anchor);
                    break;
                }
            }
        }

        // The loop stopped at the anchor, so a raw key of the form
        // `p + ".delta"` (p a proper prefix of the anchor) that sorts above the
        // anchor may not have been read yet, and each one still belongs on the
        // page. Confirm that bounded set with one exact request each — reading
        // forward to their maximum instead is what made a delimiter-less
        // listing walk the whole subtree (issue #82). `confirmable_candidates`
        // owns the eligibility decision (request-prefix scope, delimiter
        // collapse, already-read skip) so it stays unit-testable.
        if let Some(anchor) = settled_anchor {
            let last_read = raw_objects.last().map(|o| o.key.clone());
            let candidates =
                confirmable_candidates(&anchor, prefix, delimiter, last_read.as_deref());
            // Probes are independent; run them concurrently (bounded, same
            // doctrine as `bounded_head_calls` — never blast the backend).
            // A probe error fails the listing closed, deliberately: silently
            // skipping a candidate would drop a user-visible key, and the
            // probes are the same failure class as the page fetches above.
            let probes = candidates.into_iter().map(|candidate| {
                let client = self.client.clone();
                let bucket = bucket.to_string();
                async move {
                    DELEGATED_LIST_PROBE_REQUESTS.inc();
                    // `max_keys(3)`: the exact key sorts first among keys
                    // sharing its prefix, and the next slots catch foreign
                    // multi-suffix forms (`candidate + ".delta"…`) that
                    // classification also maps to the candidate's user key.
                    let found = client
                        .list_objects_v2()
                        .bucket(&bucket)
                        .prefix(&candidate)
                        .max_keys(3)
                        .send()
                        .await
                        .map_err(|e| self.classify(&bucket, &e, S3Op::ListObjects))?;
                    let hits: Vec<S3ListedObject> = found
                        .contents
                        .into_iter()
                        .flatten()
                        .filter(|obj| {
                            obj.key
                                .as_deref()
                                .is_some_and(|k| probe_hit_serves_candidate(k, &candidate))
                        })
                        .filter_map(S3ListedObject::from_s3_object)
                        .collect();
                    Ok::<_, StorageError>(hits)
                }
            });
            let results: Vec<Result<Vec<S3ListedObject>, StorageError>> =
                futures::stream::iter(probes)
                    .buffer_unordered(Self::MAX_CONCURRENT_HEADS)
                    .collect()
                    .await;
            for result in results {
                // No ordering fix-up needed: `dedup_keep_latest` keys by user
                // key and `interleave_and_paginate` sorts every entry itself —
                // ordering (and duplicate absorption) is owned downstream.
                raw_objects.extend(result?);
            }
        }

        // Classify and build lite metadata (no HEAD calls — same as bulk_list_objects).
        let listing = Self::classify_listed_objects(raw_objects);
        let objects: Vec<(String, FileMetadata)> =
            Self::resolve_classified_lite(listing.classified, listing.dir_markers);

        // Apply max_keys across both objects and common_prefixes (interleaved)
        let common_prefixes: Vec<String> = all_common_prefixes.into_iter().collect();

        let page = crate::deltaglider::interleave_and_paginate(
            objects,
            common_prefixes,
            max_keys,
            continuation_token,
        );

        debug!(
            "Delegated list: {} objects + {} prefixes in {}/{}",
            page.objects.len(),
            page.common_prefixes.len(),
            bucket,
            prefix
        );

        Ok(Some(DelegatedListResult {
            objects: page.objects,
            common_prefixes: page.common_prefixes,
            is_truncated: page.is_truncated,
            next_continuation_token: page.next_continuation_token,
        }))
    }

    async fn put_directory_marker(&self, bucket: &str, key: &str) -> Result<(), StorageError> {
        // Directory markers are empty S3 objects; they still need SSE
        // headers when the backend runs in native-encryption mode,
        // otherwise a bucket policy that enforces encryption (common
        // for SSE-KMS deployments) will reject them.
        let mut request = self
            .client
            .put_object()
            .bucket(bucket)
            .key(key)
            .content_type("application/x-directory")
            .content_length(0)
            .body(ByteStream::from(vec![]));
        // H9: stamp the dg-encrypted-native marker symmetrically with
        // put_object_with_metadata. The marker isn't secret — it just
        // tells the read path "native-encrypted, don't try to proxy-
        // decrypt". Without it, a future read-side sniffer that
        // distinguishes "plaintext" from "native-encrypted" via the
        // marker would misclassify directory markers.
        if let Some(marker) = self.native_encryption.marker() {
            request = request.metadata("dg-encrypted-native", marker);
        }
        request = apply_native_encryption(request, &self.native_encryption);
        request
            .send()
            .await
            .map_err(|e| self.classify(bucket, &e, S3Op::PutObject))?;

        debug!("Created directory marker: {}/{}", bucket, key);
        Ok(())
    }
}
