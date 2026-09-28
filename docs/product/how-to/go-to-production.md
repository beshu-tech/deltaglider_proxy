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

Decide where the bytes live and say so explicitly. Do not take the filesystem default into production. In the admin UI, add the backend on **Storage → Backends** with **Add Backend**, and select **Set as default backend**. [How to route a bucket to a different backend](route-a-bucket-to-a-backend.md) shows every step with screenshots. The backend is saved when you create it, so this step has no **Review & apply**.

In YAML, the backend goes into the `storage` section ([why the UI and the file hold the same configuration](../explanation/two-ways-to-configure.md)). Keep the credentials out of the file with `${env:NAME}` references:

```yaml
# validate
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

S3 clients expect HTTPS. Terminate TLS either at the proxy itself or at a reverse proxy in front of it. At the proxy, turn on **Enable TLS** in the **TLS** card of **System → System**, fill in **Certificate path** and **Private key path**, apply, and restart. If you use a reverse proxy, you **must** raise its read timeout, or large uploads fail with 502/504. See [How to serve TLS](serve-tls.md) for both options with screenshots.

### 4. Set up backups

Take a Full Backup zip before you call anything production (**Download backup** on **System → System**, or `GET /_/api/admin/backup`), and put it on a schedule. Know the difference between the three mechanisms (Full Backup, DB snapshot, S3 config sync) before you need them at 3 a.m. See [How to back up and restore](back-up-and-restore.md).

### 5. Wire up monitoring

Scrape `/_/metrics` with Prometheus, import the dashboard panels, and install the alert rules for error rate, p95 latency, cache hit ratio, codec saturation, and instance down. `/_/metrics` answers without authentication unless you set `DGP_METRICS_BEARER_TOKEN`; set it for an internet-facing proxy. The token exists only as an environment variable. See [How to monitor with Prometheus and Grafana](monitor-with-prometheus.md).

### 6. Tighten rate limits

The auth-endpoint defaults are permissive (100 attempts / 5 min / per IP). For an internet-facing proxy, tighten them. The rate limits exist only as environment variables; the YAML file and the admin UI cannot set them:

```bash
DGP_RATE_LIMIT_MAX_ATTEMPTS=20
DGP_RATE_LIMIT_LOCKOUT_SECS=3600
```

Full knob list: [Rate limits](../reference/rate-limits.md).

### 7. Make the encryption-at-rest decision

Decide per backend, before real data lands: `none`, proxy-side AES-256-GCM, SSE-KMS, or SSE-S3. A later switch only affects new writes, and a lost proxy-AES key makes the objects unrecoverable. Make this choice deliberately instead of keeping the default. Options and key handling: [Encryption](../reference/encryption.md); trade-offs: [Encryption at rest](../explanation/encryption-at-rest.md).

### 8. Size the caches

Raise the reference cache (the LRU of reference baselines) to 1024 MB or more, because hot-read workloads benefit most from it. Raise the metadata cache to 200 MB or more if you list large prefixes repeatedly. The startup log warns with a `[cache]` prefix if you forgot. Both sizes apply at once, because the proxy rebuilds its caches when you apply a new size. The rebuilt caches start empty.

In the admin UI:

1. In the sidebar, open **System → System** (`/_/admin/system`), and scroll to the **Caches** card.
2. Type `1024` in **Reference cache size (MB)** and `200` in **Metadata cache size (MB)**.
3. Click **Review & apply** in the bar above the card, check the diff, and click **Apply and Persist**.

   ![The Caches card of the System page holds a reference cache of 1024 MB and a metadata cache of 200 MB; callout 1 marks the two cache fields and callout 2 marks Review & apply.](/_/screenshots/production-caches.webp)

The environment variables `DGP_CACHE_MB` and `DGP_METADATA_CACHE_MB` override these fields. When one of them is set, the admin UI shows the field as read-only with a from env badge. While you're there, check the largest object size (`max_object_size`, default 100 MiB) against your largest artefacts. The admin UI shows it as **Maximum object size (MiB)** on **Storage → Buckets**, and `DGP_MAX_OBJECT_SIZE` overrides it. The proxy writes the temporary files of delta objects larger than 16 MiB to the spool directory (`DGP_SPOOL_DIR`, default `<system temp>/dgp-spool`), so make sure that this directory is writable and has space. The spool settings exist only as environment variables. Remaining knobs: [Configuration](../reference/configuration.md).

## The same change in YAML

Items 2, 3 and 8 write this configuration into `deltaglider_proxy.yaml` ([why the UI and the file hold the same configuration](../explanation/two-ways-to-configure.md)). The backend credentials stay out of the file as `${env:NAME}` references, which the proxy expands from its environment when it loads the file:

```yaml
# validate
storage:
  backends:
    - name: hetzner-fsn1
      type: s3
      endpoint: https://fsn1.your-objectstorage.com
      region: fsn1
      force_path_style: true
      access_key_id: ${env:S3_ACCESS_KEY_ID}
      secret_access_key: ${env:S3_SECRET_ACCESS_KEY}
  default_backend: hetzner-fsn1
advanced:
  tls:
    enabled: true
    cert_path: /etc/ssl/certs/proxy.pem
    key_path: /etc/ssl/private/proxy-key.pem
  cache_size_mb: 1024
  metadata_cache_mb: 200
```

Apply the file with a restart, because the TLS settings are read only at startup. When the config file is mounted read-only, as on Docker Compose with `:ro`, the Helm chart and the Kubernetes operator, a change in the admin UI lasts only until the next restart, and the TLS settings of item 3 never take effect from the UI. On those platforms, make these changes in the YAML of your deployment.

## Verify

Run the production smoke checks against the live endpoint:

```bash
# 1. Unauthenticated access is denied
curl -s https://s3.acme.example/ | grep AccessDenied

# 2. Authenticated access works
aws s3 ls --endpoint-url https://s3.acme.example

# 3. Health endpoint answers (no credentials needed)
curl -s https://s3.acme.example/_/health

# 4. The proxy reports the expected auth mode
curl -s https://s3.acme.example/_/api/whoami
# "mode" is "iam" once IAM users exist, or "bootstrap" with only the
# bootstrap key pair. It must never be "open".
```

If anything fails, [trace the request](trace-requests.md). The admission chain and the audit log tell you which layer denied it.

## Related

- [Secure your proxy](../tutorials/secure-your-proxy.md): the security baseline this guide assumes
- [How to upgrade the proxy](upgrade.md): when the next version lands
- [How to run multiple instances (HA)](run-multiple-instances.md): scaling past one host
- [Troubleshooting](troubleshooting.md): symptom-indexed fixes
