# Changelog

## v0.2.0

### Cache Health Observability

Four layers of defense against silent cache degradation:

- **Startup warnings**: `[cache]` log prefix warns when cache is disabled (0 MB) or undersized (<1024 MB)
- **Periodic monitor**: Every 60s, warns on >90% utilization or >50% miss rate
- **Prometheus metrics**: `cache_max_bytes`, `cache_utilization_ratio`, `cache_miss_rate_ratio` — computed on scrape from existing atomic counters
- **Response header**: `x-deltaglider-cache: hit|miss` on delta-reconstructed GETs
- **Health endpoint**: `/health` now includes `cache_size_bytes`, `cache_max_bytes`, `cache_entries`, `cache_utilization_pct`

### Proxy Dashboard

Full Prometheus metrics dashboard in the built-in React UI (`#/metrics`):

- Top KPIs: uptime, peak memory, total requests, storage savings %
- Cache section: utilization gauge, hit rate with color coding, live hits vs misses chart
- Delta compression: encode/decode latency, compression ratio distribution, storage decisions
- HTTP traffic: operation breakdown (bar + donut chart), latency distribution, status codes, live request rate
- Authentication: success/failure counts with failure reason breakdown
- Auto-refresh every 5s, storage stats every 60s

### Correctness Fixes (11 bugs)

- **Codec stderr deadlock**: xdelta3 stderr was piped but only drained after stdout. If xdelta3 writes >64KB to stderr, stdout reader blocks forever. Now drains all 3 pipes concurrently.
- **Filesystem metadata-data split-brain**: Crash between `atomic_write` and `xattr_meta::write_metadata` left files without metadata. Now writes xattr to temp file before rename — atomic visibility.
- **Auth exemption path normalization**: `/health/` (trailing slash) was not matched by the exact-string exemption. Now strips trailing slashes.
- **SigV4 presigned URL case-sensitivity**: `x-amz-signature` (lowercase) was not excluded from canonical query string. Now uses case-insensitive comparison.
- **Stats cache thundering herd**: Lock released before `compute_stats`, so N concurrent requests all scanned storage. Now holds `tokio::sync::Mutex` across the async compute.
- **Multipart double-completion race**: Two concurrent `CompleteMultipartUpload` calls both read parts under read lock, both stored data. Now takes ownership under write lock atomically.
- **Multipart write lock starvation**: Assembly of large uploads held write lock during memcpy of all parts. Now removes upload from map (fast), releases lock, then assembles.
- **AWS chunked truncated payload**: Decoded length mismatch with `x-amz-decoded-content-length` was logged but data stored anyway. Now rejects with 400.
- **Admin config rollback**: Backend config was committed before engine swap. If `DynEngine::new()` failed, config showed new backend but engine was old. Now rolls back on failure.
- **S3 403 misclassification**: All 403 errors mapped to `BucketNotFound`. Now only maps for bucket-level operations; object-level 403 is reported as S3 error.
- **list_objects max_keys=0**: Produced `is_truncated=true` with no continuation token. Now clamps to >= 1.

### DRY & Code Quality

- **`StoreContext` parameter object**: Eliminated 3 `#[allow(clippy::too_many_arguments)]` suppressions from `encode_and_store`, `store_passthrough`, `set_reference_baseline`
- **`with_metrics()` helper**: Collapsed 8 inline `if let Some(m) = &self.metrics` blocks to one-liners
- **`try_acquire_codec()`**: Extracted codec semaphore acquisition with consistent error message
- **`cache_key()`**: Extracted `format!("{}/{}", bucket, deltaspace_id)` used 5 times
- **`delete_delta_idempotent()` / `delete_passthrough_idempotent()`**: Extracted 5 verbose `delete_ignoring_not_found` call sites
- **`paginate_sorted()`**: Extracted duplicated pagination logic from `list_objects`
- **`pipe_stdin_stdout_stderr()`**: Extracted duplicated codec pipe coordination from encode/decode
- **`object_url()`**: Extracted test helper URL construction
- **`parse_env()` / `parse_env_opt()`**: Extracted config env var parsing boilerplate
- **`header_value()`**: Renamed from cryptic `hval()`
- **`_guard`**: Renamed from misleading `_lock` (the Drop impl is load-bearing)
- Trimmed verbose `PERF:` archaeology comments — kept constraints, removed history
- Removed `Option<Arc<Metrics>>` from `AppState` — metrics are always present, eliminated branch per request
- Replaced `format!("%{:02X}")` with `write!()` in SigV4 URI encoding — no heap allocation per encoded byte

### Infrastructure

- `/stats` endpoint: 10s server-side cache, capped at 1,000 objects with `truncated` indicator
- `/health`, `/stats`, `/metrics` exempted from SigV4 authentication
- Demo server exposes `/health`, `/stats`, `/metrics` alongside admin API
- `prefixed_key()` helper in S3 backend eliminates 3x prefix if/else duplication

## v0.1.9

Initial public release with S3-compatible proxy, delta compression, filesystem and S3 backends, SigV4 authentication, multipart uploads, embedded React UI, and Prometheus metrics.
