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

# Tests (MinIO required for S3 integration tests)
cargo test --all --locked
cargo test --test delta_test              # single test file
cargo test --test delta_test test_name    # single test
cargo test -- --nocapture                 # show println output
cargo test --lib                          # unit tests only, no integration

# Docker (multi-stage: UI build → Rust build → slim runtime)
docker build -t deltaglider-proxy .
```

CI runs: `fmt` → `clippy -D warnings` → `test` (with MinIO) → RustSec audit → Cargo deny → Frontend lint → claude-review. All must pass.

## Architecture

```
HTTP request
  → admission/middleware.rs  Pre-auth admission chain (deny / reject / allow-anonymous)
  → api/handlers/       S3-compatible handlers split by domain:
      object.rs            GET/PUT/HEAD/DELETE (range, conditional, Content-MD5, ACL stubs)
      object_helpers.rs    Shared helpers extracted from object.rs (headers, conditional evaluation)
      bucket.rs            Bucket CRUD and ListObjectsV2 (start-after, encoding-type, fetch-owner, base64 tokens); hosts `validate_bucket_name` + `is_ip_format` as pure validators with unit + proptest coverage
      multipart.rs         Multipart upload lifecycle
      status.rs            /health, /stats, /metrics
  → api/auth.rs         SigV4 authentication middleware (bootstrap or per-user IAM)
  → api/extractors.rs   ValidatedBucket/ValidatedPath extractors (S3 name rules, path traversal protection)
  → deltaglider/engine/   Orchestration split into submodules:
      mod.rs               Core engine: route, compress, cache, metadata resolution
      store.rs             PUT pipeline: delta encoding, migration, reference management
      retrieve.rs          GET pipeline: delta reconstruction, streaming, range requests
  → storage/traits.rs       StorageBackend trait (async_trait, object-safe)
  → storage/filesystem.rs   Local filesystem impl (xattr metadata, list_objects_delegated)
  → storage/s3.rs           AWS S3/MinIO impl (S3 user metadata headers, S3Op enum, `classify_s3_error` + `classify_get_error` pure fns with unit-test coverage)
  → demo.rs                 Embedded UI + admin API router, mounted under /_/
  → session.rs              In-memory session store (OsRng tokens, 4h default TTL)
  → admission/              Admission chain (pre-auth gating):
      spec.rs                Operator-authored YAML wire format (AdmissionBlockSpec, MatchSpec, ActionSpec)
      evaluator.rs           Decision evaluator (first-match-wins over compiled chain)
      middleware.rs          Request-info extraction, marker injection for AllowAnonymous
      mod.rs                 Runtime Match/Action/Decision types, chain builder
  → config.rs               Flat in-memory Config struct + ENV_VAR_REGISTRY
  → config_sections.rs      Sectioned YAML wire shape (admission/access/storage/advanced) + shorthand expanders
  → iam/                    IAM module:
      mod.rs                 IamState enum, auth mode detection
      types.rs               IamUser, AuthenticatedUser, Permission types
      permissions.rs         ABAC evaluation, is_admin, action matching
      middleware.rs          Per-request auth middleware
      keygen.rs              Secure key generation
  → config_db/              Encrypted SQLCipher database for IAM users
  → config_db_sync.rs       Multi-instance IAM sync via S3 (DGP_CONFIG_SYNC_BUCKET). Hosts `reopen_and_rebuild_iam` (shared by startup, periodic poller, and `POST /api/admin/config/sync-now`)
  → cli/config.rs           `config migrate|lint|schema|defaults|apply` + `admission trace`
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

