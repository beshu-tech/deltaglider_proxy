# How to scale out with the Kubernetes operator

This guide shows you how to run DeltaGlider Proxy on Kubernetes with more than one pod,
using the official operator. The operator manages the one component that a multi-pod
deployment cannot work without: a routing layer that sends all requests for a given
object to the same pod.

If a single pod is enough for you, use the [Helm chart](deploy-on-kubernetes.md)
instead. It is simpler, and the routing machinery described here adds nothing when
there is only one replica.

## Why you can't just set `replicas: 3` on a plain Deployment

Two pieces of the proxy's state live only inside a single pod — in that pod's memory
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
  time, because each write updates a shared reference file. The lock that enforces
  this ordering lives inside a single process, so it cannot protect against two
  different pods writing into the same prefix at the same moment.

The operator solves both problems in the same way, and this is the only multipart
strategy DeltaGlider implements: **consistent hashing by the directory of the URL
path**. An HAProxy router runs in front of the proxy pods and chooses the target pod by
hashing the request path with its last segment removed — in S3 terms, the bucket and
the key's prefix. Everything that lives in one directory therefore reaches the same
pod: every object key in that prefix, every part of any multipart upload of those
keys, and the prefix's delta reference file. That is deliberately one level coarser
than hashing the full path, because a delta prefix is shared between all of the keys
inside it — pinning only per key would still let two pods update the same reference
file at the same time. This approach has real trade-offs; they are listed at the end
of this guide, and you should read them before going live.

## 1. Install the operator

```bash
kubectl apply -f operator/deploy/crd.yaml
kubectl apply -f operator/deploy/operator.yaml
```

## 2. Create the credentials Secret

Every pod receives the same Secret. This also guarantees that all pods share the same
bootstrap password hash, which multi-pod IAM synchronisation requires:

```bash
kubectl create namespace dgp
kubectl -n dgp create secret generic dgp-env \
  --from-literal=DGP_ACCESS_KEY_ID=admin \
  --from-literal=DGP_SECRET_ACCESS_KEY=replace-me \
  --from-literal=DGP_BOOTSTRAP_PASSWORD_HASH="JDJiJDEyJ..." \
  --from-literal=DGP_BE_AWS_ACCESS_KEY_ID="..." \
  --from-literal=DGP_BE_AWS_SECRET_ACCESS_KEY="..."
```

Generate the hash with the proxy binary:

```bash
printf '%s\n' 'your-admin-password' | deltaglider_proxy --set-bootstrap-password
```

## 3. Declare the proxy

A multi-pod deployment needs an S3 storage backend, because the filesystem backend is
local disk on each pod and the pods would each see different data. It also needs a
config sync bucket, which carries IAM changes between the pods and hosts the
replication leader leases:

```yaml
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
    access:
      iam_mode: gui
    advanced:
      listen_addr: "0.0.0.0:9000"
      config_sync_bucket: dgp-iam-sync
  envFromSecret: dgp-env
  storage:
    size: 20Gi
  service:
    type: ClusterIP
```

```bash
kubectl apply -f dgp.yaml
kubectl -n dgp get dgp dgp -w     # wait for phase: Ready
```

The operator creates the proxy pods (a StatefulSet with one persistent volume per
pod), the HAProxy router pods, and a Service named `dgp` in front of the routers.
Point your Ingress and all of your S3 clients at the `dgp` Service — **never at the
proxy pods directly**. A client that bypasses the router also bypasses the
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
8 MB, so this test exercises the full sequence — `CreateMultipartUpload`, several
parallel `UploadPart` requests, and the final `CompleteMultipartUpload` — through the
hash-pinned path. If you see a `NoSuchUpload` error here, some client is reaching the
proxy pods without going through the router.

## Know the trade-offs

Consistent hashing pins traffic to pods; it does not share any state between them.
Accept the following consequences before you scale:

| Behaviour | Consequence |
|---|---|
| Scaling the proxy pods changes part of the hash ring | A multipart upload that is in flight during a scale-up or scale-down can fail with `NoSuchUpload` if its key now maps to a different pod. The client has to restart that upload from the beginning. Scale during quiet periods. |
| A proxy pod restart discards the multipart uploads it was holding | This is the same behaviour as a single-instance restart: the client has to retry the upload. |
| All traffic for one prefix goes to one pod | Load is spread across pods by directory, not by request. A single very busy prefix will not fan out across the fleet. |
| The admin UI sticks to a pod by source IP | Admin sessions are held in memory, so a pod restart logs its admin users out. |

The rest of the multi-instance contract — one IAM writer at a time, synchronisation
lag, upgrade ordering — is unchanged and described in
[How to run multiple instances (HA)](run-multiple-instances.md).

## Related

- [Operator README](https://github.com/beshu-tech/deltaglider_proxy/tree/main/operator) — the full spec reference and the development workflow
- [How to run multiple instances (HA)](run-multiple-instances.md) — the sync bucket, the one-writer rule, and upgrades
- [How to deploy on Kubernetes with Helm](deploy-on-kubernetes.md) — the single-pod path
- [How to take a proxy to production](go-to-production.md) — the production checklist
