# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

DeltaGlider Proxy — an S3-compatible proxy with transparent delta compression for versioned binary artifacts. Clients see a standard S3 API; the proxy silently deduplicates using xdelta3 against a per-prefix reference baseline.

## Build & Dev Commands

```bash
# Rust
cargo build --release
cargo fmt --all                # fix formatting
cargo clippy --locked --all-targets --all-features -- -D warnings

# Demo UI (must be built before cargo build — rust-embed embeds dist/)
cd demo/s3-browser/ui && npm ci && npm run build
npm run dev                    # dev server on :5173, proxies /api to :9001

# Tests
# Merge gate (see `.github/workflows/ci.yml`): `cargo test --lib`, curated
# integration batches, delta/memory, frontend lint/tsc/knip, Node
# regression scripts, E2E smoke — not a single `cargo test --all`.
cargo test --lib --locked
./scripts/check-integration-tests-in-ci.sh   # every tests/*.rs appears in ci.yml
cargo test --test delta_test                 # single integration binary
cargo test --test delta_test test_name       # single test
cargo test -- --nocapture                    # show println output

# Before a release or when touching integration tests, run the full matrix locally
# (needs MinIO on localhost:9000 — same as CI):
cargo test --all --locked

# Nightly CI also runs `test-all-nightly.yml` (`cargo test --all` default + s3s).

# Benchmarks (local/manual only — NOT in the CI merge gate, like coverage)
cargo bench --bench codec    # criterion harness for delta encode/decode hot paths (needs xdelta3)

# Docker (multi-stage: UI build → Rust build → slim runtime)
docker build -t deltaglider-proxy .
```

## Documentation (Diátaxis)

`docs/product/` follows Diátaxis strictly — every page is exactly ONE of: `tutorials/` (lessons, executed end-to-end before merge), `how-to/` (goal-named recipes), `reference/` (austere facts), `explanation/` (concepts). Manifest groups mirror the quadrants (Start here · 3× Guides · Reference · Concepts · Releases). Content loads by Vite glob over `docs/product/**/*.md` on BOTH surfaces (`docs-imports.ts`, `marketing/src/lib/docContent.ts`) — there is NO hand-maintained import list. Adding a doc = drop the .md + add its manifest entry; manifest.json must stay in parity with disk (CI: check-docs-registry.sh); `# validate` YAML blocks are linted. Running examples use ONE fixed cast (backends hetzner-fsn1/local-disk/aws-dr; buckets releases/db-archive/downloads; users ci-uploader/backup-bot/dana; group Engineering) — never invent new example names. Marketing 301s for old slugs live in marketing/astro.config.mjs REDIRECTS (sitemap filter derives from it). New docs: pick the quadrant first; mixed-type pages are a regression.

CI merge gate: `verify-integration-test-registry` → `fmt` → `clippy -D warnings` → parallel test jobs (lib, curated integration + extended admin/IAM/replication, delta) → `e2e-smoke` → RustSec audit → Cargo deny → frontend (lint, tsc, knip, Node scripts) → docs/schema → claude-review. See `ci.yml` for the exact `--test` lists.

## Architecture

The S3 protocol surface is served by the **s3s** framework (the `s3s` crate),
NOT hand-rolled axum handlers. `startup.rs::build_s3_router` mounts
`s3_adapter_s3s::DeltaGliderS3Service` (impls `s3s::S3`, ~32 verb methods) as the
axum `fallback_service`. The legacy axum S3 handlers were retired; `s3s` is the
only S3 implementation. The admin API + demo UI stays axum, mounted under `/_/`.

