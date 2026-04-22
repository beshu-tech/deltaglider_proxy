# Production deployment

*Running DeltaGlider Proxy as a real service: config, TLS, health checks, backups, and multi-instance sync.*

This is the operational counterpart to the [Security checklist](20-production-security-checklist.md). Follow both — security covers *what to turn on*; this page covers *how to run the process*.

## Configuration file

YAML is the canonical format. TOML still loads but emits a deprecation warning on every startup (suppress with `DGP_SILENCE_TOML_DEPRECATION=1`). Full field reference: [reference/configuration.md](reference/configuration.md). If you're migrating from TOML, see [the upgrade guide](21-upgrade-guide.md).

**File search order** (first match wins):

1. `DGP_CONFIG` env var — explicit path, used unconditionally when set
2. `./deltaglider_proxy.yaml`
3. `./deltaglider_proxy.yml`
4. `./deltaglider_proxy.toml` (deprecated)
5. `/etc/deltaglider_proxy/config.yaml`
6. `/etc/deltaglider_proxy/config.yml`
7. `/etc/deltaglider_proxy/config.toml` (deprecated)

Environment variables (`DGP_*` prefix) override file contents. CLI flags override everything:

```bash
deltaglider_proxy --config /etc/deltaglider_proxy/config.yaml --listen 0.0.0.0:9000
```

**Validate before shipping** — wire this into CI:

```bash
deltaglider_proxy config lint /etc/deltaglider_proxy/config.yaml
# Exit: 0 = valid, 3 = I/O error, 4 = parse error, 6 = validation error
```

## Storage backend

Pick a backend in YAML. The most common production shape is AWS-compatible S3:

```yaml
# validate
storage:
  backend:
    type: s3
    endpoint: https://s3.eu-central-1.amazonaws.com
    region: eu-central-1
    force_path_style: false
    access_key_id: AKIA...
    secret_access_key: ...
```

For filesystem-backed dev:

```yaml
# validate
storage:
  backend:
    type: filesystem
    path: /var/lib/deltaglider_proxy/data
```

Per-bucket routing, aliasing, and compression policies live under `storage.buckets`. See [Setting up a new bucket](10-first-bucket.md).

## TLS and reverse proxy

Run the proxy bound to `127.0.0.1:9000` and front it with Traefik, Caddy, or nginx. Let the reverse proxy terminate TLS and forward to DeltaGlider Proxy over the loopback.

**Required env vars when behind a reverse proxy:**

| Variable | Value | Why |
|---|---|---|
| `DGP_TRUST_PROXY_HEADERS` | `true` | Accept `X-Forwarded-For` / `X-Real-IP` for rate limiting + IAM IP conditions. Default is `false`; flip it only when a reverse proxy is genuinely in front, otherwise clients can spoof IPs. |
| `DGP_SECURE_COOKIES` | `true` | Already the default. Keeps admin session cookies HTTPS-only. |

**Sample Traefik label set** (Docker Compose):

```yaml
deltaglider_proxy:
  image: beshultd/deltaglider_proxy:latest
  environment:
    DGP_TRUST_PROXY_HEADERS: "true"
  labels:
    traefik.enable: "true"
    traefik.http.routers.dgp.rule: "Host(`dgp.example.com`)"
    traefik.http.routers.dgp.entrypoints: "websecure"
    traefik.http.routers.dgp.tls.certresolver: "letsencrypt"
    traefik.http.services.dgp.loadbalancer.server.port: "9000"
```

The UI (`/_/*`) and the S3 API (`/`) share the same port — route everything under `/` to the proxy; no per-path rules needed.

## Health and observability

Three always-on endpoints, exempt from SigV4 auth so monitoring systems can scrape them without credentials:

| Path | Payload | Cache |
|---|---|---|
| `GET /_/health` | `{status, peak_rss_bytes, cache_size_bytes, cache_max_bytes, cache_entries, cache_utilization_pct}` | none |
| `GET /_/stats` | Aggregate storage stats | 10s server-side |
| `GET /_/metrics` | Prometheus text format | none |

Version is intentionally **not** in `/health` (anti-fingerprinting). The authenticated `GET /_/api/whoami` returns it.

Full metrics catalog + alert suggestions: [reference/metrics.md](reference/metrics.md) and [Monitoring and alerts](40-monitoring-and-alerts.md).

### Cache health

Four layers of visibility:

1. **Startup log lines** with a `[cache]` prefix — `cache_size_mb == 0` warns "DISABLED"; `< 1024` warns undersized for production.
2. **Periodic monitor** (60s) — warns at utilisation > 90% or miss rate > 50% (min 10 ops/interval).
3. **Prometheus gauges** — `deltaglider_cache_max_bytes`, `deltaglider_cache_utilization_ratio`, `deltaglider_cache_miss_rate_ratio`.
4. **Per-response header** — `x-deltaglider-cache: hit|miss` on every delta-reconstructed GET (when `DGP_DEBUG_HEADERS=true`).

