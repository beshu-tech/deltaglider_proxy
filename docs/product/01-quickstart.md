# Quickstart

*Install, first run, first upload.*

DeltaGlider Proxy is a single process that speaks S3. You run the binary, point an S3 client at it, and carry on. This page gets you to "first upload succeeds" in under five minutes.

## 1. Install

Pick one. All three give you the same binary.

**Docker (recommended for servers):**

```bash
docker run --rm \
  -p 9000:9000 \
  -v "$PWD/dgp-data:/data" \
  beshultd/deltaglider_proxy:latest
```

**Binary release (macOS, Linux):**

Grab the latest from [the releases page](https://github.com/beshu-tech/deltaglider_proxy/releases), unpack, and run:

```bash
./deltaglider_proxy
```

**From source:**

```bash
git clone https://github.com/beshu-tech/deltaglider_proxy.git
cd deltaglider_proxy
cd demo/s3-browser/ui && npm install && npm run build && cd -
cargo run --release
```

The UI is baked into the binary, so the source build needs Node 20+ for the one-time UI compile.

## 2. First run

On a fresh install with no config, the proxy:

- binds `0.0.0.0:9000`
- uses the filesystem backend at `./data/`
- **auto-generates a bootstrap password** on first start and prints it to stderr (in an interactive terminal; in containers only the bcrypt hash is logged — see logs to recover it)

You'll see something like:

```
╔══════════════════════════════════════════════════════════════╗
║  BOOTSTRAP PASSWORD (first run — save this!)                ║
║                                                              ║
║  Password: 8p2Xq9nKzV4mTbR7                                  ║
║                                                              ║
║  This password appears ONCE. Store it securely.              ║
║  Set DGP_BOOTSTRAP_PASSWORD_HASH to skip auto-generation.    ║
╚══════════════════════════════════════════════════════════════╝
```

Save it. You need it to log into the admin UI.

Open `http://localhost:9000/_/` — the admin UI should load. Log in with the bootstrap password.

## 3. Create a bucket + upload

From the UI:

1. Sidebar → **Create bucket** → name it `demo` → Create.
2. Click `demo` → Upload → drop any file.

Or from the command line with the AWS CLI — the proxy is SigV4-signed when you configure credentials. For a first-run proxy without auth, any valid SigV4 signature works:

```bash
# any AK/SK pair — the proxy accepts all requests until you enable auth
AWS_ACCESS_KEY_ID=dummy AWS_SECRET_ACCESS_KEY=dummy \
  aws --endpoint-url http://localhost:9000 s3 mb s3://demo

AWS_ACCESS_KEY_ID=dummy AWS_SECRET_ACCESS_KEY=dummy \
  aws --endpoint-url http://localhost:9000 s3 cp ./some-file.zip s3://demo/
```

The object lands on disk under `./data/demo/`. The proxy automatically decides whether to store it as passthrough or as a delta against a per-prefix reference baseline — you don't configure anything for delta compression to kick in.

## 4. Where to go next

- **Put it in production** → [Production deployment](20-production-deployment.md) and the [security checklist](20-production-security-checklist.md). An auth-less proxy is fine for local dev; anywhere else, enable SigV4 or IAM.
- **Set up OAuth login** → [OAuth setup](auth/30-oauth-setup.md) for Google, Okta, Azure AD, or any OIDC provider.
- **Route buckets to a real S3 backend** → [Setting up buckets](10-first-bucket.md) covers backend routing, aliasing, and per-bucket compression policies.
- **Something broke** → [Troubleshooting](41-troubleshooting.md).

## One-page feature summary

- **S3-compatible API** — standard SigV4. `aws-cli`, boto3, Terraform, rclone all work unchanged.
- **Transparent delta compression** — versioned binaries stored as xdelta3 diffs against a reference; reconstructed on GET, byte-identical, SHA-256 verified.
- **Multi-backend routing** — aggregate AWS S3, Hetzner, Backblaze, MinIO, and local filesystem behind one endpoint.
- **Authentication** — SigV4 bootstrap, per-user IAM with ABAC permissions, or OAuth/OIDC.
- **Admin UI** at `/_/` on the same port — file browser, user management, backend config, Prometheus dashboard, analytics.
- **Single binary, single port** — no sidecars. The UI and API share port 9000.
