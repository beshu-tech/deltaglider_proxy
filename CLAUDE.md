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
  → iam/                    IAM module:
      mod.rs                 IamState enum, auth mode detection
      types.rs               IamUser, AuthenticatedUser, Permission types
      permissions.rs         ABAC evaluation, is_admin, action matching
      middleware.rs          Per-request auth middleware
      keygen.rs              Secure key generation
  → config_db.rs            Encrypted SQLCipher database for IAM users
  → config_db_sync.rs       Multi-instance IAM sync via S3 (DGP_CONFIG_SYNC_BUCKET)
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
- `ConfigDb` (in `config_db.rs`) — encrypted SQLCipher database for IAM users, stored as `deltaglider_config.db`
- `MetadataCache` (in `metadata_cache.rs`) — 50MB moka-based in-memory cache for `FileMetadata`. Populated on PUT, HEAD, and LIST+metadata=true. Consulted on HEAD, GET, and LIST (even without metadata=true, for file_size correction). Invalidated on DELETE (exact key) and prefix delete (all matching keys). 10-minute TTL. Configurable size via `DGP_METADATA_CACHE_MB` (default: 50).
- `RateLimiter` (in `rate_limiter.rs`) — per-IP token bucket rate limiter for auth endpoints. 5 attempts per 15-minute window, 30-minute lockout after exhaustion. Expired entries cleaned up periodically.
- `UsageScanner` (in `usage_scanner.rs`) — background prefix size scanner with 5-minute cached results, 1000-entry LRU, and 100K-object scan cap per prefix.
- `S3Op` (in `storage/s3.rs`) — enum for S3 operation context in error classification
- `SessionStore` (in `session.rs`) — in-memory session store with OsRng token generation, configurable TTL (`DGP_SESSION_TTL_HOURS`, default 4h), IP binding, max 10 concurrent sessions with oldest-eviction.
- `env_parse()` / `env_parse_opt()` (in `config.rs`) — DRY helpers for environment variable parsing

**Config:** TOML file (`deltaglider_proxy.toml`) with env var overrides (`DGP_*` prefix). See `deltaglider_proxy.toml.example`.

## Authentication & IAM

The proxy **refuses to start** without authentication credentials unless `authentication = "none"` is explicitly set (dev only). Two auth modes at runtime, determined by whether IAM users exist in the config DB:

- **Bootstrap mode**: Single credential pair from TOML/env vars (`DGP_ACCESS_KEY_ID` + `DGP_SECRET_ACCESS_KEY`). Admin GUI requires the bootstrap password. This is the default on fresh installs.
- **IAM mode**: Per-user credentials from encrypted SQLCipher DB (`deltaglider_config.db`). Admin GUI access is permission-based (no password needed for IAM admins).
- **Open access** (dev only): Set `authentication = "none"` or `DGP_AUTHENTICATION=none`. No SigV4 verification.

The **bootstrap password** is a single infrastructure secret that:
1. Encrypts the SQLCipher config DB
2. Signs admin GUI session cookies
3. Gates admin GUI access in bootstrap mode (before IAM users exist)

Auto-generated on first run (printed to stderr). Reset via `--set-bootstrap-password` CLI flag (warning: invalidates encrypted IAM database).

IAM users have ABAC permissions: `{ actions: ["read", "write", "delete", "list", "admin"], resources: ["bucket/*"] }`. Admin = wildcard actions AND wildcard resources.

Key files: `src/iam/` (types, permissions, middleware, keygen), `src/config_db.rs` (SQLCipher CRUD), `src/api/admin/` (auth, users CRUD, config, groups, backup, scanner).

## Frontend (demo/s3-browser/ui)

React 18 + TypeScript + Ant Design 6 + Recharts. Path-based routing (`/_/browse`, `/_/upload`, `/_/metrics`, `/_/docs/configuration`, `/_/admin/users`). Custom `usePathRouter` hook (no react-router dependency). `NavigationContext` provides `navigate()` and `subPath` to child components. Embedded in the Rust binary via `rust-embed` and served under `/_/` on the same port as the S3 API (e.g., `http://localhost:9000/_/`). The `/_/` prefix is safe because `_` is not a valid S3 bucket name character. Single-port architecture: no separate UI port.

Key components: `MetricsPage` (Prometheus dashboard + analytics with Monitoring/Analytics tab toggle), `AnalyticsSection` (cost savings dashboard with per-bucket charts), `ObjectTable` (sortable, double-click preview, bulk selection), `BulkActionBar` (Copy/Move/ZIP/Delete for selected objects), `DestinationPickerModal` (bucket+prefix picker for copy/move), `InspectorPanel` (object details drawer with download, share duration selector, storage stats, metadata), `FilePreview` (double-click preview for text/images), `AdminPage` (full-screen settings with user management, tabs), `UsersPanel` (master-detail IAM user CRUD with ABAC permissions, effective permissions view), `UserForm`, `AuthenticationPanel` (OAuth/OIDC providers, group mapping rules), `BackendsPanel` (storage backends + compression + per-bucket policies), `SimpleSelect`/`SimpleAutoComplete` (custom dropdowns — Ant Design popups are broken in this layout), `OAuthProviderList` (shared OAuth buttons), `TabHeader` (centered tab headers), `DocsPage` (embedded markdown docs with search, Mermaid diagrams, lightbox), `DocsLanding` (landing page with screenshots and feature cards), `FullScreenHeader` (shared header for Admin/Docs with branding and theme toggle). Admin API at `/_/api/admin/*` (login, login-as, whoami, users CRUD, groups, config, auth providers, mapping rules, backup). Whoami returns user identity from session (name, access_key_id, is_admin, version). S3 operations in `s3client.ts` (includes copyObject, listAllKeys, getObjectBytes). Metrics at `/_/metrics`, stats at `/_/stats` (metadata=true for accurate delta sizes), health at `/_/health` (no version — security). Error pages respect user theme (dark/light via localStorage + CSS prefers-color-scheme). **Ant Design tooltips are globally disabled** via CSS (`display: none !important` on `.ant-tooltip, .ant-popover`) — use native `title` attributes instead.

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
