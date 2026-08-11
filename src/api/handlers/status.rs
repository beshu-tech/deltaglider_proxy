// SPDX-License-Identifier: BUSL-1.1

//! Health-check and aggregate statistics handlers.

use super::{AppState, S3Error};
use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicI64, Ordering};
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
    /// Storage backend reachability. `ready` = the ListBuckets probe succeeded;
    /// `degraded` = the list is throttled but a cheap HEAD proved the backend is
    /// still reachable; `cached` = both probes failed but a backend call
    /// succeeded within the last-known-good window; `unreachable` / `timeout` =
    /// not ready.
    pub backend: &'static str,
    /// Config DB openability ("ready" | "locked" | "absent").
    pub config_db: &'static str,
}

/// Unix seconds of the last CONFIRMED-good backend interaction (0 = never).
/// Process-global, mirroring the codebase's other observable counters
/// (`IAM_VERSION`, `EXT_AUTH_VERSION`): readiness is a per-node property, so a
/// static avoids threading a field through `AppState` and every engine rebuild.
static LAST_BACKEND_OK_AT: AtomicI64 = AtomicI64::new(0);

/// Stamp "the backend answered just now". Called on any successful readiness
/// probe (list or the cheap HEAD fallback).
fn record_backend_ok(now: i64) {
    LAST_BACKEND_OK_AT.store(now, Ordering::Relaxed);
}

/// A bucket name used purely as a reachability probe. It does not need to
/// exist — `Ok(true)` and `Ok(false)` BOTH prove the backend answered; only an
/// `Err` means unreachable. DNS-safe (no underscore) so the SDK doesn't reject
/// it client-side before the round-trip. Creates nothing.
const READINESS_PROBE_BUCKET: &str = "dgp-readiness-probe";

/// Pure decision kernel for the backend half of readiness (#78).
///
/// `list_ok` — the ListBuckets probe succeeded.
/// `cheap_ok` — the fallback HEAD probe result (`None` = not attempted).
/// `cache_ttl` — last-known-good window in seconds; `0` disables the whole
/// resilient path, restoring the strict "list must succeed" contract.
///
/// The point of #78: a sustained provider LIST throttle should not flip a node
/// out of rotation while its data plane is demonstrably serving. So a cheap HEAD
/// that still answers means `degraded` (ready, list throttled), and only when
/// nothing answers do we fall back to the staleness window before reporting the
/// hard failure verdict.
pub fn resolve_backend_readiness(
    list_ok: bool,
    cheap_ok: Option<bool>,
    last_ok_at: i64,
    now: i64,
    cache_ttl: i64,
    hard_failure: &'static str,
) -> &'static str {
    if list_ok {
        return "ready";
    }
    if cache_ttl <= 0 {
        // Opt-out: exactly the pre-#78 behavior.
        return hard_failure;
    }
    if cheap_ok == Some(true) {
        return "degraded";
    }
    // Nothing answered right now — trust a recent confirmed-good interaction
    // rather than paging on a probe-only failure.
    if last_ok_at > 0 && now.saturating_sub(last_ok_at) <= cache_ttl {
        return "cached";
    }
    hard_failure
}

