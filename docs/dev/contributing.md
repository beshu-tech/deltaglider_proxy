# Contributing

*Build, test, project structure*

Thanks for your interest in contributing! Whether it's a bug report, feature idea, or code change, we appreciate your help.

## Getting Started

### Prerequisites

- **Rust stable** — install via [rustup](https://rustup.rs/) (the repo's `rust-toolchain.toml` pins the channel automatically)
- **Node.js 20+** — needed to build the embedded demo UI
- **Docker** — optional, used for running MinIO in integration tests

### Building from Source

```bash
# 1. Clone the repo
git clone https://github.com/beshu-tech/deltaglider_proxy.git
cd deltaglider_proxy

# 2. Build the demo UI (rust-embed bakes it into the binary)
cd demo/s3-browser/ui && npm install && npm run build && cd -

# 3. Build the proxy
cargo build

# 4. Run it
DGP_DATA_DIR=./data cargo run
```

The S3 API and demo UI both start on `http://localhost:9000`. The UI is available at `http://localhost:9000/_/`.

### Running Tests

```bash
# Unit tests (no MinIO)
cargo test --lib --locked

# One integration binary (many need MinIO on localhost:9000 — see tests/common/mod.rs)
cargo test --locked --test s3_integration_test

# Full matrix (run before a release or after changing shared test harness / CI lists)
cargo test --all --locked
```

PR CI does **not** run `cargo test --all` (wall-clock); it runs `cargo test --lib` plus **explicit** integration binaries listed in `.github/workflows/ci.yml`. Every `tests/<name>.rs` must appear there — `./scripts/check-integration-tests-in-ci.sh` enforces it. A **nightly** workflow (`test-all-nightly.yml`) runs `cargo test --all` with MinIO.

### Code Quality Checks

The merge gate matches `.github/workflows/ci.yml` — run these before submitting a PR:

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --lib --locked
./scripts/check-integration-tests-in-ci.sh
cd demo/s3-browser/ui && npm ci && npm run build && npm run lint && npm run typecheck && npm run knip \
  && npm run test:permissions && npm run test:storage-path
# Optional local parity with CI integration batches (needs MinIO):
cargo test --locked --test s3_integration_test
```

Embedded UI smoke (Playwright — same as `e2e-smoke` CI job): from repo root, `cargo build --release --bin deltaglider_proxy` with UI already built, then `cd demo/s3-browser/ui && npx playwright install chromium && cd ../../../.. && ./scripts/e2e-smoke.sh`.

## Project Structure

The S3 protocol surface (GET/PUT/HEAD/DELETE, ListObjectsV2, copy, multipart) is
served by the **s3s** framework — `s3_adapter_s3s.rs` implements `s3s::S3` (~32
verb methods) and is
mounted as the axum `fallback_service` in `startup.rs`. The hand-rolled axum S3
handlers were retired; what survives in `api/handlers/` is just shared state +
the helpers s3s can't model (browser form-POST, health/stats). The admin API and
demo UI stay axum, under `/_/`.

```
src/
├── s3_adapter_s3s.rs    # THE S3 implementation: impls s3s::S3 (~32 verb methods); delegates product logic to the engine
├── startup.rs           # Server startup: builds the s3s service + axum router, middleware stack (admission → auth → interceptors → fallback)
├── api/
│   ├── mod.rs           # API module root, re-exports S3Error
│   ├── handlers/        # Post-s3s survivors: mod.rs (AppState + helpers), form_post.rs (browser PostObject), object_helpers.rs (quota gate + outbox enqueue), status.rs (/_/health, /_/stats)
│   ├── auth.rs          # SigV4 + IAM authorization middleware + public prefix bypass
│   ├── admin/           # Admin API (login, config, users, groups, external_auth, backup, replication, lifecycle, event_outbox, scanner, audit, …)
│   ├── aws_chunked.rs   # AWS chunked transfer encoding decoder
│   └── errors.rs        # S3 error responses
├── deltaglider/
│   ├── engine/          # Core engine (mod, store, retrieve submodules)
│   ├── codec.rs         # xdelta3 encode/decode (subprocess)
│   ├── cache.rs         # Reference file LRU cache (moka)
│   ├── savings.rs       # Delta-savings accounting
│   └── file_router.rs   # File type routing (delta-eligible vs passthrough)
├── storage/
│   ├── traits.rs        # StorageBackend trait (async_trait, object-safe)
│   ├── filesystem.rs    # Local filesystem backend (xattr metadata)
│   ├── s3.rs            # S3 backend (AWS SDK; classify_s3_error / classify_get_error pure fns)
│   ├── encrypting.rs    # At-rest encryption wrapper backend (per-backend AES key)
│   ├── routing.rs       # Multi-backend routing (virtual bucket → real backend)
│   └── xattr_meta.rs    # Extended attribute helpers
├── iam/
│   ├── mod.rs           # IamState enum, IamIndex, hot-swap, IAM_VERSION counter
│   ├── types.rs         # IamUser, Permission, AuthenticatedUser, S3Action
│   ├── permissions.rs   # ABAC evaluation (legacy + iam-rs with conditions)
│   ├── middleware.rs    # Per-request IAM authorization middleware
│   ├── keygen.rs        # Secure access key generation
│   ├── declarative.rs   # Declarative-mode reconciler (diff_iam → apply_iam_reconcile)
│   └── external_auth/   # OAuth/OIDC providers (Google, generic OIDC)
├── config_db/           # Encrypted SQLCipher DB (users, groups, auth_providers, declarative; classify_sqlite_error)
├── config.rs            # Flat Config struct + ENV_VAR_REGISTRY + env_parse/env_bool helpers + classify_auth_config (YAML canonical; TOML deprecated)
├── config_sections.rs   # Sectioned YAML wire shape (admission/access/storage/advanced) + shorthand expanders
├── config_db_sync.rs    # Multi-instance IAM sync via S3 (reopen_and_rebuild_iam)
├── admission/           # Pre-auth admission chain (operator-authored + synthesized blocks)
├── replication/         # Event-driven + scheduled bucket replication (planner, worker, scheduler, event_consumer, state_store)
├── lifecycle/           # Delete-only object lifecycle rules (planner, scheduler, worker, state_store)
├── transfer.rs          # Shared engine-routed copy primitive (used by replication + lifecycle)
├── event_outbox.rs      # Durable object-event outbox (append on mutation)
├── event_delivery.rs    # Background dispatcher → webhook JSON or Slack message
├── slack_format.rs      # Pure Slack Block Kit formatter + notification filter
├── security.rs          # Pure security primitives (validate_bucket_name, bucket_name_is_ip_like, outbound-URL SSRF policy)
├── secret.rs            # Secret trait (opaque material + non-secret id) — future KMS/Vault home
├── tls.rs               # TLS setup (user PEM or self-signed via rcgen)
├── background.rs        # Shared background-runner helpers (parse_duration_or)
├── init.rs              # Interactive --init config wizard
├── bucket_policy.rs     # Per-bucket policies + PublicPrefixSnapshot
├── session.rs           # Admin session store (OsRng tokens, IP binding)
├── rate_limiter.rs      # Per-IP rate limiting (token bucket)
├── metadata_cache.rs    # Object metadata LRU cache (moka)
├── usage_scanner.rs     # Background prefix size scanner (cached)
├── audit.rs             # Audit logging + in-memory AuditEntry ring
├── multipart.rs         # In-memory multipart upload state
├── types.rs             # Core types (FileMetadata, StorageInfo, etc.)
├── demo.rs              # Embedded UI (rust-embed) + admin API router
├── cli/                 # CLI subcommands (cp, sync, migrate, ls, rm, purge, verify, config, …)
├── lib.rs               # Library root
└── main.rs              # Entry point
demo/s3-browser/ui/      # React 18 + TypeScript + Ant Design 6 admin GUI
tests/                   # Integration tests (S3 ops, auth, IAM, public prefixes)
docs/                    # Documentation
```

### Key Concepts

- **DeltaSpace**: A group of objects under the same directory prefix that share a single baseline for delta compression. For example, all objects under `releases/` form one deltaspace.
- **Reference file**: The internal baseline stored once per deltaspace. All deltas are computed against it (no chaining), so reconstruction is always O(1).
- **StorageBackend**: A trait abstracting where bytes live — local filesystem or upstream S3. Adding a new backend means implementing this trait.
- **File router**: Decides whether a file is delta-eligible based on its extension (`.zip`, `.jar`, `.tar`, etc.) or should be stored as passthrough (`.jpg`, `.mp4`, etc.).

## Admin-UI patterns (frontend)

The admin GUI (`demo/s3-browser/ui`) has converged on a small set of canonical
patterns. New panels and edits should follow these rather than re-inventing —
divergence here has historically produced a recurring "admin-editor bug class"
(stale closures, array-index keys, double sources of truth).

**Two data families, two pipelines.**
- **Config sections** (`admission` / `access` / `storage` / `advanced`) → the
  **`useSectionEditor`** hook. It owns fetch → dirty-tracking → validate →
  `ApplyDialog` → section-PUT → re-fetch, with `pick` (wire→form) / `toPayload`
  (form→wire) hooks. The editor `value` IS the single source of truth — never
  keep a parallel state mirror of it. Examples: Admission, Credentials, all
  Advanced sub-panels, Webhook delivery, Replication, Lifecycle, Buckets.
- **IAM DB resources** (users / groups / OAuth providers / mapping rules) →
  **react-query** (`queries/`, keyed by `qk.*`) for reads + per-record mutations.
  Read the cached admin config with `useAdminConfig()` — do NOT hand-roll
  `useEffect(getAdminConfig().then(setState))`. After a config mutation,
  invalidate `qk.config()` (the section editor already does this on apply).

**Single source of truth for forms.** A form's React state should be the only
copy of its editable data. For master-detail forms (User/Group/etc.), initialize
state from the selected record with **lazy `useState(() => ...)` initializers +
a `key` on the form** (`key={record.id}` for edit, `key="new"` for create) so a
keyed remount resets state — never a `useEffect([record])` prop→state mirror.

**Stable row ids, never array index.** Row-list editors (endpoints, headers,
glob rows, routes, permissions, rules) key React lists by a stable id (a
per-instance `nextId()` counter or the record's own id), and mutate rows by id
(`rows.map(r => r.id === id ? {...} : r)`), never by index.

**Pure helpers + Node regression tests.** Validation, payload-building, and
normalization live in pure functions (e.g. `webhookDeliveryPayload.ts`'s
`formFromWire` / `buildPayloadFromForm`), not inline in components, and get a
`scripts/*-regression-test.mjs` Node test (registered in `package.json` + CI).
Mirrors the Rust convention of pure decision-fns at seams (`classify_*`,
`validate_*`, `resolve_*`) with colocated unit tests.

**Secret round-trip.** Secret fields (webhook headers, Slack bot token, SigV4)
are masked to the `REDACTED_SENTINEL` (`__redacted__`) on GET; the UI shows a
"•••• (unchanged — type to replace)" placeholder and, on save, passes the
sentinel through untouched so the backend preserves the real value — on BOTH the
section-PUT and document export→apply paths. Removed map entries emit an explicit
`null` (RFC 7396 delete).

**Shared visual primitives** (reuse, don't re-style): `useCardStyles` /
`SectionHeader` (cards — owns its own header gap), `FormField` (label +
YAML-path breadcrumb + helpText + example chips — wrap every field; help should
always be present; the yaml-path chip shows on hover/focus only so the bold label
leads), `StickyDirtyBar` (the slim floating unsaved-changes bar — use `floating`
when it can't be the last child in the scroll flow), `ApplyDialog` (plan → diff →
apply). Every editable field should have a `helpText` and a `placeholder`/example.

The PR #24 convergence pass added three more (use these instead of re-rolling):
- **`MaskedSecretInput`** — the one masked-secret field. `mode="sentinel"`
  (shows empty while the value is the `REDACTED_SENTINEL`, passes it through
  untouched on save) for webhook headers / Slack bot token; `mode="blank-keeps"`
  (blank = keep existing, non-blank = rotate) for SigV4-style secrets.
- **`RowListEditor<T>`** — stable-`id`-keyed add/remove/update-by-id list
  scaffolding (the "stable row ids, never array index" rule, centralised). You
  supply `renderRow` + `newItem()`; it hands each row `update`/`remove`-by-id and
  emits the next array for you to fold into your single source of truth.
- **`StatePlaceholders`** — `LoadingState` / `EmptyState` chrome (consistent
  centering, padding, icon size, type scale) so panels stop hand-rolling spinners.

**IAM DB reads go through react-query**, not `useEffect(load)`. The
`queries/{groups,authProviders,mappingRules,users,backends,…}.ts` hooks (keyed by
`qk.*`) own reads + per-record mutations and invalidate `qk.*` on success — the
old `loadData()`-after-every-mutation + prop→state-mirror idiom is gone.

## Submitting Changes

1. Fork the repo and create a branch from `main`
2. Make your changes
3. Run `cargo fmt`, `cargo clippy`, and `cargo test`
4. Open a pull request with a clear description of what and why

## Reporting Issues

Open an issue on GitHub. If it's a bug, include:

- What you expected vs. what happened
- Steps to reproduce
- DeltaGlider Proxy version (`deltaglider_proxy --version`)
- Backend type (filesystem or S3)

## License and Contributor Agreement

### Code is GPL-3.0

DeltaGlider Proxy is licensed under [GPL-3.0-only](../../LICENSE).
Every Rust source file must start with the SPDX header:

```rust
// SPDX-License-Identifier: GPL-3.0-only
```

CI fails if a `.rs` file is missing this header. Run
`./scripts/check-spdx-headers.sh` locally before pushing.

### CLA required for all contributions

To contribute, you must sign the
[Contributor License Agreement](../../CLA.md). By signing, you
**assign copyright** in your contribution to **Beshu Limited**. This
lets us dual-license the project (GPL-3.0 + commercial) — the same
model used by ReadonlyREST and many other open-core products.

**How to sign**: when you open your first pull request, a CLA
Assistant bot will comment with a link and signing instructions. You
sign once; future PRs from the same GitHub account are automatically
accepted.

If the bot has trouble or you need to sign by other means, email a
signed copy of the CLA to `contact@beshu.tech` with subject:
`DeltaGlider Proxy — CLA signed by [Your Name]`.
