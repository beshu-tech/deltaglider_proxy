# DeltaGlider Proxy

**S3-compatible proxy with transparent delta compression for versioned binary artifacts.**

Clients see a standard S3 API. The proxy silently deduplicates using xdelta3 against a per-prefix reference baseline — typically saving **60–95%** storage on versioned builds, firmware images, and binary releases.

## Quick Start

```bash
docker run -d \
  -p 9000:9000 \
  -p 9001:9001 \
  -v dgp-data:/data \
  beshultd/deltaglider_proxy
```

- **Port 9000** — S3-compatible API (point your S3 client here)
- **Port 9001** — Admin GUI + Prometheus metrics dashboard

Then open `http://localhost:9001` for the built-in browser and dashboard.

## With MinIO as Backend

```bash
docker run -d \
  -p 9000:9000 \
  -p 9001:9001 \
  -e DGP_S3_ENDPOINT=http://minio:9000 \
  -e DGP_S3_REGION=us-east-1 \
  -e DGP_BE_AWS_ACCESS_KEY_ID=minioadmin \
  -e DGP_BE_AWS_SECRET_ACCESS_KEY=minioadmin \
  -e DGP_CACHE_MB=1024 \
  beshultd/deltaglider_proxy
```

## Docker Compose

```yaml
services:
  minio:
    image: minio/minio
    command: server /data
    environment:
      MINIO_ROOT_USER: minioadmin
      MINIO_ROOT_PASSWORD: minioadmin

  deltaglider:
    image: beshultd/deltaglider_proxy
    ports:
      - "9000:9000"
      - "9001:9001"
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

## How It Works

```
S3 Client ──PUT──▶ DeltaGlider Proxy ──delta──▶ Storage Backend
                        │                            (S3 / filesystem)
                   xdelta3 encode
                   reference cache
                   transparent to clients
```

1. **PUT**: Files within a prefix are delta-compressed against a shared reference baseline
2. **GET**: Deltas are transparently reconstructed — clients receive the original file
3. **Passthrough**: Non-compressible files (images, video, already-compressed) skip delta entirely

## Configuration

All settings via environment variables:

| Variable | Default | Description |
|----------|---------|-------------|
| `DGP_LISTEN_ADDR` | `0.0.0.0:9000` | S3 API listen address |
| `DGP_MAX_DELTA_RATIO` | `0.5` | Max delta/original ratio (lower = more aggressive) |
| `DGP_MAX_OBJECT_SIZE` | `104857600` | Max object size for delta (100 MB) |
| `DGP_CACHE_MB` | `100` | Reference cache size in MB |
| `DGP_ACCESS_KEY_ID` | *(unset)* | Proxy SigV4 access key (unset = open access) |
| `DGP_SECRET_ACCESS_KEY` | *(unset)* | Proxy SigV4 secret key |
| `DGP_DATA_DIR` | `./data` | Filesystem backend data directory |
| `DGP_S3_ENDPOINT` | *(unset)* | S3 backend endpoint URL |
| `DGP_S3_REGION` | `us-east-1` | S3 backend region |
| `DGP_BE_AWS_ACCESS_KEY_ID` | *(unset)* | Backend S3 credentials |
| `DGP_BE_AWS_SECRET_ACCESS_KEY` | *(unset)* | Backend S3 credentials |
| `DGP_LOG_LEVEL` | `debug` | Log filter (changeable at runtime via admin GUI) |
| `DGP_TLS_ENABLED` | `false` | Enable HTTPS |

Or mount a TOML config file:

```bash
docker run -v ./my-config.toml:/etc/deltaglider_proxy.toml \
  beshultd/deltaglider_proxy -c /etc/deltaglider_proxy.toml
```

## Built-in Admin GUI

The admin GUI on port 9001 provides:

- **S3 Object Browser** — browse, upload, download, delete objects
- **Proxy Dashboard** — live Prometheus metrics with charts (cache health, compression stats, HTTP traffic, auth)
- **Settings** — hot-reload configuration, change backend, tune compression, manage credentials
- **Demo Data Generator** — populate test data for evaluation

## Ports

| Port | Protocol | Purpose |
|------|----------|---------|
| 9000 | HTTP/S | S3-compatible API |
| 9001 | HTTP/S | Admin GUI + `/metrics` + `/health` + `/stats` |

## Health Checks

```bash
# S3 API health
curl http://localhost:9000/health

# Prometheus metrics
curl http://localhost:9000/metrics

# Storage stats (objects, savings %)
curl http://localhost:9000/stats
```

The Docker image includes a built-in healthcheck on port 9001 (15s interval).

## Image Details

- **Base**: `debian:bookworm-slim`
- **Runtime deps**: `xdelta3`, `ca-certificates`, `curl`
- **Runs as**: non-root user `dg`
- **Platforms**: `linux/amd64`, `linux/arm64`
- **Size**: ~60 MB compressed

## Tags

| Tag | Description |
|-----|-------------|
| `latest` | Latest stable release |
| `0.2.0` | Specific version |
| `0.2` | Latest patch in 0.2.x |
| `0` | Latest minor in 0.x.x |

## Source & License

- **Source**: [github.com/beshu-tech/deltaglider_proxy](https://github.com/beshu-tech/deltaglider_proxy)
- **License**: GPL-2.0
