// SPDX-License-Identifier: GPL-3.0-only

//! S3 API request handlers (shared state + helpers).
//!
//! With the legacy axum-handler S3 path retired, the only S3
//! implementation is `src/s3_adapter_s3s.rs` (the `s3s` crate
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
//! - `ensure_bucket_exists`, `debug_headers_enabled`, `audit_log_s3` —
//!   small free helpers reused by the surviving handlers.
//!
//! Pre-consolidation, this module also hosted ~3500 LOC of axum-based
//! S3 handlers (object, bucket, multipart). Those moved into the
//! s3s adapter; the old files are gone.

pub mod form_post;
pub(crate) mod object_helpers;
mod status;

use super::errors::S3Error;
use crate::config_db::ConfigDb;
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
pub struct AppState {
    pub engine: ArcSwap<DynEngine>,
    pub multipart: Arc<MultipartStore>,
    pub metrics: Arc<Metrics>,
    pub usage_scanner: Arc<crate::usage_scanner::UsageScanner>,
    /// Per-instance running bucket-size counter (O(1) reads; None in open-mode
    /// dev when the usage DB couldn't be opened). Re-attached to the engine on
    /// every rebuild so a config reload never drops the counter.
    pub bucket_usage: Option<Arc<crate::bucket_usage::BucketUsage>>,
    pub config_db: Option<Arc<tokio::sync::Mutex<ConfigDb>>>,
    /// Replay cache for form-POST policy signatures. Keyed on the
    /// signature itself; value carries the policy's expiration `Instant`
    /// (NOT the insertion time — form-POST entries need per-entry
    /// TTLs because policy expirations vary from minutes to days) plus a
    /// fingerprint of the (key, body) that the signature first wrote, so
    /// an idempotent re-send of the SAME object is allowed while reuse of
    /// the signature for a DIFFERENT key/body is still blocked.
    /// See `enforce_form_post_replay` in `handlers/form_post.rs`.
    pub form_post_replay:
        Arc<dashmap::DashMap<String, crate::api::handlers::form_post::ReplayEntry>>,
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
///    `ensure_dir` at `src/storage/filesystem.rs::ensure_dir` calls
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
pub(crate) async fn ensure_bucket_exists(
    state: &Arc<AppState>,
    bucket: &str,
) -> Result<(), S3Error> {
    let engine = state.engine.load();
    match engine.head_bucket(bucket).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(S3Error::NoSuchBucket(bucket.to_string())),
        Err(e) => Err(S3Error::from(e)),
    }
}

/// Whether to emit debug headers (x-amz-storage-type, x-deltaglider-stored-size).
/// Checked once at startup from the `DGP_DEBUG_HEADERS` env var.
pub fn debug_headers_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| crate::config::env_bool("DGP_DEBUG_HEADERS", false))
}
