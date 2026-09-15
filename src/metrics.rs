// SPDX-License-Identifier: BUSL-1.1

//! Prometheus metrics for DeltaGlider Proxy.
//!
//! All metric types use atomics internally (no locks on the hot path).
//! The `Metrics` struct is `Clone`-cheap (Arc-based registry + Arc-based collectors).

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use prometheus::{
    Encoder, Gauge, GaugeVec, Histogram, HistogramOpts, HistogramVec, IntCounter, IntCounterVec,
    IntGauge, Opts, Registry, TextEncoder, TEXT_FORMAT,
};
use std::sync::Arc;
use std::time::Instant;

use crate::api::handlers::AppState;

/// All Prometheus metrics for DeltaGlider Proxy.
#[derive(Clone)]
pub struct Metrics {
    pub registry: Registry,

    // -- Process & Build --
    pub process_start_time_seconds: Gauge,
    pub build_info: GaugeVec,
    pub process_peak_rss_bytes: Gauge,

    // -- HTTP Requests --
    pub http_requests_total: IntCounterVec,
    pub http_request_duration_seconds: HistogramVec,
    pub http_request_size_bytes: HistogramVec,
    pub http_response_size_bytes: HistogramVec,

    // -- Delta Compression --
    pub delta_compression_ratio: Histogram,
    pub delta_bytes_saved_total: IntCounter,
    pub delta_encode_duration_seconds: Histogram,
    pub delta_decode_duration_seconds: Histogram,
    pub delta_decisions_total: IntCounterVec,

    // -- Cache --
    pub cache_hits_total: IntCounter,
    pub cache_misses_total: IntCounter,
    pub cache_size_bytes: Gauge,
    pub cache_entries: Gauge,
    pub cache_max_bytes: Gauge,
    pub cache_utilization_ratio: Gauge,
    pub cache_miss_rate_ratio: Gauge,

    // -- Codec Concurrency --
    pub codec_semaphore_available: Gauge,

    // -- Auth --
    pub auth_attempts_total: IntCounterVec,
    pub auth_failures_total: IntCounterVec,

    // -- Multipart Sweep --
    pub multipart_sweep_runs_total: IntCounterVec,
    pub multipart_sweep_duration_seconds: HistogramVec,
    pub multipart_swept_uploads_total: IntCounterVec,
    pub multipart_sweep_reclaimed_bytes_total: IntCounter,
    pub multipart_sweep_orphan_relay_dirs_total: IntCounter,
    pub multipart_sweep_orphan_relay_files_total: IntCounter,
    pub multipart_sweep_last_uploads_reclaimed: Gauge,
    pub multipart_sweep_last_reclaimed_bytes: Gauge,
    pub multipart_uploads_inflight: Gauge,

    // -- Replication streaming-copy (Phase B) --
    // Deterministic high-water gauges + totals. Prove bounded memory, real
    // part/object concurrency, and per-part resume without clock/RSS gating.
    pub replication_part_bytes_resident: IntGauge,
    pub replication_part_bytes_resident_peak: IntGauge,
    pub replication_parts_inflight: IntGauge,
    pub replication_parts_inflight_peak: IntGauge,
    pub replication_objects_inflight: IntGauge,
    pub replication_objects_inflight_peak: IntGauge,
    pub replication_multipart_parts_total: IntCounter,
    pub replication_part_retries_total: IntCounter,
    pub replication_bytes_streamed_total: IntCounter,
    pub replication_delta_passthrough_bytes_saved_total: IntCounter,

    // -- Replication reconcile walk --
    // Deterministic I/O counters for the tree walk: tests gate on counts
    // (zero HEADs on PureMirror, list-calls ≈ 2×dirs), never wall-clock.
    // Bumped ONLY at the reconcile driver's call sites — parity, the event
    // consumer, and API listings must not pollute them.
    pub replication_list_calls_total: IntCounter,
    pub replication_head_calls_total: IntCounter,
    pub replication_dirs_completed_total: IntCounter,
}

