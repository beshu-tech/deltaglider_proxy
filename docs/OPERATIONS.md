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

YAML is the canonical format as of v0.8.0. TOML still loads but emits a deprecation warning on every startup (suppress with `DGP_SILENCE_TOML_DEPRECATION=1`). See [CONFIGURATION.md](CONFIGURATION.md) for the full field reference and [HOWTO_MIGRATE_TO_YAML.md](HOWTO_MIGRATE_TO_YAML.md) for the migration path.

DeltaGlider Proxy loads configuration in this order:

1. `DGP_CONFIG` (explicit file path — returned unconditionally when set)
2. `./deltaglider_proxy.yaml`
3. `./deltaglider_proxy.yml`
4. `./deltaglider_proxy.toml` (deprecated)
5. `/etc/deltaglider_proxy/config.yaml`
6. `/etc/deltaglider_proxy/config.yml`
7. `/etc/deltaglider_proxy/config.toml` (deprecated)

Environment variables (`DGP_*` prefix) override file contents regardless of format. CLI flags override everything:

```bash
./target/release/deltaglider_proxy --config deltaglider_proxy.yaml --listen 0.0.0.0:9000
```

**TOML → YAML migration:**

```bash
deltaglider_proxy config migrate deltaglider_proxy.toml --out deltaglider_proxy.yaml
```

**Offline validation** (wire into CI):

```bash
deltaglider_proxy config lint deltaglider_proxy.yaml
# Exit codes: 0 = valid; 3 = I/O; 4 = parse; 6 = validation.
```

## Admin GUI

![S3 file browser](/_/screenshots/filebrowser.jpg)

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

Authentication endpoints are protected by a per-IP rate limiter. See [RATE_LIMITING.md](RATE_LIMITING.md) for the full model (progressive delay, tiered lockout, IP extraction).

- **100 failed attempts** per **5-minute** rolling window per IP (configurable via `DGP_RATE_LIMIT_MAX_ATTEMPTS` / `DGP_RATE_LIMIT_WINDOW_SECS`).
- After exceeding the limit, the IP is **locked out for 10 minutes** (configurable via `DGP_RATE_LIMIT_LOCKOUT_SECS`).
- Progressive delay (100ms → 5s cap) kicks in before lockout.
- Expired entries are periodically cleaned up to prevent memory growth.
- Applies to admin login endpoints (`/_/api/admin/login`, `/_/api/admin/login-as`, `/_/api/admin/oauth/callback`).

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

Concurrent multipart uploads are limited to prevent resource exhaustion. Default: 1000 concurrent uploads. Override with `DGP_MAX_MULTIPART_UPLOADS` env var.

## Security-related environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `DGP_SESSION_TTL_HOURS` | `4` | Admin session lifetime in hours |
| `DGP_CLOCK_SKEW_SECONDS` | `300` | SigV4 clock skew tolerance in seconds |
| `DGP_REPLAY_WINDOW_SECS` | `2` | SigV4 replay detection window |
| `DGP_MAX_MULTIPART_UPLOADS` | `1000` | Max concurrent multipart uploads |
| `DGP_DEBUG_HEADERS` | `false` | Expose debug/fingerprinting headers |
| `DGP_SECURE_COOKIES` | `true` | Require HTTPS for session cookies |
| `DGP_TRUST_PROXY_HEADERS` | `false` | Trust `X-Forwarded-For` / `X-Real-IP` (set `true` only behind a reverse proxy) |

