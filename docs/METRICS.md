# Prometheus Metrics

DeltaGlider Proxy exposes a `/metrics` endpoint in Prometheus text format on the admin port (S3 port + 1, default `9001`). Metrics are collected via lightweight atomics on the hot path -- no locks, no sampling, no performance impact.

## Quick start

```bash
# Verify metrics are working
curl -s http://localhost:9001/metrics | head -20

# Validate format (if you have promtool)
curl -s http://localhost:9001/metrics | promtool check metrics
```

## Scrape configuration

### Prometheus

Add to `prometheus.yml`:

```yaml
scrape_configs:
  - job_name: deltaglider
    scrape_interval: 15s
    static_configs:
      - targets: ["localhost:9001"]
```

For multiple instances behind a load balancer, use service discovery or list each target:

```yaml
scrape_configs:
  - job_name: deltaglider
    scrape_interval: 15s
    static_configs:
      - targets:
          - "dgp-1:9001"
          - "dgp-2:9001"
          - "dgp-3:9001"
```

### Docker Compose (Prometheus + Grafana)

Minimal stack to get dashboards running:

```yaml
# docker-compose.monitoring.yml
services:
  prometheus:
    image: prom/prometheus:latest
    volumes:
      - ./prometheus.yml:/etc/prometheus/prometheus.yml
    ports:
      - "9090:9090"

  grafana:
    image: grafana/grafana:latest
    ports:
      - "3000:3000"
    environment:
      - GF_SECURITY_ADMIN_PASSWORD=admin
    volumes:
      - grafana-data:/var/lib/grafana

volumes:
  grafana-data:
```

Start with `docker compose -f docker-compose.monitoring.yml up -d`, then open Grafana at `http://localhost:3000` (admin/admin), add Prometheus as a data source (`http://prometheus:9090`), and import the dashboard JSON below.

## Metrics reference

### Process & Build

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `process_start_time_seconds` | Gauge | -- | Unix timestamp when the process started |
| `deltaglider_build_info` | Gauge | `version`, `backend_type` | Always 1; labels carry build metadata |
| `process_peak_rss_bytes` | Gauge | -- | Peak resident set size (updated on scrape) |
| `process_*` (Linux only) | various | -- | Standard process collector: RSS, CPU seconds, open FDs, virtual memory |

### HTTP Requests

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `deltaglider_http_requests_total` | Counter | `method`, `status`, `operation` | Total requests by method, HTTP status code, and S3 operation |
| `deltaglider_http_request_duration_seconds` | Histogram | `method`, `operation` | Request latency distribution |
| `deltaglider_http_request_size_bytes` | Histogram | `method` | Request body size distribution |
| `deltaglider_http_response_size_bytes` | Histogram | `method` | Response body size distribution |

**`operation` label values** (bounded, derived from method + path):

| Value | Meaning |
|-------|---------|
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

**Histogram buckets:**
- Duration: default Prometheus buckets (0.005s .. 10s)
- Body sizes: exponential `[1KB, 10KB, 100KB, 1MB, 10MB, 100MB]`

### Delta Compression

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `deltaglider_delta_compression_ratio` | Histogram | -- | Ratio distribution (`delta_size / original_size`). Lower is better; 0.1 = 90% savings |
| `deltaglider_delta_bytes_saved_total` | Counter | -- | Cumulative bytes saved by delta compression |
| `deltaglider_delta_encode_duration_seconds` | Histogram | -- | Time spent in xdelta3 encode |
| `deltaglider_delta_decode_duration_seconds` | Histogram | -- | Time spent in xdelta3 decode |
| `deltaglider_delta_decisions_total` | Counter | `decision` | Storage decision counts: `delta`, `passthrough`, or `reference` |

**`decision` label values:**
- `delta` -- stored as a delta patch against the reference baseline
- `passthrough` -- stored as-is (non-eligible file type, or poor compression ratio)
- `reference` -- new reference baseline created for a deltaspace

**Histogram buckets:**
- Codec duration: `[1ms, 5ms, 10ms, 25ms, 50ms, 100ms, 250ms, 500ms, 1s, 2.5s, 5s, 10s, 30s]`
- Compression ratio: `[0.01, 0.05, 0.1, 0.15, 0.2, 0.3, 0.4, 0.5, 0.6, 0.7, 0.8, 0.9, 1.0]`

