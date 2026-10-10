# Rate limits and concurrency

This page lists the limits that protect the proxy against overload, abuse, and resource exhaustion. Every limit has a default and an environment-variable override.

## Auth rate limiter

The auth rate limiter protects SigV4 authentication and the admin login endpoints against brute-force guessing. The admin login endpoints count failures for each client IP address. The S3 API counts failures for each pair of client IP address and access key id: a key that keeps failing at an address is locked out at that address, and the other keys at the same address are not. Behind a reverse proxy every client has the address of the proxy, so a lockout of the whole address would refuse every client because of one client with a wrong secret.

| Setting | Default | Env var |
|---------|---------|---------|
| Max failures before lockout | 100 | `DGP_RATE_LIMIT_MAX_ATTEMPTS` |
| Rolling window | 300 s (5 min) | `DGP_RATE_LIMIT_WINDOW_SECS` |
| Lockout duration | 600 s (10 min) | `DGP_RATE_LIMIT_LOCKOUT_SECS` |

After a lockout expires, the failure counter resets and the IP (or the key at that IP) can authenticate again. A successful request does not reset the counter of its IP address: the counter holds the guesses at every secret that the address tried, and one success proves only one secret. The counter ends when its window ends or when the lockout ends. A successful admin sign-in (the bootstrap password, `login-as`, `recover-db`) clears the per-account counter of the account it proved.

### Per-account lockout

The admin sign-in endpoints also count failures for each account, from every IP address together. The bootstrap password login and `recover-db` count against the account `bootstrap`, and `login-as` counts against the access key id that the request names. An attacker who sends guesses from many IP addresses stays below every per-IP limit, but the per-account count still stops the guesses.

| Setting | Default | Env var |
|---------|---------|---------|
| Max failures for one account before it locks | 10 | `DGP_RATE_LIMIT_ACCOUNT_MAX_ATTEMPTS` |
| Rolling window | 3600 s (1 h) | `DGP_RATE_LIMIT_ACCOUNT_WINDOW_SECS` |
| Lockout duration | 3600 s (1 h) | `DGP_RATE_LIMIT_ACCOUNT_LOCKOUT_SECS` |

A locked account still admits two kinds of caller, so that the attacker cannot lock the operator out. The first is an IP address that signed in to that account successfully within the last 30 days. The second is a direct request from the host itself: its TCP peer is a loopback address and it carries no forwarding header, for example a request through an SSH tunnel. The per-IP limit still applies to both.

Only a failed credential counts toward the lockout. On the S3 API, a failure is a request whose signature the proxy checked and refused: a wrong secret for a known access key, or a signature that is too old. Some requests are refused but not counted: a request with no credentials, a malformed `Authorization` header, an expired presigned link, and an unknown access key. These requests never reach a signature check, so they teach the sender nothing about a secret. If they counted, any anonymous client could lock out every client that shares its IP address, for example all clients behind one load balancer. On a browser form upload (a `POST` with `multipart/form-data`), only a signature that does not match counts. The access key of a form upload is in the form body, which the proxy reads after the lockout check, so the form uploads from one address count together as one key. A read of a public prefix needs no credentials, so the proxy serves it to a locked-out IP too.

A locked-out request gets a response that names the lockout and says how long it lasts. The correct password is refused too while the lockout lasts.

- The admin API sign-in endpoints (`/_/api/admin/login`, `login-as`, the browser-session endpoints, `recover-db`, and a `/_/metrics` bearer token) answer `429 Too Many Requests` with a `Retry-After` header in seconds and the JSON body `{"ok": false, "error": "too_many_attempts", "message": "Too many failed sign-in attempts. Try again in 10 min.", "retry_after_secs": 600}`.
- The single sign-on pages (`/_/api/admin/oauth/authorize/<provider>` and `/_/api/admin/oauth/callback`) are pages that the browser opens, not API requests. So they answer `429 Too Many Requests` with the same `Retry-After` header and an HTML error page that says how long to wait.
- The S3 API answers `503 SlowDown`, because S3 clients know that code, with the same `Retry-After` header and the wait in the error message.

### Progressive delay

Before an IP address (on the S3 API, a key at an IP address) reaches the lockout threshold, each failed auth attempt adds an artificial delay to its own refused response. A request that succeeds is not delayed, so a client that shares an address with a failing client keeps its speed:

| Failures | Delay |
|----------|-------|
| 1 to 10 | none |
| 11 | 200 ms |
| 12 | 400 ms |
| 13 | 800 ms |
| 14 | 1.6 s |
| 15 | 3.2 s |
| 16+ | 5 s (cap) |

### IP extraction

