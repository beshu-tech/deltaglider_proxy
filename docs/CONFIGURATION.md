# Configuration Reference

DeltaGlider Proxy is configured via a TOML file and/or environment variables. Environment variables take precedence over TOML values.

## Table of Contents

- [Server](#server)
- [Delta Engine](#delta-engine)
- [Storage Backend](#storage-backend)
  - [Filesystem](#filesystem-backend)
  - [S3](#s3-backend)
- [Authentication](#authentication)
- [Security](#security)
  - [Rate Limiting](#rate-limiting)
- [TLS](#tls)
- [Config Sync](#config-sync)
- [Multi-Backend Routing](#multi-backend-routing)
- [Bucket Policies](#bucket-policies)
- [Full Example](#full-example)

---

## Server

Core server settings.

### `listen_addr`

Address and port to bind the HTTP server.

| | |
|---|---|
| **Env var** | `DGP_LISTEN_ADDR` |
| **TOML** | `listen_addr` |
| **Default** | `0.0.0.0:9000` |
| **Hot-reload** | No (restart required) |

```toml
listen_addr = "0.0.0.0:9000"
```
```bash
DGP_LISTEN_ADDR=0.0.0.0:8080
```

### `log_level`

Tracing filter string. Controls log verbosity. Overridden by `RUST_LOG` if set.

| | |
|---|---|
| **Env var** | `DGP_LOG_LEVEL` |
| **TOML** | `log_level` |
| **Default** | `deltaglider_proxy=debug,tower_http=debug` |
| **Hot-reload** | Yes (via admin GUI) |

```toml
log_level = "deltaglider_proxy=info,tower_http=warn"
```
```bash
DGP_LOG_LEVEL=deltaglider_proxy=trace
```

### `request_timeout_secs`

Maximum time in seconds for any single HTTP request. Returns HTTP 504 Gateway Timeout when exceeded.

| | |
|---|---|
| **Env var** | `DGP_REQUEST_TIMEOUT_SECS` |
| **TOML** | N/A (env only) |
| **Default** | `300` (5 minutes) |
| **Hot-reload** | No (restart required) |

```bash
DGP_REQUEST_TIMEOUT_SECS=600
```

### `max_concurrent_requests`

Maximum in-flight HTTP requests. Requests beyond this limit queue until a slot opens or the request timeout fires.

| | |
|---|---|
| **Env var** | `DGP_MAX_CONCURRENT_REQUESTS` |
| **TOML** | N/A (env only) |
| **Default** | `1024` |
| **Hot-reload** | No (restart required) |

```bash
DGP_MAX_CONCURRENT_REQUESTS=2048
```

### `max_multipart_uploads`

Maximum concurrent multipart uploads. Each upload holds part data in memory until completed or aborted.

| | |
|---|---|
| **Env var** | `DGP_MAX_MULTIPART_UPLOADS` |
| **TOML** | N/A (env only) |
| **Default** | `1000` |
| **Hot-reload** | No (restart required) |

```bash
DGP_MAX_MULTIPART_UPLOADS=500
```

### `blocking_threads`

Maximum tokio blocking thread pool size. Controls how many concurrent CPU-bound operations (xdelta3) can run.

| | |
|---|---|
| **Env var** | `DGP_BLOCKING_THREADS` |
| **TOML** | `blocking_threads` |
| **Default** | `512` (tokio default) |
| **Hot-reload** | No (restart required) |

```toml
blocking_threads = 64
```

### `debug_headers`

Expose debug/fingerprinting headers in responses (`x-amz-storage-type`, `x-deltaglider-cache`). Disable in production to prevent server fingerprinting.

| | |
|---|---|
| **Env var** | `DGP_DEBUG_HEADERS` |
| **TOML** | N/A (env only) |
| **Default** | `false` |
| **Hot-reload** | No (restart required) |

```bash
DGP_DEBUG_HEADERS=true
```

### `cors_permissive`

Enable permissive CORS headers for development. Do not use in production.

| | |
|---|---|
| **Env var** | `DGP_CORS_PERMISSIVE` |
| **TOML** | N/A (env only) |
| **Default** | `false` |
| **Hot-reload** | No (restart required) |

### `config`

Path to the TOML configuration file.

| | |
|---|---|
| **Env var** | `DGP_CONFIG` |
| **TOML** | N/A |
| **Default** | Auto-detect (`deltaglider_proxy.toml` in CWD) |

```bash
DGP_CONFIG=/etc/deltaglider_proxy/config.toml
```

---

## Delta Engine

Controls delta compression behavior, caching, and the xdelta3 codec.

### `max_delta_ratio`

Store an object as a delta only if `delta_size / original_size` is below this ratio. Lower values = more aggressive space savings but more compute. Higher values = more files get delta treatment.

| | |
|---|---|
| **Env var** | `DGP_MAX_DELTA_RATIO` |
| **TOML** | `max_delta_ratio` |
| **Default** | `0.75` |
| **Hot-reload** | Yes |

```toml
max_delta_ratio = 0.5
```

A ratio of `0.75` means deltas must save at least 25% space to be kept.

### `max_object_size`

Maximum object size in bytes for delta processing. Objects larger than this are always stored as-is (passthrough). This is an xdelta3 memory constraint.

| | |
|---|---|
| **Env var** | `DGP_MAX_OBJECT_SIZE` |
| **TOML** | `max_object_size` |
| **Default** | `104857600` (100 MB) |
| **Hot-reload** | Yes |

```toml
max_object_size = 209715200  # 200 MB
```

### `cache_size_mb`

In-memory reference cache size in MB. Each active deltaspace needs its reference baseline in cache for fast delta reconstruction. Recommend 1024+ MB for production workloads.

| | |
|---|---|
| **Env var** | `DGP_CACHE_MB` |
| **TOML** | `cache_size_mb` |
| **Default** | `100` |
| **Hot-reload** | No (restart required) |

```toml
cache_size_mb = 2048
```

### `metadata_cache_mb`

In-memory metadata cache size in MB. Caches object metadata from HEAD calls, eliminating redundant S3 HEAD requests. Set to `0` to disable.

| | |
|---|---|
| **Env var** | `DGP_METADATA_CACHE_MB` |
| **TOML** | `metadata_cache_mb` |
| **Default** | `50` |
| **Hot-reload** | No (restart required) |

```toml
metadata_cache_mb = 100
```

### `codec_concurrency`

Maximum concurrent xdelta3 encode/decode subprocess operations. Auto-detected as `num_cpus * 4` (minimum 16). Increase if you have many concurrent delta reconstructions.

| | |
|---|---|
| **Env var** | `DGP_CODEC_CONCURRENCY` |
| **TOML** | `codec_concurrency` |
| **Default** | `num_cpus * 4` (min 16) |
| **Hot-reload** | No (restart required) |

```toml
codec_concurrency = 32
```

### `codec_timeout_secs`

Maximum time in seconds for an xdelta3 subprocess to complete. Hung processes are killed after this timeout.

| | |
|---|---|
| **Env var** | `DGP_CODEC_TIMEOUT_SECS` |
| **TOML** | N/A (env only) |
| **Default** | `60` |
| **Hot-reload** | No (restart required) |

```bash
DGP_CODEC_TIMEOUT_SECS=120
```

---

## Storage Backend

### Filesystem Backend

Store objects on the local filesystem. Activated by setting `DGP_DATA_DIR` or configuring `[backend]` with `type = "filesystem"`.

#### `data_dir`

| | |
|---|---|
| **Env var** | `DGP_DATA_DIR` |
| **TOML** | `backend.path` |
| **Default** | `./data` |
| **Hot-reload** | Yes (triggers engine rebuild) |

```toml
[backend]
type = "filesystem"
path = "/var/lib/deltaglider"
```

### S3 Backend

Store objects in an S3-compatible backend (AWS S3, MinIO, Hetzner Object Storage, etc.). Activated by setting `DGP_S3_ENDPOINT` or configuring `[backend]` with `type = "s3"`.

#### `s3_endpoint`

| | |
|---|---|
| **Env var** | `DGP_S3_ENDPOINT` |
| **TOML** | `backend.endpoint` |
| **Default** | None (uses AWS default) |
| **Hot-reload** | Yes (triggers engine rebuild) |

```toml
[backend]
type = "s3"
endpoint = "https://hel1.your-objectstorage.com"
```

#### `s3_region`

| | |
|---|---|
| **Env var** | `DGP_S3_REGION` |
| **TOML** | `backend.region` |
| **Default** | `us-east-1` |

```toml
[backend]
region = "eu-central-1"
```

#### `s3_path_style`

Use path-style URLs instead of virtual-hosted. Required for MinIO, LocalStack, and most S3-compatible stores.

| | |
|---|---|
| **Env var** | `DGP_S3_PATH_STYLE` |
| **TOML** | `backend.force_path_style` |
| **Default** | `true` |

```toml
[backend]
force_path_style = true
```

#### `backend_access_key_id` / `backend_secret_access_key`

Credentials for the S3 backend storage (not the proxy's SigV4 credentials).

| | |
|---|---|
| **Env var** | `DGP_BE_AWS_ACCESS_KEY_ID` / `DGP_BE_AWS_SECRET_ACCESS_KEY` |
| **TOML** | `backend.access_key_id` / `backend.secret_access_key` |

```toml
[backend]
access_key_id = "AKIAIOSFODNN7EXAMPLE"
secret_access_key = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
```

---

## Authentication

Controls how S3 clients authenticate with the proxy. The proxy **refuses to start** without credentials unless you explicitly set `authentication = "none"`.

### `authentication`

Explicit authentication mode selector. When omitted, the proxy infers the mode from credentials. When no credentials are configured, the proxy requires this field to be set to `"none"` — otherwise it exits with an error.

| | |
|---|---|
| **Env var** | `DGP_AUTHENTICATION` |
| **TOML** | `authentication` |
| **Default** | None (auto-detect from credentials; exits if no credentials) |
| **Hot-reload** | No (restart required) |

| Value | Meaning |
|-------|---------|
| `"none"` | Open access — no SigV4 verification (development only) |
| *(omitted)* | Auto-detect: credentials present → bootstrap/IAM; credentials absent → fatal error |

```toml
# Development only:
authentication = "none"
```

OAuth/OIDC providers are configured via the admin GUI (not TOML) and stored in the encrypted config database. See [Authentication](AUTHENTICATION.md) for setup instructions.

### `access_key_id` / `secret_access_key`

Proxy-level SigV4 credentials. This is the single credential pair used in bootstrap mode.

| | |
|---|---|
| **Env var** | `DGP_ACCESS_KEY_ID` / `DGP_SECRET_ACCESS_KEY` |
| **TOML** | `access_key_id` / `secret_access_key` |
| **Default** | None (proxy refuses to start without `authentication = "none"`) |
| **Hot-reload** | Yes |

```toml
access_key_id = "my-proxy-key"
secret_access_key = "my-proxy-secret"
```

### `bootstrap_password_hash`

Bcrypt hash of the bootstrap password. This single secret:
1. Encrypts the IAM config database (SQLCipher)
2. Signs admin GUI session cookies
3. Gates admin GUI access in bootstrap mode

Auto-generated on first run if not set. Use `--set-bootstrap-password` CLI to change. Supports base64-encoded hashes to avoid `$` escaping in Docker.

| | |
|---|---|
| **Env var** | `DGP_BOOTSTRAP_PASSWORD_HASH` (or legacy `DGP_ADMIN_PASSWORD_HASH`) |
| **TOML** | `bootstrap_password_hash` |
| **Default** | Auto-generated |

```toml
bootstrap_password_hash = "JDJiJDEyJENYbDVPRm84bDg2..."
```

---

## Security

### `trust_proxy_headers`

Trust `X-Forwarded-For` and `X-Real-IP` headers for client IP extraction. Used for rate limiting and `aws:SourceIp` IAM conditions. **Disable if the proxy is exposed directly to the internet** (no reverse proxy), as clients can spoof their IP.

| | |
|---|---|
| **Env var** | `DGP_TRUST_PROXY_HEADERS` |
| **TOML** | N/A (env only) |
| **Default** | `true` |
| **Hot-reload** | No (restart required) |

```bash
DGP_TRUST_PROXY_HEADERS=false
```

### `session_ttl_hours`

Admin GUI session time-to-live in hours. Lower values are more secure; higher values require less frequent re-login.

| | |
|---|---|
| **Env var** | `DGP_SESSION_TTL_HOURS` |
| **TOML** | N/A (env only) |
| **Default** | `4` |
| **Hot-reload** | No (restart required) |

```bash
DGP_SESSION_TTL_HOURS=8
```

### `clock_skew_seconds`

Maximum allowed time difference (in seconds) between client and server clocks for SigV4 signatures. Matches AWS S3 behavior.

| | |
|---|---|
| **Env var** | `DGP_CLOCK_SKEW_SECONDS` |
| **TOML** | N/A (env only) |
| **Default** | `300` (5 minutes) |
| **Hot-reload** | No (restart required) |

```bash
DGP_CLOCK_SKEW_SECONDS=600
```

### `replay_window_secs`

Window in seconds for detecting duplicate SigV4 signatures (replay attack protection). Lower values reduce false positives for rapid sequential requests. Presigned URLs are exempt from replay detection.

| | |
|---|---|
| **Env var** | `DGP_REPLAY_WINDOW_SECS` |
| **TOML** | N/A (env only) |
| **Default** | `2` |
| **Hot-reload** | No (restart required) |

```bash
DGP_REPLAY_WINDOW_SECS=5
```

### `secure_cookies`

Require HTTPS for admin session cookies (`Secure` flag). Disable only for local development over HTTP.

| | |
|---|---|
| **Env var** | `DGP_SECURE_COOKIES` |
| **TOML** | N/A (env only) |
| **Default** | `true` |
| **Hot-reload** | No (restart required) |

```bash
DGP_SECURE_COOKIES=false
```

### Rate Limiting

Brute-force protection for authentication endpoints. See [docs/RATE_LIMITING.md](RATE_LIMITING.md) for the full rate limiting architecture.

#### `rate_limit_max_attempts`

Maximum failed authentication attempts from a single IP before lockout.

| | |
|---|---|
| **Env var** | `DGP_RATE_LIMIT_MAX_ATTEMPTS` |
| **Default** | `100` |

#### `rate_limit_window_secs`

Rolling time window in seconds for counting failed attempts.

| | |
|---|---|
| **Env var** | `DGP_RATE_LIMIT_WINDOW_SECS` |
| **Default** | `300` (5 minutes) |

#### `rate_limit_lockout_secs`

Duration in seconds that a locked-out IP is blocked.

| | |
|---|---|
| **Env var** | `DGP_RATE_LIMIT_LOCKOUT_SECS` |
| **Default** | `600` (10 minutes) |

```bash
DGP_RATE_LIMIT_MAX_ATTEMPTS=50
DGP_RATE_LIMIT_WINDOW_SECS=600
DGP_RATE_LIMIT_LOCKOUT_SECS=1800
```

---

## TLS

Optional TLS termination. When enabled, both the S3 API and admin GUI serve HTTPS.

### `tls.enabled`

| | |
|---|---|
| **Env var** | `DGP_TLS_ENABLED` |
| **TOML** | `tls.enabled` |
| **Default** | `false` |

### `tls.cert_path`

Path to PEM certificate file. If omitted, a self-signed certificate is auto-generated.

| | |
|---|---|
| **Env var** | `DGP_TLS_CERT` |
| **TOML** | `tls.cert_path` |

### `tls.key_path`

Path to PEM private key file.

| | |
|---|---|
| **Env var** | `DGP_TLS_KEY` |
| **TOML** | `tls.key_path` |

```toml
[tls]
enabled = true
cert_path = "/etc/ssl/certs/proxy.pem"
key_path = "/etc/ssl/private/proxy-key.pem"
```

---

## Config Sync

Multi-instance IAM synchronization via S3. When enabled, the encrypted config database is replicated to/from an S3 bucket, allowing multiple proxy instances to share IAM state.

### `config_sync_bucket`

| | |
|---|---|
| **Env var** | `DGP_CONFIG_SYNC_BUCKET` |
| **TOML** | `config_sync_bucket` |
| **Default** | None (disabled) |

```toml
config_sync_bucket = "my-config-bucket"
```

---

## Multi-Backend Routing

Route different buckets to different storage backends. When `[[backends]]` is configured, the legacy `[backend]` section is used as a fallback for buckets without an explicit backend assignment.

```toml
default_backend = "primary"

[[backends]]
name = "primary"
type = "s3"
endpoint = "https://s3.us-east-1.amazonaws.com"
region = "us-east-1"
access_key_id = "AWS_KEY"
secret_access_key = "AWS_SECRET"

[[backends]]
name = "europe"
type = "s3"
endpoint = "https://hel1.your-objectstorage.com"
region = "hel1"
access_key_id = "HETZNER_KEY"
secret_access_key = "HETZNER_SECRET"

[[backends]]
name = "local"
type = "filesystem"
path = "/data/cache"
```

Backends can be added and removed via the admin GUI (**Backends** tab) without restart.

---

## Bucket Policies

Per-bucket overrides for compression, backend routing, aliasing, and public access. All fields are optional — omitted fields inherit global defaults.

```toml
[buckets.releases]
compression = true              # override global compression setting
max_delta_ratio = 0.9           # override global ratio threshold
backend = "europe"              # route to a specific named backend
alias = "prod-releases-2024"    # real bucket name on that backend
public_prefixes = ["builds/", "artifacts/"]  # unauthenticated read access
```

### Fields

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `compression` | bool | global setting | Enable/disable delta compression for this bucket |
| `max_delta_ratio` | float (0-1) | global setting | Delta kept only if `delta_size/original_size < ratio` |
| `backend` | string | default backend | Route to a named backend from `[[backends]]` |
| `alias` | string | same as bucket name | Map virtual bucket name to a real bucket on the backend |
| `public_prefixes` | string array | `[]` (none) | Key prefixes for unauthenticated read-only access |

### Public Prefixes

When `public_prefixes` is set, anonymous users (no SigV4 credentials) can GET, HEAD, and LIST objects under the specified prefixes. Writes always require authentication.

- Use trailing `/` for directory-aligned matching: `"builds/"` matches `builds/v1.zip` but not `buildscripts/`
- Empty string `""` makes the entire bucket public (logged as a warning)
- Prefixes containing `..`, null bytes, or `//` are rejected

Configurable via TOML or the admin GUI (**Backends** → per-bucket policy card → **Public Prefixes**).

---

## Full Example

```toml
listen_addr = "0.0.0.0:9000"

# Delta compression
max_delta_ratio = 0.75
max_object_size = 104857600  # 100 MB
cache_size_mb = 2048
metadata_cache_mb = 100
codec_concurrency = 32

# Logging
log_level = "deltaglider_proxy=info,tower_http=warn"

# SigV4 authentication (bootstrap credentials)
access_key_id = "my-proxy-key"
secret_access_key = "my-proxy-secret"

# Bootstrap password (base64-encoded bcrypt hash)
bootstrap_password_hash = "JDJiJDEyJENYbDVPRm84bDg2..."

# Multi-instance config sync
config_sync_bucket = "my-config-bucket"

# Multi-backend routing
default_backend = "primary"

[[backends]]
name = "primary"
type = "s3"
endpoint = "https://hel1.your-objectstorage.com"
region = "hel1"
force_path_style = true
access_key_id = "HETZNER_KEY"
secret_access_key = "HETZNER_SECRET"

[[backends]]
name = "cold"
type = "s3"
endpoint = "https://s3.us-east-1.amazonaws.com"
region = "us-east-1"
access_key_id = "AWS_KEY"
secret_access_key = "AWS_SECRET"

# Per-bucket policies
[buckets.releases]
backend = "primary"
compression = true
public_prefixes = ["builds/", "artifacts/"]

[buckets.archive]
backend = "cold"
alias = "prod-archive-2024"
compression = false

# TLS (optional)
[tls]
enabled = true
cert_path = "/etc/ssl/certs/proxy.pem"
key_path = "/etc/ssl/private/proxy-key.pem"
```

Equivalent environment variables:

```bash
DGP_LISTEN_ADDR=0.0.0.0:9000
DGP_MAX_DELTA_RATIO=0.75
DGP_MAX_OBJECT_SIZE=104857600
DGP_CACHE_MB=2048
DGP_METADATA_CACHE_MB=100
DGP_CODEC_CONCURRENCY=32
DGP_LOG_LEVEL=deltaglider_proxy=info,tower_http=warn
DGP_ACCESS_KEY_ID=my-proxy-key
DGP_SECRET_ACCESS_KEY=my-proxy-secret
DGP_BOOTSTRAP_PASSWORD_HASH=JDJiJDEyJENYbDVPRm84bDg2...
DGP_CONFIG_SYNC_BUCKET=my-config-bucket
DGP_S3_ENDPOINT=https://hel1.your-objectstorage.com
DGP_S3_REGION=hel1
DGP_S3_PATH_STYLE=true
DGP_BE_AWS_ACCESS_KEY_ID=BACKEND_KEY
DGP_BE_AWS_SECRET_ACCESS_KEY=BACKEND_SECRET
DGP_REQUEST_TIMEOUT_SECS=300
DGP_MAX_CONCURRENT_REQUESTS=1024
DGP_MAX_MULTIPART_UPLOADS=1000
DGP_TRUST_PROXY_HEADERS=true
DGP_SESSION_TTL_HOURS=4
DGP_CLOCK_SKEW_SECONDS=300
DGP_RATE_LIMIT_MAX_ATTEMPTS=100
DGP_RATE_LIMIT_WINDOW_SECS=300
DGP_RATE_LIMIT_LOCKOUT_SECS=600
DGP_REPLAY_WINDOW_SECS=2
DGP_SECURE_COOKIES=true
DGP_TLS_ENABLED=true
DGP_TLS_CERT=/etc/ssl/certs/proxy.pem
DGP_TLS_KEY=/etc/ssl/private/proxy-key.pem
```
