# Operations

DeltaGlider Proxy is a single-process S3-compatible HTTP server. Clients speak “normal S3” (mostly), while DeltaGlider Proxy stores data as full objects or delta patches in a backend (filesystem or S3).

## Running

### Filesystem backend (local dev)

```bash
cargo run --release
# or
DGP_DATA_DIR=./data cargo run --release
```

By default DeltaGlider Proxy listens on `0.0.0.0:9000`. Create buckets via the S3 API (`CreateBucket`) or the demo UI.

### S3 backend (MinIO example)

Run MinIO on `:9000`, run DeltaGlider Proxy on a different port (example `:9002`):

```bash
docker compose up -d

DGP_LISTEN_ADDR=127.0.0.1:9002 \
DGP_S3_ENDPOINT=http://127.0.0.1:9000 \
DGP_BE_AWS_ACCESS_KEY_ID=minioadmin \
DGP_BE_AWS_SECRET_ACCESS_KEY=minioadmin \
cargo run --release
```

Point S3 clients at DeltaGlider Proxy (`:9002` in the example), not at MinIO.

## Configuration

DeltaGlider Proxy loads configuration in this order:

1. `DGP_CONFIG` (explicit TOML path)
2. `./deltaglider_proxy.toml`
3. `/etc/deltaglider_proxy/config.toml`
4. Environment variables (see `deltaglider_proxy.toml.example`)

CLI flags override anything loaded from the file/env:

```bash
./target/release/deltaglider_proxy --config deltaglider_proxy.toml --listen 0.0.0.0:9000
```

## Admin GUI

An embedded React-based management UI is served under `/_/` on the same port as the S3 API. For example, if DeltaGlider Proxy listens on `:9000`, the GUI is at `http://localhost:9000/_/`. The `/_/` prefix is safe because `_` is not a valid S3 bucket name character, so there is no conflict with S3 operations.

No extra ports, no extra containers, no manual configuration needed. Features include:

- **S3 File Browser** — navigate buckets, upload, download, preview files (text, images), bulk copy/move/delete, download as ZIP, presigned URL sharing (1h / 24h / 7 days)
- **User Management** — create, edit, delete IAM users with ABAC permissions (Allow/Deny, actions, resources, conditions); key rotation; organize users into groups
- **OAuth/OIDC Configuration** — add identity providers (Google, Okta, Azure AD, any OIDC), configure group mapping rules for automatic permission assignment
- **Backend Management** — add/remove S3 storage backends, configure per-bucket routing and aliasing, per-bucket compression policies, public prefix configuration
- **Monitoring Dashboard** — live Prometheus metrics with charts: request rates, latencies, cache hit rates, status codes, auth events, uptime, memory
- **Storage Analytics** — per-bucket storage savings breakdown, estimated monthly cost savings (configurable provider rates), compression opportunity detection
- **Embedded Documentation** — full-text searchable reference docs with Mermaid diagrams, lightbox image viewer
- **Demo Data Generator** — populate test data for evaluation

Charts auto-refresh every 5s. Storage stats (from `/_/stats`) refresh every 60s.

To build for local development:

```bash
cd demo/s3-browser/ui && npm install && npm run build && cd -
cargo build
```

The Docker build handles the Node.js UI build automatically via a multi-stage Dockerfile.

## Health & Observability

- `GET /health` (or `/_/health`) returns JSON with `status`, `peak_rss_bytes`, and cache state (`cache_size_bytes`, `cache_max_bytes`, `cache_entries`, `cache_utilization_pct`). Version is intentionally excluded from health (anti-fingerprinting) — available via the authenticated `/_/api/whoami` endpoint.
- `GET /stats` (or `/_/stats`) returns aggregate storage statistics with 10s server-side cache, capped at 1,000 objects.
- `GET /metrics` (or `/_/metrics`) returns Prometheus text format with 20+ metrics covering HTTP requests, delta compression, cache, codec concurrency, and auth. See [METRICS.md](METRICS.md) for the full reference.
- Operational endpoints (`/health`, `/stats`, `/metrics`) are exempted from SigV4 authentication — accessible by monitoring systems without S3 credentials. Available on both root paths and under `/_/`.

### Cache health observability

Four layers of defense against silent cache degradation:

1. **Startup warnings** — log lines with `[cache]` prefix:
   - `cache_size_mb == 0`: warns cache is DISABLED
   - `cache_size_mb < 1024`: warns about undersized cache for production
   - Normal: `info!("[cache] Reference cache: {N} MB")`

