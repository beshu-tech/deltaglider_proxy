// SPDX-License-Identifier: GPL-3.0-only

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
    // O(1): read the per-bucket running counter instead of scanning. The 1000-
    // object cap (and its `truncated` best-effort contract) is gone — the
    // counter is maintained inline on every PUT/DELETE and reconciled by the
    // explicit Refresh endpoint. savings% is derived from the SAME logical-vs-
    // stored math the scan uses (see `SavingsTotals`), just pre-aggregated.
    let Some(usage) = state.bucket_usage.as_ref() else {
        // Open-mode dev with no usage DB: report zeros rather than fall back to
        // an unbounded scan. (Counters are the one size system now.)
        return Ok(StatsResponse {
            total_objects: 0,
            total_original_size: 0,
            total_stored_size: 0,
            savings_percentage: 0.0,
        });
    };

    let (object_count, logical, stored) = if let Some(bucket) = bucket_filter {
        match usage
            .read(bucket)
            .map_err(|e| S3Error::InternalError(format!("bucket usage read failed: {}", e)))?
        {
            Some(r) => (r.object_count, r.logical_bytes, r.stored_bytes),
            None => (0, 0, 0),
        }
    } else {
        let rows = usage
            .read_all()
            .map_err(|e| S3Error::InternalError(format!("bucket usage read failed: {}", e)))?;
        rows.iter().fold((0u64, 0u64, 0u64), |(c, l, s), (_b, r)| {
            (
                c + r.object_count,
                l.saturating_add(r.logical_bytes),
                s.saturating_add(r.stored_bytes),
            )
        })
    };

    Ok(StatsResponse {
        total_objects: object_count,
        total_original_size: logical,
        total_stored_size: stored,
        savings_percentage: crate::bucket_usage::savings_pct(logical, stored).unwrap_or(0.0),
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

/// Liveness probe — the process is up and the engine pointer is live. Fast, no
/// I/O (an LB liveness check must not block on a slow backend). Always 200 while
/// the process answers. For dependency health use the readiness probe below.
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
        backend: "live".to_string(),
        peak_rss_bytes: get_peak_rss_bytes(),
        cache_size_bytes,
        cache_max_bytes,
        cache_entries,
        cache_utilization_pct,
    })
}

#[derive(Debug, Serialize)]
pub struct ReadinessResponse {
    /// "ready" when every checked dependency is reachable, else "not_ready".
    pub status: &'static str,
    /// Storage backend reachability ("ready" | "unreachable" | "timeout").
    pub backend: &'static str,
    /// Config DB openability ("ready" | "locked" | "absent").
    pub config_db: &'static str,
}

/// Readiness probe — actually exercises dependencies, so an LB can pull a node
/// out of rotation when its backend is down (vs `/health` which only proves the
/// process answers). Returns 503 when not ready. Bounded by a short timeout so a
/// hung backend yields a fast "not ready" instead of blocking the probe.
/// GET /ready
pub async fn readiness_check(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<ReadinessResponse>) {
    use std::time::Duration;
    let engine = state.engine.load();
    // Cheapest real backend op: list buckets, capped so a dead/slow backend
    // fails the probe fast rather than hanging the LB health check.
    //
    // Both the per-attempt timeout and the retry count are tunable (#62): a
    // brief provider latency spike (e.g. Hetzner Object Storage tail latency)
    // should not immediately flip readiness to 503 and page. We retry the
    // list a few times with a short backoff and only report not-ready if EVERY
    // attempt fails — so one slow/failed call between good ones passes.
    let timeout_secs: u64 =
        crate::config::env_parse_with_default("DGP_READY_TIMEOUT_SECS", 3).max(1);
    let retries: u32 = crate::config::env_parse_with_default("DGP_READY_RETRIES", 2);
    let per_attempt = Duration::from_secs(timeout_secs);
    let mut backend = "timeout";
    for attempt in 0..=retries {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        match tokio::time::timeout(per_attempt, engine.list_buckets()).await {
            Ok(Ok(_)) => {
                backend = "ready";
                break;
            }
            Ok(Err(_)) => backend = "unreachable",
            Err(_) => backend = "timeout",
        }
    }
    // config DB: if configured, confirm we can take the lock (a poisoned/wedged
    // mutex would mean the control plane is stuck). `None` = legacy/open mode.
    let config_db = match &state.config_db {
        Some(db) => match db.try_lock() {
            Ok(_) => "ready",
            Err(_) => "locked",
        },
        None => "absent",
    };
    let ready = backend == "ready" && config_db != "locked";
    let code = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        code,
        Json(ReadinessResponse {
            status: if ready { "ready" } else { "not_ready" },
            backend,
            config_db,
        }),
    )
}