```
HTTP request (axum Router; cross-cutting layers: TraceLayer, body limit, timeout, concurrency cap, CORS)
  → admission/middleware.rs  Pre-auth admission chain (deny / reject / allow-anonymous)
  → api/auth.rs              SigV4 + IAM authorization middleware (bootstrap or per-user IAM), runs before storage
  → HEAD-`/` + POST form-data interceptors (shapes s3s rejects: Cyberduck probe, browser PostObject → api/handlers/form_post.rs)
  → s3_adapter_s3s.rs        fallback_service: the `s3s::S3` adapter — owns S3 parse/DTO/XML/error; delegates product logic to the engine
                             (GET/PUT/HEAD/DELETE, ListObjectsV2, copy, multipart lifecycle, ACL/tagging stubs)
  → api/handlers/            Survivors after the s3s consolidation:
      mod.rs                 AppState (shared by s3s adapter + admin API), audit_log_s3, ensure_bucket_exists helpers
      object_helpers.rs      Shared quota gate + per-object event-outbox enqueue (called by s3s adapter & form_post)
      form_post.rs           Browser multipart/form-data PostObject upload (s3s doesn't model this shape)
      status.rs              /_/health, /_/stats
  → deltaglider/engine/      Orchestration split into submodules:
      mod.rs                 Core engine: route, compress, cache, metadata resolution, RetrieveResponse, validated_key
      store.rs               PUT pipeline: delta encoding, migration, reference management; `store_spooled_delta` (streaming PUT for objects > spool threshold)
      retrieve.rs            GET pipeline: delta reconstruction, streaming, range requests; `reconstruct_delta_to_spool` (streaming GET)
  → deltaglider/spool.rs    Quota'd temp space for streaming codec ops (SpoolDir byte-budget semaphore; `acquire_pair` for the deadlock-safe ref+out reservation; age-based startup orphan sweep). DGP_SPOOL_DIR/`_MAX_BYTES`/`_THRESHOLD_BYTES`/`_ACQUIRE_TIMEOUT_SECS`.
  → storage/traits.rs       StorageBackend trait (async_trait, object-safe). `get_reference_to_file`/`put_reference_from_file` materialise the reference to/from a local file WITHOUT heap-loading it (filesystem hardlinks, S3 streams) — the bounded-memory backbone of the streaming-delta paths.
  → storage/filesystem.rs   Local filesystem impl (xattr metadata via xattr_meta.rs, list_objects_delegated)
  → storage/s3.rs           AWS S3/MinIO impl (S3 user metadata headers, S3Op enum, `classify_s3_error` + `classify_get_error` pure fns with unit-test coverage)
  → storage/encrypting.rs   At-rest encryption wrapper backend (per-backend AES key, dg-encryption-key-id metadata)
  → storage/routing.rs      Multi-backend routing (virtual bucket → real backend)
  → security.rs             Pure security primitives (validate_bucket_name, bucket_name_is_ip_like, outbound-URL SSRF policy) — unit + proptest, shared by CLI URL parser
  → demo.rs                 Embedded UI + admin API router, mounted under /_/
  → session.rs              In-memory session store (OsRng tokens, 4h default TTL)
  → admission/              Admission chain (pre-auth gating):
      spec.rs                Operator-authored YAML wire format (AdmissionBlockSpec, MatchSpec, ActionSpec)
      evaluator.rs           Decision evaluator (first-match-wins over compiled chain)
      middleware.rs          Request-info extraction, marker injection for AllowAnonymous
      mod.rs                 Runtime Match/Action/Decision types, chain builder
  → config.rs               Flat in-memory Config struct + ENV_VAR_REGISTRY; `Config::classify_auth_config` pure auth-mode decision
  → config_sections.rs      Sectioned YAML wire shape (admission/access/storage/advanced) + shorthand expanders
  → iam/                    IAM module:
      mod.rs                 IamState enum, auth mode detection, IAM_VERSION counter
      types.rs               IamUser, AuthenticatedUser, Permission types
      permissions.rs         ABAC evaluation, is_admin, action matching
      middleware.rs          Per-request auth middleware
      keygen.rs              Secure key generation
      declarative.rs         Declarative-mode reconciler (diff_iam → apply_iam_reconcile)
      external_auth/         OAuth/OIDC providers (Google, generic OIDC)
  → config_db/              Encrypted SQLCipher DB (users.rs, groups.rs, auth_providers.rs, declarative.rs; `classify_sqlite_error` pure fn in mod.rs)
  → config_db_sync.rs       Multi-instance IAM sync via S3 (DGP_CONFIG_SYNC_BUCKET). Hosts `reopen_and_rebuild_iam` (shared by startup, periodic poller, and `POST /api/admin/config/sync-now`)
  → event_outbox.rs         Durable object-event outbox: handlers append facts after successful mutations (never block S3 path)
  → event_delivery.rs       Background dispatcher: claims due outbox rows, delivers as webhook JSON or Slack message; disabled unless advanced.event_delivery.enabled + a target
  → slack_format.rs         PURE Slack Block Kit formatter + notification filter for event_delivery format=slack
  → replication/            Event-driven + scheduled bucket replication (engine-routed so encryption/delta stay transparent):
      planner.rs             Pure rewrite_key / should_replicate / plan_batch
      worker.rs              Async copy loop (engine.retrieve source → engine.store dest); resumable cursor + poison-token guard
      scheduler.rs           Periodic tick from replication.tick_interval; executes due rules (skips paused)
      event_consumer.rs      Consumes the event outbox for lazy/event-driven replication (defers busy-gated destinations)
      state_store.rs         ConfigDb wrapper for replication_state / run_history / failures tables (delegates to config_db/job_store.rs)
  → lifecycle/              Object lifecycle (YAML-authored, disabled by default, previewable; acts through the engine): planner/scheduler/worker/state_store.
                            Has crash-resume cursor (scope-stamped `bucket|prefix`, v15 — a redefined same-named rule never replays the old token)
                            + pause/resume parity with replication. Scheduler AND run-now defer when ANY rule write-bucket (source or transition
                            destination — planner::rule_write_buckets) is maintenance-gated
  → maintenance/            One-off bucket jobs (DB-born, not YAML): store.rs (maintenance_jobs/failures; maintenance_requeue_abandoned is
                            LEASE-AWARE and runs at boot + every worker poll tick — a synced DB carrying a peer's LIVE job is never resurrected;
                            maintenance_gate_arm_keys is kind/phase-aware), gate.rs (per-bucket WRITE gate middleware: 503 SlowDown on writes to
                            busy buckets, reads pass; in-flight write drain; admin bulk copy/move/delete loops participate via
                            write_started/finished + per-item is_busy), worker.rs (sequential runner, kind dispatch; heartbeat returns
                            Err(LEASE_LOST) on refused renewal → phase stops, row NOT settled), migrate.rs (kind=migrate: stage→copy→verify→
                            flip→cleanup, transient __dgmigrate_* routes — gated from creation, filtered out of all bucket listings, cleared at
                            flip; pre-flip cancel unwind; cleanup re-checks routed_to_target PER SWEEP; cancel-in-cleanup settles completed with
                            a note), mod.rs (pure: resolve_desired/needs_rewrite/progress_percent/display_percent)
  → config_apply.rs         ConfigMutator: mutate → rebuild engine (rollback on failure) → persist, for BACKGROUND tasks (migrate flips);
                            admin rebuild_engine delegates to rebuild_engine_only
  → job_loop.rs             THE canonical pagination state machine (Pager): token threading, resume detection, poison-token
                            guard (one-shot restart_fresh), MAX_JOB_PAGES cap, truncated_by_page_budget() — phase machines (migrate/reencrypt)
                            MUST fail on budget truncation instead of falling through to the next phase; cursor loops may ignore it
  → config_db/job_store.rs  THE canonical job machinery (identifier-checked): leader-lease acquire/renew (lapsed never resurrects),
                            failure-ring prune (id DESC), zombie-run scan — all three subsystems delegate here
  → transfer.rs             Shared engine-routed copy primitive (retrieve→store, preserve multipart ETags, stamp provenance, retry transient) — used by replication + lifecycle
  → usage_scanner.rs        Background prefix size scanner (cached results, LRU, scan cap)
  → secret.rs               Secret trait (opaque material + non-secret stable id) — formalises the AES-key shape; future KMS/Vault impls plug in here
  → tls.rs                  TLS setup: user-provided PEM or ephemeral self-signed (rcgen)
  → background.rs           Shared background-runner helpers (parse_duration_or for replication/lifecycle/event_delivery)
  → init.rs                 Interactive `--init` config wizard
  → cli/config.rs           `config lint|schema|defaults|apply` + `admission trace`
```

**Key data flow:**
- **PUT**: FileRouter decides delta-eligible vs passthrough → compute delta against reference baseline → store if ratio < threshold, else passthrough
- **GET**: Read metadata → if delta, reconstruct from reference + delta via xdelta3 → stream to client transparently
- **Deltaspace layout**: `bucket/prefix/.dg/reference.bin` + `bucket/prefix/key[.delta]`

