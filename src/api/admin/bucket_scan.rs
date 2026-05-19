// SPDX-License-Identifier: GPL-3.0-only

//! Bucket-wide object scan with persistent cache and streaming progress.
//!
//! ## Why this exists
//!
//! The dashboard's "objects stored" + "storage savings" headline
//! numbers come from `/_/stats`, which caps its scan at 1000 objects.
//! That's fine for a demo with three files; for any real bucket it
//! lies. This module replaces that single-shot cap with a paginated
//! scan that:
//!
//! 1. Walks **every** object in the bucket via the engine's
//!    `list_objects` pagination, accumulating `objects`,
//!    `original_bytes`, and `stored_bytes`.
//! 2. Reports progress to subscribers (SSE clients) via a
//!    `tokio::sync::watch` channel.
//! 3. Is cancellable mid-flight via a `CancellationToken`.
//! 4. Persists each completed result to disk
//!    (`.deltaglider_scans/<bucket>.json`) so the dashboard can show a
//!    truthful answer instantly on next page-load — even after a
//!    proxy restart. There is no TTL: S3 data is largely write-once;
//!    if you want a fresh number, click "Re-scan".
//!
//! ## State machine
//!
//! Per bucket, exactly one of:
//!
//! * **Idle** — no run record on disk, no job in memory. UI shows
//!   "Run scan".
//! * **Done(result)** — last completed scan loaded from disk or
//!   produced by a recent run. Carries `completed_at` so the UI can
//!   show "scanned 3h ago".
//! * **Running(job)** — a scan is in flight. Carries the watch handle
//!   so SSE can replay current progress, and the cancel token so
//!   `/stop` can abort.
//!
//! A successful `Running` transitions to `Done` and is persisted.
//! A cancelled `Running` falls back to whatever `Done` was previously
//! present (or `Idle` if none).
//!
//! ## Why we don't aggregate across buckets in the same job
//!
//! "Scan all buckets" composes from N per-bucket scans on the client
//! side — `GET /scan/status` (no bucket) returns the map of every
//! known bucket's last result, so the dashboard can sum trustworthy
//! per-bucket totals and footnote any bucket that's "never scanned".
//! Keeping one job per bucket means a flake in one doesn't kill the
//! rest, and the cache file is naturally keyed by what was actually
//! scanned.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::Json;
use chrono::{DateTime, Utc};
use futures::stream::{self, Stream};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use super::AdminState;
use crate::api::handlers::AppState;

/// Per-page batch size for the underlying `list_objects` walk. 1000 is
/// the S3 protocol cap; the filesystem backend accepts anything but
/// matches the same convention for predictable progress increments.
const PAGE_SIZE: u32 = 1000;

/// On-disk + in-memory record of a completed scan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanResult {
    pub bucket: String,
    pub total_objects: u64,
    pub total_original_bytes: u64,
    pub total_stored_bytes: u64,
    /// Bytes occupied by `reference.bin` files across the bucket.
    /// Included in `total_stored_bytes`. Exposed so the UI can break
    /// down storage cost as "deltas + passthroughs + references" and
    /// explain *why* savings aren't 100%.
    #[serde(default)]
    pub total_reference_bytes: u64,
    pub savings_percentage: f64,
    pub started_at: DateTime<Utc>,
    pub completed_at: DateTime<Utc>,
    pub duration_ms: u64,
    /// Schema version. Bumping this is how we invalidate caches when
    /// the shape of `ScanResult` changes. Old files fail to
    /// deserialize → loader drops them silently and the UI shows
    /// "never scanned".
    #[serde(default = "default_version")]
    pub version: u32,
}

fn default_version() -> u32 {
    1
}

// Bumped from 1 → 2 when reference bytes started counting toward
// `total_stored_bytes`. Old cache files (v1) drop because their
// `total_stored_bytes` undercounts by the reference total, and we
// don't want the UI to render lies just because we have a cached
// number to show.
const CURRENT_VERSION: u32 = 2;

/// Live progress snapshot, broadcast to SSE subscribers via
/// `watch::channel`. The terminal frame is the one where
/// `finished == true`.
#[derive(Debug, Clone, Serialize)]
pub struct ScanProgress {
    pub bucket: String,
    pub objects: u64,
    pub original_bytes: u64,
    pub stored_bytes: u64,
    pub pages_done: u32,
    pub has_more: bool,
    pub finished: bool,
    pub error: Option<String>,
    pub started_at: DateTime<Utc>,
}

