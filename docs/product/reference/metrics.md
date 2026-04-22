# Metrics reference

*Every Prometheus metric the proxy exposes, with labels, types, and bucket boundaries.*

![Storage analytics dashboard](/_/screenshots/analytics.jpg)

`GET /_/metrics` returns Prometheus text format on the same port as the S3 API. Metrics are collected via lock-free atomics on the hot path — no mutexes, no sampling, no performance impact.

For scrape configuration, Grafana panels, and alerting rules, see [Monitoring and alerts](../40-monitoring-and-alerts.md).

## Quick sanity check

```bash
curl -s http://localhost:9000/_/metrics | head -20

# If you have promtool:
curl -s http://localhost:9000/_/metrics | promtool check metrics
```

## Process and build

| Metric | Type | Labels | Description |
|---|---|---|---|
| `process_start_time_seconds` | Gauge | — | Unix timestamp when the process started |
| `deltaglider_build_info` | Gauge | `version`, `backend_type` | Always 1; labels carry build metadata |
| `process_peak_rss_bytes` | Gauge | — | Peak resident set size (updated on scrape) |
| `process_*` (Linux only) | various | — | Standard process collector: RSS, CPU seconds, open FDs, virtual memory |

## HTTP requests

| Metric | Type | Labels | Description |
|---|---|---|---|
| `deltaglider_http_requests_total` | Counter | `method`, `status`, `operation` | Total requests by method, HTTP status code, S3 operation |
| `deltaglider_http_request_duration_seconds` | Histogram | `method`, `operation` | Request latency distribution |
| `deltaglider_http_request_size_bytes` | Histogram | `method` | Request body size distribution |
| `deltaglider_http_response_size_bytes` | Histogram | `method` | Response body size distribution |

### `operation` label values (bounded)

| Value | Meaning |
|---|---|
| `list_buckets` | `GET /` |
| `head_root` | `HEAD /` |
| `list_objects` | `GET /:bucket` |
| `create_bucket` | `PUT /:bucket` |
| `delete_bucket` | `DELETE /:bucket` |
| `head_bucket` | `HEAD /:bucket` |
| `post_bucket` | `POST /:bucket` (batch delete) |
| `get_object` | `GET /:bucket/*key` |
| `put_object` | `PUT /:bucket/*key` |
| `delete_object` | `DELETE /:bucket/*key` |
| `head_object` | `HEAD /:bucket/*key` |
| `post_object` | `POST /:bucket/*key` (multipart) |
| `health` | `GET /health` |
| `stats` | `GET /stats` |
| `metrics` | `GET /metrics` |

### Histogram buckets

- Duration: default Prometheus buckets (0.005s … 10s)
- Body sizes: exponential `[1KB, 10KB, 100KB, 1MB, 10MB, 100MB]`

## Delta compression

| Metric | Type | Labels | Description |
|---|---|---|---|
| `deltaglider_delta_compression_ratio` | Histogram | — | Ratio distribution (`delta_size / original_size`). Lower = better; 0.1 = 90% saved |
| `deltaglider_delta_bytes_saved_total` | Counter | — | Cumulative bytes saved by delta compression |
| `deltaglider_delta_encode_duration_seconds` | Histogram | — | Time spent in xdelta3 encode |
| `deltaglider_delta_decode_duration_seconds` | Histogram | — | Time spent in xdelta3 decode |
| `deltaglider_delta_decisions_total` | Counter | `decision` | Storage decision counts |

### `decision` label values

- `delta` — stored as a delta patch against the reference baseline
- `passthrough` — stored as-is (non-eligible file type, or poor compression ratio)
- `reference` — new reference baseline created for a deltaspace

### Histogram buckets

- Codec duration: `[1ms, 5ms, 10ms, 25ms, 50ms, 100ms, 250ms, 500ms, 1s, 2.5s, 5s, 10s, 30s]`
- Compression ratio: `[0.01, 0.05, 0.1, 0.15, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1.0]`

## Cache

| Metric | Type | Labels | Description |
|---|---|---|---|
| `deltaglider_cache_hits_total` | Counter | — | Reference cache hits (cheap `Bytes` refcount clone) |
| `deltaglider_cache_misses_total` | Counter | — | Reference cache misses (triggers backend read) |
| `deltaglider_cache_size_bytes` | Gauge | — | Current weighted cache size (updated on scrape) |
| `deltaglider_cache_entries` | Gauge | — | Current number of cached reference entries |
| `deltaglider_cache_max_bytes` | Gauge | — | Configured max capacity (constant, set at startup) |
| `deltaglider_cache_utilization_ratio` | Gauge | — | `weighted_size / max_capacity` (0.0–1.0) |
| `deltaglider_cache_miss_rate_ratio` | Gauge | — | `misses / (hits + misses)` since startup (0.0–1.0) |

The ratio gauges are pre-computed so dashboards + alerts don't need PromQL arithmetic:

```promql
deltaglider_cache_utilization_ratio > 0.9   # cache nearly full
deltaglider_cache_miss_rate_ratio > 0.5     # cache thrashing
```

## Codec concurrency

| Metric | Type | Labels | Description |
|---|---|---|---|
| `deltaglider_codec_semaphore_available` | Gauge | — | Available xdelta3 subprocess permits. `0` = all slots busy |

## Auth

| Metric | Type | Labels | Description |
|---|---|---|---|
| `deltaglider_auth_attempts_total` | Counter | `result` | Auth attempts: `success` or `failure` |
| `deltaglider_auth_failures_total` | Counter | `reason` | Failure breakdown: `missing_header`, `invalid_presigned`, `invalid_signature` |

Auth metrics stay at zero when SigV4 is disabled.

## Label cardinality

All label sets are bounded:

| Label | Max values |
|---|---|
| `method` | ~5 (GET, PUT, HEAD, DELETE, POST) |
| `status` | ~15 HTTP status codes in practice |
| `operation` | 15 (see table above) |
| `decision` | 3 (delta, passthrough, reference) |
| `result` | 2 (success, failure) |
| `reason` | 3 (missing_header, invalid_presigned, invalid_signature) |

No bucket names, no object keys in labels. No unbounded cardinality.

## What's NOT in `/metrics`

`/stats` returns aggregate storage statistics (`total_objects`, `total_original_size`, `total_stored_size`, `savings_percentage`, `truncated`). These are intentionally excluded from `/metrics` because computing them requires scanning storage objects. The endpoint has a **10-second server-side cache** and **caps at 1,000 objects** (the `truncated` field signals more exist). Use `/stats` for admin dashboards; use `/metrics` for Prometheus.

## Implementation details

- Counters and histograms use the `prometheus` crate's atomic collectors — no mutex on the hot path.
- Gauges requiring state inspection (`cache_size_bytes`, `codec_semaphore_available`, `process_peak_rss_bytes`) are computed lazily on each scrape via O(1) atomic reads.
- The HTTP metrics middleware sits between `TraceLayer` and auth, so it captures the full request lifecycle including auth time.
- The `process` feature of the prometheus crate adds standard Linux process metrics. On macOS, only `process_peak_rss_bytes` is populated (via `getrusage`).
