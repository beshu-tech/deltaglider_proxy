// SPDX-License-Identifier: GPL-3.0-only

//! In-memory multipart upload state management
//!
//! Parts are buffered in memory until CompleteMultipartUpload assembles them
//! and passes the result through `engine.store()` for delta compression.
//! Uploads are ephemeral — lost on restart; clients handle this gracefully.

use crate::api::S3Error;
use bytes::{Bytes, BytesMut};
use chrono::{DateTime, Duration, Utc};

/// Per-part listing entry used by ListParts. Previously defined in
/// `src/api/xml.rs`; moved here when the axum XML response builders
/// were retired with the legacy S3 adapter. The s3s adapter
/// translates these into its own wire types.
#[derive(Debug, Clone)]
pub struct PartInfo {
    pub part_number: u32,
    pub etag: String,
    pub size: u64,
    pub last_modified: DateTime<Utc>,
}

/// Per-upload entry used by ListMultipartUploads. Same migration
/// story as [`PartInfo`].
#[derive(Debug, Clone)]
pub struct UploadInfo {
    pub key: String,
    pub upload_id: String,
    pub initiated: DateTime<Utc>,
}
use md5::{Digest, Md5};
use parking_lot::RwLock;
use rand::Rng;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use tempfile::NamedTempFile;

const RELAY_ROOT_DIR: &str = "deltaglider-mpu-relay";

/// Data for a single uploaded part
enum PartPayload {
    InMemory(Bytes),
    RelayedFile(PathBuf),
}

impl PartPayload {
    /// Load the part bytes. For a RELAYED file, re-hash the content and verify it
    /// against the MD5 recorded at UploadPart time (`expected_md5`). The relay dir
    /// lives under a fixed world-known temp path; on a shared host a local attacker
    /// who pre-creates it owns the parent and could swap a 0600 part file between
    /// UploadPart and Complete. Re-verifying on read closes that substitution
    /// (the stored object would otherwise carry attacker bytes under a valid ETag).
    fn load_bytes(&self, expected_md5: &[u8; 16]) -> Result<Bytes, S3Error> {
        match self {
            Self::InMemory(bytes) => Ok(bytes.clone()),
            Self::RelayedFile(path) => {
                let bytes = fs::read(path).map(Bytes::from).map_err(|e| {
                    S3Error::InternalError(format!("Failed to read relayed part: {}", e))
                })?;
                let actual: [u8; 16] = Md5::digest(&bytes).into();
                if &actual != expected_md5 {
                    return Err(S3Error::InternalError(
                        "relayed multipart part failed integrity check (content changed on disk \
                         since upload)"
                            .to_string(),
                    ));
                }
                Ok(bytes)
            }
        }
    }
}

struct PartData {
    payload: PartPayload,
    md5_hex: String,
    md5_raw: [u8; 16],
    size: u64,
    uploaded_at: DateTime<Utc>,
}

/// Lifecycle state of a multipart upload. Replaces the old
/// `completed: bool` flag to close a race between `complete()` and
/// `abort()` where the handler could return 204 "aborted" AFTER
/// complete had already validated parts and the subsequent
/// `engine.store*` was about to publish the object (C4 security fix).
///
/// The state machine:
///
/// ```text
///                   upload_part ↻       abort
///                      │                 │
///                      ▼                 ▼
///   [create] ─▶ Open ─▶─ begin_complete ─▶─ Completing
///                │                            │
///                │                            ├── finish_upload ──▶ (removed)
///                │                            └── rollback_upload ──▶ Open
///                │
///                └── abort ──▶ (removed)
/// ```
///
/// Invariants enforced by callers:
/// - `upload_part` rejects unless state is `Open`.
/// - `abort` rejects when state is `Completing` (409 Conflict).
/// - `begin_complete` only returns parts if state was `Open`; atomically
///   flips to `Completing` under the write lock.
/// - `finish_upload` / `rollback_upload` terminate `Completing` state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MultipartState {
    /// Accepting UploadPart calls. Abort is allowed.
    Open,
    /// `begin_complete` has validated and handed off parts; `engine.store*`
    /// is in flight. New UploadParts and aborts are refused.
    Completing,
}

/// State for an in-progress multipart upload
struct MultipartUpload {
    upload_id: String,
    bucket: String,
    key: String,
    created_at: DateTime<Utc>,
    /// Latest UploadPart or Create timestamp — drives the idle-TTL sweeper
    /// that reclaims memory from attackers who open uploads and walk away
    /// (C3 DoS fix).
    last_activity: DateTime<Utc>,
    content_type: Option<String>,
    user_metadata: HashMap<String, String>,
    parts: HashMap<u32, PartData>,
    state: MultipartState,
    relay_strategy: RelayStrategy,
    /// True while the engine store for this upload is actively running. The
    /// idle-TTL sweeper (`cleanup_expired`) must NOT reclaim a Completing upload
    /// whose store is in flight — a slow store to a remote backend can exceed
    /// completing_timeout, and sweeping it deletes the relay part files the
    /// store is still reading, losing them permanently on retry (H18). The store
    /// is separately bounded (request timeout + codec watchdog), so this flag
    /// can never wedge an upload forever.
    store_in_progress: bool,
}

enum RelayStrategy {
    InMemory { relay_threshold_bytes: Option<u64> },
    Relayed { relay_dir: PathBuf },
}

/// RAII: clears an upload's `store_in_progress` flag on drop so a store that
/// returns (success, failure, OR a dropped future) always re-enables sweeping.
#[must_use = "the store-in-progress protection ends when the guard is dropped"]
pub struct StoreInProgressGuard {
    store: std::sync::Arc<MultipartStore>,
    upload_id: String,
}

impl Drop for StoreInProgressGuard {
    fn drop(&mut self) {
        self.store.clear_store_in_progress(&self.upload_id);
    }
}

/// Result of assembling a completed multipart upload
#[derive(Debug)]
pub struct CompletedUpload {
    pub data: Bytes,
    pub etag: String,
    pub content_type: Option<String>,
    pub user_metadata: HashMap<String, String>,
}

pub enum PassthroughPayload {
    Chunks(Vec<Bytes>),
    RelayedParts(Vec<PathBuf>),
}

pub struct CompletedPassthrough {
    pub payload: PassthroughPayload,
    pub etag: String,
    pub total_size: u64,
    pub content_type: Option<String>,
    pub user_metadata: HashMap<String, String>,
}

/// Summary of one multipart sweeper run.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MultipartSweepReport {
    pub swept_open_uploads: u64,
    pub swept_completing_uploads: u64,
    pub reclaimed_bytes: u64,
    pub orphan_relay_dirs_removed: u64,
    pub orphan_relay_files_removed: u64,
}

impl MultipartSweepReport {
    pub fn total_uploads_swept(self) -> u64 {
        self.swept_open_uploads + self.swept_completing_uploads
    }
}

/// Internal: validated parts from the shared validation step.
struct ValidatedParts {
    part_data: Vec<Bytes>,
    etag: String,
    total_size: u64,
}

/// Default maximum number of concurrent multipart uploads.
/// Overridable via `DGP_MAX_MULTIPART_UPLOADS` env var.
fn default_max_uploads() -> usize {
    crate::config::env_parse_with_default("DGP_MAX_MULTIPART_UPLOADS", 1000)
}

/// Default global cap on total in-flight multipart bytes across all uploads.
/// Overridable via `DGP_MAX_TOTAL_MULTIPART_BYTES` env var. Protects against
/// the C3 DoS where an attacker opens many uploads and sends many large
/// parts without completing — pre-fix the only cap was `max_object_size`
/// per upload at complete-time, leaving `max_object_size * max_uploads`
/// bytes of RAM reachable.
///
/// Default formula: `max_object_size * (max_uploads / 4)`. The /4 is a
/// safety margin so legitimate multi-uploader workloads still fit while
/// attackers hit the ceiling before they can saturate memory.
fn default_max_total_multipart_bytes(max_object_size: u64, max_uploads: usize) -> u64 {
    // Allow operator override (absolute bytes). Routed through env_parse
    // for consistent warn-on-invalid behaviour.
    if let Some(n) = crate::config::env_parse::<u64>("DGP_MAX_TOTAL_MULTIPART_BYTES") {
        return n;
    }
    // Default: max_object_size * (max_uploads / 4), clamped to at least
    // max_object_size (one full upload must always fit).
    max_object_size.saturating_mul((max_uploads.max(4) / 4) as u64)
}

/// TTL before an idle (no recent UploadPart activity) multipart upload is
/// garbage-collected. Overridable via `DGP_MULTIPART_IDLE_TTL_HOURS`.
/// Default 24h — matches AWS's default abort-incomplete-multipart-upload
/// lifecycle recommendation.
fn default_multipart_idle_ttl_hours() -> i64 {
    crate::config::env_parse_with_default("DGP_MULTIPART_IDLE_TTL_HOURS", 24)
}

