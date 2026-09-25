// SPDX-License-Identifier: BUSL-1.1

//! `GET /_/api/admin/deltaspace/savings?bucket=X&prefix=Y`
//!
//! Per-prefix savings totals for the SPA's "compression chip" + any
//! future visualisation that wants honest reference-aware numbers
//! without forcing the user to trigger a full bucket scan.
//!
//! Why a dedicated endpoint vs. computing client-side: the SPA can't
//! see `reference.bin` files (the engine hides them from list_objects
//! by design), so any client-side aggregator undercounts stored bytes
//! by one reference per deltaspace. Centralising the math here closes
//! that gap once for every consumer.
//!
//! Cost model: walks `engine.list_objects(prefix)` paginated for the
//! user-visible side, then `engine.list_deltaspace_references(prefix,
//! limit=Some(REFERENCE_SCAN_LIMIT))` for the on-disk reference cost.
//! Result is cached for 30 s per `(bucket, prefix)` via [`moka`]'s
//! coalescing `try_get_with` — concurrent misses for the same key
//! share one in-flight computation rather than each spawning a fresh
//! paginated scan. On large prefixes (>100 k objects OR >1 k
//! deltaspaces) the response carries `truncated: true` and the totals
//! are a lower bound — the operator-facing path for that is the
//! bucket-wide scan in `bucket_scan.rs`.
//!
//! HA caveat: the cache is per-instance. After a PUT routed to
//! instance A, instance B can serve pre-PUT savings up to 30 s later.
//! That window is intentional — the chip is a fingerprint, not an
//! invoice. Operators wanting cross-instance freshness invoke
//! `/_/api/admin/diagnostics/scan/start` which writes to a shared
//! disk-cached `ScanResult`.

use super::path_guard::{AdminBucket, AdminObjectPath};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use chrono::{DateTime, Utc};
use moka::future::Cache;
use serde::{Deserialize, Serialize};

use crate::api::handlers::AppState;
use crate::deltaglider::{SavingsTotals, REFERENCE_SCAN_LIMIT};

/// Max objects we'll walk before bailing with `truncated: true`. The
/// per-bucket scan in `bucket_scan.rs` is the path for huge prefixes.
const MAX_LISTING_OBJECTS: usize = 100_000;

/// In-memory cache TTL for savings responses. Tight enough that
/// freshly-uploaded data shows up quickly, loose enough that browsing
/// a tree doesn't fire scans every click.
const CACHE_TTL: Duration = Duration::from_secs(30);

/// Page size for the user-visible LIST walk. Larger than the dashboard
/// scan because we expect smaller scopes (prefix not whole bucket).
const PAGE_SIZE: u32 = 1000;

/// Maximum cached entries. Sized generously — each entry is small
/// (~120 bytes) and the cost of a miss is the same paginated scan
/// either way. Beyond this moka does true TinyLFU eviction.
const CACHE_MAX_ENTRIES: u64 = 4096;

#[derive(Deserialize)]
pub struct SavingsQuery {
    pub bucket: AdminBucket,
    /// Default empty = whole bucket (same shape as the bucket scan).
    #[serde(default)]
    pub prefix: AdminObjectPath,
}

#[derive(Serialize, Clone)]
pub struct SavingsResponse {
    pub bucket: String,
    pub prefix: String,
    pub totals: SavingsTotals,
    /// Computed savings percentage 0..=99.99, or null when there's
    /// nothing under the prefix yet (avoids the UI showing "0%" for an
    /// empty browse).
    pub savings_percentage: Option<f64>,
    /// True when the walk hit `MAX_LISTING_OBJECTS` OR the reference
    /// scan hit `REFERENCE_SCAN_LIMIT`. The UI shows a `+` suffix and a
    /// "scope truncated" tooltip; numbers are a strict lower bound.
    pub truncated: bool,
    /// UTC timestamp when this scan finished. The SPA renders a
    /// "Recomputed Xs ago" hint from it.
    pub computed_at: DateTime<Utc>,
}

