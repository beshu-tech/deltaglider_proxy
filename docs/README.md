# Overview

*Project overview, quickstart, API compatibility*

## Getting Started

- [Operations](OPERATIONS.md) — running, configuring, deploying
- [Configuration Reference](CONFIGURATION.md) — all settings with TOML + env var forms

## Security

- [Authentication](AUTHENTICATION.md) — SigV4 signatures and presigned URLs
- [Security Basics](HOWTO_SECURITY_BASICS.md) — step-by-step hardening guide
- [IAM Conditions](HOWTO_IAM_CONDITIONS.md) — IP restrictions and prefix scoping
- [Rate Limiting](RATE_LIMITING.md) — throttling and abuse prevention
- [Hardening Plan](HARDENING_PLAN.md) — completed security phases

## Internals

- [Delta Reconstruction](DELTA_RECONSTRUCTION.md) — how GET reconstructs files
- [Storage Format](STORAGE_FORMAT.md) — on-disk layout and metadata schema
- [Metrics](METRICS.md) — Prometheus metrics and Grafana setup

## Operations

- [Contributing](CONTRIBUTING.md) — build, test, project structure
- [Releasing](RELEASING.md) — release process, tagging, Docker builds
- [CI Infrastructure](CI_INFRA.md) — build pipeline and runners