**Important types:**
- `StorageBackend` (trait in `storage/traits.rs`) — all storage operations; two impls: Filesystem, S3. Includes `list_objects_delegated()` for optimized delimiter-based listing.
- `SharedConfig` = `Arc<RwLock<Config>>` — hot-reloadable via admin API
- `RetrieveResponse` — enum: `Streamed` (zero-copy passthrough) vs `Buffered` (delta reconstruction, includes `cache_hit: Option<bool>`)
- `FileMetadata` (in `types.rs`) — per-object metadata with DG-specific tags; `fallback()` constructor for unmanaged objects
- `Engine::validated_key()` — shared parse+validate+deltaspace_id helper used by all public engine methods
- `IamState` (in `iam/mod.rs`) — enum: `Disabled`, `Legacy(AuthConfig)`, or `Iam(IamIndex)` for multi-user auth
- `IamMode` (in `config_sections.rs`) — `Gui` (default) = encrypted IAM DB is source of truth; `Declarative` = YAML owns IAM state. In `Declarative` mode: (a) admin-API IAM mutations return 403, and (b) `apply_config_transition` runs the **Phase 3c.3 reconciler** (`src/iam/declarative.rs` + `src/config_db/declarative.rs`) — pure `diff_iam` → single-transaction `apply_iam_reconcile` diffs the YAML `access.iam_users` / `iam_groups` / `auth_providers` / `group_mapping_rules` against the DB and applies creates/updates/deletes atomically. Diff is **by NAME** so user/group IDs stay stable across updates and `external_identities` stay valid. An empty-YAML gate blocks `gui→declarative` flips that would wipe a non-empty DB. Every mutation emits an `iam_reconcile_*` audit entry.
- `ConfigDb` (in `config_db/mod.rs`) — encrypted SQLCipher database for IAM users, groups, OAuth providers, and mapping rules. Stored as `deltaglider_config.db`. Independent of the YAML config file; `access: {}` in YAML with no legacy SigV4 creds is **correct** when IAM users exist in the DB.
- `SectionedConfig` (in `config_sections.rs`) — serde boundary only: the on-disk sectioned YAML shape (`admission` / `access` / `storage` / `advanced`). Collapsed into/from the flat `Config` struct via `into_flat` / `from_flat`. Canonical YAML export always uses the sectioned shape; the flat shape still loads for backwards compat. Hard error on "mixed" shapes (a doc that combines flat-root + section-header keys).
- `AdmissionBlockSpec` (in `admission/spec.rs`) — operator-authored wire format for admission blocks: `name`, `match` (method / source_ip / source_ip_list CIDR / bucket / path_glob / authenticated / config_flag), `action` (allow-anonymous / deny / reject { status, message } / continue). `source_ip_list` capped at 4096 entries; names restricted to `[A-Za-z0-9_:.-]` (max 128 chars) with the `public-prefix:` prefix reserved for synthesized blocks.
- `MetadataCache` (in `metadata_cache.rs`) — 50MB moka-based in-memory cache for `FileMetadata`. Populated on PUT, HEAD, and LIST+metadata=true. Consulted on HEAD, GET, and LIST (even without metadata=true, for file_size correction). Invalidated on DELETE (exact key) and prefix delete (all matching keys). 10-minute TTL. Configurable size via `DGP_METADATA_CACHE_MB` (default: 50).
- `RateLimiter` (in `rate_limiter.rs`) — per-IP token bucket rate limiter for auth endpoints. 100 attempts per 5-minute window, 10-minute lockout after exhaustion (configurable via `DGP_RATE_LIMIT_*` env vars). Expired entries cleaned up periodically.
- `UsageScanner` (in `usage_scanner.rs`) — background prefix size scanner with 5-minute cached results, 1000-entry LRU, and 100K-object scan cap per prefix.
- `S3Op` (in `storage/s3.rs`) — enum for S3 operation context in error classification
- `SessionStore` (in `session.rs`) — in-memory session store with OsRng token generation, configurable TTL (`DGP_SESSION_TTL_HOURS`, default 4h), IP binding, max 10 concurrent sessions with oldest-eviction.
- `env_parse()` / `env_bool()` / `env_parse_with_default()` (in `config.rs`) — DRY helpers for environment variable parsing
- `PublicPrefixSnapshot` (in `bucket_policy.rs`) — pre-built index of public prefix config for the SigV4 auth middleware. Stored in `Arc<ArcSwap<...>>` for lock-free reads, rebuilt on config hot-reload. When a request targets a public prefix without auth credentials, an anonymous `$anonymous` `AuthenticatedUser` is constructed with scoped read+list permissions (including `s3:prefix` conditions for LIST scoping). Synthesized admission blocks with name prefix `public-prefix:` are derived from bucket `public_prefixes` entries.
- `AuditEntry` (in `audit.rs`) — serde-serialisable structured audit record (timestamp / action / user / target / ip / ua / bucket / path). Every `audit_log()` call pushes a sanitised copy onto a bounded `VecDeque<AuditEntry>` (parking_lot Mutex; default 500 entries, override via `DGP_AUDIT_RING_SIZE`). `recent_audit(limit)` snapshots newest-first for the admin GUI; stdout emission via `tracing::info!` is unchanged.
- `CommandPalette.CommandAction` (frontend, `demo/s3-browser/ui/src/components/CommandPalette.tsx`) — `{ id, label, hint?, keywords?, icon, shortcut?, onRun }`. Nav commands are derived from `ADMIN_IA` (exported from `AdminSidebar`); shell-scope extras (Export YAML, Import YAML, Setup wizard, Keyboard shortcuts, Back to Browser) are passed in via `extraActions`. Recents MRU stored as last-5 ids in localStorage.

**Config:** The ONLY format is **YAML** (`deltaglider_proxy.yaml`) with four optional top-level sections — `admission`, `access`, `storage`, `advanced`. TOML support was removed in v1.4.1: a `.toml` config (via `DGP_CONFIG`/`--config` or found on the search path) is a fatal startup error pointing at the one-time conversion path (`config migrate` on v1.4.0); persist to a `.toml` target errors the same way; `config migrate` and `DGP_SILENCE_TOML_DEPRECATION` are gone. Env var overrides (`DGP_*` prefix) apply on top of the loaded file. **`${env:NAME}` / `${env:NAME:-default}` references in the file are expanded in-process against the environment when the file is loaded from disk** (the in-program replacement for an external `envsubst` — see `expand_env_vars` in `config.rs`, hooked into `from_yaml_file`, `config lint`/`apply`, AND the admin `/config/apply` doc-body path, which expands against the SERVER env; NOT `from_yaml_str`/section PUTs). **Env refs round-trip**: `expand_env_vars_recording` stores `name→value` provenance in `Config::env_refs` (`#[serde(skip)]`); `with_env_refs_reinserted` re-emits `${env:NAME}` for any matching string scalar at persist AND export time, and every redactor keeps `is_env_ref` values — so the IaC loop (template → GUI tweaks persisted in-container → export → back to IaC) is lossless for ref-sourced secrets. The `env:` prefix is mandatory and deliberate — it's one namespace among several `${ns:name}` forms; the IAM permission templates `${iam:username}` / `${iam:access_key_id}` (substituted per-request at auth time, see `iam/permissions.rs`) and any other non-`env:` `${...}` pass through the config expander untouched. `$$` is a literal `$`; an unset `${env:NAME}` with no default fails loudly at load. See `deltaglider_proxy.example.yaml` (canonical). Per-bucket policies support `public_prefixes` (and the `public: true` shorthand) for unauthenticated read-only access. Config file-search order: `DGP_CONFIG` env > `./deltaglider_proxy.yaml` > `.yml` > `/etc/deltaglider_proxy/config.{yaml,yml}` (a leftover `.toml` matched by the search is a fatal startup error, never silently skipped).

