# Rate Limiting & Throttling

DeltaGlider Proxy has several layers of protection against overload, abuse, and resource exhaustion. All limits have sensible defaults and are configurable via environment variables.

## Auth Rate Limiter

Per-IP brute force protection for SigV4 authentication and admin login endpoints.

| Setting | Default | Env var |
|---------|---------|---------|
| Max failures before lockout | 100 | `DGP_RATE_LIMIT_MAX_ATTEMPTS` |
| Rolling window | 300s (5 min) | `DGP_RATE_LIMIT_WINDOW_SECS` |
| Lockout duration | 600s (10 min) | `DGP_RATE_LIMIT_LOCKOUT_SECS` |

After a lockout expires, the failure counter resets and the IP can authenticate again.

### Progressive delay

Failed auth attempts add an artificial delay to responses, making brute force expensive even before the lockout threshold:

| Failures | Delay |
|----------|-------|
| 1–10 | none |
| 11 | 100ms |
| 12 | 200ms |
| 13 | 400ms |
| 14 | 800ms |
| 15 | 1.6s |
| 16 | 3.2s |
| 17+ | 5s (cap) |

### IP extraction

Rate limiting requires a client IP. The proxy extracts it from `X-Forwarded-For` or `X-Real-IP` headers when `DGP_TRUST_PROXY_HEADERS=true` (the default). Behind a reverse proxy (nginx, Caddy, Coolify, ALB), this is the correct setting.

For direct-to-internet deployments, set `DGP_TRUST_PROXY_HEADERS=false` to prevent IP spoofing — but note that without `ConnectInfo` support (not yet implemented), rate limiting will be disabled since no IP can be determined. SigV4 signature verification still applies regardless.

## Codec Semaphore

Limits the number of concurrent xdelta3 encode/decode subprocesses. Delta reconstruction (decode) is CPU-fast but I/O-bound (fetching reference + delta from storage), so the default is generous.

| Setting | Default | Env var |
|---------|---------|---------|
| Max concurrent xdelta3 processes | `num_cpus * 4` (min 16) | `DGP_CODEC_CONCURRENCY` |

Behavior differs by operation:

- **GET (decode)**: Waits up to 60 seconds for a codec slot. If no slot becomes available, returns 503 SlowDown.
- **PUT (encode)**: Fails immediately with 503 SlowDown if no slot is available. This prevents queuing uploads that hold large request bodies in memory while waiting.

## HTTP Concurrency Limit

Caps the total number of in-flight HTTP requests across the entire server. Requests beyond the limit queue until a slot opens or the request timeout fires.

| Setting | Default | Env var |
|---------|---------|---------|
| Max concurrent requests | 1024 | `DGP_MAX_CONCURRENT_REQUESTS` |

## Request Timeout

Per-request deadline applied to all S3 API requests. Returns HTTP 504 Gateway Timeout when exceeded. Set this high enough to accommodate large delta reconstructions over slow storage links.

| Setting | Default | Env var |
|---------|---------|---------|
| Request timeout | 300s | `DGP_REQUEST_TIMEOUT_SECS` |

## Multipart Upload Limit

Caps concurrent in-progress multipart uploads. Each upload holds part data in memory until completion, so this limit prevents memory exhaustion from abandoned or excessive uploads.

| Setting | Default | Env var |
|---------|---------|---------|
| Max concurrent uploads | 1000 | `DGP_MAX_MULTIPART_UPLOADS` |

Returns 503 SlowDown when exceeded.

## Replay Detection Cache

Prevents replay attacks by caching SigV4 signatures and rejecting duplicates within the clock skew window.

| Setting | Default | Env var |
|---------|---------|---------|
| Clock skew window | 300s | `DGP_CLOCK_SKEW_SECONDS` |
| Max cache entries | 500,000 | — |

When the cache exceeds 500K entries, expired signatures are evicted first. Duplicate signatures within the window are rejected with 403.

## S3 Backend HEAD Concurrency

During LIST operations that require per-object metadata, the proxy issues HEAD requests to the upstream S3 backend. These are rate-limited to avoid triggering S3's own SlowDown throttling.

| Setting | Default | Configurable |
|---------|---------|--------------|
| Max concurrent HEADs | 50 | No |

## Summary of all env vars

| Env var | Default | Description |
|---------|---------|-------------|
| `DGP_RATE_LIMIT_MAX_ATTEMPTS` | 100 | Auth failures before lockout |
| `DGP_RATE_LIMIT_WINDOW_SECS` | 300 | Rolling window for failure counting |
| `DGP_RATE_LIMIT_LOCKOUT_SECS` | 600 | Lockout duration after max failures |
| `DGP_TRUST_PROXY_HEADERS` | true | Trust X-Forwarded-For for IP extraction |
| `DGP_CODEC_CONCURRENCY` | cpus*4 (min 16) | Max concurrent xdelta3 processes |
| `DGP_MAX_CONCURRENT_REQUESTS` | 1024 | Max in-flight HTTP requests |
| `DGP_REQUEST_TIMEOUT_SECS` | 300 | Per-request timeout |
| `DGP_MAX_MULTIPART_UPLOADS` | 1000 | Max concurrent multipart uploads |
| `DGP_CLOCK_SKEW_SECONDS` | 300 | SigV4 timestamp tolerance / replay window |
