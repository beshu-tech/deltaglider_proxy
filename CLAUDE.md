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
      bucket.rs            Bucket CRUD and ListObjectsV2 (start-after, encoding-type, fetch-owner, base64 tokens)
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
  → storage/s3.rs           AWS S3/MinIO impl (S3 user metadata headers, S3Op enum)
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
  → config_db_sync.rs       Multi-instance IAM sync via S3 (DGP_CONFIG_SYNC_BUCKET)
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
- `IamMode` (in `config_sections.rs`) — `Gui` (default) = encrypted IAM DB is source of truth; `Declarative` = YAML owns IAM state and admin-API IAM mutations return 403. Phase 3c.3 reconciler (sync-diff DB ↔ YAML on apply) is still pending — declarative mode is currently a lockout.
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
- `declarative` — YAML is authoritative. Admin API IAM mutation routes (`POST/PUT/PATCH/DELETE` on `/users`, `/groups`, `/ext-auth/*`, `/migrate`, backup import) return `403 { "error": "iam_declarative" }`. Read endpoints stay accessible for diagnostics. The reconciler that sync-diffs DB to YAML is Phase 3c.3 (pending) — declarative mode today is a pure lockout; seed IAM via a one-time GUI session, then flip. Mode transitions are audit-logged (warn-level).

The **bootstrap password** is a single infrastructure secret that:
1. Encrypts the SQLCipher config DB
2. Signs admin GUI session cookies
3. Gates admin GUI access in bootstrap mode (before IAM users exist)

Auto-generated on first run (printed to stderr when stderr is a TTY; hidden in containers/CI — only the bcrypt hash is logged). Reset via `--set-bootstrap-password` CLI flag (warning: invalidates encrypted IAM database).

IAM users have ABAC permissions: `{ actions: ["read", "write", "delete", "list", "admin"], resources: ["bucket/*"] }`. Admin = wildcard actions AND wildcard resources. The IAM DB is independent of the YAML config file — `access: {}` in YAML with no legacy creds is correct when users/groups/OAuth providers live in the DB. Multi-instance sync via S3 (`DGP_CONFIG_SYNC_BUCKET` / `config_sync_bucket`) uploads the encrypted DB after every mutation; readers poll S3 every 5 minutes and download on ETag change.

Key files: `src/iam/` (types, permissions, middleware, keygen), `src/config_db/` (SQLCipher CRUD), `src/config_db_sync.rs` (S3 sync), `src/api/admin/` (auth, users CRUD, config, groups, backup, scanner), `src/api/admin/config/section_level.rs` (section-level admin API with RFC 7396 merge-patch semantics).

## Frontend (demo/s3-browser/ui)

React 18 + TypeScript + Ant Design 6 + Recharts. Path-based routing (`/_/browse`, `/_/upload`, `/_/metrics`, `/_/docs/configuration`, `/_/admin/users`). Custom `usePathRouter` hook (no react-router dependency). `NavigationContext` provides `navigate()` and `subPath` to child components. Embedded in the Rust binary via `rust-embed` and served under `/_/` on the same port as the S3 API (e.g., `http://localhost:9000/_/`). The `/_/` prefix is safe because `_` is not a valid S3 bucket name character. Single-port architecture: no separate UI port.

The admin UI revamp (all 10 planned waves + Wave 11 audit viewer shipped in v0.8.0; see `docs/plan/admin-ui-revamp.md`) restructures the admin settings into a 4-group IA (Diagnostics + Configuration: Admission / Access / Storage / Advanced) with hierarchical URLs (`/_/admin/configuration/access/credentials`, `/_/admin/configuration/storage/buckets`, `/_/admin/diagnostics/audit`, etc.). Legacy flat URLs (`/_/admin/users`, `/_/admin/backends`) keep working via `LEGACY_TO_NEW` in `AdminPage.tsx`. A first-run setup wizard at `/_/admin/setup` covers the zero-to-working flow (wave 8).

**Keyboard shortcuts** (waves 10 + 10.1) mounted on AdminPage: `⌘K` / `Ctrl+K` opens the `CommandPalette` (fuzzy nav over every entry in `ADMIN_IA` + shell-scope actions, recents MRU, group headings for Recent/Navigate/Actions); `⌘S` / `Ctrl+S` dispatches Apply to the currently-visible dirty section via `requestApplyCurrent()` (falls through to the browser default when no section handler is registered); `?` opens `ShortcutsHelp` (platform-aware — ⌘ on Apple / Ctrl elsewhere via `platform.ts::metaKeyLabel()`). Strict modifier match on the palette binding avoids hijacking ⌘⇧K. Listeners are gated on `authed` so the bootstrap login screen isn't affected.

**Mobile drawer** (wave 10.1 §10.4) — below 900px (`useIsNarrow(900)` in AdminPage) the persistent sidebar collapses to an AntD `Drawer` slid from the left; a hamburger in the header opens it; navigation auto-closes it. **i18n scaffold** (`src/i18n.ts`) exposes `t(key, fallback)` + `useT()` as a pass-through today — single swap point when a locale ships.

**Audit log** (Wave 11) — `src/audit.rs` maintains an in-memory `VecDeque<AuditEntry>` ring (default 500 entries, `DGP_AUDIT_RING_SIZE`) that mirrors every `audit_log()` call. `AuditEntry` is serde-serialisable with ISO-8601 UTC timestamp. `GET /api/admin/audit?limit=N` (session-gated, not IAM-gated) powers `AuditLogPanel` at `/_/admin/diagnostics/audit`. Stdout / JSON log shippers see nothing change — the ring is supplementary.

