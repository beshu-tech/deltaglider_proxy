# How to take a proxy to production

This guide shows you how to take a working DeltaGlider Proxy from "runs on my laptop" to a production service. It is a checklist. Each item is short and links to the guide that covers the task in full.

## Prerequisites

- A proxy that already passes the security baseline: SigV4 auth on, bootstrap password set, IAM users created. If you haven't done that yet, complete [Secure your proxy](../tutorials/secure-your-proxy.md) first.
- A production host (Docker, Kubernetes, or systemd) and a DNS name. This guide uses `https://s3.acme.example`.

## Pick a platform

- Docker Compose: the simplest production shape, with a secret-free config and an `env_file`. See [How to deploy with Docker Compose](deploy-with-docker-compose.md).
- Kubernetes: the official Helm chart with a PVC, probes, and an Ingress. See [How to deploy on Kubernetes with Helm](deploy-on-kubernetes.md).
- systemd: run as `deltaglider_proxy.service` with `WorkingDirectory=/var/lib/deltaglider_proxy` and `EnvironmentFile=/etc/deltaglider_proxy/env`. The binary exits non-zero on unrecoverable errors, so `Restart=on-failure` is appropriate.
- Coolify or plain Docker hosts: mount a persistent volume at `/data` and inject the env vars through the platform's secret store. The container writes `./deltaglider_proxy.yaml`, `./deltaglider_config.db`, and `./data/` relative to its CWD (`/data`).
- Behind an AWS ALB or NLB: point at port 9000, health-check path `/_/ready` (HTTP 200 = ready, 503 = not ready). `/_/ready` probes the storage backends and the config DB, and `/_/health` only shows that the process is alive. The ALB is a reverse proxy: set `DGP_TRUST_PROXY_HEADERS=true`, set `DGP_TRUSTED_PROXY_CIDRS` to the subnets of the ALB, and raise the idle timeout (see [How to serve TLS](serve-tls.md)).

On every platform, one port serves everything, because the UI (`/_/*`) and the S3 API (`/`) share the listener. On every platform, `/data` must persist across restarts.

## The checklist

### 1. Ship a config file and lint it in CI

Put your config in a versioned `deltaglider_proxy.yaml` rather than a pile of env vars, keep secrets out of it with `${env:NAME}` placeholders, and validate every change before it ships:

```bash
deltaglider_proxy config lint /etc/deltaglider_proxy/config.yaml
# Exit: 0 = valid, 3 = I/O error, 4 = parse error, 6 = validation error
```

`config lint` refuses an empty file and an unknown key, as the proxy does when it loads the file.

Wire that into CI so that the review catches drift. Full field reference: [Configuration](../reference/configuration.md). For the secret-free-config pattern end to end, see [How to deploy with Docker Compose](deploy-with-docker-compose.md).

### 2. Pin a storage backend

Decide where the bytes live and say so explicitly. Do not take the filesystem default into production:

```yaml
storage:
  backends:
    - name: hetzner-fsn1
      type: s3
      endpoint: ${env:S3_ENDPOINT}
      region: ${env:S3_REGION}
      force_path_style: true        # true for MinIO/Hetzner; false for AWS
      access_key_id: ${env:S3_ACCESS_KEY_ID}
      secret_access_key: ${env:S3_SECRET_ACCESS_KEY}
  default_backend: hetzner-fsn1
```

If you use the filesystem backend, the data directory must support xattrs (ext4, XFS, Btrfs, ZFS, APFS). Otherwise, the proxy refuses to start. Backend options: [Configuration → Storage](../reference/configuration.md).

### 3. Serve TLS

S3 clients expect HTTPS. Terminate TLS either at the proxy itself or at a reverse proxy in front of it. If you use a reverse proxy, you **must** raise its read timeout, or large uploads fail with 502/504. See [How to serve TLS](serve-tls.md).

### 4. Set up backups

Take a Full Backup zip before you call anything production, and put it on a schedule. Know the difference between the three mechanisms (Full Backup, DB snapshot, S3 config sync) before you need them at 3 a.m. See [How to back up and restore](back-up-and-restore.md).

### 5. Wire up monitoring

Scrape `/_/metrics` with Prometheus, import the dashboard panels, and install the alert rules for error rate, p95 latency, cache hit ratio, codec saturation, and instance down. `/_/metrics` answers without authentication unless you set `DGP_METRICS_BEARER_TOKEN`; set it for an internet-facing proxy. See [How to monitor with Prometheus and Grafana](monitor-with-prometheus.md).

### 6. Tighten rate limits

The auth-endpoint defaults are permissive (100 attempts / 5 min / per IP). For an internet-facing proxy, tighten them:

```bash
DGP_RATE_LIMIT_MAX_ATTEMPTS=20
DGP_RATE_LIMIT_LOCKOUT_SECS=3600
```

Full knob list: [Rate limits](../reference/rate-limits.md).

### 7. Make the encryption-at-rest decision

Decide per backend, before real data lands: `none`, proxy-side AES-256-GCM, SSE-KMS, or SSE-S3. A later switch only affects new writes, and a lost proxy-AES key makes the objects unrecoverable. Make this choice deliberately instead of keeping the default. Options and key handling: [Encryption](../reference/encryption.md); trade-offs: [Encryption at rest](../explanation/encryption-at-rest.md).

### 8. Size the caches

Bump `DGP_CACHE_MB` to 1024+ (the reference-baseline LRU; hot-read workloads benefit most) and `DGP_METADATA_CACHE_MB` to 200+ if you list large prefixes repeatedly. The startup log warns with a `[cache]` prefix if you forgot. While you're there, check `DGP_MAX_OBJECT_SIZE` (default 100 MB) against your largest artefacts. The proxy writes the temporary files of delta objects larger than 16 MiB to the spool directory (`DGP_SPOOL_DIR`, default `<system temp>/dgp-spool`), so make sure that this directory is writable and has space. Remaining knobs: [Configuration](../reference/configuration.md).

## Verify

Run the production smoke checks against the live endpoint:

```bash
# 1. Unauthenticated access is denied
curl -s https://s3.acme.example/ | grep AccessDenied

# 2. Authenticated access works
aws s3 ls --endpoint-url https://s3.acme.example

# 3. Health endpoint answers (no credentials needed)
curl -s https://s3.acme.example/_/health

# 4. Admin GUI reports the expected auth mode
curl -s https://s3.acme.example/_/api/whoami
# Should return: {"mode":"iam"} (not "open")
```

If anything fails, [trace the request](trace-requests.md). The admission chain and the audit log tell you which layer denied it.

## Related

- [Secure your proxy](../tutorials/secure-your-proxy.md): the security baseline this guide assumes
- [How to upgrade the proxy](upgrade.md): when the next version lands
- [How to run multiple instances (HA)](run-multiple-instances.md): scaling past one host
- [Troubleshooting](troubleshooting.md): symptom-indexed fixes
