# How to scale out with the Kubernetes operator

This guide shows you how to run DeltaGlider Proxy on Kubernetes with more than one pod,
using the official operator. The operator manages the one component that a multi-pod
deployment cannot work without: a routing layer that sends all requests for a given
object to the same pod.

If a single pod is enough for you, use the [Helm chart](deploy-on-kubernetes.md)
instead. It is simpler, and the routing machinery described here adds nothing when
there is only one replica.

## Why you can't just set `replicas: 3` on a plain Deployment

Two pieces of the proxy's state live only inside a single pod, in that pod's memory
and on its local disk:

- **Multipart uploads.** When a client starts a multipart upload, the pod that receives
  the `CreateMultipartUpload` request generates the upload id and keeps track of every
  part that arrives afterwards. No other pod knows that this upload exists. Behind a
  round-robin Service, the SDK sends its parallel `UploadPart` requests to whichever
  pods the load balancer happens to pick, and every request that lands on a different
  pod is rejected with a `NoSuchUpload` error. The usual Kubernetes remedies do not
  apply here: S3 clients do not carry cookies, so cookie-based session affinity at the
  Ingress cannot help, and `ClientIP` affinity stops working as soon as many clients
  share one IP address behind a NAT gateway.
- **The delta reference lock.** All writes into one delta prefix must happen one at a
  time, because each write updates a shared reference file. Inside one pod, an
  in-process lock enforces this ordering. Across pods, a lock object in the config sync
  bucket enforces it, so two pods cannot corrupt the reference file. A pod that
  waits for another pod's lock slows its writes down, and a pod that does not get the
  lock in time fails the write.

The operator solves both problems in the same way, and this is the only multipart
strategy DeltaGlider implements: consistent hashing by the directory of the URL
path. An HAProxy router runs in front of the proxy pods and chooses the target pod by
hashing the request path with its last segment removed. In S3 terms, this is the bucket and
the key's prefix. Everything that lives in one directory therefore reaches the same
pod: every object key in that prefix, every part of any multipart upload of those
keys, and the prefix's delta reference file. The router deliberately hashes one level coarser
than the full path, because a delta prefix is shared between all of the keys
inside it. With pinning per key, two pods would still take turns on the same
reference file. This approach has trade-offs. The end of this guide lists them, and you should read them before you go live.

## 1. Install the operator

```bash
kubectl apply -f operator/deploy/crd.yaml
kubectl apply -f operator/deploy/operator.yaml
```

To upgrade an installed operator, apply the new `crd.yaml` before the new
`operator.yaml`. Operator 0.3.0 adds `spec.router.trustedProxyCidrs` to the CRD.

## 2. Create the credentials Secret

Every pod receives the same Secret. This also guarantees that all pods share the same
config DB key, which multi-pod IAM synchronisation requires (the synced IAM database is
encrypted with it), and the same bootstrap password hash, so that the admin password is
the same on every pod:

```bash
kubectl create namespace dgp
kubectl -n dgp create secret generic dgp-env \
  --from-literal=DGP_ACCESS_KEY_ID=admin \
  --from-literal=DGP_SECRET_ACCESS_KEY=replace-me \
  --from-literal=DGP_BOOTSTRAP_PASSWORD_HASH="JDJiJDEyJ..." \
  --from-literal=DGP_CONFIG_DB_KEY="$(openssl rand -hex 32)" \
  --from-literal=DGP_BE_AWS_ACCESS_KEY_ID="..." \
  --from-literal=DGP_BE_AWS_SECRET_ACCESS_KEY="..."
```

Generate the hash with the proxy binary:

```bash
printf '%s\n' 'your-admin-password' | deltaglider_proxy --set-bootstrap-password
```

