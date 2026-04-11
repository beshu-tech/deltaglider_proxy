# Grail Objectives: Enterprise S3 Feature Gap Analysis

## Stefano's Requirements (verbatim, translated)

> The problem with 100% of tools that aren't Ceph or MinIO is they have practically no features beyond basic read/write APIs (they're just translators). Most of the community ignores this, which is baffling.
>
> Switching to something that doesn't implement at least: **replica, SSE-S3, SSE-C, Policy, Lifecycle Rules, Object Locking & Immutability, Event Notification, Quota, LDAP/OIDC login + IAM integration, and STS support** is a problem.
>
> Not to mention **product stability, adequate performance, and sustainable production operations**.

## Current Score: 3/13

| Feature | Required | Status | Gap |
|---------|----------|--------|-----|
| OIDC login | Yes | **DONE** | - |
| IAM integration | Yes | **DONE** (ABAC, groups, conditions) | - |
| Stability / Performance | Yes | **DONE** (Rust, async, tested) | - |
| Replica | Yes | NOT IMPLEMENTED | Critical |
| SSE-S3 | Yes | NOT IMPLEMENTED | Critical |
| SSE-C | Yes | NOT IMPLEMENTED | Critical |
| Bucket Policy (resource-based) | Yes | PARTIAL (public prefixes only) | High |
| Lifecycle Rules | Yes | NOT IMPLEMENTED | High |
| Object Locking / Immutability | Yes | NOT IMPLEMENTED | High |
| Event Notification | Yes | NOT IMPLEMENTED | High |
| Quota | Yes | NOT IMPLEMENTED | Medium |
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
| Current | OIDC, IAM, stability | - | 3/13 (23%) |
| **NOW-1** | Quota | 2 days | 4/13 (31%) |
| **NOW-2** | Transparent Encryption (SSE-Proxy) | 1 week | 6/13 (46%) |
| **NOW-3** | Background Replication | 2 weeks | 7/13 (54%) |
| Next | Lifecycle, Versioning | 1-2 weeks | 9/13 (69%) |
| Next | Events (webhooks), Bucket Policy | 1-2 weeks | 11/13 (85%) |
| Later | LDAP, STS, Object Lock | 2-3 weeks | 13/13 (100%) |

See individual feature files:
- [ENCRYPTION.md](ENCRYPTION.md) — Transparent encryption at rest
- [REPLICATION.md](REPLICATION.md) — Eventually-consistent background sync
- [QUOTA.md](QUOTA.md) — Per-bucket storage quotas