### Cache

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `deltaglider_cache_hits_total` | Counter | -- | Reference cache hits (cheap Bytes refcount clone) |
| `deltaglider_cache_misses_total` | Counter | -- | Reference cache misses (triggers backend read) |
| `deltaglider_cache_size_bytes` | Gauge | -- | Current weighted cache size in bytes (updated on scrape) |
| `deltaglider_cache_entries` | Gauge | -- | Current number of cached reference entries (updated on scrape) |

### Codec Concurrency

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `deltaglider_codec_semaphore_available` | Gauge | -- | Available xdelta3 subprocess permits (updated on scrape). 0 = all slots busy |

### Auth

| Metric | Type | Labels | Description |
|--------|------|--------|-------------|
| `deltaglider_auth_attempts_total` | Counter | `result` | Auth attempts: `success` or `failure` |
| `deltaglider_auth_failures_total` | Counter | `reason` | Failure breakdown: `missing_header`, `invalid_presigned`, `invalid_signature` |

Auth metrics are only populated when SigV4 authentication is enabled (`DGP_ACCESS_KEY_ID` + `DGP_SECRET_ACCESS_KEY` set).

## Label cardinality

All label sets are bounded to prevent metric explosion:

| Label | Max values |
|-------|------------|
| `method` | ~5 (GET, PUT, HEAD, DELETE, POST) |
| `status` | ~15 HTTP status codes in practice |
| `operation` | 15 (see table above) |
| `decision` | 3 (delta, passthrough, reference) |
| `result` | 2 (success, failure) |
| `reason` | 3 (missing_header, invalid_presigned, invalid_signature) |

No bucket names or object keys appear in labels. No unbounded cardinality.

## Grafana dashboards

### Recommended panels

Below are PromQL expressions for the most useful panels. Import these into Grafana or use them in Explore.

#### Request rate (requests/sec by operation)

```promql
sum by (operation) (rate(deltaglider_http_requests_total[5m]))
```

Panel type: Time series, stacked. Shows which S3 operations dominate.

#### Request latency (p50, p95, p99)

```promql
histogram_quantile(0.50, sum by (le) (rate(deltaglider_http_request_duration_seconds_bucket[5m])))
histogram_quantile(0.95, sum by (le) (rate(deltaglider_http_request_duration_seconds_bucket[5m])))
histogram_quantile(0.99, sum by (le) (rate(deltaglider_http_request_duration_seconds_bucket[5m])))
```

Panel type: Time series with 3 queries (legend: p50/p95/p99). Unit: seconds.

#### Latency by operation (p95)

```promql
histogram_quantile(0.95, sum by (le, operation) (rate(deltaglider_http_request_duration_seconds_bucket[5m])))
```

Panel type: Time series. Useful for spotting slow operations (delta decode on GET vs fast HEAD).

#### Error rate

```promql
sum(rate(deltaglider_http_requests_total{status=~"5.."}[5m]))
  /
sum(rate(deltaglider_http_requests_total[5m]))
```

Panel type: Stat or Time series. Unit: percent (0-1).

#### Delta compression effectiveness

```promql
# Bytes saved per second
rate(deltaglider_delta_bytes_saved_total[5m])

# Cumulative bytes saved
deltaglider_delta_bytes_saved_total

# Average compression ratio over last hour
histogram_quantile(0.50, rate(deltaglider_delta_compression_ratio_bucket[1h]))
```

Panel type: Stat (cumulative bytes saved) + Time series (rate, ratio).

#### Storage decisions breakdown

```promql
sum by (decision) (rate(deltaglider_delta_decisions_total[5m]))
```

Panel type: Pie chart or stacked time series. Shows the mix of delta vs passthrough vs reference.

#### Cache hit ratio

```promql
rate(deltaglider_cache_hits_total[5m])
  /
(rate(deltaglider_cache_hits_total[5m]) + rate(deltaglider_cache_misses_total[5m]))
```

Panel type: Gauge (0-1, format as percent). Target: >90%.

#### Cache utilization

```promql
deltaglider_cache_size_bytes
deltaglider_cache_entries
```

Panel type: Stat. Compare `cache_size_bytes` against configured `DGP_CACHE_MB * 1048576` to see headroom.

#### Codec pressure

```promql
deltaglider_codec_semaphore_available
```

