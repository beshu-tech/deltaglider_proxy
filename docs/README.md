# Overview

*Unified S3 gateway with multi-backend routing, delegated authentication, fine-grained access control, transparent delta compression, and a built-in management GUI.*

DeltaGlider Proxy sits between your S3 clients and one or more storage backends — AWS S3, Hetzner, Backblaze, MinIO, or local filesystem. Clients see a single, standard S3 endpoint. The proxy handles authentication (SigV4, OAuth/OIDC, or public access), routes each bucket to the right backend, and silently delta-compresses versioned binaries to cut storage 60-95%.

![Object Browser](/_/screenshots/filebrowser.jpg)
*S3 file browser with bucket navigation, preview, bulk operations, and compression indicators*

## Key Features

- **Multi-backend routing** — aggregate AWS S3, Hetzner, Backblaze, MinIO, and filesystem behind one endpoint. Route each bucket to a different backend with optional aliasing.
- **Delegated authentication** — OAuth/OIDC single sign-on (Google, Okta, Azure AD), SigV4 per-user IAM, or public prefix access. Group mapping rules auto-assign permissions from identity provider claims.
- **Fine-grained access control** — ABAC permission rules with Allow/Deny, action verbs, resource patterns, and conditions (IP ranges, prefix restrictions). IAM groups for shared policies.
- **Public prefixes** — publish specific folders for anonymous download without exposing the rest of the bucket.
- **Transparent delta compression** — versioned binaries stored as xdelta3 diffs, reconstructed on GET. SHA-256 verified, byte-identical.
- **Built-in admin GUI** — file browser, user/group management, OAuth config, backend routing, monitoring dashboard, storage analytics, embedded docs.
- **Single binary, single port** — S3 API on `/`, admin GUI and APIs under `/_/`. No extra containers.

![IAM user management](/_/screenshots/iam.jpg)
*IAM user management with ABAC permissions, groups, and key rotation*

![Storage analytics](/_/screenshots/analytics.jpg)
*Per-bucket savings breakdown with cost estimation*

## Documentation

### Getting Started

- [Operations](OPERATIONS.md) — running, configuring, deploying, admin GUI features, admin API endpoint catalog
- [Configuration Reference](CONFIGURATION.md) — all settings (canonical YAML + env vars + TOML equivalents), backend routing, bucket policies, admission chain
- [Migrate TOML → YAML](HOWTO_MIGRATE_TO_YAML.md) — canonical format since v0.8.0

### Authentication & Access Control

- [Authentication](AUTHENTICATION.md) — SigV4, OAuth/OIDC, public prefixes, bootstrap vs IAM mode
- [Security Basics](HOWTO_SECURITY_BASICS.md) — step-by-step hardening from open access to production
- [IAM Conditions](HOWTO_IAM_CONDITIONS.md) — IP restrictions, prefix scoping, group policies
- [Rate Limiting](RATE_LIMITING.md) — throttling, progressive delay, lockout

### Internals

- [Delta Reconstruction](DELTA_RECONSTRUCTION.md) — how GET reconstructs files from reference + delta
- [Storage Format](STORAGE_FORMAT.md) — on-disk layout, metadata schema, config database
- [Metrics](METRICS.md) — Prometheus metrics reference and Grafana setup

### Developer

- [Contributing](CONTRIBUTING.md) — build, test, project structure
- [Releasing](RELEASING.md) — release process, tagging, Docker builds
- [CI Infrastructure](CI_INFRA.md) — build pipeline and runners
