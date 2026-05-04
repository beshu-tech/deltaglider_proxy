# Grail Objectives: Enterprise S3 Feature Gap Analysis

## Stefano's Requirements (verbatim, translated)

> The problem with 100% of tools that aren't Ceph or MinIO is they have practically no features beyond basic read/write APIs (they're just translators). Most of the community ignores this, which is baffling.
>
> Switching to something that doesn't implement at least: **replica, SSE-S3, SSE-C, Policy, Lifecycle Rules, Object Locking & Immutability, Event Notification, Quota, LDAP/OIDC login + IAM integration, and STS support** is a problem.
>
> Not to mention **product stability, adequate performance, and sustainable production operations**.

## Current Score: 8/13

| Feature | Required | Status | Gap |
|---------|----------|--------|-----|
| OIDC login | Yes | **DONE** | - |
| IAM integration | Yes | **DONE** (ABAC, groups, conditions) | - |
| Stability / Performance | Yes | **DONE** (Rust, async, tested) | - |
| Replica | Yes | **DONE** (scheduled source→destination object replication) | - |
| SSE-S3 | Yes | **DONE** (backend-delegated SSE-S3 / SSE-KMS) | - |
| SSE-C | Yes | NOT IMPLEMENTED | Critical |
| Bucket Policy (resource-based) | Yes | PARTIAL (public prefixes only) | High |
| Lifecycle Rules | Yes | PARTIAL (delete-only expiration) | Medium |
| Object Locking / Immutability | Yes | NOT IMPLEMENTED | High |
| Event Notification | Yes | PARTIAL (durable outbox + webhook delivery) | Medium |
| Quota | Yes | PARTIAL (soft bucket quotas / freeze) | Medium |
| LDAP login | Yes | NOT IMPLEMENTED | Medium |
| STS (temp credentials) | Yes | NOT IMPLEMENTED | Medium |
| Versioning | Implied | STUB ONLY | High |

## The Proxy Advantage

DGP is a **proxy**, not a storage engine. This changes the calculus:

- Some features can be **delegated** to backends (Object Lock passthrough)
- Some features are **gateway-level** (Quota, Policy, Events) — natural fit
- **Encryption at the proxy** is a unique value prop: cheap untrusted cloud + on-prem DGP = encrypted at rest
- **Replication** reframed as background sync to secondary backend — not distributed consensus

The MinIO fork argument doesn't apply — DGP sits IN FRONT of MinIO. Complementary, not competitive.

## Roadmap to 90%

| Phase | Features | Effort | Score |
|-------|----------|--------|-------|
| Current | OIDC, IAM, stability, quota, encryption, replication, lifecycle, event outbox | - | 8/13 (62%) |
| Next | SSE-C, stronger bucket policy, versioning | 1-2 weeks | 10/13 (77%) |
| Next | Full lifecycle/events parity, object lock passthrough | 1-2 weeks | 12/13 (92%) |
| Later | LDAP, STS | 2-3 weeks | 13/13 (100%) |

See individual feature files:
- [ENCRYPTION.md](ENCRYPTION.md) — Transparent encryption at rest
- [REPLICATION.md](REPLICATION.md) — Eventually-consistent background sync
- [QUOTA.md](QUOTA.md) — Per-bucket storage quotas