## Authentication & IAM

The proxy **refuses to start** without authentication credentials unless `authentication = "none"` is explicitly set (dev only). Two auth modes at runtime, determined by whether IAM users exist in the config DB:

- **Bootstrap mode**: Single credential pair from YAML/env vars (`DGP_ACCESS_KEY_ID` + `DGP_SECRET_ACCESS_KEY`). Admin GUI requires the bootstrap password. This is the default on fresh installs.
- **IAM mode**: Per-user credentials from encrypted SQLCipher DB (`deltaglider_config.db`). Admin GUI access is permission-based (no password needed for IAM admins).
- **Open access** (dev only): Set `authentication = "none"` or `DGP_AUTHENTICATION=none`. No SigV4 verification.

Orthogonal to bootstrap/IAM mode, the **`access.iam_mode` YAML selector** (Phase 3c) controls *where IAM state lives*:

- `gui` (default) — encrypted SQLCipher DB is the source of truth. Admin GUI + admin API mutate the DB directly.
- `declarative` — YAML is authoritative. Admin API IAM mutation routes (`POST/PUT/PATCH/DELETE` on `/users`, `/groups`, `/ext-auth/*`, `/migrate`, backup import) return `403 { "error": "iam_declarative" }`. Read endpoints stay accessible for diagnostics. **Phase 3c.3 reconciler (shipped)**: every `/config/apply` or section-PUT on `access` runs `diff_iam` (validates YAML — unique names, valid group refs, valid permissions, no access-key collisions; zero DB writes on validation failure) followed by `apply_iam_reconcile` (all creates/updates/deletes in one SQLite transaction). Diff-by-name means a renamed access_key_id is an UPDATE that preserves the DB row id, so external_identities stay valid through rotations. The initial `gui→declarative` flip is gated: if the incoming YAML has no `iam_users`/`iam_groups`, apply fails loudly rather than wiping the DB. Mode transitions are audit-logged (warn-level); individual reconcile mutations emit `iam_reconcile_user_create` / `_update` / `_delete` / `_group_*` / `_provider_*` audit entries.

The **bootstrap password** is a single infrastructure secret that:
1. Encrypts the SQLCipher config DB
2. Signs admin GUI session cookies
3. Gates admin GUI access in bootstrap mode (before IAM users exist)

Auto-generated on first run (printed to stderr when stderr is a TTY; hidden in containers/CI — only the bcrypt hash is logged). Reset via `--set-bootstrap-password` CLI flag (warning: invalidates encrypted IAM database).

IAM users have ABAC permissions: `{ actions: ["read", "write", "delete", "list", "admin"], resources: ["bucket/*"] }`. Admin = wildcard actions AND wildcard resources. The IAM DB is independent of the YAML config file — `access: {}` in YAML with no legacy creds is correct when users/groups/OAuth providers live in the DB. Multi-instance sync via S3 (`DGP_CONFIG_SYNC_BUCKET` / `config_sync_bucket`) uploads the encrypted DB after every mutation; readers poll S3 every 5 minutes and download on ETag change.

Key files: `src/iam/` (types, permissions, middleware, keygen, declarative reconciler, `external_auth/` OAuth/OIDC; `bump_iam_version`/`current_iam_version` + `GET /_/api/admin/iam/version` power the deterministic rebuild barrier used by tests), `src/config_db/` (SQLCipher CRUD split into users/groups/auth_providers/declarative + `classify_sqlite_error` in mod.rs), `src/config_db_sync.rs` (S3 sync + `reopen_and_rebuild_iam`), `src/api/admin/` (auth, users CRUD, config, groups, external_auth, backup, scanner, audit, plus replication / lifecycle / event_outbox / backends / savings panels; `with_config_db()` in `mod.rs` wraps the "lock DB → run closure → log-and-500" boilerplate; `external_auth.rs` hosts `validate_mapping_rule` + the `EXT_AUTH_VERSION` counter), `src/api/admin/config/{document_level,field_level,section_level,password,trace}.rs` (section-level uses RFC 7396 merge-patch; `mod.rs` hosts `POST /api/admin/config/sync-now` — operator affordance for forcing an immediate pull from the sync bucket).

## Multi-instance / HA contract (READ before deploying behind a load balancer)

DGP is **single-instance production-ready**. Behind a load balancer it is
**multi-instance for SOME planes only** — do NOT assume true round-robin HA. Run
N instances behind a **sticky-session** LB, not naive round-robin, until the
single-instance planes below are addressed.

**Shared across instances (genuinely HA):**
- IAM / OAuth providers / mapping rules / session revocations — the encrypted
  SQLCipher DB synced via `DGP_CONFIG_SYNC_BUCKET` (ETag-poll every 5 min →
  eventually consistent, ≤5min lag; mutations push immediately with
  reconcile-then-retry on CAS conflict). The sync IS atomic at the S3-OBJECT
  level (`config_db_sync.rs` PUT uses `If-Match`/`If-None-Match` → 412 →
  reconcile-retry, a real compare-and-swap) — but the IAM merge is
  DELETE-all+INSERT-all table-replace, i.e. **row-level LAST-WRITER-WINS**: a
  concurrent IAM edit on peer B can silently revert node A's row (only
  `session_revocations` merges monotonically via `MAX`). So: linearizable blob,
  LWW rows, coordination tables excluded entirely. This shape is fine for
  human-paced identity and structurally WRONG for leases/locks — which is
  exactly why B3 dropped the coordination tables (below) rather than sync them.