impl ScanProgress {
    fn initial(bucket: &str) -> Self {
        Self {
            bucket: bucket.to_string(),
            objects: 0,
            original_bytes: 0,
            stored_bytes: 0,
            pages_done: 0,
            has_more: true,
            finished: false,
            error: None,
            started_at: Utc::now(),
        }
    }
}

/// Handle to a running scan: lets SSE read live progress and lets the
/// stop endpoint cancel.
pub struct RunningJob {
    progress_rx: watch::Receiver<ScanProgress>,
    cancel: CancellationToken,
}

/// Per-bucket state in the scanner registry.
enum BucketState {
    Done(ScanResult),
    Running(RunningJob),
}

/// Tracks bucket scans across the process lifetime. One per
/// `AdminState`.
pub struct BucketScanner {
    /// `bucket → state`. `parking_lot::RwLock` because mutations are
    /// brief (insert / replace one entry) and the read path
    /// (dashboard polling status) wants to be cheap.
    buckets: Arc<RwLock<HashMap<String, BucketState>>>,
    /// Directory where completed scans persist. Created lazily on
    /// first successful run.
    scan_dir: PathBuf,
}

impl BucketScanner {
    /// Build a scanner and hydrate from any persisted scans on disk.
    /// Files that fail to deserialize are dropped with a warning;
    /// they'll be re-generated the next time the user clicks "Scan".
    pub fn load(scan_dir: PathBuf) -> Arc<Self> {
        let mut buckets = HashMap::new();

        if scan_dir.is_dir() {
            match std::fs::read_dir(&scan_dir) {
                Ok(entries) => {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if path.extension().and_then(|s| s.to_str()) != Some("json") {
                            continue;
                        }
                        match Self::load_one(&path) {
                            Ok(result) => {
                                buckets.insert(result.bucket.clone(), BucketState::Done(result));
                            }
                            Err(e) => {
                                warn!(
                                    path = %path.display(),
                                    error = %e,
                                    "Dropping unreadable scan cache file — will regenerate on next scan"
                                );
                                // Best-effort cleanup so we don't keep
                                // logging the same broken file forever.
                                let _ = std::fs::remove_file(&path);
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        dir = %scan_dir.display(),
                        error = %e,
                        "Could not enumerate persisted scans; starting cold"
                    );
                }
            }
        }

        Arc::new(Self {
            buckets: Arc::new(RwLock::new(buckets)),
            scan_dir,
        })
    }

    fn load_one(path: &std::path::Path) -> Result<ScanResult, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("read: {e}"))?;
        let result: ScanResult =
            serde_json::from_slice(&bytes).map_err(|e| format!("parse: {e}"))?;
        if result.version != CURRENT_VERSION {
            return Err(format!(
                "version {} != current {}",
                result.version, CURRENT_VERSION
            ));
        }
        Ok(result)
    }

    /// Snapshot of every bucket's last known state, for the no-bucket
    /// dashboard view. Excludes in-flight running jobs from the totals
    /// — those are only meaningful through SSE.
    pub fn snapshot_all(&self) -> HashMap<String, ScanResult> {
        self.buckets
            .read()
            .iter()
            .filter_map(|(k, v)| match v {
                BucketState::Done(r) => Some((k.clone(), r.clone())),
                BucketState::Running(_) => None,
            })
            .collect()
    }

    /// Snapshot of a single bucket. `None` if never scanned and not
    /// currently running.
    fn snapshot_one(&self, bucket: &str) -> Option<StatusSnapshot> {
        match self.buckets.read().get(bucket)? {
            BucketState::Done(r) => Some(StatusSnapshot::Done(r.clone())),
            BucketState::Running(job) => {
                Some(StatusSnapshot::Running(job.progress_rx.borrow().clone()))
            }
        }
    }

    /// Start a scan if one isn't already running for this bucket.
    /// Returns the handle the SSE handler should subscribe to. If a
    /// scan was already running, returns the existing handle.
    fn start(&self, bucket: String, s3_state: Arc<AppState>) -> watch::Receiver<ScanProgress> {
        let mut buckets = self.buckets.write();
        if let Some(BucketState::Running(job)) = buckets.get(&bucket) {
            return job.progress_rx.clone();
        }

        let (tx, rx) = watch::channel(ScanProgress::initial(&bucket));
        let cancel = CancellationToken::new();
        let job = RunningJob {
            progress_rx: rx.clone(),
            cancel: cancel.clone(),
        };

        // Spawn the worker. It owns the sender; when it drops the
        // sender, subscribers' `changed().await` returns Err, which
        // the SSE adapter translates into stream end.
        let scanner_buckets = self.buckets.clone();
        let scan_dir = self.scan_dir.clone();
        let bucket_for_task = bucket.clone();
        tokio::spawn(async move {
            let started_at = Utc::now();
            let started_instant = std::time::Instant::now();
            let outcome = run_scan(
                s3_state,
                &bucket_for_task,
                started_at,
                tx.clone(),
                cancel.clone(),
            )
            .await;

            // Drop the running entry. On success we replace with the
            // completed Done; on cancel/error we leave whatever Done
            // was there previously alone (or remove if none).
            let mut buckets = scanner_buckets.write();
            match outcome {
                Ok(result) => {
                    // Persist to disk before we publish into the
                    // in-memory map. If the write fails the warn
                    // shows up in the logs but the in-memory result
                    // is still valid for THIS process — restart
                    // would lose it.
                    persist_scan(&scan_dir, &result);
                    let duration_ms = started_instant.elapsed().as_millis() as u64;
                    debug!(
                        bucket = %bucket_for_task,
                        objects = result.total_objects,
                        duration_ms,
                        "Bucket scan complete"
                    );
                    buckets.insert(bucket_for_task.clone(), BucketState::Done(result));
                }
                Err(ScanFailure::Cancelled) => {
                    // Leave any pre-existing Done in place; otherwise
                    // drop the Running entry so the bucket appears
                    // Idle on next status poll.
                    if matches!(buckets.get(&bucket_for_task), Some(BucketState::Running(_))) {
                        buckets.remove(&bucket_for_task);
                    }
                }
                Err(ScanFailure::Error(e)) => {
                    warn!(
                        bucket = %bucket_for_task,
                        error = %e,
                        "Bucket scan failed"
                    );
                    if matches!(buckets.get(&bucket_for_task), Some(BucketState::Running(_))) {
                        buckets.remove(&bucket_for_task);
                    }
                }
            }
        });

        buckets.insert(bucket, BucketState::Running(job));
        rx
    }

    /// Cancel a running scan. Returns true if a scan was running and
    /// got signalled (it may take a beat to actually exit), false if
    /// no scan was running.
    fn cancel(&self, bucket: &str) -> bool {
        if let Some(BucketState::Running(job)) = self.buckets.read().get(bucket) {
            job.cancel.cancel();
            true
        } else {
            false
        }
    }

    /// Drop a persisted scan for a bucket (e.g. user clicked "forget
    /// this result"). Returns true if a record existed.
    pub fn forget(&self, bucket: &str) -> bool {
        let removed_in_memory = {
            let mut buckets = self.buckets.write();
            match buckets.get(bucket) {
                Some(BucketState::Done(_)) => buckets.remove(bucket).is_some(),
                _ => false,
            }
        };
        let path = self
            .scan_dir
            .join(format!("{}.json", sanitise_bucket_for_filename(bucket)));
        let removed_on_disk = std::fs::remove_file(&path).is_ok();
        removed_in_memory || removed_on_disk
    }
}

