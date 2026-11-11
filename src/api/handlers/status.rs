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
    /// Config DB state ("ready" | "locked" = mutex held too long |
    /// "key_mismatch" = no key opens it, the S3 API answers 503 | "absent").
    pub config_db: &'static str,
    /// Spool directory ("ready" | "unusable"): can a temp file be made.
    pub spool: &'static str,
    /// Jobs DB openability ("ready" | "locked" | "absent"): the node-local
    /// job state and event outbox.
    pub jobs_db: &'static str,
    /// Live health of each configured backend (name → `healthy` |
    /// `unreachable` | `auth-rejected` | `erroring`), from the periodic
    /// health probe and from requests that found a backend unavailable.
    /// Empty when no probe ran (`DGP_BOOT_BACKEND_PROBE=off`).
    pub backends: std::collections::BTreeMap<String, String>,
    /// Multi-instance only: the encryption witness verdict (`agree`,
    /// `mismatch` with the buckets whose writes this node refuses, or
    /// `unknown`). A mismatch leaves the node READY: it refuses writes to
    /// those buckets only, and still serves their reads and every other
    /// bucket; a load balancer that removed it would drop all of that.
    /// Public: the status only (`{"status": ...}`); bucket names, key ids
    /// and errors are behind the admin session
    /// (`GET /_/api/admin/encryption-witness`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encryption_witness: Option<WitnessReadiness>,
    /// The atomic-writes state of each backend that stores buckets which
    /// encrypt with the proxy (`checking`: no verdict yet; `unverified`:
    /// the check could not reach it; both answer writes to those buckets
    /// with 503, retried. `supported`; `unsupported`: read-only buckets).
    /// Informational: it never fails readiness, because reads and every
    /// other bucket work, and the pending state ends on its own.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub atomic_writes: std::collections::BTreeMap<String, &'static str>,
}

/// Pure: the readiness word of an atomic-writes verdict.
pub fn atomic_writes_word(
    verdict: Option<&crate::coordination::CapabilityVerdict>,
) -> &'static str {
    use crate::coordination::CapabilityVerdict;
    match verdict {
        None => "checking",
        Some(CapabilityVerdict::Unknown { .. }) => "unverified",
        Some(CapabilityVerdict::CasVerified { .. }) => "supported",
        Some(CapabilityVerdict::NonCas) => "unsupported",
    }
}

/// The public part of the encryption witness verdict.
#[derive(Debug, Serialize)]
pub struct WitnessReadiness {
    pub status: &'static str,
}

/// Pure: the node is not ready when every backend with a verdict is gated
/// (no bucket can be served). One gated backend of several leaves the node
/// ready: the other backends' buckets still work, and every node sees the
/// same outage, so pulling nodes from rotation would not help.
fn live_backends_ready(verdicts: &[crate::coordination::HealthVerdict]) -> bool {
    verdicts.is_empty() || verdicts.iter().any(|v| !v.is_gating())
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

/// How long `/_/ready` waits for the config DB mutex before it reports the
/// DB `locked`. Holders keep it for milliseconds; a wedged one for ever.
const CONFIG_DB_READY_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

/// Is this backend verdict good enough to serve traffic?
fn backend_verdict_is_ready(verdict: &str) -> bool {
    matches!(verdict, "ready" | "degraded" | "cached")
}

/// Pure: the backend half of readiness from fresh health-loop verdicts.
/// A healthy backend makes it `ready`; one that answers with errors
/// (`Erroring`: 5xx, persistent throttle) `degraded`, which serves, as the
/// request gate keeps its buckets open; only all-gating is `unreachable`.
fn readiness_from_health_loop(verdicts: &[crate::coordination::HealthVerdict]) -> &'static str {
    use crate::coordination::HealthVerdict as V;
    if verdicts.is_empty() || verdicts.iter().any(V::is_healthy) {
        "ready"
    } else if verdicts.iter().any(|v| !v.is_gating()) {
        "degraded"
    } else {
        "unreachable"
    }
}

/// The live backend probe of `/_/ready`: ListBuckets with retries, then
/// the cheap HEAD and the last-known-good window (#78). Bounded by short
/// timeouts so a hung backend yields a fast "not ready".
async fn live_backend_readiness(engine: &crate::deltaglider::DynEngine) -> &'static str {
    use std::time::Duration;
    // Cheapest real backend op: list buckets, capped so a dead/slow backend
    // fails the probe fast rather than hanging the LB health check.
    //
    // Both the per-attempt timeout and the retry count are tunable (#62): a
    // brief provider latency spike (e.g. Hetzner Object Storage tail latency)
    // should not immediately flip readiness to 503 and page. We retry the
    // list a few times with a short backoff and only report not-ready if EVERY
    // attempt fails — so one slow/failed call between good ones passes.
    // `DGP_READY_TIMEOUT_SECS` / `DGP_READY_RETRIES` / `DGP_READY_CACHE_TTL_SECS`.
    // #78: last-known-good window. 0 (default) keeps the strict pre-#78
    // contract — the ListBuckets probe must succeed or the node is not ready.
    let crate::config::tuning::ReadyProbe {
        timeout_secs,
        retries,
        cache_ttl_secs: cache_ttl,
    } = engine.tuning().ready;
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
    backend
}