2. **Periodic monitor** (every 60s) — warns when thresholds are breached:
   - Cache utilization >90%: `[cache] utilization 94% (940/1024 MB, 12 entries)`
   - Miss rate >50% over interval (min 10 ops): `[cache] miss rate 67% (8/12 in last 60s)`

3. **Prometheus metrics** — three derived gauges computed on scrape:
   - `deltaglider_cache_max_bytes` (constant, set at startup)
   - `deltaglider_cache_utilization_ratio` (0.0–1.0)
   - `deltaglider_cache_miss_rate_ratio` (0.0–1.0)

4. **Per-response header** — `x-deltaglider-cache: hit` or `miss` on every delta-reconstructed GET. Passthrough files (no cache involved) omit the header.

### Logging

- Logging uses `tracing`. The log level is resolved in this priority order:
  1. `RUST_LOG` env var (standard tracing-subscriber)
  2. `DGP_LOG_LEVEL` env var (e.g. `DGP_LOG_LEVEL=deltaglider_proxy=warn,tower_http=warn`)
  3. `--verbose` CLI flag (sets trace level)
  4. Default: `deltaglider_proxy=debug,tower_http=debug`

```bash
# Using RUST_LOG
RUST_LOG=deltaglider_proxy=debug,tower_http=info cargo run --release

# Using DGP_LOG_LEVEL
DGP_LOG_LEVEL=deltaglider_proxy=warn cargo run --release
```

- **Runtime log level changes**: The log level can be changed at runtime through the admin GUI (Settings page) without restarting the server. Changes take effect immediately for all new log messages.

## Security model (read this twice)

- **Optional SigV4 authentication**: When `DGP_ACCESS_KEY_ID` and `DGP_SECRET_ACCESS_KEY` are both set, all requests must be signed with valid AWS Signature V4 credentials — either via the `Authorization` header or via presigned URL query parameters. Standard S3 tools (aws-cli, boto3, Terraform) and presigned URLs (`aws s3 presign`) work out of the box. The proxy verifies client signatures, then re-signs upstream requests with separate backend credentials via the AWS SDK. See [AUTHENTICATION.md](AUTHENTICATION.md) for details and the presigned URL flow diagram.
- **Without authentication**: If credentials are not configured, DeltaGlider Proxy accepts all requests. Treat it like an internal service and put it behind network policy / a trusted reverse proxy.
- **Bootstrap password**: A single infrastructure secret that encrypts the IAM config database (SQLCipher), signs admin session cookies, and gates admin GUI access before IAM users exist. Auto-generated on first run (printed to stderr). Set via `DGP_BOOTSTRAP_PASSWORD_HASH` env var or `--set-bootstrap-password` CLI flag. See [AUTHENTICATION.md](AUTHENTICATION.md) for details.
- **IAM mode**: When IAM users exist in the config DB, the proxy switches to per-user credentials with ABAC permissions. Admin GUI access is permission-based (no password needed for IAM admins). Session tokens generated with `OsRng` for cryptographic security. Admin sessions use in-memory tokens with 24-hour TTL, independent of S3 SigV4 auth.
- Keys are validated to reject `..` path segments and backslashes, but you should still avoid exposing the proxy directly to untrusted clients.

## Performance knobs

- `DGP_MAX_OBJECT_SIZE`: hard cutoff for delta processing (and currently for uploads in general).
- `DGP_MAX_DELTA_RATIO`: if `delta_size/original_size` is >= this value, DeltaGlider Proxy stores the object as passthrough (unchanged, with original filename).
- `DGP_CACHE_MB`: LRU cache for reference baselines to avoid re-fetching on hot reads.
- `DGP_METADATA_CACHE_MB`: In-memory metadata cache size (default: 50 MB).

### Metadata cache

DeltaGlider Proxy maintains a moka-based in-memory cache for object metadata (`FileMetadata`). This eliminates HEAD calls for repeated access patterns (e.g., a client that does HEAD then GET, or repeated LISTs on the same prefix).

**What it caches**: The full `FileMetadata` struct for each object — file size, ETag, last-modified, storage type, DeltaGlider-specific tags.

**When it's populated**:
- **PUT**: After successfully storing an object, its metadata is cached.
- **HEAD**: After retrieving metadata from the backend, the result is cached.
- **LIST with metadata=true**: Each object's metadata returned by the backend is cached.

**When it's consulted**:
- **HEAD**: Checked before hitting the storage backend.
- **GET**: Checked to avoid a separate HEAD for metadata enrichment.
- **LIST**: Even without `metadata=true`, the cache is consulted for `file_size` correction (replacing compressed delta sizes with original sizes when available).

