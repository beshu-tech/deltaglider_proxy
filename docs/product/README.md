# DeltaGlider Proxy

*An S3-compatible control plane in front of existing storage, not another object store or storage cluster. DeltaGlider routes buckets across backends and local filesystems, gives operators a proper, centralized admin UI for IAM, OAuth, lifecycle, replication, events, audits, caching, encryption, and policy, and stores repeated binaries as compact xdelta3 deltas when that saves space.*

## Why this exists

Versioned binary artifacts — firmware builds, backup archives, AI model variants, game builds, DB dumps — pay full price for every copy, even when each new version is 99% identical to the last. DeltaGlider Proxy stores those copies as xdelta3 deltas against a reference baseline, typically cutting storage 60–95% on high-similarity workloads. Clients see a standard S3 API and never know; you run the binary, point an S3 client at it, and carry on.

![Object Browser](/_/screenshots/filebrowser.jpg)

## Where it sits: the data path

DeltaGlider is a proxy *in front of* storage you already have, not a new object store. Your client speaks the standard S3 API to the proxy; the proxy authenticates the request, decides whether the object is delta-eligible, runs xdelta3 if so, and reads or writes the actual bytes on whichever backend that bucket is routed to — AWS S3, an S3-compatible provider, or a local filesystem.

```mermaid
flowchart LR
    C["S3 client<br/>(aws-cli, boto3,<br/>Terraform, rclone)"] -->|"S3 API (SigV4)"| P
    subgraph P["DeltaGlider Proxy"]
        direction TB
        A["Auth + admission<br/>IAM / SigV4"] --> R["Router<br/>bucket → backend"]
        R --> X["xdelta3 codec<br/>encode on PUT /<br/>reconstruct on GET"]
    end
    P -->|"baselines + deltas"| B[("Backend<br/>AWS S3 · Hetzner ·<br/>Backblaze · filesystem")]
```

The control plane (IAM, routing table, per-object metadata, jobs) lives in the proxy; the data plane (your bytes) lives on the backends. That split is the design decision everything else follows from — see [multi-backend routing](explanation/multi-backend-architecture.md) for why, and [how delta compression works](explanation/delta-compression.md) for the PUT/GET mechanics. Before a production rollout, read [capacity planning](reference/capacity-planning.md): because the proxy actively encodes and reconstructs payloads, it has a different CPU/RAM profile than a pass-through proxy.

## Pick your path

### Learn it

Three hands-on tutorials, each a complete session from nothing to a working result. **New here? Start with the first one — it's the quickstart:**

- [Your first delta savings](tutorials/first-delta-savings.md) **(quickstart)** — run the proxy in Docker, upload two firmware versions, watch the second one shrink to almost nothing.
- [Securing your first proxy](tutorials/secure-your-proxy.md) — go from open access to a locked-down proxy with real credentials and a least-privilege IAM user.
- [Your first Helm deployment on kind](tutorials/kubernetes-hello-world.md) — install the official Helm chart on a disposable local cluster and prove it round-trips a file.

### Get something done

Task-shaped guides, grouped by what you're touching:

- **Deploy and operate** — [go to production](how-to/go-to-production.md), [deploy with Docker Compose](how-to/deploy-with-docker-compose.md), [deploy on Kubernetes](how-to/deploy-on-kubernetes.md), [troubleshooting](how-to/troubleshooting.md). Also: TLS, upgrades, backups, HA, Prometheus, request tracing.
- **Storage** — [route a bucket to a backend](how-to/route-a-bucket-to-a-backend.md), [migrate existing data into the proxy](how-to/migrate-existing-data-into-the-proxy.md), [encrypt data at rest](how-to/encrypt-data-at-rest.md), [replicate a bucket](how-to/replicate-a-bucket.md). Also: quotas and compression policy, bucket migration, lifecycle, key rotation, event notifications.
- **Access** — [create IAM users](how-to/create-iam-users.md), [set up SSO](how-to/set-up-sso.md), [restrict access with conditions](how-to/restrict-access-with-conditions.md), [publish a public folder](how-to/publish-a-public-folder.md). Also: IAM as code, admission rules.

### Look something up

Every page under Reference is pure facts — fields, endpoints, defaults, limits. Start with:

- [Configuration](reference/configuration.md) — every YAML field and env var.
- [CLI](reference/cli.md) — every flag and subcommand of the binary.
- [Admin API](reference/admin-api.md) — every `/_/api/admin/*` endpoint.
- [Metrics](reference/metrics.md) — every Prometheus metric and label.
- [Capacity planning](reference/capacity-planning.md) — CPU/RAM/disk sizing for a production rollout.

Plus references for [authentication](reference/authentication.md), [IAM permissions](reference/iam-permissions.md), [encryption](reference/encryption.md), [jobs](reference/jobs.md), [replication](reference/replication.md), [lifecycle](reference/lifecycle.md), [the event outbox](reference/event-outbox.md), [declarative IAM](reference/declarative-iam.md), and [rate limits](reference/rate-limits.md).

### Understand it

The why behind the design, one concept per page:

- [How delta compression works](explanation/delta-compression.md) — what deltas well, what honestly doesn't, and why GETs are byte-identical.
- [How migration works](explanation/how-migration-works.md) — in-place vs. copy-through, why it's not lazy-on-read, and why there's no downtime.
- [Compression vs. S3 Object Versioning](explanation/versioning-vs-s3-versioning.md) — the disambiguation: DeltaGlider does not implement native S3 versioning, and what that means for ransomware rollback.
- [Multi-backend routing](explanation/multi-backend-architecture.md) — one endpoint over many backends, aliasing, and trusting cheap storage.
- [Authentication and access control](explanation/security-model.md) — the four layers every request passes through, in order.
- [Encryption at rest](explanation/encryption-at-rest.md) — the threat model, the modes, and the honest costs.
- [Jobs, write gates, and durability](explanation/jobs-and-durability.md) — why background work is one surface, and what "durable" means here.

## Install

Pick one. All three give you the same binary.

```bash
# Docker (recommended) — then follow the first tutorial
docker run --rm -it -p 9000:9000 -v dgp-data:/data beshultd/deltaglider_proxy
```

**Binary release** — grab the latest for macOS or Linux from the [releases page](https://github.com/beshu-tech/deltaglider_proxy/releases), unpack, run `./deltaglider_proxy`.

**From source** — the UI is baked into the binary, so build it first (needs Node 20+): `cd demo/s3-browser/ui && npm ci && npm run build && cd -`, then `cargo build --release`.

## Operator summary

**Shape**

- Single-process Rust binary, single port.
- S3 API on `/`; admin UI, admin APIs, and these docs embedded on `/_/`.
- Backend can be filesystem, AWS S3, or any S3-compatible provider: Hetzner, Backblaze, Wasabi, R2, MinIO.

**Storage path**

- Transparent xdelta3 compression for repeated binaries; byte-identical, SHA-256-verified reconstruction on read.
- Optional proxy-side AES-256-GCM encryption keeps keys in your environment before bytes land in cheap or untrusted storage.

**Control plane**

- SigV4 for S3 clients: `aws-cli`, `boto3`, Terraform, rclone. OAuth/OIDC for the admin UI.
- Per-user ABAC permissions with IP and prefix conditions; soft quotas; admission rules.
- Replication, lifecycle, re-encryption, and migration jobs on one screen; Prometheus metrics, audit ring, durable event outbox, encrypted IAM DB, optional multi-instance config sync.
