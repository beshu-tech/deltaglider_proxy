# Rate limits and concurrency

Reference for the proxy's protection layers against overload, abuse, and resource exhaustion. Every limit has a default and an environment-variable override.

## Auth rate limiter

Per-IP brute-force protection for SigV4 authentication and admin login endpoints.

| Setting | Default | Env var |
|---------|---------|---------|
| Max failures before lockout | 100 | `DGP_RATE_LIMIT_MAX_ATTEMPTS` |
| Rolling window | 300 s (5 min) | `DGP_RATE_LIMIT_WINDOW_SECS` |
| Lockout duration | 600 s (10 min) | `DGP_RATE_LIMIT_LOCKOUT_SECS` |

After a lockout expires, the failure counter resets and the IP can authenticate again. Lockout responses are `429 SlowDown`.

### Progressive delay

Failed auth attempts add an artificial delay to responses before the lockout threshold is reached:

| Failures | Delay |
|----------|-------|
| 1–10 | none |
| 11 | 200 ms |
| 12 | 400 ms |
| 13 | 800 ms |
| 14 | 1.6 s |
| 15 | 3.2 s |
| 16+ | 5 s (cap) |

### IP extraction

Rate limiting requires a client IP. The proxy reads `X-Forwarded-For` or `X-Real-IP` headers only when `DGP_TRUST_PROXY_HEADERS=true`; the default is `false`, so direct-to-internet deployments are protected against IP spoofing out of the box. `DGP_TRUST_PROXY_HEADERS=true` is appropriate only behind a trusted reverse proxy (nginx, Caddy, ALB) that injects these headers.

For direct-to-internet deployments without trusted headers, the rate limiter receives no IP and is effectively a no-op for those requests; SigV4 signature verification and the replay cache still apply. The admission chain's `source_ip_list` predicates use axum `ConnectInfo` (wired at startup) and continue to work in the direct case; the rate limiter does not consume `ConnectInfo`.

## Codec semaphore

Limits concurrent xdelta3 encode/decode subprocesses. Delta reconstruction (decode) is CPU-fast but I/O-bound (fetching reference + delta from storage), so the default is generous.

| Setting | Default | Env var |
|---------|---------|---------|
| Max concurrent xdelta3 processes | `num_cpus * 4` (min 16) | `DGP_CODEC_CONCURRENCY` |

Behavior differs by operation:

- **GET (decode)**: waits up to 60 seconds for a codec slot, then returns `503 SlowDown`.
- **PUT (encode)**: fails immediately with `503 SlowDown` when no slot is available, so queued uploads do not hold large request bodies in memory while waiting.

## HTTP concurrency limit

Caps total in-flight HTTP requests across the server. Requests beyond the limit queue until a slot opens or the request timeout fires.

| Setting | Default | Env var |
|---------|---------|---------|
| Max concurrent requests | 1024 | `DGP_MAX_CONCURRENT_REQUESTS` |

## Request timeout

Per-request deadline applied to all S3 API requests; returns HTTP `504 Gateway Timeout` when exceeded. Large delta reconstructions over slow storage links count toward this deadline.

| Setting | Default | Env var |
|---------|---------|---------|
| Request timeout | 300 s | `DGP_REQUEST_TIMEOUT_SECS` |

## Multipart upload limit

Caps concurrent in-progress multipart uploads. Each upload holds part data in memory until completion; the limit bounds memory consumption from abandoned or excessive uploads. Returns `503 SlowDown` when exceeded.

| Setting | Default | Env var |
|---------|---------|---------|
| Max concurrent uploads | 1000 | `DGP_MAX_MULTIPART_UPLOADS` |

## Replay detection cache

Caches SigV4 signatures and rejects duplicates within the replay window. This is independent of `DGP_CLOCK_SKEW_SECONDS`, which governs how far a request timestamp may drift from the server clock during SigV4 verification — a different check.

| Setting | Default | Env var |
|---------|---------|---------|
| Replay window | 2 s | `DGP_REPLAY_WINDOW_SECS` |
| Clock skew tolerance | 300 s | `DGP_CLOCK_SKEW_SECONDS` |
| Max cache entries | 500,000 | — |

A duplicate of a **mutating** request (PUT/POST/DELETE) within the window is rejected with 400. A duplicate of an **idempotent read** (GET/HEAD) is tolerated and served — boto3 emits byte-identical signatures for the same request within one signing second, and replaying a read re-reads the same bytes. Replay rejections are not counted toward the auth-failure lockout. `DGP_REPLAY_WINDOW_SECS=0` disables replay rejection entirely. When the cache exceeds 500K entries, expired signatures are evicted first.

## S3 backend HEAD concurrency

During LIST operations that require per-object metadata, the proxy issues HEAD requests to the upstream S3 backend. These are limited to avoid triggering the backend's own throttling.

| Setting | Default | Configurable |
|---------|---------|--------------|
| Max concurrent HEADs | 50 | No |

## Summary of all env vars

| Env var | Default | Description |
|---------|---------|-------------|
| `DGP_RATE_LIMIT_MAX_ATTEMPTS` | 100 | Auth failures before lockout |
| `DGP_RATE_LIMIT_WINDOW_SECS` | 300 | Rolling window for failure counting |
| `DGP_RATE_LIMIT_LOCKOUT_SECS` | 600 | Lockout duration after max failures |
| `DGP_TRUST_PROXY_HEADERS` | false | Trust `X-Forwarded-For` / `X-Real-IP` for IP extraction (only behind a reverse proxy) |
| `DGP_CODEC_CONCURRENCY` | cpus*4 (min 16) | Max concurrent xdelta3 processes |
| `DGP_MAX_CONCURRENT_REQUESTS` | 1024 | Max in-flight HTTP requests |
| `DGP_REQUEST_TIMEOUT_SECS` | 300 | Per-request timeout |
| `DGP_MAX_MULTIPART_UPLOADS` | 1000 | Max concurrent multipart uploads |
| `DGP_CLOCK_SKEW_SECONDS` | 300 | SigV4 request-timestamp drift tolerance |
| `DGP_REPLAY_WINDOW_SECS` | 2 | SigV4 replay detection window (0 disables) |

## Related

- [Authentication and access](authentication.md) — SigV4 verification and replay-detection semantics
- [Configuration](configuration.md) — full env-var registry
