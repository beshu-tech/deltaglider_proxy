// SPDX-License-Identifier: BUSL-1.1

//! Multipart upload state, held in memory: a restart loses every open
//! upload, and clients handle that.
//!
//! Every part is a relay file in the spool: UploadPart ingests the body
//! straight into a spool file (`Destination::MultipartPart`), and the
//! store keeps that file as the part ([`MultipartStore::upload_part`]),
//! with the spool budget its ingest reserved. No part is held in memory.
//! CompleteMultipartUpload hands the relay parts to the engine
//! ([`MultipartStore::complete_parts`]), which reads each file once and
//! checks it against the MD5 and size recorded here.
//!
//! The `uploads` lock guards the map only: no function that takes it does
//! file I/O (source guard `the_uploads_lock_is_never_held_for_file_io`).
//! A part's file is renamed into place before the lock, and a replaced or
//! refused part's file is deleted after it.

use crate::api::S3Error;
use crate::deltaglider::ingest::IngestedBody;
use crate::deltaglider::RelayPart;
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
use std::path::{Path, PathBuf};

use crate::deltaglider::spool::{mib_ceil, SpoolDir, SpoolReservation};
use std::sync::Arc;

/// Relay root, under the spool dir (it was under the system temp dir).
const RELAY_ROOT_DIR: &str = "deltaglider-mpu-relay";

/// Env var: the relay bytes one multipart upload may hold in the spool,
/// which is also the largest multipart object.
pub const RELAY_UPLOAD_MAX_ENV: &str = "DGP_SPOOL_RELAY_UPLOAD_MAX_BYTES";

/// Env var: the relay bytes all open uploads may hold together.
pub const MAX_TOTAL_MULTIPART_ENV: &str = "DGP_MAX_TOTAL_MULTIPART_BYTES";

/// Pure: the relay bytes one upload may hold, for a spool of
/// `spool_max_bytes` and the configured `DGP_SPOOL_RELAY_UPLOAD_MAX_BYTES`
/// (`None` = unset). Default: half of the spool, so one upload never pins
/// the budget of every other request. `0` = no per-upload cap (only the
/// global budget). `None` in the result means no cap. Every part is a
/// relay part, so this is the largest multipart object too.
pub fn relay_upload_cap(spool_max_bytes: u64, configured: Option<u64>) -> Option<u64> {
    match configured {
        Some(0) => None,
        Some(n) => Some(n),
        None => Some((spool_max_bytes / 2).max(1024 * 1024)),
    }
}

/// Pure: the part bytes all open uploads may hold together, for a spool of
/// `spool_max_bytes` and the configured `DGP_MAX_TOTAL_MULTIPART_BYTES`
/// (`None` = unset). Every part is a spool file, so the default is half of
/// the spool: open uploads never pin the budget of every other request.
pub fn max_total_multipart_bytes(spool_max_bytes: u64, configured: Option<u64>) -> u64 {
    configured.unwrap_or(spool_max_bytes / 2)
}

/// Pure: the spool budget, in bytes, that a part of `size` bytes holds.
/// The spool reserves whole MiB, at least one per file, so the caps count
/// this and not the part's bytes: counted in bytes, 10,000 parts of one
/// byte passed a cap of a few KiB while they held 10,000 MiB of the spool.
pub fn part_charge(size: u64) -> u64 {
    const MIB: u64 = 1024 * 1024;
    (mib_ceil(size).max(1) as u64).saturating_mul(MIB)
}

/// The charges ([`part_charge`]) of the parts that were admitted and are
/// still streaming: counted against the caps from their admission until
/// they are kept or dropped ([`AdmittedPart`]), so parallel parts cannot
/// pass the caps together.
#[derive(Default)]
struct Admitted {
    total: u64,
    by_upload: HashMap<String, u64>,
}

impl Admitted {
    fn of(&self, upload_id: &str) -> u64 {
        self.by_upload.get(upload_id).copied().unwrap_or(0)
    }

    fn add(&mut self, upload_id: &str, charge: u64) {
        self.total = self.total.saturating_add(charge);
        *self.by_upload.entry(upload_id.to_string()).or_default() += charge;
    }

    fn release(&mut self, upload_id: &str, charge: u64) {
        self.total = self.total.saturating_sub(charge);
        if let Some(held) = self.by_upload.get_mut(upload_id) {
            *held = held.saturating_sub(charge);
            if *held == 0 {
                self.by_upload.remove(upload_id);
            }
        }
    }
}

/// RAII: the charge of an admitted part, released when it is kept
/// ([`MultipartStore::upload_part`]) or when it is dropped (a failed
/// ingest, a dropped request future, an abort).
#[must_use = "the part's admission ends when this is dropped"]
pub struct AdmittedPart {
    admitted: Arc<parking_lot::Mutex<Admitted>>,
    upload_id: String,
    charge: u64,
}

impl AdmittedPart {
    /// Release the charge under the caller's lock of `admitted`.
    fn release_in(&mut self, admitted: &mut Admitted) {
        admitted.release(&self.upload_id, std::mem::take(&mut self.charge));
    }
}

impl Drop for AdmittedPart {
    fn drop(&mut self) {
        if self.charge > 0 {
            let charge = std::mem::take(&mut self.charge);
            self.admitted.lock().release(&self.upload_id, charge);
        }
    }
}

