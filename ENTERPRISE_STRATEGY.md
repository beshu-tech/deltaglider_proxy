# DeltaGlider Enterprise Strategy

## What We Have Today (v0.1.8)

A single-binary, S3-compatible reverse proxy that transparently delta-compresses versioned binary artifacts using xdelta3. Rust, fast, drop-in. Claim of 60-95% storage savings is credible for target workloads (JARs, ZIPs, firmware, ML checkpoints). Filesystem + S3 backends, SigV4 auth, admin GUI, hot-reload config. GPL-2.0 licensed.

**Core value prop**: Sits in an existing workflow (S3 API), requires zero client-side changes, delivers ROI that's trivially measurable ($X/month on S3 before, $Y after).

---

## Target Audience

**Primary**: Platform/DevOps teams at mid-market and enterprise companies (200-5000 engineers) who store versioned binary artifacts in S3 and are paying $50K-$500K+/year in S3 storage + egress costs.

### Beachhead Segments (pick one to start)

| Segment | Buyer | Why |
|---------|-------|-----|
| **CI/CD artifact stores** (JARs, WARs, Docker image layers, Helm charts) | DevOps lead / platform team | Largest addressable market, universal pain |
| **Firmware/embedded OTA** (IoT companies shipping incremental firmware updates) | VP Engineering | High willingness to pay, clear ROI |
| **ML model registries** (checkpoint storage for model training) | MLOps lead | Exploding market, huge file sizes |

---

## Pain Points We Solve

1. **S3 storage costs scale linearly** with version count. Versioned binaries are 90%+ identical. Companies either pay the bill or build fragile cleanup scripts.
2. **Egress costs for artifact distribution** are brutal. Delta-reconstructed pulls reduce bytes transferred.
3. **No existing solution is transparent**. Alternatives require client-side tooling changes (e.g., casync, zsync). DeltaGlider is a drop-in proxy — zero client changes.
4. **Retention policy pressure**: Teams delete old versions to save money, losing audit trail and rollback capability. Delta compression lets you keep everything.

---

## Competitive Landscape & Moat

- **Direct competitors**: Essentially none for transparent S3 delta proxy. This is a greenfield category.
- **Adjacent**: S3 lifecycle policies (not the same thing), git-lfs (requires client tooling), OCI registries with dedup (Docker-specific), Artifactory/Nexus (different problem).
- **Moat**: First-mover in "transparent delta proxy for S3." The moat deepens with: (a) reference baseline intelligence (ML-optimized baseline selection), (b) multi-cloud support, (c) ecosystem integrations (GitHub Actions, GitLab CI, Jenkins plugins).

---

## Enterprise Feature Roadmap

### Phase 1: Production-Ready (0-3 months)
_Goal: First paying design partners_

| Feature | Why |
|---------|-----|
| **Multi-tenant auth** (IAM-style, per-bucket ACLs) | Enterprise won't share one set of SigV4 credentials |
| **Observability** (Prometheus metrics endpoint, structured JSON logs) | Non-negotiable for enterprise ops teams. Export: requests/s, delta ratio histogram, cache hit rate, storage savings per bucket, codec latency p50/p95/p99 |
| **Horizontal scaling** (stateless proxy + shared S3 backend) | Current architecture already supports this conceptually (S3 backend is shared), but needs documentation, testing, and cache coherency |
| **Native delta codec** (replace xdelta3 CLI with Rust library) | Eliminate process spawn overhead, simplify deployment, remove binary dependency |
| **Commercial license** | Dual-license: GPLv2 for OSS, commercial license for enterprise |
| **Helm chart + Terraform module** | Enterprise deploys via IaC, not `docker run` |

### Phase 2: Enterprise Differentiation (3-6 months)
_Goal: Close first $50K+ ARR contracts_

