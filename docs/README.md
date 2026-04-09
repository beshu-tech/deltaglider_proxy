# Overview

*S3-compatible proxy with transparent delta compression for versioned binary artifacts*

DeltaGlider Proxy sits between your S3 clients and storage backend, silently deduplicating versioned files using xdelta3. Clients see a standard S3 API — the compression is invisible.

![Object Browser](/_/screenshots/filebrowser.jpg)
*S3 object browser with bucket navigation, file preview, and compression indicators*

## Key Features

- **Transparent delta compression** — similar files stored as deltas, reconstructed on GET
- **Standard S3 API** — works with AWS CLI, SDKs, Cyberduck, rclone
- **Multi-user IAM** — per-user credentials with ABAC permissions and conditions
- **Embedded admin GUI** — user management, metrics dashboard, configuration, documentation
- **Dual backend** — local filesystem or any S3-compatible storage (AWS, MinIO, Hetzner)

![Admin Settings](/_/screenshots/admin-users.png)
*IAM user management with fine-grained permissions and key rotation*

## Documentation

### Getting Started

- [Operations](OPERATIONS.md) — running, configuring, deploying
- [Configuration Reference](CONFIGURATION.md) — all settings with TOML + env var forms

### Security

- [Authentication](AUTHENTICATION.md) — SigV4 signatures and presigned URLs
- [Security Basics](HOWTO_SECURITY_BASICS.md) — step-by-step hardening guide
- [IAM Conditions](HOWTO_IAM_CONDITIONS.md) — IP restrictions and prefix scoping
- [Rate Limiting](RATE_LIMITING.md) — throttling and abuse prevention

### Internals

- [Delta Reconstruction](DELTA_RECONSTRUCTION.md) — how GET reconstructs files from reference + delta
- [Storage Format](STORAGE_FORMAT.md) — on-disk layout and metadata schema
- [Metrics](METRICS.md) — Prometheus metrics and Grafana setup

### Developer

- [Contributing](CONTRIBUTING.md) — build, test, project structure
- [Releasing](RELEASING.md) — release process, tagging, Docker builds
- [CI Infrastructure](CI_INFRA.md) — build pipeline and runners
