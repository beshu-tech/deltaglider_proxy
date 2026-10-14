// SPDX-License-Identifier: BUSL-1.1

//! S3 API request handlers (shared state + helpers).
//!
//! With the legacy axum-handler S3 path retired, the only S3
//! implementation is `src/s3_adapter_s3s/` (the `s3s` crate
//! adapter). This module now hosts only:
//!
//! - `AppState` — shared application state for both the s3s adapter
//!   and the admin API.
//! - `form_post` — the browser form-POST upload path. Lives outside
//!   the s3s crate because s3s doesn't model the multipart/form-data
//!   PostObject shape; the s3s router intercepts that one request
//!   shape and hands it to `form_post::handle_form_post_upload`.
//! - `object_helpers` — small shared helpers (the quota gate + the
//!   per-object event-outbox enqueue), called from both
//!   `s3_adapter_s3s` and `form_post`.
//! - `status` — `/_/health` and `/_/stats` legacy endpoints.
//! - `ensure_bucket_exists`, `audit_log_s3` —
//!   small free helpers reused by the surviving handlers.
//!
//! Pre-consolidation, this module also hosted ~3500 LOC of axum-based
//! S3 handlers (object, bucket, multipart). Those moved into the
//! s3s adapter; the old files are gone.

pub mod form_post;
pub(crate) mod object_helpers;
mod status;

use super::errors::S3Error;
use crate::config_db::{ConfigDb, JobsDb};
use crate::deltaglider::DynEngine;
use crate::metrics::Metrics;
use crate::multipart::MultipartStore;
use arc_swap::ArcSwap;
use axum::http::HeaderMap;
use std::sync::Arc;

/// S3 audit log helper — delegates to the shared audit module.
pub(crate) fn audit_log_s3(
    action: &str,
    user: &str,
    headers: &HeaderMap,
    bucket: &str,
    path: &str,
) {
    crate::audit::audit_log(action, user, "", headers, bucket, path);
}

pub use status::{
    get_stats, head_root, health_check, readiness_check, HealthResponse, ReadinessResponse,
    StatsQuery, StatsResponse,
};

// Re-export for use by metrics module
pub(crate) use status::get_peak_rss_bytes;

/// Application state shared across handlers
#[cfg(test)]
impl AppState {
    /// An app state around `engine` with no config DB and no counters.
    pub(crate) fn for_tests(engine: DynEngine) -> Arc<Self> {
        let config = crate::config::Config::default();
        Arc::new(Self {
            engine: arc_swap::ArcSwap::from_pointee(engine),
            multipart: Arc::new(crate::multipart::MultipartStore::new(
                config.max_object_size,
            )),
            metrics: Arc::new(crate::metrics::Metrics::new()),
            usage_scanner: Arc::new(crate::usage_scanner::UsageScanner::new()),
            bucket_usage: None,
            reference_lock: None,
            config_db: None,
            jobs_db: None,
            maintenance_gate: Arc::new(crate::maintenance::gate::MaintenanceGate::new()),
            maintenance_notify: Arc::new(tokio::sync::Notify::new()),
            backend_capabilities: Default::default(),
            backend_health: Default::default(),
        })
    }
}