/// One received part: its relay file and what was recorded when it
/// arrived.
struct PartData {
    path: PathBuf,
    /// The spool budget the relay file holds until the part is dropped
    /// (overwrite, abort, complete, sweep): held for its Drop, after the
    /// file is deleted ([`discard_part`]).
    budget: SpoolReservation,
    md5_hex: String,
    md5_raw: [u8; 16],
    size: u64,
    uploaded_at: DateTime<Utc>,
}

impl PartData {
    fn relay_part(&self) -> RelayPart {
        RelayPart {
            path: self.path.clone(),
            md5: self.md5_raw,
            size: self.size,
        }
    }
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
///   [create] ─▶ Open ─▶─ complete_parts ─▶─ Completing
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
/// - `complete_parts` only returns parts if state was `Open`; atomically
///   flips to `Completing` under the write lock.
/// - `finish_upload` / `rollback_upload` terminate `Completing` state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MultipartState {
    /// Accepting UploadPart calls. Abort is allowed.
    Open,
    /// `complete_parts` has validated and handed off parts; `engine.store*`
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
    /// True while the engine store for this upload is actively running. The
    /// idle-TTL sweeper (`cleanup_expired`) must NOT reclaim a Completing upload
    /// whose store is in flight — a slow store to a remote backend can exceed
    /// completing_timeout, and sweeping it deletes the relay part files the
    /// store is still reading, losing them permanently on retry (H18). The store
    /// is separately bounded (request timeout + codec watchdog), so this flag
    /// can never wedge an upload forever.
    store_in_progress: bool,
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

/// RAII for the store of a Complete, armed from the flip to `Completing`.
/// A pre-commit exit (an error, a panic, a dropped future) rolls the upload
/// back to `Open`, so the client's retry does not wait for the
/// `Completing` timeout. Once the write COMMITTED (the engine told the
/// `committed` flag), an exit is not a retry: the upload is completed, the
/// registry keeps the success for retries, and joiners get it
/// ([`CompletionPublisher`]'s drop). [`Self::finish`] ends the guard after
/// `finish_upload`.
#[must_use = "the rollback runs when the guard is dropped"]
pub struct CompletingGuard {
    store: std::sync::Arc<MultipartStore>,
    upload_id: String,
    etag: String,
    committed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    finished: bool,
}

impl CompletingGuard {
    /// The store succeeded and `finish_upload` ended the `Completing` state.
    pub fn finish(mut self) {
        self.finished = true;
    }
}

impl Drop for CompletingGuard {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if self.committed.load(std::sync::atomic::Ordering::SeqCst) {
            self.store.commit_unwound(&self.upload_id, &self.etag);
        } else {
            self.store.rollback_upload(&self.upload_id);
        }
    }
}

/// The validated parts of an upload that `complete_parts` handed to the
/// store: relay files in the requested order, with their recorded MD5 and
/// size (no file is read here).
#[derive(Debug)]
pub struct CompletedParts {
    pub parts: Vec<RelayPart>,
    pub etag: String,
    pub total_size: u64,
    pub content_type: Option<String>,
    pub user_metadata: HashMap<String, String>,
}

/// What [`MultipartStore::admit_part`] found for a part that may be read,
/// and its charge against the caps until [`MultipartStore::upload_part`]
/// keeps it (or it is dropped).
#[must_use = "the part's admission ends when this is dropped"]
pub struct PartAdmission {
    /// The spool budget the upload holds already (its relay parts): its
    /// ingest never waits for more.
    pub held_spool_mib: usize,
    part: AdmittedPart,
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
    parts: Vec<RelayPart>,
    etag: String,
    total_size: u64,
}

/// Default maximum number of concurrent multipart uploads.
/// Overridable via `DGP_MAX_MULTIPART_UPLOADS` env var.
fn default_max_uploads() -> usize {
    crate::config::env_parse_with_default("DGP_MAX_MULTIPART_UPLOADS", 1000)
}

/// TTL before an idle (no recent UploadPart activity) multipart upload is
/// garbage-collected. Overridable via `DGP_MULTIPART_IDLE_TTL_HOURS`.
/// Default 24h — matches AWS's default abort-incomplete-multipart-upload
/// lifecycle recommendation.
fn default_multipart_idle_ttl_hours() -> i64 {
    crate::config::env_parse_with_default("DGP_MULTIPART_IDLE_TTL_HOURS", 24)
}

/// Outcome of a completion, shareable with retried Complete requests.
pub type CompletionResult = Result<String, CompletionFailure>;

/// A failed completion as the owner answered it: a joined retry answers the
/// same S3 error, not a 500 for an owner's 400 (s3surface-17).
#[derive(Debug, Clone, PartialEq)]
pub struct CompletionFailure {
    pub code: s3s::S3ErrorCode,
    pub message: Option<String>,
    /// The owner's explicit status (a code that has no status of its own,
    /// such as a quota refusal, carries it here).
    pub status: Option<axum::http::StatusCode>,
}

impl CompletionFailure {
    pub fn of(err: &s3s::S3Error) -> Self {
        Self {
            code: err.code().clone(),
            message: err.message().map(str::to_string),
            status: err.status_code(),
        }
    }