/// Pure filesystem write — no scanner state needed. Pulled out of
/// `BucketScanner::persist` so the spawned worker task can call it
/// without cloning the whole scanner just to reach the scan_dir +
/// filename rules.
fn persist_scan(scan_dir: &std::path::Path, result: &ScanResult) {
    if let Err(e) = std::fs::create_dir_all(scan_dir) {
        warn!(
            dir = %scan_dir.display(),
            error = %e,
            "Could not create scan cache dir; scan will not survive restart"
        );
        return;
    }
    let path = scan_dir.join(format!(
        "{}.json",
        sanitise_bucket_for_filename(&result.bucket)
    ));
    match serde_json::to_vec_pretty(result) {
        Ok(bytes) => {
            if let Err(e) = std::fs::write(&path, &bytes) {
                warn!(
                    path = %path.display(),
                    error = %e,
                    "Failed to write scan cache; in-memory result is still valid"
                );
            }
        }
        Err(e) => warn!(error = %e, "Failed to serialise scan result"),
    }
}

/// Bucket names are S3-valid (`[a-z0-9.-]`, length 3-63) so they are
/// already filename-safe on every platform we target. We still pass
/// them through a defensive whitelist in case a future change loosens
/// validation; anything outside the whitelist is replaced with `_`.
fn sanitise_bucket_for_filename(bucket: &str) -> String {
    bucket
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

enum ScanFailure {
    Cancelled,
    Error(String),
}

/// The actual paginated scan loop. Yields progress through `tx` and
/// honours `cancel`.
async fn run_scan(
    s3_state: Arc<AppState>,
    bucket: &str,
    started_at: DateTime<Utc>,
    tx: watch::Sender<ScanProgress>,
    cancel: CancellationToken,
) -> Result<ScanResult, ScanFailure> {
    let started_instant = std::time::Instant::now();
    // The pure savings accumulator (`src/deltaglider/savings.rs`) owns
    // the math. Page through every user-visible object; once that's
    // done, walk the deltaspace references separately and feed them in
    // — references are hidden from `list_objects` but their bytes are
    // real and MUST count toward `total_stored_bytes`, otherwise the
    // dashboard reports up to "100% saved" on prefixes whose true
    // savings is ~80%.
    let mut totals = crate::deltaglider::SavingsTotals::default();
    let mut pages_done: u32 = 0;
    let mut continuation_token: Option<String> = None;

    loop {
        if cancel.is_cancelled() {
            return Err(ScanFailure::Cancelled);
        }

        // Hold the engine guard in a local so the future returned by
        // `list_objects` doesn't borrow a temporary that gets dropped
        // mid-await in the tokio::select! arm.
        let engine = s3_state.engine.load();
        let list_fut = engine.list_objects(
            bucket,
            "",
            None,
            PAGE_SIZE,
            continuation_token.as_deref(),
            true, // metadata=true so we get delta_size for stored bytes
        );
        let page = tokio::select! {
            _ = cancel.cancelled() => return Err(ScanFailure::Cancelled),
            result = list_fut => result.map_err(|e| ScanFailure::Error(e.to_string()))?,
        };

        for (_key, meta) in &page.objects {
            totals.accumulate(meta);
        }
        pages_done += 1;

        let has_more = page.is_truncated && page.next_continuation_token.is_some();
        // Emit on EVERY page. The watch channel coalesces — if the
        // SSE forwarder hasn't read the previous value, the new send
        // overwrites it, so even a 100k-object filesystem walk can't
        // flood the consumer. Earlier we throttled to every-5-pages
        // which made small buckets (<5k objects) look broken: the
        // dashboard showed "Scanning… 0 objects" right up to "done".
        let snapshot = ScanProgress {
            bucket: bucket.to_string(),
            objects: totals.user_visible_count(),
            original_bytes: totals.original_bytes,
            stored_bytes: totals.stored_bytes,
            pages_done,
            has_more,
            finished: !has_more,
            error: None,
            started_at,
        };
        // send() only fails if all receivers have been dropped; that's
        // fine — the scan keeps running for the background result +
        // disk cache, even with no live observers.
        let _ = tx.send(snapshot);

        if !has_more {
            break;
        }
        continuation_token = page.next_continuation_token;
    }

    // Second pass: fold in every `reference.bin` across the bucket so
    // `total_stored_bytes` matches what's actually on disk. Failures
    // for individual deltaspaces are logged inside the engine helper
    // and skipped — a missing reference is a per-deltaspace data
    // problem, not a reason to abort the whole scan.
    if cancel.is_cancelled() {
        return Err(ScanFailure::Cancelled);
    }
    let engine = s3_state.engine.load();
    // limit=None: the dashboard scan is the "real number" path; the
    // operator triggered it explicitly. Cost is the operator's
    // problem here — they're already waiting on a paginated walk of
    // every user-visible object. The lightweight chip endpoint passes
    // Some(REFERENCE_SCAN_LIMIT) instead. Truncation isn't surfaced
    // on this path because it never happens.
    let refs = engine
        .list_deltaspace_references(bucket, "", None)
        .await
        .map_err(|e| ScanFailure::Error(e.to_string()))?;
    for meta in &refs.references {
        totals.accumulate(meta);
    }

    let completed_at = Utc::now();
    let duration_ms = started_instant.elapsed().as_millis() as u64;
    let savings_percentage = totals.savings_percentage().unwrap_or(0.0);

    let result = ScanResult {
        bucket: bucket.to_string(),
        total_objects: totals.user_visible_count(),
        total_original_bytes: totals.original_bytes,
        total_stored_bytes: totals.stored_bytes,
        total_reference_bytes: totals.reference_bytes,
        savings_percentage,
        started_at,
        completed_at,
        duration_ms,
        version: CURRENT_VERSION,
    };

    // Emit one final terminal frame so any SSE subscriber gets a
    // clean "done" signal before the channel closes.
    let _ = tx.send(ScanProgress {
        bucket: bucket.to_string(),
        objects: totals.user_visible_count(),
        original_bytes: totals.original_bytes,
        stored_bytes: totals.stored_bytes,
        pages_done,
        has_more: false,
        finished: true,
        error: None,
        started_at,
    });

    Ok(result)
}

// ─── HTTP handlers ───────────────────────────────────────────────────

/// Query for any of the per-bucket endpoints.
#[derive(Deserialize)]
pub struct BucketQuery {
    pub bucket: String,
}

/// Status snapshot payload for the JSON endpoint.
#[derive(Serialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum StatusSnapshot {
    Done(ScanResult),
    Running(ScanProgress),
}