pub struct AppState {
    pub engine: ArcSwap<DynEngine>,
    pub multipart: Arc<MultipartStore>,
    pub metrics: Arc<Metrics>,
    pub usage_scanner: Arc<crate::usage_scanner::UsageScanner>,
    /// Per-instance running bucket-size counter (O(1) reads; None in open-mode
    /// dev when the usage DB couldn't be opened). Re-attached to the engine on
    /// every rebuild so a config reload never drops the counter.
    pub bucket_usage: Option<Arc<crate::bucket_usage::BucketUsage>>,
    /// Cross-instance per-deltaspace reference lock (multi-instance only; `None`
    /// single-instance). Re-attached to the engine on every rebuild, mirroring
    /// `bucket_usage`, so a config reload never drops the cross-node protection.
    pub reference_lock: Option<Arc<crate::coordination::DynReferenceLock<'static>>>,
    pub config_db: Option<Arc<tokio::sync::Mutex<ConfigDb>>>,
    /// The node-local jobs DB: event outbox, job state (see
    /// [`crate::config_db::jobs_db`]). Never synced.
    pub jobs_db: Option<Arc<tokio::sync::Mutex<JobsDb>>>,
    /// Per-bucket WRITE gate for maintenance jobs (re-encryption). Layered
    /// into the S3 router as middleware; admin handlers and background
    /// writers consult it explicitly. See `src/maintenance/gate.rs`.
    pub maintenance_gate: Arc<crate::maintenance::gate::MaintenanceGate>,
    /// Wakes the maintenance worker immediately when a job is created.
    pub maintenance_notify: Arc<tokio::sync::Notify>,
    /// Per-backend conditional-write verdicts from the startup capability
    /// gate (empty when single-instance — the gate doesn't run). Consulted
    /// by the hot-apply pre-commit gate + the admin backends API.
    pub backend_capabilities: Arc<crate::coordination::BackendCapabilityCache>,
    /// Per-backend connectivity/auth health from the boot probe + re-probe
    /// loop. Gates S3 requests to buckets on unhealthy backends (503) and
    /// feeds the admin backends API. See `src/coordination/health.rs`.
    pub backend_health: Arc<crate::coordination::BackendHealthCache>,
}

// ---------------------------------------------------------------------------
// Shared utility functions used across handler submodules
// ---------------------------------------------------------------------------

/// Verify that `bucket` exists on the storage backend BEFORE any subresource
/// or write path is allowed to proceed.
///
/// Two-fold purpose:
///
/// 1. **Cross-backend NoSuchBucket parity** — closes a silent-bucket-creation
///    bug on the filesystem backend (C2 from the security audit):
///    `ensure_dir` at `src/storage/filesystem/mod.rs::ensure_dir` calls
///    `create_dir_all(parent)`, which would otherwise quietly create the
///    bucket root as a side effect of the first PUT. That diverges from S3
///    (`NoSuchBucket`) and bypasses any `s3:CreateBucket`-equivalent gate.
///    The `FilesystemBackend::put_*` methods carry a belt-and-braces
///    `require_bucket_exists` check too, so the contract is enforced at
///    both layers.
/// 2. **404 parity for bucket subresources** — GetBucketLocation,
///    GetBucketVersioning, ListMultipartUploads, etc. should all answer
///    `NoSuchBucket` for ghost buckets. Same helper, same error.
///
/// Engine-level errors (e.g. backend connectivity) propagate via the
/// existing `From<EngineError> for S3Error` conversion so a missing
/// underlying backend surfaces as a meaningful error instead of a
/// mysterious 500.
///
/// A bucket found is trusted for [`BUCKET_SEEN_TTL`] per engine, so a
/// 1,000-part upload sends one HeadBucket, not 1,000, and concurrent first
/// checks of one bucket send one HeadBucket between them. Every S3 verb
/// but HeadBucket itself checks its bucket here: the write paths, the
/// multipart verbs, the bucket sub-resource verbs, DeleteObject(s), the
/// `NoSuchKey`-or-`NoSuchBucket` check of a read miss and of a copy source,
/// and a LIST that finds nothing. DeleteBucket on this instance forgets the bucket
/// ([`forget_bucket`]); a bucket deleted elsewhere can pass for up to
/// [`BUCKET_SEEN_TTL`], and a write into it still fails at the backend
/// (`require_bucket_exists` on the filesystem, `NoSuchBucket` on S3). A
/// missing bucket is asked again every time: one created a moment later is
/// usable at once.
pub(crate) async fn ensure_bucket_exists(
    state: &Arc<AppState>,
    bucket: &str,
) -> Result<(), S3Error> {
    ensure_bucket_exists_on(&state.engine.load_full(), bucket).await
}