/// Thread-safe in-memory store for multipart upload state
pub struct MultipartStore {
    uploads: RwLock<HashMap<String, MultipartUpload>>,
    max_object_size: u64,
    max_uploads: usize,
    /// Global in-flight bytes across all uploads. Kept consistent with
    /// the sum of `MultipartUpload.parts[*].size` — updated under the
    /// same write lock that mutates the parts map. Checked before each
    /// UploadPart accepts bytes (C3 DoS fix).
    in_flight_bytes: std::sync::atomic::AtomicU64,
    max_total_multipart_bytes: u64,
    idle_ttl: Duration,
}

impl MultipartStore {
    pub fn new(max_object_size: u64) -> Self {
        let max_uploads = default_max_uploads();
        let max_total_multipart_bytes =
            default_max_total_multipart_bytes(max_object_size, max_uploads);
        let idle_ttl_hours = default_multipart_idle_ttl_hours();
        Self {
            uploads: RwLock::new(HashMap::new()),
            max_object_size,
            max_uploads,
            in_flight_bytes: std::sync::atomic::AtomicU64::new(0),
            max_total_multipart_bytes,
            idle_ttl: Duration::hours(idle_ttl_hours),
        }
    }

    /// Test-only constructor with custom caps. Not part of the stable API.
    #[cfg(test)]
    pub(crate) fn new_for_test(
        max_object_size: u64,
        max_total_multipart_bytes: u64,
        idle_ttl: Duration,
    ) -> Self {
        Self {
            uploads: RwLock::new(HashMap::new()),
            max_object_size,
            max_uploads: 1000,
            in_flight_bytes: std::sync::atomic::AtomicU64::new(0),
            max_total_multipart_bytes,
            idle_ttl,
        }
    }