**Eviction**:
- **DELETE (exact key)**: The matching cache entry is removed immediately.
- **Prefix delete**: All keys matching the prefix are invalidated.
- **TTL**: Entries expire after 10 minutes. Stale metadata is harmless — worst case triggers one extra backend HEAD.
- **Capacity**: When the cache exceeds the configured byte budget, the least-recently-used entries are evicted.

**Configuration**: Set `DGP_METADATA_CACHE_MB` (env var) or `metadata_cache_mb` (TOML) to adjust the cache size. Default is 50 MB, which holds approximately 125K–150K entries.

**Impact**: Eliminates most HEAD calls for repeated access patterns. Particularly effective for workloads that do HEAD-then-GET sequences, or dashboards that frequently list the same prefixes.

### Usage scanner

The usage scanner (`/_/api/admin/usage`) computes prefix sizes asynchronously in the background. Results are cached for 5 minutes with a 1,000-entry LRU cache. Individual scans are capped at 100,000 objects per prefix to prevent OOM on very large prefixes. The scanner is triggered on-demand by the admin UI when computing folder sizes.

## Security hardening

### Rate limiting

Authentication endpoints are protected by a per-IP rate limiter:

- **5 failed attempts** per **15-minute** rolling window per IP address.
- After exceeding the limit, the IP is **locked out for 30 minutes**.
- Expired entries are periodically cleaned up to prevent memory growth.
- Applies to admin login endpoints (`/_/api/admin/login`, `/_/api/admin/login-as`).

No configuration env var — the limits are hardcoded as security defaults.

### Session hardening

Admin sessions are hardened with several protections:

- **IP binding**: Sessions are bound to the IP address that created them. Requests from a different IP are rejected even with a valid session token.
- **Max concurrent sessions**: Limited to 10 concurrent sessions. When the limit is reached, the oldest session is evicted.
- **Configurable TTL**: Session lifetime defaults to 4 hours (was 24 hours). Override with `DGP_SESSION_TTL_HOURS`.
- **Cryptographic tokens**: Session tokens generated with `OsRng` (OS-level CSPRNG).

### Password quality

Bootstrap password and IAM user passwords are validated:

- **Minimum length**: 12 characters.
- **Maximum length**: 128 characters.
- **Common password rejection**: A built-in blocklist rejects common passwords (e.g., `changeme1234`, `admin1234567`).
- Validated both in the admin API and the `--set-bootstrap-password` CLI flow.

### SigV4 replay detection

SigV4-signed requests include replay detection: duplicate signatures seen within a 5-second window are rejected with an `InvalidArgument` error. This prevents captured requests from being replayed.

### Presigned URL limits

Presigned URL expiry (`X-Amz-Expires`) is capped at **7 days** (604,800 seconds), matching the AWS S3 limit. Requests with a longer expiry are rejected.

### Clock skew validation

SigV4 clock skew is validated with a configurable tolerance:

- Default: **300 seconds** (5 minutes).
- Override with `DGP_CLOCK_SKEW_SECONDS` env var.
- Requests with timestamps outside the tolerance window are rejected with `RequestTimeTooSkewed` (403).

### Security response headers

All responses include security headers:

- `X-Content-Type-Options: nosniff` — prevents MIME type sniffing.
- `X-Frame-Options: DENY` — prevents clickjacking via iframes.
- `Strict-Transport-Security: max-age=31536000; includeSubDomains` — enforces HTTPS (only when TLS is enabled).

### Anti-fingerprinting

Server fingerprinting headers (e.g., `Server`, `x-amz-storage-type`, `x-deltaglider-cache`) are suppressed by default. Enable with `DGP_DEBUG_HEADERS=true` for debugging. This reduces the information available to attackers probing the service.

### Bootstrap password display

The auto-generated bootstrap password is displayed in plaintext **only when stderr is a TTY** (interactive terminal). In containers, CI, and piped output, the plaintext is hidden and only the bcrypt hash is logged. This prevents accidental credential exposure in log aggregators.

### Multipart upload limits

Concurrent multipart uploads are limited to prevent resource exhaustion. Default: 100 concurrent uploads. Override with `DGP_MAX_MULTIPART_UPLOADS` env var.

## Security-related environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `DGP_SESSION_TTL_HOURS` | `4` | Admin session lifetime in hours |
| `DGP_CLOCK_SKEW_SECONDS` | `300` | SigV4 clock skew tolerance in seconds |
| `DGP_MAX_MULTIPART_UPLOADS` | `100` | Max concurrent multipart uploads |
| `DGP_DEBUG_HEADERS` | `false` | Expose debug/fingerprinting headers |
| `DGP_METADATA_CACHE_MB` | `50` | Metadata cache size in MB |