/// Set `peak` to `live`'s current value when it has risen above the prior
/// high-water mark. Called after each `live` increment. Not atomic across the
/// read+set, but the streaming copy peaks settle before the post-run scrape.
pub fn bump_peak(live: &IntGauge, peak: &IntGauge) {
    let now = live.get();
    if now > peak.get() {
        peak.set(now);
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Helper: create a metric, register it, and return the clone.
/// Panics only on duplicate metric names (programmer bug, not runtime failure).
macro_rules! register {
    ($registry:expr, $metric:expr) => {{
        let m = $metric;
        $registry
            .register(Box::new(m.clone()))
            .expect("duplicate metric name");
        m
    }};
}

impl Metrics {
    pub fn new() -> Self {
        let registry = Registry::new();

        // -- Process & Build --
        let process_start_time_seconds = register!(
            registry,
            Gauge::new("process_start_time_seconds", "Start time of the process").unwrap()
        );
        let build_info = register!(
            registry,
            GaugeVec::new(
                Opts::new("deltaglider_build_info", "Build information"),
                &["version", "backend_type"],
            )
            .unwrap()
        );
        let process_peak_rss_bytes = register!(
            registry,
            Gauge::new(
                "process_peak_rss_bytes",
                "Peak resident set size in bytes (updated on scrape)",
            )
            .unwrap()
        );

        #[cfg(target_os = "linux")]
        {
            let pc = prometheus::process_collector::ProcessCollector::for_self();
            let _ = registry.register(Box::new(pc));
        }

        // -- HTTP Requests --
        let http_requests_total = register!(
            registry,
            IntCounterVec::new(
                Opts::new(
                    "deltaglider_http_requests_total",
                    "Total HTTP requests by method, status, and operation",
                ),
                &["method", "status", "operation"],
            )
            .unwrap()
        );

        // [1KB, 10KB, 100KB, 1MB, 10MB, 100MB]
        let body_size_buckets = prometheus::exponential_buckets(1024.0, 10.0, 6).unwrap();

        let http_request_duration_seconds = register!(
            registry,
            HistogramVec::new(
                HistogramOpts::new(
                    "deltaglider_http_request_duration_seconds",
                    "HTTP request duration in seconds",
                ),
                &["method", "operation"],
            )
            .unwrap()
        );
        let http_request_size_bytes = register!(
            registry,
            HistogramVec::new(
                HistogramOpts::new(
                    "deltaglider_http_request_size_bytes",
                    "HTTP request body size in bytes",
                )
                .buckets(body_size_buckets.clone()),
                &["method"],
            )
            .unwrap()
        );
        let http_response_size_bytes = register!(
            registry,
            HistogramVec::new(
                HistogramOpts::new(
                    "deltaglider_http_response_size_bytes",
                    "HTTP response body size in bytes",
                )
                .buckets(body_size_buckets),
                &["method"],
            )
            .unwrap()
        );

        // -- Delta Compression --
        let codec_duration_buckets = vec![
            0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
        ];
        let ratio_buckets = vec![
            0.01, 0.05, 0.1, 0.15, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1.0,
        ];

        let delta_compression_ratio = register!(
            registry,
            Histogram::with_opts(
                HistogramOpts::new(
                    "deltaglider_delta_compression_ratio",
                    "Delta compression ratio distribution",
                )
                .buckets(ratio_buckets),
            )
            .unwrap()
        );
        let delta_bytes_saved_total = register!(
            registry,
            IntCounter::new(
                "deltaglider_delta_bytes_saved_total",
                "Total bytes saved by delta compression",
            )
            .unwrap()
        );
        let delta_encode_duration_seconds = register!(
            registry,
            Histogram::with_opts(
                HistogramOpts::new(
                    "deltaglider_delta_encode_duration_seconds",
                    "Delta encode duration in seconds",
                )
                .buckets(codec_duration_buckets.clone()),
            )
            .unwrap()
        );
        let delta_decode_duration_seconds = register!(
            registry,
            Histogram::with_opts(
                HistogramOpts::new(
                    "deltaglider_delta_decode_duration_seconds",
                    "Delta decode duration in seconds",
                )
                .buckets(codec_duration_buckets),
            )
            .unwrap()
        );
        let delta_decisions_total = register!(
            registry,
            IntCounterVec::new(
                Opts::new(
                    "deltaglider_delta_decisions_total",
                    "Delta storage decisions by type",
                ),
                &["decision"],
            )
            .unwrap()
        );

        // -- Cache --
        let cache_hits_total = register!(
            registry,
            IntCounter::new("deltaglider_cache_hits_total", "Total reference cache hits").unwrap()
        );
        let cache_misses_total = register!(
            registry,
            IntCounter::new(
                "deltaglider_cache_misses_total",
                "Total reference cache misses",
            )
            .unwrap()
        );
        let cache_size_bytes = register!(
            registry,
            Gauge::new(
                "deltaglider_cache_size_bytes",
                "Current cache size in bytes (updated on scrape)",
            )
            .unwrap()
        );
        let cache_entries = register!(
            registry,
            Gauge::new("deltaglider_cache_entries", "Current cache entry count").unwrap()
        );
        let cache_max_bytes = register!(
            registry,
            Gauge::new(
                "deltaglider_cache_max_bytes",
                "Configured maximum cache capacity in bytes",
            )
            .unwrap()
        );
        let cache_utilization_ratio = register!(
            registry,
            Gauge::new(
                "deltaglider_cache_utilization_ratio",
                "Cache utilization ratio (weighted_size / max_capacity, 0.0-1.0)",
            )
            .unwrap()
        );
        let cache_miss_rate_ratio = register!(
            registry,
            Gauge::new(
                "deltaglider_cache_miss_rate_ratio",
                "Cache miss rate ratio since startup (misses / total, 0.0-1.0)",
            )
            .unwrap()
        );

        // -- Codec Concurrency --
        let codec_semaphore_available = register!(
            registry,
            Gauge::new(
                "deltaglider_codec_semaphore_available",
                "Available codec semaphore permits",
            )
            .unwrap()
        );

        // -- Auth --
        let auth_attempts_total = register!(
            registry,
            IntCounterVec::new(
                Opts::new("deltaglider_auth_attempts_total", "Auth attempts by result"),
                &["result"],
            )
            .unwrap()
        );
        let auth_failures_total = register!(
            registry,
            IntCounterVec::new(
                Opts::new("deltaglider_auth_failures_total", "Auth failures by reason"),
                &["reason"],
            )
            .unwrap()
        );
        // -- Multipart Sweep --
        let multipart_sweep_runs_total = register!(
            registry,
            IntCounterVec::new(
                Opts::new(
                    "deltaglider_multipart_sweep_runs_total",
                    "Multipart sweeper runs by phase",
                ),
                &["phase"],
            )
            .unwrap()
        );
        let multipart_sweep_duration_seconds = register!(
            registry,
            HistogramVec::new(
                HistogramOpts::new(
                    "deltaglider_multipart_sweep_duration_seconds",
                    "Multipart sweeper run duration in seconds",
                ),
                &["phase"],
            )
            .unwrap()
        );
        let multipart_swept_uploads_total = register!(
            registry,
            IntCounterVec::new(
                Opts::new(
                    "deltaglider_multipart_swept_uploads_total",
                    "Multipart uploads reclaimed by sweeper state",
                ),
                &["state"],
            )
            .unwrap()
        );
        let multipart_sweep_reclaimed_bytes_total = register!(
            registry,
            IntCounter::new(
                "deltaglider_multipart_sweep_reclaimed_bytes_total",
                "Total bytes reclaimed by multipart sweeper",
            )
            .unwrap()
        );
        let multipart_sweep_orphan_relay_dirs_total = register!(
            registry,
            IntCounter::new(
                "deltaglider_multipart_sweep_orphan_relay_dirs_total",
                "Total orphan multipart relay directories removed",
            )
            .unwrap()
        );
        let multipart_sweep_orphan_relay_files_total = register!(
            registry,
            IntCounter::new(
                "deltaglider_multipart_sweep_orphan_relay_files_total",
                "Total orphan multipart relay files removed",
            )
            .unwrap()
        );
        let multipart_sweep_last_uploads_reclaimed = register!(
            registry,
            Gauge::new(
                "deltaglider_multipart_sweep_last_uploads_reclaimed",
                "Uploads reclaimed in the latest multipart sweep run",
            )
            .unwrap()
        );
        let multipart_sweep_last_reclaimed_bytes = register!(
            registry,
            Gauge::new(
                "deltaglider_multipart_sweep_last_reclaimed_bytes",
                "Bytes reclaimed in the latest multipart sweep run",
            )
            .unwrap()
        );
        let multipart_uploads_inflight = register!(
            registry,
            Gauge::new(
                "deltaglider_multipart_uploads_inflight",
                "Current in-flight multipart upload count",
            )
            .unwrap()
        );

        // -- Replication streaming-copy (Phase B) --
        let replication_part_bytes_resident = register!(
            registry,
            IntGauge::new(
                "deltaglider_replication_part_bytes_resident",
                "Bytes currently held in streaming-copy part buffers",
            )
            .unwrap()
        );
        let replication_part_bytes_resident_peak = register!(
            registry,
            IntGauge::new(
                "deltaglider_replication_part_bytes_resident_peak",
                "High-water bytes held in streaming-copy part buffers",
            )
            .unwrap()
        );
        let replication_parts_inflight = register!(
            registry,
            IntGauge::new(
                "deltaglider_replication_parts_inflight",
                "Concurrent streaming-copy parts in flight",
            )
            .unwrap()
        );
        let replication_parts_inflight_peak = register!(
            registry,
            IntGauge::new(
                "deltaglider_replication_parts_inflight_peak",
                "High-water concurrent streaming-copy parts in flight",
            )
            .unwrap()
        );
        let replication_objects_inflight = register!(
            registry,
            IntGauge::new(
                "deltaglider_replication_objects_inflight",
                "Concurrent replication objects in flight",
            )
            .unwrap()
        );
        let replication_objects_inflight_peak = register!(
            registry,
            IntGauge::new(
                "deltaglider_replication_objects_inflight_peak",
                "High-water concurrent replication objects in flight",
            )
            .unwrap()
        );
        let replication_multipart_parts_total = register!(
            registry,
            IntCounter::new(
                "deltaglider_replication_multipart_parts_total",
                "Total streaming-copy multipart parts uploaded",
            )
            .unwrap()
        );
        let replication_part_retries_total = register!(
            registry,
            IntCounter::new(
                "deltaglider_replication_part_retries_total",
                "Total streaming-copy per-part range-resume retries",
            )
            .unwrap()
        );
        let replication_bytes_streamed_total = register!(
            registry,
            IntCounter::new(
                "deltaglider_replication_bytes_streamed_total",
                "Total bytes streamed through the multipart copy path",
            )
            .unwrap()
        );
        let replication_delta_passthrough_bytes_saved_total = register!(
            registry,
            IntCounter::new(
                "deltaglider_replication_delta_passthrough_bytes_saved_total",
                "Total egress bytes saved by shipping deltas verbatim (logical − delta size)",
            )
            .unwrap()
        );
        let replication_list_calls_total = register!(
            registry,
            IntCounter::new(
                "deltaglider_replication_list_calls_total",
                "Listing pages issued by the replication reconcile walk (both sides)",
            )
            .unwrap()
        );
        let replication_head_calls_total = register!(
            registry,
            IntCounter::new(
                "deltaglider_replication_head_calls_total",
                "Per-object HEAD calls issued by the replication reconcile walk",
            )
            .unwrap()
        );
        let replication_dirs_completed_total = register!(
            registry,
            IntCounter::new(
                "deltaglider_replication_dirs_completed_total",
                "Directories fully reconciled by the replication walk",
            )
            .unwrap()
        );

        // Delegated-listing request-amplification counters (issue #82). These
        // live as statics in `storage::s3` (the backend has no Metrics handle);
        // registering the clones here puts them on the same /_/metrics scrape.
        registry
            .register(Box::new(
                crate::storage::DELEGATED_LIST_UPSTREAM_PAGES.clone(),
            ))
            .expect("duplicate metric name");
        registry
            .register(Box::new(
                crate::storage::DELEGATED_LIST_PROBE_REQUESTS.clone(),
            ))
            .expect("duplicate metric name");

        Metrics {
            registry,
            process_start_time_seconds,
            build_info,
            process_peak_rss_bytes,
            http_requests_total,
            http_request_duration_seconds,
            http_request_size_bytes,
            http_response_size_bytes,
            delta_compression_ratio,
            delta_bytes_saved_total,
            delta_encode_duration_seconds,
            delta_decode_duration_seconds,
            delta_decisions_total,
            cache_hits_total,
            cache_misses_total,
            cache_size_bytes,
            cache_entries,
            cache_max_bytes,
            cache_utilization_ratio,
            cache_miss_rate_ratio,
            codec_semaphore_available,
            auth_attempts_total,
            auth_failures_total,
            multipart_sweep_runs_total,
            multipart_sweep_duration_seconds,
            multipart_swept_uploads_total,
            multipart_sweep_reclaimed_bytes_total,
            multipart_sweep_orphan_relay_dirs_total,
            multipart_sweep_orphan_relay_files_total,
            multipart_sweep_last_uploads_reclaimed,
            multipart_sweep_last_reclaimed_bytes,
            multipart_uploads_inflight,
            replication_part_bytes_resident,
            replication_part_bytes_resident_peak,
            replication_parts_inflight,
            replication_parts_inflight_peak,
            replication_objects_inflight,
            replication_objects_inflight_peak,
            replication_multipart_parts_total,
            replication_part_retries_total,
            replication_bytes_streamed_total,
            replication_delta_passthrough_bytes_saved_total,
            replication_list_calls_total,
            replication_head_calls_total,
            replication_dirs_completed_total,
        }
    }
}

/// Classify an S3 request into a bounded operation label.
pub fn classify_s3_operation(method: &str, path: &str) -> &'static str {
    // Admin/status endpoints
    match path {
        "/health" => return "health",
        "/stats" => return "stats",
        "/metrics" => return "metrics",
        _ => {}
    }

    // Count path segments (ignoring empty segments from leading/trailing slashes)
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    match (method, segments.len()) {
        // Root level
        ("GET", 0) => "list_buckets",
        ("HEAD", 0) => "head_root",
        // Bucket level
        ("GET", 1) => "list_objects",
        ("PUT", 1) => "create_bucket",
        ("DELETE", 1) => "delete_bucket",
        ("HEAD", 1) => "head_bucket",
        ("POST", 1) => "post_bucket",
        // Object level (2+ segments = bucket + key)
        ("GET", _) => "get_object",
        ("PUT", _) => "put_object",
        ("DELETE", _) => "delete_object",
        ("HEAD", _) => "head_object",
        ("POST", _) => "post_object",
        _ => "unknown",
    }
}

/// Record the bounded request counter for paths that short-circuit before
/// [`http_metrics_middleware`] can observe the response.
pub fn record_http_request_total(metrics: &Metrics, method: &str, path: &str, status: StatusCode) {
    let operation = classify_s3_operation(method, path);
    let status = status.as_u16().to_string();
    metrics
        .http_requests_total
        .with_label_values(&[method, status.as_str(), operation])
        .inc();
}

/// Axum middleware that records HTTP request metrics.
pub async fn http_metrics_middleware(
    State(state): State<Arc<AppState>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let metrics = &state.metrics;

    let method = request.method().to_string();
    let path = request.uri().path().to_string();
    let operation = classify_s3_operation(&method, &path);

    // Record request size from Content-Length if available
    if let Some(cl) = request
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<f64>().ok())
    {
        metrics
            .http_request_size_bytes
            .with_label_values(&[&method])
            .observe(cl);
    }

    let start = Instant::now();
    let response = next.run(request).await;
    let duration = start.elapsed().as_secs_f64();

    // prometheus 0.14 widened `with_label_values` to a uniform-type slice:
    // pass `&str` for every element.
    record_http_request_total(metrics, method.as_str(), &path, response.status());
    metrics
        .http_request_duration_seconds
        .with_label_values(&[method.as_str(), operation])
        .observe(duration);

    // Record response size from Content-Length if available
    if let Some(cl) = response
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<f64>().ok())
    {
        metrics
            .http_response_size_bytes
            .with_label_values(&[&method])
            .observe(cl);
    }

    response
}

