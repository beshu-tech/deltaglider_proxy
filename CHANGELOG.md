# Changelog

## v0.3.0

### S3-Compatible Endpoint Support

- **Disabled automatic request/response checksums**: AWS SDK for Rust (like boto3 1.36+) adds CRC32/CRC64 checksum headers by default. S3-compatible stores (Hetzner Object Storage, Backblaze B2, some MinIO configs) reject these with BadRequest. Now sets both `request_checksum_calculation` and `response_checksum_validation` to `WhenRequired`. (Port of Python DeltaGlider [6.1.1] fix.)
- **Retry with exponential backoff on PUT**: Hetzner returns transient 400 BadRequest errors (~1-2% of requests) with `connection: close` and no request-id. PUT operations now retry on 400 and 503 with 100/200/400ms backoff (3 retries). Also retries on network/timeout errors.
- **Verbose S3 error logging**: Every S3 error now logs operation, bucket, HTTP status code, `x-amz-request-id`, and full error details for production debugging.

### Unmanaged Object Support (Fixes #3, #4)

Objects that exist on the backend storage but were never stored through the proxy (no DeltaGlider metadata) are now fully accessible:

- **S3 backend**: HEAD, GET, and LIST now return fallback metadata from the S3 HEAD response (size, ETag, Last-Modified) instead of 404
- **Filesystem backend**: HEAD, GET, and LIST now return fallback metadata from filesystem stats (size, mtime) instead of 404
- **HEAD/GET consistency**: Both operations return metadata from the same source, ensuring consistent Content-Length and ETag
- **Delta and reference files**: Fallback metadata also works for delta/reference files without metadata (xattr or S3 headers)
- **Corrupt metadata recovery**: S3 objects with partial/corrupt DG headers now fall back to passthrough instead of hard-failing

### Error Handling Hardening

- **Error discrimination**: Replaced blanket `.ok()` and `map_err(|_| NotFound)` patterns with explicit error matching throughout the engine. `NotFound` → expected (object doesn't exist), `Io` → warn + retry path (concurrent access), other errors → propagate as 500
- **ENOENT classification**: `io_to_storage_error()` now maps file-not-found I/O errors to `StorageError::NotFound` instead of `StorageError::Io`, preventing false 500s for missing files
- **Filesystem xattr errors**: Only fall back to filesystem stats on `NotFound`; permission denied and other I/O errors are now propagated instead of silently swallowed
- **Reference cache errors**: `get_reference_cached()` now discriminates `NotFound` (→ MissingReference) from other storage errors (→ Storage)

### Security

- **SigV4 clock skew validation**: Regular (non-presigned) SigV4 requests now enforce a 15-minute clock skew window, matching AWS S3 behavior. Prevents replay attacks with arbitrarily old timestamps. Returns new `RequestTimeTooSkewed` error (403).
- **Reserved filename validation**: PUT requests for `reference.bin` and `*.delta` keys are rejected with 400 to prevent collision with internal storage files

### Reliability

- **Codec subprocess timeout**: xdelta3 subprocess now has a 5-minute timeout via `try_wait` polling loop. Kills hung processes and returns an error instead of blocking indefinitely.
- **Copy object size check**: `copy_object` now verifies actual data size after retrieval, catching cases where fallback metadata reports `file_size=0` that would bypass the pre-copy size check
- **S3 metadata size validation**: Rejects PUT if DeltaGlider metadata headers exceed S3's 2KB limit, instead of letting the upstream S3 return an opaque 400
- **Config validation**: Warns on `max_delta_ratio` outside [0.0, 1.0] and `max_object_size=0` at startup
- **Cache invalidation ordering**: Reference is now deleted from storage BEFORE cache invalidation, preventing concurrent GET from re-caching a stale reference between invalidation and deletion. Fixed in 3 code paths (passthrough fallback, deltaspace cleanup, legacy migration).

### DRY Cleanup

- **`FileMetadata::fallback()`**: New constructor consolidates 4 duplicate fallback metadata construction sites across S3 and filesystem backends
- **`Engine::validated_key()`**: Extracts the 5x repeated `ObjectKey::parse + validate_object + deltaspace_id` pattern
- **`try_unmanaged_passthrough()`**: Extracted 60-line nested match block from `retrieve_stream()` into a focused helper with flat control flow

### Testing

- 26 new tests (244 total, up from 218): unmanaged object operations, HEAD/GET metadata consistency, reserved filename rejection, copy with unmanaged sources, error discrimination (`io_to_storage_error` unit tests), delta byte-level round-trip integrity, multipart ETag format, user metadata round-trip, external file deletion → 404

### Infrastructure

- Docker build: native ARM64 on Blacksmith runners (no QEMU), cargo-chef for dep caching — ~5x faster builds
- RustSec: updated deps to fix 6 advisories in aws-lc-sys and rustls-webpki

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
