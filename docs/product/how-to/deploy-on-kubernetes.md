# How to deploy on Kubernetes with Helm

This guide shows you how to run DeltaGlider Proxy in production on Kubernetes with the official Helm chart. For a local proof-of-concept on `kind`, do the [Kubernetes hello world tutorial](../tutorials/kubernetes-hello-world.md) first.

The chart lives in `charts/deltaglider-proxy` and is intentionally minimal: one `Deployment`, one `Service`, a PVC for `/data`, a rendered config file, and an optional Ingress, HPA, PDB, and NetworkPolicy. It deploys the same single-port binary that every other deployment uses. The S3 API is on `/`, the admin UI on `/_/`, health on `/_/health`, and metrics on `/_/metrics`.

The configuration of the proxy lives in the chart values (`config.inline`), and you set it before the install, so this guide uses YAML only. The chart mounts the rendered config file read-only. A change that you make later in the admin UI therefore works in the running pod, but the proxy cannot write it into the file, and the change is lost when the pod restarts. Settings that need a restart (the listener, TLS, the reference cache size and the sync bucket) never take effect from the UI at all, and a backup restore that includes the config answers `409 config_file_read_only`. The admin UI shows a warning banner about the read-only file. To keep a change, put it into `config.inline` (export the YAML from the UI if you tried it there first) and run `helm upgrade`. [Two ways to configure DeltaGlider](../explanation/two-ways-to-configure.md) explains how the file and the UI relate. IAM users and groups are not in the file in the default `iam_mode: gui`: they live in the encrypted database on the PVC, so the UI changes to them persist.

## 1. Create the credentials Secret

Keep the credentials outside the values file. Create a Kubernetes Secret outside Helm and point the chart at it.

Minimum filesystem-backed Secret:

```yaml
# not-proxy-config: kubernetes secret
apiVersion: v1
kind: Secret
metadata:
  name: deltaglider-secrets
type: Opaque
stringData:
  DGP_ACCESS_KEY_ID: admin
  DGP_SECRET_ACCESS_KEY: replace-me
  DGP_BOOTSTRAP_PASSWORD_HASH: "JDJiJDEyJ..."
```

If you use an S3 storage backend, add the backend credentials too:

```yaml
# not-proxy-config: kubernetes secret (more stringData keys)
  DGP_BE_AWS_ACCESS_KEY_ID: "..."
  DGP_BE_AWS_SECRET_ACCESS_KEY: "..."
```

Generate the bootstrap password hash with the binary, and use the printed base64 `DGP_BOOTSTRAP_PASSWORD_HASH=...` value. Do not paste the plaintext password into the Secret:

```bash
printf '%s\n' 'your-admin-password' | deltaglider_proxy --set-bootstrap-password
```

## 2. Install the chart

```bash
helm upgrade --install dgp ./charts/deltaglider-proxy \
  --namespace dgp \
  --create-namespace \
  --set auth.createSecret=false \
  --set auth.existingSecret=deltaglider-secrets
```

## 3. Choose the storage backend

**If you use the filesystem backend** (the chart default), object data and the encrypted IAM DB live on the chart PVC. The default `config.inline` renders this proxy config:

```yaml
# validate
storage:
  filesystem: /data/storage
access:
  iam_mode: gui
advanced:
  listen_addr: "0.0.0.0:9000"
  log_level: "deltaglider_proxy=info,tower_http=warn"
```

Size the PVC for your data:

```yaml
# not-proxy-config: helm values
persistence:
  enabled: true
  storageClass: fast-ssd
  size: 500Gi
```

**If you use an S3 backend**, render the S3 config through `config.inline` and keep the backend credentials in the Secret. The chart deliberately does not store them in `config.inline`. The proxy reads them from `DGP_BE_AWS_ACCESS_KEY_ID` / `DGP_BE_AWS_SECRET_ACCESS_KEY`:

```yaml
# not-proxy-config: helm values (config.inline holds the proxy config)
auth:
  createSecret: false
  existingSecret: deltaglider-secrets

config:
  inline: |
    storage:
      s3: https://s3.eu-central-1.amazonaws.com
      region: eu-central-1
      force_path_style: false
    access:
      iam_mode: gui
    advanced:
      listen_addr: "0.0.0.0:9000"
      cache_size_mb: 2048
      log_level: "deltaglider_proxy=info,tower_http=warn"
```

### Why the config is mounted under `/data`

The binary derives the encrypted IAM database path from `DGP_CONFIG`: `dirname($DGP_CONFIG)/deltaglider_config.db`. The chart therefore mounts the rendered config as the single file `/data/deltaglider_proxy.yaml`, read-only from a ConfigMap, over the writable PVC at `/data`. The config DB then lands at `/data/deltaglider_config.db`, on the PVC, where the proxy can write it. Do not mount the config under a read-only ConfigMap directory such as `/config`, or IAM will be disabled because SQLite cannot create the encrypted DB.

## 4. Expose it with Ingress

Route the whole host to the service. The admin UI and S3 API share one listener, so do not split the paths.

```yaml
# not-proxy-config: helm values
ingress:
  enabled: true
  className: nginx
  annotations:
    nginx.ingress.kubernetes.io/proxy-body-size: "0"
  hosts:
    - host: s3.acme.example
      paths:
        - path: /
          pathType: Prefix
  tls:
    - secretName: dgp-tls
      hosts:
        - s3.acme.example

env:
  - name: DGP_TRUST_PROXY_HEADERS
    value: "true"
  - name: DGP_TRUSTED_PROXY_CIDRS
    value: "10.42.0.0/16"   # the pod network of the ingress controller
```