## Logging

Log level resolves in priority order:

1. `RUST_LOG` env var (standard `tracing-subscriber` syntax)
2. `DGP_LOG_LEVEL` env var (same syntax)
3. `--verbose` CLI flag (sets `trace`)
4. Default: `deltaglider_proxy=debug,tower_http=debug`

```bash
RUST_LOG=deltaglider_proxy=info,tower_http=warn deltaglider_proxy
```

**Runtime changes without restart:** the admin UI (Settings → Advanced → Logging) hot-reloads the filter through the normal apply pipeline.

## Performance knobs

Set before going under real load:

| Variable | Default | When to tune |
|---|---|---|
| `DGP_CACHE_MB` | 100 | **Bump to 1024+ in production.** Reference-baseline LRU cache. Hot-read workloads benefit most. |
| `DGP_METADATA_CACHE_MB` | 50 | Bump to 200+ if you list large prefixes repeatedly. Holds `FileMetadata` for HEAD/LIST acceleration. |
| `DGP_MAX_OBJECT_SIZE` | 100 MB | Hard cap on upload size (applies to both delta and passthrough). Raise if your workload has larger artefacts. |
| `DGP_MAX_DELTA_RATIO` | 0.75 | If `delta_size / original_size ≥ ratio`, the object falls back to passthrough. Lower = stricter (less delta, more passthrough); higher = keep more deltas. |
| `DGP_MAX_MULTIPART_UPLOADS` | 1000 | Max concurrent multipart uploads. |

Low-level codec tuning: `codec_concurrency`, `codec_timeout_secs` — rarely touched, see [reference/configuration.md](reference/configuration.md).

## Backup and disaster recovery

Three distinct mechanisms, routinely confused:

### 1. Full Backup (operator-initiated, point-in-time)

`GET /_/api/admin/backup` returns a **zip** with four artefacts:

- `manifest.json` — version, timestamp, sha256 of every entry
- `config.yaml` — canonical YAML, secrets fully redacted
- `iam.json` — users, groups, OAuth providers, mapping rules, external identities
- `secrets.json` — plaintext infra secrets (bootstrap hash, OAuth client_secrets, storage creds). The zip is a keystore — treat it that way.

`POST /_/api/admin/backup` accepts the same zip to restore. The import is atomic: all four parts are unpacked and sha256-verified before any state is changed.

Legacy JSON-only body is still accepted (pre-v0.8.4 scripts keep working) for IAM-only restores.

### 2. Encrypted DB snapshot (file-level)

`deltaglider_config.db` is a SQLCipher-encrypted SQLite file. You can back it up with regular filesystem snapshots — as long as the bootstrap password is also preserved, the DB can be restored onto another instance.

### 3. Multi-instance S3 sync (live replication)

Set `advanced.config_sync_bucket` in YAML (or `DGP_CONFIG_SYNC_BUCKET`). The proxy then uploads the encrypted DB after every mutation. Readers on the same bucket poll S3 every 5 minutes and download on ETag change.

```yaml
# validate
advanced:
  config_sync_bucket: my-dgp-iam-sync
```

Use this for blue-green deploys or horizontal scaling. It does **not** replace backups — it replicates the same state, so a bad mutation propagates to every reader.

See [Authentication reference](reference/authentication.md) for the interaction between sync mode, `iam_mode: declarative`, and the admin-API mutation rules.

## Upgrades and version skew

See [the upgrade guide](21-upgrade-guide.md) for the full process. Short version:

1. **Take a Full Backup** (`GET /_/api/admin/backup`) before any upgrade.
2. Roll the binary / container image.
3. Check `/_/health` and `/_/api/whoami` — both return expected values.
4. If something went wrong, the backup zip you took in step 1 is atomic + sha256-verified.

## Deployment platforms (short notes)

**Kubernetes:** `health_check_path` is `/_/health`. Mount a PersistentVolume at `/data` (the container's CWD). Inject `DGP_BOOTSTRAP_PASSWORD_HASH` via a Secret.

**systemd:** run as `deltaglider_proxy.service` with `WorkingDirectory=/var/lib/deltaglider_proxy` and `EnvironmentFile=/etc/deltaglider_proxy/env`. The binary exits non-zero on unrecoverable errors, so `Restart=on-failure` is appropriate.

**Coolify / Docker hosts:** mount a persistent volume at `/data` and inject env vars via the platform's secret store. The container writes `./deltaglider_proxy.yaml`, `./deltaglider_config.db`, and `./data/` relative to its CWD (`/data`).

**AWS ALB / NLB:** point at port 9000. The ALB is a valid reverse proxy — set `DGP_TRUST_PROXY_HEADERS=true`. Use target group health check path `/_/health`, HTTP 200 = healthy.
