# How to scale out with the Kubernetes operator

This guide shows you how to run a multi-pod DeltaGlider Proxy on Kubernetes with the
official operator, which manages the one piece a multi-pod deployment cannot work
without: a router that pins each object's requests to one pod.

If a single pod is enough, use the [Helm chart](deploy-on-kubernetes.md) instead — it is
simpler and this guide's routing machinery buys you nothing at one replica.

## Why you can't just set `replicas: 3` on a plain Deployment

Two pieces of proxy state live only in the memory and local disk of one pod:

- **Multipart uploads.** The pod that answers `CreateMultipartUpload` holds the upload
  id and the received parts. Behind a round-robin Service, the SDK's parallel
  `UploadPart` calls hit other pods and fail with `NoSuchUpload`. S3 clients don't
  carry cookies, so Ingress session affinity can't fix this, and `ClientIP` affinity
  collapses behind NAT.
- **The delta reference lock.** Writes into one delta prefix must be serialized; the
  lock is in-process, so two pods writing the same prefix concurrently is unsafe.

The operator's answer — and the only multipart strategy DeltaGlider implements — is
**consistent hashing by URL path**: an HAProxy router in front of the pods hashes every
S3 request by its path, so all requests for one object key (every part of a multipart
upload included) land on the same pod, and all writes to one prefix serialize on one
pod. The trade-offs are listed at the end of this guide; read them before going live.

## 1. Install the operator

```bash
kubectl apply -f operator/deploy/crd.yaml
kubectl apply -f operator/deploy/operator.yaml
```

## 2. Create the credentials Secret

Every pod gets the same Secret — which also guarantees the shared bootstrap password
hash that multi-pod IAM sync requires:

```bash
kubectl create namespace dgp
kubectl -n dgp create secret generic dgp-env \
  --from-literal=DGP_ACCESS_KEY_ID=admin \
  --from-literal=DGP_SECRET_ACCESS_KEY=replace-me \
  --from-literal=DGP_BOOTSTRAP_PASSWORD_HASH="JDJiJDEyJ..." \
  --from-literal=DGP_BE_AWS_ACCESS_KEY_ID="..." \
  --from-literal=DGP_BE_AWS_SECRET_ACCESS_KEY="..."
```

Generate the hash with
`printf '%s\n' 'your-admin-password' | deltaglider_proxy --set-bootstrap-password`.

## 3. Declare the proxy

Multi-pod requires an S3 storage backend (the filesystem backend is per-pod local disk)
and a config sync bucket (IAM sync + replication leader leases):

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

The operator creates the proxy StatefulSet (one PVC per pod), the HAProxy router
Deployment, and the `dgp` Service. Point your Ingress and all S3 clients at the `dgp`
Service — **never at the proxy pods directly**, or the path-pinning is bypassed.

## 4. Verify multipart uploads across pods

A multipart upload through the router must succeed even with 3 pods:

```bash
dd if=/dev/urandom of=/tmp/big.bin bs=1M count=64
aws --endpoint-url http://<dgp-service> s3 cp /tmp/big.bin s3://releases/big.bin
aws --endpoint-url http://<dgp-service> s3api head-object --bucket releases --key big.bin
```

The `aws` CLI switches to multipart above 8 MB, so this exercises
`CreateMultipartUpload` → parallel `UploadPart` → `CompleteMultipartUpload` through the
hash-pinned path. If you see `NoSuchUpload`, a client is bypassing the router.

## Know the trade-offs

Consistent hashing pins traffic; it does not share state. Accept these before scaling:

| Behaviour | Consequence |
|---|---|
| Scaling proxy pods remaps part of the hash ring | In-flight multipart uploads on remapped keys fail with `NoSuchUpload`; clients must restart the upload. Scale during quiet periods. |
| A proxy pod restart drops its in-flight multipart uploads | Same as a single-instance restart — clients retry. |
| Hot prefixes hash to one pod | Load spread is by key, not by request. A single hot bucket/prefix won't fan out. |
| Admin UI is source-IP sticky | Sessions are in-memory; a pod restart logs its admin users out. |

The rest of the multi-instance contract (single IAM writer, sync lag, upgrade order)
is unchanged — see [How to run multiple instances (HA)](run-multiple-instances.md).

## Related

- [Operator README](https://github.com/beshu-tech/deltaglider_proxy/tree/main/operator) — spec reference and development workflow
- [How to run multiple instances (HA)](run-multiple-instances.md) — the sync bucket, one-writer rule, upgrades
- [How to deploy on Kubernetes with Helm](deploy-on-kubernetes.md) — the single-pod path
- [How to take a proxy to production](go-to-production.md) — the production checklist