**Config:** Canonical format is **YAML** (`deltaglider_proxy.yaml`) with four optional top-level sections — `admission`, `access`, `storage`, `advanced`. Legacy TOML (`deltaglider_proxy.toml`) still loads but emits a deprecation warning on every startup; `DGP_SILENCE_TOML_DEPRECATION=1` suppresses it. Convert via `deltaglider_proxy config migrate <toml> --out <yaml>`. Env var overrides (`DGP_*` prefix) apply on top of whichever file is loaded. See `deltaglider_proxy.example.yaml` (canonical) and `deltaglider_proxy.toml.example` (deprecated, kept for reference). Per-bucket policies support `public_prefixes` (and the `public: true` shorthand) for unauthenticated read-only access. Config file-search order: `DGP_CONFIG` env > `./deltaglider_proxy.yaml` > `.yml` > `.toml` > `/etc/deltaglider_proxy/config.{yaml,yml,toml}`.

## Authentication & IAM

The proxy **refuses to start** without authentication credentials unless `authentication = "none"` is explicitly set (dev only). Two auth modes at runtime, determined by whether IAM users exist in the config DB:

- **Bootstrap mode**: Single credential pair from YAML/TOML/env vars (`DGP_ACCESS_KEY_ID` + `DGP_SECRET_ACCESS_KEY`). Admin GUI requires the bootstrap password. This is the default on fresh installs.
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

Key files: `src/iam/` (types, permissions, middleware, keygen; `bump_iam_version`/`current_iam_version` + `GET /_/api/admin/iam/version` power the deterministic rebuild barrier used by tests), `src/config_db/` (SQLCipher CRUD), `src/config_db_sync.rs` (S3 sync + `reopen_and_rebuild_iam`), `src/api/admin/` (auth, users CRUD, config, groups, backup, scanner, audit; `with_config_db()` in `mod.rs` wraps the "lock DB → run closure → log-and-500" boilerplate), `src/api/admin/config/{document_level,field_level,section_level,password,trace}.rs` (section-level uses RFC 7396 merge-patch; `mod.rs` hosts `POST /api/admin/config/sync-now` — operator affordance for forcing an immediate pull from the sync bucket).

## Frontend (demo/s3-browser/ui)

React 18 + TypeScript + Ant Design 6 + Recharts. Path-based routing (`/_/browse`, `/_/upload`, `/_/metrics`, `/_/docs/configuration`, `/_/admin/users`). Custom `usePathRouter` hook (no react-router dependency). `NavigationContext` provides `navigate()` and `subPath` to child components. Embedded in the Rust binary via `rust-embed` and served under `/_/` on the same port as the S3 API (e.g., `http://localhost:9000/_/`). The `/_/` prefix is safe because `_` is not a valid S3 bucket name character. Single-port architecture: no separate UI port.

The admin UI revamp (all 10 planned waves + Wave 11 audit viewer shipped in v0.8.0; see `docs/plan/admin-ui-revamp.md`) restructures the admin settings into a 4-group IA (Diagnostics + Configuration: Admission / Access / Storage / Advanced) with hierarchical URLs (`/_/admin/configuration/access/credentials`, `/_/admin/configuration/storage/buckets`, `/_/admin/diagnostics/audit`, etc.). Legacy flat URLs (`/_/admin/users`, `/_/admin/backends`) keep working via `LEGACY_TO_NEW` in `AdminPage.tsx`. A first-run setup wizard at `/_/admin/setup` covers the zero-to-working flow (wave 8).

**Keyboard shortcuts** (waves 10 + 10.1) mounted on AdminPage: `⌘K` / `Ctrl+K` opens the `CommandPalette` (fuzzy nav over every entry in `ADMIN_IA` + shell-scope actions, recents MRU, group headings for Recent/Navigate/Actions); `⌘S` / `Ctrl+S` dispatches Apply to the currently-visible dirty section via `requestApplyCurrent()` (falls through to the browser default when no section handler is registered); `?` opens `ShortcutsHelp` (platform-aware — ⌘ on Apple / Ctrl elsewhere via `platform.ts::metaKeyLabel()`). Strict modifier match on the palette binding avoids hijacking ⌘⇧K. Listeners are gated on `authed` so the bootstrap login screen isn't affected.

