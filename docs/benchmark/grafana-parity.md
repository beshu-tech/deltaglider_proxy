# Benchmark metrics vs a typical Grafana stack

A full Grafana deployment for this kind of workload usually combines:

- **Application metrics** — Prometheus scrapes the proxy’s `GET /_/metrics` text.
- **Node / host metrics** — `node_exporter` (CPU, memory, disk I/O, filesystem usage).
- **Container metrics** — cAdvisor or `docker stats` (per-container CPU and memory).

The built-in **production tax benchmark** cannot install Grafana for you, but it can
capture **the same classes of data** in a run folder so you can plot them next to
(or instead of) a live Grafana dashboard.

## What the proxy exports (Grafana: Prometheus datasource)

| Signal | Prometheus / JSON | Notes |
|--------|-------------------|--------|
| Process RSS high-water | `process_peak_rss_bytes` on `/_/metrics` | Also duplicated in `GET /_/health` as `peak_rss_bytes` |
| Reference cache | `deltaglider_cache_size_bytes`, `deltaglider_cache_entries`, miss/hit rates | See “Grafana: cache” panels |
| Codec pressure | `deltaglider_codec_semaphore_available` | Track encode/decode saturation |
| Delta work | `deltaglider_delta_encode_duration_seconds_{sum,count}`, `..._decode_...` | CPU time in delta pipeline |
| Delta storage | `deltaglider_delta_bytes_saved_total` | Cumulative; benchmark takes **per-mode Δ** from paired snapshots |
| HTTP | `deltaglider_http_requests_total{...}` | Per method/status/operation — same as most HTTP Grafana boards |

**Stats API:** `GET /_/stats?bucket=…` can report `total_original_size` vs `total_stored_size`, but the
handler is **session-protected** in the embedded admin router, so the automated benchmark
uses Prometheus + file-footprint instead for unattended runs.

## What the host script adds (Grafana: node + container)

`scripts/benchmark_resources_linux.sh` (invoked via `run --resource-command` or the default
single-VM command) prints JSON with:

| Signal | Source | Grafana analogue |
|--------|--------|------------------|
| Load average | `/proc/loadavg` | `node_load1` / `node_load5` |
| Memory | `MemTotal` / `MemAvailable` in `/proc/meminfo` | `node_memory_*` |
| Disk footprint | `du -sb` on backend roots + full tree-walk size | `node_filesystem_*` (approximate) |
| Container CPU & RAM | `docker stats` on `dgp-bench` | cAdvisor / Docker metrics |

Set `DGP_BENCH_DATA_ROOT` if your compose layout differs from `/root/dgp-single`.

**Linux only** — macOS and Windows do not expose the same `/proc` layout; run the benchmark
VM on Linux (Hetzner image, single-VM smoke, or CI) for host JSON.

## Where each snapshot lands in `results/<run-id>/`

Every benchmark phase writes:

- `before_<mode>_c<n>.prom` — raw Prometheus text  
- `before_<mode>_c<n>.json` — adds parsed **`prom_summary`**, **`health`** JSON, optional **`host_resources`**

At the end of the run, **`resources_rollup.json`** aggregates the **`after_*`** snapshots per mode.
For whole-run CPU/RAM/Disk charts, `run --resource-sample-interval <sec>` writes
**`resource_timeseries.json`** with samples across the full benchmark duration.

## Bringing it into Grafana

1. Run Prometheus with `scrape_configs` pointing at `http://<proxy>:9000/_/metrics`.
2. Import the Grafana dashboards you already use for HTTP histograms + Node exporter.
3. For offline benchmark bundles, point analysts at **`bench_production_tax.py html-report`**, which
   plots throughput/storage and embeds the RSS/Docker/`du` table when rollup data exists.