/// [`ensure_bucket_exists`] on one engine.
pub(crate) async fn ensure_bucket_exists_on(
    engine: &Arc<DynEngine>,
    bucket: &str,
) -> Result<(), S3Error> {
    if bucket_seen(engine, bucket) {
        return Ok(());
    }
    // Single flight: N first requests to a bucket send one HeadBucket and
    // share its answer: found, missing, or the error.
    let key = (Arc::as_ptr(engine) as usize, bucket.to_string());
    let answer = BUCKET_FLIGHTS
        .run(key, || async {
            use crate::storage::StorageBackend;
            engine.storage().head_bucket(bucket).await.map_err(Arc::new)
        })
        .await;
    match answer {
        Ok(true) => {
            remember_bucket(engine, bucket);
            Ok(())
        }
        Ok(false) => Err(S3Error::NoSuchBucket(bucket.to_string())),
        Err(e) => Err(S3Error::from(e.duplicate())),
    }
}

/// The cold bucket checks in flight, by (engine address, bucket).
#[allow(clippy::type_complexity)]
static BUCKET_FLIGHTS: std::sync::LazyLock<
    crate::single_flight::SingleFlight<
        (usize, String),
        Result<bool, Arc<crate::storage::StorageError>>,
    >,
> = std::sync::LazyLock::new(Default::default);

/// How long a write path trusts a bucket that it found.
const BUCKET_SEEN_TTL: std::time::Duration = std::time::Duration::from_secs(5);

/// One bucket a write path found on one engine.
struct SeenBucket {
    /// Pins the engine's allocation, so its address (the key) is not reused
    /// by a later engine while this entry lives.
    engine: std::sync::Weak<DynEngine>,
    at: std::time::Instant,
}

/// Buckets the write paths found, by (engine address, bucket). A rebuilt
/// engine (a config apply can route a bucket elsewhere) asks again.
static BUCKETS_SEEN: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashMap<(usize, String), SeenBucket>>,
> = std::sync::LazyLock::new(Default::default);

fn bucket_seen(engine: &Arc<DynEngine>, bucket: &str) -> bool {
    let key = (Arc::as_ptr(engine) as usize, bucket.to_string());
    BUCKETS_SEEN
        .lock()
        .get(&key)
        .is_some_and(|seen| seen.at.elapsed() < BUCKET_SEEN_TTL && seen.engine.strong_count() > 0)
}

/// Forget that `bucket` was found, on every engine: DeleteBucket calls it,
/// so the next check asks the backend again.
pub(crate) fn forget_bucket(bucket: &str) {
    BUCKETS_SEEN.lock().retain(|(_, b), _| b != bucket);
}

fn remember_bucket(engine: &Arc<DynEngine>, bucket: &str) {
    let mut seen = BUCKETS_SEEN.lock();
    // Drop stale entries (and those of dropped engines) as new ones come.
    seen.retain(|_, s| s.at.elapsed() < BUCKET_SEEN_TTL && s.engine.strong_count() > 0);
    seen.insert(
        (Arc::as_ptr(engine) as usize, bucket.to_string()),
        SeenBucket {
            engine: Arc::downgrade(engine),
            at: std::time::Instant::now(),
        },
    );
}

#[cfg(test)]
mod bucket_exists_tests {
    use super::*;

    fn head_buckets(fake: &crate::storage::FakeS3, bucket: &str) -> usize {
        // The SDK sends HeadBucket as `HEAD /b/`.
        let line = format!("HEAD /{bucket}/");
        fake.requests()
            .iter()
            .filter(|r| **r == line || r.starts_with(&format!("{line}?")))
            .count()
    }