**Mobile drawer** (wave 10.1 §10.4) — below 900px (`useIsNarrow(900)` in AdminPage) the persistent sidebar collapses to an AntD `Drawer` slid from the left; a hamburger in the header opens it; navigation auto-closes it.

**Audit log** (Wave 11) — `src/audit.rs` maintains an in-memory `VecDeque<AuditEntry>` ring (default 500 entries, `DGP_AUDIT_RING_SIZE`) that mirrors every `audit_log()` call. `AuditEntry` is serde-serialisable with ISO-8601 UTC timestamp. `GET /api/admin/audit?limit=N` (session-gated, not IAM-gated) powers `AuditLogPanel` at `/_/admin/diagnostics/audit`. Stdout / JSON log shippers see nothing change — the ring is supplementary.

**Trace diagnostics** (Wave 9) — `TracePanel` at `/_/admin/diagnostics/trace` calls `POST /api/admin/config/trace` and renders a Kiali-style reason path (decision tag + matched block + resolved request + example chips + Copy-as-JSON).

Key components: `MetricsPage` (Prometheus dashboard + analytics with Monitoring/Analytics tab toggle), `AnalyticsSection` (cost savings dashboard with per-bucket charts), `ObjectTable` (sortable, double-click preview, bulk selection), `BulkActionBar` (Copy/Move/ZIP/Delete for selected objects), `DestinationPickerModal` (bucket+prefix picker for copy/move), `InspectorPanel` (object details drawer with download, share duration selector, storage stats, metadata), `FilePreview` (double-click preview for text/images), `AdminPage` (full-screen settings container with hierarchical routing + keydown shortcuts + mobile drawer), `AdminSidebar` (4-group IA; amber dot for sections with unsaved edits; `ADMIN_IA` exported for the command palette), `CommandPalette` (⌘K palette with `CommandAction` + recents MRU + group headings), `ShortcutsHelp` (? modal — platform-aware key glyphs), `CopySectionYamlButton` (compact header-mounted section-scoped Copy YAML), `FormField` (label + YAML-path breadcrumb + help + default-placeholder + override-indicator + owner-badge wrapper), `ApplyDialog` (plan→diff→apply modal), `MonacoYamlEditor` (lazy-loaded Monaco + monaco-yaml with scoped JSON Schema), `IamSourceBanner` (explains DB vs YAML ownership for Access pages), `UsersPanel` (master-detail IAM user CRUD; list labels show direct-rule count AND group inheritance), `UserForm`, `AuthenticationPanel` (OAuth/OIDC providers, group mapping rules — "+ Add Rule" flushes pending edits before reload), `BackendsPanel` (storage backends), `BucketsPanel` (per-bucket policies with tri-state public read toggle: None / Specific prefixes / Entire bucket), `AdmissionPanel` (operator-authored block editor with drag-reorder + per-block form & YAML views), `GroupsPanel` (resets form + navigates to new row on successful Create), `TracePanel` (synthetic request → admission decision visualiser), `AuditLogPanel` (in-memory audit ring viewer with colour-coded Action tags + filter + 3s auto-refresh), `SetupWizard` (first-run 5-step onboarding at `/_/admin/setup`), `SimpleSelect`/`SimpleAutoComplete` (custom dropdowns — Ant Design popups are broken in this layout), `OAuthProviderList` (shared OAuth buttons), `TabHeader` (centered tab headers), `DocsPage` (embedded markdown docs with search, Mermaid diagrams, lightbox), `DocsLanding` (landing page with screenshots and feature cards), `FullScreenHeader` (shared header for Admin/Docs with branding + theme toggle + `extra` slot for hamburger + Copy/Export/Import buttons), `YamlImportExportModal` (full-document YAML round-trip). `useDirtySection` hook backs per-panel dirty state; `useApplyHandler` registers per-section Apply callbacks for ⌘S dispatch; `useDirtyGlobalIndicators` drives the `● ` tab-title prefix and beforeunload guard. `useSectionEditor` (`demo/s3-browser/ui/src/useSectionEditor.ts`) is the single home for the fetch → dirty → validate → ApplyDialog → PUT → markApplied pipeline (with `pick`/`toPayload` hooks for subset-editing Advanced sub-panels and the Admission array↔`{blocks}` shape); new section panels should plug into it rather than re-carrying ~150 LOC of boilerplate.

