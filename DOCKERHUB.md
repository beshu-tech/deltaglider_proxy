# DeltaGlider Proxy

An S3-compatible proxy with transparent delta compression for versioned binary artifacts.

Clients see a standard S3 API. The proxy deduplicates with xdelta3 against a per-prefix reference baseline. On versioned builds, firmware images, and binary releases, this typically saves 60 to 95% of the storage.

## Quick start

```bash
docker run -d \
  -p 9000:9000 \
  -v dgp-data:/data \
  -e DGP_ACCESS_KEY_ID=dgpadmin \
  -e DGP_SECRET_ACCESS_KEY=change-me-please \
  beshultd/deltaglider_proxy
```

- Port 9000: the S3-compatible API and the admin GUI (everything is on one port)
- `DGP_ACCESS_KEY_ID` / `DGP_SECRET_ACCESS_KEY`: the S3 credentials clients present to the proxy. These are required: the proxy refuses to start without authentication configured (unless you explicitly set `DGP_AUTHENTICATION=none` for local development). Choose your own values.

Then open `http://localhost:9000/_/` for the built-in browser and dashboard.

## With MinIO as backend

```bash
docker run -d \
  -p 9000:9000 \
  -e DGP_ACCESS_KEY_ID=dgpadmin \
  -e DGP_SECRET_ACCESS_KEY=change-me-please \
  -e DGP_S3_ENDPOINT=http://minio:9000 \
  -e DGP_S3_REGION=us-east-1 \
  -e DGP_BE_AWS_ACCESS_KEY_ID=minioadmin \
  -e DGP_BE_AWS_SECRET_ACCESS_KEY=minioadmin \
  -e DGP_CACHE_MB=1024 \
  beshultd/deltaglider_proxy
```

The `DGP_ACCESS_KEY_ID` / `DGP_SECRET_ACCESS_KEY` pair is the S3 credential clients present to the proxy (required to start). The `DGP_BE_AWS_*` pair is separate: it authenticates the proxy to the MinIO backend behind it.

## Docker Compose

```yaml
services:
  minio:
    # pgsty/silo is the maintained MinIO fork (minio/minio left Docker Hub);
    # any S3-compatible store works here.
    image: pgsty/silo
    command: server /data
    environment:
      MINIO_ROOT_USER: minioadmin
      MINIO_ROOT_PASSWORD: minioadmin

  deltaglider:
    image: beshultd/deltaglider_proxy
    ports:
      - "9000:9000"
    environment:
      DGP_S3_ENDPOINT: http://minio:9000
      DGP_S3_REGION: us-east-1
      DGP_BE_AWS_ACCESS_KEY_ID: minioadmin
      DGP_BE_AWS_SECRET_ACCESS_KEY: minioadmin
      DGP_ACCESS_KEY_ID: myproxykey
      DGP_SECRET_ACCESS_KEY: myproxysecret
      DGP_CACHE_MB: 1024
    depends_on:
      - minio
```

## How it works

```
S3 Client ──PUT──▶ DeltaGlider Proxy ──delta──▶ Storage Backend
                        │                            (S3 / filesystem)
                   xdelta3 encode
                   reference cache
                   transparent to clients
```

1. **PUT**: Files within a prefix are delta-compressed against a shared reference baseline
2. **GET**: The proxy transparently reconstructs deltas, so clients receive the original file
3. **Passthrough**: Non-compressible files (images, video, already-compressed) skip delta entirely

## Configuration

You can set all settings through environment variables:

| Variable | Default | Description |
|----------|---------|-------------|
| `DGP_LISTEN_ADDR` | `0.0.0.0:9000` | S3 API listen address |
| `DGP_MAX_DELTA_RATIO` | `0.75` | Max delta/original ratio (lower = more aggressive) |
| `DGP_MAX_OBJECT_SIZE` | `104857600` | Max size of an uploaded object (100 MB), for every object and not only for deltas |
| `DGP_CACHE_MB` | `100` | Reference cache size in MB (recommend ≥1024 for production) |
| `DGP_ACCESS_KEY_ID` | *(unset)* | Proxy SigV4 access key (**required**: the proxy refuses to start without credentials unless `DGP_AUTHENTICATION=none`) |
| `DGP_SECRET_ACCESS_KEY` | *(unset)* | Proxy SigV4 secret key |
| `DGP_AUTHENTICATION` | *(auto-detect)* | Set to `none` for open-access dev mode |
| `DGP_DATA_DIR` | `./data` | Filesystem backend data directory |
| `DGP_S3_ENDPOINT` | *(unset)* | S3 backend endpoint URL |
| `DGP_S3_REGION` | `us-east-1` | S3 backend region |
| `DGP_BE_AWS_ACCESS_KEY_ID` | *(unset)* | Backend S3 credentials |
| `DGP_BE_AWS_SECRET_ACCESS_KEY` | *(unset)* | Backend S3 credentials |
| `DGP_BOOTSTRAP_PASSWORD_HASH` | *(auto-generated)* | Bootstrap password bcrypt hash (signs session cookies, gates admin GUI). Base64-encoded form avoids `$` escaping in Docker. |
| `DGP_CONFIG_DB_KEY` | *(key file next to the DB)* | Encryption key of the IAM config DB, at least 32 characters. Without it, the proxy generates the key file `deltaglider_config.db.key` next to the DB on the first start: back up the key file together with the DB. Required, and the same on every instance, with `DGP_CONFIG_SYNC_BUCKET` |
| `DGP_LOG_LEVEL` | `deltaglider_proxy=info,tower_http=info` | Log filter (you can change it at runtime in the admin GUI) |
| `DGP_CONFIG_SYNC_BUCKET` | *(unset)* | S3 bucket for encrypted-DB multi-instance sync. Needs `DGP_CONFIG_DB_KEY` |
| `DGP_TLS_ENABLED` | `false` | Enable HTTPS |

Or mount a YAML config file:

```bash
docker run -v ./my-config.yaml:/etc/deltaglider_proxy.yaml \
  beshultd/deltaglider_proxy -c /etc/deltaglider_proxy.yaml
```

(YAML is the only config format, because v1.4.1 removed TOML support. Convert a TOML config with `deltaglider_proxy config migrate` on v1.4.0 before you upgrade.)

## Built-in admin GUI

The admin GUI is served at `/_/` on the same port as the S3 API:

- S3 object browser: browse, upload, download, and delete objects, preview a file on double-click, and bulk copy, move, or ZIP
- Proxy dashboard: live Prometheus metrics, with 9 headline KPIs first (savings, requests, memory, error rate, cache health), more telemetry in a collapsible "Detailed telemetry" section, and per-bucket savings analytics
- Configuration: hot-reload settings split across Access, Storage, Integrations, and System sections: multi-backend routing, per-bucket policies, compression tuning, admission control, lifecycle, replication, and webhook/Slack notification delivery
- IAM user management: create, edit, and delete users with ABAC permissions, and manage OAuth/OIDC providers and group-mapping rules
- Audit log: an in-memory ring of recent access events (size via `DGP_AUDIT_RING_SIZE`, default 500)
- Demo data generator: populate test data for evaluation

## Ports

| Port | Protocol | Purpose |
|------|----------|---------|
| 9000 | HTTP/S | S3-compatible API + Admin GUI (`/_/`) + `/_/metrics` + `/_/health` + `/_/ready` + `/_/stats` |

## Health checks

```bash
# Liveness (the process answers; no backend request)
curl http://localhost:9000/_/health

# Readiness (probes the storage backends and the config DB; 503 when not ready)
curl http://localhost:9000/_/ready

# Prometheus metrics (public unless DGP_METRICS_BEARER_TOKEN is set)
curl http://localhost:9000/_/metrics
```

`/_/stats` (objects, savings %) answers only an admin session, because it shows the size of every bucket.

The Docker image includes a built-in healthcheck on port 9000 (15s interval).

## Image details

- Base: `debian:bookworm-slim`
- Runtime deps: `xdelta3`, `ca-certificates`, `curl`
- Runs as: non-root user `dg`
- Platforms: `linux/amd64`, `linux/arm64`
- Size: about 60 MB compressed

## Tags

| Tag | Description |
|-----|-------------|
| `latest` | Latest stable release |
| `2.0.0` | Specific version |
| `2.0` | Latest patch in 2.0.x |
| `2` | Latest minor in 2.x.x |

## Source and license

- Source: [github.com/beshu-tech/deltaglider_proxy](https://github.com/beshu-tech/deltaglider_proxy)
- License: BUSL-1.1. It is free for production use while your organization's stored footprint (every copy that DeltaGlider writes, after compression) stays at or under 15 TB, and every release converts to Apache-2.0 two years after it ships. Releases up to v1.17.0 remain GPL-3.0. See [deltaglider.com/pricing](https://deltaglider.com/pricing/).