    /// Snapshot the global in-flight byte counter. Test-only observability.
    #[cfg(test)]
    pub(crate) fn in_flight_bytes(&self) -> u64 {
        self.in_flight_bytes
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Create a new multipart upload, returns the upload ID.
    /// Returns `S3Error::SlowDown` if the maximum number of concurrent uploads is reached.
    pub fn create(
        &self,
        bucket: &str,
        key: &str,
        content_type: Option<String>,
        user_metadata: HashMap<String, String>,
    ) -> Result<String, S3Error> {
        self.create_with_relay_policy(bucket, key, content_type, user_metadata, None, false)
    }

    /// Create a new multipart upload with optional relay policy.
    /// - `relay_threshold_bytes`: when set, promote in-memory parts to relayed
    ///   files once cumulative uploaded bytes exceed this threshold.
    /// - `always_relay_passthrough`: start directly in relay mode.
    #[allow(clippy::too_many_arguments)]
    pub fn create_with_relay_policy(
        &self,
        bucket: &str,
        key: &str,
        content_type: Option<String>,
        user_metadata: HashMap<String, String>,
        relay_threshold_bytes: Option<u64>,
        always_relay_passthrough: bool,
    ) -> Result<String, S3Error> {
        let now = Utc::now();

        // Cryptographically random upload ID (matches AWS S3 behavior).
        let mut random_bytes = [0u8; 16];
        rand::rngs::OsRng.fill(&mut random_bytes);
        let upload_id = hex::encode(random_bytes); // 32 hex chars

        let mut uploads = self.uploads.write();

        // Enforce maximum concurrent uploads to prevent resource exhaustion
        if uploads.len() >= self.max_uploads {
            return Err(S3Error::SlowDown(format!(
                "Too many concurrent multipart uploads (max {})",
                self.max_uploads
            )));
        }

        let upload = MultipartUpload {
            upload_id: upload_id.clone(),
            bucket: bucket.to_string(),
            key: key.to_string(),
            created_at: now,
            last_activity: now,
            content_type,
            user_metadata,
            parts: HashMap::new(),
            state: MultipartState::Open,
            relay_strategy: if always_relay_passthrough {
                RelayStrategy::Relayed {
                    relay_dir: relay_dir_for_upload(&upload_id),
                }
            } else {
                RelayStrategy::InMemory {
                    relay_threshold_bytes,
                }
            },
            store_in_progress: false,
        };

        uploads.insert(upload_id.clone(), upload);
        Ok(upload_id)
    }

    /// Upload a part, returns the quoted ETag (MD5 hex).
    pub fn upload_part(
        &self,
        upload_id: &str,
        bucket: &str,
        key: &str,
        part_number: u32,
        data: Bytes,
    ) -> Result<String, S3Error> {
        if !(1..=10000).contains(&part_number) {
            return Err(S3Error::InvalidArgument(
                "Part number must be between 1 and 10000".to_string(),
            ));
        }

        let md5_raw: [u8; 16] = Md5::digest(&data).into();
        let md5_hex = hex::encode(md5_raw);
        let etag = format!("\"{}\"", md5_hex);
        let size = data.len() as u64;

        let mut uploads = self.uploads.write();
        let upload = uploads
            .get_mut(upload_id)
            .ok_or_else(|| S3Error::NoSuchUpload(upload_id.to_string()))?;

        // Validate bucket+key match
        if upload.bucket != bucket || upload.key != key {
            return Err(S3Error::NoSuchUpload(upload_id.to_string()));
        }

        // C4 security fix: parts can only be uploaded while the upload is
        // Open. Once CompleteMultipartUpload has started (state=Completing),
        // accepting new parts would race with the in-flight `engine.store*`.
        if upload.state != MultipartState::Open {
            return Err(S3Error::InvalidRequest(
                "Upload is in the process of being completed; no more parts can be added"
                    .to_string(),
            ));
        }

        // C3 DoS fix: enforce size caps BEFORE buffering the part. Two
        // gates, checked in order:
        //
        // 1. Per-upload cap (max_object_size) — prevents one upload from
        //    assembling more bytes than a single object is allowed to be.
        //    Overwrite semantics: recompute cumulative from existing parts
        //    MINUS the old size of `part_number` (if any) PLUS the new
        //    size. Without the subtraction, re-uploading a part would
        //    double-count.
        //
        // 2. Global cap (max_total_multipart_bytes) — prevents many
        //    uploads from collectively exhausting heap. Rejects with
        //    SlowDown so AWS SDKs back off and retry.
        let old_part_size = upload.parts.get(&part_number).map(|p| p.size).unwrap_or(0);
        let cumulative_after = upload
            .parts
            .values()
            .map(|p| p.size)
            .sum::<u64>()
            .saturating_sub(old_part_size)
            .saturating_add(size);

        if cumulative_after > self.max_object_size {
            return Err(S3Error::EntityTooLarge {
                size: cumulative_after,
                max: self.max_object_size,
            });
        }

        // Compute the global delta we'd contribute (signed on overwrite).
        let delta: i64 = size as i64 - old_part_size as i64;
        if delta > 0 {
            let new_total = self
                .in_flight_bytes
                .load(std::sync::atomic::Ordering::Relaxed)
                .saturating_add(delta as u64);
            if new_total > self.max_total_multipart_bytes {
                return Err(S3Error::SlowDown(format!(
                    "Multipart in-flight bytes cap reached ({} / {} bytes)",
                    new_total, self.max_total_multipart_bytes
                )));
            }
        }

        let should_promote_to_relay = match &upload.relay_strategy {
            RelayStrategy::InMemory {
                relay_threshold_bytes: Some(threshold),
            } => cumulative_after > *threshold,
            RelayStrategy::InMemory {
                relay_threshold_bytes: None,
            } => false,
            RelayStrategy::Relayed { .. } => false,
        };
        if should_promote_to_relay {
            Self::promote_upload_to_relay(upload)?;
        }

        let payload = match &upload.relay_strategy {
            RelayStrategy::InMemory { .. } => PartPayload::InMemory(data),
            RelayStrategy::Relayed { relay_dir } => {
                let path = part_path(relay_dir, part_number);
                write_part_file(&path, &data)?;
                PartPayload::RelayedFile(path)
            }
        };

        // Overwrite semantics: re-uploading same part_number replaces previous data.
        upload.parts.insert(
            part_number,
            PartData {
                payload,
                md5_hex,
                md5_raw,
                size,
                uploaded_at: Utc::now(),
            },
        );
        upload.last_activity = Utc::now();

        // Update global counter AFTER the insert so concurrent readers see
        // a consistent view (counter ≥ actual bytes in map at any moment).
        if delta >= 0 {
            self.in_flight_bytes
                .fetch_add(delta as u64, std::sync::atomic::Ordering::Relaxed);
        } else {
            self.in_flight_bytes
                .fetch_sub((-delta) as u64, std::sync::atomic::Ordering::Relaxed);
        }

        Ok(etag)
    }

    /// Get the size of a specific uploaded part (for quota pre-check).
    pub fn get_part_size(&self, upload_id: &str, part_number: u32) -> Option<u64> {
        let uploads = self.uploads.read();
        uploads
            .get(upload_id)
            .and_then(|u| u.parts.get(&part_number))
            .map(|p| p.size)
    }

    /// Begin completion: validate parts, atomically transition to
    /// `Completing`, and return the assembled buffer. After this call
    /// the upload is reserved — new UploadParts AND abort are refused
    /// (409) until the caller invokes `finish_upload` or
    /// `rollback_upload`. This closes the C4 complete/abort race.
    ///
    /// On validation failure the state is NOT changed (upload stays
    /// `Open` so the client can retry with corrected part metadata).
    pub fn complete(
        &self,
        upload_id: &str,
        bucket: &str,
        key: &str,
        requested_parts: &[(u32, String)], // (part_number, etag)
    ) -> Result<CompletedUpload, S3Error> {
        let mut uploads = self.uploads.write();

        // Refuse if the upload is already Completing — only one complete
        // may be in flight at a time. Double-complete returns 404 to
        // preserve the prior contract.
        if let Some(u) = uploads.get(upload_id) {
            if u.state == MultipartState::Completing {
                return Err(S3Error::InvalidRequest(
                    "Upload is already being completed".to_string(),
                ));
            }
        }

        let (validated, upload) =
            self.validate_parts(&uploads, upload_id, bucket, key, requested_parts, true)?;

        let mut assembled = BytesMut::new();
        for part in &validated.part_data {
            assembled.extend_from_slice(part);
        }

        let result = CompletedUpload {
            data: assembled.freeze(),
            etag: validated.etag,
            content_type: upload.content_type.clone(),
            user_metadata: upload.user_metadata.clone(),
        };

        // Flip to Completing under the same write lock that performed the
        // validation — atomic with respect to `abort` and `upload_part`.
        if let Some(u) = uploads.get_mut(upload_id) {
            u.state = MultipartState::Completing;
            u.last_activity = Utc::now();
        }

        Ok(result)
    }

    /// Begin-complete variant optimized for passthrough storage.
    ///
    /// In relay mode this assembles a temporary file under the upload's relay
    /// directory, allowing callers to stream the final payload into storage.
    pub fn complete_passthrough(
        &self,
        upload_id: &str,
        bucket: &str,
        key: &str,
        requested_parts: &[(u32, String)],
    ) -> Result<CompletedPassthrough, S3Error> {
        let mut uploads = self.uploads.write();

        if let Some(u) = uploads.get(upload_id) {
            if u.state == MultipartState::Completing {
                return Err(S3Error::InvalidRequest(
                    "Upload is already being completed".to_string(),
                ));
            }
        }

        let hydrate_part_data = uploads
            .get(upload_id)
            .map(|u| matches!(u.relay_strategy, RelayStrategy::InMemory { .. }))
            .ok_or_else(|| S3Error::NoSuchUpload(upload_id.to_string()))?;
        let (validated, upload) = self.validate_parts(
            &uploads,
            upload_id,
            bucket,
            key,
            requested_parts,
            hydrate_part_data,
        )?;

        let payload = match &upload.relay_strategy {
            RelayStrategy::InMemory { .. } => PassthroughPayload::Chunks(validated.part_data),
            RelayStrategy::Relayed { relay_dir: _ } => {
                let ordered_paths = ordered_relay_part_paths(requested_parts, upload)?;
                PassthroughPayload::RelayedParts(ordered_paths)
            }
        };

        let result = CompletedPassthrough {
            payload,
            etag: validated.etag,
            total_size: validated.total_size,
            content_type: upload.content_type.clone(),
            user_metadata: upload.user_metadata.clone(),
        };

        if let Some(u) = uploads.get_mut(upload_id) {
            u.state = MultipartState::Completing;
            u.last_activity = Utc::now();
        }

        Ok(result)
    }

    /// Roll the upload back to `Open` after a failed engine.store*.
    /// The client is expected to retry CompleteMultipartUpload with the
    /// same part set — this matches S3's behaviour when the backing
    /// store rejects a complete.
    ///
    /// Idempotent: if the upload was already removed (e.g. via a
    /// concurrent abort after rollback), does nothing.
    pub fn rollback_upload(&self, upload_id: &str) {
        if let Some(u) = self.uploads.write().get_mut(upload_id) {
            u.state = MultipartState::Open;
        }
    }

    /// Mark an upload's engine store as in-flight and return an RAII guard that
    /// clears the flag on drop. While held, `cleanup_expired` will not reclaim
    /// the (Completing) upload even past completing_timeout, so a slow store to a
    /// remote backend can't have its relay parts swept out from under it (H18).
    pub fn store_guard(self: &std::sync::Arc<Self>, upload_id: &str) -> StoreInProgressGuard {
        if let Some(u) = self.uploads.write().get_mut(upload_id) {
            u.store_in_progress = true;
        }
        StoreInProgressGuard {
            store: std::sync::Arc::clone(self),
            upload_id: upload_id.to_string(),
        }
    }

    /// Clear the store-in-progress flag (best-effort; no-op if the upload is
    /// already gone). Called from `StoreInProgressGuard::drop`.
    fn clear_store_in_progress(&self, upload_id: &str) {
        if let Some(u) = self.uploads.write().get_mut(upload_id) {
            u.store_in_progress = false;
        }
    }

    /// Finalise a completed upload after `engine.store*` succeeds.
    /// Removes the upload from the map. This is the terminal state.
    /// Semantically equivalent to the previous `remove_upload`.
    ///
    /// Also releases the upload's bytes from the global in-flight counter
    /// so new uploads can reclaim headroom (C3 DoS fix).
    pub fn finish_upload(&self, upload_id: &str) {
        if let Some(u) = self.uploads.write().remove(upload_id) {
            let _ = self.release_bytes(&u);
            cleanup_relay_dir_for_upload(&u);
        }
    }

    /// Return the sum of all part sizes for this upload — used by the
    /// in-flight counter on release paths.
    fn release_bytes(&self, upload: &MultipartUpload) -> u64 {
        let freed: u64 = upload.parts.values().map(|p| p.size).sum();
        if freed > 0 {
            self.in_flight_bytes
                .fetch_sub(freed, std::sync::atomic::Ordering::Relaxed);
        }
        freed
    }

    /// Shared validation for complete variants.
    ///
    /// Looks up the upload, validates part ordering and ETags, enforces size limits,
    /// and computes the S3-compatible multipart ETag. Returns validated part data
    /// and a reference to the upload (for content_type / user_metadata).
    fn validate_parts<'a>(
        &self,
        uploads: &'a HashMap<String, MultipartUpload>,
        upload_id: &str,
        bucket: &str,
        key: &str,
        requested_parts: &[(u32, String)],
        hydrate_part_data: bool,
    ) -> Result<(ValidatedParts, &'a MultipartUpload), S3Error> {
        let upload = uploads
            .get(upload_id)
            .ok_or_else(|| S3Error::NoSuchUpload(upload_id.to_string()))?;
        if upload.bucket != bucket || upload.key != key {
            return Err(S3Error::NoSuchUpload(upload_id.to_string()));
        }

        if requested_parts.is_empty() {
            return Err(S3Error::InvalidPart(
                "You must specify at least one part".to_string(),
            ));
        }

        // Validate ascending order
        for window in requested_parts.windows(2) {
            if window[0].0 >= window[1].0 {
                return Err(S3Error::InvalidPartOrder);
            }
        }

        // Validate each part exists and ETags match; compute total size
        let mut total_size: u64 = 0;
        let mut md5_concat = Vec::new();
        let mut part_data = Vec::with_capacity(requested_parts.len());

        for (part_number, requested_etag) in requested_parts {
            let part = upload.parts.get(part_number).ok_or_else(|| {
                S3Error::InvalidPart(format!("Part {} has not been uploaded", part_number))
            })?;

            // Normalize ETags for comparison (strip quotes)
            let requested_clean = requested_etag.trim_matches('"');
            if requested_clean != part.md5_hex {
                return Err(S3Error::InvalidPart(format!(
                    "ETag mismatch for part {}: expected \"{}\", got \"{}\"",
                    part_number, part.md5_hex, requested_clean
                )));
            }

            total_size += part.size;
            if total_size > self.max_object_size {
                return Err(S3Error::InvalidArgument(format!(
                    "Assembled object size {} exceeds maximum {}",
                    total_size, self.max_object_size
                )));
            }

            md5_concat.extend_from_slice(&part.md5_raw);
            if hydrate_part_data {
                part_data.push(part.payload.load_bytes(&part.md5_raw)?);
            }
        }

        // S3-compatible multipart ETag: MD5(concat of part MD5 raw bytes)-N
        let final_md5 = Md5::digest(&md5_concat);
        let etag = format!("\"{}-{}\"", hex::encode(final_md5), requested_parts.len());

        Ok((
            ValidatedParts {
                part_data,
                etag,
                total_size,
            },
            upload,
        ))
    }

    /// Abort a multipart upload. Validates bucket+key match.
    ///
    /// C4 security fix: refuse when the upload is already in
    /// `Completing` state. Accepting the abort at that point would
    /// race with the in-flight `engine.store*` and return a 204
    /// "aborted" even though the object actually lands. Clients
    /// should wait for the CompleteMultipartUpload response instead.
    pub fn abort(&self, upload_id: &str, bucket: &str, key: &str) -> Result<(), S3Error> {
        let mut uploads = self.uploads.write();
        let upload = uploads
            .get(upload_id)
            .ok_or_else(|| S3Error::NoSuchUpload(upload_id.to_string()))?;

        if upload.bucket != bucket || upload.key != key {
            return Err(S3Error::NoSuchUpload(upload_id.to_string()));
        }

        if upload.state == MultipartState::Completing {
            return Err(S3Error::InvalidRequest(
                "Cannot abort: upload is currently being completed".to_string(),
            ));
        }

        // Release this upload's bytes from the global counter (C3 DoS fix).
        if let Some(removed) = uploads.remove(upload_id) {
            drop(uploads); // release write lock before touching atomic
            let _ = self.release_bytes(&removed);
            cleanup_relay_dir_for_upload(&removed);
        }
        Ok(())
    }

