// SPDX-License-Identifier: BUSL-1.1

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

use crate::deltaglider::spool::{mib_ceil, SpoolDir, SpoolReservation, CONTENDED};

/// Relay root, under the spool dir (it was under the system temp dir).
const RELAY_ROOT_DIR: &str = "deltaglider-mpu-relay";

/// Env var: the relay bytes one multipart upload may hold in the spool.
pub const RELAY_UPLOAD_MAX_ENV: &str = "DGP_SPOOL_RELAY_UPLOAD_MAX_BYTES";

/// Pure: the relay bytes one upload may hold, for a spool of
/// `spool_max_bytes` and the configured `DGP_SPOOL_RELAY_UPLOAD_MAX_BYTES`
/// (`None` = unset). Default: half of the spool, so one upload never pins
/// the budget of every other request. `0` = no per-upload cap (only the
/// global budget). `None` in the result means no cap.
pub fn relay_upload_cap(spool_max_bytes: u64, configured: Option<u64>) -> Option<u64> {
    match configured {
        Some(0) => None,
        Some(n) => Some(n),
        None => Some((spool_max_bytes / 2).max(1024 * 1024)),
    }
}

/// Data for a single uploaded part
enum PartPayload {
    InMemory(Bytes),
    /// A relay file in the spool dir, and the spool budget it holds until
    /// the part is dropped (overwrite, abort, complete, sweep). The
    /// reservation is never read: it is held for its Drop.
    RelayedFile(PathBuf, #[allow(dead_code)] SpoolReservation),
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
            Self::RelayedFile(path, _) => {
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
/// Outcome of a completion, shareable with retried Complete requests.
pub type CompletionResult = Result<String, CompletionFailure>;

/// A failed completion as the owner answered it: a joined retry answers the
/// same S3 error, not a 500 for an owner's 400 (s3surface-17).
#[derive(Debug, Clone, PartialEq)]
pub struct CompletionFailure {
    pub code: s3s::S3ErrorCode,
    pub message: Option<String>,
}

impl CompletionFailure {
    pub fn of(err: &s3s::S3Error) -> Self {
        Self {
            code: err.code().clone(),
            message: err.message().map(str::to_string),
        }
    }

    pub fn internal(message: &str) -> Self {
        Self {
            code: s3s::S3ErrorCode::InternalError,
            message: Some(message.to_string()),
        }
    }

    pub fn to_s3s(&self) -> s3s::S3Error {
        match &self.message {
            Some(message) => s3s::S3Error::with_message(self.code.clone(), message.clone()),
            None => s3s::S3Error::new(self.code.clone()),
        }
    }
}

const COMPLETION_TOMBSTONE_TTL_SECS: u64 = 900;

enum CompletionSlot {
    InFlight {
        fingerprint: u64,
        rx: tokio::sync::watch::Receiver<Option<CompletionResult>>,
        started_at: std::time::Instant,
    },
    Done {
        fingerprint: u64,
        etag: String,
        bucket: String,
        key: String,
        at: std::time::Instant,
    },
}

/// What a CompleteMultipartUpload request should do (see `begin_complete`).
pub enum BeginComplete {
    Owner(CompletionPublisher),
    Join(tokio::sync::watch::Receiver<Option<CompletionResult>>),
    AlreadyDone { etag: String },
}

/// Held by the owning completion task; publishes the outcome to joiners and
/// the tombstone. Dropping without publishing (panic/abort) publishes an error
/// and clears the slot so later attempts aren't wedged.
pub struct CompletionPublisher {
    store: std::sync::Arc<MultipartStore>,
    upload_id: String,
    fingerprint: u64,
    bucket: String,
    key: String,
    tx: tokio::sync::watch::Sender<Option<CompletionResult>>,
    published: bool,
}

impl CompletionPublisher {
    pub fn publish(mut self, result: CompletionResult) {
        self.published = true;
        self.store.publish_completion(
            &self.upload_id,
            self.fingerprint,
            &self.bucket,
            &self.key,
            &result,
        );
        let _ = self.tx.send(Some(result));
    }
}

impl Drop for CompletionPublisher {
    fn drop(&mut self) {
        if !self.published {
            self.store.clear_completion_slot(&self.upload_id);
            let _ = self.tx.send(Some(Err(CompletionFailure::internal(
                "completion task aborted before finishing",
            ))));
        }
    }
}

/// Order-sensitive digest of the requested part list: retries with the same
/// parts join/match; a different list is a different completion.
pub fn completion_fingerprint(parts: &[(u32, String)]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for (n, e) in parts {
        n.hash(&mut h);
        e.hash(&mut h);
    }
    h.finish()
}

pub struct MultipartStore {
    uploads: RwLock<HashMap<String, MultipartUpload>>,
    /// CompleteMultipartUpload registry: in-flight completions + success tombstones.
    completions: parking_lot::Mutex<HashMap<String, CompletionSlot>>,
    /// Per-upload byte cap. Atomic so a hot config apply reaches it: the S3
    /// adapter refreshes it from the live engine on every part and complete.
    max_object_size: std::sync::atomic::AtomicU64,
    max_uploads: usize,
    /// Global in-flight bytes across all uploads. Kept consistent with
    /// the sum of `MultipartUpload.parts[*].size` — updated under the
    /// same write lock that mutates the parts map. Checked before each
    /// UploadPart accepts bytes (C3 DoS fix).
    in_flight_bytes: std::sync::atomic::AtomicU64,
    max_total_multipart_bytes: u64,
    idle_ttl: Duration,
    /// Relay part files live in this spool's dir and hold its budget.
    spool: SpoolDir,
    /// `DGP_SPOOL_RELAY_UPLOAD_MAX_BYTES` as read at construction.
    relay_upload_max: Option<u64>,
}

impl MultipartStore {
    pub fn new(max_object_size: u64) -> Self {
        let max_uploads = default_max_uploads();
        let max_total_multipart_bytes =
            default_max_total_multipart_bytes(max_object_size, max_uploads);
        let idle_ttl_hours = default_multipart_idle_ttl_hours();
        Self {
            uploads: RwLock::new(HashMap::new()),
            completions: parking_lot::Mutex::new(HashMap::new()),
            max_object_size: std::sync::atomic::AtomicU64::new(max_object_size),
            max_uploads,
            in_flight_bytes: std::sync::atomic::AtomicU64::new(0),
            max_total_multipart_bytes,
            idle_ttl: Duration::hours(idle_ttl_hours),
            spool: shared_spool(),
            relay_upload_max: crate::config::env_parse(RELAY_UPLOAD_MAX_ENV),
        }
    }

    /// Use `spool` for relay part files (default: the process-wide spool).
    pub fn with_spool(mut self, spool: SpoolDir) -> Self {
        self.spool = spool;
        self
    }

    /// Override `DGP_SPOOL_RELAY_UPLOAD_MAX_BYTES` (`Some(0)` = no cap).
    pub fn with_relay_upload_max(mut self, configured: Option<u64>) -> Self {
        self.relay_upload_max = configured;
        self
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
            completions: parking_lot::Mutex::new(HashMap::new()),
            max_object_size: std::sync::atomic::AtomicU64::new(max_object_size),
            max_uploads: 1000,
            in_flight_bytes: std::sync::atomic::AtomicU64::new(0),
            max_total_multipart_bytes,
            idle_ttl,
            spool: shared_spool(),
            relay_upload_max: None,
        }
    }

    /// Snapshot the global in-flight byte counter. Test-only observability.
    #[cfg(test)]
    pub(crate) fn in_flight_bytes(&self) -> u64 {
        self.in_flight_bytes
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Per-upload byte cap in force now.
    pub fn max_object_size(&self) -> u64 {
        self.max_object_size
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Adopt the engine's current `max_object_size` (it hot-reloads; the
    /// value captured at startup did not follow a config apply).
    pub fn set_max_object_size(&self, max_object_size: u64) {
        self.max_object_size
            .store(max_object_size, std::sync::atomic::Ordering::Relaxed);
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
                    relay_dir: self.relay_dir_for_upload(&upload_id),
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

        let max_object_size = self.max_object_size();
        if cumulative_after > max_object_size {
            return Err(S3Error::EntityTooLarge {
                size: cumulative_after,
                max: max_object_size,
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
            self.promote_upload_to_relay(upload)?;
        }

        let payload = match &upload.relay_strategy {
            RelayStrategy::InMemory { .. } => PartPayload::InMemory(data),
            RelayStrategy::Relayed { relay_dir } => {
                let budget = self.reserve_relay_part(upload, size, Some(part_number))?;
                let path = part_path(relay_dir, part_number);
                write_part_file(&path, &data)?;
                PartPayload::RelayedFile(path, budget)
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

    /// Route a CompleteMultipartUpload request through the completion registry.
    /// Exactly one request becomes the Owner and runs the store pipeline (on a
    /// DETACHED task, so a client disconnect cannot cancel it); identical retries
    /// Join the in-flight outcome or hit the success tombstone. This is what makes
    /// Complete disconnect-proof and retry-idempotent (found live: a router dying
    /// mid-Complete used to destroy the upload and poison the retry).
    pub fn begin_complete(
        self: &std::sync::Arc<Self>,
        upload_id: &str,
        bucket: &str,
        key: &str,
        requested_parts: &[(u32, String)],
    ) -> Result<BeginComplete, S3Error> {
        let fingerprint = completion_fingerprint(requested_parts);
        let mut slots = self.completions.lock();
        match slots.get(upload_id) {
            Some(CompletionSlot::Done {
                fingerprint: f,
                etag,
                bucket: b,
                key: k,
                ..
            }) if *f == fingerprint && b == bucket && k == key => {
                Ok(BeginComplete::AlreadyDone { etag: etag.clone() })
            }
            // The upload is complete, so it no longer exists: S3 answers
            // NoSuchUpload to a Complete that does not match it (s3surface-17).
            Some(CompletionSlot::Done { .. }) => Err(S3Error::NoSuchUpload(upload_id.to_string())),
            Some(CompletionSlot::InFlight {
                fingerprint: f, rx, ..
            }) if *f == fingerprint => Ok(BeginComplete::Join(rx.clone())),
            Some(CompletionSlot::InFlight { .. }) => Err(S3Error::InvalidRequest(
                "another CompleteMultipartUpload with a different part list is in flight"
                    .to_string(),
            )),
            None => {
                let (tx, rx) = tokio::sync::watch::channel(None);
                slots.insert(
                    upload_id.to_string(),
                    CompletionSlot::InFlight {
                        fingerprint,
                        rx,
                        started_at: std::time::Instant::now(),
                    },
                );
                Ok(BeginComplete::Owner(CompletionPublisher {
                    store: std::sync::Arc::clone(self),
                    upload_id: upload_id.to_string(),
                    fingerprint,
                    bucket: bucket.to_string(),
                    key: key.to_string(),
                    tx,
                    published: false,
                }))
            }
        }
    }

    fn publish_completion(
        &self,
        upload_id: &str,
        fingerprint: u64,
        bucket: &str,
        key: &str,
        result: &CompletionResult,
    ) {
        let mut slots = self.completions.lock();
        match result {
            Ok(etag) => {
                slots.insert(
                    upload_id.to_string(),
                    CompletionSlot::Done {
                        fingerprint,
                        etag: etag.clone(),
                        bucket: bucket.to_string(),
                        key: key.to_string(),
                        at: std::time::Instant::now(),
                    },
                );
            }
            // Failure clears the slot: the upload rolled back to Open, so the
            // next Complete attempt becomes a fresh Owner.
            Err(_) => {
                slots.remove(upload_id);
            }
        }
    }

    /// Drop success tombstones past their TTL, and self-heal InFlight slots whose
    /// task can no longer publish (aborted/never-polled): a wedged slot would make
    /// every future Complete for that id hang on a silent receiver (review MAJOR 2).
    fn prune_completion_tombstones(&self, completing_timeout: std::time::Duration) {
        let ttl = std::time::Duration::from_secs(COMPLETION_TOMBSTONE_TTL_SECS);
        let in_flight_bound = completing_timeout.saturating_mul(2).max(ttl);
        self.completions.lock().retain(|_, slot| match slot {
            CompletionSlot::Done { at, .. } => at.elapsed() < ttl,
            CompletionSlot::InFlight { started_at, .. } => started_at.elapsed() < in_flight_bound,
        });
    }

    /// Clear an upload's completion slot (publisher Drop path).
    fn clear_completion_slot(&self, upload_id: &str) {
        self.completions.lock().remove(upload_id);
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
            if total_size > self.max_object_size() {
                return Err(S3Error::InvalidArgument(format!(
                    "Assembled object size {} exceeds maximum {}",
                    total_size,
                    self.max_object_size()
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
        self.prune_completion_tombstones(completing_timeout);
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
    pub fn sweep_orphan_relay_artifacts(
        &self,
        min_age: std::time::Duration,
    ) -> MultipartSweepReport {
        let active_relay_dirs: HashSet<PathBuf> = self
            .uploads
            .read()
            .values()
            .filter_map(|u| match &u.relay_strategy {
                RelayStrategy::Relayed { relay_dir } => Some(relay_dir.clone()),
                RelayStrategy::InMemory { .. } => None,
            })
            .collect();
        let (mut dirs_removed, mut files_removed) =
            cleanup_orphan_relay_entries_at(&self.relay_root_dir(), &active_relay_dirs, min_age);

        // Foreign per-process roots (crashed/finished proxies): reap only STALE
        // leftovers — a young dir may belong to a live sibling process's upload.
        let foreign_min_age = std::time::Duration::from_secs(
            crate::config::env_parse_with_default("DGP_RELAY_FOREIGN_MIN_AGE_SECS", 3600),
        );
        let mine = self.relay_root_dir();
        let parents = [self.relay_parent_dir(), legacy_relay_parent_dir()];
        for entry in parents
            .iter()
            .filter_map(|p| fs::read_dir(p).ok())
            .flat_map(|entries| entries.flatten())
        {
            let path = entry.path();
            if path == mine || !path.is_dir() {
                continue;
            }
            let (fd, ff) = cleanup_orphan_relay_entries_at(
                &path,
                &HashSet::new(),
                min_age.max(foreign_min_age),
            );
            dirs_removed += fd;
            files_removed += ff;
            // Removes the pid dir only when empty; a live sibling keeps it.
            let _ = fs::remove_dir(&path);
        }
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

    fn promote_upload_to_relay(&self, upload: &mut MultipartUpload) -> Result<(), S3Error> {
        let relay_dir = self.relay_dir_for_upload(&upload.upload_id);
        fs::create_dir_all(&relay_dir).map_err(|e| {
            S3Error::InternalError(format!("Failed to create multipart relay directory: {}", e))
        })?;
        let in_memory: Vec<u32> = upload
            .parts
            .iter()
            .filter(|(_, p)| matches!(p.payload, PartPayload::InMemory(_)))
            .map(|(n, _)| *n)
            .collect();
        for part_number in in_memory {
            let size = upload.parts[&part_number].size;
            let budget = self.reserve_relay_part(upload, size, None)?;
            let part = upload.parts.get_mut(&part_number).expect("collected above");
            if let PartPayload::InMemory(bytes) = &part.payload {
                let path = part_path(&relay_dir, part_number);
                write_part_file(&path, bytes)?;
                part.payload = PartPayload::RelayedFile(path, budget);
            }
        }
        upload.relay_strategy = RelayStrategy::Relayed { relay_dir };
        Ok(())
    }

    /// Spool budget for one relay part of `size` bytes. Never waits: this
    /// runs under the uploads lock, and an upload that holds relay parts is
    /// a holder (the review2 rule). A full budget is a retryable SlowDown.
    ///
    /// One upload holds at most [`relay_upload_cap`] of the budget. The
    /// reservation used to be clamped to what the upload did not hold yet,
    /// so once an upload held the whole budget every further part reserved
    /// 0 MiB and was written anyway: the disk use of one upload grew without
    /// a bound while it pinned the budget of every other request.
    fn reserve_relay_part(
        &self,
        upload: &MultipartUpload,
        size: u64,
        replacing: Option<u32>,
    ) -> Result<SpoolReservation, S3Error> {
        let held: u64 = upload
            .parts
            .iter()
            .filter(|(n, p)| {
                Some(**n) != replacing && matches!(p.payload, PartPayload::RelayedFile(..))
            })
            .map(|(_, p)| p.size)
            .sum();
        let want = held.saturating_add(size);
        if let Some(cap) = relay_upload_cap(self.spool.max_bytes(), self.relay_upload_max) {
            if want > cap {
                return Err(S3Error::EntityTooLargeReason(format!(
                    "this multipart upload would hold {want} bytes of relay parts in the spool; \
                     one upload may hold at most {cap} bytes ({RELAY_UPLOAD_MAX_ENV}, default \
                     half of DGP_SPOOL_MAX_BYTES; 0 = no per-upload cap)"
                )));
            }
        }
        let reservation = self.spool.try_reserve(size, mib_ceil(held)).map_err(|e| {
            if e.kind() == CONTENDED {
                S3Error::SlowDown(format!("spool budget in use by other requests: {e}"))
            } else {
                S3Error::InternalError(format!("spool reservation failed: {e}"))
            }
        })?;
        // Never write a part the reservation does not cover.
        if reservation.reserved_mib() < mib_ceil(size) {
            return Err(S3Error::SlowDown(
                "spool budget in use by other requests".to_string(),
            ));
        }
        Ok(reservation)
    }

    /// Parent of all per-process relay roots on this host.
    fn relay_parent_dir(&self) -> PathBuf {
        self.spool.dir().join(RELAY_ROOT_DIR)
    }

    /// Per-PROCESS relay root. Sharing one root across processes let a booting
    /// instance's age-zero startup sweep delete the LIVE relay parts of every
    /// other proxy on the host (chaos-found: "Failed to persist relay part: No
    /// such file").
    fn relay_root_dir(&self) -> PathBuf {
        self.relay_parent_dir().join(std::process::id().to_string())
    }

    fn relay_dir_for_upload(&self, upload_id: &str) -> PathBuf {
        self.relay_root_dir().join(upload_id)
    }
}

/// The relay parent of releases before the relay moved into the spool. Only
/// swept (its roots are crash debris or an older process's uploads).
#[allow(
    clippy::disallowed_methods,
    reason = "sweeps the relay root of releases before the relay moved into the spool"
)]
fn legacy_relay_parent_dir() -> PathBuf {
    std::env::temp_dir().join(RELAY_ROOT_DIR)
}

fn shared_spool() -> SpoolDir {
    SpoolDir::shared().unwrap_or_else(|e| panic!("failed to init spool dir: {e}"))
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
            PartPayload::RelayedFile(path, _) => {
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
mod tests;

#[cfg(test)]
mod review3_tests;