/// Cache + coalescing harness for per-prefix savings responses.
///
/// Implementation: `moka::future::Cache` provides three things in one
/// data structure:
///   1. TTL + TinyLFU eviction (replaces the hand-rolled
///      `RwLock<HashMap>` + "drop oldest" comment that was actually
///      drop-arbitrary).
///   2. `try_get_with` coalescing: concurrent misses for the same key
///      share ONE in-flight future. Closes the thundering-herd window
///      where N tabs hitting a cold prefix all fire N paginated
///      scans.
///   3. Lock-free reads on cache hit.
///
/// The `Arc<SavingsResponse>` value is shared by clone — cheap because
/// the inner struct is ~120 bytes and clone is just an Arc bump.
pub struct SavingsCache {
    inner: Cache<String, Arc<SavingsResponse>>,
}

impl SavingsCache {
    pub fn new() -> Self {
        Self {
            inner: Cache::builder()
                .max_capacity(CACHE_MAX_ENTRIES)
                .time_to_live(CACHE_TTL)
                .build(),
        }
    }

    fn cache_key(bucket: &str, prefix: &str) -> String {
        format!("{}\x00{}", bucket, prefix)
    }

    /// Get-or-compute with single-flight coalescing.
    ///
    /// If a value is cached and fresh, returns it. If not, runs `init`
    /// — but if another caller is already running `init` for the same
    /// key, both share that one future. Errors propagate to ALL
    /// awaiters of that key (moka semantics): if the first caller's
    /// compute fails, subsequent calls within the same await window
    /// see the same error, and the cache stays empty.
    pub async fn get_or_compute<F, Fut>(
        &self,
        bucket: &str,
        prefix: &str,
        init: F,
    ) -> Result<Arc<SavingsResponse>, String>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<SavingsResponse, String>>,
    {
        let key = Self::cache_key(bucket, prefix);
        // moka's try_get_with takes `Future<Output = Result<V, E>>`
        // and stores ONLY the success value; on error it returns the
        // error wrapped in `Arc<E>` and does not cache. That's exactly
        // the semantics we want: a transient backend failure should
        // not poison the cache for 30 s.
        self.inner
            .try_get_with(key, async move {
                let v = init().await?;
                Ok::<_, String>(Arc::new(v))
            })
            .await
            .map_err(|arc_err| (*arc_err).clone())
    }

    /// Drop a single entry. Wired up if we ever add write-path
    /// invalidation hooks; not exercised yet (the 30 s TTL is
    /// considered acceptable lag for the savings display).
    #[allow(dead_code)]
    pub async fn invalidate(&self, bucket: &str, prefix: &str) {
        self.inner
            .invalidate(&Self::cache_key(bucket, prefix))
            .await;
    }
}

impl Default for SavingsCache {
    fn default() -> Self {
        Self::new()
    }
}

/// `GET /_/api/admin/deltaspace/savings?bucket=X&prefix=Y`
pub async fn get_savings(
    State(state): State<Arc<crate::api::admin::AdminState>>,
    Query(q): Query<SavingsQuery>,
) -> impl IntoResponse {
    // Defensive: empty bucket is meaningless — clients shouldn't ask
    // and the listing path would explode if they did.
    if q.bucket.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "bucket required"})),
        )
            .into_response();
    }

    let s3_state = state.s3_state.clone();
    let bucket_for_compute = q.bucket.clone();
    let prefix_for_compute = q.prefix.clone();

    let result = state
        .savings_cache
        .get_or_compute(&q.bucket, &q.prefix, move || async move {
            compute_savings(&s3_state, &bucket_for_compute, &prefix_for_compute).await
        })
        .await;

    match result {
        Ok(arc) => {
            // moka returns `Arc<SavingsResponse>`. Serde will follow
            // the Arc transparently via the inner Serialize impl.
            (StatusCode::OK, Json((*arc).clone())).into_response()
        }
        Err(msg) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({"error": msg})),
        )
            .into_response(),
    }
}

/// How a [`scan_totals`] walk ended early.
pub(crate) enum TotalsScanError {
    Cancelled,
    Failed(String),
}