Rate limiting requires a client IP. The proxy reads the `X-Forwarded-For` or `X-Real-IP` headers only when `DGP_TRUST_PROXY_HEADERS=true`. The default is `false`, so in a deployment that faces the internet directly, a client cannot spoof its IP address with these headers. Set `DGP_TRUST_PROXY_HEADERS=true` only behind a trusted reverse proxy (nginx, Caddy, ALB) that injects these headers.

With `DGP_TRUST_PROXY_HEADERS=true`, `DGP_TRUSTED_PROXY_CIDRS` must list the networks of the reverse proxies, and the proxy reads these headers only from a connection that comes from one of those networks. From any other connection it uses the address of the connection. From a trusted proxy, `X-Forwarded-For` wins: the proxy walks the header from right to left and takes the first address that is not in a trusted network, or the leftmost address when every address in the header is trusted. When the request has no `X-Forwarded-For` header, the proxy takes `X-Real-IP`, because nginx often sets only that header. When neither header is present, it takes the address of the connection. A trusted reverse proxy must therefore overwrite any `X-Real-IP` value that the client sends (nginx `proxy_set_header X-Real-IP $remote_addr` does), or else set `X-Forwarded-For`.

The proxy refuses to start when `DGP_TRUST_PROXY_HEADERS=true` and `DGP_TRUSTED_PROXY_CIDRS` is unset or holds no valid network. Without the list, the proxy cannot tell a header that a reverse proxy wrote from a header that the client forged. If the proxy used the first `X-Forwarded-For` address, any client could choose the address that the rate limiter locks out.

> **Failure mode behind a proxy.** If the proxy sits behind a reverse proxy and `DGP_TRUST_PROXY_HEADERS` stays `false`, every request appears to come from the IP address of the reverse proxy. On the S3 API, a client that keeps sending a wrong secret then locks out its access key for every client behind the proxy, and the clients with other keys are not affected. The admin login endpoints count by address only, so failed admin logins lock out the admin logins of every client behind the proxy. Set `DGP_TRUST_PROXY_HEADERS=true` and `DGP_TRUSTED_PROXY_CIDRS` behind any trusted proxy. When you save a config that enables the rate limiter and does not trust the proxy headers, a config advisory flags that combination.

Without trusted headers, the rate limiter keys on the address of the TCP connection. So it always has an IP: in a direct-to-internet deployment that address is the client, and behind a reverse proxy that is not trusted it is the reverse proxy. The admission chain's `source_ip_list` predicates and IAM `aws:SourceIp` conditions use the same client address.

## Codec semaphore

The codec semaphore limits the number of concurrent xdelta3 encode and decode subprocesses. Delta reconstruction (decode) uses little CPU time, but it is I/O-bound because it fetches the reference and the delta from storage. For this reason, the default is generous.

| Setting | Default | Env var |
|---------|---------|---------|
| Max concurrent xdelta3 processes | `num_cpus * 4` (min 16) | `DGP_CODEC_CONCURRENCY` |

Behavior differs by operation:

- **GET (decode)**: waits up to 60 seconds for a codec slot, then returns `503 SlowDown`.
- **PUT (encode)**: fails immediately with `503 SlowDown` when no slot is available, so queued uploads do not hold large request bodies in memory while waiting.

## HTTP concurrency limit

The proxy caps the total number of in-flight S3 API requests across the server. Requests beyond the limit wait in a queue until a slot opens or the request timeout fires, because the wait counts against the request timeout.

With more than one storage backend, the requests to one backend can hold at most a share of the slots (`DGP_BACKEND_SHARE_PERCENT`, 75 % by default). A request to a backend whose share is full gets `503 SlowDown` at once, and S3 clients retry it. A slow backend therefore cannot take every slot, and the requests to the other backends and to the admin UI keep their speed. The same share applies to the spool budget (`DGP_SPOOL_MAX_BYTES`): a request that needs spool space while its backend's share of the spool is full waits for that backend's own spool space, not for the whole budget.

| Setting | Default | Env var |
|---------|---------|---------|
| Max concurrent requests | 1024 | `DGP_MAX_CONCURRENT_REQUESTS` |
| Share of one backend | 75 % | `DGP_BACKEND_SHARE_PERCENT` |

## Request timeout

The proxy applies a deadline to each S3 API request. A request that exceeds the deadline gets HTTP `504 Gateway Timeout`. The time of a large delta reconstruction over a slow storage link counts toward this deadline.

| Setting | Default | Env var |
|---------|---------|---------|
| Request timeout | 300 s | `DGP_REQUEST_TIMEOUT_SECS` |

