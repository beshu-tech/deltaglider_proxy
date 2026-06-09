// SPDX-License-Identifier: GPL-3.0-only

//! Delta efficiency diagnostics: scan deltaspaces and report bad
//! reference choices.
//!
//! When the first file uploaded to a prefix is a poor match for its
//! siblings (e.g. a stored-compression ZIP picked as the reference for
//! a folder full of deflate-compressed plugins), every subsequent
//! delta encodes at near-100% of the original — turning gigabytes of
//! deduplication potential into wasted storage. This panel surfaces
//! such cases proactively: on demand it walks the deltaspaces in a
//! bucket, computes per-prefix size statistics, and classifies each
//! prefix into a coarse health bucket. Re-uploading a flagged prefix
//! via the proxy picks a better seed and recovers the savings.
//!
//! ## Concurrency model
//!
//! Mirrors [`crate::usage_scanner::UsageScanner`] — same
//! background-task + cache + dedup shape so an operator clicking
//! "Scan" doesn't accidentally fan out a dozen parallel scans on a
//! flaky page-reload, and so a fresh page-load on a previously
//! scanned bucket gets an instant answer from cache.
//!
//! - [`DeltaEfficiencyScanner::get`]: read cached result for a
//!   `(bucket, min_deltas)` pair if present and not stale.
//! - [`DeltaEfficiencyScanner::is_scanning`]: tell the UI whether to
//!   poll vs. show empty-state.
//! - [`DeltaEfficiencyScanner::enqueue_scan`]: spawn a background
//!   scan if not already running. RAII-cleaned dedup key (panic-safe,
//!   same fix as `usage_scanner::ScanInProgressGuard`).
//!
//! ## Pure-function core
//!
//! [`classify_deltaspace`] takes only `(reference_size, &[delta_size])`
//! and returns an [`Efficiency`] verdict. No I/O, fully unit-testable.

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use chrono::{DateTime, Utc};
use futures::StreamExt;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tracing::{debug, warn};

use super::AdminState;
use crate::api::handlers::AppState;
use crate::types::{FileMetadata, StorageInfo};

/// Cache TTL — five minutes mirrors `UsageScanner` so an operator
/// reloading the diagnostics tab doesn't kick off duplicate work.
const CACHE_TTL_SECS: i64 = 300;

// ─── Pure-function core ──────────────────────────────────────────────

/// Coarse health classification for a single deltaspace.
///
/// Thresholds are on the ratio `median_delta / reference_size`:
///
/// * **Excellent**: ratio ≤ 5 % AND median delta ≤ 200 KB. The
///   reference is well-chosen and most siblings are close-cousins.
/// * **Good**: ratio < 20 % but not Excellent. Healthy delta encoding.
/// * **Fair**: 20 % ≤ ratio < 50 %. Common when a single deltaspace
///   mixes multiple variants (e.g. ES 6/7/8/9 plugins); structurally
///   bounded by inherent file dissimilarity.
/// * **Poor**: ratio ≥ 50 %. Strongly suggests a wrong reference
///   baseline — the prefix should be re-uploaded with a better seed
///   (or split into smaller deltaspaces).
/// * **NoReference**: there are deltas but no `reference.bin`.
///   Anomalous; check storage backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Efficiency {
    Excellent,
    Good,
    Fair,
    Poor,
    NoReference,
}

impl Efficiency {
    /// Operator-actionable message for this verdict.
    pub fn explanation(self) -> &'static str {
        match self {
            Efficiency::Excellent => {
                "Reference is well-chosen; deltas compress to a small fraction of the original."
            }
            Efficiency::Good => {
                "Healthy delta encoding; the reference reasonably represents the prefix."
            }
            Efficiency::Fair => {
                "Acceptable but not great — the prefix likely mixes dissimilar files. \
                 Consider splitting into sub-prefixes by file family if storage matters."
            }
            Efficiency::Poor => {
                "Likely wrong reference: deltas are nearly the size of the originals. \
                 Re-upload the prefix so a better seed is chosen, or split into sub-prefixes."
            }
            Efficiency::NoReference => {
                "Deltas exist but no reference.bin was found in this prefix. \
                 GETs on these objects will fail — investigate immediately."
            }
        }
    }
}

/// Pure classifier: given a reference size and a list of delta sizes
/// (any units, but they must be the same), decide the verdict.
///
/// `min_deltas` lets the caller skip prefixes too small to draw a
/// signal from (e.g. 1-delta prefix is just whatever the second
/// upload happened to be).
pub fn classify_deltaspace(
    reference_size: Option<u64>,
    delta_sizes: &[u64],
    min_deltas: usize,
) -> Option<Efficiency> {
    if delta_sizes.len() < min_deltas {
        return None;
    }
    let Some(ref_size) = reference_size else {
        if delta_sizes.is_empty() {
            return None;
        }
        return Some(Efficiency::NoReference);
    };
    if ref_size == 0 {
        return Some(Efficiency::NoReference);
    }

    let median = median_u64(delta_sizes);
    // Ratio: median delta as a fraction of the reference. ≥1 means
    // the delta is at least as big as the reference (the prefix is
    // basically uncompressible against this baseline).
    let ratio = median as f64 / ref_size as f64;

    // Poor: median delta ≥ 50 % of reference. The reference is wrong.
    if ratio >= 0.50 {
        return Some(Efficiency::Poor);
    }
    // Fair: 20–50 %. Multi-variant deltaspace — structural floor.
    if ratio >= 0.20 {
        return Some(Efficiency::Fair);
    }
    // Excellent: tiny absolute median AND tiny ratio.
    if median <= 200 * 1024 && ratio <= 0.05 {
        return Some(Efficiency::Excellent);
    }
    // Good: anything else under 20 %.
    Some(Efficiency::Good)
}

/// Returns the upper median (element at index `len / 2` of the sorted
/// view). For even-length inputs this is the higher of the two middle
/// elements — chosen for cheap u64 math; matches the classification
/// thresholds in `classify_deltaspace`. Returns 0 for empty input.
fn median_u64(values: &[u64]) -> u64 {
    if values.is_empty() {
        return 0;
    }
    // Heap-allocates once; select_nth_unstable is O(n) average and
    // doesn't fully sort like the previous `sort_unstable` did.
    let mut buf: Vec<u64> = values.to_vec();
    let mid = buf.len() / 2;
    let (_, median, _) = buf.select_nth_unstable(mid);
    *median
}

/// `numerator / denominator` as f64, or `None` when the denominator is
/// missing or zero. Guards the `ratio_median` field against
/// divide-by-zero on prefixes with a zero-byte reference.
fn ratio_or_none(numerator: u64, denominator: Option<u64>) -> Option<f64> {
    let d = denominator?;
    if d == 0 {
        return None;
    }
    Some(numerator as f64 / d as f64)
}

// ─── I/O layer: types ─────────────────────────────────────────────────