Set `DGP_TRUST_PROXY_HEADERS=true` only when the proxy is behind a trusted ingress controller. The proxy then reads `X-Forwarded-For` only on a connection from a network in `DGP_TRUSTED_PROXY_CIDRS`, and uses that client address for the per-IP rate limit, admin-session IP binding, admission `source_ip` rules, and IAM `aws:SourceIp` conditions. Set `DGP_TRUSTED_PROXY_CIDRS` to the pod network of the ingress controller; the proxy refuses to start with `DGP_TRUST_PROXY_HEADERS=true` and no `DGP_TRUSTED_PROXY_CIDRS`. If the pod is reachable without the ingress, leave `DGP_TRUST_PROXY_HEADERS` set to `false`. If your ingress controller has a request read-timeout (most do), raise it for large uploads. See [How to serve TLS](serve-tls.md).

## 5. Validate before deploy

```bash
helm lint ./charts/deltaglider-proxy
helm template dgp ./charts/deltaglider-proxy
helm lint ./charts/deltaglider-proxy -f charts/deltaglider-proxy/examples/s3-values.yaml
```

Validate the application config inside `config.inline` separately:

```bash
deltaglider_proxy config lint deltaglider_proxy.yaml
```

## Security defaults

The chart ships the hardening that the Dockerfile expects. Leave these settings alone:

- non-root user/group `999`
- `readOnlyRootFilesystem: true`, `allowPrivilegeEscalation: false`, all Linux capabilities dropped
- service account token automount disabled by default
- `/tmp` provided by `emptyDir`; persistent `/data` volume for mutable state

## Replicas

`replicaCount` defaults to `1`. Keep it there. The chart has no multipart-aware
routing. With more than one replica behind a round-robin Service, S3 multipart uploads
fail with `NoSuchUpload`, because the state of an upload lives only on the pod that
started it. For a multi-pod deployment, use the official operator instead. It deploys
the consistent-hashing router that this requires: [How to scale out with the Kubernetes
operator](scale-out-with-the-kubernetes-operator.md).

With config sync set up, replication rules elect one leader per rule through an S3-CAS lease in the sync bucket (default `lease_ttl: "300s"`, `heartbeat_interval: "60s"`); a dead leader's lease lapses and a peer takes over automatically. Lifecycle, maintenance, and parity audit jobs still use node-local database leases, so they may run on more than one pod. Their operations are idempotent, so this wastes work but does not corrupt data. Do not scale above one replica if each pod has its own independent `/data/deltaglider_config.db`. In that shape, each pod is an independent control plane. To run more than one instance, set up config sync and a shared `DGP_CONFIG_DB_KEY` first: [How to run multiple instances (HA)](run-multiple-instances.md).

## Useful values

| Value | Purpose |
|---|---|
| `image.repository` / `image.tag` | Container image. Defaults to chart `appVersion`. |
| `auth.existingSecret` | Secret created outside Helm. Minimum keys: `DGP_ACCESS_KEY_ID`, `DGP_SECRET_ACCESS_KEY`, `DGP_BOOTSTRAP_PASSWORD_HASH`; add `DGP_BE_AWS_*` for S3 backends, and `DGP_CONFIG_DB_KEY` when config sync is set. Keeps credentials out of Helm values and release history. |
| `auth.configDbKey` | `DGP_CONFIG_DB_KEY` when the chart creates the Secret. Leave it empty for one replica: the proxy then keeps a generated key file next to the IAM database on the PVC. Required, and the same on every replica, with config sync. |
| `config.inline` | Canonical DeltaGlider YAML rendered into `/data/deltaglider_proxy.yaml`. |
| `persistence.*` | PVC settings for `/data`. |
| `ingress.*` | Optional host/TLS routing. |
| `env` / `envFrom` | Extra non-secret env and env sources. |
| `backendCredentials.*` | Convenience S3 backend env values when the chart creates the Secret. |
| `networkPolicy.*` / `autoscaling.*` | Optional pod-level network policy / HPA. |

## Verify

```bash
kubectl -n dgp rollout status deploy/dgp-deltaglider-proxy
helm test dgp -n dgp        # starts a curl pod; fails unless /_/health returns success
```

Then from outside the cluster:

```bash
curl -fsS https://s3.acme.example/_/health
aws --endpoint-url https://s3.acme.example s3 ls
```

The liveness probe hits `GET /_/health` (fast, no I/O); the readiness probe hits `GET /_/ready`, which really probes the storage backends and the config DB. A pod stays out of rotation when it cannot reach any of its backends. With several backends, the pod stays ready while at least one backend answers, and the buckets on a backend that is down answer `503`. A pod stuck out of `Running` usually means the PVC didn't bind or the config failed validation. `kubectl logs` shows the startup error.

## Related

- [Kubernetes hello world](../tutorials/kubernetes-hello-world.md): the local `kind` walkthrough
- [How to scale out with the Kubernetes operator](scale-out-with-the-kubernetes-operator.md): multi-pod deployments
- [How to take a proxy to production](go-to-production.md): the full production checklist
- [How to serve TLS](serve-tls.md): ingress timeouts and forwarded headers
- [Configuration reference](../reference/configuration.md): every field in `config.inline`