See [CONFIGURATION.md](CONFIGURATION.md#environment-variable-registry) for the exhaustive list.

---

## Admin API endpoints

The admin GUI and GitOps integrations talk to `/_/api/admin/*`. All mutation routes require a session cookie (issued by `POST /_/api/admin/login`). Session cookies are IP-bound (rejected from a different source IP) with a default 4-hour TTL.

### Authentication & session

| Method | Path | Purpose |
|--------|------|---------|
| `POST` | `/_/api/admin/login` | Bootstrap password → session cookie |
| `POST` | `/_/api/admin/login-as` | Log in as an IAM user (access_key_id + secret_access_key) |
| `POST` | `/_/api/admin/logout` | End the current session |
| `GET` | `/_/api/admin/session` | `{valid: true/false}` |
| `GET` | `/_/api/whoami` | `{mode, version, user, external_providers}` |
| `POST` | `/_/api/admin/recover-db` | Reset the config DB when the bootstrap hash doesn't match (public, rate-limited) |
| `PUT` | `/_/api/admin/password` | Change the bootstrap password (re-encrypts the DB atomically) |

### Config — field-level (legacy GUI forms)

| Method | Path | Purpose |
|--------|------|---------|
| `GET` | `/_/api/admin/config` | Runtime config as JSON (legacy flat shape) |
| `PUT` | `/_/api/admin/config` | Partial JSON update |

### Config — section-level (Wave 1 of the admin UI revamp)

All three section endpoints route through the same `apply_config_transition` helper as field-level PATCH + document-level APPLY, so hot-reload semantics are identical.

| Method | Path | Body | Purpose |
|--------|------|------|---------|
| `GET` | `/_/api/admin/config/section/:name[?format=yaml]` | — | Section slice as JSON (default) or YAML |
| `PUT` | `/_/api/admin/config/section/:name` | JSON Merge Patch (RFC 7396) | Partial section update |
| `POST` | `/_/api/admin/config/section/:name/validate` | same as PUT | Dry-run: returns `{ok, warnings[], diff, requires_restart}` |

`:name` is one of `admission` / `access` / `storage` / `advanced`. Unknown names return 404.

RFC 7396 merge-patch semantics: keys not present in the body are preserved from the current runtime state; `null` deletes a key; objects merge recursively. Secrets are preserved across round-trips (a GET → edit → PUT cycle never clears credentials even though GET redacts them).

### Config — document-level (GitOps)

| Method | Path | Body | Purpose |
|--------|------|------|---------|
| `GET` | `/_/api/admin/config/export[?section=<name>]` | — | Canonical YAML (secrets redacted) |
| `GET` | `/_/api/admin/config/defaults[?section=<name>]` | — | JSON Schema (for YAML LSP + Monaco) |
| `POST` | `/_/api/admin/config/validate` | `{yaml: <doc>}` | Dry-run full-document apply |
| `POST` | `/_/api/admin/config/apply` | `{yaml: <doc>}` | Atomic full-document apply + persist |
| `POST` | `/_/api/admin/config/trace` | `{method, path, query?, authenticated, source_ip?}` | Evaluate a synthetic request against the admission chain |
| `GET` | `/_/api/admin/config/trace?method=&path=&...` | — | Query-param variant (bookmarkable trace URLs) |

Full-document apply returns `{applied, persisted, requires_restart, warnings, persisted_path}`. Persist failure returns HTTP 500 (not 200+warning) so GitOps pipelines can't mistake a half-applied state for a clean success.

Use the CLI wrapper:

```bash
export DGP_BOOTSTRAP_PASSWORD=...
deltaglider_proxy config apply deltaglider_proxy.yaml --server https://proxy.example.com
```

### Backends

| Method | Path | Purpose |
|--------|------|---------|
| `GET` | `/_/api/admin/backends` | List named backends |
| `POST` | `/_/api/admin/backends` | Create; validates S3 creds upfront |
| `DELETE` | `/_/api/admin/backends/:name` | Remove (safety: can't delete the default or in-use backends) |
| `POST` | `/_/api/admin/test-s3` | Test an arbitrary S3 connection without persisting |

### IAM users / groups / OAuth — gated by `iam_mode`

Mutations (`POST`/`PUT`/`DELETE`) on the routes below return `403 { "error": "iam_declarative" }` when `access.iam_mode: declarative`. Read routes (`GET`) stay accessible for diagnostics.

| Method | Path | Purpose |
|--------|------|---------|
| `GET`/`POST` | `/_/api/admin/users` | List / create IAM users |
| `PUT`/`DELETE` | `/_/api/admin/users/:id` | Update / delete user |
| `POST` | `/_/api/admin/users/:id/rotate-keys` | Rotate an IAM user's access keys |
| `GET`/`POST` | `/_/api/admin/groups` | List / create groups |
| `PUT`/`DELETE` | `/_/api/admin/groups/:id` | Update / delete group |
| `POST` | `/_/api/admin/groups/:id/members` | Add a user to a group |
| `DELETE` | `/_/api/admin/groups/:id/members/:user_id` | Remove a user from a group |
| `GET` | `/_/api/admin/policies` | List canned policy templates (public — no session required) |
| `GET`/`POST` | `/_/api/admin/ext-auth/providers` | List / create OAuth/OIDC providers |
| `PUT`/`DELETE` | `/_/api/admin/ext-auth/providers/:id` | Update / delete a provider |
| `POST` | `/_/api/admin/ext-auth/providers/:id/test` | Test an OAuth provider (probes the well-known endpoint) |
| `GET`/`POST` | `/_/api/admin/ext-auth/mappings` | List / create group mapping rules |
| `PUT`/`DELETE` | `/_/api/admin/ext-auth/mappings/:id` | Update / delete a mapping rule |
| `POST` | `/_/api/admin/ext-auth/mappings/preview` | Preview which groups a given identity would be assigned |
| `GET` | `/_/api/admin/ext-auth/identities` | List external identities (read-only; not gated by `iam_mode`) |
| `POST` | `/_/api/admin/ext-auth/sync-memberships` | Re-evaluate mapping rules and sync group memberships |
| `POST` | `/_/api/admin/migrate` | Migrate legacy bootstrap creds into an IAM user |

### IAM backup (encrypted DB export / import)

The admin UI exposes this as **IAM Backup** (distinct from the YAML Import/Export modal — different scope: encrypted DB vs. operator YAML).

| Method | Path | Purpose |
|--------|------|---------|
| `GET` | `/_/api/admin/backup` | Export the full encrypted config DB (IAM users + groups + OAuth + mappings + external identities) — always allowed |
| `POST` | `/_/api/admin/backup` | Import a backup — gated by `iam_mode` (403 when declarative) |

Response from `POST` carries per-resource counters:
`{users_created, users_skipped, groups_created, groups_skipped, memberships_created, external_identities_created, external_identities_skipped}`.

`external_identities` are remapped through the imported user + provider ID maps. Records whose user or provider didn't make it through (e.g. conflicts on access key, skipped for existing rows) are dropped with a WARN log. Legacy backups generated before v0.8.0 that lack a per-user `id` field still round-trip correctly via fallback heuristics (sibling `groups.member_ids` and SQLite autoincrement assumption).

### Usage / diagnostics

| Method | Path | Purpose |
|--------|------|---------|
| `POST` | `/_/api/admin/usage/scan` | Trigger a prefix-size scan |
| `GET` | `/_/api/admin/usage` | Read the cached usage tree |
| `GET` | `/_/api/admin/audit[?limit=N]` | Snapshot the in-memory audit ring, newest first (bounded; default 500 entries, override `DGP_AUDIT_RING_SIZE`). The server still emits every entry via `tracing::info!` — this endpoint is a GUI convenience, not a compliance substitute. |
| `GET`/`PUT`/`DELETE` | `/_/api/admin/session/s3-credentials` | Server-side S3 credential storage for the admin GUI's browse panel |

### Admin GUI keyboard shortcuts (reachable via `?` in the admin pane)

| Key | Action |
|-----|--------|
| `⌘K` / `Ctrl+K` | Open the command palette (fuzzy nav over every admin page + shell actions) |
| `⌘S` / `Ctrl+S` | Apply the current dirty section (no-op + browser default on clean pages) |
| `?` | Open the shortcuts reference modal |
| `Esc` | Close the palette / any open modal |
| `↑` / `↓` + `Enter` | Navigate + run inside the palette |

### OAuth redirect flow (public — no session required)

| Method | Path | Purpose |
|--------|------|---------|
| `GET` | `/_/api/admin/oauth/authorize/:provider` | Kick off the OAuth flow (PKCE, state, nonce) |
| `GET` | `/_/api/admin/oauth/callback` | Provider callback → issue session cookie |

### Operational

Unauthenticated (needed for load balancer probes, Prometheus scrapers):

| Method | Path | Purpose |
|--------|------|---------|
| `GET` | `/_/health` | `{status, peak_rss_bytes, cache_*}` — no version (anti-fingerprinting) |
| `GET` | `/_/metrics` | Prometheus text format |

Session-protected (reveals per-bucket storage sizes):

| Method | Path | Purpose |
|--------|------|---------|
| `GET` | `/_/stats` | Aggregate storage stats (10s server-side cache) |

---

## IAM backup, export/import, and multi-instance sync

Three distinct mechanisms, often confused:

1. **IAM Backup** (`GET/POST /_/api/admin/backup`) — exports/imports the **full encrypted SQLCipher DB** (users, groups, OAuth providers, mapping rules). The GUI calls this from the **IAM Backup** sidebar entry.
2. **YAML Import/Export** (GUI `YamlImportExportModal` → `/_/api/admin/config/export` + `/config/apply`) — the full YAML *config document* (admission chain, backends, bucket policies, advanced knobs). Does **not** include IAM state.
3. **S3-synced IAM DB** (`DGP_CONFIG_SYNC_BUCKET`) — multi-instance replication of the encrypted DB. Uploads after every mutation; readers poll S3 every 5 minutes and download on ETag change. Configurable via `advanced.config_sync_bucket` in YAML or the env var.

These are orthogonal: IAM Backup is manual point-in-time export; S3 sync is automatic live replication; YAML Import/Export manages non-IAM config.