/// Per-prefix efficiency report row, returned over the admin API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeltaspaceReport {
    pub bucket: String,
    pub prefix: String,
    pub deltas: usize,
    pub passthrough: usize,
    pub reference_bytes: Option<u64>,
    pub total_delta_bytes: u64,
    pub total_original_bytes: u64,
    pub median_delta_bytes: u64,
    pub max_delta_bytes: u64,
    pub savings_bytes: i64, // total_original - (reference + total_delta)
    pub efficiency: Efficiency,
    /// `median_delta_bytes / reference_bytes`. `None` when there is no
    /// reference. The headline number for the redesigned timeline view —
    /// computed server-side so the client doesn't repeat the division.
    #[serde(default)]
    pub ratio_median: Option<f64>,
    /// True when the report was built from a HEAD-free scan. In that
    /// mode `total_original_bytes` is a **lower bound** (passthrough
    /// sizes only — delta originals are not recovered without HEAD)
    /// and `savings_bytes` is `0` rather than the real saving. The
    /// classifier's verdict and `ratio_median` are unaffected. UIs MUST
    /// gate on this before displaying the two affected fields.
    #[serde(default)]
    pub original_size_estimated: bool,
    /// Operator-facing explanation derived from `efficiency`.
    /// Inlined here so the frontend doesn't need to duplicate the
    /// classification text.
    pub explanation: String,
}

/// Top-level response. Sorted with worst-efficiency first by default.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EfficiencyResponse {
    pub bucket: String,
    pub scanned_deltaspaces: usize,
    pub reported_deltaspaces: usize,
    pub min_deltas: usize,
    pub reports: Vec<DeltaspaceReport>,
    /// When this scan was completed. The frontend uses this to render
    /// "scanned 2m ago" so the operator knows whether the data is fresh.
    pub computed_at: DateTime<Utc>,
    /// True when this response was served from cache rather than freshly
    /// computed. Lets the UI render a "cached — re-scan?" affordance.
    #[serde(default)]
    pub cached: bool,
}

// ─── Background scanner with cache + dedup ──────────────────────────

/// Background scanner. Same `Arc<RwLock<...>>` shape as
/// [`UsageScanner`](crate::usage_scanner::UsageScanner) — one cache
/// (`bucket|min_deltas` → `EfficiencyResponse`), one dedup set.
pub struct DeltaEfficiencyScanner {
    cache: Arc<RwLock<HashMap<String, EfficiencyResponse>>>,
    scanning: Arc<RwLock<HashSet<String>>>,
}

impl Default for DeltaEfficiencyScanner {
    fn default() -> Self {
        Self::new()
    }
}

/// RAII guard mirroring `UsageScanner::ScanInProgressGuard` — clears
/// the dedup entry on drop, including drop-on-panic. Without this, a
/// panic in the scan future would leave the bucket permanently marked
/// "in progress" until process restart.
struct ScanInProgressGuard {
    scanner: Arc<DeltaEfficiencyScanner>,
    key: String,
}

impl Drop for ScanInProgressGuard {
    fn drop(&mut self) {
        self.scanner.scanning.write().remove(&self.key);
    }
}

impl DeltaEfficiencyScanner {
    pub fn new() -> Self {
        Self {
            cache: Arc::new(RwLock::new(HashMap::new())),
            scanning: Arc::new(RwLock::new(HashSet::new())),
        }
    }

    fn cache_key(bucket: &str, min_deltas: usize) -> String {
        format!("{bucket}|{min_deltas}")
    }

    /// Read the cached entry for this `(bucket, min_deltas)` pair if
    /// present AND younger than [`CACHE_TTL_SECS`]. Stale entries are
    /// ignored on read; they get overwritten by the next scan.
    pub fn get(&self, bucket: &str, min_deltas: usize) -> Option<EfficiencyResponse> {
        let key = Self::cache_key(bucket, min_deltas);
        let cache = self.cache.read();
        let entry = cache.get(&key)?.clone();
        let age = Utc::now()
            .signed_duration_since(entry.computed_at)
            .num_seconds();
        if age > CACHE_TTL_SECS {
            return None;
        }
        Some(EfficiencyResponse {
            cached: true,
            ..entry
        })
    }

    pub fn is_scanning(&self, bucket: &str, min_deltas: usize) -> bool {
        let key = Self::cache_key(bucket, min_deltas);
        self.scanning.read().contains(&key)
    }

    /// Spawn a background scan if not already running. Returns true
    /// if a new scan was started, false if one was already in flight
    /// (or cache is fresh).
    pub fn enqueue_scan(
        self: &Arc<Self>,
        bucket: String,
        min_deltas: usize,
        s3_state: Arc<AppState>,
    ) -> bool {
        let key = Self::cache_key(&bucket, min_deltas);

        // Dedup: skip if already scanning this (bucket, min_deltas).
        {
            let mut scanning = self.scanning.write();
            if !scanning.insert(key.clone()) {
                debug!(
                    bucket = %bucket,
                    min_deltas,
                    "Delta-efficiency scan already in progress, skipping"
                );
                return false;
            }
        }

        let scanner = Arc::clone(self);
        tokio::spawn(async move {
            debug!(bucket = %bucket, min_deltas, "Starting delta-efficiency scan");

            let _scan_guard = ScanInProgressGuard {
                scanner: scanner.clone(),
                key: key.clone(),
            };

            match Self::do_scan(&s3_state, &bucket, min_deltas).await {
                Ok(entry) => {
                    debug!(
                        bucket = %bucket,
                        scanned = entry.scanned_deltaspaces,
                        reported = entry.reported_deltaspaces,
                        "Delta-efficiency scan complete"
                    );
                    scanner.cache.write().insert(key.clone(), entry);
                }
                Err(e) => {
                    warn!(
                        bucket = %bucket,
                        min_deltas,
                        error = %e,
                        "Delta-efficiency scan failed"
                    );
                }
            }
            // _scan_guard drops here, removing the dedup key.
        });

        true
    }

