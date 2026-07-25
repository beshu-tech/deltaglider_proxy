# DeltaGlider Operator

The official Kubernetes operator for [DeltaGlider Proxy](../README.md). You declare a
`DeltaGliderProxy` resource, and the operator manages everything that is needed to run
it — including the routing layer that makes **multi-pod deployments actually work with
S3 multipart uploads**. Please read
[the multipart section](#multipart-uploads-and-multiple-pods-read-this) before you set
`replicas` above 1.

For a single-pod installation, the plain [Helm chart](../charts/deltaglider-proxy) is
also fine. The operator earns its keep when you scale: it deploys, and keeps up to
date, the consistent-hashing router that a multi-pod deployment requires.

## What the operator manages

For each `DeltaGliderProxy` resource, the operator creates and maintains:

| Object | Purpose |
|---|---|
| StatefulSet `<name>` | The proxy pods. Each pod gets its own persistent volume for `/data`, which holds the encrypted config database and, if you use the filesystem backend, the stored objects. |
| Service `<name>-pods` (headless) | Gives every pod a stable DNS name. The router builds its hash ring from these names. |
| ConfigMap `<name>-config` | Your inline DeltaGlider YAML, mounted into every pod at `/data/deltaglider_proxy.yaml`. |
| Deployment `<name>-router` | The HAProxy pods that route S3 traffic by consistent hashing on the URL path. |
| ConfigMap `<name>-router` | The rendered `haproxy.cfg`. The operator regenerates it whenever you scale. |
| Service `<name>` | The entrypoint (ClusterIP, NodePort, or LoadBalancer). Point your clients and your Ingress at this Service. |

## Install

```bash
kubectl apply -f deploy/crd.yaml
kubectl apply -f deploy/operator.yaml
```

Create the credentials Secret and a `DeltaGliderProxy` resource. A complete example
lives in [`deploy/example.yaml`](deploy/example.yaml):

```bash
kubectl create namespace dgp
kubectl -n dgp create secret generic dgp-env \
  --from-literal=DGP_ACCESS_KEY_ID=admin \
  --from-literal=DGP_SECRET_ACCESS_KEY=replace-me \
  --from-literal=DGP_BOOTSTRAP_PASSWORD_HASH="JDJiJDEyJ..."
kubectl apply -f deploy/example.yaml
kubectl -n dgp get dgp dgp        # the phase becomes Ready when everything is up
```

Generate the bootstrap password hash with the proxy binary:

```bash
printf '%s\n' 'your-admin-password' | deltaglider_proxy --set-bootstrap-password
```

## Multipart uploads and multiple pods (read this)

**The problem.** DeltaGlider Proxy keeps the state of a multipart upload — the upload
id and the parts received so far — in the memory and on the local disk of the pod that
received the `CreateMultipartUpload` request. No other pod knows that this upload
exists. Behind a plain round-robin Service, the SDK sends its parallel `UploadPart`
requests to whichever pods the load balancer picks, and every request that reaches a
pod other than the original one is rejected with a `NoSuchUpload` error. The usual
Kubernetes remedies do not help here: S3 clients do not carry cookies, so cookie-based
session affinity cannot apply, and `sessionAffinity: ClientIP` stops working when many
clients share one IP address behind a NAT gateway.

**What the operator does about it.** The managed HAProxy router chooses the target pod
for every S3 request by hashing the **directory of the URL path** — the request path
with its last segment removed (`balance hash path,regsub([^/]*$,x)` together with
`hash-type consistent`; the query string is never part of the hash). Everything inside
one directory therefore reaches the same pod: every object key in that prefix, and for
each of those keys the `CreateMultipartUpload` request, every `UploadPart` request,
and the final `CompleteMultipartUpload` request. The directory, not the full path, is
the right unit because a delta prefix is shared state: all keys in one prefix update
the same reference file, and the lock that serialises those updates lives inside a
single process. Hashing per directory sends all of that work to one pod, which keeps
the lock effective. The hash ring is built from the StatefulSet's stable DNS names,
which means every router pod computes exactly the same mapping.

**Consistent hashing is the only multipart strategy this deployment implements.** The
pods do not share any multipart state with each other. That has honest consequences,
and you should accept them before going live:

- **Scaling the proxy pods moves part of the hash ring.** A multipart upload that is in
  flight during a scale-up or scale-down can fail with `NoSuchUpload` if its key now
  maps to a different pod, and the client has to restart that upload from the
  beginning. Scale during quiet periods.
- **A proxy pod restart discards the multipart uploads that the pod was holding.** This
  is the same behaviour as a restart of a single instance: the client has to retry the
  whole upload.
- **All traffic for one key prefix goes to one pod.** Load is spread across the pods by
  object key, not by request, so a single very busy bucket or prefix does not fan out
  across the fleet. This is the price of correctness.
- **Do not bypass the router.** A client that reaches the proxy pods directly, or
  through a different load balancer, is not covered by the path-pinning, and its
  multipart uploads will fail. The `<name>` Service is the only supported entrypoint.

The admin UI (everything under `/_/`) is routed differently: it sticks to a pod by the
client's source IP address, because admin sessions are held in memory and are bound to
the IP address that opened them.

Two more operational notes:

- **Client IP addresses.** The router adds an `X-Forwarded-For` header to every
  request, and the operator sets `DGP_TRUST_PROXY_HEADERS=true` on the proxy pods.
  Rate limits, `aws:SourceIp` permission conditions, and the IP binding of admin
  sessions therefore see the real client address instead of the router's address. Note
  that this setting trusts whatever traffic reaches the proxy pods — if untrusted
  workloads share the cluster network, add a NetworkPolicy that only allows ingress to
  the proxy pods from the router.
- **Scaling down keeps the volumes.** The operator never deletes a pod's persistent
  volume. After you scale `replicas` down, reclaim the removed pods' volumes manually
  if you want the storage back.

## Requirements for `replicas > 1` (enforced)

Multipart routing is necessary but not sufficient. The
[multi-instance contract](../docs/product/how-to/run-multiple-instances.md) also
requires:

1. **An S3 storage backend.** The filesystem backend is local disk on each pod, so with
   more than one pod each pod would see different data.
2. **The same `DGP_BOOTSTRAP_PASSWORD_HASH` on every pod.** This hash encrypts the
   shared IAM database. The operator injects one Secret into all pods, which guarantees
   that they agree. If you would rather not create the hash yourself, set
   `spec.bootstrapPassword.autoGenerate: true` and the operator generates a random
   password and its hash once, stores both in a Secret named `<name>-bootstrap`, and
   injects the hash into every pod. Read the password with:
   `kubectl get secret <name>-bootstrap -o jsonpath='{.data.password}' | base64 -d`.
3. **A config sync bucket** (`advanced.config_sync_bucket`). It carries IAM users and
   groups between the pods and hosts the leader leases for replication rules.
4. **One admin writer at a time.** Make IAM changes through one pod only, or switch to
   `iam_mode: declarative`. The IAM synchronisation is not multi-master.

The operator checks points 1–3 before it scales. If you set `replicas: 3` but the spec
violates the contract — no sync bucket, a filesystem backend, a missing Secret, or no
bootstrap hash — the operator deploys **one** pod instead, sets the resource's phase to
`Degraded`, and writes the exact list of problems into `status.message`. Fix the spec
and the operator scales up on the next reconcile. A broken spec can never scale into
data corruption.

## Spec reference

```yaml
spec:
  replicas: 3                  # number of proxy pods (default 1)
  image: beshultd/deltaglider_proxy:1.16.0   # default: the operator's pinned release
  configYaml: |                # inline DeltaGlider YAML (do not put secrets here)
    storage:
      s3: https://s3.eu-central-1.amazonaws.com
      region: eu-central-1
  envFromSecret: dgp-env       # a Secret whose keys become DGP_* environment variables
  storage:
    size: 20Gi                 # persistent volume per pod for /data (default 10Gi)
    storageClass: fast-ssd     # optional; the cluster default is used when omitted
  router:
    replicas: 2                # number of HAProxy pods (default 2; they are stateless)
    image: haproxy:3.0-alpine  # default
  service:
    type: LoadBalancer         # default ClusterIP
  resources:                   # resources of the proxy container
    requests: { cpu: "500m", memory: 1Gi }
    limits: { memory: 4Gi }
  bootstrapPassword:
    autoGenerate: true         # operator creates <name>-bootstrap once (default false)
```

`kubectl get dgp` shows the replica count and the phase (`Ready` or `Progressing`),
and `status.message` reports how many proxy and router pods are ready.

## TLS

Terminate TLS at your Ingress or at your LoadBalancer, and route the whole host to the
`<name>` Service. Traffic inside the cluster, between the router and the pods, is plain
HTTP: the router has to see the request paths in order to hash them, so do not enable
the proxy's own TLS in this topology.

## Development

```bash
cd operator
cargo test          # builder and CRD-drift tests; no cluster needed
cargo run -- crd    # regenerate deploy/crd.yaml after changing src/crd.rs
docker build -f Dockerfile -t beshultd/deltaglider-operator:dev .
```

The operator is a separate crate. It does not affect the proxy's build or its CI gate.