If you would rather skip this step, leave `DGP_BOOTSTRAP_PASSWORD_HASH` and
`DGP_CONFIG_DB_KEY` out of the Secret and set `bootstrapPassword: { autoGenerate: true }`
in the resource below. The operator then generates a random password and a random
config DB key once, stores them with the password hash in a Secret named
`<name>-bootstrap`, and injects the hash and the key into every pod. Read the password
later with
`kubectl -n dgp get secret dgp-bootstrap -o jsonpath='{.data.password}' | base64 -d`.
Back up that Secret: its `dbKey` is the only key that opens the IAM databases on the
pod volumes. A `<name>-bootstrap` Secret from an older operator release gets its `dbKey`
added on the next reconcile, and the pods re-encrypt their databases on their next
restart.

## 3. Declare the proxy

A multi-pod deployment needs an S3 storage backend, because the filesystem backend is
local disk on each pod and the pods would each see different data. It also needs a
config sync bucket, which carries IAM changes between the pods and hosts the
replication leader leases and the reference locks. Every S3 backend that clients write
to must support conditional writes: with a sync bucket, a pod refuses to start on a
backend that does not support them (see
[How to use a backend without conditional writes](backend-capability-validation.md)). A resource that meets these requirements:

```yaml
# not-proxy-config: DeltaGliderProxy resource (configYaml holds the proxy config)
apiVersion: deltaglider.beshu.tech/v1alpha1
kind: DeltaGliderProxy
metadata:
  name: dgp
  namespace: dgp
spec:
  replicas: 3
  configYaml: |
    storage:
      s3: https://s3.eu-central-1.amazonaws.com
      region: eu-central-1
      access_key_id: ${env:DGP_BE_AWS_ACCESS_KEY_ID}
      secret_access_key: ${env:DGP_BE_AWS_SECRET_ACCESS_KEY}
    access:
      iam_mode: gui
    advanced:
      listen_addr: "0.0.0.0:9000"
      config_sync_bucket: dgp-iam-sync
  envFromSecret: dgp-env
  storage:
    size: 20Gi
  router:
    trustedProxyCidrs: ["10.42.0.0/16"]   # the pod network of your cluster
  service:
    type: ClusterIP
```

```bash
kubectl apply -f dgp.yaml
kubectl -n dgp get dgp dgp -w     # wait for phase: Ready
```

The proxy expands the `${env:...}` references inside the pod, against the
environment variables that the Secret provides. The credentials therefore reach the storage
backend, and the ConfigMap never contains them.

The `DeltaGliderProxy` resource owns the configuration: `configYaml` is the only place to
change it, so this guide uses YAML only. The operator renders `configYaml` into a
ConfigMap and mounts it read-only into every pod. A change that you make in the admin UI
therefore works only on the pod that served it, the proxy cannot write it into the file,
and the change is lost when that pod restarts. Settings that need a restart (the
listener, TLS, the reference cache size and the sync bucket) never take effect from the UI
at all, and a backup restore that includes the config answers `409 config_file_read_only`.
The admin UI shows a warning banner about the read-only file. To keep a change, edit
`configYaml` and apply the resource again; the operator then rolls the pods. IAM users and
groups in `iam_mode: gui` live in the synced database, not in the file, so UI changes to
them persist. See [Two ways to configure DeltaGlider](../explanation/two-ways-to-configure.md).

If your backend is an in-cluster MinIO reached over plain `http://`, add
`DGP_BACKEND_ALLOW_LOCAL: "true"` to the Secret. By default, the SSRF guard refuses plain-http
and private addresses. On proxy releases v1.17.0 and later, that flag also
admits in-cluster DNS names such as `minio.dgp.svc.cluster.local`; releases up to
v1.16.1 refuse those hostnames outright, so on older versions point the endpoint at
the Service's ClusterIP instead of its DNS name.

The operator checks the multi-replica requirements before it scales: if the spec has
no config sync bucket, uses a filesystem backend, or has no shared bootstrap password
hash or config DB key, the operator refuses to scale up. A fresh deployment comes up with one pod, and an
already-running fleet keeps its current size. The operator also sets the phase to `Degraded`
with the exact problems listed in `status.message` (`kubectl -n dgp describe dgp dgp`
shows them). Fix the spec and it scales up on its own.