Admin API at `/_/api/admin/*` (login, login-as, whoami, users CRUD, groups, config, auth providers, mapping rules, backup, config/section/:name, config/export, config/apply, config/validate, config/trace, config/defaults, config/sync-now, iam/version, **audit**). `POST /api/admin/backup` restores `external_identities` (v2+, with ID remapping + fallback heuristics for legacy backups without explicit user IDs). Whoami returns user identity from session (name, access_key_id, is_admin, version). S3 operations in `s3client.ts` (includes copyObject, listAllKeys, getObjectBytes). Metrics at `/_/metrics`, stats at `/_/stats` (metadata=true for accurate delta sizes), health at `/_/health` (no version — security). Error pages respect user theme (dark/light via localStorage + CSS prefers-color-scheme). **Ant Design tooltips are globally disabled** via CSS (`display: none !important` on `.ant-tooltip, .ant-popover`) — use native `title` attributes instead. The AntD 6 radio/checkbox "shrink on click" default is disabled in `theme.css`.

## Testing

Tests in `tests/` use a `TestServer` harness (`tests/common/mod.rs`) that spawns a real proxy instance with a temp directory (filesystem backend) or MinIO (S3 backend). Port allocation uses an atomic counter starting at 19000. `wait_ready` checks `process.try_wait()` BEFORE the health probe so a stray proxy holding the test port fails loudly (`lsof -i :<port>`) instead of silently intercepting requests.

S3 integration tests require MinIO running on localhost:9000. CI starts MinIO automatically; locally, use `docker run -p 9000:9000 minio/minio server /data`.

Async rebuild barrier: after any IAM mutation, use `get_iam_version(&http, &endpoint).await` BEFORE the call and `wait_for_iam_rebuild(&http, &endpoint, before_version).await` AFTER. Backed by a monotonic `AtomicU64` exposed at `GET /_/api/admin/iam/version` — replaces blind `sleep(1s)` with a ~50ms deterministic poll. HA sync is exercised via `POST /api/admin/config/sync-now` in `tests/config_sync_ha_test.rs`.

Property tests via `proptest` (dev-dep) live in the same module as the pure functions they exercise (see `src/api/handlers/bucket.rs` for `validate_bucket_name` / `is_ip_format`). Coverage is collected via `cargo-llvm-cov` as a non-blocking CI job — use it as signal, not a gate.

## Conventions

- Clippy warnings are errors in CI (`-D warnings`)
- The proxy is transparent: clients must not know delta compression is happening
- `x-amz-storage-type` response header exposes storage strategy (delta/passthrough/reference) for debugging
- Delta-eligible file types are defined in `deltaglider/file_router.rs`
- Passthrough files (images, video) skip delta entirely — already compressed
- Streaming is preferred for large files; delta reconstruction requires buffering the reference

## Testability principles (write code that's testable from day one)

