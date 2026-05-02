# Kubernetes / Helm deployment

*Run DeltaGlider Proxy in Kubernetes with the official Helm chart.*

The chart lives in `charts/deltaglider-proxy` and deploys the same single-port binary used by Docker:

- S3 API on `/`
- Admin UI on `/_/`
- Health on `/_/health`
- Metrics on `/_/metrics`

The chart is intentionally boring: one `Deployment`, one `Service`, a PVC for `/data`, a rendered config file, and optional Ingress/HPA/PDB/NetworkPolicy.

## Hello world on `kind`

Use this when you want a clean local proof that the chart boots, serves the UI, and accepts S3 traffic.

Prerequisites:

- the DeltaGlider Proxy repository checked out locally
- Docker or another container runtime usable by `kind`
- `kind`
- `kubectl`
- `helm`
- optional: `aws` CLI for the S3 smoke test

Clone the repo if you have not already:

```bash
git clone https://github.com/beshu-tech/deltaglider_proxy.git
cd deltaglider_proxy
```

This walkthrough uses the published Docker image from the chart `appVersion`. If you are testing a locally built image, build it and load it into kind first:

```bash
docker build -t deltaglider_proxy:dev .
kind load docker-image deltaglider_proxy:dev --name dgp-hello
```

Then install with `--set image.repository=deltaglider_proxy --set image.tag=dev --set image.pullPolicy=IfNotPresent`.

> **Local-only defaults**
>
> The default chart credentials are intentionally public so the chart can be smoke-tested on localhost. Do not expose a default install through Ingress, a LoadBalancer, a tunnel, or a shared cluster.

### 1. Create a disposable cluster

```bash
kind create cluster --name dgp-hello
kubectl cluster-info --context kind-dgp-hello
```

### 2. Install DeltaGlider Proxy

```bash
helm upgrade --install dgp ./charts/deltaglider-proxy \
  --namespace dgp \
  --create-namespace \
  --set persistence.size=1Gi \
  --kube-context kind-dgp-hello
```

This installs the default filesystem-backed chart. It stores object data and the encrypted IAM DB on the chart PVC under `/data`.

Wait for the pod:

```bash
kubectl --context kind-dgp-hello -n dgp rollout status deploy/dgp-deltaglider-proxy
kubectl --context kind-dgp-hello -n dgp get pods,pvc,svc
```

Expected:

- pod is `1/1 Running`
- PVC is `Bound`
- service exposes port `9000`

### 3. Port-forward the service

Keep this running in a separate terminal:

```bash
kubectl --context kind-dgp-hello -n dgp port-forward svc/dgp-deltaglider-proxy 19090:9000
```

Open the admin UI:

```text
http://127.0.0.1:19090/_/
```

The default development bootstrap password is:

```text
change-me-in-production
```

That password is only there so the chart is testable out of the box. Override it before using the chart outside a throwaway cluster.

### 4. Verify health and login

```bash
curl -fsS http://127.0.0.1:19090/_/health
```

Expected shape:

```json
{"status":"healthy","backend":"ready"}
```

Verify the bootstrap login endpoint:

```bash
curl -fsS -X POST http://127.0.0.1:19090/_/api/admin/login \
  -H 'content-type: application/json' \
  --data '{"password":"change-me-in-production"}'
```

Expected:

```json
{"ok":true}
```

### 5. Optional S3 smoke test

The default chart also creates development SigV4 credentials:

```text
access key: admin
secret key: change-me-in-production
```

With the AWS CLI:

```bash
export AWS_ACCESS_KEY_ID=admin
export AWS_SECRET_ACCESS_KEY=change-me-in-production
export AWS_DEFAULT_REGION=us-east-1
aws configure set s3.addressing_style path

aws --endpoint-url http://127.0.0.1:19090 s3 mb s3://hello
printf 'hello from kind\n' > /tmp/dgp-hello.txt
aws --endpoint-url http://127.0.0.1:19090 s3 cp /tmp/dgp-hello.txt s3://hello/hello.txt
aws --endpoint-url http://127.0.0.1:19090 s3 cp s3://hello/hello.txt -
```

Expected output:

```text
hello from kind
```

### 6. Run the Helm test hook

```bash
helm test dgp -n dgp --kube-context kind-dgp-hello
```

The test hook starts a small curl pod and fails unless `/_/health` returns success.

### 7. Clean up

```bash
kind delete cluster --name dgp-hello
```

## Production install

For production, keep credentials outside the values file and use a Kubernetes Secret that is created outside Helm.

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

S3-backed Secret:

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
  DGP_BE_AWS_ACCESS_KEY_ID: "..."
  DGP_BE_AWS_SECRET_ACCESS_KEY: "..."
```

Install with:

```bash
helm upgrade --install dgp ./charts/deltaglider-proxy \
  --namespace dgp \
  --create-namespace \
  --set auth.createSecret=false \
  --set auth.existingSecret=deltaglider-secrets
