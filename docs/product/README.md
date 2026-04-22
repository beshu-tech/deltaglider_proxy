# DeltaGlider Proxy

*An S3-compatible proxy that transparently delta-compresses versioned binaries, routes buckets across multiple storage backends, and handles authentication via SigV4 or OAuth.*

![Object Browser](/_/screenshots/filebrowser.jpg)

## Why this exists

When the same binary artefact is uploaded across many versions — ZIP releases, DB dumps, game builds, tar archives — most of each new version is identical to the previous one. DeltaGlider Proxy stores those versions as xdelta3 deltas against a reference baseline, typically cutting storage 60–95% without changing anything on the client side.

Clients see a standard S3 API. Delta compression, reconstruction, and integrity verification are invisible.

## What's here

These docs are operator-facing — everything you need to install, secure, run, and debug the proxy. Developer-facing docs (build from source, release process, CI infrastructure) live on the [GitHub repo](https://github.com/beshu-tech/deltaglider_proxy) rather than in the binary.

### Start here

- [Quickstart](01-quickstart.md) — install, first run, first upload.
- [Setting up a bucket](10-first-bucket.md) — backend routing, aliases, public prefixes, per-bucket compression.

### Deploy to production

- [Production deployment](20-production-deployment.md) — TLS, reverse proxy, cache sizing, backups, multi-instance sync.
- [Security checklist](20-production-security-checklist.md) — SigV4, bootstrap password, IAM users, rate limiting.
- [Upgrade guide](21-upgrade-guide.md) — standard upgrade workflow and the TOML → YAML migration.

### Authentication & access

- [OAuth / OIDC setup](auth/30-oauth-setup.md) — Google, Okta, Azure AD, generic OIDC. Single sign-on + group mapping.
- [SigV4 and IAM users](auth/31-sigv4-and-iam.md) — per-user credentials with ABAC permissions.
- [IAM conditions](auth/32-iam-conditions.md) — source-IP restrictions, prefix scoping, group policies.
- [Rate limiting](auth/33-rate-limiting.md) — per-IP throttling, progressive delay, lockout model.

### Day 2 operations

- [Monitoring and alerts](40-monitoring-and-alerts.md) — Prometheus scrape, Grafana panels, alerting rules.
- [Troubleshooting](41-troubleshooting.md) — common symptoms → fixes.
- [FAQ](42-faq.md) — quick answers to common questions.

### Reference

- [Configuration reference](reference/configuration.md) — every YAML field and env var.
- [Admin API reference](reference/admin-api.md) — every `/_/api/admin/*` endpoint.
- [Authentication reference](reference/authentication.md) — conceptual model, error codes, claim shapes.
- [Metrics reference](reference/metrics.md) — every Prometheus metric and label.
- [How delta works](reference/how-delta-works.md) — on-disk layout, PUT/GET flow, integrity guarantees.

## One-paragraph summary

Single-process Rust binary. S3 API on `/`, admin UI + admin APIs on `/_/`. Same port. Embedded UI (rust-embed), embedded docs (the page you're reading). Storage backend can be filesystem, AWS S3, or any S3-compatible provider (Hetzner, Backblaze, Wasabi, R2, MinIO). Transparent xdelta3 compression with byte-identical reconstruction, SHA-256 verified. SigV4 for S3 clients (aws-cli, boto3, Terraform, rclone), OAuth/OIDC for admin UI. Per-user ABAC permissions with IP and prefix conditions. Prometheus metrics, in-memory audit ring, encrypted IAM DB with optional multi-instance sync.
