# Configuration

DeltaGlider Proxy is configured via a **YAML** file and/or environment variables (`DGP_*` prefix). Environment variables always take precedence over file contents.

YAML is the only supported format. TOML support was removed in v1.4.1: a `.toml` config — whether set via `DGP_CONFIG` or found on the default search path — makes the proxy fail at startup with `TOML configs are no longer supported (removed in v1.4.1)`. If you still carry a TOML config, run `deltaglider_proxy config migrate` **on v1.4.0** to convert it, then point the server at the YAML file before upgrading. See [How to upgrade the proxy](../how-to/upgrade.md).

## Table of contents

- [YAML layout](#yaml-layout)
- [Shorthands](#shorthands)
- [Config-file search order](#config-file-search-order)
- [Server / Advanced](#server--advanced)
- [Delta engine](#delta-engine)
- [Storage backend](#storage-backend)
  - [Filesystem](#filesystem-backend)
  - [S3](#s3-backend)
- [Access — authentication](#access--authentication)
- [Access — IAM mode](#access--iam-mode)
- [Admission chain](#admission-chain)
- [Security](#security)
  - [Config advisories](#config-advisories)
  - [Rate limiting](#rate-limiting)
- [TLS](#tls)
- [Config sync](#config-sync)
- [Multi-backend routing](#multi-backend-routing)
- [Bucket policies](#bucket-policies)
- [Lifecycle rules](#lifecycle-rules)
- [Event delivery](#event-delivery)
- [Encryption at rest](#encryption-at-rest)
- [CLI subcommands](#cli-subcommands)
- [Full example](#full-example)
- [Environment variable registry](#environment-variable-registry)

---

## YAML layout

The canonical YAML has four optional top-level sections:

```yaml
# deltaglider_proxy.yaml

admission:   # pre-auth request gating (deny / reject / allow-anonymous)
  blocks: [...]

access:      # SigV4 credentials + iam_mode selector
  iam_mode: gui             # gui (default) or declarative
  access_key_id: admin
  secret_access_key: changeme

storage:     # backend(s) + per-bucket overrides
  s3: https://s3.example.com
  buckets: {...}

advanced:    # process-level tunables
  listen_addr: "0.0.0.0:9000"
  cache_size_mb: 2048
  log_level: deltaglider_proxy=info
```

Every section is optional. Fields equal to their default are omitted from canonical exports (`GET /api/admin/config/export`), keeping GitOps diffs minimal.

The flat (pre-Phase-3) shape — root-level `listen_addr:`, `backend:`, etc. — still loads unchanged. Mixing the two shapes in one document is a hard parse error naming the conflicting keys.

The same document is editable from the admin UI. The form keeps section ownership visible, shows YAML paths next to fields, and calls out restart-only environment overrides.

![Access configuration form](/_/screenshots/config-access-form.jpg)

![Storage backend configuration form](/_/screenshots/config-storage-form.jpg)

![Advanced limits configuration form](/_/screenshots/config-limits-form.jpg)

---

## Shorthands

Three operator-authoring shorthands expand at load time into their canonical forms.

### Storage shorthand — single backend

```yaml
storage:
  s3: https://s3.example.com       # expands to backend: { type: s3, endpoint: ... }
  region: eu-central-1             # optional
  access_key_id: admin             # optional
  secret_access_key: changeme      # optional
  force_path_style: true           # optional
```

or

```yaml
storage:
  filesystem: /var/lib/deltaglider
```

Only one of `backend:` / `s3:` / `filesystem:` may be set. Companion fields (`region`, `access_key_id`, etc.) apply only to `s3:`.

### Bucket `public: true`

```yaml
storage:
  buckets:
    docs-site:
      public: true           # shorthand for public_prefixes: [""]
```

The canonical exporter collapses `public_prefixes: [""]` back to `public: true` when unambiguous. The GUI "Public read" toggle maps 1:1 to the YAML.

Mixing `public: true` and a non-empty `public_prefixes` is a hard error.

---

## Config-file search order

`Config::resolve_config_path` returns the first match from:

1. `DGP_CONFIG` env var (returned unconditionally — if set, the path is used even when the file doesn't yet exist).
2. `./deltaglider_proxy.yaml`
3. `./deltaglider_proxy.yml`
4. `./deltaglider_proxy.toml` (tripwire — startup fails)
5. `/etc/deltaglider_proxy/config.yaml`
6. `/etc/deltaglider_proxy/config.yml`
7. `/etc/deltaglider_proxy/config.toml` (tripwire — startup fails)

The `.toml` entries are tripwires, not loadable formats: a leftover TOML config matched by the search (with no YAML earlier in the order) stops startup with an actionable error rather than being silently ignored.

CLI flags (`--config <path>`, `--listen <addr>`) take precedence over all of the above; env vars take precedence over file contents.

---

## Server / Advanced

Process-level knobs. In sectioned YAML these live under `advanced:`.

### `listen_addr`

HTTP listen address.

| | |
|---|---|
| **Env var** | `DGP_LISTEN_ADDR` |
| **YAML** | `advanced.listen_addr` (sectioned) or root `listen_addr:` (flat) |
| **Default** | `0.0.0.0:9000` |
| **Hot-reload** | No (restart required) |

```yaml
advanced:
  listen_addr: "0.0.0.0:8080"
```

### `log_level`

Tracing filter string (`tracing-subscriber` syntax). Overridden by `RUST_LOG` if set. Changeable at runtime via the admin GUI (Settings → System → Logging card), which hot-reloads the filter through the apply pipeline.

Resolution order at startup: `RUST_LOG` > `DGP_LOG_LEVEL` > `advanced.log_level` in file > `--verbose` CLI flag (sets `trace`) > default.

| | |
|---|---|
| **Env var** | `DGP_LOG_LEVEL` |
| **YAML** | `advanced.log_level` |
| **Default** | `deltaglider_proxy=debug,tower_http=debug` |
| **Hot-reload** | Yes (via admin GUI or `config apply`) |

```yaml
advanced:
  log_level: deltaglider_proxy=info,tower_http=warn
```

#### Structured logs and the in-GUI log ring

Three env-only knobs control log output and the admin **System logs** viewer (see [View live logs](../how-to/view-live-logs.md)):

| Env var | Default | Effect |
|---|---|---|
| `DGP_LOG_FORMAT` | `text` | `json` emits one JSON object per stdout line — `jq`-greppable by client IP, bucket, action. Startup-only. |
| `DGP_LOG_RING_SIZE` | `2000` | Capacity of the in-memory operational-log ring behind the admin Logs viewer. |
| `DGP_LOG_RING_LEVEL` | `info` | Minimum severity captured into the ring/live-tail stream, independent of `DGP_LOG_LEVEL`. |

The ring is per-instance, in-memory, and bounded — a triage convenience. Point a log shipper at the `DGP_LOG_FORMAT=json` stdout stream for retention and aggregation.

### `request_timeout_secs`

Per-request deadline (HTTP 504 when exceeded).

| | |
|---|---|
| **Env var** | `DGP_REQUEST_TIMEOUT_SECS` |
| **Default** | `300` (5 minutes) |
| **Hot-reload** | No |

### `max_concurrent_requests`

Global tower `ConcurrencyLimit`. Requests beyond this queue.

| | |
|---|---|
| **Env var** | `DGP_MAX_CONCURRENT_REQUESTS` |
| **Default** | `1024` |
| **Hot-reload** | No |

### `max_multipart_uploads`

Concurrent multipart uploads cap. Each upload holds part data in memory.

| | |
|---|---|
| **Env var** | `DGP_MAX_MULTIPART_UPLOADS` |
| **Default** | `1000` |
| **Hot-reload** | No |

### `blocking_threads`

Tokio blocking thread-pool size. Controls how many concurrent CPU-bound ops (xdelta3 subprocesses) can run.

| | |
|---|---|
| **Env var** | `DGP_BLOCKING_THREADS` |
| **YAML** | `advanced.blocking_threads` |
| **Default** | tokio default (512) |
| **Hot-reload** | No |

### `debug_headers`

Expose debug/fingerprinting headers (`x-amz-storage-type`, `x-deltaglider-cache`). Disable in production to prevent server fingerprinting.

| | |
|---|---|
| **Env var** | `DGP_DEBUG_HEADERS` |
| **Default** | `false` |
| **Hot-reload** | No |

### `cors_permissive`

Enable permissive CORS for cross-origin admin access (dev only — opens the door to CSRF against session-cookie endpoints).

| | |
|---|---|
| **Env var** | `DGP_CORS_PERMISSIVE` |
| **Default** | `false` |
| **Hot-reload** | No |

### `config`

Path to the config file.

| | |
|---|---|
| **Env var** | `DGP_CONFIG` |
| **Default** | Auto-detect (search list above) |

When `DGP_CONFIG` is set, the path is returned unconditionally — a missing file there is NOT silently replaced by the default search list. This prevents the admin API from persisting to a CWD-relative file the operator never asked for.

---

## Delta engine

### `max_delta_ratio`

Store an object as a delta only if `delta_size / original_size` is below this ratio. Lower = more aggressive savings; higher = more files kept as deltas.

| | |
|---|---|
| **Env var** | `DGP_MAX_DELTA_RATIO` |
| **YAML** | `advanced.max_delta_ratio` |
| **Default** | `0.75` |
| **Hot-reload** | Yes |

### `max_object_size`

Maximum object size in bytes. Enforced as the HTTP request body limit, so it caps uploads for both delta and passthrough objects; it is also the per-object ceiling for delta processing (xdelta3 memory constraint) and sizes the multipart upload budget. `0` rejects all uploads (startup warning).

| | |
|---|---|
| **Env var** | `DGP_MAX_OBJECT_SIZE` |
| **Default** | `104857600` (100 MB) |
| **Hot-reload** | Yes |

### `cache_size_mb`

In-memory reference cache size in MB. **Recommend 1024+ MB for production.** Undersized caches (<1024 MB) emit a startup warning.

| | |
|---|---|
| **Env var** | `DGP_CACHE_MB` |
| **YAML** | `advanced.cache_size_mb` |
| **Default** | `100` |
| **Hot-reload** | No |

### `metadata_cache_mb`

In-memory `FileMetadata` cache size in MB. Set to `0` to disable. Budget: ~125K-150K entries at 50 MB. 10-minute TTL.

| | |
|---|---|
| **Env var** | `DGP_METADATA_CACHE_MB` |
| **YAML** | `advanced.metadata_cache_mb` |
| **Default** | `50` |
| **Hot-reload** | No |

### `codec_concurrency`

Maximum concurrent xdelta3 subprocesses. Auto-detected as `num_cpus * 4` (min 16).

| | |
|---|---|
| **Env var** | `DGP_CODEC_CONCURRENCY` |
| **YAML** | `advanced.codec_concurrency` |
| **Default** | `num_cpus * 4` (min 16) |
| **Hot-reload** | No |

### `codec_timeout_secs`

Maximum time for an xdelta3 subprocess. Hung processes are killed after this.

| | |
|---|---|
| **Env var** | `DGP_CODEC_TIMEOUT_SECS` |
| **Default** | `60` |
| **Hot-reload** | No |

---

## Storage backend

### Filesystem backend

Local filesystem. Activated by setting `DGP_DATA_DIR` or a `backend:` block with `type = "filesystem"`.

#### `data_dir`

| | |
|---|---|
| **Env var** | `DGP_DATA_DIR` |
| **YAML (shorthand)** | `storage.filesystem: <path>` |
| **YAML (canonical)** | `storage.backend.path` |
| **Default** | `./data` |
| **Hot-reload** | Yes (triggers engine rebuild) |

```yaml
# Shorthand
storage:
  filesystem: /var/lib/deltaglider

# Canonical (equivalent)
storage:
  backend:
    type: filesystem
    path: /var/lib/deltaglider
```

Paths containing `..` components are rejected at load time.

### S3 backend

AWS S3 / MinIO / Hetzner / Backblaze / any S3-compatible service. Activated by setting `DGP_S3_ENDPOINT` or a `backend:` block with `type = "s3"`.

#### `endpoint` / `region` / `force_path_style` / `access_key_id` / `secret_access_key`

| Field | Env var | YAML shorthand | YAML canonical | Default |
|-------|---------|----------------|----------------|---------|
| endpoint | `DGP_S3_ENDPOINT` | `storage.s3: <url>` | `storage.backend.endpoint` | — (AWS default) |
| region | `DGP_S3_REGION` | `storage.region` | `storage.backend.region` | `us-east-1` |
| force_path_style | `DGP_S3_PATH_STYLE` | `storage.force_path_style` | `storage.backend.force_path_style` | `true` |
| access_key_id | `DGP_BE_AWS_ACCESS_KEY_ID` | `storage.access_key_id` | `storage.backend.access_key_id` | — |
| secret_access_key | `DGP_BE_AWS_SECRET_ACCESS_KEY` | `storage.secret_access_key` | `storage.backend.secret_access_key` | — |

```yaml
# Shorthand
storage:
  s3: https://hel1.your-objectstorage.com
  region: hel1
  access_key_id: AKIAIOSFODNN7EXAMPLE
  secret_access_key: wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY

# Canonical
storage:
  backend:
    type: s3
    endpoint: https://hel1.your-objectstorage.com
    region: hel1
    force_path_style: true
    access_key_id: AKIAIOSFODNN7EXAMPLE
    secret_access_key: wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY
```

Endpoint URLs must start with `http://` or `https://` (scheme-less values rejected at load time).

---

## Access — authentication

The proxy **refuses to start** without credentials unless you set `authentication = "none"`.

### `authentication`

Explicit auth-mode selector. Absent = auto-detect from credentials; `"none"` = open access (dev only).

| | |
|---|---|
| **Env var** | `DGP_AUTHENTICATION` |
| **YAML** | `access.authentication` |
| **Default** | — (auto-detect; **fatal error** if absent AND no credentials) |
| **Hot-reload** | No |

### `access_key_id` / `secret_access_key`

Proxy-level SigV4 credentials (the "bootstrap admin" credential pair).

| | |
|---|---|
| **Env vars** | `DGP_ACCESS_KEY_ID` / `DGP_SECRET_ACCESS_KEY` |
| **YAML** | `access.access_key_id` / `access.secret_access_key` |
| **Default** | None |
| **Hot-reload** | Yes |

```yaml
access:
  access_key_id: admin
  secret_access_key: changeme
```

### `bootstrap_password_hash`

Bcrypt hash of the bootstrap password (encrypts the IAM config DB, signs session cookies, gates admin GUI access in bootstrap mode). Auto-generated on first run. Accepts base64-encoded hashes to avoid `$` escaping in Docker/env vars.

| | |
|---|---|
| **Env var** | `DGP_BOOTSTRAP_PASSWORD_HASH` (legacy alias: `DGP_ADMIN_PASSWORD_HASH`) |
| **YAML** | `advanced.bootstrap_password_hash` (treated as an **infra secret** — stripped by canonical exports) |
| **Default** | Auto-generated on first run |

### `DGP_BOOTSTRAP_PASSWORD`

Plaintext bootstrap password for the `config apply` / `admission trace` admin CLI commands (they authenticate via this env var; argv is avoided because it leaks via `ps`). Not read by the server itself.

| | |
|---|---|
| **Env var** | `DGP_BOOTSTRAP_PASSWORD` |
| **Consumer** | Admin CLI (`deltaglider_proxy config apply`, `... admission trace`) |

---

## Access — IAM mode

The `access.iam_mode` YAML selector controls where IAM state (users, groups, OAuth providers, mapping rules) lives. Orthogonal to the `authentication` selector.

| Mode | Meaning |
|------|---------|
| `gui` *(default)* | Encrypted SQLCipher DB is source of truth. Admin GUI + admin API mutate it. YAML `access.*` carries only the legacy SigV4 pair + `authentication` selector. |
| `declarative` | YAML `access.iam_users`, `iam_groups`, `auth_providers`, and `group_mapping_rules` are authoritative. Admin API IAM mutation routes (`POST/PUT/PATCH/DELETE` on `/users`, `/groups`, `/ext-auth/*`, `/migrate`, backup import) return `403 { "error": "iam_declarative" }`. Read routes stay accessible. |

```yaml
access:
  iam_mode: declarative
```

Mode transitions are audit-logged at `warn` level on the `deltaglider_proxy::config` target. In declarative mode, every `/config/apply` or section-PUT on `access` runs a dry validation + diff, then reconciles the encrypted config DB to YAML in one SQLite transaction. Creates, updates, and deletes emit `iam_reconcile_*` audit entries.

The initial `gui → declarative` flip is guarded: if YAML contains no users or groups while the DB is non-empty, apply fails instead of wiping IAM by accident. To seed GitOps YAML from an existing DB, use `GET /_/api/admin/config/declarative-iam-export`; see [Declarative IAM](declarative-iam.md) for the full workflow.

---

## Admission chain

Operator-authored pre-auth request gating. Blocks are evaluated top-to-bottom; first match wins. Operator blocks fire *before* synthesized public-prefix blocks derived from `storage.buckets[*].public_prefixes`.

```yaml
admission:
  blocks:
    - name: deny-known-bad-ips
      match:
        source_ip_list:
          - "198.51.100.17"
          - "198.51.100.0/24"
      action: deny

    - name: maintenance-mode
      match: {}              # empty = match every request
      action:
        type: reject
        status: 503
        message: "Planned maintenance — back at 18:00 UTC."

    - name: allow-public-zips
      match:
        method: [GET, HEAD]
        bucket: releases
        path_glob: "*.zip"
      action: allow-anonymous
```

### Block fields

| Field | Type | Notes |
|-------|------|-------|
| `name` | string (required) | 1-128 chars, `[A-Za-z0-9_:.-]`. Must be unique across the chain. `public-prefix:*` is reserved for synthesized blocks. |
| `match` | object (default `{}`) | AND-combined predicates. Empty `{}` fires on every request. |
| `match.method` | `[string]` | HTTP methods: `GET` `HEAD` `PUT` `POST` `DELETE` `PATCH` `OPTIONS`. Case-insensitive on parse. |
| `match.source_ip` | IP | Exact match. Mutually exclusive with `source_ip_list`. |
| `match.source_ip_list` | `[IP \| CIDR]` | Accepts bare IPs (promoted to `/32` or `/128`) and CIDRs. Cap: 4096 entries. |
| `match.bucket` | string | Target bucket (lowercased on parse). |
| `match.path_glob` | string | Glob against the full key: `*.zip`, `releases/**`, `docs/readme.md`. |
| `match.authenticated` | bool | `true` = only authenticated; `false` = only anonymous; absent = either. |
| `match.config_flag` | string | Named flag. Registry is not yet live — `maintenance_mode` is recognised but always evaluates false; a warning fires at chain-build time. |
| `action` | string \| object (required) | Simple: `allow-anonymous`, `deny`, `continue`. Tagged: `{ type: reject, status: <4xx\|5xx>, message?: <string> }`. |

`continue` is an explicit terminal that falls through to authentication — useful as the final block for diagnostic visibility in trace output.

### Round-trip

Operator-authored `source_ip_list` entries round-trip verbatim (bare IPs stay bare, CIDRs stay CIDRs) so GitOps diffs don't flip on every apply.

The admin GUI's Admission page (`/_/admin/access/admission`) is the authoring surface. Synthesized `public-prefix:*` blocks appear read-only below the operator list; edit them via Storage → Buckets instead.

---

## Security

### `trust_proxy_headers`

Trust `X-Forwarded-For` / `X-Real-IP` for rate limiting and `aws:SourceIp` IAM conditions. **Disable** if the proxy is internet-facing without a reverse proxy.

| | |
|---|---|
| **Env var** | `DGP_TRUST_PROXY_HEADERS` |
| **Default** | `false` (secure-by-default) |
| **Hot-reload** | No |

**Production-critical behind a reverse proxy.** If the proxy sits behind Coolify, Traefik, nginx, Caddy, or an ALB and this stays `false`, every request appears to come from the proxy's own IP — so all clients collapse onto a **single shared rate-limit bucket**, and one busy client can lock out everyone else with `503 SlowDown`. Set it to `true` whenever a trusted reverse proxy injects `X-Forwarded-For` / `X-Real-IP`. The save-time config advisories (see [Config advisories](#config-advisories)) flag the rate-limit-on + trust-off combination for you. Leave it `false` only when the proxy is directly internet-facing (otherwise a client could spoof the headers).

### `session_ttl_hours`

Admin GUI session TTL.

| | |
|---|---|
| **Env var** | `DGP_SESSION_TTL_HOURS` |
| **Default** | `4` |
| **Hot-reload** | No |

### `clock_skew_seconds`

SigV4 clock skew tolerance.

| | |
|---|---|
| **Env var** | `DGP_CLOCK_SKEW_SECONDS` |
| **Default** | `300` (5 min) |
| **Hot-reload** | No |

### `replay_window_secs`

SigV4 replay detection window. A request whose signature was already seen within this many seconds is treated as a replay.

- **Mutating methods (PUT/POST/DELETE/…)** are rejected with `400 Request replay detected`.
- **Idempotent reads (GET/HEAD)** are *tolerated*: a duplicate within the window is served normally rather than rejected. This is deliberate — boto3/botocore emit **byte-identical SigV4 signatures** for the same request issued (or auto-retried) within one signing second, because SigV4 timestamps have 1-second granularity. A replayed read just re-reads the same bytes, so there is no double-effect to guard against. The signature still stays in the cache, so a captured read signature can't be replayed past the window.
- **Presigned URLs** are exempt entirely (they are designed to be reused for their whole expiry).
- A replay rejection is **not** an authentication failure — the signature is valid — so it is audited as `replay_rejected` and does **not** count toward the per-IP brute-force lockout.

Set `DGP_REPLAY_WINDOW_SECS=0` to disable replay rejection entirely (the window never matches). Useful in CI, or as an escape hatch if a client clusters mutations tighter than the default tolerates.

| | |
|---|---|
| **Env var** | `DGP_REPLAY_WINDOW_SECS` |
| **Default** | `2` |
| **Hot-reload** | No |

### `secure_cookies`

Require HTTPS for admin session cookies (`Secure` flag).

| | |
|---|---|
| **Env var** | `DGP_SECURE_COOKIES` |
| **Default** | `true` |
| **Hot-reload** | No |

### Config advisories

At save time — in the admin **Apply** dialog and in `config apply` / `config lint` — the proxy runs a set of cross-field checks and surfaces warnings for combinations that are individually valid but suspicious together. They never block a save; they flag footguns before they reach production. Current rules:

| Advisory | Fires when | Why it matters |
|---|---|---|
| Shared rate-limit bucket | Rate limiting is enabled but `trust_proxy_headers` is `false` | Behind a reverse proxy, every client collapses onto the proxy's IP and one shared bucket — one client can lock out all others. |
| Stale IAM template | A permission resource uses a bare `${username}` instead of `${iam:username}` | The bare form is not substituted, so the rule matches nothing and silently denies the user. |
| Frozen bucket quota | A bucket's `quota_bytes` is `0` | A zero quota rejects all writes to that bucket. |
| Redundant public prefix | `public_prefixes` are set while `authentication: none` | Auth is already open, so the public-prefix rules add nothing. |

### Rate limiting

Per-IP brute-force protection for auth endpoints. See [Rate limits and concurrency](rate-limits.md) for the full model.

| Setting | Env var | Default |
|---------|---------|---------|
| Max failures before lockout | `DGP_RATE_LIMIT_MAX_ATTEMPTS` | `100` |
| Rolling window | `DGP_RATE_LIMIT_WINDOW_SECS` | `300` (5 min) |
| Lockout duration | `DGP_RATE_LIMIT_LOCKOUT_SECS` | `600` (10 min) |

---

## TLS

When enabled, both the S3 API and admin GUI serve HTTPS on the single listener.

```yaml
advanced:
  tls:
    enabled: true
    cert_path: /etc/ssl/certs/proxy.pem
    key_path: /etc/ssl/private/proxy-key.pem
```

| Field | Env var | YAML | Default |
|-------|---------|------|---------|
| enabled | `DGP_TLS_ENABLED` | `advanced.tls.enabled` | `false` |
| cert_path | `DGP_TLS_CERT` | `advanced.tls.cert_path` | Auto-generate self-signed |
| key_path | `DGP_TLS_KEY` | `advanced.tls.key_path` | Auto-generate |

When `cert_path` and `key_path` are both absent, a self-signed certificate is generated on startup.

---

## Config sync

Multi-instance IAM sync via S3. When enabled, the encrypted config DB file is replicated to a shared S3 bucket.

| | |
|---|---|
| **Env var** | `DGP_CONFIG_SYNC_BUCKET` |
| **YAML** | `advanced.config_sync_bucket` |
| **Default** | None (disabled) |

```yaml
advanced:
  config_sync_bucket: dgp-iam-sync
```

Sync uses the same S3 credentials as the storage backend (`DGP_BE_AWS_*`) and only works when the storage backend is S3 (not filesystem). On every IAM mutation, the DB is uploaded to `s3://<bucket>/.deltaglider/config.db`; readers poll the S3 ETag every 5 minutes and download on change.

By default update uploads use `If-Match` compare-and-swap protection. For a single-writer deployment on an S3-compatible endpoint that rejects conditional update PUTs, set `advanced.config_sync_update_cas: false` or `DGP_CONFIG_SYNC_UPDATE_CAS=false`. First-create uploads still use `If-None-Match: *`; if a remote object already exists but cannot be adopted by the local bootstrap key, the next upload is treated as an unguarded single-writer replacement. Disabling update CAS is not safe for multi-writer use.

---

## Multi-backend routing

Route different buckets to different storage backends. When `backends` is non-empty, the legacy single `backend` is ignored at runtime.

```yaml
storage:
  default_backend: hetzner-fsn1
  backends:
    - name: hetzner-fsn1
      type: s3
      endpoint: https://fsn1.your-objectstorage.com
      region: fsn1
      access_key_id: HETZNER_KEY
      secret_access_key: HETZNER_SECRET
    - name: aws-dr
      type: s3
      endpoint: https://s3.eu-west-1.amazonaws.com
      region: eu-west-1
      access_key_id: AWS_KEY
      secret_access_key: AWS_SECRET
    - name: local-disk
      type: filesystem
      path: /var/lib/dgp-local
  buckets:
    db-archive:
      backend: hetzner-fsn1
      alias: acme-db-archive-prod
```

Backends can be added/removed via the admin GUI (**Storage → Backends**) without restart. `default_backend` is validated against the `backends` list at load time — invalid references are cleared with a warning.

---

## Bucket policies

Per-bucket overrides. All fields optional.

```yaml
storage:
  buckets:
    releases:
      compression: true
      max_delta_ratio: 0.9
      backend: hetzner-fsn1
      alias: acme-prod-releases-fsn1
      quota_bytes: 10737418240    # 10 GiB
    downloads:
      public_prefixes: ["public/"]
    docs-site:
      public: true                # shorthand for public_prefixes: [""]
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `compression` | bool | global | Enable/disable delta compression for this bucket |
| `max_delta_ratio` | float (0-1) | global | Override the delta-keep threshold |
| `backend` | string | default | Route to a named backend from `storage.backends` |
| `alias` | string | same as bucket name | Virtual → real bucket name mapping on the backend |
| `public_prefixes` | `[string]` | `[]` | Anonymous read (GET/HEAD/LIST) scoped to these key prefixes |
| `public` | bool | — | Shorthand for `public_prefixes: [""]` (entire bucket public) |
| `quota_bytes` | u64 | — | Soft storage quota (may overshoot by up to 5 minutes of writes); `0` = freeze bucket |

### Public prefixes

When `public_prefixes` (or `public: true`) is set, anonymous users can GET, HEAD, and LIST objects under the prefix. Writes always require authentication. Use trailing `/` for directory-aligned matching (`"public/"` matches `public/installer.zip` but not `publicity/`). The empty string `""` makes the entire bucket public (logged as a warning). Prefixes containing `..`, null bytes, or `//` are rejected. The proxy synthesizes `public-prefix:<bucket>` admission blocks from this config.

---

## Lifecycle rules

Expiration (delete) and transition/archive rules live under `storage.lifecycle`. Disabled by default; every delete and copy goes through the DeltaGlider engine.

```yaml
storage:
  lifecycle:
    enabled: false
    tick_interval: "1h"
    max_failures_retained: 100
    rules:
      - name: expire-nightly-dumps
        enabled: false
        bucket: db-archive
        prefix: "nightly/"
        action: delete
        expire_after: "90d"
        include_globs: ["nightly/**/*.dump"]
        exclude_globs: [".deltaglider/**", "nightly/golden/**"]
```

Use `POST /_/api/admin/jobs/lifecycle:<name>/preview` (or the Preview button on the Jobs screen) before enabling a rule. See [Lifecycle Rules](lifecycle.md) for API details, skip rules, and limitations.

---

## Event delivery

Durable object mutation events are always appended to the encrypted config DB
when it is available. HTTP delivery is disabled by default; enabling
`advanced.event_delivery` starts a background dispatcher that POSTs each event
to every configured webhook endpoint.

```yaml
advanced:
  event_delivery:
    enabled: true
    webhook_url: "https://events.example.com/deltaglider"
    webhook_urls:
      - "https://audit.example.com/deltaglider"
    webhook_headers:
      authorization: "Bearer redacted-token"
      x-dgp-env: "prod"
    tick_interval: "10s"
    batch_size: 50
    request_timeout: "5s"
    max_attempts: 8
    retry_base: "5s"
    retry_max: "5m"
    stale_claim_after: "60s"
    delivered_retention: "24h"
    delivered_max_rows: 10000
    prune_batch: 100
```

`webhook_url` is the single-endpoint shortcut. `webhook_urls` adds fan-out
endpoints, and `webhook_headers` are attached to every delivery request. A row
is marked delivered only after all endpoints return 2xx; failed rows back off
and can be requeued from the admin API/UI. See [Event outbox](event-outbox.md)
for payload and diagnostics details.

### Slack format

Set `format: slack` to render each event as a Slack message (Block Kit + text
fallback) instead of the raw `{schema,event}` envelope. Two modes — pick one:

```yaml
advanced:
  event_delivery:
    enabled: true
    format: slack
    # Incoming Webhook mode (simplest, single channel):
    webhook_url: "https://hooks.slack.com/services/T000/B000/XXXX"
    slack_username: "DeltaGlider"        # optional cosmetic override
    slack_icon_emoji: ":package:"        # optional
    # Bot-token mode (multi-channel + @mentions) — set these INSTEAD of webhook_url:
    # slack_bot_token: "xoxb-..."        # needs chat:write + chat:write.public scopes
    # slack_channel: "C0123456"          # channel id or #name (required in this mode)
    # Scope what gets posted:
    slack_notify_kinds: ["ObjectCreated"]   # add ObjectDeleted, etc.
    slack_include_globs: ["firmware/**"]    # empty = all user objects
    slack_exclude_globs: ["**/*.tmp"]       # exclude wins over include
    # Per-bucket/prefix routing (bot-token mode only):
    slack_routes:
      - name: "Releases → #ci"
        bucket: releases
        prefix_globs: ["firmware/**"]    # empty = any key in the bucket
        channel: "C_CI"
```

| Key | Notes |
|-----|-------|
| `format` | `raw` (default) or `slack`. |
| `slack_bot_token` | `xoxb-…` Slack Web API token. **Secret** — masked on export, preserved on an untouched round-trip. Selects bot-token mode (`chat.postMessage`). |
| `slack_channel` | Target channel (`C0123` or `#name`). Required in bot-token mode; ignored for Incoming Webhook URLs (each URL is bound to one channel by Slack). |
| `slack_username` / `slack_icon_emoji` | Cosmetic sender overrides (Incoming Webhook mode). |
| `slack_notify_kinds` | Which event kinds post. Default `["ObjectCreated"]`. |
| `slack_include_globs` / `slack_exclude_globs` | Key-glob pre-filter (exclude wins). |
| `slack_routes` | Per-bucket / per-prefix → channel routing (**bot-token mode only**). When non-empty, an eligible event posts to every matching route; `slack_channel` is the fallback for events matching no route. |

The whole thing is editable from the admin GUI at **Integrations →
Event delivery** (toggle the format to *Slack*). See [Event outbox](event-outbox.md#slack-format)
for delivery semantics.

---

## Encryption at rest

Per-backend encryption with four modes: `none`, `aes256-gcm-proxy`, `sse-kms`, `sse-s3`. Each backend carries its own `encryption` block — operators can mix (e.g. SSE-KMS for the production backend, plaintext for a public-CDN backend) without sharing a single blast-radius key.

**YAML** — named-backends path:

```yaml
storage:
  backends:
    - name: hetzner-fsn1
      type: s3
      # endpoint, region, credentials …
      encryption:
        mode: aes256-gcm-proxy
        key: "${env:DGP_BACKEND_HETZNER_FSN1_ENCRYPTION_KEY}"
        key_id: hetzner-2026-06   # optional; derived from SHA-256(name + key) when absent
    - name: aws-dr
      type: s3
      # region, credentials …
      encryption:
        mode: sse-kms
        kms_key_id: arn:aws:kms:eu-west-1:123456789012:key/abcd-ef01
        bucket_key_enabled: true
```

**YAML** — singleton-backend path (`backends:` empty):

```yaml
storage:
  backend: { ... }
  backend_encryption:
    mode: aes256-gcm-proxy
    key: "${env:DGP_ENCRYPTION_KEY}"
```

**Env vars** (infra secrets — these are the recommended key source; every `key` / `kms_key_id` field in YAML is stripped by canonical exports):

| Env var | Binds to |
|---|---|
| `DGP_ENCRYPTION_KEY` | `backend_encryption.key` (singleton path) |
| `DGP_BACKEND_<NAME>_ENCRYPTION_KEY` | `backends[name=<NAME>].encryption.key` |
| `DGP_SSE_KMS_KEY_ID` | `backend_encryption.kms_key_id` (singleton SSE-KMS) |
| `DGP_BACKEND_<NAME>_SSE_KMS_KEY_ID` | named SSE-KMS override |

Name normalisation: `<NAME>` is uppercased; `-` and `.` become `_` (so `hetzner-fsn1` → `DGP_BACKEND_HETZNER_FSN1_ENCRYPTION_KEY`).

**Defaults:** absent `encryption` block → `mode: none` (plaintext).

**Formats:** `key` / `legacy_key` are 64-char lowercase hex (256 bits). `kms_key_id` is a KMS ARN or alias. `key_id` (optional) must match `[A-Za-z0-9_.-]{1,64}` (S3 user-metadata header-safe).

Rotation within a single mode is not automated — use the `legacy_key` / `legacy_key_id` shim fields (decrypt-only, for proxy→native transitions) or copy objects to a new backend. See the [encryption reference](encryption.md) for the full wire format, key-id mismatch mechanics, and the shim lifecycle.

---

## CLI subcommands

See [Command-line tools](cli.md).

---

## Full example

A kitchen-sink YAML covering every top-level section. Fields omitted here inherit their defaults.

```yaml
# deltaglider_proxy.yaml

# Operator-authored admission chain (pre-auth gating)
admission:
  blocks:
    - name: deny-known-bad-ips
      match:
        source_ip_list: ["198.51.100.0/24"]
      action: deny

    - name: allow-public-zips
      match:
        method: [GET, HEAD]
        bucket: releases
        path_glob: "*.zip"
      action: allow-anonymous

# SigV4 credentials + IAM mode
access:
  iam_mode: gui               # or declarative
  access_key_id: admin
  secret_access_key: changeme

# Backends + per-bucket overrides
storage:
  default_backend: hetzner-fsn1
  backends:
    - name: hetzner-fsn1
      type: s3
      endpoint: https://fsn1.your-objectstorage.com
      region: fsn1
      force_path_style: true
      access_key_id: HETZNER_KEY
      secret_access_key: HETZNER_SECRET
    - name: aws-dr
      type: s3
      endpoint: https://s3.eu-west-1.amazonaws.com
      region: eu-west-1
      access_key_id: AWS_KEY
      secret_access_key: AWS_SECRET
  buckets:
    releases:
      backend: hetzner-fsn1
      compression: true
    db-archive:
      backend: aws-dr
      alias: acme-db-archive-prod
      compression: false
    downloads:
      public_prefixes: ["public/"]

# Process-level tunables
advanced:
  listen_addr: "0.0.0.0:9000"
  log_level: deltaglider_proxy=info,tower_http=warn
  max_delta_ratio: 0.75
  cache_size_mb: 2048
  metadata_cache_mb: 100
  codec_concurrency: 32
  config_sync_bucket: dgp-iam-sync
  event_delivery:
    enabled: true
    webhook_urls:
      - "https://audit.example.com/deltaglider"
    webhook_headers:
      authorization: "Bearer redacted-token"
  tls:
    enabled: true
    cert_path: /etc/ssl/certs/proxy.pem
    key_path: /etc/ssl/private/proxy-key.pem
```

Equivalent environment variables for container deployments:

```bash
DGP_LISTEN_ADDR=0.0.0.0:9000
DGP_MAX_DELTA_RATIO=0.75
DGP_MAX_OBJECT_SIZE=104857600
DGP_CACHE_MB=2048
DGP_METADATA_CACHE_MB=100
DGP_CODEC_CONCURRENCY=32
DGP_LOG_LEVEL=deltaglider_proxy=info,tower_http=warn
DGP_ACCESS_KEY_ID=admin
DGP_SECRET_ACCESS_KEY=changeme
DGP_BOOTSTRAP_PASSWORD_HASH=JDJiJDEyJENYbDVPRm84bDg2...
DGP_CONFIG_SYNC_BUCKET=dgp-iam-sync
DGP_S3_ENDPOINT=https://fsn1.your-objectstorage.com
DGP_S3_REGION=fsn1
DGP_S3_PATH_STYLE=true
DGP_BE_AWS_ACCESS_KEY_ID=HETZNER_KEY
DGP_BE_AWS_SECRET_ACCESS_KEY=HETZNER_SECRET
DGP_TLS_ENABLED=true
DGP_TLS_CERT=/etc/ssl/certs/proxy.pem
DGP_TLS_KEY=/etc/ssl/private/proxy-key.pem
```

---

## Environment variable registry

Exhaustive list of every `DGP_*` variable the server reads. The unit test `test_registry_completeness` in `src/config.rs` enforces that this list and `ENV_VAR_REGISTRY` stay in sync.

### Server / Advanced

| Variable | Default | Description |
|----------|---------|-------------|
| `DGP_CONFIG` | auto | Path to the YAML config file (`.yaml` / `.yml`) |
| `DGP_LISTEN_ADDR` | `0.0.0.0:9000` | HTTP listen address |
| `DGP_LOG_LEVEL` | `deltaglider_proxy=debug,tower_http=debug` | Tracing filter (overridden by `RUST_LOG`) |
| `DGP_LOG_FORMAT` | `text` | Stdout log format: `text` or `json` (one JSON object per line) |
| `DGP_LOG_RING_SIZE` | `2000` | In-memory operational-log ring capacity (admin Logs viewer) |
| `DGP_LOG_RING_LEVEL` | `info` | Minimum severity captured into the log ring/stream |
| `DGP_AUDIT_RING_SIZE` | `500` | In-memory audit ring capacity (admin Audit log viewer) |
| `DGP_BLOCKING_THREADS` | 512 | Max tokio blocking threads |
| `DGP_REQUEST_TIMEOUT_SECS` | 300 | Per-request timeout (returns 504) |
| `DGP_MAX_CONCURRENT_REQUESTS` | 1024 | Tower concurrency limit |
| `DGP_MAX_MULTIPART_UPLOADS` | 1000 | Concurrent multipart upload cap |
| `DGP_DEBUG_HEADERS` | false | Expose fingerprinting headers |
| `DGP_CORS_PERMISSIVE` | false | Enable permissive CORS (dev only) |

### Delta engine

| Variable | Default | Description |
|----------|---------|-------------|
| `DGP_MAX_DELTA_RATIO` | 0.75 | Keep delta only if `delta/original < ratio` |
| `DGP_MAX_OBJECT_SIZE` | 104857600 | Max bytes eligible for delta (xdelta3 mem cap) |
| `DGP_CACHE_MB` | 100 | Reference cache size in MB |
| `DGP_METADATA_CACHE_MB` | 50 | `FileMetadata` cache size in MB (0 to disable) |
| `DGP_CODEC_CONCURRENCY` | `num_cpus * 4` (min 16) | Max concurrent xdelta3 subprocesses |
| `DGP_CODEC_TIMEOUT_SECS` | 60 | Per-subprocess timeout |

### Storage

| Variable | Default | Description |
|----------|---------|-------------|
| `DGP_DATA_DIR` | `./data` | Filesystem backend data directory |
| `DGP_S3_ENDPOINT` | — | S3 endpoint (activates S3 backend when set) |
| `DGP_S3_REGION` | `us-east-1` | AWS region |
| `DGP_S3_PATH_STYLE` | true | Use path-style URLs (MinIO/LocalStack) |
| `DGP_BE_AWS_ACCESS_KEY_ID` | — | Backend S3 access key |
| `DGP_BE_AWS_SECRET_ACCESS_KEY` | — | Backend S3 secret key |

### Authentication

| Variable | Default | Description |
|----------|---------|-------------|
| `DGP_AUTHENTICATION` | — | `"none"` for open access; absent = auto-detect |
| `DGP_ACCESS_KEY_ID` | — | Proxy SigV4 access key |
| `DGP_SECRET_ACCESS_KEY` | — | Proxy SigV4 secret key |
| `DGP_BOOTSTRAP_PASSWORD_HASH` | auto | Bcrypt hash (legacy alias: `DGP_ADMIN_PASSWORD_HASH`) |
| `DGP_BOOTSTRAP_PASSWORD` | — | Plaintext password for admin CLI only |

### Security

| Variable | Default | Description |
|----------|---------|-------------|
| `DGP_TRUST_PROXY_HEADERS` | false | Trust `X-Forwarded-For` / `X-Real-IP` |
| `DGP_SESSION_TTL_HOURS` | 4 | Admin session lifetime |
| `DGP_CLOCK_SKEW_SECONDS` | 300 | SigV4 clock skew tolerance |
| `DGP_REPLAY_WINDOW_SECS` | 2 | SigV4 replay detection window |
| `DGP_SECURE_COOKIES` | true | Require HTTPS for session cookies |
| `DGP_RATE_LIMIT_MAX_ATTEMPTS` | 100 | Max auth failures before lockout |
| `DGP_RATE_LIMIT_WINDOW_SECS` | 300 | Rate-limit rolling window |
| `DGP_RATE_LIMIT_LOCKOUT_SECS` | 600 | Lockout duration |

### TLS / Config sync / Encryption at rest / Misc

| Variable | Default | Description |
|----------|---------|-------------|
| `DGP_TLS_ENABLED` | false | Enable HTTPS |
| `DGP_TLS_CERT` | auto self-signed | PEM cert path |
| `DGP_TLS_KEY` | auto self-signed | PEM key path |
| `DGP_CONFIG_SYNC_BUCKET` | — | S3 bucket for encrypted-DB multi-instance sync |
| `DGP_CONFIG_SYNC_UPDATE_CAS` | true | Use `If-Match` CAS on encrypted-DB update uploads; set false only for single-writer S3 endpoints that reject conditional update PUTs |
| `DGP_ENCRYPTION_KEY` | — | Singleton-backend AES-256 key (64-char hex). Named backends use `DGP_BACKEND_<NAME>_ENCRYPTION_KEY`. |
| `DGP_SSE_KMS_KEY_ID` | — | Singleton-backend SSE-KMS ARN/alias. Named backends use `DGP_BACKEND_<NAME>_SSE_KMS_KEY_ID`. |

### Consumed only by tests / build

| Variable | Consumer |
|----------|----------|
| `DGP_BUILD_TIME` | `build.rs` (compile-time timestamp) |
| `DGP_BUCKET` | historical comment in tests; no longer read |
