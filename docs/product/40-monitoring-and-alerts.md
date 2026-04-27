# Monitoring and alerts

*Wire Prometheus + Grafana up to DeltaGlider Proxy and set up the alerts you actually want to be paged on.*

The full metrics catalog lives at [reference/metrics.md](reference/metrics.md). This page is the operational task sheet: how to scrape, what to graph, what to alert on.

## Scrape configuration

### Prometheus

```yaml
# validate
scrape_configs:
  - job_name: deltaglider
    metrics_path: /_/metrics
    scrape_interval: 15s
    static_configs:
      - targets: ["dgp.example.com:9000"]
```

For multiple instances behind a load balancer, use service discovery or list each target directly:

```yaml
scrape_configs:
  - job_name: deltaglider
    metrics_path: /_/metrics
    scrape_interval: 15s
    static_configs:
      - targets:
          - "dgp-1:9000"
          - "dgp-2:9000"
          - "dgp-3:9000"
```

The `/_/metrics` endpoint is exempt from SigV4 auth, so Prometheus doesn't need credentials. Bare `/metrics` is part of the S3-compatible namespace and must not be used for Prometheus scraping.

### Docker Compose starter (Prometheus + Grafana)

```yaml
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

`docker compose up -d`, open `http://localhost:3000` (admin/admin), add Prometheus as a data source at `http://prometheus:9090`, import the panels below.

## Dashboard panels (PromQL)

### Request rate by operation

```promql
sum by (operation) (rate(deltaglider_http_requests_total[5m]))
```

Time series, stacked. Shows which S3 operations dominate.

### Latency p50 / p95 / p99

```promql
histogram_quantile(0.50, sum by (le) (rate(deltaglider_http_request_duration_seconds_bucket[5m])))
histogram_quantile(0.95, sum by (le) (rate(deltaglider_http_request_duration_seconds_bucket[5m])))
histogram_quantile(0.99, sum by (le) (rate(deltaglider_http_request_duration_seconds_bucket[5m])))
```

Three queries on one panel. Unit: seconds.

### Latency by operation (p95)

```promql
histogram_quantile(0.95, sum by (le, operation) (rate(deltaglider_http_request_duration_seconds_bucket[5m])))
```

Useful for spotting slow operations — GET (delta decode) vs HEAD (cache read) have very different profiles.

### Error rate

```promql
sum(rate(deltaglider_http_requests_total{status=~"5.."}[5m]))
  /
sum(rate(deltaglider_http_requests_total[5m]))
```

Stat panel. Unit: percent (0–1).

### Delta compression effectiveness

```promql
# Bytes saved per second
rate(deltaglider_delta_bytes_saved_total[5m])

# Cumulative bytes saved
deltaglider_delta_bytes_saved_total

# p50 compression ratio (lower is better; 0.1 = 90% saved)
histogram_quantile(0.50, rate(deltaglider_delta_compression_ratio_bucket[1h]))
```

### Storage decisions mix

```promql
sum by (decision) (rate(deltaglider_delta_decisions_total[5m]))
```

Pie chart. Shows delta vs passthrough vs reference split.

### Cache hit ratio

```promql
rate(deltaglider_cache_hits_total[5m])
  /
(rate(deltaglider_cache_hits_total[5m]) + rate(deltaglider_cache_misses_total[5m]))
```

Gauge. Target: > 90%.

### Cache headroom

```promql
deltaglider_cache_size_bytes
deltaglider_cache_entries
```

Compare `cache_size_bytes` against `DGP_CACHE_MB * 1048576` to see utilisation.

### Codec pressure

```promql
deltaglider_codec_semaphore_available
```

Gauge. When it drops to 0, xdelta3 permits are all in use and encode/decode queue. Increase `DGP_CODEC_CONCURRENCY`.

### Encode + decode latency (p95)

```promql
histogram_quantile(0.95, rate(deltaglider_delta_encode_duration_seconds_bucket[5m]))
histogram_quantile(0.95, rate(deltaglider_delta_decode_duration_seconds_bucket[5m]))
```

### Auth failure rate

```promql
sum by (reason) (rate(deltaglider_auth_failures_total[5m]))
```

A spike in `invalid_signature` = client misconfiguration. A spike in `missing_header` = unauthenticated probes.

### Uptime

```promql
time() - process_start_time_seconds
```

## Alerting rules

Drop these into your Prometheus `rules.yml`. Tune thresholds to your SLO.

```yaml
groups:
  - name: deltaglider
    rules:
      - alert: DeltaGliderHighErrorRate
        expr: >
          sum(rate(deltaglider_http_requests_total{status=~"5.."}[5m]))
          / sum(rate(deltaglider_http_requests_total[5m])) > 0.05
        for: 5m
        labels: { severity: warning }
        annotations:
          summary: "DeltaGlider error rate above 5%"

      - alert: DeltaGliderSlowRequests
        expr: >
          histogram_quantile(0.95,
            sum by (le) (rate(deltaglider_http_request_duration_seconds_bucket[5m]))
          ) > 2
        for: 10m
        labels: { severity: warning }
        annotations:
          summary: "DeltaGlider p95 latency above 2s"

      - alert: DeltaGliderLowCacheHitRatio
        expr: >
          rate(deltaglider_cache_hits_total[15m])
          / (rate(deltaglider_cache_hits_total[15m]) + rate(deltaglider_cache_misses_total[15m]))
          < 0.5
        for: 15m
        labels: { severity: warning }
        annotations:
          summary: "Reference cache hit ratio < 50% — consider raising DGP_CACHE_MB"

      - alert: DeltaGliderCodecSaturated
        expr: deltaglider_codec_semaphore_available == 0
        for: 5m
        labels: { severity: warning }
        annotations:
          summary: "All codec slots busy for 5+ minutes — consider raising DGP_CODEC_CONCURRENCY"

      - alert: DeltaGliderAuthFailureSpike
        expr: sum(rate(deltaglider_auth_failures_total[5m])) > 1
        for: 5m
        labels: { severity: warning }
        annotations:
          summary: "Sustained auth failures (> 1/s for 5 min)"

      - alert: DeltaGliderDown
        expr: up{job="deltaglider"} == 0
        for: 2m
        labels: { severity: critical }
        annotations:
          summary: "DeltaGlider instance unreachable"
```

## Built-in admin dashboard

The admin UI ships a live monitoring page at `/_/admin/diagnostics/dashboard` — same metrics, auto-refreshed every 5s, with a storage-analytics tab that surfaces per-bucket savings and estimated cost. It's not a substitute for a proper Grafana setup in production (no historical retention, no alerting), but it's enough to answer "is the proxy healthy right now?" without leaving the UI.

## Related

- [Metrics reference](reference/metrics.md) — full catalog, labels, buckets.
- [Production deployment](20-production-deployment.md) — cache sizing, codec concurrency, log levels.
- [Troubleshooting](41-troubleshooting.md) — symptom → metric mapping when something misbehaves.