    /// N concurrent first checks of a bucket send one HeadBucket; N
    /// concurrent checks of a missing bucket share its answer too.
    #[tokio::test]
    async fn concurrent_cold_checks_send_one_head_bucket() {
        let (engine, fake) = crate::deltaglider::s3_engine().await;
        let state = AppState::for_tests(engine);
        fake.clear();
        fake.set_delay_ms("HEAD_BUCKET", 100);
        let checks = (0..8).map(|_| ensure_bucket_exists(&state, "b"));
        for r in futures::future::join_all(checks).await {
            r.unwrap();
        }
        assert_eq!(head_buckets(&fake, "b"), 1, "{:?}", fake.requests());

        fake.fail("HEAD_BUCKET", "/nope", 404, "NoSuchBucket", u32::MAX);
        let checks = (0..8).map(|_| ensure_bucket_exists(&state, "nope"));
        for r in futures::future::join_all(checks).await {
            assert!(matches!(r, Err(S3Error::NoSuchBucket(_))), "{r:?}");
        }
        assert_eq!(head_buckets(&fake, "nope"), 1, "{:?}", fake.requests());
        // Missing is not remembered: the next check asks again.
        assert!(ensure_bucket_exists(&state, "nope").await.is_err());
        assert_eq!(head_buckets(&fake, "nope"), 2);
    }

    /// Review 2d: when the shared HeadBucket fails, every waiter gets its
    /// error. Before, each waiter then sent its own HeadBucket.
    #[tokio::test]
    async fn a_failed_cold_check_answers_all_its_waiters() {
        let (engine, fake) = crate::deltaglider::s3_engine().await;
        let state = AppState::for_tests(engine);
        fake.clear();
        fake.set_delay_ms("HEAD_BUCKET", 100);
        fake.fail("HEAD_BUCKET", "/zb", 503, "SlowDown", u32::MAX);
        let checks = (0..8).map(|_| ensure_bucket_exists(&state, "zb"));
        let all = futures::future::join_all(checks).await;
        assert!(all.iter().all(|r| r.is_err()), "{all:?}");
        assert_eq!(head_buckets(&fake, "zb"), 1, "{:?}", fake.requests());
        // The error is not kept: the next check asks again.
        assert!(ensure_bucket_exists(&state, "zb").await.is_err());
        assert_eq!(head_buckets(&fake, "zb"), 2);
    }

    /// Review 2d: a check cancelled during its HeadBucket (the client went
    /// away) leaves no flight behind. Before, each one stayed for good.
    #[tokio::test]
    async fn a_cancelled_cold_check_leaves_no_flight() {
        let (engine, fake) = crate::deltaglider::s3_engine().await;
        let state = AppState::for_tests(engine);
        fake.set_delay_ms("HEAD_BUCKET", 300);
        for i in 0..5 {
            let name = format!("gone{i}");
            let check = ensure_bucket_exists(&state, &name);
            let _ = tokio::time::timeout(std::time::Duration::from_millis(50), check).await;
        }
        let left = BUCKET_FLIGHTS.count(|(_, b)| b.starts_with("gone"));
        assert_eq!(left, 0, "cancelled checks left their flights");
    }

    /// The write paths check a bucket with one HeadBucket per bucket per
    /// short interval, not one per PUT or UploadPart. A missing bucket is
    /// asked again every time (a bucket created a moment later is usable).
    #[tokio::test]
    async fn a_write_checks_its_bucket_once_per_interval() {
        let (engine, fake) = crate::deltaglider::s3_engine().await;
        let state = AppState::for_tests(engine);
        fake.clear();
        for _ in 0..3 {
            ensure_bucket_exists(&state, "b").await.unwrap();
        }
        assert_eq!(head_buckets(&fake, "b"), 1, "{:?}", fake.requests());
        fake.fail("HEAD_BUCKET", "/nope", 404, "NoSuchBucket", u32::MAX);
        for _ in 0..2 {
            let err = ensure_bucket_exists(&state, "nope").await.unwrap_err();
            assert!(matches!(err, S3Error::NoSuchBucket(_)), "{err:?}");
        }
        assert_eq!(head_buckets(&fake, "nope"), 2);
    }
}