## Multipart upload limit

The proxy caps the number of concurrent in-progress multipart uploads. The proxy holds the parts of each upload until completion, in memory or in relay files in `DGP_SPOOL_DIR`, so the limit bounds the memory and the spool space that abandoned or excessive uploads can take. A CreateMultipartUpload request past the limit gets `503 SlowDown`.

| Setting | Default | Env var |
|---------|---------|---------|
| Max concurrent uploads | 1000 | `DGP_MAX_MULTIPART_UPLOADS` |

## Replay detection cache

The proxy caches the SigV4 signatures of mutating requests and rejects duplicates within the replay window. `DGP_CLOCK_SKEW_SECONDS` governs how far a request timestamp may drift from the server clock during SigV4 verification. The replay window defaults to that skew tolerance, because a signature outside the skew tolerance already fails verification: with the default, a captured mutating request is refused for its whole valid life.

| Setting | Default | Env var |
|---------|---------|---------|
| Replay window | twice the clock skew tolerance (1800 s) | `DGP_REPLAY_WINDOW_SECS` |
| Clock skew tolerance | 900 s | `DGP_CLOCK_SKEW_SECONDS` |
| Max cache entries | 500,000 | — |

The proxy rejects a duplicate of a mutating request (PUT/POST/DELETE) within the window with 400. The exception is a PUT or DELETE that arrives less than one second after the first copy. Such a request is an SDK retry inside the signing second, so the proxy serves it (see [Authentication and access](authentication.md)). An idempotent read (GET/HEAD) does not enter the cache at all. boto3 emits byte-identical signatures for the same request within one signing second, and a replayed read only re-reads the same bytes. Only a request that succeeds keeps its signature in the cache, so an SDK retry of a failed request is not a replay. Replay rejections do not count toward the auth-failure lockout. `DGP_REPLAY_WINDOW_SECS=0` disables replay rejection entirely. When the cache exceeds 500K entries, the proxy evicts the oldest signatures first. The cache is per instance: behind a load balancer, a replay that reaches another instance is not detected.

## S3 backend HEAD concurrency

During a LIST request that needs per-object metadata, the proxy sends HEAD requests to the upstream S3 backend. The proxy limits these requests so that they do not trigger the backend's own throttling.

| Setting | Default | Configurable |
|---------|---------|--------------|
| Max concurrent HEADs | 10 | No |

## Summary of all env vars

| Env var | Default | Description |
|---------|---------|-------------|
| `DGP_RATE_LIMIT_MAX_ATTEMPTS` | 100 | Auth failures before lockout |
| `DGP_RATE_LIMIT_WINDOW_SECS` | 300 | Rolling window for failure counting |
| `DGP_RATE_LIMIT_LOCKOUT_SECS` | 600 | Lockout duration after max failures |
| `DGP_RATE_LIMIT_ACCOUNT_MAX_ATTEMPTS` | 10 | Failed sign-ins for one account, from any IP, before that account locks |
| `DGP_RATE_LIMIT_ACCOUNT_WINDOW_SECS` | 3600 | Rolling window for the per-account failure count |
| `DGP_RATE_LIMIT_ACCOUNT_LOCKOUT_SECS` | 3600 | Per-account lockout duration |
| `DGP_TRUST_PROXY_HEADERS` | false | Trust `X-Forwarded-For` / `X-Real-IP` for IP extraction (only behind a reverse proxy) |
| `DGP_TRUSTED_PROXY_CIDRS` | unset | Networks of the trusted reverse proxies; required when `DGP_TRUST_PROXY_HEADERS=true` |
| `DGP_CODEC_CONCURRENCY` | cpus*4 (min 16) | Max concurrent xdelta3 processes |
| `DGP_MAX_CONCURRENT_REQUESTS` | 1024 | Max in-flight S3 API requests |
| `DGP_BACKEND_SHARE_PERCENT` | 75 | With more than one backend, the most of the request slots and of the spool budget that the requests to one backend can hold |
| `DGP_REQUEST_TIMEOUT_SECS` | 300 | Per-request timeout |
| `DGP_MAX_MULTIPART_UPLOADS` | 1000 | Max concurrent multipart uploads |
| `DGP_CLOCK_SKEW_SECONDS` | 900 | SigV4 request-timestamp drift tolerance |
| `DGP_REPLAY_WINDOW_SECS` | 2 × `DGP_CLOCK_SKEW_SECONDS` (1800) | SigV4 replay detection window for mutating requests (0 disables) |

## Related

- [Authentication and access](authentication.md): SigV4 verification and replay-detection semantics
- [Configuration](configuration.md): the full env-var registry
