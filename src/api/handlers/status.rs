//! Health-check and aggregate statistics handlers.

use super::{AppState, S3Error};
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Instant;

/// Cached stats response — avoids re-scanning storage on every dashboard poll.
/// Uses tokio::sync::Mutex so the lock can be held across the async compute_stats()
/// call, preventing thundering herd (N concurrent requests all scanning storage).
static STATS_CACHE: std::sync::LazyLock<tokio::sync::Mutex<Option<(Instant, StatsResponse)>>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(None));

const STATS_CACHE_TTL_SECS: u64 = 10;

/// Query parameters for /stats endpoint
#[derive(Debug, Deserialize, Default)]
pub struct StatsQuery {
    pub bucket: Option<String>,
}

/// Aggregate storage statistics
#[derive(Debug, Clone, Serialize)]
pub struct StatsResponse {
    pub total_objects: u64,
    pub total_original_size: u64,
    pub total_stored_size: u64,
    pub savings_percentage: f64,
    /// True if the scan was truncated at the limit (more objects exist).
    pub truncated: bool,
}

/// Stats handler
/// GET /stats — aggregate stats across all buckets (cached for 10s)
/// GET /stats?bucket=NAME — stats for a specific bucket (uncached)
pub async fn get_stats(
    State(state): State<Arc<AppState>>,
    Query(query): Query<StatsQuery>,
) -> Result<Json<StatsResponse>, S3Error> {
    // Bucket-specific queries bypass the cache
    if query.bucket.is_some() {
        let result = compute_stats(&state, query.bucket.as_deref()).await?;
        return Ok(Json(result));
    }

    // Hold the lock across compute to prevent thundering herd:
    // only one request computes stats, others wait and get the cached result.
    let mut cache = STATS_CACHE.lock().await;
    if let Some((ts, cached)) = cache.as_ref() {
        if ts.elapsed().as_secs() < STATS_CACHE_TTL_SECS {
            return Ok(Json(cached.clone()));
        }
    }

    let result = compute_stats(&state, None).await?;
    *cache = Some((Instant::now(), result.clone()));

    Ok(Json(result))
}

async fn compute_stats(
    state: &AppState,
    bucket_filter: Option<&str>,
) -> Result<StatsResponse, S3Error> {
    let buckets_to_scan: Vec<String> = if let Some(bucket) = bucket_filter {
        vec![bucket.to_string()]
    } else {
        state
            .engine
            .load()
            .list_buckets()
            .await
            .unwrap_or_else(|e| {
                tracing::warn!("Stats: failed to list buckets: {}", e);
                Vec::new()
            })
    };

    const SCAN_LIMIT: u64 = 1000;

    let mut total_objects: u64 = 0;
    let mut total_original_size: u64 = 0;
    let mut total_stored_size: u64 = 0;
    let mut truncated = false;

    'outer: for bucket in &buckets_to_scan {
        let page = state
            .engine
            .load()
            .list_objects(bucket, "", None, SCAN_LIMIT as u32, None, true)
            .await?;
        for (_key, meta) in &page.objects {
            total_objects += 1;
            total_original_size += meta.file_size;
            total_stored_size += meta.delta_size().unwrap_or(meta.file_size);
            if total_objects >= SCAN_LIMIT {
                truncated = true;
                break 'outer;
            }
        }
    }

    let savings_percentage = if total_original_size > 0 {
        (1.0 - total_stored_size as f64 / total_original_size as f64) * 100.0
    } else {
        0.0
    };

    Ok(StatsResponse {
        total_objects,
        total_original_size,
        total_stored_size,
        savings_percentage,
        truncated,
    })
}

/// Health check response
#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub backend: String,
    pub peak_rss_bytes: u64,
    pub cache_size_bytes: u64,
    pub cache_max_bytes: u64,
    pub cache_entries: u64,
    pub cache_utilization_pct: f64,
}

/// Return the process-lifetime peak RSS (high-water mark) in bytes.
/// Uses `getrusage(RUSAGE_SELF)` which captures even microsecond-lived allocations.
pub(crate) fn get_peak_rss_bytes() -> u64 {
    // SAFETY: `libc::getrusage` is a POSIX syscall that writes into a caller-provided
    // `rusage` struct. We zero-initialise it first, and the call is infallible for
    // RUSAGE_SELF. No aliasing or lifetime issues — `usage` is a local stack variable.
    unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        if libc::getrusage(libc::RUSAGE_SELF, &mut usage) == 0 {
            let ru_maxrss = usage.ru_maxrss as u64;
            // macOS reports ru_maxrss in bytes; Linux reports in KB
            if cfg!(target_os = "macos") {
                ru_maxrss
            } else {
                ru_maxrss * 1024
            }
        } else {
            0
        }
    }
}

/// S3 root HEAD handler — connection probe used by Cyberduck and other S3 clients
/// HEAD /
pub async fn head_root() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header("x-amz-request-id", "0")
        .body(Body::empty())
        .unwrap()
}

/// Health check handler
/// GET /health
pub async fn health_check(State(state): State<Arc<AppState>>) -> Json<HealthResponse> {
    let engine = state.engine.load();
    let cache_size_bytes = engine.cache_weighted_size();
    let cache_max_bytes = engine.cache_max_capacity();
    let cache_entries = engine.cache_entry_count();
    let cache_utilization_pct = if cache_max_bytes > 0 {
        (cache_size_bytes as f64 / cache_max_bytes as f64) * 100.0
    } else {
        0.0
    };

    Json(HealthResponse {
        status: "healthy".to_string(),
        backend: "ready".to_string(),
        peak_rss_bytes: get_peak_rss_bytes(),
        cache_size_bytes,
        cache_max_bytes,
        cache_entries,
        cache_utilization_pct,
    })
}