The routers add an `X-Forwarded-For` header that names the client. The proxy reads that
header only on a connection from a network in `spec.router.trustedProxyCidrs`, so set it
to the pod network of your cluster (`10.42.0.0/16` is the k3s default). When you leave it
out, the operator uses every private range, and then any pod in those ranges can name a
client.

The operator creates the proxy pods (a StatefulSet with one persistent volume per
pod), the HAProxy router pods, and a Service named `dgp` in front of the routers.
Point your Ingress and all of your S3 clients at the `dgp` Service. **Never point them at the
proxy pods directly.** A client that bypasses the router also bypasses the
path-pinning, and its multipart uploads will fail.

## 4. Verify that multipart uploads work across pods

A multipart upload sent through the router must succeed even though there are three
pods behind it:

```bash
dd if=/dev/urandom of=/tmp/big.bin bs=1M count=64
aws --endpoint-url http://<dgp-service> s3 cp /tmp/big.bin s3://releases/big.bin
aws --endpoint-url http://<dgp-service> s3api head-object --bucket releases --key big.bin
```

The `aws` command-line tool switches to a multipart upload for any file larger than
8 MB, so this test exercises the full sequence (`CreateMultipartUpload`, several
parallel `UploadPart` requests, and the final `CompleteMultipartUpload`) through the
hash-pinned path. If you see a `NoSuchUpload` error here, some client is reaching the
proxy pods without going through the router.

## Know the trade-offs

Consistent hashing pins traffic to pods; it does not share any state between them.
Accept the following consequences before you scale.

Four different events move the hash ring, and every one of them has the same
consequence: a multipart upload that is in flight on a moved prefix fails with
`NoSuchUpload`, and the client has to restart that upload from the beginning:

| Ring-moving event | How it happens |
|---|---|
| Scaling up | You raise `replicas`; part of the key space moves to the new pods. |
| Scaling down | You lower `replicas`; the removed pods' key space redistributes. |
| A proxy pod restart | The pod's ring slot is unchanged (stable name), but the multipart state it held in memory is gone. |
| Readiness ejection | A pod that fails its readiness probe for about thirty seconds (one backend hiccup is enough) is removed from the ring by the routers, without any operator action. The pod comes back when it recovers. The effect is the same as the effect of a scale event. |

The remaining structural consequences:

| Behaviour | Consequence |
|---|---|
| Readiness is all-or-nothing per pod | Every pod's readiness probe (`/_/ready`) checks the storage backends, so a backend outage removes **all** pods from the ring at once. With one backend, this is correct, because no pod could serve a request anyway. With several backends, a pod stays ready while at least one backend answers, and the buckets on a dead backend answer `503` on their own. |
| All traffic for one prefix goes to one pod | Load is spread across pods by directory, not by request. A single very busy prefix will not fan out across the fleet. |
| The admin UI is effectively single-pod | Sessions are in memory and source-IP sticky: a pod restart logs its admin users out, everyone behind one NAT gateway lands on the same pod, and they share that pod's login rate-limit budget. Treat the admin GUI as a one-pod surface for now. |

The rest of the multi-instance contract (the IAM merge, synchronisation lag, and upgrade
ordering) is the same, and
[How to run multiple instances (HA)](run-multiple-instances.md) describes it.

## Related

- [Operator README](https://github.com/beshu-tech/deltaglider_proxy/tree/main/operator): the full spec reference and the development workflow
- [How to run multiple instances (HA)](run-multiple-instances.md): the sync bucket, the IAM merge, and upgrades
- [How to deploy on Kubernetes with Helm](deploy-on-kubernetes.md): the single-pod path
- [How to take a proxy to production](go-to-production.md): the production checklist