/// Handler for GET /metrics — returns Prometheus text format.
pub async fn metrics_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let metrics = &state.metrics;

    // Update on-demand gauges (all O(1) atomic reads)
    let engine = state.engine.load();
    metrics
        .process_peak_rss_bytes
        .set(crate::api::handlers::get_peak_rss_bytes() as f64);
    metrics
        .cache_size_bytes
        .set(engine.cache_weighted_size() as f64);
    metrics.cache_entries.set(engine.cache_entry_count() as f64);
    // Derived cache gauges (computed from existing atomic counters — zero overhead)
    let max = engine.cache_max_capacity() as f64;
    if max > 0.0 {
        metrics
            .cache_utilization_ratio
            .set(engine.cache_weighted_size() as f64 / max);
    }
    let hits = metrics.cache_hits_total.get() as f64;
    let misses = metrics.cache_misses_total.get() as f64;
    let total = hits + misses;
    if total > 0.0 {
        metrics.cache_miss_rate_ratio.set(misses / total);
    }
    metrics
        .codec_semaphore_available
        .set(engine.codec_available_permits() as f64);
    metrics
        .multipart_uploads_inflight
        .set(state.multipart.count_uploads() as f64);

    let encoder = TextEncoder::new();
    let metric_families = metrics.registry.gather();
    let mut buffer = Vec::new();
    if let Err(e) = encoder.encode(&metric_families, &mut buffer) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to encode metrics: {}", e),
        )
            .into_response();
    }

    (StatusCode::OK, [("content-type", TEXT_FORMAT)], buffer).into_response()
}