/// What `/_/ready` knows about this node, as plain values. The verdict is
/// [`readiness_verdict`], a pure function over this snapshot.
///
/// PARITY RULE: every condition where the S3 API refuses ALL requests
/// (a global 503) is a row here that makes the node not ready, so a load
/// balancer never keeps a node that answers 503 to everything. The source
/// test `every_global_s3_refusal_has_a_readiness_row` lists those
/// conditions and fails when a new one appears without a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeConditions {
    /// The storage backends answer (see `backend_verdict_is_ready`).
    pub backends_ready: bool,
    /// No config-DB key opens the config DB. The S3 API answers 503
    /// "Config database locked" to every request.
    pub config_db_key_mismatch: bool,
    /// The config DB mutex is held past the wait.
    pub config_db_wedged: bool,
    /// The jobs DB mutex is held past the wait.
    pub jobs_db_wedged: bool,
    /// The spool directory takes a new file (after one re-create).
    pub spool_usable: bool,
}

/// A row's name and the edit that makes it fail.
#[cfg(test)]
type FailingRow = (&'static str, fn(&mut NodeConditions));

impl NodeConditions {
    /// Every row of the table, for the exhaustive unit test: each field
    /// as the one condition that fails.
    #[cfg(test)]
    const FAILING_ROWS: [FailingRow; 5] = [
        ("backends", |c| c.backends_ready = false),
        ("config_db_key_mismatch", |c| {
            c.config_db_key_mismatch = true
        }),
        ("config_db_wedged", |c| c.config_db_wedged = true),
        ("jobs_db_wedged", |c| c.jobs_db_wedged = true),
        ("spool", |c| c.spool_usable = false),
    ];
}

/// THE readiness decision: ready only when no row fails.
pub fn readiness_verdict(c: &NodeConditions) -> bool {
    c.backends_ready
        && !c.config_db_key_mismatch
        && !c.config_db_wedged
        && !c.jobs_db_wedged
        && c.spool_usable
}

/// The `config_db` word of the response. A key mismatch is named apart
/// from a wedged mutex: the fix differs (restore the key vs. restart).
fn config_db_word(mutex_word: &'static str, key_mismatch: bool) -> &'static str {
    if key_mismatch {
        "key_mismatch"
    } else {
        mutex_word
    }
}

/// How long `/_/ready` waits for the spool check before it calls the spool
/// unusable.
const SPOOL_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// `ready` when the lock is free within [`CONFIG_DB_READY_WAIT`], else `locked`.
async fn lock_readiness<T>(db: &tokio::sync::Mutex<T>) -> &'static str {
    match tokio::time::timeout(CONFIG_DB_READY_WAIT, db.lock()).await {
        Ok(_) => "ready",
        Err(_) => "locked",
    }
}

/// Readiness probe — actually exercises dependencies, so an LB can pull a node
/// out of rotation when its backend is down (vs `/health` which only proves the
/// process answers). Returns 503 when not ready.
///
/// The backend half costs no upstream request while the health loop runs
/// and its verdicts are younger than 2 x its interval: it answers from them.
/// Otherwise (loop off, boot before the first round, a hung round) it
/// probes live, through a single flight whose answer serves every probe for
/// [`crate::coordination::health::READY_PROBE_TTL`]. The config-DB half is
/// always live.
/// GET /ready
pub async fn readiness_check(
    State(state): State<Arc<AppState>>,
) -> (StatusCode, Json<ReadinessResponse>) {
    let backend = match state.backend_health.loop_verdicts() {
        Some(verdicts) => readiness_from_health_loop(&verdicts),
        None => {
            let engine = state.engine.load_full();
            state
                .backend_health
                .ready_probe(live_backend_readiness(&engine))
                .await
        }
    };
    // The config DB and the jobs DB: if configured, confirm we can take the
    // lock (a wedged mutex means the control plane is stuck). Requests do
    // not take the jobs DB lock (they queue their outbox events), but the
    // outbox writer task and the job workers hold it for a moment at a
    // time, so the probe waits briefly: a busy mutex is ready, only one
    // held past the wait is not. `None` = legacy/open mode.
    let config_db_mutex = match &state.config_db {
        Some(db) => lock_readiness(db).await,
        None => "absent",
    };
    let config_db = config_db_word(config_db_mutex, state.config_db_key_mismatch);
    let jobs_db = match &state.jobs_db {
        Some(db) => lock_readiness(db).await,
        None => "absent",
    };
    let live = state.backend_health.snapshot();
    let verdicts: Vec<_> = live.values().map(|e| e.verdict.clone()).collect();
    let backends = live
        .into_iter()
        .map(|(name, e)| {
            let status = serde_json::to_value(&e.verdict)
                .ok()
                .and_then(|v| v["status"].as_str().map(str::to_string))
                .unwrap_or_default();
            (name, status)
        })
        .collect();
    let backends_ok = live_backends_ready(&verdicts);
    let backend = if backends_ok { backend } else { "unreachable" };
    // The spool check makes and drops one temp file (and re-creates a
    // removed directory): off the async threads, single-flight, cached for
    // a few seconds and bounded (`BackendHealthCache::spool_probe`).
    let spool_usable = {
        let engine = state.engine.load_full();
        state
            .backend_health
            .spool_probe(move || engine.spool_usable(), SPOOL_READY_TIMEOUT)
            .await
    };
    let ready = readiness_verdict(&NodeConditions {
        backends_ready: backend_verdict_is_ready(backend),
        config_db_key_mismatch: state.config_db_key_mismatch,
        config_db_wedged: config_db_mutex == "locked",
        jobs_db_wedged: jobs_db == "locked",
        spool_usable,
    });
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
            jobs_db,
            spool: if spool_usable { "ready" } else { "unusable" },
            backends,
            encryption_witness: state.encryption_witness.as_ref().map(|w| WitnessReadiness {
                status: w.status().status,
            }),
            atomic_writes: state
                .backend_capabilities
                .encrypted_plan()
                .backends()
                .into_iter()
                .map(|(name, def)| {
                    let word =
                        atomic_writes_word(state.backend_capabilities.get(&name, &def).as_ref());
                    (name, word)
                })
                .collect(),
        }),
    )
}