/// `GET /_/api/admin/diagnostics/scan/status?bucket=X`
/// or with no `bucket` → returns the full map across every known
/// bucket as `{ buckets: { <name>: ScanResult } }`. The map form is
/// what the dashboard uses to compute global totals from trusted
/// per-bucket cached scans.
#[derive(Serialize)]
pub struct AllStatusResponse {
    pub buckets: HashMap<String, ScanResult>,
}

#[derive(Deserialize)]
pub struct OptionalBucketQuery {
    pub bucket: Option<String>,
}

pub async fn get_scan_status(
    State(state): State<Arc<AdminState>>,
    Query(q): Query<OptionalBucketQuery>,
) -> Result<axum::response::Response, StatusCode> {
    let scanner = &state.bucket_scanner;
    if let Some(bucket) = q.bucket {
        match scanner.snapshot_one(&bucket) {
            Some(snap) => Ok(Json(snap).into_response()),
            None => Ok((
                StatusCode::OK,
                Json(serde_json::json!({ "state": "idle", "bucket": bucket })),
            )
                .into_response()),
        }
    } else {
        Ok(Json(AllStatusResponse {
            buckets: scanner.snapshot_all(),
        })
        .into_response())
    }
}

/// `POST /_/api/admin/diagnostics/scan/start?bucket=X`
///
/// Starts a scan if one isn't already running. Idempotent — calling
/// twice in quick succession returns the same running job.
pub async fn post_scan_start(
    State(state): State<Arc<AdminState>>,
    Query(q): Query<BucketQuery>,
) -> Result<Json<ScanProgress>, StatusCode> {
    let rx = state
        .bucket_scanner
        .start(q.bucket.clone(), state.s3_state.clone());
    let snapshot = rx.borrow().clone();
    Ok(Json(snapshot))
}

