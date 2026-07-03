# DGP Architecture & HA Audit — 2026-06-28

> **STATUS (2026-07-03 — partially superseded by shipped work).** Since this
> audit: (a) the **S3-CAS coordination lease shipped** (`src/coordination/` —
> `CoordinationLease` trait, `LocalLease` SQLite default, `S3Lease` on the
> boot-validated coordination bucket): REPLICATION leader election now has
> automatic cross-instance failover, superseding the "job leases are node-local
> SQLite → zero cross-instance exclusion" premise *for the replication path*
> (lifecycle/maintenance/parity/run-now remain node-local, as documented in the
> CLAUDE.md HA contract). (b) The coordination bucket AND named S3 backends
> hosting client-writable buckets are **CAS-validated at boot** (witness-cached
> probe; non-CAS → exit 1; hot-apply gate at runtime), and the
> **`replication_target_only`** bucket marker makes non-CAS backends (B2) safe
> replication destinations. (c) The reference-RMW finding (#3) is **still
> open** — `prefix_locks` remains an in-process DashMap; the new gates BOUND who
> may write, they do not fix the RMW itself. The headline "true round-robin HA
> today? NO" still stands overall, but for narrower reasons than written below.

Expert-swarm audit (1 survey + 7 parallel expert lenses + synthesis, 9 agents,
44 findings). Read-only; every quantitative claim traced to source. Lenses:
architecture, security, correctness, statelessness, HA/load-balancing, debt/perf,
limitations.

> **Execution update (B1 reference-RMW lock — ATTEMPTED, REVERTED):** the fix
> this report proposed for the delta-reference RMW (#3 / "MUST change" item 3 —
> "cross-instance lease via the existing job_store leader-lease machinery") is
> **architecturally void**: `job_store` leases live in each node's **local**
> SQLite file, so a lease table gives **zero** cross-instance exclusion (two
> instances never see each other's rows). A 20-agent review of the built fix
> returned a BLOCKER on exactly this. The lock was reverted. A correct
> cross-instance lock for a shared S3 backend requires **S3-object-level locking**
> (conditional-PUT lock object) or an **external lock service** (Redis/etcd/
> DynamoDB) — a materially larger design than this report assumed. Until then,
> the reference-RMW corrupt-on-concurrent-PUT hazard is mitigated only by
> single-writer-per-deltaspace oper.

## Headline verdict

**Can DGP run as a true round-robin load-balanced HA service today? NO.**

- **Single-instance: production-ready.** Auth is well-hardened (constant-time
  SigV4, AKID length-blinding, fail-closed IAM, JWT alg allowlist, issuer-pinned
  OIDC, SameSite=Strict cookies). No unauthenticated bypass or priv-esc was
  proven. Delta-reconstruction integrity gates are solid.
- **Sticky-session HA: workable with caveats.**
- **True round-robin (non-sticky) HA: not yet** — proven correctness + security
  defects appear under exactly the topology `DGP_CONFIG_SYNC_BUCKET` was built for.

> **Tier-C execution update (C1/C3/C4 — investigated, NOT built):** three of the
> four strategic refactors this report proposed do not survive code inspection.
> **C1 (collapse dual SigV4):** NOT feasible — the hand-rolled `sigv4_auth_middleware`
> is not redundant with s3s; it uniquely owns the replay cache, rate-limiter
> feedback, the `AuthenticatedUser` extension that authorization + the s3s adapter
> read, the x-amz-content-sha256 body-integrity gate, and the anonymous/public-prefix
> short-circuit. axum applies `.layer()` bottom-up, so SigV4+authorization run in the
> middleware stack BEFORE the s3s fallback service — s3s structurally cannot be the
> sole authority feeding an authorization step that runs earlier. The "~1000 LOC
> deleted" premise is false; a two-phase redesign would keep most of the LOC and add
> risk to the auth path. **C3 (promote parity):** declined — parity is cohesively
> coupled to replication (operates on `ReplicationRule`, reuses the replication
> planner's `rewrite_key`/`should_replicate`, `event_consumer`, `remediation`); moving
> it leaks replication internals and the `JobSubsystem` trait is a one-implementation
> abstraction. **C4 (single ConfigView):** declined — the 4 ArcSwaps span two structs
> (engine on AppState w/ 48 read sites; the rest on AdminState); the transition already
> runs the only fallible step first and the torn-read window fails closed (LOW
> severity). Only **C2 (capability-split StorageBackend)** is a genuine, modest
> improvement. Lesson: audit recommendations need their premise verified against the
> wiring before execution.

### The core trap (biggest architectural risk)
**Multi-instance coherence is half-true in a way operators cannot infer.** DGP
ships a multi-instance story (IAM + job leases genuinely share state via
`DGP_CONFIG_SYNC_BUCKET` + `job_store`), creating a reasonable expectation that
it is round-robin-safe. But:
- **Coordination state** (`event_outbox`, `listener_cursors`, replication/
  maintenance **leases**) lives in the SAME SQLCipher file that config-sync
  replaces **wholesale** via `tokio::fs::rename` (`config_db_sync.rs:218`) — so
  lease/cursor semantics that are correct in-process are periodically
  **clobbered/desynced** across instances.
- **Data + auth planes** are purely instance-local: `SessionStore`
  (`session.rs:98`), multipart uploads, `MetadataCache` (10-min TTL, local-only
  invalidate), `RateLimiter`, `MaintenanceGate.busy`, and the delta-reference
  RMW lock (`engine/mod.rs:266`, in-process `DashMap`).

Failure modes only surface under round-robin: intermittent admin 401s, MPU
completing on the wrong node, stale post-delete reads, N× rate-limit bypass,
IP-authz spoofing, and concurrent `reference.bin` corruption.

## Top 10 issues (severity × certainty, deduped)

| # | Sev | Issue | One-line fix |
|---|-----|-------|--------------|
| 1 | HIGH | Coordination state (outbox/cursors/leases) in the wholesale-swapped DB file; sessions/MPU/cache/reference-RMW instance-local | Split coordination tables out of the synced file; share or document every plane; sticky sessions interim (`config_db_sync.rs:218`) |
| 2 | HIGH | X-Forwarded-For taken first-element-verbatim → spoof `aws:SourceIp` IAM + admission CIDR when `DGP_TRUST_PROXY_HEADERS=true` (the documented prod setup) | `DGP_TRUSTED_PROXY_CIDRS` allow-list, parse XFF right-to-left, honor only from trusted peer (`rate_limiter.rs:367`) |
| 3 | HIGH | Cross-instance delta-reference RMW unguarded (`prefix_locks` is in-process) → two nodes corrupt `reference.bin` | Cross-instance lease via existing `job_store` (`engine/mod.rs:266`) |
| 4 | HIGH | In-memory sessions + MPU break under non-sticky round-robin | Shared store, or mandate sticky + fail-loud on non-owned MPU complete (`session.rs:98`) |
| 5 | MED | SigV4 verified **twice** per request — 1638-LOC hand-rolled outer impl AND s3s's re-derivation; divergence hazard (produced the form-POST replay surprises) | Make s3s sole signature authority; outer = authorization-only; ~1000 LOC deleted (`auth.rs:1248`, `startup.rs:346`) |
| 6 | MED | `MetadataCache` no cross-instance invalidation → stale existence/size up to 10 min | Piggyback the event outbox, or drop cache from HA story (`metadata_cache.rs:50`) |
| 7 | MED | SSRF guard does no DNS resolution — hostname resolving to 169.254.169.254 / private space passes literal-IP check | reqwest connect-time resolver hook re-checking resolved addr (OIDC + webhook + S3) (`security.rs:44`) |
| 8 | MED | `StorageBackend` ~30 buffering default-impls silently defeat the streaming/memory-bound guarantee; policed by a grep test, not types | Required core + opt-in capability traits; loud fallback (`traits.rs:934`) |
| 9 | MED | Replication is a 6607-LOC gravity well hiding a distinct 5th job flavour (parity + remediation, own counter + lease) | Promote parity to `src/parity/` behind a `JobSubsystem` trait + CI conformance check |
| 10 | LOW | Hot-reload publishes 4 derived ArcSwaps sequentially/non-atomically; rollback restores only the engine → half-applied config window | One immutable `ConfigView` via a single ArcSwap; one snapshot per request (`config/mod.rs:151`) |

## What MUST change for true round-robin HA
1. **Sessions** → shared/synced store, or mandate sticky sessions in docs.
2. **Multipart** → shared MPU state, or fail `CompleteMultipartUpload` loudly when the instance doesn't own the upload (today it silently fails).
3. **Delta reference RMW** → cross-instance lease (reuse `job_store`), not the in-process `DashMap`.
4. **Coordination tables** → split `event_outbox`/`listener_cursors`/leases out of the file config-sync renames wholesale.
5. **MetadataCache** → cross-instance invalidation (via outbox) or drop from the HA correctness story.
6. **XFF/IP authz** → `DGP_TRUSTED_PROXY_CIDRS` allow-list + right-to-left parse.
7. **Document the contract** in CLAUDE.md: which planes are HA (IAM, jobs) vs single-instance (sessions, MPU, cache, rate-limit, gate, reference RMW).

## Quick wins (high value, low effort)
- **Document the HA contract** (zero code; kills the biggest false-inference trap).
- **`DGP_TRUSTED_PROXY_CIDRS` + right-to-left XFF parse** (closes the HIGH IP-spoof; one function).
- **Fail-loud on non-owned `CompleteMultipartUpload`** (data-loss-shaped error → clear error).
- **Ensure `STREAMING-AWS4-HMAC-*` PUTs actually fail** at SigV4 (verify the `NotImplemented` isn't swallowed).
- **Regression test**: anonymous LIST (no prefix) on a prefix-scoped public bucket returns 0 keys (locks the fragile `$anonymous` ListScope invariant).
- **CI conformance test**: any `src/*/worker.rs` spawn loop must go through `Pager` + a `job_store` lease.

## Strategic bets
- **Collapse the dual SigV4** → s3s sole authority; delete ~1000 LOC of duplicated security-critical code + the divergence hazard.
- **Move sessions + MPU + replay cache to a shared store** and add the cross-instance reference-RMW lease → converts "sticky HA with caveats" into "true round-robin HA."
- **Split coordination tables out of the wholesale-renamed DB file** so multi-instance coordination actually holds.
- **Promote parity to a sibling subsystem** + a `JobSubsystem` trait so the job taxonomy is honest (5 flavours, one enforced spine).
- **Capability-split `StorageBackend`** so the memory-bound guarantee is type-enforced, not grep-enforced.
- **Bundle hot-reload ArcSwaps into one `ConfigView`** → atomic reload + all-or-nothing rollback.

## Other proven findings worth noting
- **`/health` is a hardcoded 200** — never probes storage/DB/sync; no readiness/liveness split (HIGH, ops).
- **No schema forward-compat guard** — an older node silently down-stamps a newer node's synced DB (HIGH).
- **Lost bootstrap password = permanent, unrecoverable IAM-DB loss** (HIGH; document + recovery story).
- **Delimiter-less ListObjectsV2 materializes the entire prefix into memory** before honoring max-keys; pagination re-lists per page (HIGH, perf).

## What's genuinely strong (don't regress)
- The **shared job spine** (`job_store` leader-leases + zombie recovery + failure-ring; `job_loop::Pager` resume/poison-token) — single-sourced, reused by every background flavour.
- The **engine/storage trait seam** cleanly isolating delta logic with single-sourced authorization.
- **Auth hardening** with evidence of prior adversarial review (no proven bypass/priv-esc).
- The **global `Arc<Mutex<ConfigDb>>` never sits on the per-request S3 path** (auth reads the in-memory `IamState` ArcSwap; DB lock is admin/background-only) — the contention worry is bounded.