    pub fn internal(message: &str) -> Self {
        Self {
            code: s3s::S3ErrorCode::InternalError,
            message: Some(message.to_string()),
            status: None,
        }
    }

    /// What a joined retry gets when the owner's request ended before it
    /// published (a request timeout, a client disconnect, a cancel): the
    /// store did not run, so the retry is safe. 503, never a 500.
    pub fn owner_gone() -> Self {
        Self {
            code: s3s::S3ErrorCode::SlowDown,
            message: Some(
                "the request that completes this upload ended before it finished; retry".into(),
            ),
            status: Some(axum::http::StatusCode::SERVICE_UNAVAILABLE),
        }
    }

    pub fn to_s3s(&self) -> s3s::S3Error {
        let mut err = match &self.message {
            Some(message) => s3s::S3Error::with_message(self.code.clone(), message.clone()),
            None => s3s::S3Error::new(self.code.clone()),
        };
        if let Some(status) = self.status {
            err.set_status_code(status);
        }
        err
    }
}

/// Pure: does a joined retry get this refusal of the owner as it is? Only
/// a refusal that depends on the upload and its parts, never on the
/// owner's headers or principal: the joiner matched the owner on bucket,
/// key and part list only, so a precondition (`If-Match`, `If-None-Match`),
/// a quota of the owner's principal or an access refusal says nothing
/// about the joiner. An internal error is not shared either: it is the
/// owner's, and a retry may well succeed. Everything else is not shared: the joiner gets
/// [`CompletionFailure::owner_gone`] (503), retries, becomes the owner and
/// gets its own answer.
pub fn shared_with_joiners(code: &s3s::S3ErrorCode) -> bool {
    use s3s::S3ErrorCode as C;
    matches!(
        code,
        C::InvalidPart
            | C::InvalidPartOrder
            | C::EntityTooSmall
            | C::EntityTooLarge
            | C::NoSuchUpload
    )
}

/// Pure: what the owner of a Complete publishes to its joiners when it ends
/// WITHOUT a success: its own answer when [`shared_with_joiners`], else (or
/// when the request was dropped, `None`) [`CompletionFailure::owner_gone`].
pub fn joiner_failure(answer: Option<&s3s::S3Error>) -> CompletionFailure {
    match answer {
        Some(err) if shared_with_joiners(err.code()) => CompletionFailure::of(err),
        _ => CompletionFailure::owner_gone(),
    }
}

const COMPLETION_TOMBSTONE_TTL_SECS: u64 = 900;

enum CompletionSlot {
    InFlight {
        fingerprint: u64,
        bucket: String,
        key: String,
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
/// the tombstone. Dropping without publishing (cancel/abort) publishes
/// [`CompletionFailure::owner_gone`] and clears the slot so later attempts aren't wedged.
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
        // A failure that follows a commit is the success for the joiners.
        let result = match (
            &result,
            self.store.unwound_commit(&self.upload_id, self.fingerprint),
        ) {
            (Err(_), Some(etag)) => Ok(etag),
            _ => result,
        };
        let _ = self.tx.send(Some(result));
    }
}

