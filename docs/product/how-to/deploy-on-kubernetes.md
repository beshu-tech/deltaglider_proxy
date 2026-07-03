# How to deploy on Kubernetes with Helm

This guide shows you how to run DeltaGlider Proxy in production on Kubernetes with the official Helm chart. For a local proof-of-concept on `kind`, do the [Kubernetes hello world tutorial](../tutorials/kubernetes-hello-world.md) first.

The chart lives in `charts/deltaglider-proxy` and is intentionally boring: one `Deployment`, one `Service`, a PVC for `/data`, a rendered config file, and optional Ingress/HPA/PDB/NetworkPolicy. It deploys the same single-port binary used everywhere else — S3 API on `/`, admin UI on `/_/`, health on `/_/health`, metrics on `/_/metrics`.

## 1. Create the credentials Secret

Keep credentials outside the values file: create a Kubernetes Secret outside Helm and point the chart at it.

Minimum filesystem-backed Secret:

```yaml
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
  DGP_BE_AWS_ACCESS_KEY_ID: "..."
  DGP_BE_AWS_SECRET_ACCESS_KEY: "..."
```

Generate the bootstrap password hash with the binary, and use the printed base64 `DGP_BOOTSTRAP_PASSWORD_HASH=...` value — do not paste the plaintext password into the Secret:

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

**If you use the filesystem backend** (the chart default), object data and the encrypted IAM DB live on the chart PVC:

```yaml
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
persistence:
  enabled: true
  storageClass: fast-ssd
  size: 500Gi
```

**If you use an S3 backend**, render the S3 config via `config.inline` and keep the backend credentials in the Secret — they are deliberately not stored in `config.inline`; the proxy reads them from `DGP_BE_AWS_ACCESS_KEY_ID` / `DGP_BE_AWS_SECRET_ACCESS_KEY`:

```yaml
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

The binary derives the encrypted IAM database path from `DGP_CONFIG`: `dirname($DGP_CONFIG)/deltaglider_config.db`. The chart therefore mounts the rendered config as `/data/deltaglider_proxy.yaml`, so the config DB lands at `/data/deltaglider_config.db` — both on the writable PVC. Do not mount the config under a read-only ConfigMap directory such as `/config`, or IAM will be disabled because SQLite cannot create the encrypted DB.

## 4. Expose it with Ingress

Route the whole host to the service. The admin UI and S3 API share one listener — do not split paths.

```yaml
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
```

Set `DGP_TRUST_PROXY_HEADERS=true` only when the proxy is actually behind a trusted ingress controller — it affects rate limiting and IAM `aws:SourceIp` conditions. If the pod is reachable without the ingress, leave it `false`. If your ingress controller has a request read-timeout (most do), raise it for large uploads — see [How to serve TLS](serve-tls.md).

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

The chart ships the hardening the Dockerfile expects — leave these alone:

- non-root user/group `999`
- `readOnlyRootFilesystem: true`, `allowPrivilegeEscalation: false`, all Linux capabilities dropped
- service account token automount disabled by default
- `/tmp` provided by `emptyDir`; persistent `/data` volume for mutable state

## Replicas

`replicaCount` defaults to `1` — keep it there unless the pods share IAM state.

With config sync set up, replication rules elect one leader per rule through an S3-CAS lease in the sync bucket (default `lease_ttl: "300s"`, `heartbeat_interval: "60s"`); a dead leader's lease lapses and a peer takes over automatically. Lifecycle and maintenance jobs still use node-local database leases, so they may run on more than one pod (idempotent — wasteful, not corrupting). Do not scale above one replica if each pod has its own independent `/data/deltaglider_config.db` — in that shape, each pod is an independent control plane. To run more than one instance, set up config sync first: [How to run multiple instances (HA)](run-multiple-instances.md).

## Useful values

| Value | Purpose |
|---|---|
| `image.repository` / `image.tag` | Container image. Defaults to chart `appVersion`. |
| `auth.existingSecret` | Secret created outside Helm. Minimum keys: `DGP_ACCESS_KEY_ID`, `DGP_SECRET_ACCESS_KEY`, `DGP_BOOTSTRAP_PASSWORD_HASH`; add `DGP_BE_AWS_*` for S3 backends. Keeps credentials out of Helm values and release history. |
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

The liveness/readiness probes hit `GET /_/health`; a pod stuck out of `Running` usually means the PVC didn't bind or the config failed validation — `kubectl logs` shows the startup error.

## Related

- [Kubernetes hello world](../tutorials/kubernetes-hello-world.md) — the local `kind` walkthrough
- [How to take a proxy to production](go-to-production.md) — the full production checklist
- [How to serve TLS](serve-tls.md) — ingress timeouts and forwarded headers
- [Configuration reference](../reference/configuration.md) — every field in `config.inline`