| Feature | Why |
|---------|-----|
| **Storage savings dashboard** (per-bucket, per-prefix, trend over time) | The killer feature for getting budget approval. "We saved $X this month." Finance teams need this |
| **Configurable file routing policies** (per-bucket rules for what gets delta'd) | Enterprises have mixed workloads; one-size-fits-all routing won't work |
| **Lifecycle policies** (auto-promote reference baselines, garbage collect orphaned deltas) | Operational maturity — the reference baseline needs to evolve as artifacts drift |
| **Audit logging** (who accessed what, when, with what result) | Compliance requirement for regulated industries |
| **HA / active-active** (multiple proxy instances, distributed cache coordination) | SLA requirements demand this |
| **Rate limiting + quotas** (per-tenant, per-bucket) | Multi-tenant environments need resource isolation |

### Phase 3: Platform Play (6-12 months)
_Goal: $500K+ ARR, self-serve or managed_

| Feature | Why |
|---------|-----|
| **Managed service option** (SaaS — you host the proxy, customer brings their S3) | Reduces deployment friction, enables usage-based pricing |
| **Multi-cloud backends** (GCS, Azure Blob, R2) | Enterprise is multi-cloud; S3-only limits TAM |
| **SDK / client-side integration** (optional client library that does partial downloads — like HTTP range requests on deltas) | Unlocks egress cost savings on top of storage savings |
| **Data sovereignty controls** (region-pinned deployments, encryption at rest for deltas) | EU/regulated customers require this |
| **SSO / SAML for admin console** | Enterprise procurement checklist item |

---

## Enterprise Auth Strategy

### Rust Ecosystem Reality Check

| Protocol | Library | Maturity | Verdict |
|----------|---------|----------|---------|
| **OAuth2 / OIDC** | [openidconnect-rs](https://github.com/ramosbugs/openidconnect-rs) | v4.0+, strongly-typed, well-maintained | Production-ready. Ship confidently. |
| **SAML** | [samael](https://github.com/njaremko/samael) | v0.0.19, depends on C bindings to xmlsec1 | Risky. Enterprise-grade is a stretch. |
| **LDAP** | [ldap3](https://github.com/inejge/ldap3) | v0.12, pure-Rust, async, TLS support | Good enough for bind auth flows. |
| **Full IdP** | [Kanidm](https://kanidm.com/) | Quarterly releases, active dev | Separate service, not embeddable. |

### Recommended Approach

**Tier 1 (Phase 1): OIDC + OAuth2**
Use `openidconnect-rs`. Covers 80% of enterprise SSO needs — Okta, Azure AD, Google Workspace, Keycloak all speak OIDC. The Rust library is genuinely production-quality.

**Tier 2 (Phase 2, if customers demand it): LDAP bind auth**
Use `ldap3` for simple bind authentication (verify username + password against Active Directory). Straightforward integration — just checking credentials, not building an LDAP server.

**Tier 3 (defer or outsource): SAML**
Two options:
1. **Sidecar pattern**: Tell enterprise customers to deploy an auth proxy like OAuth2 Proxy or Pomerium in front of DeltaGlider. The proxy handles SAML-to-OIDC bridging. This is the standard pattern for Rust/Go infrastructure tools.
2. **Wrap samael carefully**: If a specific deal requires native SAML, use `samael` but treat it as a high-risk dependency — pin versions, test extensively against the customer's IdP.

### The Reverse Proxy Pattern (Cheat Code)
Since DeltaGlider is already a proxy, auth can be layered externally:
```
Client -> Auth Proxy (SAML/OIDC/LDAP) -> DeltaGlider -> S3
```
This is how most infrastructure tools (Grafana, MinIO, Vault) handle enterprise auth. The admin GUI needs OIDC natively, but the S3 API layer can rely on SigV4 credentials issued after upstream authentication.

---

## Pricing Model

| Tier | Price | Target |
|------|-------|--------|
| **Community** (GPL) | Free | Developers, small teams |
| **Pro** | $500/mo per proxy node | Small teams needing commercial license + support |
| **Enterprise** | $2K-5K/mo (usage-based on data processed) | Platform teams, multi-tenant |
| **Managed** | % of storage savings (gain-share model) | "We save you $X, you pay us 0.2X" |

The **gain-share pricing model** is particularly interesting: you can prove ROI trivially with `(original_size - stored_size) * $/GB/month = savings`. The customer literally sees the number. Very few infrastructure products can do this.

---

## Validation Strategy

### Step 1: Quantify the Pain (2 weeks)
- Find 10 companies on HackerNews/Reddit/Twitter who have publicly complained about S3 costs for artifact storage
- Calculate their likely savings from public info (artifact counts, sizes, version frequency)
- Cold outreach: "I estimated you're spending $X/year on S3 for versioned artifacts. We can cut that by 60-90% with zero client changes. 30-minute demo?"

### Step 2: Design Partner Program (4-6 weeks)
- Offer free deployment + white-glove setup for 3-5 companies
- Requirements: they share before/after metrics, do a case study, and give feedback on enterprise features
- Target: companies storing >1TB of versioned binaries

### Step 3: Validate Willingness to Pay (concurrent with Step 2)
- Ask design partners: "If this saves you $5K/month, would you pay $500/month for commercial support + SLA?"
- The answer validates the pricing model before you build the billing system

### Step 4: Build the Landing Page Before the Product
- Put the savings calculator front and center: "Enter your S3 bill -> see projected savings"
- Capture emails from people who can't install yet (waiting for enterprise features)

---

## Open Questions for Founder

1. **Current usage**: Is anyone running this in production today beyond you? Are the numbers on the README (98.4% savings on ReadOnlyREST builds) from your own product?
2. **Licensing intent**: GPL-2.0 today. For enterprise B2B, you'll almost certainly want dual-license (open-core + commercial). Have you decided?
3. **xdelta3 CLI dependency**: The codec shells out to `xdelta3`. For enterprise, this is a risk surface (binary dependency, process spawning overhead, version compatibility). Open to Rust-native delta library?
4. **What workloads are you seeing demand for?** Releases, firmware, ML checkpoints, Docker layers — which is the primary beachhead?
5. **Team**: Solo, or co-founders / engineers? Affects roadmap aggressiveness.
6. **Revenue timeline**: Pre-revenue looking for YC narrative, or early design partners already?
7. **Scale ambitions**: Single-node today. Managed service, self-hosted appliance, or both?
