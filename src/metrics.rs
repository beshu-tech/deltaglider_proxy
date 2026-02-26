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
    Opts, Registry, TextEncoder, TEXT_FORMAT,
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

    // -- Codec Concurrency --
    pub codec_semaphore_available: Gauge,

    // -- Auth --
    pub auth_attempts_total: IntCounterVec,
    pub auth_failures_total: IntCounterVec,
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

impl Metrics {
    pub fn new() -> Self {
        let registry = Registry::new();

        // -- Process & Build --
        let process_start_time_seconds =
            Gauge::new("process_start_time_seconds", "Start time of the process").unwrap();
        registry
            .register(Box::new(process_start_time_seconds.clone()))
            .unwrap();

        let build_info = GaugeVec::new(
            Opts::new("deltaglider_build_info", "Build information"),
            &["version", "backend_type"],
        )
        .unwrap();
        registry.register(Box::new(build_info.clone())).unwrap();

        let process_peak_rss_bytes = Gauge::new(
            "process_peak_rss_bytes",
            "Peak resident set size in bytes (updated on scrape)",
        )
        .unwrap();
        registry
            .register(Box::new(process_peak_rss_bytes.clone()))
            .unwrap();

        // Register standard process metrics (RSS, CPU, open FDs on Linux)
        #[cfg(target_os = "linux")]
        {
            let pc = prometheus::process_collector::ProcessCollector::for_self();
            let _ = registry.register(Box::new(pc));
        }

        // -- HTTP Requests --
        let http_requests_total = IntCounterVec::new(
            Opts::new(
                "deltaglider_http_requests_total",
                "Total HTTP requests by method, status, and operation",
            ),
            &["method", "status", "operation"],
        )
        .unwrap();
        registry
            .register(Box::new(http_requests_total.clone()))
            .unwrap();

        let body_size_buckets = prometheus::exponential_buckets(1024.0, 10.0, 6).unwrap();
        // [1KB, 10KB, 100KB, 1MB, 10MB, 100MB]

        let http_request_duration_seconds = HistogramVec::new(
            HistogramOpts::new(
                "deltaglider_http_request_duration_seconds",
                "HTTP request duration in seconds",
            ),
            &["method", "operation"],
        )
        .unwrap();
        registry
            .register(Box::new(http_request_duration_seconds.clone()))
            .unwrap();

        let http_request_size_bytes = HistogramVec::new(
            HistogramOpts::new(
                "deltaglider_http_request_size_bytes",
                "HTTP request body size in bytes",
            )
            .buckets(body_size_buckets.clone()),
            &["method"],
        )
        .unwrap();
        registry
            .register(Box::new(http_request_size_bytes.clone()))
            .unwrap();

        let http_response_size_bytes = HistogramVec::new(
            HistogramOpts::new(
                "deltaglider_http_response_size_bytes",
                "HTTP response body size in bytes",
            )
            .buckets(body_size_buckets),
            &["method"],
        )
        .unwrap();
        registry
            .register(Box::new(http_response_size_bytes.clone()))
            .unwrap();

        // -- Delta Compression --
        let codec_duration_buckets = vec![
            0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0,
        ];
        let ratio_buckets = vec![
            0.01, 0.05, 0.1, 0.15, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1.0,
        ];

        let delta_compression_ratio = Histogram::with_opts(
            HistogramOpts::new(
                "deltaglider_delta_compression_ratio",
                "Delta compression ratio distribution",
            )
            .buckets(ratio_buckets),
        )
        .unwrap();
        registry
            .register(Box::new(delta_compression_ratio.clone()))
            .unwrap();

        let delta_bytes_saved_total = IntCounter::new(
            "deltaglider_delta_bytes_saved_total",
            "Total bytes saved by delta compression",
        )
        .unwrap();
        registry
            .register(Box::new(delta_bytes_saved_total.clone()))
            .unwrap();

        let delta_encode_duration_seconds = Histogram::with_opts(
            HistogramOpts::new(
                "deltaglider_delta_encode_duration_seconds",
                "Delta encode duration in seconds",
            )
            .buckets(codec_duration_buckets.clone()),
        )
        .unwrap();
        registry
            .register(Box::new(delta_encode_duration_seconds.clone()))
            .unwrap();

        let delta_decode_duration_seconds = Histogram::with_opts(
            HistogramOpts::new(
                "deltaglider_delta_decode_duration_seconds",
                "Delta decode duration in seconds",
            )
            .buckets(codec_duration_buckets),
        )
        .unwrap();
        registry
            .register(Box::new(delta_decode_duration_seconds.clone()))
            .unwrap();

        let delta_decisions_total = IntCounterVec::new(
            Opts::new(
                "deltaglider_delta_decisions_total",
                "Delta storage decisions by type",
            ),
            &["decision"],
        )
        .unwrap();
        registry
            .register(Box::new(delta_decisions_total.clone()))
            .unwrap();

        // -- Cache --
        let cache_hits_total =
            IntCounter::new("deltaglider_cache_hits_total", "Total reference cache hits").unwrap();
        registry
            .register(Box::new(cache_hits_total.clone()))
            .unwrap();

        let cache_misses_total = IntCounter::new(
            "deltaglider_cache_misses_total",
            "Total reference cache misses",
        )
        .unwrap();
        registry
            .register(Box::new(cache_misses_total.clone()))
            .unwrap();

        let cache_size_bytes = Gauge::new(
            "deltaglider_cache_size_bytes",
            "Current cache size in bytes (updated on scrape)",
        )
        .unwrap();
        registry
            .register(Box::new(cache_size_bytes.clone()))
            .unwrap();

        let cache_entries =
            Gauge::new("deltaglider_cache_entries", "Current cache entry count").unwrap();
        registry.register(Box::new(cache_entries.clone())).unwrap();

        // -- Codec Concurrency --
        let codec_semaphore_available = Gauge::new(
            "deltaglider_codec_semaphore_available",
            "Available codec semaphore permits",
        )
        .unwrap();
        registry
            .register(Box::new(codec_semaphore_available.clone()))
            .unwrap();

        // -- Auth --
        let auth_attempts_total = IntCounterVec::new(
            Opts::new("deltaglider_auth_attempts_total", "Auth attempts by result"),
            &["result"],
        )
        .unwrap();
        registry
            .register(Box::new(auth_attempts_total.clone()))
            .unwrap();

        let auth_failures_total = IntCounterVec::new(
            Opts::new("deltaglider_auth_failures_total", "Auth failures by reason"),
            &["reason"],
        )
        .unwrap();
        registry
            .register(Box::new(auth_failures_total.clone()))
            .unwrap();

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
            codec_semaphore_available,
            auth_attempts_total,
            auth_failures_total,
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

/// Axum middleware that records HTTP request metrics.
pub async fn http_metrics_middleware(
    State(state): State<Arc<AppState>>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let metrics = match &state.metrics {
        Some(m) => m,
        None => return next.run(request).await,
    };

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

    let status = response.status().as_u16().to_string();

    metrics
        .http_requests_total
        .with_label_values(&[&method, &status, operation])
        .inc();
    metrics
        .http_request_duration_seconds
        .with_label_values(&[&method, operation])
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
    let metrics = match &state.metrics {
        Some(m) => m,
        None => {
            return (StatusCode::NOT_FOUND, "Metrics not enabled").into_response();
        }
    };

    // Update on-demand gauges (all O(1) atomic reads)
    let engine = state.engine.load();
    metrics
        .process_peak_rss_bytes
        .set(crate::api::handlers::get_peak_rss_bytes() as f64);
    metrics
        .cache_size_bytes
        .set(engine.cache_weighted_size() as f64);
    metrics.cache_entries.set(engine.cache_entry_count() as f64);
    metrics
        .codec_semaphore_available
        .set(engine.codec_available_permits() as f64);

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
}