#[cfg(test)]
mod readiness_tests {
    use super::*;
    use crate::coordination::HealthVerdict;
    use std::time::Duration;

    /// An app state on a fake S3, and the default backend's definition.
    async fn s3_state() -> (
        Arc<AppState>,
        Arc<crate::storage::FakeS3>,
        crate::config::BackendConfig,
    ) {
        let (engine, fake) = crate::deltaglider::s3_engine().await;
        fake.clear();
        let backend = crate::config::Config::default().backend;
        (AppState::for_tests(engine), fake, backend)
    }

    async fn ready(state: &Arc<AppState>) -> (StatusCode, &'static str) {
        let (code, Json(body)) = readiness_check(State(state.clone())).await;
        (code, body.backend)
    }

    const EVERY: Duration = Duration::from_secs(30);

    fn all_ok() -> NodeConditions {
        NodeConditions {
            backends_ready: true,
            config_db_key_mismatch: false,
            config_db_wedged: false,
            jobs_db_wedged: false,
            spool_usable: true,
        }
    }

    /// The full table: ready with no failing row, not ready with any one
    /// failing, and not ready for every combination of two.
    #[test]
    fn the_verdict_follows_the_table() {
        assert!(readiness_verdict(&all_ok()));
        for (name, fail) in NodeConditions::FAILING_ROWS {
            let mut c = all_ok();
            fail(&mut c);
            assert!(
                !readiness_verdict(&c),
                "row {name} must make the node not ready"
            );
        }
        for (a, fa) in NodeConditions::FAILING_ROWS {
            for (b, fb) in NodeConditions::FAILING_ROWS {
                let mut c = all_ok();
                fa(&mut c);
                fb(&mut c);
                assert!(!readiness_verdict(&c), "rows {a}+{b}");
            }
        }
    }

    #[test]
    fn a_key_mismatch_is_named_apart_from_a_wedged_mutex() {
        assert_eq!(config_db_word("ready", true), "key_mismatch");
        assert_eq!(config_db_word("locked", true), "key_mismatch");
        assert_eq!(config_db_word("locked", false), "locked");
        assert_eq!(config_db_word("absent", false), "absent");
    }