// ─────────────────────────────────────────────────────────────────────────
// Tokio runtime metrics (#87)
//
// Compiled ONLY under `--cfg tokio_unstable` (docs: the RuntimeMetrics API
// is unstable and gated). A default build registers nothing and spawns no
// task — /metrics simply omits the series, which is the honest signal that
// the binary was built without the flag. The nightly workflow builds with
// RUSTFLAGS="--cfg tokio_unstable" so the series type-checks + works there.
//
// This is the diagnostic layer behind #85/#86: poll-time shows whether
// workers run long polls (blocking work inside async), queue depths show
// saturation, budget-yield counts show tasks hogging workers until the
// scheduler forces them off.
// ─────────────────────────────────────────────────────────────────────────

/// Spawn the runtime-metrics sampler (1s cadence). No-op unless the binary
/// was built with `--cfg tokio_unstable`.
#[cfg(tokio_unstable)]
pub fn spawn_tokio_runtime_metrics_sampler(metrics: &Arc<Metrics>) {
    use std::sync::OnceLock;

    static SAMPLER: OnceLock<()> = OnceLock::new();
    // Defensive only: a second call would double-register the metric names.
    // Nothing calls this twice today (`main.rs` calls it once), so this makes
    // the single-sampler assumption explicit rather than papering over a
    // real re-init path.
    if SAMPLER.set(()).is_err() {
        return;
    }

    let rt = tokio::runtime::Handle::current().metrics();
    let registry = &metrics.registry;

    let g = |name: &str, help: &str| -> Gauge {
        let gauge = Gauge::new(name, help).unwrap();
        // Best-effort: a duplicate name would mean a programmer error in a
        // single-sampler world; ignore rather than crash the proxy.
        let _ = registry.register(Box::new(gauge.clone()));
        gauge
    };

    let worker_mean_poll_seconds = g(
        "deltaglider_tokio_worker_mean_poll_seconds",
        "Worst worker's mean task poll duration (EWMA). Long polls = blocking work inside async context.",
    );
    let global_queue_depth = g(
        "deltaglider_tokio_global_queue_depth",
        "Tasks pending in the runtime global (injection) queue. Sustained depth = saturation.",
    );
    let blocking_queue_depth = g(
        "deltaglider_tokio_blocking_queue_depth",
        "Tasks pending in the blocking pool. Sustained depth = spawn_blocking saturation.",
    );
    let workers_total = g("deltaglider_tokio_workers", "Runtime worker threads.");
    workers_total.set(rt.num_workers() as f64);

    // Cumulatives are COUNTERS, not gauges: we sample the absolute total and
    // `inc_by` the delta, so `rate()`/`increase()` see a real monotonic
    // counter. (A `_total` gauge misleads Prometheus tooling.)
    let budget_forced_yields_total = {
        let c = IntCounter::new(
            "deltaglider_tokio_budget_forced_yields_total",
            "Polls the scheduler force-yielded after exhausting their budget.",
        )
        .unwrap();
        let _ = registry.register(Box::new(c.clone()));
        c
    };

    // Poll-time histogram: this is the series `enable_metrics_poll_time_histogram()`
    // in `main.rs` turns on, and it is what exposes the TAIL a mean hides.
    // Bucket boundaries are runtime-configured, so build one child counter
    // per range and label by range.
    let poll_hist = if rt.poll_time_histogram_enabled() {
        let n = rt.poll_time_histogram_num_buckets();
        let ranges: Vec<String> = (0..n)
            .map(|i| {
                let r = rt.poll_time_histogram_bucket_range(i);
                format!("{}-{}us", r.start.as_micros(), r.end.as_micros())
            })
            .collect();
        let v = IntCounterVec::new(
            Opts::new(
                "deltaglider_tokio_poll_time_range_total",
                "Task polls per duration range, summed across workers. \
                 Counts in the high ranges are long polls.",
            ),
            &["range"],
        )
        .unwrap();
        // Pre-create the children so every range appears from the first
        // scrape, including ranges that have seen no polls yet.
        for r in &ranges {
            v.with_label_values(&[r.as_str()]);
        }
        let _ = registry.register(Box::new(v.clone()));
        Some((v, ranges))
    } else {
        None
    };

    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
        let mut prev_budget: u64 = 0;
        let mut prev_buckets: Vec<u64> =
            vec![0; poll_hist.as_ref().map(|(_, r)| r.len()).unwrap_or(0)];
        loop {
            tick.tick().await;
            // Worst (not mean-of-means) worker: a single poisoned worker must
            // stay visible.
            let mut worst_secs = 0f64;
            for w in 0..rt.num_workers() {
                let secs = rt.worker_mean_poll_time(w).as_secs_f64();
                if secs > worst_secs {
                    worst_secs = secs;
                }
            }
            worker_mean_poll_seconds.set(worst_secs);
            global_queue_depth.set(rt.global_queue_depth() as f64);
            blocking_queue_depth.set(rt.blocking_queue_depth() as f64);

            let cur_budget = rt.budget_forced_yield_count();
            if cur_budget >= prev_budget {
                budget_forced_yields_total.inc_by(cur_budget - prev_budget);
                prev_budget = cur_budget;
            }
            if let Some((hist, ranges)) = poll_hist.as_ref() {
                for (i, range) in ranges.iter().enumerate() {
                    let cur: u64 = (0..rt.num_workers())
                        .map(|w| rt.poll_time_histogram_bucket_count(w, i))
                        .sum();
                    if cur >= prev_buckets[i] {
                        hist.with_label_values(&[range.as_str()])
                            .inc_by(cur - prev_buckets[i]);
                        prev_buckets[i] = cur;
                    }
                }
            }
        }
    });
}