/// `POST /_/api/admin/diagnostics/scan/stop?bucket=X`
pub async fn post_scan_stop(
    State(state): State<Arc<AdminState>>,
    Query(q): Query<BucketQuery>,
) -> Json<serde_json::Value> {
    let cancelled = state.bucket_scanner.cancel(&q.bucket);
    Json(serde_json::json!({ "cancelled": cancelled }))
}

/// `DELETE /_/api/admin/diagnostics/scan?bucket=X` — drop the
/// persisted result so the dashboard reverts to "never scanned".
pub async fn delete_scan(
    State(state): State<Arc<AdminState>>,
    Query(q): Query<BucketQuery>,
) -> Json<serde_json::Value> {
    let forgotten = state.bucket_scanner.forget(&q.bucket);
    Json(serde_json::json!({ "forgotten": forgotten }))
}

/// `GET /_/api/admin/diagnostics/scan/stream?bucket=X`
///
/// Server-Sent Events stream of [`ScanProgress`] frames. If no scan
/// is currently running for the bucket, this kicks one off
/// implicitly — the dashboard treats opening the stream as "start
/// and watch". Closes when the scan ends (success, cancel, error).
pub async fn get_scan_stream(
    State(state): State<Arc<AdminState>>,
    Query(q): Query<BucketQuery>,
) -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    let rx = state
        .bucket_scanner
        .start(q.bucket.clone(), state.s3_state.clone());

    // Emit the current frame immediately so the client gets state
    // without waiting for the next page.
    let initial = rx.borrow().clone();

    let stream = stream::unfold(
        (rx, Some(initial), false),
        |(mut rx, pending, done)| async move {
            if done {
                return None;
            }
            let (frame, next_done) = if let Some(frame) = pending {
                // First yield: replay the current snapshot.
                (frame, false)
            } else {
                match rx.changed().await {
                    Ok(()) => (rx.borrow().clone(), false),
                    Err(_) => return None,
                }
            };
            let is_terminal = frame.finished || frame.error.is_some();
            let event = match Event::default().json_data(&frame) {
                Ok(e) => e,
                Err(e) => Event::default()
                    .event("error")
                    .data(format!("serialise progress: {e}")),
            };
            let event = if is_terminal {
                event.event("done")
            } else {
                event.event("progress")
            };
            Some((
                Ok::<_, std::convert::Infallible>(event),
                (rx, None, next_done || is_terminal),
            ))
        },
    );

    Sse::new(stream).keep_alive(KeepAlive::new())
}