    /// Return the number of in-flight uploads targeting `bucket`.
    /// Used by DeleteBucket (H2) to refuse deletion when MPU state
    /// would be orphaned. Counts uploads in Open AND Completing state
    /// because both would become unreachable after the bucket is gone.
    pub fn count_uploads_for_bucket(&self, bucket: &str) -> usize {
        self.uploads
            .read()
            .values()
            .filter(|u| u.bucket == bucket)
            .count()
    }

    /// Force-remove all uploads targeting `bucket`.
    ///
    /// Used by DeleteBucket when the bucket has no visible objects:
    /// MPU state is internal residue and should not block deletion.
    ///
    /// **Refuses** if any upload is in `Completing` state. A
    /// `Completing` upload is mid-flight on `engine.store_*` and holds
    /// borrowed buffers / relay-dir paths that the handler is still
    /// reading; tearing those down here while the storage write is
    /// in-progress causes a P0-class race (the storage layer's
    /// `create_dir_all` silently recreates the bucket inside the
    /// just-deleted directory tree). The operator gets a clean
    /// `BucketNotEmpty` error and can retry once the multipart
    /// finalises (typically seconds).
    ///
    /// On success: returns the number of `Open` uploads purged.
    /// On refusal: returns `Err(count_completing)` — never partially
    /// purges so the caller's bookkeeping is all-or-nothing.
    pub fn purge_uploads_for_bucket(&self, bucket: &str) -> Result<usize, usize> {
        // Collect `Open` uploads under the write lock; refuse and
        // release the lock if any `Completing` upload is targeting
        // this bucket. Atomic check-and-purge.
        let removed: Vec<MultipartUpload> = {
            let mut uploads = self.uploads.write();

            let completing_count = uploads
                .values()
                .filter(|u| u.bucket == bucket && u.state == MultipartState::Completing)
                .count();
            if completing_count > 0 {
                return Err(completing_count);
            }

            let mut removed = Vec::new();
            uploads.retain(|_, u| {
                if u.bucket == bucket {
                    removed.push(take_upload_for_cleanup(u));
                    return false;
                }
                true
            });
            removed
        };

        let removed_count = removed.len();
        for upload in removed {
            let _ = self.release_bytes(&upload);
            cleanup_relay_dir_for_upload(&upload);
        }

        Ok(removed_count)
    }

    /// List parts for an upload. Validates bucket+key match.
    pub fn list_parts(
        &self,
        upload_id: &str,
        bucket: &str,
        key: &str,
    ) -> Result<Vec<PartInfo>, S3Error> {
        let (parts, _, _) = self.list_parts_paginated(upload_id, bucket, key, 0, 10000)?;
        Ok(parts)
    }

    /// Paginated variant of [`Self::list_parts`] (L1 correctness fix).
    /// Returns `(page, is_truncated, next_part_number_marker)`.
    ///
    /// - `part_number_marker`: return parts with part_number strictly
    ///   greater than this value (0 = from beginning, per S3 spec).
    /// - `max_parts`: cap on returned count; clamp at 10_000 (S3 hard
    ///   limit on parts per upload).
    pub fn list_parts_paginated(
        &self,
        upload_id: &str,
        bucket: &str,
        key: &str,
        part_number_marker: u32,
        max_parts: u32,
    ) -> Result<(Vec<PartInfo>, bool, u32), S3Error> {
        let uploads = self.uploads.read();
        let upload = uploads
            .get(upload_id)
            .ok_or_else(|| S3Error::NoSuchUpload(upload_id.to_string()))?;

        if upload.bucket != bucket || upload.key != key {
            return Err(S3Error::NoSuchUpload(upload_id.to_string()));
        }

        let cap = max_parts.clamp(1, 10_000) as usize;

        let mut all: Vec<PartInfo> = upload
            .parts
            .iter()
            .filter(|(&num, _)| num > part_number_marker)
            .map(|(&num, pd)| PartInfo {
                part_number: num,
                etag: format!("\"{}\"", pd.md5_hex),
                size: pd.size,
                last_modified: pd.uploaded_at,
            })
            .collect();
        all.sort_by_key(|p| p.part_number);

        let is_truncated = all.len() > cap;
        if is_truncated {
            all.truncate(cap);
        }
        let next_marker = all.last().map(|p| p.part_number).unwrap_or(0);
        Ok((all, is_truncated, next_marker))
    }

    /// Paginated ListMultipartUploads (L1 correctness fix).
    /// Returns `(page, is_truncated, next_key_marker, next_upload_id_marker)`.
    ///
    /// - `key_marker` + `upload_id_marker`: tuple-cursor — skip any
    ///   upload whose (key, upload_id) is ≤ (key_marker, upload_id_marker)
    ///   lexicographically. Matches AWS S3 semantics.
    /// - `max_uploads`: cap on returned count, clamped to 1..=1000.
    pub fn list_uploads_paginated(
        &self,
        bucket: Option<&str>,
        prefix: Option<&str>,
        key_marker: &str,
        upload_id_marker: &str,
        max_uploads: u32,
    ) -> (Vec<UploadInfo>, bool, String, String) {
        let uploads = self.uploads.read();
        let cap = max_uploads.clamp(1, 1000) as usize;
        let mut filtered: Vec<UploadInfo> = uploads
            .values()
            .filter(|u| {
                if let Some(b) = bucket {
                    if u.bucket != b {
                        return false;
                    }
                }
                if let Some(p) = prefix {
                    if !u.key.starts_with(p) {
                        return false;
                    }
                }
                // Tuple-cursor skip.
                if !key_marker.is_empty() || !upload_id_marker.is_empty() {
                    let cmp =
                        (u.key.as_str(), u.upload_id.as_str()).cmp(&(key_marker, upload_id_marker));
                    if cmp != std::cmp::Ordering::Greater {
                        return false;
                    }
                }
                true
            })
            .map(|u| UploadInfo {
                key: u.key.clone(),
                upload_id: u.upload_id.clone(),
                initiated: u.created_at,
            })
            .collect();
        filtered.sort_by(|a, b| a.key.cmp(&b.key).then(a.upload_id.cmp(&b.upload_id)));

        let is_truncated = filtered.len() > cap;
        if is_truncated {
            filtered.truncate(cap);
        }
        let (next_key, next_upload_id) = filtered
            .last()
            .map(|u| (u.key.clone(), u.upload_id.clone()))
            .unwrap_or_default();
        (filtered, is_truncated, next_key, next_upload_id)
    }

    /// Remove uploads that have been idle longer than the configured idle
    /// TTL OR have exceeded `max_age` (whichever is stricter). The idle
    /// TTL is measured from `last_activity` (last UploadPart or Create).
    ///
    /// C3 DoS fix: sweeps uploads opened by an attacker who never
    /// completes. Also decrements the global in-flight byte counter so
    /// legitimate callers can reclaim headroom.
    ///
    /// Uploads that are stuck in `Completing` are also swept once
    /// `completing_timeout` elapses from their last activity.
    pub fn cleanup_expired(
        &self,
        max_age: std::time::Duration,
        completing_timeout: std::time::Duration,
    ) -> MultipartSweepReport {
        let now = Utc::now();
        let max_age_cutoff = now - Duration::from_std(max_age).unwrap_or(Duration::hours(1));
        let idle_cutoff = now - self.idle_ttl;
        let completing_cutoff =
            now - Duration::from_std(completing_timeout).unwrap_or(Duration::hours(1));
        // Take stricter of the two cutoffs (newer / later = stricter).
        let cutoff = if idle_cutoff > max_age_cutoff {
            idle_cutoff
        } else {
            max_age_cutoff
        };

        // Collect + remove under write lock, then release bytes without it.
        let expired: Vec<MultipartUpload> = {
            let mut uploads = self.uploads.write();
            let mut expired = Vec::new();
            uploads.retain(|_, u| {
                if u.state == MultipartState::Completing {
                    // Never reclaim an upload whose engine store is in flight —
                    // sweeping it deletes the relay parts the store is reading,
                    // losing them permanently on retry (H18). Bounded elsewhere.
                    if u.last_activity <= completing_cutoff && !u.store_in_progress {
                        expired.push(take_upload_for_cleanup(u));
                        return false;
                    }
                } else if u.last_activity <= cutoff {
                    expired.push(take_upload_for_cleanup(u));
                    return false;
                }
                true
            });
            expired
        };

        let mut report = MultipartSweepReport::default();
        for u in expired {
            if u.state == MultipartState::Completing {
                report.swept_completing_uploads += 1;
            } else {
                report.swept_open_uploads += 1;
            }
            report.reclaimed_bytes += self.release_bytes(&u);
            cleanup_relay_dir_for_upload(&u);
        }
        report
    }

