# Your first Helm deployment on kind

In this tutorial we'll run DeltaGlider Proxy on Kubernetes — a real chart on a real (if disposable) cluster. We'll boot a local `kind` cluster, install the official Helm chart, and prove the whole thing works: the admin UI loads, the health probe answers, and a file round-trips through the S3 API. Then we'll delete the cluster and leave nothing behind.

You'll need the DeltaGlider Proxy repository checked out locally (the chart ships inside it), plus Docker, `kind`, `kubectl`, `helm`, and the `aws` CLI.

If you don't have the repository yet:

```bash
git clone https://github.com/beshu-tech/deltaglider_proxy.git
cd deltaglider_proxy
```

One thing to know before we start: the chart ships with intentionally public development credentials so it can be smoke-tested out of the box. That's exactly what we're doing here — and it's why a default install must never be exposed beyond localhost.

## Step 1: create a disposable cluster

```bash
kind create cluster --name dgp-hello
kubectl cluster-info --context kind-dgp-hello
```

You should see the cluster answer:

```
Kubernetes control plane is running at https://127.0.0.1:...
CoreDNS is running at https://127.0.0.1:.../api/v1/namespaces/kube-system/services/kube-dns:dns/proxy
```

## Step 2: install the chart

From the repository root:

```bash
helm upgrade --install dgp ./charts/deltaglider-proxy \
  --namespace dgp \
  --create-namespace \
  --set persistence.size=1Gi \
  --kube-context kind-dgp-hello
```

You should see Helm report the release:

```
Release "dgp" does not exist. Installing it now.
NAME: dgp
NAMESPACE: dgp
STATUS: deployed
```

This installs the default filesystem-backed configuration: one Deployment, one Service, and a 1 Gi PVC under `/data` for object data and the encrypted IAM database. Wait for the pod to come up:

```bash
kubectl --context kind-dgp-hello -n dgp rollout status deploy/dgp-deltaglider-proxy
kubectl --context kind-dgp-hello -n dgp get pods,pvc,svc
```

You should see the rollout succeed, the pod at `1/1 Running`, the PVC `Bound`, and the service exposing port `9000`:

```
deployment "dgp-deltaglider-proxy" successfully rolled out
NAME                                         READY   STATUS    RESTARTS   AGE
pod/dgp-deltaglider-proxy-6b9f...            1/1     Running   0          45s
...
```

## Step 3: port-forward the service

Keep this running in a separate terminal:

```bash
kubectl --context kind-dgp-hello -n dgp port-forward svc/dgp-deltaglider-proxy 19090:9000
```

Now open the admin UI in your browser:

```text
http://127.0.0.1:19090/_/
```

You should see the DeltaGlider Proxy connect screen. The chart's development bootstrap password is `change-me-in-production` — it exists so the chart is testable out of the box, and it's the first thing to override anywhere that isn't a throwaway cluster.

## Step 4: verify health and login

Let's confirm the same things a load balancer and an operator would check. First the health probe:

```bash
curl -fsS http://127.0.0.1:19090/_/health
```

You should see a healthy JSON response:

```json
{"status":"healthy","backend":"live", ...}
```

Then the bootstrap login:

```bash
curl -fsS -X POST http://127.0.0.1:19090/_/api/admin/login \
  -H 'content-type: application/json' \
  --data '{"password":"change-me-in-production"}'
```

```json
{"ok":true}
```

Notice we've now verified the two endpoints that matter for operations: the probe Kubernetes uses to decide the pod is alive, and the credential a human uses to get in.

## Step 5: round-trip a file through the S3 API

The chart also creates development SigV4 credentials (`admin` / `change-me-in-production`). Let's push a file through the proxy and pull it back:

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

You should see the bucket created, the upload confirmed, and — the line that proves the whole pipeline — your file's content echoed back from the cluster:

```text
hello from kind
```

## Step 6: run the chart's own test

The chart ships a Helm test hook that spins up a small curl pod and fails unless `/_/health` returns success:

```bash
helm test dgp -n dgp --kube-context kind-dgp-hello
```

You should see the suite pass:

```
NAME: dgp
...
TEST SUITE:     dgp-deltaglider-proxy-test-health
...             Phase: Succeeded
```

## Step 7: tear it down

```bash
kind delete cluster --name dgp-hello
```

```
Deleting cluster "dgp-hello" ...
```

Cluster, PVC, and the development credentials are all gone.

## What you built

You took the official chart from zero to verified in seven steps: a running pod with persistent storage, a reachable admin UI, a passing health probe, a working bootstrap login, and a file that round-tripped through the S3 API on Kubernetes. Just as importantly, you've now seen exactly which defaults (`change-me-in-production`, everywhere) must be replaced before this leaves your laptop.

## Where next

- [Deploy on Kubernetes](../how-to/deploy-on-kubernetes.md) — the production version of what you just did: credentials in a real Secret, S3 backends, Ingress with TLS, and the chart values that matter.
- [Securing your first proxy](secure-your-proxy.md) — the security walkthrough, if you haven't done it yet.