/// Options for [`scan_totals`].
pub(crate) struct TotalsScanOpts<'a> {
    pub prefix: &'a str,
    /// Stop after this many user-visible objects (`truncated` = true).
    pub object_cap: Option<usize>,
    /// Cap on the reference walk (see `list_deltaspace_references`).
    pub ref_limit: Option<usize>,
    pub cancel: Option<&'a tokio_util::sync::CancellationToken>,
}

/// THE savings scan: page every user-visible object through
/// `SavingsTotals`, then fold in each `reference.bin` (hidden from LIST
/// but real stored bytes). `on_page(totals, pages_done, has_more)` runs
/// after each page. Shared by the savings chip, the dashboard scan and the
/// usage Refresh, which each carried a copy of this loop. The engine is
/// re-loaded per page, so a config reload mid-scan uses the new engine.
pub(crate) async fn scan_totals(
    s3_state: &Arc<AppState>,
    bucket: &str,
    opts: TotalsScanOpts<'_>,
    mut on_page: impl FnMut(&SavingsTotals, u32, bool),
) -> Result<(SavingsTotals, bool), TotalsScanError> {
    let cancelled = || opts.cancel.is_some_and(|c| c.is_cancelled());
    let mut totals = SavingsTotals::default();
    let mut continuation: Option<String> = None;
    let mut walked: usize = 0;
    let mut truncated = false;
    let mut pages_done: u32 = 0;
    loop {
        if cancelled() {
            return Err(TotalsScanError::Cancelled);
        }
        let engine = s3_state.engine.load();
        let list = engine.list_objects(
            bucket,
            opts.prefix,
            None,
            PAGE_SIZE,
            continuation.as_deref(),
            true, // metadata=true so deltas carry delta_size (stored bytes)
        );
        let page = match opts.cancel {
            Some(cancel) => tokio::select! {
                _ = cancel.cancelled() => return Err(TotalsScanError::Cancelled),
                r = list => r,
            },
            None => list.await,
        }
        .map_err(|e| TotalsScanError::Failed(e.to_string()))?;
        for (_key, meta) in &page.objects {
            totals.accumulate(meta);
            walked += 1;
            if opts.object_cap.is_some_and(|cap| walked >= cap) {
                truncated = true;
                break;
            }
        }
        pages_done += 1;
        let has_more = !truncated && page.is_truncated && page.next_continuation_token.is_some();
        on_page(&totals, pages_done, has_more);
        if !has_more {
            break;
        }
        continuation = page.next_continuation_token;
    }
    if cancelled() {
        return Err(TotalsScanError::Cancelled);
    }
    let refs = s3_state
        .engine
        .load()
        .list_deltaspace_references(bucket, opts.prefix, opts.ref_limit)
        .await
        .map_err(|e| TotalsScanError::Failed(e.to_string()))?;
    for (_, meta) in &refs.references {
        totals.accumulate(meta);
    }
    Ok((totals, truncated || refs.truncated))
}

/// The savings chip: a capped [`scan_totals`]. The reference walk is
/// capped too — a bucket with 50k deltaspaces would otherwise fire 50k
/// HEADs against S3 on every cache miss; the dashboard scan is the
/// exhaustive path. Tests can lower the cap via `DGP_REFERENCE_SCAN_LIMIT`.
async fn compute_savings(
    s3_state: &Arc<AppState>,
    bucket: &str,
    prefix: &str,
) -> Result<SavingsResponse, String> {
    let ref_limit =
        crate::config::env_parse_with_default("DGP_REFERENCE_SCAN_LIMIT", REFERENCE_SCAN_LIMIT);
    let (totals, truncated) = scan_totals(
        s3_state,
        bucket,
        TotalsScanOpts {
            prefix,
            object_cap: Some(MAX_LISTING_OBJECTS),
            ref_limit: Some(ref_limit),
            cancel: None,
        },
        |_, _, _| {},
    )
    .await
    .map_err(|e| match e {
        TotalsScanError::Failed(msg) => msg,
        TotalsScanError::Cancelled => "scan cancelled".to_string(),
    })?;

    let savings_percentage = totals.savings_percentage();
    Ok(SavingsResponse {
        bucket: bucket.to_string(),
        prefix: prefix.to_string(),
        totals,
        savings_percentage,
        truncated,
        computed_at: Utc::now(),
    })
}