/// Is this backend verdict good enough to serve traffic?
fn backend_verdict_is_ready(verdict: &str) -> bool {
    matches!(verdict, "ready" | "degraded" | "cached")
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
    // #78: last-known-good window. 0 (default) keeps the strict pre-#78
    // contract — the ListBuckets probe must succeed or the node is not ready.
    let cache_ttl: i64 =
        crate::config::env_parse_with_default("DGP_READY_CACHE_TTL_SECS", 0i64).max(0);
    let per_attempt = Duration::from_secs(timeout_secs);
    let now = crate::event_outbox::current_unix_seconds();

    let mut list_ok = false;
    // The hard-failure verdict to report if nothing rescues the probe — carries
    // WHICH way the list failed (timeout vs error) for operator diagnosis.
    let mut hard_failure = "timeout";
    for attempt in 0..=retries {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        match tokio::time::timeout(per_attempt, engine.list_buckets()).await {
            Ok(Ok(_)) => {
                list_ok = true;
                break;
            }
            Ok(Err(_)) => hard_failure = "unreachable",
            Err(_) => hard_failure = "timeout",
        }
    }

    // The list is the throttle-prone call (#78: Hetzner throttles LIST while the
    // data plane keeps serving). Before declaring the node not-ready, try a much
    // cheaper HEAD — it proves reachability without listing. Either outcome of
    // the HEAD (bucket present or absent) means the backend ANSWERED; only an
    // error/timeout means it did not. Skipped entirely on the strict path so the
    // default contract costs no extra round-trip.
    let cheap_ok = if list_ok || cache_ttl <= 0 {
        None
    } else {
        match tokio::time::timeout(per_attempt, engine.head_bucket(READINESS_PROBE_BUCKET)).await {
            Ok(Ok(_)) => Some(true),
            _ => Some(false),
        }
    };

    let backend = resolve_backend_readiness(
        list_ok,
        cheap_ok,
        LAST_BACKEND_OK_AT.load(Ordering::Relaxed),
        now,
        cache_ttl,
        hard_failure,
    );
    // Stamp only on a probe that actually round-tripped to the backend, so the
    // window can never be refreshed by its own staleness ("cached" must decay).
    if list_ok || cheap_ok == Some(true) {
        record_backend_ok(now);
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
    let ready = backend_verdict_is_ready(backend) && config_db != "locked";
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

#[cfg(test)]
mod readiness_tests {
    use super::*;

    const TTL: i64 = 300;
    const NOW: i64 = 10_000;

    #[test]
    fn list_success_is_ready_regardless_of_everything_else() {
        // The happy path never consults the fallback or the window.
        assert_eq!(
            resolve_backend_readiness(true, None, 0, NOW, 0, "timeout"),
            "ready"
        );
        assert_eq!(
            resolve_backend_readiness(true, Some(false), 0, NOW, TTL, "unreachable"),
            "ready"
        );
    }

    #[test]
    fn strict_mode_preserves_pre_issue78_contract() {
        // cache_ttl = 0 (the DEFAULT): a failed list is not-ready, full stop —
        // even with a healthy cheap probe and a fresh last-known-good stamp.
        assert_eq!(
            resolve_backend_readiness(false, Some(true), NOW, NOW, 0, "timeout"),
            "timeout"
        );
        assert_eq!(
            resolve_backend_readiness(false, None, NOW, NOW, 0, "unreachable"),
            "unreachable"
        );
    }

    #[test]
    fn throttled_list_with_reachable_backend_is_degraded_not_down() {
        // THE #78 scenario: provider throttles LIST, data plane is fine. The
        // cheap HEAD still answers → keep serving.
        assert_eq!(
            resolve_backend_readiness(false, Some(true), 0, NOW, TTL, "timeout"),
            "degraded"
        );
        assert!(backend_verdict_is_ready("degraded"));
    }

    #[test]
    fn both_probes_down_falls_back_to_last_known_good_window() {
        // Nothing answers right now, but the backend answered 10s ago → ride it
        // out rather than paging on a probe-only failure.
        assert_eq!(
            resolve_backend_readiness(false, Some(false), NOW - 10, NOW, TTL, "timeout"),
            "cached"
        );
        assert!(backend_verdict_is_ready("cached"));
    }

    #[test]
    fn stale_window_reports_the_real_failure() {
        // Past the TTL the node IS genuinely not ready — the window must decay,
        // otherwise a truly dead backend would stay green forever.
        assert_eq!(
            resolve_backend_readiness(false, Some(false), NOW - TTL - 1, NOW, TTL, "unreachable"),
            "unreachable"
        );
        // Exact boundary is still inside the window (<=).
        assert_eq!(
            resolve_backend_readiness(false, Some(false), NOW - TTL, NOW, TTL, "unreachable"),
            "cached"
        );
    }

    #[test]
    fn never_stamped_backend_cannot_be_cached_ready() {
        // last_ok_at == 0 means "no backend call has EVER succeeded" — a node
        // that never reached its backend must not come up ready on boot.
        assert_eq!(
            resolve_backend_readiness(false, Some(false), 0, NOW, TTL, "timeout"),
            "timeout"
        );
    }

    #[test]
    fn only_serving_verdicts_are_ready() {
        for v in ["ready", "degraded", "cached"] {
            assert!(backend_verdict_is_ready(v), "{v} must serve");
        }
        for v in ["timeout", "unreachable"] {
            assert!(!backend_verdict_is_ready(v), "{v} must NOT serve");
        }
    }
}