    /// Remove orphan relay temp artifacts that don't belong to a currently-
    /// tracked relayed upload. `min_age` guards the promotion race for the
    /// PERIODIC sweep (H19) — pass a grace period there; startup passes ZERO
    /// (no concurrent uploads exist yet).
    pub fn sweep_orphan_relay_artifacts(&self, min_age: std::time::Duration) -> MultipartSweepReport {
        let active_relay_dirs: HashSet<PathBuf> = self
            .uploads
            .read()
            .values()
            .filter_map(|u| match &u.relay_strategy {
                RelayStrategy::Relayed { relay_dir } => Some(relay_dir.clone()),
                RelayStrategy::InMemory { .. } => None,
            })
            .collect();
        let (dirs_removed, files_removed) =
            cleanup_orphan_relay_entries_at(&relay_root_dir(), &active_relay_dirs, min_age);
        MultipartSweepReport {
            orphan_relay_dirs_removed: dirs_removed,
            orphan_relay_files_removed: files_removed,
            ..MultipartSweepReport::default()
        }
    }

    /// Current number of tracked uploads (Open + Completing).
    pub fn count_uploads(&self) -> usize {
        self.uploads.read().len()
    }

    fn promote_upload_to_relay(upload: &mut MultipartUpload) -> Result<(), S3Error> {
        let relay_dir = relay_dir_for_upload(&upload.upload_id);
        fs::create_dir_all(&relay_dir).map_err(|e| {
            S3Error::InternalError(format!("Failed to create multipart relay directory: {}", e))
        })?;
        for (part_number, part) in &mut upload.parts {
            if let PartPayload::InMemory(bytes) = &part.payload {
                let path = part_path(&relay_dir, *part_number);
                write_part_file(&path, bytes)?;
                part.payload = PartPayload::RelayedFile(path);
            }
        }
        upload.relay_strategy = RelayStrategy::Relayed { relay_dir };
        Ok(())
    }
}

fn relay_root_dir() -> PathBuf {
    std::env::temp_dir().join(RELAY_ROOT_DIR)
}

fn relay_dir_for_upload(upload_id: &str) -> PathBuf {
    relay_root_dir().join(upload_id)
}

fn part_path(relay_dir: &Path, part_number: u32) -> PathBuf {
    relay_dir.join(format!("part-{:05}.bin", part_number))
}

fn write_part_file(path: &Path, data: &Bytes) -> Result<(), S3Error> {
    let parent = path
        .parent()
        .ok_or_else(|| S3Error::InternalError("Multipart relay path has no parent".to_string()))?;
    fs::create_dir_all(parent)
        .map_err(|e| S3Error::InternalError(format!("Failed to create relay directory: {}", e)))?;
    let mut tmp = NamedTempFile::new_in(parent)
        .map_err(|e| S3Error::InternalError(format!("Failed to create relay tmp file: {}", e)))?;
    tmp.write_all(data)
        .map_err(|e| S3Error::InternalError(format!("Failed to write relay part: {}", e)))?;
    tmp.as_file()
        .sync_all()
        .map_err(|e| S3Error::InternalError(format!("Failed to sync relay part: {}", e)))?;
    tmp.persist(path).map_err(|e| {
        S3Error::InternalError(format!("Failed to persist relay part: {}", e.error))
    })?;
    Ok(())
}

fn ordered_relay_part_paths(
    requested_parts: &[(u32, String)],
    upload: &MultipartUpload,
) -> Result<Vec<PathBuf>, S3Error> {
    let mut paths = Vec::with_capacity(requested_parts.len());
    for (part_number, _) in requested_parts {
        let part = upload.parts.get(part_number).ok_or_else(|| {
            S3Error::InvalidPart(format!("Part {} has not been uploaded", part_number))
        })?;
        match &part.payload {
            PartPayload::RelayedFile(path) => {
                // Re-verify content vs the MD5 recorded at UploadPart before the
                // engine streams these paths (the streamed path never re-hashes
                // against the per-part digest). Closes the shared-host local-
                // attacker part-substitution the buffered load_bytes now guards.
                let bytes = fs::read(path).map_err(|e| {
                    S3Error::InternalError(format!("Failed to read relayed part: {}", e))
                })?;
                let actual: [u8; 16] = Md5::digest(&bytes).into();
                if actual != part.md5_raw {
                    return Err(S3Error::InternalError(
                        "relayed multipart part failed integrity check (content changed on disk \
                         since upload)"
                            .to_string(),
                    ));
                }
                paths.push(path.clone());
            }
            PartPayload::InMemory(_) => {
                return Err(S3Error::InternalError(
                    "Relay upload contains in-memory part unexpectedly".to_string(),
                ))
            }
        }
    }
    Ok(paths)
}

fn cleanup_relay_dir_for_upload(upload: &MultipartUpload) {
    if let RelayStrategy::Relayed { relay_dir } = &upload.relay_strategy {
        let _ = fs::remove_dir_all(relay_dir);
    }
}

fn take_upload_for_cleanup(upload: &mut MultipartUpload) -> MultipartUpload {
    MultipartUpload {
        upload_id: upload.upload_id.clone(),
        bucket: upload.bucket.clone(),
        key: upload.key.clone(),
        created_at: upload.created_at,
        last_activity: upload.last_activity,
        content_type: upload.content_type.clone(),
        user_metadata: upload.user_metadata.clone(),
        parts: std::mem::take(&mut upload.parts),
        state: upload.state,
        relay_strategy: match &upload.relay_strategy {
            RelayStrategy::InMemory {
                relay_threshold_bytes,
            } => RelayStrategy::InMemory {
                relay_threshold_bytes: *relay_threshold_bytes,
            },
            RelayStrategy::Relayed { relay_dir } => RelayStrategy::Relayed {
                relay_dir: relay_dir.clone(),
            },
        },
        store_in_progress: upload.store_in_progress,
    }
}