impl Drop for CompletionPublisher {
    fn drop(&mut self) {
        if self.published {
            return;
        }
        // The store committed and its task then ended abnormally
        // ([`CompletingGuard`]): the tombstone holds the success, and
        // joiners get it. Otherwise the slot clears and they retry.
        if let Some(etag) = self.store.unwound_commit(&self.upload_id, self.fingerprint) {
            let _ = self.tx.send(Some(Ok(etag)));
            return;
        }
        self.store.clear_completion_slot(&self.upload_id);
        let _ = self.tx.send(Some(Err(joiner_failure(None))));
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

/// Thread-safe in-memory store for multipart upload state.
pub struct MultipartStore {
    uploads: RwLock<HashMap<String, MultipartUpload>>,
    /// CompleteMultipartUpload registry: in-flight completions + success tombstones.
    completions: parking_lot::Mutex<HashMap<String, CompletionSlot>>,
    /// Per-upload byte cap. Atomic so a hot config apply reaches it: the S3
    /// adapter refreshes it from the live engine on every part and complete.
    max_object_size: std::sync::atomic::AtomicU64,
    max_uploads: usize,
    /// The spool budget that the kept parts of all uploads hold, in bytes:
    /// the sum of [`part_charge`] of every `MultipartUpload.parts[*]`,
    /// updated under the same write lock that mutates the parts map.
    /// Checked, with `admitted`, before each UploadPart is read and again
    /// when it is kept (C3 DoS fix).
    in_flight_bytes: std::sync::atomic::AtomicU64,
    /// The charges of the parts admitted and still streaming.
    admitted: Arc<parking_lot::Mutex<Admitted>>,
    /// `DGP_MAX_TOTAL_MULTIPART_BYTES` as read at construction (`None`:
    /// half of the spool, see [`max_total_multipart_bytes`]).
    max_total_configured: Option<u64>,
    idle_ttl: Duration,
    /// Relay part files live in this spool's dir and hold its budget.
    spool: SpoolDir,
    /// `DGP_SPOOL_RELAY_UPLOAD_MAX_BYTES` as read at construction.
    relay_upload_max: Option<u64>,
}

/// Pure-ish: 400 InvalidArgument for a part number outside S3's range.
fn check_part_number(part_number: u32) -> Result<(), S3Error> {
    if !(1..=10000).contains(&part_number) {
        return Err(S3Error::InvalidArgument(
            "Part number must be between 1 and 10000".to_string(),
        ));
    }
    Ok(())
}

impl MultipartStore {
    pub fn new(max_object_size: u64) -> Self {
        Self {
            uploads: RwLock::new(HashMap::new()),
            completions: parking_lot::Mutex::new(HashMap::new()),
            max_object_size: std::sync::atomic::AtomicU64::new(max_object_size),
            max_uploads: default_max_uploads(),
            in_flight_bytes: std::sync::atomic::AtomicU64::new(0),
            admitted: Default::default(),
            max_total_configured: crate::config::env_parse::<u64>(MAX_TOTAL_MULTIPART_ENV),
            idle_ttl: Duration::hours(default_multipart_idle_ttl_hours()),
            spool: shared_spool(),
            relay_upload_max: crate::config::env_parse(RELAY_UPLOAD_MAX_ENV),
        }
    }

    /// The cap on concurrent uploads (`DGP_MAX_MULTIPART_UPLOADS`).
    pub fn max_uploads(&self) -> usize {
        self.max_uploads
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
            max_uploads: 1000,
            max_total_configured: Some(max_total_multipart_bytes),
            idle_ttl,
            relay_upload_max: None,
            ..Self::new(max_object_size)
        }
    }

    /// Snapshot the global in-flight byte counter. Test-only observability.
    #[cfg(test)]
    pub(crate) fn in_flight_bytes(&self) -> u64 {
        self.in_flight_bytes
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The spool the relay parts live in.
    pub fn spool(&self) -> &SpoolDir {
        &self.spool
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

    /// The part bytes all open uploads may hold together.
    fn max_total_multipart_bytes(&self) -> u64 {
        max_total_multipart_bytes(self.spool.max_bytes(), self.max_total_configured)
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
            store_in_progress: false,
        };

        uploads.insert(upload_id.clone(), upload);
        Ok(upload_id)
    }

    /// May a part of `declared` bytes be read for this upload? The checks
    /// that `upload_part` runs again when it keeps the part, run here on
    /// the declared size BEFORE the body is read: a part for an unknown,
    /// aborted or completing upload, or one past a cap, is refused before
    /// up to 5 GiB of it reach the spool. An admitted part counts against
    /// the caps from now on, while it streams ([`PartAdmission`]).
    pub fn admit_part(
        &self,
        upload_id: &str,
        bucket: &str,
        key: &str,
        part_number: u32,
        declared: u64,
    ) -> Result<PartAdmission, S3Error> {
        check_part_number(part_number)?;
        let uploads = self.uploads.read();
        let upload = open_upload(uploads.get(upload_id), upload_id, bucket, key)?;
        // Check and count under one lock: two parallel parts cannot both
        // pass a cap that only one of them fits.
        let mut admitted = self.admitted.lock();
        self.check_part_caps(upload, &admitted, part_number, declared)?;
        let charge = part_charge(declared);
        admitted.add(upload_id, charge);
        Ok(PartAdmission {
            held_spool_mib: upload.parts.values().map(|p| p.budget.reserved_mib()).sum(),
            part: AdmittedPart {
                admitted: self.admitted.clone(),
                upload_id: upload_id.to_string(),
                charge,
            },
        })
    }

    /// Keep an ingested part as a relay part, returns the quoted ETag (MD5
    /// hex). `body` is the part's spool file (`IngestedBody::Spooled`,
    /// see [`Self::admit_part`]); it is renamed into the upload's relay dir
    /// with its reservation, so the part costs no copy and no second
    /// reservation. The rename runs before the `uploads` lock, the map
    /// update under it, and the file of a replaced or refused part is
    /// deleted after it.
    pub fn upload_part(
        &self,
        upload_id: &str,
        bucket: &str,
        key: &str,
        part_number: u32,
        body: IngestedBody,
        admission: PartAdmission,
    ) -> Result<String, S3Error> {
        check_part_number(part_number)?;
        let IngestedBody::Spooled {
            spool,
            size,
            hashes,
            storage,
        } = body
        else {
            return Err(S3Error::InternalError(
                "a multipart part must be ingested into a spool file".to_string(),
            ));
        };
        // The spool file shares this reservation; `keep_as` keeps it.
        drop(storage);
        let md5_hex = hashes.md5().to_string();
        let mut md5_raw = [0u8; 16];
        hex::decode_to_slice(&md5_hex, &mut md5_raw)
            .map_err(|e| S3Error::InternalError(format!("part MD5: {e}")))?;
        let relay_dir = self.relay_dir_for_upload(upload_id);
        let kept = fs::create_dir_all(&relay_dir)
            .and_then(|()| spool.keep_as(&relay_dir.join(relay_part_name(part_number))));
        let (path, budget) = match kept {
            Ok(kept) => kept,
            // An abort removed the dir between the two steps.
            Err(_) if !self.has_upload(upload_id) => {
                self.forget_relay_dir(upload_id);
                return Err(S3Error::NoSuchUpload(upload_id.to_string()));
            }
            Err(e) => {
                return Err(S3Error::InternalError(format!(
                    "Failed to keep relay part: {e}"
                )))
            }
        };
        let part = PartData {
            path,
            budget,
            md5_hex,
            md5_raw,
            size,
            uploaded_at: Utc::now(),
        };
        let etag = format!("\"{}\"", part.md5_hex);
        match self.insert_part(upload_id, bucket, key, part_number, part, admission.part) {
            Ok(replaced) => {
                if let Some(old) = replaced {
                    discard_part(old);
                }
                Ok(etag)
            }
            Err((err, refused)) => {
                discard_part(refused);
                // The upload is gone (aborted while the part streamed): the
                // dir this part re-created goes too.
                if !self.has_upload(upload_id) {
                    self.forget_relay_dir(upload_id);
                }
                Err(err)
            }
        }
    }

    /// The map half of [`Self::upload_part`], under the `uploads` lock:
    /// the part's admission charge is released, the checks of
    /// [`Self::admit_part`] run again on the received size, then the
    /// insert. The part this one replaces, or this part when it is refused,
    /// is handed back for the caller to delete AFTER the lock.
    #[allow(clippy::result_large_err)]
    fn insert_part(
        &self,
        upload_id: &str,
        bucket: &str,
        key: &str,
        part_number: u32,
        part: PartData,
        mut admission: AdmittedPart,
    ) -> Result<Option<PartData>, (S3Error, PartData)> {
        let mut uploads = self.uploads.write();
        let mut admitted = self.admitted.lock();
        admission.release_in(&mut admitted);
        let upload = match open_upload_mut(uploads.get_mut(upload_id), upload_id, bucket, key) {
            Ok(upload) => upload,
            Err(e) => return Err((e, part)),
        };
        let delta = match self.check_part_caps(upload, &admitted, part_number, part.size) {
            Ok(delta) => delta,
            Err(e) => return Err((e, part)),
        };
        // Overwrite semantics: re-uploading same part_number replaces previous data.
        let replaced = upload.parts.insert(part_number, part);
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
        Ok(replaced)
    }

    /// The caps a part of `size` bytes must pass (C3 DoS fix); the change
    /// of the global held bytes it makes. Overwrite semantics: the part it
    /// replaces no longer counts. `admitted`: the parts admitted and still
    /// streaming (not this one), which count against the spool caps.
    ///
    /// 1. Per upload: `max_object_size`, on the part bytes (EntityTooLarge).
    /// 2. Global: `DGP_MAX_TOTAL_MULTIPART_BYTES` across all uploads, on
    ///    the spool budget held ([`part_charge`]; SlowDown, so SDKs back off
    ///    and retry).
    /// 3. Per upload: the spool budget it may hold ([`relay_upload_cap`],
    ///    EntityTooLarge naming the variable). A part that went past it was
    ///    written anyway, so the disk use of one upload grew without a bound
    ///    while it pinned the budget of every other request.
    fn check_part_caps(
        &self,
        upload: &MultipartUpload,
        admitted: &Admitted,
        part_number: u32,
        size: u64,
    ) -> Result<i64, S3Error> {
        let old_part_size = upload.parts.get(&part_number).map(|p| p.size);
        let others = upload
            .parts
            .values()
            .map(|p| p.size)
            .sum::<u64>()
            .saturating_sub(old_part_size.unwrap_or(0));
        let cumulative_after = others.saturating_add(size);

        let max_object_size = self.max_object_size();
        if cumulative_after > max_object_size {
            return Err(S3Error::EntityTooLarge {
                size: cumulative_after,
                max: max_object_size,
            });
        }

        // The spool budget: whole MiB per part.
        let charge = part_charge(size);
        let old_charge = old_part_size.map_or(0, part_charge);
        let delta: i64 = charge as i64 - old_charge as i64;
        if delta > 0 {
            let new_total = self
                .in_flight_bytes
                .load(std::sync::atomic::Ordering::Relaxed)
                .saturating_add(admitted.total)
                .saturating_add(delta as u64);
            let cap = self.max_total_multipart_bytes();
            if new_total > cap {
                return Err(S3Error::SlowDown(format!(
                    "Multipart uploads would hold {new_total} bytes of spool budget, over the cap \
                     of {cap} bytes ({MAX_TOTAL_MULTIPART_ENV}; each part holds whole MiB, at \
                     least one)"
                )));
            }
        }

        if let Some(cap) = relay_upload_cap(self.spool.max_bytes(), self.relay_upload_max) {
            let held_after = upload
                .parts
                .values()
                .map(|p| part_charge(p.size))
                .sum::<u64>()
                .saturating_sub(old_charge)
                .saturating_add(admitted.of(&upload.upload_id))
                .saturating_add(charge);
            if held_after > cap {
                return Err(S3Error::EntityTooLargeReason(format!(
                    "this multipart upload would hold {held_after} bytes of spool budget for its \
                     relay parts (each part holds whole MiB, at least one); one upload may hold \
                     at most {cap} bytes ({RELAY_UPLOAD_MAX_ENV}, default half of \
                     DGP_SPOOL_MAX_BYTES; 0 = no per-upload cap)"
                )));
            }
        }
        Ok(delta)
    }

    /// Whether the upload exists (Open or Completing).
    fn has_upload(&self, upload_id: &str) -> bool {
        self.uploads.read().contains_key(upload_id)
    }

    /// The user metadata the upload was created with: its store hints decide
    /// how the completion stores it.
    pub fn user_metadata(&self, upload_id: &str) -> Option<HashMap<String, String>> {
        let uploads = self.uploads.read();
        uploads.get(upload_id).map(|u| u.user_metadata.clone())
    }

    /// Get the size of a specific uploaded part (for quota pre-check).
    pub fn get_part_size(&self, upload_id: &str, part_number: u32) -> Option<u64> {
        let uploads = self.uploads.read();
        uploads
            .get(upload_id)
            .and_then(|u| u.parts.get(&part_number))
            .map(|p| p.size)
    }

    /// Begin completion: validate the requested parts, atomically flip
    /// the upload to `Completing`, and hand back its relay parts in order
    /// (no file is read). After this call the upload is reserved — new
    /// UploadParts AND abort are refused (409) until the caller invokes
    /// `finish_upload` or `rollback_upload`. This closes the C4
    /// complete/abort race.
    ///
    /// On validation failure the state is NOT changed (upload stays
    /// `Open` so the client can retry with corrected part metadata).
    pub fn complete_parts(
        &self,
        upload_id: &str,
        bucket: &str,
        key: &str,
        requested_parts: &[(u32, String)], // (part_number, etag)
    ) -> Result<CompletedParts, S3Error> {
        let mut uploads = self.uploads.write();

        // Refuse if the upload is already Completing — only one complete
        // may be in flight at a time.
        if let Some(u) = uploads.get(upload_id) {
            if u.state == MultipartState::Completing {
                return Err(S3Error::InvalidRequest(
                    "Upload is already being completed".to_string(),
                ));
            }
        }

        let (validated, upload) =
            self.validate_parts(&uploads, upload_id, bucket, key, requested_parts)?;
        let result = CompletedParts {
            parts: validated.parts,
            etag: validated.etag,
            total_size: validated.total_size,
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
            // A Complete of another bucket or key names an upload that is not
            // its own: NoSuchUpload, as in the Done arm. Joining it answered
            // 200 with the owner's ETag and wrote nothing (F33).
            Some(CompletionSlot::InFlight {
                bucket: b, key: k, ..
            }) if b != bucket || k != key => Err(S3Error::NoSuchUpload(upload_id.to_string())),
            Some(CompletionSlot::InFlight {
                fingerprint: f, rx, ..
            }) if *f == fingerprint => Ok(BeginComplete::Join(rx.clone())),
            Some(CompletionSlot::InFlight { .. }) => Err(S3Error::InvalidRequest(
                "another CompleteMultipartUpload with a different part list is in flight"
                    .to_string(),
            )),
            // No completion yet: only a request for the upload's own bucket
            // and key may own one. A Complete of an unknown id or of another
            // key held the owner window (and the per-key lock of ITS key)
            // until complete_parts refused it.
            None if !self
                .uploads
                .read()
                .get(upload_id)
                .is_some_and(|u| u.bucket == bucket && u.key == key) =>
            {
                Err(S3Error::NoSuchUpload(upload_id.to_string()))
            }
            None => {
                let (tx, rx) = tokio::sync::watch::channel(None);
                slots.insert(
                    upload_id.to_string(),
                    CompletionSlot::InFlight {
                        fingerprint,
                        bucket: bucket.to_string(),
                        key: key.to_string(),
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
            // next Complete attempt becomes a fresh Owner. A `Done` slot
            // stays: the write committed (see [`CompletingGuard`]), and a
            // failure after it never erases the success.
            Err(_) => {
                if !matches!(slots.get(upload_id), Some(CompletionSlot::Done { .. })) {
                    slots.remove(upload_id);
                }
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

    /// The guard of an upload that `complete_parts` flipped to `Completing`
    /// (see [`CompletingGuard`]); `committed` is set when the write commits.
    pub fn completing_guard(
        self: &std::sync::Arc<Self>,
        upload_id: &str,
        etag: &str,
        committed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> CompletingGuard {
        CompletingGuard {
            store: std::sync::Arc::clone(self),
            upload_id: upload_id.to_string(),
            etag: etag.to_string(),
            committed,
            finished: false,
        }
    }

    /// The write committed and its task ended before `finish_upload`:
    /// complete the upload here and keep the success for retries and
    /// joiners.
    fn commit_unwound(&self, upload_id: &str, etag: &str) {
        {
            let mut slots = self.completions.lock();
            if let Some(CompletionSlot::InFlight {
                fingerprint,
                bucket,
                key,
                ..
            }) = slots.remove(upload_id)
            {
                slots.insert(
                    upload_id.to_string(),
                    CompletionSlot::Done {
                        fingerprint,
                        etag: etag.to_string(),
                        bucket,
                        key,
                        at: std::time::Instant::now(),
                    },
                );
            }
        }
        self.finish_upload(upload_id);
    }

    /// The success that [`Self::commit_unwound`] kept for the completion
    /// with `fingerprint`, if any.
    fn unwound_commit(&self, upload_id: &str, fingerprint: u64) -> Option<String> {
        match self.completions.lock().get(upload_id) {
            Some(CompletionSlot::Done {
                fingerprint: f,
                etag,
                ..
            }) if *f == fingerprint => Some(etag.clone()),
            _ => None,
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
    ///
    /// Also releases the upload's bytes from the global in-flight counter
    /// so new uploads can reclaim headroom (C3 DoS fix), and deletes its
    /// relay files.
    pub fn finish_upload(&self, upload_id: &str) {
        if let Some(upload) = self.take_upload(upload_id) {
            self.discard_upload(upload);
        }
    }

    /// Remove an upload from the map, under the `uploads` lock only.
    fn take_upload(&self, upload_id: &str) -> Option<MultipartUpload> {
        self.uploads.write().remove(upload_id)
    }

    /// Drop a removed upload: its bytes leave the global in-flight counter,
    /// its relay dir is deleted, then its parts release their spool budget.
    /// Never under the `uploads` lock.
    fn discard_upload(&self, upload: MultipartUpload) -> u64 {
        let freed = self.release_bytes(&upload);
        self.forget_relay_dir(&upload.upload_id);
        drop(upload);
        freed
    }

    /// Delete the relay dir of `upload_id` (best effort).
    fn forget_relay_dir(&self, upload_id: &str) {
        let _ = fs::remove_dir_all(self.relay_dir_for_upload(upload_id));
    }

    /// Return the spool budget the upload's parts hold ([`part_charge`]),
    /// and take it off the global counter — used on release paths.
    fn release_bytes(&self, upload: &MultipartUpload) -> u64 {
        let freed: u64 = upload.parts.values().map(|p| part_charge(p.size)).sum();
        if freed > 0 {
            self.in_flight_bytes
                .fetch_sub(freed, std::sync::atomic::Ordering::Relaxed);
        }
        freed
    }

    /// Shared validation for complete variants.
    ///
    /// Looks up the upload, validates part ordering and ETags, enforces size limits,
    /// and computes the S3-compatible multipart ETag. Returns the relay parts in the
    /// requested order and a reference to the upload (for content_type /
    /// user_metadata). No I/O: the store reads and checks the files.
    fn validate_parts<'a>(
        &self,
        uploads: &'a HashMap<String, MultipartUpload>,
        upload_id: &str,
        bucket: &str,
        key: &str,
        requested_parts: &[(u32, String)],
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
        let mut parts = Vec::with_capacity(requested_parts.len());

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
            parts.push(part.relay_part());
        }

        // S3-compatible multipart ETag: MD5(concat of part MD5 raw bytes)-N
        let final_md5 = Md5::digest(&md5_concat);
        let etag = format!("\"{}-{}\"", hex::encode(final_md5), requested_parts.len());

        Ok((
            ValidatedParts {
                parts,
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
        let removed = self.take_for_abort(upload_id, bucket, key)?;
        // Release this upload's bytes from the global counter (C3 DoS fix).
        self.discard_upload(removed);
        Ok(())
    }

    /// The map half of [`Self::abort`], under the `uploads` lock.
    fn take_for_abort(
        &self,
        upload_id: &str,
        bucket: &str,
        key: &str,
    ) -> Result<MultipartUpload, S3Error> {
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
        uploads
            .remove(upload_id)
            .ok_or_else(|| S3Error::NoSuchUpload(upload_id.to_string()))
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
    /// relay-dir paths that the handler is still
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
        let removed = self.take_uploads_for_bucket(bucket)?;
        let removed_count = removed.len();
        for upload in removed {
            self.discard_upload(upload);
        }
        Ok(removed_count)
    }

    /// The map half of [`Self::purge_uploads_for_bucket`], under the
    /// `uploads` lock: an atomic check-and-remove.
    fn take_uploads_for_bucket(&self, bucket: &str) -> Result<Vec<MultipartUpload>, usize> {
        let mut uploads = self.uploads.write();
        let completing_count = uploads
            .values()
            .filter(|u| u.bucket == bucket && u.state == MultipartState::Completing)
            .count();
        if completing_count > 0 {
            return Err(completing_count);
        }
        let ids: Vec<String> = uploads
            .values()
            .filter(|u| u.bucket == bucket)
            .map(|u| u.upload_id.clone())
            .collect();
        Ok(ids.iter().filter_map(|id| uploads.remove(id)).collect())
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
        visible: &dyn Fn(&str) -> bool,
    ) -> (Vec<UploadInfo>, bool, String, String) {
        let uploads = self.uploads.read();
        let cap = max_uploads.clamp(1, 1000) as usize;
        let mut filtered: Vec<UploadInfo> = uploads
            .values()
            .filter(|u| {
                // Before the page cut, so a marker never names a hidden key.
                if !visible(&u.key) {
                    return false;
                }
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
        let mut report = MultipartSweepReport::default();
        for u in self.take_expired(max_age, completing_timeout) {
            if u.state == MultipartState::Completing {
                report.swept_completing_uploads += 1;
            } else {
                report.swept_open_uploads += 1;
            }
            report.reclaimed_bytes += self.discard_upload(u);
        }
        report
    }

    /// The map half of [`Self::cleanup_expired`], under the `uploads` lock.
    fn take_expired(
        &self,
        max_age: std::time::Duration,
        completing_timeout: std::time::Duration,
    ) -> Vec<MultipartUpload> {
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
        let mut uploads = self.uploads.write();
        let expired: Vec<String> = uploads
            .values()
            .filter(|u| {
                if u.state == MultipartState::Completing {
                    // Never reclaim an upload whose engine store is in flight —
                    // sweeping it deletes the relay parts the store is reading,
                    // losing them permanently on retry (H18). Bounded elsewhere.
                    u.last_activity <= completing_cutoff && !u.store_in_progress
                } else {
                    u.last_activity <= cutoff
                }
            })
            .map(|u| u.upload_id.clone())
            .collect();
        expired.iter().filter_map(|id| uploads.remove(id)).collect()
    }

    /// Remove orphan relay temp artifacts that don't belong to a currently-
    /// tracked upload. `min_age` guards the race with a part whose relay dir
    /// is made before its upload is in the snapshot, for the PERIODIC sweep
    /// (H19) — pass a grace period there; startup passes ZERO (no concurrent
    /// uploads exist yet).
    pub fn sweep_orphan_relay_artifacts(
        &self,
        min_age: std::time::Duration,
    ) -> MultipartSweepReport {
        let active_relay_dirs = self.active_relay_dirs();
        let (mut dirs_removed, mut files_removed) =
            cleanup_orphan_relay_entries_at(&self.relay_root_dir(), &active_relay_dirs, min_age);

        // Foreign per-process roots (crashed/finished proxies): reap only STALE
        // leftovers of a process that is gone. A root named by a LIVE pid is a
        // sibling proxy's (its upload may run for hours), never ours to sweep;
        // the age bound still guards a dead pid's root that is young.
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
            if path == mine || !path.is_dir() || relay_root_owner_is_alive(&path) {
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

    /// The relay dirs of every tracked upload.
    fn active_relay_dirs(&self) -> HashSet<PathBuf> {
        self.uploads
            .read()
            .keys()
            .map(|id| self.relay_dir_for_upload(id))
            .collect()
    }

    /// Current number of tracked uploads (Open + Completing).
    pub fn count_uploads(&self) -> usize {
        self.uploads.read().len()
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

/// The upload `found` as `upload_id`, if it is Open and is for
/// `bucket`/`key`: UploadPart's precondition.
fn open_upload<'a>(
    found: Option<&'a MultipartUpload>,
    upload_id: &str,
    bucket: &str,
    key: &str,
) -> Result<&'a MultipartUpload, S3Error> {
    let upload = found.ok_or_else(|| S3Error::NoSuchUpload(upload_id.to_string()))?;
    // Validate bucket+key match
    if upload.bucket != bucket || upload.key != key {
        return Err(S3Error::NoSuchUpload(upload_id.to_string()));
    }
    // C4 security fix: parts can only be uploaded while the upload is
    // Open. Once CompleteMultipartUpload has started (state=Completing),
    // accepting new parts would race with the in-flight `engine.store*`.
    if upload.state != MultipartState::Open {
        return Err(S3Error::InvalidRequest(
            "Upload is in the process of being completed; no more parts can be added".to_string(),
        ));
    }
    Ok(upload)
}

/// [`open_upload`] for the write half.
fn open_upload_mut<'a>(
    found: Option<&'a mut MultipartUpload>,
    upload_id: &str,
    bucket: &str,
    key: &str,
) -> Result<&'a mut MultipartUpload, S3Error> {
    open_upload(found.as_deref(), upload_id, bucket, key)?;
    found.ok_or_else(|| S3Error::NoSuchUpload(upload_id.to_string()))
}

/// The relay file name of a part: unique per attempt, so a part that
/// replaces another never writes over the file the old one still names.
fn relay_part_name(part_number: u32) -> String {
    format!(
        "part-{part_number:05}-{}.bin",
        uuid::Uuid::new_v4().simple()
    )
}

/// Delete a dropped part's relay file, then release its spool budget
/// (the budget never comes back before the disk does). Never under the
/// `uploads` lock.
fn discard_part(part: PartData) {
    let _ = fs::remove_file(&part.path);
    drop(part);
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

/// Remove relay artifacts under `relay_root` that don't belong to a currently-
/// tracked upload. `min_age` guards the promotion race (H19): the periodic sweep
/// snapshots active dirs then scans without the lock, so a concurrent
/// UploadPart's freshly-created relay dir isn't in the snapshot — deleting it
/// would destroy an active upload's parts. Only entries whose mtime is older than
/// `min_age` are eligible, so a just-created dir is always spared. Startup passes
/// `Duration::ZERO` (no concurrent uploads exist yet).
/// Whether the per-process relay root `root` (named by its owner's pid)
/// belongs to a process that is still running. `false` for a name that is
/// not a pid (it is swept by age only, as before).
fn relay_root_owner_is_alive(root: &Path) -> bool {
    let Some(pid) = root
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.parse::<i32>().ok())
        .filter(|pid| *pid > 0)
    else {
        return false;
    };
    process_is_alive(pid)
}

/// Signal 0 checks that `pid` exists without touching it: `EPERM` means it
/// exists under another user.
#[cfg(unix)]
fn process_is_alive(pid: i32) -> bool {
    // SAFETY: kill with signal 0 sends nothing; it only checks the pid.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn process_is_alive(_pid: i32) -> bool {
    false
}

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
