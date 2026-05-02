# DeltaGlider Proxy Helm chart

This chart deploys DeltaGlider Proxy as a single-port Kubernetes workload:

- S3-compatible API: `/`
- Admin UI: `/_/`
- Health: `/_/health`
- Metrics: `/_/metrics`

The container runs as non-root, mounts persistent state at `/data`, mounts the
rendered YAML config at `/data/deltaglider_proxy.yaml`, and uses an `emptyDir`
for `/tmp` so the runtime image can keep `readOnlyRootFilesystem: true`.

## Hello world on `kind`

Clone the repo if needed:

```bash
git clone https://github.com/beshu-tech/deltaglider_proxy.git
cd deltaglider_proxy
```

These commands use the published Docker image from the chart `appVersion`. For a locally built image, build it, run `kind load docker-image ...`, and set `image.repository`, `image.tag`, and `image.pullPolicy` during install.

> Local-only defaults: the default chart credentials are public. Do not expose this install through Ingress, a LoadBalancer, or a shared cluster.

```bash
kind create cluster --name dgp-hello

helm upgrade --install dgp ./charts/deltaglider-proxy \
  --namespace dgp \
  --create-namespace \
  --set persistence.size=1Gi \
  --kube-context kind-dgp-hello

kubectl --context kind-dgp-hello -n dgp rollout status deploy/dgp-deltaglider-proxy
```

Port-forward in a second terminal:

```bash
kubectl --context kind-dgp-hello -n dgp port-forward svc/dgp-deltaglider-proxy 19090:9000
```

Then:

```bash
curl -fsS http://127.0.0.1:19090/_/health
curl -fsS -X POST http://127.0.0.1:19090/_/api/admin/login \
  -H 'content-type: application/json' \
  --data '{"password":"change-me-in-production"}'
helm test dgp -n dgp --kube-context kind-dgp-hello
```

Open `http://127.0.0.1:19090/_/`.

The default development bootstrap password is:

```text
change-me-in-production
```

The default development SigV4 credentials are `admin` / `change-me-in-production`.

Clean up:

```bash
kind delete cluster --name dgp-hello
```

For production, provide stable secrets through `auth.existingSecret` or a
sealed/external secret. Do not rely on the chart defaults.

## Production secrets

Create a Secret outside Helm and point `auth.existingSecret` at it. This keeps credentials out of Helm values and release history.

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

Generate the bootstrap hash with:

```bash
printf '%s\n' 'your-admin-password' | deltaglider_proxy --set-bootstrap-password
```

Use the printed base64 `DGP_BOOTSTRAP_PASSWORD_HASH=...` value.

Install with:

```bash
helm upgrade --install dgp ./charts/deltaglider-proxy \
  --set auth.createSecret=false \
  --set auth.existingSecret=deltaglider-secrets
```

## Filesystem backend

The default config is intentionally simple:

```yaml
storage:
  filesystem: /data/storage
access:
  iam_mode: gui
advanced:
  listen_addr: "0.0.0.0:9000"
```

The PVC mounted at `/data` stores the filesystem backend, the generated
config DB, and any runtime-local state.

## S3 backend

Override `config.inline` and provide backend credentials:

```yaml
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

backendCredentials:
  accessKeyId: AKIA...
  secretAccessKey: ...
```

For production, put backend credentials in `auth.existingSecret` instead of
plain values.

## Ingress

The S3 API and admin UI are served from the same port. Route the whole host to
the service; do not split `/_/` and `/`.

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

## Verify

```bash
helm lint ./charts/deltaglider-proxy
helm template dgp ./charts/deltaglider-proxy
helm test dgp -n dgp
```