/// Remove relay artifacts under `relay_root` that don't belong to a currently-
/// tracked upload. `min_age` guards the promotion race (H19): the periodic sweep
/// snapshots active dirs then scans without the lock, so a concurrent
/// UploadPart's freshly-created relay dir isn't in the snapshot — deleting it
/// would destroy an active upload's parts. Only entries whose mtime is older than
/// `min_age` are eligible, so a just-created dir is always spared. Startup passes
/// `Duration::ZERO` (no concurrent uploads exist yet).
fn cleanup_orphan_relay_entries_at(
    relay_root: &Path,
    active_relay_dirs: &HashSet<PathBuf>,
    min_age: std::time::Duration,
) -> (u64, u64) {
    let mut dirs_removed = 0u64;
    let mut files_removed = 0u64;
    let Ok(entries) = fs::read_dir(relay_root) else {
        return (0, 0);
    };

    for entry in entries.flatten() {
        let path = entry.path();
        // Skip entries younger than min_age — they may be an in-flight upload's
        // relay dir created after the active-set snapshot.
        if !min_age.is_zero() {
            let recent = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|mt| mt.elapsed().ok())
                .map(|age| age < min_age)
                .unwrap_or(false);
            if recent {
                continue;
            }
        }
        if path.is_dir() {
            if active_relay_dirs.contains(&path) {
                continue;
            }
            if fs::remove_dir_all(&path).is_ok() {
                dirs_removed += 1;
            }
        } else if fs::remove_file(&path).is_ok() {
            files_removed += 1;
        }
    }
    (dirs_removed, files_removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_and_upload_part() {
        let store = MultipartStore::new(100 * 1024 * 1024);
        let upload_id = store
            .create("bucket", "key.bin", None, HashMap::new())
            .unwrap();

        let data = Bytes::from(vec![0u8; 1024]);
        let etag = store
            .upload_part(&upload_id, "bucket", "key.bin", 1, data)
            .unwrap();
        assert!(etag.starts_with('"'));
        assert!(etag.ends_with('"'));
    }

    #[test]
    fn test_complete_roundtrip() {
        let store = MultipartStore::new(100 * 1024 * 1024);
        let upload_id = store
            .create("bucket", "key.bin", None, HashMap::new())
            .unwrap();

        let part1 = Bytes::from(vec![1u8; 100]);
        let part2 = Bytes::from(vec![2u8; 200]);
        let etag1 = store
            .upload_part(&upload_id, "bucket", "key.bin", 1, part1.clone())
            .unwrap();
        let etag2 = store
            .upload_part(&upload_id, "bucket", "key.bin", 2, part2.clone())
            .unwrap();

        let result = store
            .complete(&upload_id, "bucket", "key.bin", &[(1, etag1), (2, etag2)])
            .unwrap();

        assert_eq!(result.data.len(), 300);
        assert_eq!(&result.data[..100], &[1u8; 100]);
        assert_eq!(&result.data[100..], &[2u8; 200]);
        assert!(result.etag.ends_with("-2\""));
    }

    #[test]
    fn test_abort() {
        let store = MultipartStore::new(100 * 1024 * 1024);
        let upload_id = store
            .create("bucket", "key.bin", None, HashMap::new())
            .unwrap();
        store.abort(&upload_id, "bucket", "key.bin").unwrap();

        let result = store.upload_part(
            &upload_id,
            "bucket",
            "key.bin",
            1,
            Bytes::from(vec![0u8; 10]),
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_bucket_key_mismatch() {
        let store = MultipartStore::new(100 * 1024 * 1024);
        let upload_id = store
            .create("bucket-a", "key.bin", None, HashMap::new())
            .unwrap();

        let result = store.upload_part(
            &upload_id,
            "bucket-b",
            "key.bin",
            1,
            Bytes::from(vec![0u8; 10]),
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_invalid_part_number() {
        let store = MultipartStore::new(100 * 1024 * 1024);
        let upload_id = store
            .create("bucket", "key.bin", None, HashMap::new())
            .unwrap();

        let result = store.upload_part(
            &upload_id,
            "bucket",
            "key.bin",
            0,
            Bytes::from(vec![0u8; 10]),
        );
        assert!(result.is_err());

        let result = store.upload_part(
            &upload_id,
            "bucket",
            "key.bin",
            10001,
            Bytes::from(vec![0u8; 10]),
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_list_parts() {
        let store = MultipartStore::new(100 * 1024 * 1024);
        let upload_id = store
            .create("bucket", "key.bin", None, HashMap::new())
            .unwrap();

        for i in 1..=3 {
            store
                .upload_part(
                    &upload_id,
                    "bucket",
                    "key.bin",
                    i,
                    Bytes::from(vec![i as u8; 100]),
                )
                .unwrap();
        }

        let parts = store.list_parts(&upload_id, "bucket", "key.bin").unwrap();
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].part_number, 1);
        assert_eq!(parts[1].part_number, 2);
        assert_eq!(parts[2].part_number, 3);
    }

    #[test]
    fn test_overwrite_part() {
        let store = MultipartStore::new(100 * 1024 * 1024);
        let upload_id = store
            .create("bucket", "key.bin", None, HashMap::new())
            .unwrap();

        let etag1 = store
            .upload_part(
                &upload_id,
                "bucket",
                "key.bin",
                1,
                Bytes::from(vec![1u8; 100]),
            )
            .unwrap();
        let etag2 = store
            .upload_part(
                &upload_id,
                "bucket",
                "key.bin",
                1,
                Bytes::from(vec![2u8; 100]),
            )
            .unwrap();

        assert_ne!(etag1, etag2);

        let parts = store.list_parts(&upload_id, "bucket", "key.bin").unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].etag, etag2);
    }

    #[test]
    fn test_complete_with_zero_parts() {
        let store = MultipartStore::new(100 * 1024 * 1024);
        let upload_id = store
            .create("bucket", "key.bin", None, HashMap::new())
            .unwrap();
        store
            .upload_part(
                &upload_id,
                "bucket",
                "key.bin",
                1,
                Bytes::from(vec![1u8; 100]),
            )
            .unwrap();

        // Complete with empty parts list should fail
        let result = store.complete(&upload_id, "bucket", "key.bin", &[]);
        assert!(result.is_err(), "complete with zero parts should fail");
    }

    #[test]
    fn test_complete_with_wrong_etag() {
        let store = MultipartStore::new(100 * 1024 * 1024);
        let upload_id = store
            .create("bucket", "key.bin", None, HashMap::new())
            .unwrap();
        store
            .upload_part(
                &upload_id,
                "bucket",
                "key.bin",
                1,
                Bytes::from(vec![1u8; 100]),
            )
            .unwrap();

        // Complete with wrong etag should fail
        let result = store.complete(
            &upload_id,
            "bucket",
            "key.bin",
            &[(1, "\"wrong_etag\"".to_string())],
        );
        assert!(result.is_err(), "complete with wrong etag should fail");
    }

    #[test]
    fn test_complete_with_non_contiguous_parts() {
        let store = MultipartStore::new(100 * 1024 * 1024);
        let upload_id = store
            .create("bucket", "key.bin", None, HashMap::new())
            .unwrap();

        let part1 = Bytes::from(vec![1u8; 100]);
        let part3 = Bytes::from(vec![3u8; 100]);
        let etag1 = store
            .upload_part(&upload_id, "bucket", "key.bin", 1, part1)
            .unwrap();
        let etag3 = store
            .upload_part(&upload_id, "bucket", "key.bin", 3, part3)
            .unwrap();

        // Parts 1 and 3 (skip 2) — should succeed per S3 spec
        let result = store
            .complete(&upload_id, "bucket", "key.bin", &[(1, etag1), (3, etag3)])
            .unwrap();
        assert_eq!(result.data.len(), 200);
        assert_eq!(&result.data[..100], &[1u8; 100]);
        assert_eq!(&result.data[100..], &[3u8; 100]);
    }

    #[test]
    fn test_max_uploads_limit() {
        let store = MultipartStore::new(100 * 1024 * 1024);
        // Override max_uploads for testing
        let store = MultipartStore {
            max_uploads: 3,
            ..store
        };

        // Create 3 uploads (at limit)
        for i in 0..3 {
            store
                .create("bucket", &format!("key{}.bin", i), None, HashMap::new())
                .unwrap();
        }

        // 4th upload should fail
        let result = store.create("bucket", "key3.bin", None, HashMap::new());
        assert!(result.is_err());
    }

    // === C4 security fix: state-machine tests ===

    fn seed_upload(store: &MultipartStore) -> String {
        let upload_id = store
            .create("bucket", "key.bin", None, HashMap::new())
            .unwrap();
        let data = Bytes::from(vec![0u8; 100]);
        store
            .upload_part(&upload_id, "bucket", "key.bin", 1, data)
            .unwrap();
        upload_id
    }

    #[test]
    fn test_complete_flips_state_to_completing() {
        let store = MultipartStore::new(100 * 1024 * 1024);
        let upload_id = seed_upload(&store);
        let etag = {
            let u = store.uploads.read();
            let p = u.get(&upload_id).unwrap().parts.get(&1).unwrap();
            format!("\"{}\"", p.md5_hex)
        };

        // Before complete → Open.
        assert_eq!(
            store.uploads.read().get(&upload_id).unwrap().state,
            MultipartState::Open
        );

        store
            .complete(&upload_id, "bucket", "key.bin", &[(1, etag)])
            .unwrap();

        // After complete, upload stays in map but as Completing.
        assert_eq!(
            store.uploads.read().get(&upload_id).unwrap().state,
            MultipartState::Completing
        );
    }

    #[test]
    fn test_abort_refused_when_completing() {
        let store = MultipartStore::new(100 * 1024 * 1024);
        let upload_id = seed_upload(&store);
        let etag = {
            let u = store.uploads.read();
            let p = u.get(&upload_id).unwrap().parts.get(&1).unwrap();
            format!("\"{}\"", p.md5_hex)
        };

        store
            .complete(&upload_id, "bucket", "key.bin", &[(1, etag)])
            .unwrap();

        let err = store.abort(&upload_id, "bucket", "key.bin").unwrap_err();
        assert!(matches!(err, S3Error::InvalidRequest(_)));
        // Upload still in map, still Completing.
        assert_eq!(
            store.uploads.read().get(&upload_id).unwrap().state,
            MultipartState::Completing
        );
    }

    /// C-P0-1 regression: `purge_uploads_for_bucket` must NOT remove
    /// uploads that are in `Completing` state. Doing so would tear down
    /// state that the in-flight `engine.store_*` handler still has
    /// borrowed paths/buffers for; the storage layer's `create_dir_all`
    /// would then race to resurrect a bucket the operator just deleted.
    ///
    /// Pre-fix: `purge_uploads_for_bucket` silently removed Completing
    /// uploads and returned a usize. Post-fix: it returns
    /// `Err(count_completing)` when any Completing upload targets the
    /// bucket; `delete_bucket` translates that to `BucketNotEmpty`.
    #[test]
    fn test_purge_for_bucket_refuses_when_completing() {
        let store = MultipartStore::new(100 * 1024 * 1024);

        // One Open upload in `bucket-a` — would be safe to purge alone.
        let _ = seed_upload(&store); // bucket="bucket", key="key.bin"

        // Second upload, drive it into Completing.
        let upload_b = store
            .create("bucket", "other.bin", None, HashMap::new())
            .unwrap();
        store
            .upload_part(
                &upload_b,
                "bucket",
                "other.bin",
                1,
                Bytes::from_static(b"x"),
            )
            .unwrap();
        let etag = {
            let u = store.uploads.read();
            let p = u.get(&upload_b).unwrap().parts.get(&1).unwrap();
            format!("\"{}\"", p.md5_hex)
        };
        store
            .complete(&upload_b, "bucket", "other.bin", &[(1, etag)])
            .unwrap();
        assert_eq!(
            store.uploads.read().get(&upload_b).unwrap().state,
            MultipartState::Completing,
            "second upload should be Completing"
        );

        // Purge must refuse, with the count of Completing uploads as
        // the error payload. Nothing must be removed (all-or-nothing).
        let result = store.purge_uploads_for_bucket("bucket");
        assert_eq!(result, Err(1), "must refuse with completing count");
        assert_eq!(
            store.uploads.read().len(),
            2,
            "must not have partially purged"
        );
    }

    /// Sister test: when ALL uploads for the bucket are `Open`, purge
    /// proceeds and returns the count purged. Sanity check that the new
    /// signature didn't break the happy path.
    #[test]
    fn test_purge_for_bucket_proceeds_when_all_open() {
        let store = MultipartStore::new(100 * 1024 * 1024);
        let _ = seed_upload(&store);
        let _ = store
            .create("bucket", "other.bin", None, HashMap::new())
            .unwrap();
        // Different bucket — must NOT be purged.
        let _ = store
            .create("other-bucket", "elsewhere.bin", None, HashMap::new())
            .unwrap();

        let result = store.purge_uploads_for_bucket("bucket");
        assert_eq!(result, Ok(2), "purges Open uploads in target bucket");
        assert_eq!(
            store.uploads.read().len(),
            1,
            "leaves the other-bucket upload alone"
        );
    }

    #[test]
    fn test_upload_part_refused_when_completing() {
        let store = MultipartStore::new(100 * 1024 * 1024);
        let upload_id = seed_upload(&store);
        let etag = {
            let u = store.uploads.read();
            let p = u.get(&upload_id).unwrap().parts.get(&1).unwrap();
            format!("\"{}\"", p.md5_hex)
        };
        store
            .complete(&upload_id, "bucket", "key.bin", &[(1, etag)])
            .unwrap();

        let err = store
            .upload_part(
                &upload_id,
                "bucket",
                "key.bin",
                2,
                Bytes::from(vec![0u8; 50]),
            )
            .unwrap_err();
        assert!(matches!(err, S3Error::InvalidRequest(_)));
    }

    #[test]
    fn test_rollback_upload_returns_to_open() {
        let store = MultipartStore::new(100 * 1024 * 1024);
        let upload_id = seed_upload(&store);
        let etag = {
            let u = store.uploads.read();
            let p = u.get(&upload_id).unwrap().parts.get(&1).unwrap();
            format!("\"{}\"", p.md5_hex)
        };
        store
            .complete(&upload_id, "bucket", "key.bin", &[(1, etag)])
            .unwrap();

        // Simulate engine.store* failure → rollback.
        store.rollback_upload(&upload_id);

        assert_eq!(
            store.uploads.read().get(&upload_id).unwrap().state,
            MultipartState::Open
        );

        // Client can now retry Complete or abort.
        store.abort(&upload_id, "bucket", "key.bin").unwrap();
    }

    #[test]
    fn test_finish_upload_removes_entry() {
        let store = MultipartStore::new(100 * 1024 * 1024);
        let upload_id = seed_upload(&store);
        let etag = {
            let u = store.uploads.read();
            let p = u.get(&upload_id).unwrap().parts.get(&1).unwrap();
            format!("\"{}\"", p.md5_hex)
        };
        store
            .complete(&upload_id, "bucket", "key.bin", &[(1, etag)])
            .unwrap();

        store.finish_upload(&upload_id);

        assert!(store.uploads.read().get(&upload_id).is_none());
    }

    #[test]
    fn test_double_complete_returns_conflict() {
        let store = MultipartStore::new(100 * 1024 * 1024);
        let upload_id = seed_upload(&store);
        let etag = {
            let u = store.uploads.read();
            let p = u.get(&upload_id).unwrap().parts.get(&1).unwrap();
            format!("\"{}\"", p.md5_hex)
        };

        store
            .complete(&upload_id, "bucket", "key.bin", &[(1, etag.clone())])
            .unwrap();
        let err = store
            .complete(&upload_id, "bucket", "key.bin", &[(1, etag)])
            .unwrap_err();
        assert!(
            matches!(err, S3Error::InvalidRequest(_)),
            "double-complete should return InvalidRequest while in Completing, got {:?}",
            err
        );
    }

    #[test]
    fn test_validation_failure_does_not_change_state() {
        // If complete() fails validation (wrong etag), state must stay Open
        // so the client can retry with correct metadata.
        let store = MultipartStore::new(100 * 1024 * 1024);
        let upload_id = seed_upload(&store);

        let err = store
            .complete(
                &upload_id,
                "bucket",
                "key.bin",
                &[(1, "\"wrong-etag\"".to_string())],
            )
            .unwrap_err();
        assert!(matches!(err, S3Error::InvalidPart(_)));
        assert_eq!(
            store.uploads.read().get(&upload_id).unwrap().state,
            MultipartState::Open,
            "validation failure must leave upload Open for retry"
        );
    }

    #[test]
    fn test_abort_while_open_drops_upload() {
        // Baseline: abort on an Open upload still works normally.
        let store = MultipartStore::new(100 * 1024 * 1024);
        let upload_id = seed_upload(&store);
        store.abort(&upload_id, "bucket", "key.bin").unwrap();
        assert!(store.uploads.read().get(&upload_id).is_none());
    }

    // === C3 DoS fix: size-cap + global-counter + TTL sweeper tests ===

    #[test]
    fn test_upload_part_rejects_when_cumulative_exceeds_max_object_size() {
        // max_object_size = 1 KiB. Upload 700 B + 200 B → OK, 300 B → rejected.
        let store = MultipartStore::new(1024);
        let upload_id = store.create("bucket", "key", None, HashMap::new()).unwrap();
        store
            .upload_part(&upload_id, "bucket", "key", 1, Bytes::from(vec![0u8; 700]))
            .unwrap();
        store
            .upload_part(&upload_id, "bucket", "key", 2, Bytes::from(vec![0u8; 200]))
            .unwrap();
        let err = store
            .upload_part(&upload_id, "bucket", "key", 3, Bytes::from(vec![0u8; 300]))
            .unwrap_err();
        assert!(
            matches!(err, S3Error::EntityTooLarge { size, max } if size == 1200 && max == 1024),
            "got {:?}",
            err
        );
    }

    #[test]
    fn test_upload_part_overwrite_adjusts_cumulative_correctly() {
        // Overwrite a 1000 B part with 200 B — cumulative goes DOWN, not up.
        let store = MultipartStore::new(1500);
        let upload_id = store.create("bucket", "key", None, HashMap::new()).unwrap();
        store
            .upload_part(&upload_id, "bucket", "key", 1, Bytes::from(vec![0u8; 1000]))
            .unwrap();
        // Add 400 more via a second part — total 1400, under cap.
        store
            .upload_part(&upload_id, "bucket", "key", 2, Bytes::from(vec![0u8; 400]))
            .unwrap();
        // Now overwrite part 1 with 200 B. New cumulative = 200 + 400 = 600.
        store
            .upload_part(&upload_id, "bucket", "key", 1, Bytes::from(vec![0u8; 200]))
            .unwrap();
        // Counter should reflect the overwrite.
        assert_eq!(store.in_flight_bytes(), 600);
    }

    #[test]
    fn test_upload_part_respects_global_byte_cap() {
        // Tight global cap: 2 KiB total across all uploads.
        let store = MultipartStore::new_for_test(10 * 1024, 2 * 1024, Duration::hours(24));
        let id_a = store.create("b", "a", None, HashMap::new()).unwrap();
        let id_b = store.create("b", "b", None, HashMap::new()).unwrap();

        // Fill upload A to 1 KiB.
        store
            .upload_part(&id_a, "b", "a", 1, Bytes::from(vec![0u8; 1024]))
            .unwrap();
        // Fill upload B to 1 KiB (total now 2 KiB = cap).
        store
            .upload_part(&id_b, "b", "b", 1, Bytes::from(vec![0u8; 1024]))
            .unwrap();
        // Next byte anywhere → SlowDown.
        let err = store
            .upload_part(&id_a, "b", "a", 2, Bytes::from(vec![0u8; 1]))
            .unwrap_err();
        assert!(matches!(err, S3Error::SlowDown(_)), "got {:?}", err);
    }

    #[test]
    fn test_abort_releases_in_flight_bytes() {
        let store = MultipartStore::new_for_test(10 * 1024, 2 * 1024, Duration::hours(24));
        let id = store.create("b", "a", None, HashMap::new()).unwrap();
        store
            .upload_part(&id, "b", "a", 1, Bytes::from(vec![0u8; 1024]))
            .unwrap();
        assert_eq!(store.in_flight_bytes(), 1024);

        store.abort(&id, "b", "a").unwrap();
        assert_eq!(
            store.in_flight_bytes(),
            0,
            "abort must release bytes to the global counter"
        );
    }

    #[test]
    fn test_finish_upload_releases_in_flight_bytes() {
        let store = MultipartStore::new_for_test(10 * 1024, 10 * 1024, Duration::hours(24));
        let id = store.create("b", "k", None, HashMap::new()).unwrap();
        let data = Bytes::from(vec![0u8; 500]);
        let etag = store.upload_part(&id, "b", "k", 1, data).unwrap();
        assert_eq!(store.in_flight_bytes(), 500);

        store.complete(&id, "b", "k", &[(1, etag)]).unwrap();
        // Still in map (Completing) — counter unchanged.
        assert_eq!(store.in_flight_bytes(), 500);

        store.finish_upload(&id);
        assert_eq!(store.in_flight_bytes(), 0);
    }

    #[test]
    fn test_cleanup_expired_idle_ttl_sweeps_and_releases_bytes() {
        // Tiny idle TTL so we can trip it synchronously.
        let store = MultipartStore::new_for_test(10 * 1024, 10 * 1024, Duration::milliseconds(1));
        let id = store.create("b", "k", None, HashMap::new()).unwrap();
        store
            .upload_part(&id, "b", "k", 1, Bytes::from(vec![0u8; 700]))
            .unwrap();
        assert_eq!(store.in_flight_bytes(), 700);

        // Sleep past the idle TTL.
        std::thread::sleep(std::time::Duration::from_millis(5));
        let report = store.cleanup_expired(
            std::time::Duration::from_secs(3600),
            std::time::Duration::from_secs(3600),
        );
        assert_eq!(report.swept_open_uploads, 1);

        assert!(
            store.uploads.read().get(&id).is_none(),
            "idle upload should have been swept"
        );
        assert_eq!(
            store.in_flight_bytes(),
            0,
            "sweep must release bytes to the global counter"
        );
    }

    #[test]
    fn test_cleanup_expired_preserves_recent_completing_upload() {
        // Completing uploads should survive until completing_timeout elapses.
        let store = MultipartStore::new_for_test(10 * 1024, 10 * 1024, Duration::hours(24));
        let id = store.create("b", "k", None, HashMap::new()).unwrap();
        let etag = store
            .upload_part(&id, "b", "k", 1, Bytes::from(vec![0u8; 100]))
            .unwrap();
        store.complete(&id, "b", "k", &[(1, etag)]).unwrap();

        let report = store.cleanup_expired(
            std::time::Duration::from_secs(3600),
            std::time::Duration::from_secs(3600),
        );
        assert_eq!(report.swept_completing_uploads, 0);

        assert!(
            store.uploads.read().get(&id).is_some(),
            "recent Completing uploads must be preserved"
        );
    }

    #[test]
    fn test_cleanup_expired_sweeps_stuck_completing_upload() {
        let store = MultipartStore::new_for_test(10 * 1024, 10 * 1024, Duration::hours(24));
        let id = store.create("b", "k", None, HashMap::new()).unwrap();
        let etag = store
            .upload_part(&id, "b", "k", 1, Bytes::from(vec![0u8; 100]))
            .unwrap();
        store.complete(&id, "b", "k", &[(1, etag)]).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));

        let report = store.cleanup_expired(
            std::time::Duration::from_secs(3600),
            std::time::Duration::from_millis(1),
        );
        assert_eq!(report.swept_completing_uploads, 1);
        assert_eq!(store.in_flight_bytes(), 0);
        assert!(store.uploads.read().get(&id).is_none());
    }

    #[test]
    fn test_cleanup_expired_spares_completing_upload_with_store_in_flight() {
        // H18: a Completing upload past completing_timeout must NOT be swept
        // while its engine store is in flight (store_in_progress), else the relay
        // parts the store is reading get deleted and are lost on retry.
        let store = std::sync::Arc::new(MultipartStore::new_for_test(
            10 * 1024,
            10 * 1024,
            Duration::hours(24),
        ));
        let id = store.create("b", "k", None, HashMap::new()).unwrap();
        let etag = store
            .upload_part(&id, "b", "k", 1, Bytes::from(vec![0u8; 100]))
            .unwrap();
        store.complete(&id, "b", "k", &[(1, etag)]).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));

        // Store in flight → spared even with a 1ms completing_timeout.
        let guard = store.store_guard(&id);
        let report = store.cleanup_expired(
            std::time::Duration::from_secs(3600),
            std::time::Duration::from_millis(1),
        );
        assert_eq!(
            report.swept_completing_uploads, 0,
            "an in-flight store must protect its Completing upload from the sweeper"
        );
        assert!(store.uploads.read().get(&id).is_some());

        // Once the store finishes (guard dropped), the sweeper reclaims it.
        drop(guard);
        let report = store.cleanup_expired(
            std::time::Duration::from_secs(3600),
            std::time::Duration::from_millis(1),
        );
        assert_eq!(report.swept_completing_uploads, 1);
        assert!(store.uploads.read().get(&id).is_none());
    }

    #[test]
    fn test_cleanup_orphan_relay_entries_removes_untracked_entries() {
        let dir = tempfile::tempdir().unwrap();
        let active_dir = dir.path().join("active");
        let orphan_dir = dir.path().join("orphan");
        let orphan_file = dir.path().join("stray.tmp");
        fs::create_dir_all(&active_dir).unwrap();
        fs::create_dir_all(&orphan_dir).unwrap();
        fs::write(orphan_dir.join("part-00001.bin"), b"orphan").unwrap();
        fs::write(&orphan_file, b"stray").unwrap();

        let mut active = HashSet::new();
        active.insert(active_dir.clone());
        let (dirs_removed, files_removed) =
            cleanup_orphan_relay_entries_at(dir.path(), &active, std::time::Duration::ZERO);

        assert_eq!(dirs_removed, 1);
        assert_eq!(files_removed, 1);
        assert!(active_dir.exists(), "active relay dir must be preserved");
        assert!(!orphan_dir.exists(), "orphan relay dir must be removed");
        assert!(!orphan_file.exists(), "orphan relay file must be removed");
    }

    #[test]
    fn orphan_sweep_min_age_spares_recent_entries() {
        // H19: a freshly-created (untracked) relay dir — as a concurrent
        // promotion would produce after the active-set snapshot — must NOT be
        // deleted by the periodic sweep when a min_age guard is set.
        let dir = tempfile::tempdir().unwrap();
        let fresh_orphan = dir.path().join("fresh");
        fs::create_dir_all(&fresh_orphan).unwrap();
        fs::write(fresh_orphan.join("part-00001.bin"), b"in-flight").unwrap();

        let active = HashSet::new(); // nothing tracked yet (mid-promotion)
        let (dirs_removed, _files) = cleanup_orphan_relay_entries_at(
            dir.path(),
            &active,
            std::time::Duration::from_secs(3600),
        );
        assert_eq!(dirs_removed, 0, "a recent dir must be spared by min_age");
        assert!(fresh_orphan.exists(), "the in-flight relay dir must survive");
    }

    #[test]
    fn relayed_part_load_bytes_rejects_substituted_content() {
        // H20: a relay part file swapped on disk (shared-host local attacker)
        // must be REJECTED at read time — the recorded MD5 no longer matches.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("part-00001.bin");
        let good = Bytes::from_static(b"the-real-part-bytes");
        fs::write(&path, &good).unwrap();
        let expected: [u8; 16] = Md5::digest(&good).into();

        let payload = PartPayload::RelayedFile(path.clone());
        // Untouched → loads fine.
        assert_eq!(payload.load_bytes(&expected).unwrap(), good);

        // Attacker swaps the file content → integrity check fails.
        fs::write(&path, b"attacker-controlled-bytes").unwrap();
        let err = payload.load_bytes(&expected).unwrap_err();
        assert!(
            format!("{err:?}").contains("integrity check"),
            "substituted part must be rejected, got {err:?}"
        );
    }

    #[test]
    fn test_relay_promotion_on_threshold_cross() {
        let store = MultipartStore::new(10 * 1024);
        let id = store
            .create_with_relay_policy("b", "k", None, HashMap::new(), Some(512), false)
            .unwrap();

        store
            .upload_part(&id, "b", "k", 1, Bytes::from(vec![0u8; 256]))
            .unwrap();
        {
            let uploads = store.uploads.read();
            let upload = uploads.get(&id).unwrap();
            assert!(matches!(
                upload.relay_strategy,
                RelayStrategy::InMemory { .. }
            ));
        }

        store
            .upload_part(&id, "b", "k", 2, Bytes::from(vec![1u8; 300]))
            .unwrap();
        let uploads = store.uploads.read();
        let upload = uploads.get(&id).unwrap();
        assert!(matches!(
            upload.relay_strategy,
            RelayStrategy::Relayed { .. }
        ));
        let part1 = upload.parts.get(&1).unwrap();
        let part2 = upload.parts.get(&2).unwrap();
        assert!(matches!(part1.payload, PartPayload::RelayedFile(_)));
        assert!(matches!(part2.payload, PartPayload::RelayedFile(_)));
    }

    #[test]
    fn test_complete_passthrough_returns_relayed_file_payload() {
        let store = MultipartStore::new(10 * 1024);
        let id = store
            .create_with_relay_policy("b", "k", None, HashMap::new(), None, true)
            .unwrap();
        let e1 = store
            .upload_part(&id, "b", "k", 1, Bytes::from_static(b"hello"))
            .unwrap();
        let e2 = store
            .upload_part(&id, "b", "k", 2, Bytes::from_static(b"world"))
            .unwrap();

        let completed = store
            .complete_passthrough(&id, "b", "k", &[(1, e1), (2, e2)])
            .unwrap();
        assert_eq!(completed.total_size, 10);
        match completed.payload {
            PassthroughPayload::RelayedParts(paths) => {
                assert_eq!(paths.len(), 2);
                let mut data = Vec::new();
                for path in paths {
                    data.extend_from_slice(&std::fs::read(path).unwrap());
                }
                assert_eq!(data, b"helloworld");
            }
            PassthroughPayload::Chunks(_) => {
                panic!("expected relayed part payload for always-relay upload")
            }
        }
    }
}