    /// A state that the S3 API refuses entirely is not ready, through the
    /// real handler.
    #[tokio::test]
    async fn a_config_db_key_mismatch_is_not_ready() {
        let (engine, _fake) = crate::deltaglider::s3_engine().await;
        let mut state = AppState::for_tests(engine);
        Arc::get_mut(&mut state).unwrap().config_db_key_mismatch = true;
        state.backend_health.set_loop_interval(EVERY);
        let (code, Json(body)) = readiness_check(State(state)).await;
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body.config_db, "key_mismatch");
        assert_eq!(body.status, "not_ready");
    }

    /// How far a 503 refusal reaches.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Reach {
        /// Refuses one request or one client (a cap, a conflict, a rate
        /// limit, a failed backend call): never a readiness row.
        PerRequest,
        /// Refuses the requests of one bucket or one backend: the node
        /// still serves the rest, so not a readiness row.
        PerBucket,
        /// Refuses EVERY request of the S3 path: the node is not ready, so
        /// the row names the `NodeConditions` field that covers it.
        Global(&'static str),
        /// Defines, names or translates the 503 type; builds no refusal of
        /// its own (the sites that cause it are rows of their own).
        Plumbing,
    }

    /// CLASS GUARD. Every `SlowDown` / `ServiceUnavailable` /
    /// `SERVICE_UNAVAILABLE`, and every `StorageError` / `EngineError`
    /// variant that the bridges answer with a 503 (read from the bridges),
    /// in production code of `src/` outside `src/storage` has a row
    /// below: a file, a needle (`=` starts a whole-line match), how many
    /// lines match it, how far it reaches, and why. A site that no row
    /// covers fails, whatever its shape, until someone classifies it. A
    /// `Global` row must name a `NodeConditions` field that `/_/ready`
    /// reads: where the S3 path refuses every request, the node is not
    /// ready.
    ///
    /// The coordination writers that refuse until the bucket is proven
    /// (S3Lease, S3ReferenceLock, witness writes) refuse per request: a delta
    /// PUT meets the reference lock and gets `503 ServiceUnavailable`, a
    /// write to a witnessed bucket gets `503 SlowDown` (`WritesRefused`).
    /// Both reach clients through the `errors.rs` mapping below.
    const REFUSALS: &[(&str, &str, usize, Reach, &str)] = &[
        (
            "src/api/errors.rs",
            "429 | 503 => \"SlowDown\"",
            1,
            Reach::Plumbing,
            "the code of a rejected status",
        ),
        (
            "src/api/errors.rs",
            "if status == StatusCode::SERVICE_UNAVAILABLE",
            1,
            Reach::Plumbing,
            "adds Retry-After to a 503",
        ),
        (
            "src/api/errors.rs",
            "=#[error(\"SlowDown: Please reduce your request rate.\")]",
            1,
            Reach::Plumbing,
            "the S3Error type",
        ),
        (
            "src/api/errors.rs",
            "=SlowDown(String),",
            1,
            Reach::Plumbing,
            "the S3Error type",
        ),
        (
            "src/api/errors.rs",
            "=#[error(\"ServiceUnavailable: {0}\")]",
            1,
            Reach::Plumbing,
            "the S3Error type",
        ),
        (
            "src/api/errors.rs",
            "=ServiceUnavailable(String),",
            1,
            Reach::Plumbing,
            "the S3Error type",
        ),
        (
            "src/api/errors.rs",
            "S3Error::SlowDown(_) => \"SlowDown\"",
            1,
            Reach::Plumbing,
            "the S3 code of the type",
        ),
        (
            "src/api/errors.rs",
            "S3Error::ServiceUnavailable(_) => \"ServiceUnavailable\"",
            1,
            Reach::Plumbing,
            "the S3 code of the type",
        ),
        (
            "src/api/errors.rs",
            "S3Error::SlowDown(_) => StatusCode::SERVICE_UNAVAILABLE",
            1,
            Reach::Plumbing,
            "the HTTP status of the type",
        ),
        (
            "src/api/errors.rs",
            "S3Error::ServiceUnavailable(_) => StatusCode::SERVICE_UNAVAILABLE",
            1,
            Reach::Plumbing,
            "the HTTP status of the type",
        ),
        (
            "src/api/errors.rs",
            "S3Error::SlowDown(_) | S3Error::ServiceUnavailable(_)",
            1,
            Reach::Plumbing,
            "is_retryable of the type",
        ),
        (
            "src/api/errors.rs",
            "S3Error::SlowDown(msg) => s3s::s3_error!",
            1,
            Reach::Plumbing,
            "the s3s bridge",
        ),
        (
            "src/api/errors.rs",
            "S3Error::ServiceUnavailable(msg) => s3s::s3_error!",
            1,
            Reach::Plumbing,
            "the s3s bridge",
        ),
        (
            "src/api/errors.rs",
            "S3Error::ServiceUnavailable(DISK_FULL_503",
            1,
            Reach::PerBucket,
            "the backend that hosts the bucket is full",
        ),
        (
            "src/api/errors.rs",
            "StorageError::WritesRefused(msg) => S3Error::SlowDown(msg)",
            1,
            Reach::PerRequest,
            "the witness or a coordination writer refuses this write",
        ),
        (
            "src/api/errors.rs",
            "StorageError::Throttled(_) => S3Error::SlowDown(",
            1,
            Reach::PerRequest,
            "a backend throttled this request",
        ),
        (
            "src/api/errors.rs",
            "S3Error::ServiceUnavailable(format!(\"the storage backend did not answer",
            1,
            Reach::PerBucket,
            "one backend did not answer",
        ),
        (
            "src/api/errors.rs",
            "S3Error::ServiceUnavailable(TRANSIENT_503",
            2,
            Reach::PerRequest,
            "a transient fault in this request",
        ),
        (
            "src/api/errors.rs",
            "=S3Error::SlowDown(",
            1,
            Reach::PerRequest,
            "another writer holds this object",
        ),
        (
            "src/api/errors.rs",
            "S3Error::ServiceUnavailable(QUOTA_503",
            1,
            Reach::PerBucket,
            "the backend quota is used up",
        ),
        (
            "src/api/errors.rs",
            "=S3Error::ServiceUnavailable(",
            1,
            Reach::PerRequest,
            "the codec timed out on this request",
        ),
        (
            "src/api/s3_router.rs",
            "a request to it got 503 SlowDown",
            1,
            Reach::PerBucket,
            "log line: one backend holds its share of slots",
        ),
        (
            "src/api/s3_router.rs",
            "crate::api::S3Error::SlowDown(",
            1,
            Reach::PerBucket,
            "one backend holds its share of the request slots",
        ),
        (
            "src/api/auth.rs",
            "S3Error::SlowDown(format!(",
            1,
            Reach::PerRequest,
            "auth lockout of one (client IP, access key)",
        ),
        (
            "src/api/auth.rs",
            "crate::api::S3Error::ServiceUnavailable(",
            1,
            Reach::Global("config_db_key_mismatch"),
            "no key opens the config database: every request refused",
        ),
        (
            "src/api/admin/external_auth.rs",
            "StatusCode::SERVICE_UNAVAILABLE",
            1,
            Reach::PerRequest,
            "one provider is not discovered yet",
        ),
        (
            "src/api/admin/objects.rs",
            "StatusCode::SERVICE_UNAVAILABLE",
            1,
            Reach::PerRequest,
            "a ZIP request met an overloaded backend",
        ),
        (
            "src/api/admin/error.rs",
            "StatusCode::SERVICE_UNAVAILABLE => \"unavailable\"",
            1,
            Reach::Plumbing,
            "the error code of a 503",
        ),
        (
            "src/api/admin/error.rs",
            "Self::status(StatusCode::SERVICE_UNAVAILABLE, message)",
            1,
            Reach::PerRequest,
            "an admin request cannot be served now",
        ),
        (
            "src/api/admin/config/conversion_start.rs",
            "status: StatusCode::SERVICE_UNAVAILABLE",
            2,
            Reach::PerRequest,
            "an apply cannot read the jobs DB",
        ),
        (
            "src/s3_adapter_s3s/object.rs",
            "=SlowDown,",
            1,
            Reach::PerRequest,
            "the object changed while this ranged read ran",
        ),
        (
            "src/coordination/health.rs",
            "S3Error::ServiceUnavailable(format!(",
            1,
            Reach::PerBucket,
            "BackendHealthGate: the bucket's backend is gated",
        ),
        (
            "src/api/handlers/status.rs",
            "=StatusCode::SERVICE_UNAVAILABLE",
            1,
            Reach::Plumbing,
            "the /_/ready answer itself",
        ),
        (
            "src/maintenance/gate.rs",
            "S3Error::SlowDown(refusal_message",
            1,
            Reach::PerBucket,
            "a job holds this bucket's write gate",
        ),
        (
            "src/api/admin/objects.rs",
            "EngineError::Overloaded(_) | EngineError::Storage(StorageError::Throttled(_))",
            1,
            Reach::PerRequest,
            "the admin ZIP maps an overloaded engine to its busy answer",
        ),
        (
            "src/api/errors.rs",
            "crate::storage::StorageError::DiskFull => disk_full_503(",
            1,
            Reach::Plumbing,
            "the bridge: a full disk",
        ),
        (
            "src/api/errors.rs",
            "crate::storage::StorageError::Unavailable(msg) => {",
            1,
            Reach::Plumbing,
            "the bridge: a backend that did not answer",
        ),
        (
            "src/api/errors.rs",
            "crate::storage::StorageError::Transient(cause) => {",
            1,
            Reach::Plumbing,
            "the bridge: a retryable backend fault",
        ),
        (
            "src/api/errors.rs",
            "=crate::storage::StorageError::Contended(msg)",
            1,
            Reach::Plumbing,
            "the bridge: a writer holds the resource",
        ),
        (
            "src/api/errors.rs",
            "=| crate::storage::StorageError::PreconditionFailed(msg) => {",
            1,
            Reach::Plumbing,
            "the bridge: the source changed",
        ),
        (
            "src/api/errors.rs",
            "crate::storage::StorageError::QuotaExceeded(cause) => {",
            1,
            Reach::Plumbing,
            "the bridge: a backend quota",
        ),
        (
            "src/api/errors.rs",
            "crate::storage::StorageError::NoSuchUpload(cause) => {",
            1,
            Reach::Plumbing,
            "the bridge: a proxy-owned upload vanished",
        ),
        (
            "src/api/handlers/object_helpers.rs",
            "EngineError::Storage(StorageError::PreconditionFailed(_))",
            1,
            Reach::PerRequest,
            "the copy source changed during this copy",
        ),
        (
            "src/coordination/atomicity.rs",
            "Self::Pending { .. } => crate::api::errors::S3Error::SlowDown(message)",
            1,
            Reach::PerBucket,
            "the atomic-write verdict of one proxy-encrypted bucket is pending",
        ),
        (
            "src/deltaglider/engine/locking.rs",
            "EngineError::Storage(StorageError::Unavailable(detail))",
            1,
            Reach::PerRequest,
            "the cross-instance reference lock cannot be reached for this deltaspace",
        ),
        (
            "src/deltaglider/engine/locking.rs",
            "EngineError::Storage(StorageError::Contended(format!(",
            1,
            Reach::PerRequest,
            "a peer holds this deltaspace past the wait",
        ),
        (
            "src/deltaglider/engine/mod.rs",
            "EngineError::Overloaded(msg) => crate::api::S3Error::SlowDown(msg)",
            1,
            Reach::Plumbing,
            "the engine bridge",
        ),
        (
            "src/deltaglider/engine/resources.rs",
            "EngineError::Overloaded(",
            5,
            Reach::PerRequest,
            "a codec slot or the spool budget is full for this request",
        ),
        (
            "src/deltaglider/engine/retrieve.rs",
            "EngineError::Storage(StorageError::PreconditionFailed(",
            1,
            Reach::PerRequest,
            "the object changed during this read",
        ),
        (
            "src/deltaglider/engine/retrieve.rs",
            "EngineError::Storage(StorageError::Throttled(format!(",
            1,
            Reach::PerRequest,
            "a shared reconstruction was throttled",
        ),
        (
            "src/multipart.rs",
            "S3Error::SlowDown(",
            2,
            Reach::PerRequest,
            "a multipart request met a cap or a full spool budget",
        ),
        (
            "src/api/errors.rs",
            "S3Error::SlowDown(\"the copy source changed during the copy; retry the request\".to_string())",
            1,
            Reach::PerRequest,
            "the copy source changed during a copy (the one answer, `copy_source_changed`)",
        ),
        (
            "src/deltaglider/engine/metadata.rs",
            "return Err(StorageError::Throttled(format!(",
            1,
            Reach::PerRequest,
            "a stored object changed while it was read",
        ),
        (
            "src/deltaglider/engine/store.rs",
            "Err(EngineError::Overloaded(why)) => {",
            1,
            Reach::PerRequest,
            "no spool for a delta: handled, the object is stored whole, no 503 is sent",
        ),
        (
            "src/deltaglider/ingest.rs",
            "return Err(IngestError::Engine(EngineError::Overloaded(",
            1,
            Reach::PerRequest,
            "the spool budget cannot hold this body now",
        ),
    ];

    /// The variants of `prefix` (a path such as `crate::storage::StorageError::`)
    /// that the exhaustive bridge `impl_header` answers with a 503 (a
    /// `SlowDown`, a `ServiceUnavailable`, or a `*_503` helper). Read from
    /// the source of the bridge, so a new 503 variant joins the scan with no
    /// edit here. A variable-carrying `Io` arm that is a 503 only for a full
    /// disk is left out: every file write raises it.
    fn variants_answered_503(file: &str, impl_header: &str, prefix: &str) -> Vec<String> {
        let text = crate::source_scan::prod_text(&crate::source_scan::read(file));
        let body = text
            .split(impl_header)
            .nth(1)
            .unwrap_or_else(|| panic!("{file}: {impl_header}"));
        let body = body
            .split(format!("\n{}\n", '\u{7d}').as_str())
            .next()
            .unwrap();
        // (variants of the arm, text of the arm)
        let mut arms: Vec<(Vec<String>, String)> = Vec::new();
        for line in body.lines() {
            let head = line
                .trim()
                .trim_start_matches("e @ (")
                .trim_start_matches("| ");
            if let Some(rest) = head.strip_prefix(prefix) {
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                match arms.last_mut() {
                    // The arm's head is not finished (no `=>` yet): an or-pattern.
                    Some((names, text)) if !text.contains("=>") => names.push(name),
                    _ => arms.push((vec![name], String::new())),
                }
            }
            if let Some((_, text)) = arms.last_mut() {
                text.push_str(line);
                text.push('\n');
            }
        }
        let is_503 = regex_lite::Regex::new(r"SlowDown|ServiceUnavailable|_503").unwrap();
        let mut out: Vec<String> = arms
            .into_iter()
            .filter(|(_, text)| is_503.is_match(text))
            .flat_map(|(names, _)| names)
            .filter(|n| n != "Io")
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// The source words of a 503 refusal: the S3 types, and every
    /// `StorageError` / `EngineError` variant that the bridges turn into one
    /// (`S3Error::from(StorageError::Throttled(..))` refuses as much as
    /// `S3Error::SlowDown(..)` does).
    fn refusal_words() -> regex_lite::Regex {
        let storage = variants_answered_503(
            "src/api/errors.rs",
            "impl From<crate::storage::StorageError> for S3Error {",
            "crate::storage::StorageError::",
        );
        let engine = variants_answered_503(
            "src/deltaglider/engine/mod.rs",
            "impl From<EngineError> for crate::api::S3Error {",
            "EngineError::",
        );
        for known in [
            "Throttled",
            "WritesRefused",
            "Unavailable",
            "Transient",
            "Contended",
        ] {
            assert!(
                storage.iter().any(|v| v == known),
                "the bridge read lost {known}"
            );
        }
        assert!(engine.iter().any(|v| v == "Overloaded"));
        regex_lite::Regex::new(&format!(
            r"\b(SlowDown|ServiceUnavailable|SERVICE_UNAVAILABLE|StorageError::({})|EngineError::({}))\b",
            storage.join("|"),
            engine.join("|")
        ))
        .unwrap()
    }

    /// Production code that cannot refuse a client request: the backends
    /// RAISE these errors on every fault (the bridges decide the answer),
    /// the config registry holds help text, and the replication workers run
    /// in the background (they log a throttled backend and stop a run).
    const NOT_SCANNED: &[&str] = &["src/storage/", "src/config/registry.rs", "src/replication/"];

    /// `(file, trimmed line)` of every site in the production code of
    /// `src/` (minus [`NOT_SCANNED`] and test modules).
    fn refusal_sites() -> Vec<(String, String)> {
        use crate::source_scan::{prod_lines, prod_sources};
        let word = refusal_words();
        let mut sites = Vec::new();
        for (file, text) in prod_sources("src") {
            if NOT_SCANNED.iter().any(|p| file.starts_with(p)) {
                continue;
            }
            for (_, line) in prod_lines(&text) {
                let line = line.trim();
                if !line.starts_with("//") && word.is_match(line) {
                    sites.push((file.clone(), line.to_string()));
                }
            }
        }
        sites
    }

    fn row_covers(row_needle: &str, line: &str) -> bool {
        match row_needle.strip_prefix('=') {
            Some(whole) => line == whole,
            None => line.contains(row_needle),
        }
    }

    /// Sites with no row, and rows whose count is wrong.
    fn refusal_problems(sites: &[(String, String)]) -> Vec<String> {
        let mut problems = Vec::new();
        for (file, line) in sites {
            let n = REFUSALS
                .iter()
                .filter(|(f, needle, ..)| f == file && row_covers(needle, line))
                .count();
            if n != 1 {
                problems.push(format!(
                    "{file}: `{line}` is covered by {n} rows (want 1): classify it in REFUSALS"
                ));
            }
        }
        for (file, needle, want, ..) in REFUSALS {
            let got = sites
                .iter()
                .filter(|(f, line)| f == file && row_covers(needle, line))
                .count();
            if got != *want {
                problems.push(format!(
                    "{file}: `{needle}` matches {got} lines, the row says {want}"
                ));
            }
        }
        problems
    }

    #[test]
    fn every_503_site_is_classified_and_every_global_one_has_a_readiness_row() {
        let sites = refusal_sites();
        assert!(sites.len() > 30, "the scan found the request path");
        let problems = refusal_problems(&sites);
        assert!(problems.is_empty(), "{}", problems.join("\n"));

        // A Global row names a NodeConditions field that readiness reads.
        let status_src = crate::source_scan::read("src/api/handlers/status.rs");
        let status_src = status_src
            .split("#[cfg(test)]\nmod readiness_tests")
            .next()
            .unwrap();
        let verdict_src = status_src
            .split("pub fn readiness_verdict")
            .nth(1)
            // A closing brace at column 0 (built, not written: the scan
            // that masks the test module counts braces, even in strings).
            .and_then(|s| s.split(format!("\n{}\n", '\u{7d}').as_str()).next())
            .expect("readiness_verdict");
        let mut globals = 0;
        for (file, needle, _, reach, why) in REFUSALS {
            if let Reach::Global(field) = reach {
                globals += 1;
                assert!(
                    NodeConditions::FAILING_ROWS
                        .iter()
                        .any(|(name, _)| name == field),
                    "{file} `{needle}` ({why}): {field} is not a NodeConditions row"
                );
                assert!(
                    verdict_src.contains(field),
                    "readiness_verdict does not read {field} ({file}: {why})"
                );
            }
        }
        assert_eq!(globals, 1, "a new global refusal needs a readiness row");
        // The row is fed by the SAME fact the S3 router locks on.
        assert!(
            status_src.contains("config_db_key_mismatch: state.config_db_key_mismatch"),
            "readiness must read AppState::config_db_key_mismatch"
        );
        assert!(
            crate::source_scan::read("src/api/s3_router.rs")
                .contains("if state.config_db_key_mismatch"),
            "the S3 router must lock on AppState::config_db_key_mismatch"
        );
    }

    /// The review's regression: a shutdown refusal written before the auth
    /// gate, as `S3Error::SlowDown`, left the old count-based guard green.
    #[test]
    fn a_drain_refusal_in_the_auth_middleware_is_caught() {
        let mut sites = refusal_sites();
        assert!(refusal_problems(&sites).is_empty());
        sites.push((
            "src/api/auth.rs".into(),
            "return Err(S3Error::SlowDown(\"draining\".into()).into_response());".into(),
        ));
        let problems = refusal_problems(&sites);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("draining"), "{problems:?}");
        // Other shapes, in a file with no rows at all.
        for line in [
            "return StatusCode::SERVICE_UNAVAILABLE.into_response();",
            "Err(s3s::s3_error!(SlowDown, \"busy\"))",
            "S3Error::ServiceUnavailable(\"x\".into())",
            // A refusal that goes through the bridges.
            "return Err(S3Error::from(StorageError::Throttled(\"draining\".into())).into_response());",
            "return Err(EngineError::Overloaded(\"x\".into()).into());",
        ] {
            let mut sites = refusal_sites();
            sites.push(("src/api/middleware_new.rs".into(), line.into()));
            assert!(!refusal_problems(&sites).is_empty(), "{line}");
        }
    }

    /// The health loop already probes every backend each interval; a
    /// second probe per `/_/ready` (kubelet + every router check) only
    /// repeated it.
    #[tokio::test]
    async fn ready_answers_from_a_fresh_health_loop_verdict() {
        let (state, fake, backend) = s3_state().await;
        let health = &state.backend_health;
        health.set_loop_interval(EVERY);
        let now = std::time::Instant::now();
        health.set_for_test(&[("default", &backend)], HealthVerdict::Healthy, now);
        for _ in 0..3 {
            assert_eq!(ready(&state).await, (StatusCode::OK, "ready"));
        }
        assert_eq!(
            fake.requests(),
            Vec::<String>::new(),
            "fresh verdict: 0 upstream"
        );

        let down = HealthVerdict::Unreachable { detail: "x".into() };
        health.set_for_test(&[("default", &backend)], down, std::time::Instant::now());
        assert_eq!(
            ready(&state).await,
            (StatusCode::SERVICE_UNAVAILABLE, "unreachable"),
            "a fresh gating verdict is not ready"
        );
        let erroring = HealthVerdict::Erroring {
            detail: "503".into(),
        };
        health.set_for_test(
            &[("default", &backend)],
            erroring,
            std::time::Instant::now(),
        );
        assert_eq!(ready(&state).await, (StatusCode::OK, "degraded"));
        assert_eq!(fake.requests(), Vec::<String>::new());
    }

    /// A verdict older than 2 x the interval (a hung round), a loop that
    /// never ran its first round, and no loop at all: each probes live,
    /// once per `READY_PROBE_TTL`.
    #[tokio::test]
    async fn ready_probes_live_when_the_loop_cannot_answer() {
        // Stale verdict.
        let (state, fake, backend) = s3_state().await;
        state.backend_health.set_loop_interval(EVERY);
        let old = std::time::Instant::now() - EVERY * 2;
        state
            .backend_health
            .set_for_test(&[("default", &backend)], HealthVerdict::Healthy, old);
        assert_eq!(ready(&state).await, (StatusCode::OK, "ready"));
        assert_eq!(ready(&state).await, (StatusCode::OK, "ready"));
        assert_eq!(
            fake.requests().len(),
            1,
            "stale: 1 live probe, then its TTL"
        );

        // Loop started, first round not done: no verdict yet.
        let (state, fake, _) = s3_state().await;
        state.backend_health.set_loop_interval(EVERY);
        assert_eq!(ready(&state).await, (StatusCode::OK, "ready"));
        assert_eq!(fake.requests().len(), 1, "boot: live probe");

        // Loop off (DGP_BACKEND_HEALTH_INTERVAL_SECS=0): a verdict alone
        // (boot probe, admin test) is not enough.
        let (state, fake, backend) = s3_state().await;
        let now = std::time::Instant::now();
        state
            .backend_health
            .set_for_test(&[("default", &backend)], HealthVerdict::Healthy, now);
        assert_eq!(ready(&state).await, (StatusCode::OK, "ready"));
        assert_eq!(fake.requests().len(), 1, "loop off: live probe");
    }

    /// N concurrent probes that find no fresh verdict send ONE request.
    #[tokio::test]
    async fn concurrent_live_probes_share_one_request() {
        let (state, fake, _) = s3_state().await;
        fake.set_delay_ms("LIST_BUCKETS", 100);
        let probes = (0..8).map(|_| ready(&state));
        for answer in futures::future::join_all(probes).await {
            assert_eq!(answer, (StatusCode::OK, "ready"));
        }
        assert_eq!(fake.requests().len(), 1, "{:?}", fake.requests());
    }

    /// A backend that is down is not ready: the single flight caches the
    /// failure too (for its TTL), and a fresh probe sees it.
    #[tokio::test]
    async fn a_failed_live_probe_is_not_ready() {
        let (state, fake, _) = s3_state().await;
        fake.fail("LIST_BUCKETS", "/", 500, "InternalError", 100);
        let (code, _) = ready(&state).await;
        assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn health_loop_verdicts_map_to_readiness() {
        let down = || HealthVerdict::Unreachable { detail: "x".into() };
        let erroring = || HealthVerdict::Erroring { detail: "x".into() };
        assert_eq!(
            readiness_from_health_loop(&[HealthVerdict::Healthy]),
            "ready"
        );
        assert_eq!(
            readiness_from_health_loop(&[down(), HealthVerdict::Healthy]),
            "ready"
        );
        assert_eq!(
            readiness_from_health_loop(&[erroring(), down()]),
            "degraded"
        );
        assert_eq!(readiness_from_health_loop(&[down(), down()]), "unreachable");
        let rejected = HealthVerdict::AuthRejected { detail: "x".into() };
        assert_eq!(readiness_from_health_loop(&[rejected]), "unreachable");
    }

    /// Every S3 mutation holds the config DB mutex for a moment (the event
    /// outbox append), and so do replication, event delivery and the IAM
    /// sync. A readiness probe that met such a holder answered 503, so a
    /// load balancer ejected a busy, healthy node.
    #[tokio::test]
    async fn a_config_db_held_for_a_moment_is_ready() {
        let dir = tempfile::tempdir().unwrap();
        let backend: Box<crate::storage::DynStorageBackend<'static>> =
            crate::storage::DynStorageBackend::new_box(
                crate::storage::FilesystemBackend::new(dir.path().to_path_buf())
                    .await
                    .unwrap(),
            );
        let engine = crate::deltaglider::DeltaGliderEngine::new_with_backend(
            Arc::new(backend),
            &crate::config::Config::default(),
            None,
        );
        let mut state = AppState::for_tests(engine);
        let db = Arc::new(tokio::sync::Mutex::new(
            crate::config_db::ConfigDb::in_memory("readiness-test-key").unwrap(),
        ));
        Arc::get_mut(&mut state).unwrap().config_db = Some(db.clone());
        let held = db.clone().lock_owned().await;
        let release = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            drop(held);
        });
        let (code, Json(body)) = readiness_check(State(state)).await;
        release.await.unwrap();
        assert_eq!(
            (code, body.config_db),
            (StatusCode::OK, "ready"),
            "a config DB held for 200 ms made the node not ready"
        );
    }

    #[test]
    fn live_backends_gate_readiness_only_when_all_are_gated() {
        use crate::coordination::HealthVerdict as V;
        let down = || V::Unreachable { detail: "x".into() };
        assert!(live_backends_ready(&[]));
        assert!(live_backends_ready(&[V::Healthy, down()]));
        assert!(!live_backends_ready(&[down(), down()]));
        // Erroring (reachable, 5xx) never gates.
        assert!(live_backends_ready(&[V::Erroring { detail: "x".into() }]));
    }

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