    /// Walk every deltaspace in `bucket` and build the efficiency
    /// response.
    ///
    /// ## Cost model
    ///
    /// One `list_deltaspaces` (single bucket-wide LIST) + one
    /// `scan_deltaspace_lite` per prefix, fanned out
    /// `PARALLEL_PREFIX_SCANS`-at-a-time. **No HEAD calls** —
    /// `scan_deltaspace_lite` reads everything we need from listing
    /// data alone (see `StorageBackend::scan_deltaspace_lite` docs).
    ///
    /// On the migration bucket (141 prefixes × ~500 deltas), the
    /// previous serial + HEAD-storm shape took ~60s+ and blew the
    /// frontend timeout. This shape lands in ~3-5s.
    async fn do_scan(
        s3_state: &AppState,
        bucket: &str,
        min_deltas: usize,
    ) -> Result<EfficiencyResponse, String> {
        let engine = s3_state.engine.load_full();
        let prefixes = engine
            .storage()
            .list_deltaspaces(bucket)
            .await
            .map_err(|e| format!("list_deltaspaces failed: {e}"))?;
        let scanned = prefixes.len();

        // Fan out per-prefix scans. `buffer_unordered` bounds the in-
        // flight count — too low and we leave throughput on the table,
        // too high and we risk S3 SlowDown (or process FD pressure on
        // filesystem backends). 8 is conservative and works well across
        // both backends.
        //
        // We clone `Arc<DynEngine>` into each future so the engine
        // stays alive across the awaits even if a hot-reload swaps the
        // ArcSwap mid-scan.
        let bucket_owned = bucket.to_string();
        let mut scan_stream = futures::stream::iter(prefixes.into_iter().map(|prefix| {
            let engine = engine.clone();
            let bucket = bucket_owned.clone();
            async move {
                let scan = engine
                    .storage()
                    .scan_deltaspace_lite(&bucket, &prefix)
                    .await;
                (prefix, scan)
            }
        }))
        .buffer_unordered(PARALLEL_PREFIX_SCANS);

        let mut reports: Vec<DeltaspaceReport> = Vec::new();
        while let Some((prefix, scan_result)) = scan_stream.next().await {
            let lite = match scan_result {
                Ok(v) => v,
                Err(e) => {
                    warn!(
                        "delta-efficiency: scan_deltaspace_lite failed for {}/{}: {}",
                        bucket, prefix, e
                    );
                    continue;
                }
            };
            // The backend tells us whether `file_size` on deltas is
            // the true original (filesystem default path with xattr)
            // or just the on-disk delta size (S3 override skips
            // HEAD). We pass that through so the report can honestly
            // mark `original_size_estimated` for the UI.
            if let Some(report) = build_report_for_prefix(
                bucket,
                prefix,
                &lite.metadata,
                min_deltas,
                lite.originals_estimated,
            ) {
                reports.push(report);
            }
        }

        sort_reports_worst_first(&mut reports);

        let reported = reports.len();
        Ok(EfficiencyResponse {
            bucket: bucket.to_string(),
            scanned_deltaspaces: scanned,
            reported_deltaspaces: reported,
            min_deltas,
            reports,
            computed_at: Utc::now(),
            cached: false,
        })
    }
}

/// How many per-prefix scans to run concurrently. Balanced against the
/// S3 client connection pool and FD limits on filesystem backends. 8
/// is conservative — moving up gives diminishing returns on bandwidth-
/// bound LISTs and risks SlowDown on S3.
const PARALLEL_PREFIX_SCANS: usize = 8;

/// Stable severity ordering: NoReference (broken — investigate first),
/// then Poor → Fair → Good → Excellent. Lower number = surface earlier.
fn efficiency_severity(e: Efficiency) -> u8 {
    match e {
        Efficiency::NoReference => 0,
        Efficiency::Poor => 1,
        Efficiency::Fair => 2,
        Efficiency::Good => 3,
        Efficiency::Excellent => 4,
    }
}

/// Sort reports for the UI: worst severity first, then biggest waste,
/// then prefix ASC as a deterministic tiebreaker (without it,
/// `buffer_unordered` completion order would make tied rows flip
/// positions across reloads).
fn sort_reports_worst_first(reports: &mut [DeltaspaceReport]) {
    reports.sort_by(|a, b| {
        efficiency_severity(a.efficiency)
            .cmp(&efficiency_severity(b.efficiency))
            .then_with(|| b.total_delta_bytes.cmp(&a.total_delta_bytes))
            .then_with(|| a.prefix.cmp(&b.prefix))
    });
}

/// Pure aggregator: from a single deltaspace's scan, build a
/// `DeltaspaceReport` if it meets the `min_deltas` floor, else None.
///
/// Separated from `do_scan` so it stays unit-testable without
/// spinning up a storage backend.
///
/// `originals_estimated` mirrors the field in
/// [`StorageBackend::scan_deltaspace_lite`]'s result: when true, the
/// `file_size` on every `Delta` entry is the on-disk delta size rather
/// than the original-file size. In that case we can't honestly compute
/// `total_original_bytes` or `savings_bytes` — both are reported as
/// `0` and the flag propagates to the report so the UI can suppress
/// the corresponding columns.
fn build_report_for_prefix(
    bucket: &str,
    prefix: String,
    scan: &[FileMetadata],
    min_deltas: usize,
    originals_estimated: bool,
) -> Option<DeltaspaceReport> {
    let partition = partition_deltaspace_scan(scan, originals_estimated);
    let efficiency = classify_deltaspace(
        partition.reference_bytes,
        &partition.delta_sizes,
        min_deltas,
    )?;

    let median = median_u64(&partition.delta_sizes);
    let max_delta = partition.delta_sizes.iter().copied().max().unwrap_or(0);
    // Route the byte totals through the canonical accumulator so the
    // sums here stay in lockstep with bucket_scan, cli stats, and the
    // SPA chip. We pass the scan slice in directly — `SavingsTotals`
    // handles passthrough / reference / delta bookkeeping the same way
    // it does for every other consumer, and we crucially do NOT double-
    // count delta `file_size` as "original" under lite mode (where
    // `file_size == delta_size` for deltas). That special case is
    // expressed by clearing original_bytes here, not by another inline
    // formula.
    let mut totals = crate::deltaglider::SavingsTotals::default();
    for m in scan {
        totals.accumulate(m);
    }
    if originals_estimated {
        // Lite mode: delta `file_size` is actually on-disk size, not
        // the original. The accumulator adds it to original_bytes —
        // strip the contribution so the savings figure isn't a lie.
        // Passthrough originals stay (their file_size IS the original).
        totals.original_bytes = totals
            .original_bytes
            .saturating_sub(totals.delta_stored_bytes);
    }
    let savings_bytes = if originals_estimated {
        // Without true originals, savings is unknowable. Sentinel "0"
        // (not negative) is what the UI expects to suppress the cell.
        0
    } else {
        totals.saved_bytes_signed()
    };
    let total_delta = totals.delta_stored_bytes;
    let ratio_median = ratio_or_none(median, partition.reference_bytes);

    Some(DeltaspaceReport {
        bucket: bucket.to_string(),
        prefix,
        deltas: partition.delta_sizes.len(),
        passthrough: partition.passthrough_count,
        reference_bytes: partition.reference_bytes,
        total_delta_bytes: total_delta,
        total_original_bytes: totals.original_bytes,
        median_delta_bytes: median,
        max_delta_bytes: max_delta,
        savings_bytes,
        efficiency,
        ratio_median,
        original_size_estimated: originals_estimated,
        explanation: efficiency.explanation().to_string(),
    })
}

/// Sums collected from a single deltaspace's scan, prior to verdict
/// computation. Kept separate from `build_report_for_prefix` so the
/// partition loop has its own name and unit-test surface.
struct DeltaspacePartition {
    reference_bytes: Option<u64>,
    delta_sizes: Vec<u64>,
    passthrough_count: usize,
    /// Sum of `file_size` across non-reference entries. Under
    /// `originals_estimated` the delta `file_size` is the on-disk delta
    /// size (not the original), so it's excluded — otherwise we'd
    /// double-count delta storage as "original" and make
    /// `savings_bytes` look negative on healthy prefixes.
    total_original: u64,
}

