# DeltaGlider Operator

The official Kubernetes operator for [DeltaGlider Proxy](../README.md). You declare a
`DeltaGliderProxy` resource; the operator manages everything needed to run it — including
the routing layer that makes **multi-pod deployments actually work with S3 multipart
uploads** (see [the multipart section](#multipart-uploads-and-multiple-pods-read-this)
before setting `replicas` above 1).

For a single-pod install, the plain [Helm chart](../charts/deltaglider-proxy) is also
fine. The operator earns its keep when you scale: it deploys and keeps in sync the
consistent-hashing router that a multi-pod deployment requires.

## What the operator manages

For each `DeltaGliderProxy` resource:

| Object | Purpose |
|---|---|
| StatefulSet `<name>` | The proxy pods, one PVC each for `/data` (config DB, filesystem-backend objects) |
| Service `<name>-pods` (headless) | Stable per-pod DNS names — the router's hash ring targets |
| ConfigMap `<name>-config` | Your inline DeltaGlider YAML, mounted at `/data/deltaglider_proxy.yaml` |
| Deployment `<name>-router` | HAProxy pods that consistent-hash S3 traffic by URL path |
| ConfigMap `<name>-router` | The rendered `haproxy.cfg` (regenerated when you scale) |
| Service `<name>` | The entrypoint (ClusterIP / NodePort / LoadBalancer) — point clients and Ingress here |

## Install

```bash
kubectl apply -f deploy/crd.yaml
kubectl apply -f deploy/operator.yaml
```

Create the credentials Secret and a `DeltaGliderProxy` resource (full example in
[`deploy/example.yaml`](deploy/example.yaml)):

```bash
kubectl create namespace dgp
kubectl -n dgp create secret generic dgp-env \
  --from-literal=DGP_ACCESS_KEY_ID=admin \
  --from-literal=DGP_SECRET_ACCESS_KEY=replace-me \
  --from-literal=DGP_BOOTSTRAP_PASSWORD_HASH="JDJiJDEyJ..."
kubectl apply -f deploy/example.yaml
kubectl -n dgp get dgp dgp        # phase: Ready when everything is up
```

Generate the bootstrap password hash with the proxy binary:
`printf '%s\n' 'your-admin-password' | deltaglider_proxy --set-bootstrap-password`.

## Multipart uploads and multiple pods (read this)

**The problem.** DeltaGlider Proxy keeps multipart-upload state (the upload id, the
received parts) in the memory and local disk of the pod that received
`CreateMultipartUpload`. Behind a naive round-robin Service, the SDK's parallel
`UploadPart` calls land on pods that have never heard of that upload id and fail with
`NoSuchUpload`. Kubernetes' usual answers don't help: S3 clients don't carry cookies,
and `sessionAffinity: ClientIP` collapses when many clients sit behind one NAT.

**What the operator does about it.** The managed HAProxy router consistent-hashes every
S3 request **by URL path** (`balance uri` + `hash-type consistent`). All requests for
one object key — `CreateMultipartUpload`, every `UploadPart`, `CompleteMultipartUpload` —
share a path, so they all pin to the same proxy pod. The hash ring is built from the
StatefulSet's stable DNS names, so every router pod computes the same mapping. This same
pinning also keeps all writes to one delta prefix on one pod, which the delta engine's
in-process reference lock requires.

**Consistent hashing is the only multipart strategy this deployment implements** — there
is no cross-pod multipart state sharing. The honest consequences:

- **Scaling the proxy pods remaps part of the ring.** Multipart uploads in flight during
  a scale-up/down can fail with `NoSuchUpload` and must be restarted by the client.
  Scale during quiet periods.
- **A proxy pod restart loses its in-flight multipart uploads** (same as a single-node
  restart). Clients must retry the whole upload.
- **Hot key prefixes concentrate on one pod.** Path-hashing trades even load spread for
  correctness.
- **Don't bypass the router.** Clients that reach the proxy pods directly (or through a
  different load balancer) break the pinning. The `<name>` Service is the only supported
  entrypoint.

The admin UI (`/_/`) is routed with source-IP stickiness instead, because admin sessions
are in-memory and IP-bound.

Two more operational notes:

- **Client IPs.** The router stamps `X-Forwarded-For` and the operator sets
  `DGP_TRUST_PROXY_HEADERS=true` on the proxy pods, so rate limits, `aws:SourceIp`
  conditions, and admin-session IP binding see the real client. This trusts whatever
  reaches the proxy pods — if untrusted workloads share the cluster network, add a
  NetworkPolicy restricting the proxy pods to router-only ingress.
- **Scaling down keeps PVCs.** The operator never deletes per-pod volumes; after
  scaling `replicas` down, reclaim the removed pods' PVCs manually if you want the
  storage back.

## Requirements for `replicas > 1`

Multipart routing is necessary but not sufficient. From the
[multi-instance contract](../docs/product/how-to/run-multiple-instances.md):

1. **S3 storage backend** — the filesystem backend is per-pod local disk; pods would
   each see different data.
2. **Same `DGP_BOOTSTRAP_PASSWORD_HASH` on every pod** — it encrypts the shared IAM DB.
   The operator injects one Secret into all pods, which guarantees this.
3. **Config sync bucket** (`advanced.config_sync_bucket`) — syncs IAM users/groups
   between pods and hosts the replication leader leases.
4. **One admin-GUI writer** — make IAM changes through one pod (or use
   `iam_mode: declarative`); IAM sync is not multi-master.

## Spec reference

```yaml
spec:
  replicas: 3                  # proxy pods (default 1)
  image: beshultd/deltaglider_proxy:1.16.0   # default: operator's pinned release
  configYaml: |                # inline DeltaGlider YAML (no secrets here)
    storage:
      s3: https://s3.eu-central-1.amazonaws.com
      region: eu-central-1
  envFromSecret: dgp-env       # Secret with DGP_* env vars (credentials)
  storage:
    size: 20Gi                 # per-pod PVC for /data (default 10Gi)
    storageClass: fast-ssd     # optional
  router:
    replicas: 2                # HAProxy pods (default 2, stateless)
    image: haproxy:3.0-alpine  # default
  service:
    type: LoadBalancer         # default ClusterIP
  resources:                   # proxy container resources
    requests: { cpu: "500m", memory: 1Gi }
    limits: { memory: 4Gi }
```

Status: `kubectl get dgp` shows `Replicas` and `Ready` (phase `Ready` /
`Progressing`); `status.message` has the pod counts.

## TLS

Terminate TLS at your Ingress or LoadBalancer and route the whole host to the `<name>`
Service. In-cluster traffic between router and pods is plain HTTP — the router must see
paths to hash them, so don't enable DGP-level TLS in this topology.

## Development

```bash
cd operator
cargo test          # builders + CRD-drift tests, no cluster needed
cargo run -- crd    # regenerate deploy/crd.yaml after changing src/crd.rs
docker build -f Dockerfile -t beshultd/deltaglider-operator:dev .
```

The operator is a separate crate — it does not affect the proxy's build or CI gate.