/// Compiled-out stub so call sites never need their own cfg.
#[cfg(not(tokio_unstable))]
pub fn spawn_tokio_runtime_metrics_sampler(_metrics: &Arc<Metrics>) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_s3_operation() {
        assert_eq!(classify_s3_operation("GET", "/health"), "health");
        assert_eq!(classify_s3_operation("GET", "/stats"), "stats");
        assert_eq!(classify_s3_operation("GET", "/metrics"), "metrics");
        assert_eq!(classify_s3_operation("GET", "/"), "list_buckets");
        assert_eq!(classify_s3_operation("HEAD", "/"), "head_root");
        assert_eq!(classify_s3_operation("GET", "/mybucket"), "list_objects");
        assert_eq!(classify_s3_operation("PUT", "/mybucket"), "create_bucket");
        assert_eq!(
            classify_s3_operation("DELETE", "/mybucket"),
            "delete_bucket"
        );
        assert_eq!(classify_s3_operation("HEAD", "/mybucket"), "head_bucket");
        assert_eq!(
            classify_s3_operation("GET", "/mybucket/mykey"),
            "get_object"
        );
        assert_eq!(
            classify_s3_operation("PUT", "/mybucket/mykey"),
            "put_object"
        );
        assert_eq!(
            classify_s3_operation("DELETE", "/mybucket/mykey"),
            "delete_object"
        );
        assert_eq!(
            classify_s3_operation("HEAD", "/mybucket/mykey"),
            "head_object"
        );
        assert_eq!(
            classify_s3_operation("POST", "/mybucket/mykey"),
            "post_object"
        );
        assert_eq!(
            classify_s3_operation("GET", "/mybucket/deep/nested/key"),
            "get_object"
        );
    }

    /// #87: the sampler must actually export the series it claims, including
    /// the poll-time histogram it enables. Runs only under `--cfg
    /// tokio_unstable` (the whole code path is gated there).
    #[cfg(tokio_unstable)]
    #[test]
    fn runtime_metrics_sampler_exports_series() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .enable_metrics_poll_time_histogram()
            .build()
            .unwrap();
        rt.block_on(async {
            let m = Arc::new(Metrics::new());
            spawn_tokio_runtime_metrics_sampler(&m);
            // Let at least one tick run (interval fires immediately, but give
            // the task a moment to be polled).
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let names: Vec<String> = m
                .registry
                .gather()
                .iter()
                .map(|f| f.name().to_string())
                .collect();
            for expected in [
                "deltaglider_tokio_workers",
                "deltaglider_tokio_global_queue_depth",
                "deltaglider_tokio_blocking_queue_depth",
                "deltaglider_tokio_budget_forced_yields_total",
                "deltaglider_tokio_poll_time_range_total",
            ] {
                assert!(
                    names.iter().any(|n| n == expected),
                    "sampler must export {expected}; got {names:?}"
                );
            }
            // The histogram counter must carry a range label.
            let hist = m
                .registry
                .gather()
                .into_iter()
                .find(|f| f.name() == "deltaglider_tokio_poll_time_range_total")
                .expect("histogram family present");
            let has_range = hist
                .get_metric()
                .iter()
                .any(|mm| mm.get_label().iter().any(|l| l.name() == "range"));
            assert!(has_range, "poll-time counter must be labelled by range");
        });
    }
}