fn partition_deltaspace_scan(
    scan: &[FileMetadata],
    originals_estimated: bool,
) -> DeltaspacePartition {
    let mut p = DeltaspacePartition {
        reference_bytes: None,
        delta_sizes: Vec::new(),
        passthrough_count: 0,
        total_original: 0,
    };
    for m in scan {
        match &m.storage_info {
            StorageInfo::Reference { .. } => {
                p.reference_bytes = Some(m.file_size);
            }
            StorageInfo::Delta { delta_size, .. } => {
                p.delta_sizes.push(*delta_size);
                if !originals_estimated {
                    p.total_original = p.total_original.saturating_add(m.file_size);
                }
            }
            StorageInfo::Passthrough => {
                p.passthrough_count += 1;
                p.total_original = p.total_original.saturating_add(m.file_size);
            }
        }
    }
    p
}

// ─── Admin API handlers ─────────────────────────────────────────────

/// Clamp a caller-supplied `min_deltas` into a sane range. Defaults to 3,
/// floors at 1 (0 would be meaningless) and caps at 1000 so a huge value
/// can't blow up cache keys / scan work.
fn clamp_min_deltas(min_deltas: Option<usize>) -> usize {
    min_deltas.unwrap_or(3).clamp(1, 1000)
}

#[derive(Deserialize)]
pub struct EfficiencyQuery {
    /// Bucket to scan. Required to bound work per request.
    pub bucket: String,
    /// Skip prefixes with fewer than this many deltas. Default 3.
    #[serde(default)]
    pub min_deltas: Option<usize>,
}

/// `GET /_/api/admin/diagnostics/delta-efficiency?bucket=X&min_deltas=N`
///
/// Returns:
/// * `200 OK` with the full response when a fresh cached result
///   (or one we just computed inline) is available.
/// * `202 Accepted` with `{ scanning: true }` when no fresh cache
///   exists and a background scan is now running. The frontend
///   should poll the same endpoint until it gets a 200.
pub async fn get_delta_efficiency(
    State(state): State<Arc<AdminState>>,
    axum::extract::Query(q): axum::extract::Query<EfficiencyQuery>,
) -> impl IntoResponse {
    let min_deltas = clamp_min_deltas(q.min_deltas);

    // Cache hit → return immediately.
    if let Some(cached) = state.delta_efficiency_scanner.get(&q.bucket, min_deltas) {
        return (StatusCode::OK, Json(cached)).into_response();
    }

    // No fresh cache → enqueue a background scan and tell the caller
    // to poll. Same affordance as `UsageScanner::enqueue_scan` →
    // `get_usage`'s 404+`scanning: true` shape, but we use 202 here
    // because "the work has been accepted" is the more accurate
    // semantic than "not found".
    let started = state.delta_efficiency_scanner.enqueue_scan(
        q.bucket.clone(),
        min_deltas,
        state.s3_state.clone(),
    );
    let scanning = started
        || state
            .delta_efficiency_scanner
            .is_scanning(&q.bucket, min_deltas);

    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "bucket": q.bucket,
            "min_deltas": min_deltas,
            "scanning": scanning,
            "status": if started { "scan_started" } else { "scan_already_running" },
        })),
    )
        .into_response()
}

#[derive(Deserialize)]
pub struct EfficiencyScanRequest {
    pub bucket: String,
    #[serde(default)]
    pub min_deltas: Option<usize>,
}

/// `POST /_/api/admin/diagnostics/delta-efficiency/scan` — operator
/// "Re-scan" affordance. Force-enqueues a background scan, ignoring
/// the cache (the cache will be overwritten on completion). Always
/// returns 202.
pub async fn post_delta_efficiency_scan(
    State(state): State<Arc<AdminState>>,
    Json(req): Json<EfficiencyScanRequest>,
) -> impl IntoResponse {
    let min_deltas = clamp_min_deltas(req.min_deltas);
    let started = state.delta_efficiency_scanner.enqueue_scan(
        req.bucket.clone(),
        min_deltas,
        state.s3_state.clone(),
    );
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "bucket": req.bucket,
            "min_deltas": min_deltas,
            "status": if started { "scan_started" } else { "scan_already_running" },
        })),
    )
}

// ─── Per-prefix verified scan (HEAD-based, opt-in) ──────────────────

/// Per-delta breakdown returned by [`verify_delta_efficiency`].
/// Suitable for percentile / distribution displays in the UI.
#[derive(Debug, Clone, Serialize)]
pub struct VerifiedDelta {
    pub key: String,
    pub original_size: u64,
    pub delta_size: u64,
    /// `delta_size / original_size` — the true compression ratio per
    /// file. Different from the lite-scan's `delta / reference`
    /// proxy: this one says "the encoded delta took X% of the
    /// original file". Lower is better; > 1 means xdelta3 made the
    /// file bigger.
    pub ratio: f64,
}

/// Result returned by `POST /_/api/admin/diagnostics/delta-efficiency/verify`.
/// All sizes are in bytes; `true_savings_bytes` is signed so the
/// frontend can render "−860 MB lost" the same way as "+5.83 GB saved".
#[derive(Debug, Clone, Serialize)]
pub struct VerifyResponse {
    pub bucket: String,
    pub prefix: String,
    pub reference_bytes: Option<u64>,
    pub deltas: usize,
    pub passthrough_count: usize,
    pub total_original_bytes: u64,
    pub total_stored_bytes: u64,
    pub true_savings_bytes: i64,
    /// `1 − total_stored / total_original` as a fraction. `None` when
    /// `total_original == 0` (would otherwise divide by zero).
    pub compression_ratio: Option<f64>,
    /// Per-delta breakdown — sorted ascending by `ratio` so the UI's
    /// p10 / p50 / p90 picks fall on the right indices without
    /// re-sorting client-side.
    pub per_delta: Vec<VerifiedDelta>,
}

#[derive(Deserialize)]
pub struct VerifyRequest {
    pub bucket: String,
    pub prefix: String,
}