- **Pure functions at decision points.** When a handler is deciding "is this request valid / authorized / retryable", extract the decision into a `fn(typed_input) -> typed_output` and have the handler do I/O around it. Prior art: `classify_s3_error` / `classify_get_error` in `src/storage/s3.rs`, `validate_bucket_name` / `is_ip_format` in `src/api/handlers/bucket.rs`. Both are unit-tested against the full truth table (and proptested in the bucket case) without spinning up a server.
- **Expose observable counters for async state transitions.** The `IAM_VERSION` `AtomicU64` in `src/iam/mod.rs` + `GET /_/api/admin/iam/version` turned a 1s sleep into a 50ms deterministic barrier (`wait_for_iam_rebuild` in `tests/common/mod.rs`). Future async state changes (config reloads, external-auth discovery, scanner refreshes) should follow the same pattern: a monotonic counter bumped after the new state is published, polled by whoever needs to wait for it.
- **Don't put test-relevant helpers in binary-only modules.** `reopen_and_rebuild_iam` had to move from `src/startup.rs` (binary) to `src/config_db_sync.rs` (library) before the admin `sync-now` handler could reach it. Rule: if a helper might be called from more than one trigger (startup, periodic task, admin endpoint, test), put it in the library side from day one — `src/startup.rs` is the last mile, not a grab bag.
- **Test the observable contract, not the implementation.** Integration tests cost a TestServer spawn (~200ms + MinIO when S3). Unit-test anything that doesn't need the whole HTTP/SigV4/storage stack; reserve integration tests for request-pipeline seams, concurrency races, and cross-subsystem invariants (metadata cache vs storage, admission vs auth, etc.). The hygiene pass in `5f38941` deleted 659 LOC of integration tests that were re-covering pure-function logic.
- **Property tests for pure validators.** Anywhere there's an "is this valid / allowed / well-formed" check, proptest it alongside hand-picked cases. Saves enumerating 20 shapes and catches the weird input nobody thought of. Keep the enumerated cases too — they document the specific invariants a reader should see at a glance. Stress with `PROPTEST_CASES=2000` locally before a big change.
- **Watch for cross-test contamination.** Each test spawns its own `TestServer` on a unique port, but shared disk artifacts (config DBs in CWD) and stray processes holding ports have bitten us. `wait_ready` now fails loudly on port collision — preserve that safety net when touching `tests/common/mod.rs`. Env-var-mutating tests need serialisation (see the `static LOCK` pattern in `src/rate_limiter.rs` tests).
- **Use `cargo-llvm-cov` output to steer.** Coverage numbers are signal, not a gate. If a new hot-path file lands at 0 line coverage that's a flag; don't let it merge without explaining why (e.g. MinIO-only branches intentionally excluded from the unit run).

## When proposing architecture

Anti-patterns we've fought recently — don't reintroduce them:
- Duplicating code across "this is DRY but awkward" boundaries. See the deleted `default_*` duplicates in `config_sections.rs` (`0742780`) — the `config.rs` defaults are now `pub(crate)` and reused. If a shape is duplicated in two places "because they serve different serdes," question it.
- Creating a new admin endpoint for test convenience. `POST /api/admin/config/sync-now` exists as an OPERATOR affordance (force an immediate sync after a known-good change on another instance). It happens to enable HA tests — but that's the bar: endpoints ship only when there's a real production use case. Don't add test-only routes.
- Writing integration tests for logic a unit test would cover. The suite has been trimmed once; keep it that way. Ask "does this test need the HTTP pipeline / SigV4 / MinIO?" before reaching for `TestServer`.
- Hiding post-mutation side effects (audit log, config-sync trigger, IAM rebuild) behind helper wrappers. `with_config_db` intentionally stops at "lock the DB and run a closure" — ordering of `rebuild_iam_index` → `trigger_config_sync` → `audit_log` stays explicit at call sites because getting it wrong is how split-brain happens.

## Architecture Decisions (DO NOT CHANGE)

- **xdelta3 CLI subprocess**: The codec shells out to `xdelta3` via `std::process::Command`. This is intentional and non-negotiable. Do NOT replace with FFI bindings, Rust crates, or in-process libraries. The CLI approach ensures exact compatibility with deltas created by the original DeltaGlider Python toolchain, avoids linking C code into the binary, and keeps the codec trivially debuggable (`xdelta3` can be run standalone on any delta file). The subprocess overhead is acceptable for our workload.