**Trace diagnostics** (Wave 9) — `TracePanel` at `/_/admin/diagnostics/trace` calls `POST /api/admin/config/trace` and renders a Kiali-style reason path (decision tag + matched block + resolved request + example chips + Copy-as-JSON).

Key components: `MetricsPage` (Prometheus dashboard + analytics with Monitoring/Analytics tab toggle), `AnalyticsSection` (cost savings dashboard with per-bucket charts), `ObjectTable` (sortable, double-click preview, bulk selection), `BulkActionBar` (Copy/Move/ZIP/Delete for selected objects), `DestinationPickerModal` (bucket+prefix picker for copy/move), `InspectorPanel` (object details drawer with download, share duration selector, storage stats, metadata), `FilePreview` (double-click preview for text/images), `AdminPage` (full-screen settings container with hierarchical routing + keydown shortcuts + mobile drawer), `AdminSidebar` (4-group IA; amber dot for sections with unsaved edits; `ADMIN_IA` exported for the command palette), `CommandPalette` (⌘K palette with `CommandAction` + recents MRU + group headings), `ShortcutsHelp` (? modal — platform-aware key glyphs), `CopySectionYamlButton` (compact header-mounted section-scoped Copy YAML), `FormField` (label + YAML-path breadcrumb + help + default-placeholder + override-indicator + owner-badge wrapper), `ApplyDialog` (plan→diff→apply modal), `MonacoYamlEditor` (lazy-loaded Monaco + monaco-yaml with scoped JSON Schema), `IamSourceBanner` (explains DB vs YAML ownership for Access pages), `UsersPanel` (master-detail IAM user CRUD; list labels show direct-rule count AND group inheritance), `UserForm`, `AuthenticationPanel` (OAuth/OIDC providers, group mapping rules — "+ Add Rule" flushes pending edits before reload), `BackendsPanel` (storage backends), `BucketsPanel` (per-bucket policies with tri-state public read toggle: None / Specific prefixes / Entire bucket), `AdmissionPanel` (operator-authored block editor with drag-reorder + per-block form & YAML views), `GroupsPanel` (resets form + navigates to new row on successful Create), `TracePanel` (synthetic request → admission decision visualiser), `AuditLogPanel` (in-memory audit ring viewer with colour-coded Action tags + filter + 3s auto-refresh), `SetupWizard` (first-run 5-step onboarding at `/_/admin/setup`), `SimpleSelect`/`SimpleAutoComplete` (custom dropdowns — Ant Design popups are broken in this layout), `OAuthProviderList` (shared OAuth buttons), `TabHeader` (centered tab headers), `DocsPage` (embedded markdown docs with search, Mermaid diagrams, lightbox), `DocsLanding` (landing page with screenshots and feature cards), `FullScreenHeader` (shared header for Admin/Docs with branding + theme toggle + `extra` slot for hamburger + Copy/Export/Import buttons), `YamlImportExportModal` (full-document YAML round-trip). `useDirtySection` hook backs per-panel dirty state; `useApplyHandler` registers per-section Apply callbacks for ⌘S dispatch; `useDirtyGlobalIndicators` drives the `● ` tab-title prefix and beforeunload guard.

Admin API at `/_/api/admin/*` (login, login-as, whoami, users CRUD, groups, config, auth providers, mapping rules, backup, config/section/:name, config/export, config/apply, config/validate, config/trace, config/defaults, **audit**). `POST /api/admin/backup` restores `external_identities` (v2+, with ID remapping + fallback heuristics for legacy backups without explicit user IDs). Whoami returns user identity from session (name, access_key_id, is_admin, version). S3 operations in `s3client.ts` (includes copyObject, listAllKeys, getObjectBytes). Metrics at `/_/metrics`, stats at `/_/stats` (metadata=true for accurate delta sizes), health at `/_/health` (no version — security). Error pages respect user theme (dark/light via localStorage + CSS prefers-color-scheme). **Ant Design tooltips are globally disabled** via CSS (`display: none !important` on `.ant-tooltip, .ant-popover`) — use native `title` attributes instead. The AntD 6 radio/checkbox "shrink on click" default is disabled in `theme.css`.

## Testing

Tests in `tests/` use a `TestServer` harness (`tests/common/mod.rs`) that spawns a real proxy instance with a temp directory (filesystem backend) or MinIO (S3 backend). Port allocation uses an atomic counter starting at 19000.

S3 integration tests require MinIO running on localhost:9000. CI starts MinIO automatically; locally, use `docker run -p 9000:9000 minio/minio server /data`.

## Conventions

- Clippy warnings are errors in CI (`-D warnings`)
- The proxy is transparent: clients must not know delta compression is happening
- `x-amz-storage-type` response header exposes storage strategy (delta/passthrough/reference) for debugging
- Delta-eligible file types are defined in `deltaglider/file_router.rs`
- Passthrough files (images, video) skip delta entirely — already compressed
- Streaming is preferred for large files; delta reconstruction requires buffering the reference

## Architecture Decisions (DO NOT CHANGE)

- **xdelta3 CLI subprocess**: The codec shells out to `xdelta3` via `std::process::Command`. This is intentional and non-negotiable. Do NOT replace with FFI bindings, Rust crates, or in-process libraries. The CLI approach ensures exact compatibility with deltas created by the original DeltaGlider Python toolchain, avoids linking C code into the binary, and keeps the codec trivially debuggable (`xdelta3` can be run standalone on any delta file). The subprocess overhead is acceptable for our workload.