// ─── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitise_keeps_valid_bucket_chars() {
        assert_eq!(sanitise_bucket_for_filename("my-bucket.1"), "my-bucket.1");
        assert_eq!(sanitise_bucket_for_filename("foo_bar"), "foo_bar");
    }

    #[test]
    fn sanitise_replaces_path_separators() {
        assert_eq!(sanitise_bucket_for_filename("../etc"), ".._etc");
        assert_eq!(sanitise_bucket_for_filename("a/b"), "a_b");
        assert_eq!(sanitise_bucket_for_filename("a\\b"), "a_b");
    }

    #[test]
    fn load_handles_missing_dir() {
        let dir = std::env::temp_dir().join(format!("dgp_scan_test_{}", uuid::Uuid::new_v4()));
        let scanner = BucketScanner::load(dir);
        assert!(scanner.snapshot_all().is_empty());
    }

    #[test]
    fn load_skips_unreadable_files() {
        let dir = std::env::temp_dir().join(format!("dgp_scan_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("garbage.json"), b"this is not json").unwrap();
        let scanner = BucketScanner::load(dir.clone());
        assert!(scanner.snapshot_all().is_empty());
        // The unreadable file should have been removed.
        assert!(!dir.join("garbage.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_drops_wrong_version() {
        let dir = std::env::temp_dir().join(format!("dgp_scan_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let bogus = serde_json::json!({
            "bucket": "old",
            "total_objects": 1,
            "total_original_bytes": 1,
            "total_stored_bytes": 1,
            "savings_percentage": 0.0,
            "started_at": "2024-01-01T00:00:00Z",
            "completed_at": "2024-01-01T00:00:00Z",
            "duration_ms": 1,
            "version": 99999
        });
        std::fs::write(dir.join("old.json"), serde_json::to_vec(&bogus).unwrap()).unwrap();
        let scanner = BucketScanner::load(dir.clone());
        assert!(scanner.snapshot_all().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn round_trip_persist_then_load() {
        let dir = std::env::temp_dir().join(format!("dgp_scan_test_{}", uuid::Uuid::new_v4()));
        let started = Utc::now();
        let result = ScanResult {
            bucket: "mybucket".into(),
            total_objects: 42,
            total_original_bytes: 1_000_000,
            total_stored_bytes: 800_000,
            total_reference_bytes: 100_000,
            savings_percentage: 20.0,
            started_at: started,
            completed_at: started,
            duration_ms: 10,
            version: CURRENT_VERSION,
        };
        persist_scan(&dir, &result);

        let reloaded = BucketScanner::load(dir.clone());
        let snap = reloaded.snapshot_all();
        assert_eq!(snap.len(), 1);
        let got = snap.get("mybucket").unwrap();
        assert_eq!(got.total_objects, 42);
        assert_eq!(got.total_original_bytes, 1_000_000);
        assert_eq!(got.total_stored_bytes, 800_000);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