**Instance-LOCAL (NOT shared — break or degrade under non-sticky round-robin):**
- **Background-job leadership** — the SQLite leases (`config_db/job_store.rs`) are
  NODE-LOCAL (B3 dropped coordination tables from the sync set). **Exception —
  REPLICATION now has an OPTIONAL cross-instance lease** (`src/coordination/`):
  when a CAS-capable coordination bucket is configured (`config_sync_bucket`), the
  replication scheduler elects a single leader via an S3-CAS lease OBJECT
  (`_dgp/leases/replication/<rule>.json`) → a dead leader's lease lapses (≤TTL,
  default 300s) and a peer steals it = automatic failover, no double-run. The seam
  is `CoordinationLease` (trait) with `LocalLease` (SQLite, single-instance default)
  and `S3Lease` (shared) impls, selected once at startup; `durable_node_id` lets a
  rebooted node reclaim its own live lease. **Still node-local:** LIFECYCLE (hourly,
  pure-plan-idempotent), MAINTENANCE (operator one-offs), PARITY, and admin
  RUN-NOW — those still double-run under round-robin. **Cross-node kill/pause,
  resume-from-cursor on takeover, and post-failover run-history are NOT delivered**
  (cursor / `cancelling`+`paused` rows / run_history stay node-local) — a cross-node
  takeover RESTARTS the run (safe: idempotent copy / generation-pin / provenance-
  gated delete — just wasteful). A non-CAS coordination backend (B2/501) falls back
  to `LocalLease` (every node runs; tolerable churn) — never a SPOF.
- **Admin/browser sessions** (`session.rs`, in-memory) — a cookie minted on node A
  is invalid on B (intermittent 401s). Sticky sessions required for the admin GUI.
- **Multipart uploads** (`multipart.rs`, in-memory) — UploadPart/Complete must hit
  the SAME node as CreateMultipartUpload (else `NoSuchUpload`; the error message
  now says so). Supported answer: consistent-hash by URL path at the LB — on k8s the
  official operator (`operator/`, CRD DeltaGliderProxy) deploys an HAProxy router
  doing exactly that (`balance uri path-only`); it's the ONLY multipart strategy
  implemented (no cross-pod MPU state). Trade-offs in operator/README.md +
  docs/product/how-to/scale-out-with-the-kubernetes-operator.md.
- **Metadata cache** (`metadata_cache.rs`, 10-min TTL, local invalidate) — a
  DELETE/PUT on A leaves B serving stale existence/size for up to 10 min.
- **Rate limiter** (`rate_limiter.rs`, per-instance) — effective limit is N× the
  configured cap across N nodes.
- **Maintenance write-gate busy-set**, **delta-reference RMW lock**
  (`engine/mod.rs` `prefix_locks`, in-process) — concurrent same-prefix PUTs on
  two nodes can corrupt `reference.bin`. Single-writer per deltaspace assumed.

**Hard prerequisites for any multi-instance deployment:**
- **All instances MUST share the same `DGP_BOOTSTRAP_PASSWORD_HASH`** — it
  encrypts the synced SQLCipher DB; a mismatch makes the synced DB unreadable on
  the other node (`config_db_mismatch` then locks the S3 API + blocks sync).
- The **filesystem** storage backend is per-node local disk — NOT shareable across
  instances. Multi-instance needs a shared backend (S3/MinIO) or per-node buckets.
- Config-apply on one instance does NOT propagate to others except via the IAM/DB
  sync; YAML config itself is per-instance.

**Backend-role CAS boundaries (enforced, guard A+B):**

| Role | CAS needed | Enforcement |
|---|---|---|
| Coordination bucket (`config_sync_bucket`) | always | boot probe → exit(1) |
| Client-writable delta storage, multi-instance | yes | startup probe of every named S3 backend hosting such buckets + hot-apply pre-commit gate (`apply_config_transition` step 0) → exit(1)/reject |
| Client-writable delta storage, single-instance | no | in-process prefix lock (no probes, zero cost) |
| `replication_target_only` bucket (any backend incl. B2) | no | client writes → 403 (`check_client_write_allowed`, ten s3s/form-POST/admin-bulk call sites) → replication is the single writer |

The `replication_target_only` bucket marker (`bucket_policy.rs`) is what makes a
non-CAS backend (Backblaze B2 answers conditional writes with 501) safe as a cheap
replication destination. Verdicts live in `BackendCapabilityCache`
(`src/coordination/capability.rs`, stamped on `GET /backends` → BackendsPanel
banner), keyed to the backend DEFINITION fingerprint (redefine = re-probe; no
witness object in data buckets — it would be client-visible and block
DeleteBucket). Probe outcomes: 412=CAS, 200-or-501=definitive non-CAS,
transport error=Unknown (warn, never fatal). Every enforcement message
ends with the doc URL (`CAPABILITY_DOC_URL` → `docs/product/how-to/backend-capability-validation.md`);
the GUI linkifies it (`LinkifiedText`). Test seam: `DGP_TEST_FORCE_NONCAS_BACKEND`.
`Config::check()` warns on orphaned markers, lifecycle rules writing into marked
buckets, and overlapping replication destination prefixes.

See `docs/plan/architecture-ha-audit-2026-06-28.md` for the full analysis and the
roadmap (audit Tiers A→C) toward true round-robin HA. `/_/health` is liveness-only
(fast, no I/O); `/_/ready` does a real backend + config-DB probe (503 when not
ready) — point LB readiness checks at `/_/ready`.

## Frontend (demo/s3-browser/ui)

React 18 + TypeScript + Ant Design 6 + Recharts. Path-based routing (`/_/browse`, `/_/upload`, `/_/metrics`, `/_/docs/configuration`, `/_/admin/users`). Custom `usePathRouter` hook (no react-router dependency). `NavigationContext` provides `navigate()` and `subPath` to child components. Embedded in the Rust binary via `rust-embed` and served under `/_/` on the same port as the S3 API (e.g., `http://localhost:9000/_/`). The `/_/` prefix is safe because `_` is not a valid S3 bucket name character. Single-port architecture: no separate UI port.

**Embedded browser sessions (`dgp_session`):** S3 secrets stay off `localStorage`; reload restores via the cookie + `GET /_/api/admin/session/s3-credentials`. **AdminGui** sessions (bootstrap password, `login-as` for IAM admins, OAuth) can call the full admin API. **S3BrowserLift** sessions (`POST /_/api/admin/session/browser-connect` for IAM non-admins, `POST …/open-browser-connect` when `authentication: none`) only reach logout, `GET /session`, and s3-credentials — config/IAM/etc. return **403** (`admin_session_required`). `GET /_/api/admin/session` returns `{ valid, admin_gui }` so the UI can enable folder usage scan only when `admin_gui` is true. **Proxy restart** clears all sessions (in-memory store). Same-origin only: do not enable permissive CORS with cookie auth.