Panel type: Gauge. When this drops to 0, all xdelta3 slots are busy and new encode/decode operations queue. Consider increasing `DGP_CODEC_CONCURRENCY`.

#### Encode/decode latency (p95)

```promql
histogram_quantile(0.95, rate(deltaglider_delta_encode_duration_seconds_bucket[5m]))
histogram_quantile(0.95, rate(deltaglider_delta_decode_duration_seconds_bucket[5m]))
```

Panel type: Time series with 2 queries (legend: encode/decode). Unit: seconds.

#### Auth failure rate

```promql
sum by (reason) (rate(deltaglider_auth_failures_total[5m]))
```

Panel type: Time series. Spike in `invalid_signature` = client misconfiguration. Spike in `missing_header` = unauthenticated client attempting access.

#### Memory usage

```promql
process_peak_rss_bytes
```

Panel type: Stat. Unit: bytes (base 1024). Shows lifetime peak RSS.

#### Uptime

```promql
time() - process_start_time_seconds
```

Panel type: Stat. Unit: duration (seconds). Shows how long the process has been running.

## Alerting rules

Example Prometheus alerting rules (add to your `rules.yml`):

```yaml
groups:
  - name: deltaglider
    rules:
      # High error rate
      - alert: DeltaGliderHighErrorRate
        expr: >
          sum(rate(deltaglider_http_requests_total{status=~"5.."}[5m]))
          / sum(rate(deltaglider_http_requests_total[5m])) > 0.05
        for: 5m
        labels:
          severity: warning
        annotations:
          summary: "DeltaGlider error rate above 5%"

      # Slow requests
      - alert: DeltaGliderSlowRequests
        expr: >
          histogram_quantile(0.95,
            sum by (le) (rate(deltaglider_http_request_duration_seconds_bucket[5m]))
          ) > 2
        for: 10m
        labels:
          severity: warning
        annotations:
          summary: "DeltaGlider p95 latency above 2s"

      # Cache hit ratio too low
      - alert: DeltaGliderLowCacheHitRatio
        expr: >
          rate(deltaglider_cache_hits_total[15m])
          / (rate(deltaglider_cache_hits_total[15m]) + rate(deltaglider_cache_misses_total[15m]))
          < 0.5
        for: 15m
        labels:
          severity: warning
        annotations:
          summary: "Reference cache hit ratio below 50% -- consider increasing DGP_CACHE_MB"

      # Codec fully saturated
      - alert: DeltaGliderCodecSaturated
        expr: deltaglider_codec_semaphore_available == 0
        for: 5m
        labels:
          severity: warning
        annotations:
          summary: "All codec slots busy for 5+ minutes -- consider increasing DGP_CODEC_CONCURRENCY"

      # Auth failures spike
      - alert: DeltaGliderAuthFailureSpike
        expr: sum(rate(deltaglider_auth_failures_total[5m])) > 1
        for: 5m
        labels:
          severity: warning
        annotations:
          summary: "Sustained auth failures (>1/sec for 5 min)"

      # Instance down
      - alert: DeltaGliderDown
        expr: up{job="deltaglider"} == 0
        for: 2m
        labels:
          severity: critical
        annotations:
          summary: "DeltaGlider instance unreachable"
```

## What is NOT in /metrics

The existing `/stats` JSON endpoint (`GET http://localhost:9000/stats`) returns aggregate storage statistics (`total_objects`, `total_original_size`, `total_stored_size`, `savings_percentage`). These are **intentionally excluded** from `/metrics` because they require an O(N) scan of all objects on every call. Prometheus scrapes every 15-30s, which would make this unacceptably expensive. Use `/stats` for on-demand dashboards or one-off checks instead.

## Implementation details

- All counters and histograms use the `prometheus` crate's atomic-based collectors. No mutexes on the hot path.
- Gauges that require state inspection (`cache_size_bytes`, `cache_entries`, `codec_semaphore_available`, `process_peak_rss_bytes`) are updated lazily on each `/metrics` scrape. All are O(1) atomic reads.
- The HTTP metrics middleware runs between `TraceLayer` and the auth middleware in the Axum layer stack, so it captures the full request lifecycle including auth time.
- When auth is disabled, `auth_attempts_total` and `auth_failures_total` remain at zero.
- The `process` feature of the prometheus crate adds standard Linux process metrics automatically. On macOS, only `process_peak_rss_bytes` is available (via `getrusage`).