```

Generate the bootstrap password hash with the binary:

```bash
printf '%s\n' 'your-admin-password' | deltaglider_proxy --set-bootstrap-password
```

Use the printed base64 `DGP_BOOTSTRAP_PASSWORD_HASH=...` value in the Secret. Do not paste the plaintext password there.

## Why the config is mounted under `/data`

The binary derives the encrypted IAM database path from `DGP_CONFIG`:

```text
dirname($DGP_CONFIG)/deltaglider_config.db
```

So the chart mounts the rendered config as:

```text
/data/deltaglider_proxy.yaml
```

That means the config DB lands at:

```text
/data/deltaglider_config.db
```

Both files live on the writable PVC. Do not mount the config under a read-only ConfigMap directory such as `/config`, or IAM will be disabled because SQLite cannot create the encrypted DB.

## Filesystem backend

The default chart config is filesystem-backed:

```yaml
storage:
  filesystem: /data/storage
access:
  iam_mode: gui
advanced:
  listen_addr: "0.0.0.0:9000"
  log_level: "deltaglider_proxy=info,tower_http=warn"
```

State stored on the PVC:

- object data under `/data/storage`
- `deltaglider_config.db`
- any runtime-local files created by the process

Tune the PVC:

```yaml
persistence:
  enabled: true
  storageClass: fast-ssd
  size: 500Gi
```

## S3 backend

For S3-compatible storage, render S3 config and provide backend credentials through the Secret:

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
  DGP_BE_AWS_ACCESS_KEY_ID: "..."
  DGP_BE_AWS_SECRET_ACCESS_KEY: "..."
```

Values:

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

The S3 backend credentials are deliberately not stored in `config.inline`. They are read from `DGP_BE_AWS_ACCESS_KEY_ID` and `DGP_BE_AWS_SECRET_ACCESS_KEY`.

## Ingress

Route the whole host to the service. The admin UI and S3 API share one listener, so do not split paths.

```yaml
ingress:
  enabled: true
  className: nginx
  annotations:
    nginx.ingress.kubernetes.io/proxy-body-size: "0"
  hosts:
    - host: dgp.example.com
      paths:
        - path: /
          pathType: Prefix
  tls:
    - secretName: dgp-tls
      hosts:
        - dgp.example.com

env:
  - name: DGP_TRUST_PROXY_HEADERS
    value: "true"
```

`DGP_TRUST_PROXY_HEADERS=true` should only be enabled when the proxy is actually behind a trusted reverse proxy or ingress controller. It affects rate limiting and IAM `aws:SourceIp` conditions.

## Security defaults

The chart uses the hardening expected by the Dockerfile:

- non-root user/group `999`
- `readOnlyRootFilesystem: true`
- `allowPrivilegeEscalation: false`
- all Linux capabilities dropped
- service account token automount disabled by default
- `/tmp` provided by `emptyDir`
- persistent `/data` volume for mutable state

## Probes and Helm test

The chart probes:

```text
GET /_/health
```

Run:

```bash
helm test dgp -n dgp
```

The test hook starts a tiny curl pod and fails unless `/_/health` returns success.

## Validating before deploy

Render and lint:

```bash
helm lint ./charts/deltaglider-proxy
helm template dgp ./charts/deltaglider-proxy
helm lint ./charts/deltaglider-proxy -f charts/deltaglider-proxy/examples/s3-values.yaml
```

Validate the application config inside `config.inline` separately with:

```bash
deltaglider_proxy config lint deltaglider_proxy.yaml
```

## Useful values

| Value | Purpose |
|---|---|
| `image.repository` / `image.tag` | Container image. Defaults to chart `appVersion`. |
| `auth.existingSecret` | Name of a Kubernetes Secret you created outside Helm. When set, the chart reads `DGP_*` env vars from it and does not store credentials in Helm values/release history. Minimum keys: `DGP_ACCESS_KEY_ID`, `DGP_SECRET_ACCESS_KEY`, `DGP_BOOTSTRAP_PASSWORD_HASH`. Add `DGP_BE_AWS_ACCESS_KEY_ID` and `DGP_BE_AWS_SECRET_ACCESS_KEY` for S3 backends. |
| `auth.bootstrapPasswordHash` | Development Secret value for `DGP_BOOTSTRAP_PASSWORD_HASH`. |
| `config.inline` | Canonical DeltaGlider YAML rendered into `/data/deltaglider_proxy.yaml`. |
| `persistence.*` | PVC settings for `/data`. |
| `ingress.*` | Optional host/TLS routing. |
| `env` / `envFrom` | Extra non-secret env and env sources. |
| `backendCredentials.*` | Convenience S3 backend env values when the chart creates the Secret. |
| `networkPolicy.*` | Optional pod-level network policy. |
| `autoscaling.*` | Optional HPA. |