The admin IA is **5 groups / 17 leaves** (`components/adminNavigation.tsx` `ADMIN_IA`; see `docs/plan/admin-ui-taxonomy.md`): Observability(dashboard/trace/delta-efficiency/audit/system-logs) · Access(credentials/users/groups/external-auth/admission) · Storage(backends/buckets/**Jobs**) · Integrations(event-delivery/event-outbox) · System. **Jobs** is ONE screen for replication+lifecycle rules and one-off reencrypt/migrate jobs (unified table over GET /jobs, drawer with Definition/Runs/Failures, TWO storage-section editors behind one dirty bar with a SEQUENTIAL apply queue). **System** is ONE page stacking Listener&TLS/Caches/Limits/Logging/Sync/Backup cards, each with its own dirtyKey + inline dirty bar. Section panels share TWO content-column widths (`CONTENT_FORM`/`CONTENT_WIDE` in `shared-styles.ts`), not per-panel maxWidth. Every old URL scheme remaps via `src/adminPathRemap.ts` (exhaustive table, regression-tested). `SidebarEntry` carries `dirtyKeys[]` (multi-editor leaves), `applyKey` (⌘S dispatch), and `saveModel` — TabHeader shows a 'Saves immediately' / 'Review & apply' badge per page. Pure job-view logic in `src/jobsView.ts`. A first-run setup wizard at `/_/admin/setup` covers the zero-to-working flow.

**Keyboard shortcuts** (waves 10 + 10.1) mounted on AdminPage: `⌘K` / `Ctrl+K` opens the `CommandPalette` (fuzzy nav over every entry in `ADMIN_IA` + shell-scope actions, recents MRU, group headings for Recent/Navigate/Actions); `⌘S` / `Ctrl+S` dispatches Apply to the currently-visible dirty section via `requestApplyCurrent()` (falls through to the browser default when no section handler is registered); `?` opens `ShortcutsHelp` (platform-aware — ⌘ on Apple / Ctrl elsewhere via `platform.ts::metaKeyLabel()`). Strict modifier match on the palette binding avoids hijacking ⌘⇧K. Listeners are gated on `authed` so the bootstrap login screen isn't affected.

**Mobile drawer** (wave 10.1 §10.4) — below 900px (`useIsNarrow(900)` in AdminPage) the persistent sidebar collapses to an AntD `Drawer` slid from the left; a hamburger in the header opens it; navigation auto-closes it.

**Audit log** (Wave 11) — `src/audit.rs` maintains an in-memory `VecDeque<AuditEntry>` ring (default 500 entries, `DGP_AUDIT_RING_SIZE`) that mirrors every `audit_log()` call. `AuditEntry` is serde-serialisable with ISO-8601 UTC timestamp. `GET /api/admin/audit?limit=N` (session-gated, not IAM-gated) powers `AuditLogPanel` at `/_/admin/diagnostics/audit`. Stdout / JSON log shippers see nothing change — the ring is supplementary.

**Trace diagnostics** (Wave 9) — `TracePanel` at `/_/admin/diagnostics/trace` calls `POST /api/admin/config/trace` and renders a Kiali-style reason path (decision tag + matched block + resolved request + example chips + Copy-as-JSON).

Key components: `MetricsPage` (Prometheus dashboard + analytics with Monitoring/Analytics tab toggle), `AnalyticsSection` (cost savings dashboard with per-bucket charts), `ObjectTable` (sortable, double-click preview, bulk selection), `BulkActionBar` (Copy/Move/ZIP/Delete for selected objects), `DestinationPickerModal` (bucket+prefix picker for copy/move), `InspectorPanel` (object details drawer with download, share duration selector, storage stats, metadata), `FilePreview` (double-click preview for text/images), `AdminPage` (full-screen settings container with hierarchical routing + keydown shortcuts + mobile drawer), `AdminSidebar` (4-group IA; amber dot for sections with unsaved edits; `ADMIN_IA` exported for the command palette), `CommandPalette` (⌘K palette with `CommandAction` + recents MRU + group headings), `ShortcutsHelp` (? modal — platform-aware key glyphs), `CopySectionYamlButton` (compact header-mounted section-scoped Copy YAML), `FormField` (label + YAML-path breadcrumb + help + default-placeholder + override-indicator + owner-badge wrapper), `ApplyDialog` (plan→diff→apply modal), `MonacoYamlEditor` (lazy-loaded Monaco + monaco-yaml with scoped JSON Schema), `IamSourceBanner` (explains DB vs YAML ownership for Access pages), `UsersPanel` (master-detail IAM user CRUD; list labels show direct-rule count AND group inheritance), `UserForm`, `AuthenticationPanel` (OAuth/OIDC providers, group mapping rules — "+ Add Rule" flushes pending edits before reload), `BackendsPanel` (storage backends), `BucketsPanel` (per-bucket policies with tri-state public read toggle: None / Specific prefixes / Entire bucket), `AdmissionPanel` (operator-authored block editor with drag-reorder + per-block form & YAML views), `GroupsPanel` (resets form + navigates to new row on successful Create), `TracePanel` (synthetic request → admission decision visualiser), `AuditLogPanel` (in-memory audit ring viewer with colour-coded Action tags + filter + 3s auto-refresh), `SetupWizard` (first-run 5-step onboarding at `/_/admin/setup`), `SimpleSelect`/`SimpleAutoComplete` (custom dropdowns — Ant Design popups are broken in this layout), `OAuthProviderList` (shared OAuth buttons), `TabHeader` (centered tab headers), `DocsPage` (embedded markdown docs with search, Mermaid diagrams, lightbox), `DocsLanding` (landing page with screenshots and feature cards), `FullScreenHeader` (shared header for Admin/Docs with branding + theme toggle + `extra` slot for hamburger + Copy/Export/Import buttons), `YamlImportExportModal` (full-document YAML round-trip). `useDirtySection` hook backs per-panel dirty state; `useApplyHandler` registers per-section Apply callbacks for ⌘S dispatch; `useDirtyGlobalIndicators` drives the `● ` tab-title prefix and beforeunload guard. `useSectionEditor` (`demo/s3-browser/ui/src/useSectionEditor.ts`) is the single home for the fetch → dirty → validate → ApplyDialog → PUT → markApplied pipeline (with `pick`/`toPayload` hooks for subset-editing Advanced sub-panels and the Admission array↔`{blocks}` shape); new section panels should plug into it rather than re-carrying ~150 LOC of boilerplate.

**Convergence primitives (PR #24).** Three shared frontend primitives replaced copy-pasted variants — reuse, don't re-implement: `MaskedSecretInput` (`components/MaskedSecretInput.tsx`) — the single masked-secret field; `mode="sentinel"` (shows empty while carrying `REDACTED_SENTINEL`, passes it through untouched on save — webhook headers, Slack bot token) vs `mode="blank-keeps"` (blank = keep existing, non-blank = rotate). `RowListEditor<T>` (`components/RowListEditor.tsx`) — stable-`id`-keyed add/remove/update-by-id list scaffolding (never array index); the consumer owns `renderRow` + folds the next array into its single source of truth. `StatePlaceholders` (`components/StatePlaceholders.tsx`) — `LoadingState` / `EmptyState` chrome (consistent centering/padding/type scale). **IAM panels are react-query-backed**: the `queries/{groups,authProviders,mappingRules,users,backends,...}.ts` hooks (keyed by `qk.*`) own reads + per-record mutations and invalidate on success — the old `loadData()`/prop→state-mirror idiom is gone (residual `loadData` mentions in panels are tombstone comments). `FormField`'s yaml-path chip is hover/focus-only (`.dg-field .dg-yaml-path` in theme.css) so the bold label leads; `SectionHeader` owns its own header gap.

Admin API at `/_/api/admin/*` (login, login-as, whoami, users CRUD, groups, config, auth providers, mapping rules, backup, config/section/:name, config/export, config/apply, config/validate, config/trace, config/defaults, config/sync-now, iam/version, ext-auth/version, **audit**, **jobs**: `GET /jobs` unified row over replication rules + lifecycle rules + maintenance one-offs (id `replication:<rule>`/`lifecycle:<rule>`/`maintenance:<n>`, normalized status, percent/progress), `GET /jobs/:id/runs|failures`, `POST /jobs/:id/{pause,resume,run-now,preview,cancel}` (per-kind capability matrix), `POST /jobs/reencrypt`, `POST /buckets/:bucket/migrate` → 202 job; session-light `GET /jobs/bucket/:bucket` powers the browser busy banner). The old per-subsystem /replication*, /lifecycle*, /maintenance* routes are GONE. `POST /api/admin/backup` restores `external_identities` (v2+, with ID remapping + fallback heuristics for legacy backups without explicit user IDs). Whoami returns user identity from session (name, access_key_id, is_admin, version). S3 operations in `s3client.ts` (includes copyObject, listAllKeys, getObjectBytes). Metrics at `/_/metrics`, stats at `/_/stats` (metadata=true for accurate delta sizes), health at `/_/health` (no version — security). Error pages respect user theme (dark/light via localStorage + CSS prefers-color-scheme). **Ant Design tooltips are globally disabled** via CSS (`display: none !important` on `.ant-tooltip, .ant-popover`) — use native `title` attributes instead. The AntD 6 radio/checkbox "shrink on click" default is disabled in `theme.css`.

## Testing

Tests in `tests/` use a `TestServer` harness (`tests/common/mod.rs`) that spawns a real proxy instance with a temp directory (filesystem backend) or MinIO (S3 backend). Port allocation uses an atomic counter starting at 19000. `wait_ready` checks `process.try_wait()` BEFORE the health probe so a stray proxy holding the test port fails loudly (`lsof -i :<port>`) instead of silently intercepting requests.

S3 integration tests require MinIO running on localhost:9000. CI starts MinIO automatically; locally, use `docker run -p 9000:9000 minio/minio server /data`.

Async rebuild barrier: after any IAM mutation, use `get_iam_version(&http, &endpoint).await` BEFORE the call and `wait_for_iam_rebuild(&http, &endpoint, before_version).await` AFTER. Backed by a monotonic `AtomicU64` exposed at `GET /_/api/admin/iam/version` — replaces blind `sleep(1s)` with a ~50ms deterministic poll. HA sync is exercised via `POST /api/admin/config/sync-now` in `tests/config_sync_ha_test.rs`.

Property tests via `proptest` (dev-dep) live in the same module as the pure functions they exercise (see `src/security.rs` for `validate_bucket_name` / `bucket_name_is_ip_like`). Coverage is collected via `cargo-llvm-cov` as a non-blocking CI job — use it as signal, not a gate.

**Prod-config regression** is two-layered: `prod_shape_tests` in `src/config.rs` (lib tests, in the CI gate) validate `tests/fixtures/prod_shape_config.yaml` — a SANITIZED, structure-true snapshot of the production config (declarative IAM + conditions + `${iam:username}`, OIDC + mapping rules, s3 + encrypted-filesystem backends, routing, public_prefixes, replication/lifecycle rules) — through parse, reconciler validation, auth classification, and canonical-export round-trip. Update the fixture deliberately when prod adopts new features; keep it sanitized (public repo). `scripts/test-prod-config.sh` (local-only, pre-release) boots the current branch's release binary against the REAL prod state (backup zip, or a clone of `/private/tmp/dgp-prod-local` incl. the `.deltaglider_bootstrap_hash` sidecar) with expectations derived dynamically from the prod YAML itself; it only ever writes to filesystem-routed buckets, never to remote backends.

## Conventions

- Clippy warnings are errors in CI (`-D warnings`)
- The proxy is transparent: clients must not know delta compression is happening
- `x-amz-storage-type` response header exposes storage strategy (delta/passthrough/reference) for debugging
- Delta-eligible file types are defined in `deltaglider/file_router.rs`
- Passthrough files (images, video) skip delta entirely — already compressed
- Streaming is preferred for large files; delta reconstruction requires buffering the reference
- Parse env vars through `env_parse` / `env_bool` / `env_parse_with_default` in `config.rs` — these are THE convention; don't hand-roll `std::env::var(...).ok().and_then(...)` at call sites

## Testability principles (write code that's testable from day one)

- **Pure functions at decision points.** When a handler is deciding "is this request valid / authorized / retryable", extract the decision into a `fn(typed_input) -> typed_output` and have the handler do I/O around it. Prior art: `classify_s3_error` / `classify_get_error` in `src/storage/s3.rs`, `validate_bucket_name` / `bucket_name_is_ip_like` in `src/security.rs`, `classify_sqlite_error` in `src/config_db/mod.rs` (NotFound / Conflict / Other from a `rusqlite::Error`), `validate_mapping_rule` in `src/api/admin/external_auth.rs` (regex-compile gate for OIDC mapping rules), `Config::classify_auth_config` in `src/config.rs` (credentials-enabled / open-access / fatal auth-mode decision), `expand_env_with` in `src/config.rs` (pure `${env:NAME}` expander; the env lookup is injected so the whole truth table — defaults, escapes, IAM-template passthrough, UTF-8 — is unit-tested without touching the real environment). All are unit-tested against the full truth table (and proptested where applicable) without spinning up a server.
- **Expose observable counters for async state transitions.** The `IAM_VERSION` `AtomicU64` in `src/iam/mod.rs` + `GET /_/api/admin/iam/version` turned a 1s sleep into a 50ms deterministic barrier (`wait_for_iam_rebuild` in `tests/common/mod.rs`). `EXT_AUTH_VERSION` in `src/api/admin/external_auth.rs` + `GET /_/api/admin/ext-auth/version` mirrors the pattern for OAuth/OIDC-provider rebuilds (`bump_ext_auth_version` / `current_ext_auth_version`). Future async state changes (config reloads, external-auth discovery, scanner refreshes) should follow the same pattern: a monotonic counter bumped after the new state is published, polled by whoever needs to wait for it.
- **Don't put test-relevant helpers in binary-only modules.** `reopen_and_rebuild_iam` had to move from `src/startup.rs` (binary) to `src/config_db_sync.rs` (library) before the admin `sync-now` handler could reach it. Rule: if a helper might be called from more than one trigger (startup, periodic task, admin endpoint, test), put it in the library side from day one — `src/startup.rs` is the last mile, not a grab bag.
- **Test the observable contract, not the implementation.** Integration tests cost a TestServer spawn (~200ms + MinIO when S3). Unit-test anything that doesn't need the whole HTTP/SigV4/storage stack; reserve integration tests for request-pipeline seams, concurrency races, and cross-subsystem invariants (metadata cache vs storage, admission vs auth, etc.). The hygiene pass in `5f38941` deleted 659 LOC of integration tests that were re-covering pure-function logic. **But demote only when the assertion is TRULY pure**: a test that spawns a server but asserts real emergent behaviour — xdelta3 actually compressing (`savings_test`), on-disk corrupt xattrs degrading gracefully (`metadata_validation_test`), s3s XML-body parsing → 400 (`error_test`) — is NOT demotable; a filename/header saying "runs the same operations as X" is a hint to VERIFY, not a demotion warrant (both `s3_backend_test` and `public_prefix_list_test` were already trimmed to their irreducible seams). The pure-mapping half CAN move: `engine_error_to_s3s`'s NoSuchKey→code mapping is a unit test; the wire round-trip that proved it was redundant.
- **Property tests for pure validators.** Anywhere there's an "is this valid / allowed / well-formed" check, proptest it alongside hand-picked cases. Saves enumerating 20 shapes and catches the weird input nobody thought of. Keep the enumerated cases too — they document the specific invariants a reader should see at a glance. Stress with `PROPTEST_CASES=2000` locally before a big change.
- **Watch for cross-test contamination.** Each test spawns its own `TestServer` on a unique port, but shared disk artifacts (config DBs in CWD) and stray processes holding ports have bitten us. `wait_ready` now fails loudly on port collision — preserve that safety net when touching `tests/common/mod.rs`. Env-var-mutating tests need serialisation (see the `static LOCK` pattern in `src/rate_limiter.rs` tests).
- **Use `cargo-llvm-cov` output to steer.** Coverage numbers are signal, not a gate. If a new hot-path file lands at 0 line coverage that's a flag; don't let it merge without explaining why (e.g. MinIO-only branches intentionally excluded from the unit run). Coverage + supply-chain (cargo-deny/audit) now run in `test-all-nightly.yml`, NOT on the PR gate — the PR gate is correctness-only. The PR CI uses **build-once-consume-many**: `build-ui` and `build-bin` produce the UI `dist/` and release binary as artifacts once; every downstream job `needs:` + downloads them instead of re-running `npm ci` / `cargo build --release`. Don't reintroduce a per-job UI/binary build.

## When proposing architecture

Anti-patterns we've fought recently — don't reintroduce them:
- Duplicating code across "this is DRY but awkward" boundaries. See the deleted `default_*` duplicates in `config_sections.rs` (`0742780`) — the `config.rs` defaults are now `pub(crate)` and reused. If a shape is duplicated in two places "because they serve different serdes," question it.
- Creating a new admin endpoint for test convenience. `POST /api/admin/config/sync-now` exists as an OPERATOR affordance (force an immediate sync after a known-good change on another instance). It happens to enable HA tests — but that's the bar: endpoints ship only when there's a real production use case. Don't add test-only routes.
- Writing integration tests for logic a unit test would cover. The suite has been trimmed once; keep it that way. Ask "does this test need the HTTP pipeline / SigV4 / MinIO?" before reaching for `TestServer`.
- Hiding post-mutation side effects (audit log, config-sync trigger, IAM rebuild) behind helper wrappers. `with_config_db` intentionally stops at "lock the DB and run a closure" — ordering of `rebuild_iam_index` → `trigger_config_sync` → `audit_log` stays explicit at call sites because getting it wrong is how split-brain happens.

## Architecture Decisions (DO NOT CHANGE)

- **xdelta3 CLI subprocess**: The codec shells out to `xdelta3` via `std::process::Command`. This is intentional and non-negotiable. Do NOT replace with FFI bindings, Rust crates, or in-process libraries. The CLI approach ensures exact compatibility with deltas created by the original DeltaGlider Python toolchain, avoids linking C code into the binary, and keeps the codec trivially debuggable (`xdelta3` can be run standalone on any delta file). The subprocess overhead is acceptable for our workload.

- **Streaming codec (bounded memory for any object size)**: alongside the buffered `encode`/`decode` (`&[u8] → Vec<u8>`, small-object path), the codec has streaming entry points `encode_from_reader` / `decode_to_writer` (source = caller-owned seekable file, input streamed via stdin, output to a `Write` sink). Driven by `pipe_streaming` (bounded 256KiB chunks, not `read_to_end`) with a STALL-based watchdog (`ProgressClock` — kill on no-progress for `DGP_CODEC_STALL_SECS`, plus an absolute ceiling), NOT the buffered path's wall-clock timeout. Large delta GETs decode to a spool file then stream it (integrity verified BEFORE the first byte); large PUTs encode from a spool with cap-and-abort ratio decision. Memory is bounded by the pump (xdelta3 mmaps the source — measured 73MB RSS on a 2.5GB decode), never the object size. See `docs/plan/streaming-delta-any-size.md`.

  **Streaming-subprocess review checklist** (these are the failure modes an adversarial x-ray of this code found — check them on any change here): (1) STALL watchdog must tick on BOTH stdin writes and stdout reads (a sparse-output encode false-stalls otherwise); (2) spool reservations that need two files at once use `acquire_pair` (two sequential `acquire`s deadlock when 2×size > budget); (3) any path that holds the per-deltaspace prefix lock must DROP it before calling another `store_*` method that re-acquires it (re-entrant deadlock); (4) streaming output must be CAPPED at the expected size (the buffered path's decompression-bomb guard); (5) the SHA-256 integrity gate must run BEFORE the first response byte ships (decode-to-spool, not pipe-to-client); (6) every `?`/early-return must release the spool budget + codec permit (RAII `Spool`/permit drop covers this — keep it).