/// `POST /_/api/admin/diagnostics/delta-efficiency/verify` — operator
/// "deep dive" affordance for ONE prefix. Calls the HEAD-based
/// `scan_deltaspace` (NOT the lite path) so we recover exact original
/// sizes from per-delta user metadata, then computes true savings.
///
/// Cost: one prefix-scoped LIST + one HEAD per delta in the prefix
/// (bounded-parallel via `bounded_head_calls` inside the backend).
/// On the migration bucket's largest prefix (~700 deltas) this is
/// ~700 HEADs, ~1-2 s wall-clock at 64-way concurrency. Tolerable
/// because it's per-row opt-in, not bulk.
///
/// Why not enable this by default? See [`scan_deltaspace_lite`]'s
/// docstring — for a 308-prefix bucket, the bulk version is ~70k
/// HEADs and times out. This endpoint surfaces the trade explicitly:
/// the operator pays the cost only when they want true numbers for
/// a specific prefix.
pub async fn verify_delta_efficiency(
    State(state): State<Arc<AdminState>>,
    Json(req): Json<VerifyRequest>,
) -> impl IntoResponse {
    let engine = state.s3_state.engine.load_full();
    let scan = match engine
        .storage()
        .scan_deltaspace(&req.bucket, &req.prefix)
        .await
    {
        Ok(v) => v,
        Err(e) => {
            warn!(
                "delta-efficiency verify failed for {}/{}: {}",
                req.bucket, req.prefix, e
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };

    let response = build_verify_response(&req.bucket, &req.prefix, &scan);
    (StatusCode::OK, Json(response)).into_response()
}

/// Pure aggregator over a HEAD-resolved scan. Same shape as
/// [`build_report_for_prefix`] but trusts `m.file_size` on deltas as
/// the original size (the HEAD path populates it from user metadata).
///
/// Separated so it can be unit-tested without standing up a backend.
fn build_verify_response(bucket: &str, prefix: &str, scan: &[FileMetadata]) -> VerifyResponse {
    // Per-delta diagnostic rows: keep ratio computed per-object so the
    // UI can sort + show percentiles. The TOTALS, however, route
    // through the canonical SavingsTotals accumulator — no separate
    // formula lives here.
    let mut per_delta: Vec<VerifiedDelta> = Vec::new();
    let mut reference_bytes: Option<u64> = None;
    let mut totals = crate::deltaglider::SavingsTotals::default();
    for m in scan {
        totals.accumulate(m);
        if let StorageInfo::Reference { .. } = &m.storage_info {
            reference_bytes = Some(m.file_size);
        }
        if let StorageInfo::Delta { delta_size, .. } = &m.storage_info {
            let original = m.file_size;
            let delta = *delta_size;
            let ratio = if original == 0 {
                // Pathological — a 0-byte original. Treat as ratio 0
                // so it doesn't blow out percentile sorts.
                0.0
            } else {
                delta as f64 / original as f64
            };
            per_delta.push(VerifiedDelta {
                key: m.original_name.clone(),
                original_size: original,
                delta_size: delta,
                ratio,
            });
        }
    }

    // Sort per_delta by ratio ascending so the UI gets percentile
    // picks for free at indices [n*0.1], [n*0.5], [n*0.9].
    per_delta.sort_by(|a, b| {
        a.ratio
            .partial_cmp(&b.ratio)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    VerifyResponse {
        bucket: bucket.to_string(),
        prefix: prefix.to_string(),
        reference_bytes,
        deltas: per_delta.len(),
        passthrough_count: totals.passthrough_count as usize,
        total_original_bytes: totals.original_bytes,
        total_stored_bytes: totals.stored_bytes,
        true_savings_bytes: totals.saved_bytes_signed(),
        compression_ratio: totals.compression_ratio(),
        per_delta,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── classify_deltaspace truth table ─────────────────────────────

    #[test]
    fn empty_below_min_returns_none() {
        assert_eq!(classify_deltaspace(Some(1_000_000), &[], 3), None);
        assert_eq!(classify_deltaspace(Some(1_000_000), &[100, 200], 3), None);
    }

    #[test]
    fn no_reference_when_no_ref_but_deltas_exist() {
        assert_eq!(
            classify_deltaspace(None, &[100, 200, 300], 3),
            Some(Efficiency::NoReference)
        );
    }

    #[test]
    fn no_reference_when_zero_size() {
        assert_eq!(
            classify_deltaspace(Some(0), &[100, 200, 300], 3),
            Some(Efficiency::NoReference)
        );
    }

    /// Median delta ≥ 50 % of reference → Poor.
    /// Real prod scenario: pre5 had Kibana zip ref (61 MB) and ES
    /// plugin deltas at 91 MB → ratio > 1.0.
    #[test]
    fn median_at_or_above_half_is_poor() {
        // 91 MB delta, 61 MB ref ⇒ ratio ≈ 1.49 — Poor
        assert_eq!(
            classify_deltaspace(Some(61 * 1024 * 1024), &[91 * 1024 * 1024; 5], 3),
            Some(Efficiency::Poor),
        );
        // Edge: exactly 50 %
        assert_eq!(
            classify_deltaspace(Some(1_000_000), &[500_000; 5], 3),
            Some(Efficiency::Poor),
        );
    }

    /// 20 % ≤ median < 50 % → Fair (typical multi-variant prefix).
    #[test]
    fn median_between_20_and_50_pct_is_fair() {
        // 30 % of reference
        assert_eq!(
            classify_deltaspace(Some(1_000_000), &[300_000; 5], 3),
            Some(Efficiency::Fair),
        );
        // Edge: exactly 20 %
        assert_eq!(
            classify_deltaspace(Some(1_000_000), &[200_000; 5], 3),
            Some(Efficiency::Fair),
        );
    }

    /// < 20 % AND median ≤ 200 KB AND ≤ 5 % → Excellent.
    /// Real prod scenario: ror/builds/.../free with median 70 KB
    /// against 19.5 MB ref → 0.36 % ratio.
    #[test]
    fn small_absolute_and_small_ratio_is_excellent() {
        // 70 KB median against 19.5 MB ref ⇒ ~0.35 % — Excellent
        assert_eq!(
            classify_deltaspace(Some(19_500_000), &[70_000; 49], 3),
            Some(Efficiency::Excellent),
        );
        // Edge: exactly 200 KB / 5 % boundary
        assert_eq!(
            classify_deltaspace(Some(4_000_000), &[200_000; 5], 3),
            Some(Efficiency::Excellent),
        );
    }

    /// ≥ 200 KB median (even at low ratio) is Good, not Excellent.
    /// Real prod scenario: pre6/universal had median 270 KB against
    /// 61 MB ref → 0.4 % ratio — small, but absolute size makes it Good.
    #[test]
    fn large_absolute_but_small_ratio_is_good() {
        // 270 KB median, 61 MB ref ⇒ ratio 0.44 % but absolute > 200 KB
        assert_eq!(
            classify_deltaspace(Some(61 * 1024 * 1024), &[270 * 1024; 188], 3),
            Some(Efficiency::Good),
        );
    }

    /// Real prod scenario: ror/builds/1.70.0-pre5 (after the fix)
    /// 2.1 MB median against 91 MB ref ⇒ ratio ≈ 2.4 % — Good
    /// (cross-major delta; the 2 MB floor is structural).
    #[test]
    fn cross_major_delta_is_good() {
        assert_eq!(
            classify_deltaspace(Some(91 * 1024 * 1024), &[2_182_000; 242], 3),
            Some(Efficiency::Good),
        );
    }

    // ── median_u64 sanity ──────────────────────────────────────────

    #[test]
    fn median_picks_middle_element() {
        assert_eq!(median_u64(&[1, 2, 3]), 2);
        assert_eq!(median_u64(&[3, 1, 2]), 2);
        assert_eq!(median_u64(&[5]), 5);
        assert_eq!(median_u64(&[]), 0);
    }

    // ── ratio_or_none ──────────────────────────────────────────────

    #[test]
    fn ratio_or_none_handles_missing_and_zero_denominators() {
        assert_eq!(ratio_or_none(100, None), None);
        assert_eq!(ratio_or_none(100, Some(0)), None);
        assert_eq!(ratio_or_none(0, Some(100)), Some(0.0));
        assert_eq!(ratio_or_none(50, Some(100)), Some(0.5));
        assert_eq!(ratio_or_none(150, Some(100)), Some(1.5));
    }

    // ── Efficiency::explanation ────────────────────────────────────

    #[test]
    fn explanation_is_non_empty_for_every_variant() {
        for e in [
            Efficiency::Excellent,
            Efficiency::Good,
            Efficiency::Fair,
            Efficiency::Poor,
            Efficiency::NoReference,
        ] {
            assert!(!e.explanation().is_empty());
        }
    }

    /// The serde rename should produce stable lowercase JSON that
    /// the frontend type alias `DeltaEfficiency` depends on.
    #[test]
    fn efficiency_serializes_to_lowercase_string() {
        for (variant, expected) in [
            (Efficiency::Excellent, "\"excellent\""),
            (Efficiency::Good, "\"good\""),
            (Efficiency::Fair, "\"fair\""),
            (Efficiency::Poor, "\"poor\""),
            (Efficiency::NoReference, "\"no_reference\""),
        ] {
            let s = serde_json::to_string(&variant).unwrap();
            assert_eq!(s, expected, "JSON for {:?}", variant);
        }
    }

    // ── Cache + dedup behaviour ────────────────────────────────────

    /// Fresh cache entry is returned with `cached: true`.
    #[test]
    fn get_returns_cached_with_flag_set() {
        let scanner = DeltaEfficiencyScanner::new();
        let key = DeltaEfficiencyScanner::cache_key("bucket", 3);
        scanner.cache.write().insert(
            key,
            EfficiencyResponse {
                bucket: "bucket".into(),
                scanned_deltaspaces: 0,
                reported_deltaspaces: 0,
                min_deltas: 3,
                reports: vec![],
                computed_at: Utc::now(),
                cached: false, // stored as false
            },
        );
        let got = scanner.get("bucket", 3).expect("present");
        assert!(got.cached, "get() must mark response as cached");
    }

    /// Stale cache entries are ignored on read.
    #[test]
    fn get_returns_none_when_entry_is_stale() {
        let scanner = DeltaEfficiencyScanner::new();
        let key = DeltaEfficiencyScanner::cache_key("bucket", 3);
        scanner.cache.write().insert(
            key,
            EfficiencyResponse {
                bucket: "bucket".into(),
                scanned_deltaspaces: 0,
                reported_deltaspaces: 0,
                min_deltas: 3,
                reports: vec![],
                computed_at: Utc::now() - chrono::Duration::seconds(CACHE_TTL_SECS + 1),
                cached: false,
            },
        );
        assert!(
            scanner.get("bucket", 3).is_none(),
            "stale cache entry must be ignored"
        );
    }

    /// `is_scanning` reflects the dedup set.
    #[test]
    fn is_scanning_tracks_dedup() {
        let scanner = DeltaEfficiencyScanner::new();
        assert!(!scanner.is_scanning("bucket", 3));
        let key = DeltaEfficiencyScanner::cache_key("bucket", 3);
        scanner.scanning.write().insert(key);
        assert!(scanner.is_scanning("bucket", 3));
        // Different (bucket, min_deltas) combos are independent.
        assert!(!scanner.is_scanning("bucket", 5));
        assert!(!scanner.is_scanning("other", 3));
    }

    /// RAII guard removes its key on drop.
    #[test]
    fn scan_guard_clears_dedup_on_drop() {
        let scanner = Arc::new(DeltaEfficiencyScanner::new());
        let key = DeltaEfficiencyScanner::cache_key("bucket", 3);
        scanner.scanning.write().insert(key.clone());
        assert!(scanner.is_scanning("bucket", 3));
        {
            let _g = ScanInProgressGuard {
                scanner: scanner.clone(),
                key: key.clone(),
            };
            // Still in set during guard's lifetime.
            assert!(scanner.is_scanning("bucket", 3));
        }
        // Cleared on drop.
        assert!(!scanner.is_scanning("bucket", 3));
    }

    // ── build_report_for_prefix ────────────────────────────────────

    /// Build a Delta metadata fixture.
    /// `file_size` is what the listing/HEAD layer reports as the
    /// object's effective size. Under lite, this equals `delta_size`.
    /// Under the HEAD path it equals the original-file size.
    fn make_delta_meta(file_name: &str, delta_size: u64, file_size: u64) -> FileMetadata {
        let now = Utc::now();
        FileMetadata::fallback(
            file_name.to_string(),
            file_size,
            String::new(),
            now,
            None,
            StorageInfo::Delta {
                ref_path: "reference.bin".to_string(),
                ref_sha256: String::new(),
                delta_size,
                delta_cmd: String::new(),
            },
        )
    }

    fn make_ref_meta(size: u64) -> FileMetadata {
        let now = Utc::now();
        FileMetadata::fallback(
            "reference.bin".to_string(),
            size,
            String::new(),
            now,
            None,
            StorageInfo::Reference {
                source_name: String::new(),
            },
        )
    }

    fn make_passthrough_meta(file_name: &str, size: u64) -> FileMetadata {
        let now = Utc::now();
        FileMetadata::fallback(
            file_name.to_string(),
            size,
            String::new(),
            now,
            None,
            StorageInfo::Passthrough,
        )
    }

    /// Sanity: real-shaped Poor case — reference + 3 large deltas →
    /// report has Poor verdict and ratio_median around 1.49.
    /// Uses lite=true (file_size == delta_size on deltas).
    #[test]
    fn build_report_for_prefix_poor_with_ratio() {
        let scan = vec![
            make_ref_meta(61 * 1024 * 1024),
            make_delta_meta("a.delta", 91 * 1024 * 1024, 91 * 1024 * 1024),
            make_delta_meta("b.delta", 91 * 1024 * 1024, 91 * 1024 * 1024),
            make_delta_meta("c.delta", 91 * 1024 * 1024, 91 * 1024 * 1024),
        ];
        let report = build_report_for_prefix("bk", "prod/1.0".into(), &scan, 3, true)
            .expect("Poor verdict must produce a report");
        assert_eq!(report.efficiency, Efficiency::Poor);
        assert_eq!(report.deltas, 3);
        // ratio = median(91MB) / 61MB ≈ 1.49
        let ratio = report.ratio_median.expect("Poor must have ratio");
        assert!(
            (ratio - 1.49).abs() < 0.01,
            "expected ratio≈1.49, got {ratio}"
        );
    }

    /// Below `min_deltas` → no report (None).
    #[test]
    fn build_report_for_prefix_skips_below_min_deltas() {
        let scan = vec![
            make_ref_meta(1_000_000),
            make_delta_meta("a.delta", 500_000, 500_000),
        ];
        assert!(build_report_for_prefix("bk", "p".into(), &scan, 3, true).is_none());
    }

    /// Passthrough counts/bytes flow into the report. Under lite, the
    /// delta `file_size` is NOT added to `total_original_bytes` — only
    /// passthrough bytes are. `original_size_estimated` is set true.
    /// `savings_bytes` is `0` (sentinel for "not computable").
    #[test]
    fn build_report_for_prefix_counts_passthrough_under_lite() {
        let scan = vec![
            make_ref_meta(10_000_000),
            make_delta_meta("a.delta", 100_000, 100_000),
            make_delta_meta("b.delta", 100_000, 100_000),
            make_delta_meta("c.delta", 100_000, 100_000),
            make_passthrough_meta("video.mp4", 5_000_000),
            make_passthrough_meta("image.jpg", 1_500_000),
        ];
        let report = build_report_for_prefix("bk", "p".into(), &scan, 3, true).expect("present");
        assert_eq!(report.deltas, 3);
        assert_eq!(report.passthrough, 2);
        // Lite: only passthrough bytes contribute to total_original.
        assert_eq!(report.total_original_bytes, 5_000_000 + 1_500_000);
        assert_eq!(
            report.savings_bytes, 0,
            "lite mode must zero savings (unknown without HEAD)"
        );
        assert!(report.original_size_estimated);
    }

    /// Same input under non-lite (HEAD path) — delta `file_size` IS
    /// the true original size, so both pathways contribute to
    /// `total_original_bytes`, and `savings_bytes` reflects real
    /// compression.
    #[test]
    fn build_report_for_prefix_counts_originals_under_head_path() {
        // Here file_size on each delta = the original-file size that
        // HEAD would have recovered. Pretend each delta's original
        // was ~10 MB compressing to 100 KB.
        let scan = vec![
            make_ref_meta(10_000_000),
            make_delta_meta("a.delta", 100_000, 10_000_000),
            make_delta_meta("b.delta", 100_000, 10_000_000),
            make_delta_meta("c.delta", 100_000, 10_000_000),
            make_passthrough_meta("video.mp4", 5_000_000),
        ];
        let report = build_report_for_prefix("bk", "p".into(), &scan, 3, false).expect("present");
        // Originals: 3 deltas × 10MB + 1 passthrough × 5MB = 35MB
        assert_eq!(report.total_original_bytes, 35_000_000);
        // Stored = reference + sum(delta_size) + passthrough = 10MB + 3×100KB + 5MB = 15.3MB
        // (Passthrough bytes are stored as-is — they MUST be in stored_bytes;
        // pre-DRY-centralization fix, `build_report_for_prefix` silently
        // omitted them and `build_verify_response` correctly included them,
        // which is the divergence this consolidation closes.)
        // savings = 35MB - 15.3MB = 19.7MB
        assert_eq!(
            report.savings_bytes,
            35_000_000 - (10_000_000 + 300_000 + 5_000_000)
        );
        assert!(!report.original_size_estimated);
    }

    /// Healthy S3-style prefix under lite must NOT report negative
    /// savings (regression test: pre-fix `savings_bytes` came out
    /// negative on every Excellent prefix because the delta `file_size`
    /// was double-counted as "original").
    #[test]
    fn build_report_lite_excellent_prefix_does_not_show_negative_savings() {
        let scan = vec![
            make_ref_meta(19_500_000),
            make_delta_meta("a.delta", 70_000, 70_000),
            make_delta_meta("b.delta", 70_000, 70_000),
            make_delta_meta("c.delta", 70_000, 70_000),
        ];
        let report = build_report_for_prefix("bk", "p".into(), &scan, 3, true).expect("present");
        assert_eq!(report.efficiency, Efficiency::Excellent);
        // The whole point: lite must not surface a misleading negative
        // savings number to the UI.
        assert!(
            report.savings_bytes >= 0,
            "lite must never report negative savings on a healthy prefix \
             (got {})",
            report.savings_bytes
        );
        assert!(report.original_size_estimated);
    }

    /// `ratio_median` is None when there's no reference.
    #[test]
    fn build_report_for_prefix_no_reference_has_no_ratio() {
        let scan = vec![
            make_delta_meta("a.delta", 1_000, 1_000),
            make_delta_meta("b.delta", 2_000, 2_000),
            make_delta_meta("c.delta", 3_000, 3_000),
        ];
        let report = build_report_for_prefix("bk", "p".into(), &scan, 3, true).expect("present");
        assert_eq!(report.efficiency, Efficiency::NoReference);
        assert!(report.ratio_median.is_none());
    }

    /// `ratio_median` reflects the Excellent floor (0.36 %).
    #[test]
    fn build_report_for_prefix_excellent_ratio_is_small() {
        let scan = vec![
            make_ref_meta(19_500_000),
            make_delta_meta("a.delta", 70_000, 70_000),
            make_delta_meta("b.delta", 70_000, 70_000),
            make_delta_meta("c.delta", 70_000, 70_000),
        ];
        let report = build_report_for_prefix("bk", "p".into(), &scan, 3, true).expect("present");
        assert_eq!(report.efficiency, Efficiency::Excellent);
        let ratio = report.ratio_median.expect("ratio present");
        assert!(
            ratio < 0.01,
            "Excellent must yield small ratio, got {ratio}"
        );
    }

    /// Zero-byte reference + multiple deltas: classifier says
    /// NoReference, ratio is None, no panic on division-by-zero.
    #[test]
    fn build_report_for_prefix_zero_byte_reference_is_safe() {
        let scan = vec![
            make_ref_meta(0),
            make_delta_meta("a.delta", 100, 100),
            make_delta_meta("b.delta", 200, 200),
            make_delta_meta("c.delta", 300, 300),
        ];
        let report = build_report_for_prefix("bk", "p".into(), &scan, 3, true).expect("present");
        assert_eq!(report.efficiency, Efficiency::NoReference);
        assert!(report.ratio_median.is_none());
    }

    /// Two `Reference` entries in one prefix (latent corner case):
    /// classifier doesn't panic; the last-write-wins. Documented
    /// behavior — not expected in normal proxy operation but the code
    /// should be robust to malformed buckets.
    #[test]
    fn build_report_for_prefix_two_references_uses_last_write() {
        let scan = vec![
            make_ref_meta(1_000_000),
            make_ref_meta(5_000_000),
            make_delta_meta("a.delta", 100, 100),
            make_delta_meta("b.delta", 200, 200),
            make_delta_meta("c.delta", 300, 300),
        ];
        let report = build_report_for_prefix("bk", "p".into(), &scan, 3, true).expect("present");
        // Either reference size is acceptable — but the field must be
        // populated and ratio_median must be derived from the same.
        let r = report.reference_bytes.expect("reference present");
        assert!(r == 1_000_000 || r == 5_000_000);
        let ratio = report.ratio_median.expect("ratio present");
        assert!((ratio - (200.0 / r as f64)).abs() < 1e-9);
    }

    // ── sort_reports_worst_first ───────────────────────────────────

    fn dummy_report(prefix: &str, efficiency: Efficiency, total_delta: u64) -> DeltaspaceReport {
        DeltaspaceReport {
            bucket: "b".into(),
            prefix: prefix.into(),
            deltas: 3,
            passthrough: 0,
            reference_bytes: Some(100),
            total_delta_bytes: total_delta,
            total_original_bytes: 0,
            median_delta_bytes: 100,
            max_delta_bytes: 100,
            savings_bytes: 0,
            efficiency,
            ratio_median: Some(1.0),
            original_size_estimated: true,
            explanation: String::new(),
        }
    }

    /// Two prefixes that tie on (efficiency, total_delta_bytes) must
    /// produce a deterministic order across runs. Before the tertiary
    /// sort key on `prefix` was added, `buffer_unordered` completion
    /// order made the row order flip across reloads.
    #[test]
    fn sort_is_deterministic_for_tied_prefixes() {
        let mut reports = [
            dummy_report("z", Efficiency::Poor, 300),
            dummy_report("a", Efficiency::Poor, 300),
        ];
        sort_reports_worst_first(&mut reports);
        assert_eq!(reports[0].prefix, "a");
        assert_eq!(reports[1].prefix, "z");
    }

    /// Severity order: NoReference first, then Poor → Excellent.
    #[test]
    fn sort_puts_no_reference_before_poor_before_excellent() {
        let mut reports = [
            dummy_report("c", Efficiency::Excellent, 10),
            dummy_report("a", Efficiency::NoReference, 10),
            dummy_report("b", Efficiency::Poor, 10),
        ];
        sort_reports_worst_first(&mut reports);
        assert_eq!(reports[0].efficiency, Efficiency::NoReference);
        assert_eq!(reports[1].efficiency, Efficiency::Poor);
        assert_eq!(reports[2].efficiency, Efficiency::Excellent);
    }

    /// Within one verdict, larger waste rises.
    #[test]
    fn sort_within_verdict_orders_by_waste_desc() {
        let mut reports = [
            dummy_report("a", Efficiency::Poor, 100),
            dummy_report("b", Efficiency::Poor, 500),
            dummy_report("c", Efficiency::Poor, 300),
        ];
        sort_reports_worst_first(&mut reports);
        assert_eq!(reports[0].prefix, "b"); // 500
        assert_eq!(reports[1].prefix, "c"); // 300
        assert_eq!(reports[2].prefix, "a"); // 100
    }

    // ── build_verify_response (HEAD-path aggregator) ───────────────

    /// Healthy prefix: originals 10 MB each, deltas 100 KB each.
    /// True savings = (3 × 10 MB) − (10 MB ref + 3 × 100 KB) = 19.7 MB.
    /// compression_ratio ≈ (1 − 10.3MB / 30MB) ≈ 0.657.
    #[test]
    fn verify_response_computes_true_savings_on_healthy_prefix() {
        let scan = vec![
            make_ref_meta(10_000_000),
            make_delta_meta("a", 100_000, 10_000_000),
            make_delta_meta("b", 100_000, 10_000_000),
            make_delta_meta("c", 100_000, 10_000_000),
        ];
        let r = build_verify_response("bk", "p", &scan);
        assert_eq!(r.deltas, 3);
        assert_eq!(r.total_original_bytes, 30_000_000);
        // 10MB reference + 3 × 100KB deltas + 0 passthrough = 10.3MB
        assert_eq!(r.total_stored_bytes, 10_000_000 + 300_000);
        // 30MB − 10.3MB = 19.7MB
        assert_eq!(r.true_savings_bytes, 30_000_000 - (10_000_000 + 300_000));
        // 1 − 10.3/30 = 0.657
        let ratio = r.compression_ratio.expect("non-zero original");
        assert!(
            (ratio - 0.657).abs() < 0.005,
            "expected compression_ratio≈0.657, got {ratio}"
        );
        // Per-delta ratios all equal (100K/10M = 0.01).
        for d in &r.per_delta {
            assert!((d.ratio - 0.01).abs() < 1e-9);
        }
    }

    /// Pathological prefix: deltas are LARGER than originals
    /// (xdelta3 lost). True savings should be negative — that's the
    /// "you'd be better off without DG" signal.
    #[test]
    fn verify_response_signals_negative_savings_when_xdelta_lost() {
        // 5 originals @ 1 MB each → 5 MB total. Each delta is 1.5 MB.
        // Ref is 800 KB. Stored = 800K + 5×1.5M = 8.3 MB > 5 MB.
        let scan = vec![
            make_ref_meta(800_000),
            make_delta_meta("a", 1_500_000, 1_000_000),
            make_delta_meta("b", 1_500_000, 1_000_000),
            make_delta_meta("c", 1_500_000, 1_000_000),
            make_delta_meta("d", 1_500_000, 1_000_000),
            make_delta_meta("e", 1_500_000, 1_000_000),
        ];
        let r = build_verify_response("bk", "p", &scan);
        assert_eq!(r.total_original_bytes, 5_000_000);
        assert_eq!(r.total_stored_bytes, 800_000 + 5 * 1_500_000);
        assert!(
            r.true_savings_bytes < 0,
            "expected negative savings, got {}",
            r.true_savings_bytes
        );
        let ratio = r.compression_ratio.expect("present");
        assert!(
            ratio < 0.0,
            "expected negative compression_ratio, got {ratio}"
        );
    }

    /// Per-delta array is sorted ascending by ratio so the UI can
    /// pick percentiles from indices directly.
    #[test]
    fn verify_response_sorts_per_delta_by_ratio_ascending() {
        // Three deltas with widely different ratios:
        //   a: 100K / 10M = 0.01  (excellent)
        //   b: 5M  / 10M = 0.5   (poor)
        //   c: 500K / 10M = 0.05 (good)
        let scan = vec![
            make_ref_meta(10_000_000),
            make_delta_meta("a", 100_000, 10_000_000),
            make_delta_meta("b", 5_000_000, 10_000_000),
            make_delta_meta("c", 500_000, 10_000_000),
        ];
        let r = build_verify_response("bk", "p", &scan);
        assert_eq!(r.per_delta.len(), 3);
        assert_eq!(r.per_delta[0].key, "a"); // 0.01
        assert_eq!(r.per_delta[1].key, "c"); // 0.05
        assert_eq!(r.per_delta[2].key, "b"); // 0.5
    }

    /// Passthrough bytes count toward both `total_original_bytes`
    /// AND `total_stored_bytes` (they're stored as-is, no compression).
    #[test]
    fn verify_response_handles_passthrough_correctly() {
        let scan = vec![
            make_ref_meta(1_000_000),
            make_delta_meta("a", 50_000, 1_000_000),
            make_delta_meta("b", 50_000, 1_000_000),
            make_delta_meta("c", 50_000, 1_000_000),
            make_passthrough_meta("vid.mp4", 8_000_000),
        ];
        let r = build_verify_response("bk", "p", &scan);
        assert_eq!(r.deltas, 3);
        assert_eq!(r.passthrough_count, 1);
        // Originals: 3 × 1MB + 8MB = 11MB
        assert_eq!(r.total_original_bytes, 11_000_000);
        // Stored: 1MB ref + 3 × 50KB delta + 8MB passthrough = 9.15MB
        assert_eq!(r.total_stored_bytes, 1_000_000 + 3 * 50_000 + 8_000_000);
        assert!(r.true_savings_bytes > 0);
    }

    /// Zero-original edge: compression_ratio is None rather than a
    /// NaN / Infinity panic.
    #[test]
    fn verify_response_zero_originals_returns_none_ratio() {
        let scan = vec![make_ref_meta(1_000_000)];
        let r = build_verify_response("bk", "p", &scan);
        assert_eq!(r.total_original_bytes, 0);
        assert!(r.compression_ratio.is_none());
    }
}
