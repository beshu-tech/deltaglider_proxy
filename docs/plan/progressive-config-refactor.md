# Progressive Config Refactor — Implementation Plan

> **📍 Progress snapshot (2026-04-20)**
>
> This document is the original planning artifact. Per-phase `**STATUS**`
> callouts below record what's actually shipped vs. planned, as of the
> commits on `main` up to `e9d23b2` ("Configuration overhaul: YAML +
> document-level admin API + admission chain").
>
> | Phase | Status | What's next |
> |---|---|---|
> | 0 — Foundations                    | ✅ **Done**        | — |
> | 1 — Admin API upgrades             | ✅ **Done**        | `?section=` / `?resolve=` query params on `/export` deferred to Phase 3 |
> | 2 — Admission chain                | 🟡 **Partial**     | Scope narrowed: only `AllowAnonymous` / `Continue` + `PublicPrefixGrant` shipped; `Deny` / `RateLimit` / `Reject` variants and the 5-block default chain are carried into Phase 3 |
> | 3 — Progressive-disclosure schema  | ❌ **Not started** | **The critical next target** — unblocks Phases 4, 5, 6 |
> | 4 — CLI                            | 🟡 **20% done**    | Only `config migrate` + `config schema` shipped; `init` / `lint` / `show` / `defaults` / `explain` / `apply` / `admission trace` pending |
> | 5 — GUI redesign                   | ❌ **Not started** | Blocked on Phase 3 |
> | 6 — Deprecation sweep              | ❌ **Not started** | Blocked on Phase 3/4/5 |
>
> **Bonus work that shipped alongside** (not in the original plan but
> landed in the same `e9d23b2` commit): a hygiene pass (admin/config.rs
> split into submodules, `apply_config_transition` helper,
> `RateLimitGuard`), seven targeted correctness fixes from adversarial
> audits (NaN/Infinity clamp, IPv4-mapped-IPv6 normalisation, atomic
> persist, duplicate-backend detection, log_level pre-validation, etc.),
> and `examples/scrape_full_config.rs` — a read-only tool that dumps a
> live server's config into the Phase 3 target YAML shape + a companion
> `.env`. See "Bonus: unplanned work that shipped" at the bottom.

## Context

DeltaGlider Proxy has accumulated ~35 configurable features across global / backend / bucket scopes with inconsistent override semantics, all in a flat TOML file plus a SQLCipher-backed IAM DB. First-time users face a wall of knobs; GitOps users have no declarative end-to-end story; GUI users can't easily export their working setup for version control. Prior design discussion converged on:

- **Five semantic layers** (admission, identity, IAM, parameters, routing) each with its own composition rule — don't conflate them.
- **Progressive-disclosure YAML** with four top-level sections (`admission`, `access`, `storage`, `advanced`) that degrades to ~5 lines for solo users and scales to ~150 for enterprise.
- **Convention over configuration** — defaults never appear in files; `dgpctl defaults` discloses them on demand.
- **Shorthand forms** that auto-expand (single backend inferred, `public: true` synthesizes admission blocks, group presets replace hand-written IAM JSON for common cases).
- **Two personas first-class**: the GUI admin (clicks, iterates, hits "Copy as YAML" when ready) and the GitOps user (edits YAML, runs `dgpctl lint` in CI, Flux/Argo applies).
- **Admission chain** as a new first-class feature (RRR-style ordered blocks) layered before identity/IAM, not replacing them.

Outcome the plan targets: a first-time user can paste 5 lines and run; an enterprise admin can declare the Acme kitchen-sink config from prior discussion; both can round-trip between GUI and GitOps without loss.

User decisions locked in via AskUserQuestion:
- **IAM**: per-deployment `iam_mode: gui | declarative` toggle. GUI mode = DB is source of truth, YAML `access:` seeds on empty DB. Declarative mode = YAML is source of truth, GUI IAM CRUD returns 403.
- **CLI packaging**: subcommands on the existing `deltaglider_proxy` binary (`deltaglider_proxy config init`, `deltaglider_proxy admission trace`, etc.). No separate `dgpctl` binary.

---

## Phases (ship-independent, each mergeable on its own)

### Phase 0 — Foundations (1–2 weeks)

> **✅ STATUS: Done**
>
> All acceptance criteria met:
> - `serde_yaml` + `schemars` in `Cargo.toml`; `JsonSchema` derives on `Config`, `BackendConfig`, `NamedBackendConfig`, `TlsConfig`, `BucketPolicyConfig`, `DefaultsVersion`.
> - `Config::from_file` dispatches on extension (`src/config.rs::ConfigFormat::from_path`). `.yaml`/`.yml` → YAML, else → TOML.
> - `Config::to_canonical_yaml()` emits serde-stable shape via `redact_infra_secrets().to_string()`.
> - `persist_to_file()` is atomic (write-to-tempfile + fsync + rename) — this exceeded the plan's scope and closed a pre-existing "crash during config save corrupts file" hole.
> - `DefaultsVersion::V1` enum pinned via `#[serde(rename = "defaults", skip_serializing_if = "DefaultsVersion::is_default")]`.
> - `deltaglider_proxy config migrate <in> [--out <path>]` shipped in `src/cli/config.rs`.
> - CI `config-schema` job (`.github/workflows/ci.yml`) generates `schema/deltaglider.schema.json`, round-trips `deltaglider_proxy.toml.example` through migrate twice to prove idempotence, and uploads the schema as a workflow artifact.
>
> **Scope divergence**: added a bonus `deltaglider_proxy config schema [--out <path>]` subcommand (writes the JSON Schema to disk) — useful for CI and YAML-LSP integrations; not in the original plan.

**Goal:** YAML support exists in parallel with TOML. Nothing user-facing changes yet; internal mechanics are in place.

**Scope**
- Add `serde_yaml` (or `serde_yml`) to `Cargo.toml`.
- Add `schemars` crate and derive `JsonSchema` on every public config struct in `src/config.rs` and `src/bucket_policy.rs`.
- Implement `Config::from_yaml_file(path)` alongside existing `from_file` in `src/config.rs:527` (TOML). Dispatcher in `load()` picks by extension (`.yaml`/`.yml` vs `.toml`).
- Implement `Config::to_canonical_yaml() -> String` that serializes in the fixed section order (`admission`, `access`, `storage`, `advanced`) and omits fields equal to their default. This is the backing for every "export" surface.
- Introduce `defaults_version: V1` enum field (serde-skipped when default) so `defaults: v1` in YAML pins defaults.
- Add `deltaglider_proxy config migrate <input.toml> [--out <output.yaml>]` subcommand that loads TOML and emits canonical YAML.
- CI step: generate `schema/deltaglider.schema.json` from schemars on release; publish as artifact.

**Key files**
- `src/config.rs` — add YAML loader, canonical serializer, schemars derives.
- `src/main.rs:36` — add `config` subcommand scaffold (`deltaglider_proxy config ...`).
- `src/bin/` — **not** used; reuse main binary per user decision.
- New: `src/cli/config.rs` — subcommand dispatcher (`migrate`, `show`, `defaults` will land here in later phases).
- Keep `src/config.rs:1029` (test_registry_completeness) green.

**Acceptance**
- `deltaglider_proxy --config example.yaml` starts and behaves identically to `--config example.toml` when both express the same content.
- `deltaglider_proxy config migrate deltaglider_proxy.toml.example` produces a YAML that round-trips (re-loaded config is structurally equal to the TOML-loaded one).
- `cargo test --all --locked` passes; new tests cover YAML parsing for every variant currently covered for TOML.
- `schema/deltaglider.schema.json` is generated by CI and validates the migrated example YAML.

**Migration considerations**
- TOML remains fully supported. Deprecation is a Phase 6 concern.
- The current two-stage load in `src/main.rs:144` (pre-async then async-main) must still work with YAML — just swap the file reader.
- Env var overrides (`ENV_VAR_REGISTRY`) apply after file load regardless of format. No change needed there.

---

### Phase 1 — Admin API upgrades (1 week)

> **✅ STATUS: Done (with two deferrals)**
>
> All four endpoints shipped, session-gated, all registered in `src/demo.rs`:
> - `GET /_/api/admin/config/export` — canonical YAML with every secret redacted via `Config::redact_all_secrets()` → `to_canonical_yaml()`. (See `src/api/admin/config/document_level.rs::export_config`.)
> - `GET /_/api/admin/config/defaults` — schemars JSON Schema of `Config`, including per-field defaults and docstrings.
> - `POST /_/api/admin/config/validate` — parse + structural validation via `parse_and_validate_yaml()` (shared with `/apply`).
> - `POST /_/api/admin/config/apply` — atomic full-document apply: parse → merge runtime secrets forward (`preserve_runtime_secrets`) → bootstrap-hash defense-in-depth reject → rebuild engine → hot-reload log/IAM → rebuild snapshots → persist. Returns `{applied, persisted, requires_restart, warnings, persisted_path}`. HTTP 500 (not 200+warning) on persist failure so GitOps pipelines can't mistake a half-applied state for a clean success.
>
> Seven integration tests cover the acceptance criteria in `tests/admin_config_test.rs`: export→apply→export byte-stability, redacted-secret round-trip preserving auth, persist-failure rollback, bootstrap-hash defense, empty-YAML rejection, invalid-log-filter rejection, asymmetric-creds warning.
>
> **Deferred to Phase 3** (by design — needs the sectioned Config shape):
> - `/export?section=admission|access|storage|advanced` query param. Today `/export` returns the whole document.
> - `/export?resolve=true` (expanding presets). Presets don't exist yet.
>
> **Scope divergence**: the plan called for a new `src/config/validator.rs` module. We kept validation centralised in `Config::check()` (which returns `Vec<String>` warnings) rather than creating a separate module — this is the single source of truth now, used from both `check()`-for-startup and `parse_and_validate_yaml()`-for-apply.

**Goal:** Server-side endpoints for document-level config operations exist, even before the GUI uses them.

**Scope**
- `GET /api/admin/config/export?format=yaml[&section=<name>][&resolve=true]` — returns canonical YAML of the current in-memory config. `section` narrows to one of `admission|access|storage|advanced`. `resolve=true` expands presets and defaults (for debugging).
- `GET /api/admin/config/defaults` — JSON with every default, its type, current value, and docstring (pulled from schemars descriptions). Backs `dgpctl defaults`.
- `POST /api/admin/config/apply` — accepts a full YAML document, validates (see below), atomically swaps `Arc<RwLock<Config>>`, persists to the configured file path. Respects the hot-reload vs. restart-required split that already exists at `src/api/admin/config.rs:409`. Returns `{ applied: true, requires_restart: bool, tainted_fields: [...] }`.
- `POST /api/admin/config/validate` — dry-run. Same YAML payload as `apply`, but only runs the validator. Used by CI (`dgpctl lint`) without needing a running target.
- Existing field-level `PUT /api/admin/config` stays — it's what the GUI forms use today.

**Key files**
- `src/api/admin/config.rs` — add `export`, `defaults`, `apply`, `validate` handlers. Reuse `compute_tainted_fields` at `:174` and `rebuild_engine` at `:293`.
- `src/api/admin/mod.rs` — route registration.
- New: `src/config/validator.rs` — centralizes validation logic (reference resolution, schema checks, dangerous-default warnings). Used by both `apply` and the CLI.

**Acceptance**
- `curl http://.../api/admin/config/export?format=yaml` returns a YAML that, when passed to `apply`, is a no-op (idempotency test).
- `apply` with a malformed YAML returns 400 with a field-level error list, no state change.
- Existing hot-reload tests at `tests/admin_config_test.rs` keep passing; add new tests for `apply`.

---

### Phase 2 — Admission chain (2–3 weeks, largest single chunk)

> **🟡 STATUS: Partial — scope deliberately narrowed, carries into Phase 3**
>
> **What shipped:**
> - `src/admission/` module with `AdmissionChain`, `AdmissionBlock`, `Match`, `Action`, `Decision` types + an `evaluator` sub-module + a `middleware` sub-module.
> - `SharedAdmissionChain = Arc<ArcSwap<AdmissionChain>>` on `AdminState`; rebuilt alongside the `PublicPrefixSnapshot` whenever bucket policies change.
> - `admission_middleware` inserted in `src/startup.rs` BEFORE `sigv4_auth_middleware`. On a match with `AllowAnonymous`, it plants an `AdmissionAllowAnonymous` marker in request extensions; SigV4 checks the marker and skips signature verification, minting the `$anonymous` principal.
> - `sigv4_auth_middleware` refactored: the old inline public-prefix lookup is gone; SigV4 now only consumes the extension marker. The pre-existing `tests/public_prefix_test.rs` suite (12 tests) passes unchanged — behaviour-preserving refactor.
> - `POST /_/api/admin/config/trace` handler in `src/api/admin/config/trace.rs`. Same evaluator backs live traffic and trace: trace cannot lie.
> - Integration tests in `tests/admission_test.rs`: three acceptance scenarios from the plan (anonymous GET on public bucket → allow; anonymous GET on private bucket → continue-to-deny; authenticated PUT never rides a public-prefix grant) + trace-vs-live parity on bucket/key parsing.
>
> **What was deliberately cut from Phase 2 scope** (deferred to Phase 3):
>
> | Plan item | Why cut | Lands in |
> |---|---|---|
> | `Match` variants: `source_ip` / `source_cidr` / `source_ip_list`, `path_glob`, `method`, `authenticated`, `config_flag` | No operator-facing YAML to populate them yet — only sense once `Config::admission:` exists. | Phase 3 |
> | `Action` variants: `Deny`, `RateLimit`, `Reject` | `RateLimit` in particular requires a new sliding-window primitive; the existing `RateLimiter` is an auth-failure counter, not a request-rate limiter. Shipping these as empty shells would be vaporware. | Phase 3 |
> | 5-block default chain (`deny-known-bad-ips`, `allow-anonymous-public-buckets`, `rate-limit-anonymous`, `rate-limit-authenticated`, `continue`) | Shipped only the public-prefix block (derived from `buckets.*.public_prefixes`); the other four blocks either depend on variants above or on operator-facing config. | Phase 3 |
> | `Config::admission: Option<AdmissionChain>` field | Today the chain is entirely derived from `config.buckets[*].public_prefixes` — adding a field now is pure ceremony. It lands when operators can author blocks directly. | Phase 3 |
> | Trace endpoint returning `{admission, identity, iam, parameters, routing}` for all five layers | Only admission is a first-class concept today. | Phase 2.5+ (when those layers exist) |
> | Deprecation INFO on legacy `DGP_RATE_LIMIT_*` / `[buckets.*] public_prefixes` | No replacement exists yet for operators to migrate to. | Phase 6 |
>
> **Shipped extras (not in original plan but emerged from implementation):**
> - A SigV4 extension-marker protocol between the admission and SigV4 middlewares so admission has no reverse dependency on `crate::api::auth::AuthenticatedUser`. Clean module seam.
> - `Decision` as a distinct type (plan bundled decision into `Action`). Makes trace output serde-friendly.
> - The `percent_decode` function in `api/auth.rs` was made `pub` and reused by admission middleware + trace handler — closes a 3-copy duplication the plan didn't foresee.

**Goal:** Admission becomes a first-class feature with its own data model, evaluator, and trace tool. Existing public-prefix and rate-limiter behavior is reimplemented ON TOP of it, identically, from the operator's perspective.

**Scope**
- New module `src/admission/` with `AdmissionChain`, `AdmissionBlock`, `Match`, `Action`.
  - `Match` fields: `method`, `source_ip` / `source_cidr` / `source_ip_list`, `path_glob`, `bucket`, `authenticated: bool`, `config_flag` (for maintenance mode toggle).
  - `Action` variants: `Deny`, `AllowAnonymous`, `RateLimit { per_ip, per_principal, burst }`, `Reject { status, message }`, `Continue`.
- Built-in default chain (constructed in code, used when `admission:` is absent or empty):
  1. `deny-known-bad-ips` (match `source_ip_list: @abuse-list` — empty list unless configured; block is a no-op until populated).
  2. `allow-anonymous-public-buckets` (synthesized from buckets with `public: true`).
  3. `rate-limit-anonymous` (30/min per IP).
  4. `rate-limit-authenticated` (1000/min per principal).
  5. `continue` (default terminal).
- Evaluator: `admission::evaluate(&chain, &req) -> Decision`. Runs before SigV4 middleware at `src/api/auth.rs`.
- Anonymous-ok matches inject the existing `$anonymous` `AuthenticatedUser` (same mechanism as today's public prefix path at `src/bucket_policy.rs`).
- Rate-limit actions delegate to the existing `RateLimiter` at `src/rate_limiter.rs` but keyed by the admission block's scope (per-IP, per-principal, or per-IP+bucket composite).
- Migration: on YAML load, if legacy `[buckets.<name>] public_prefixes = [...]` or `DGP_RATE_LIMIT_*` env vars are present, synthesize equivalent blocks and log an INFO "deprecated, consider moving to admission chain".
- Trace endpoint: `POST /api/admin/config/trace` with `{method, path, source_ip, authenticated, access_key_id?}` → returns per-layer decisions `{admission, identity, iam, parameters, routing}`.

**Key files**
- New: `src/admission/mod.rs`, `src/admission/match_.rs`, `src/admission/action.rs`, `src/admission/evaluator.rs`, `src/admission/default_chain.rs`.
- `src/api/auth.rs` — insert admission evaluation before SigV4 verification.
- `src/bucket_policy.rs:41` — `public_prefixes` becomes a synthesis source for admission, not a runtime concept.
- `src/rate_limiter.rs` — extend with scope-keyed limiters (existing per-IP stays; add per-principal and per-IP+bucket).
- `src/api/admin/config.rs` — add `/trace` handler; reuse engine and IAM state.
- `src/config.rs` — new `admission: Option<AdmissionChain>` field.

**Acceptance**
- Integration test: request to a public-prefixed bucket succeeds without credentials before and after migration (behavior preserved).
- `admission trace` dry-runs return expected layer outputs for:
  - anonymous GET on public bucket → allow-anonymous, skip SigV4.
  - anonymous GET on private bucket → rate-limit hit then deny.
  - authenticated PUT with IAM deny → admission:continue, auth:ok, iam:deny.
- Env-var-driven rate limits (`DGP_RATE_LIMIT_*`) continue to work unchanged for existing deployments.

---

### Phase 3 — Progressive-disclosure YAML schema (2 weeks)

> **❌ STATUS: Not started — this is the critical next target**
>
> Phase 3 blocks the remaining Phases 4 / 5 / 6. It's the largest
> structural change but the rest of the programme cannot materially
> advance without it.
>
> **Opening moves, in order:**
> 1. Reshape `Config` to expose `pub admission: Option<AdmissionChain>`, `pub access: AccessConfig`, `pub storage: StorageConfig`, `pub advanced: AdvancedConfig`. Use `#[serde(flatten)]` to keep the wire format (and therefore all existing Phase 0/1/2 tests) stable during the transition.
> 2. Extend the admission module to carry the variants Phase 2 deferred (`Deny`, `RateLimit`, `Reject`, plus the `Match` fields for IP/path/method). Write the 5-block default chain.
> 3. Add `src/config/sections/` with the new section types. Shorthand deserialisers for `storage` (single-backend inference + `public: true` → synthesised admission block).
> 4. Implement `access.iam_mode: Gui | Declarative` + the reconciler (`src/iam/reconciler.rs`) that sync-diffs DB ↔ YAML on apply.
> 5. Flesh out the admin-API: `/export?section=...` + `/export?resolve=true` (presets expanded) become implementable.
>
> **Helpful artifact already in the repo**: `examples/scrape_full_config.rs`
> emits a close approximation of the Phase 3 target YAML. It's not
> wire-stable yet (the current `Config` struct doesn't parse it) but it
> provides a living reference of the shape to aim for. Treat it as the
> executable spec for this phase.

**Goal:** The 4-section YAML layout with shorthands, presets, and auto-implies is the canonical format. TOML still loads (deprecated).

**Scope**
- Rework `Config` shape to expose four public sections:
  - `admission: Option<AdmissionChain>`
  - `access: AccessConfig` (provider sources, group presets, users, mapping rules, plus `iam_mode: Gui | Declarative`)
  - `storage: StorageConfig` (backends + buckets; shorthand with single backend inferred when only one is present)
  - `advanced: AdvancedConfig` (everything else: listener, TLS, limits, timeouts, observability, rate-limit tuning, session TTL)
- Shorthand deserializer for `storage`:
  - `storage: { s3: URL, region, credentials, buckets: [...] }` → single unnamed backend + inline buckets.
  - `storage: { filesystem: PATH, buckets: [...] }` → filesystem backend.
  - `storage: { backends: [...], buckets: [...] }` → explicit long form with `backend:` refs.
- Bucket `public: true` → auto-synthesizes an admission `allow-anonymous` block for that bucket's GET/HEAD at startup.
- Group presets: `{ preset: admin | read-only | read-write | tenant-scoped | public-read-only, buckets: [...], prefix_from?: tag.X, quota_from?: tag.Y }` expands to a built-in IAM policy document in-memory. Visible in `export?resolve=true`.
- `access.iam_mode`:
  - `Gui` (default for backward compat): DB is source of truth. YAML `access.users/groups/providers` applied as seed only if DB is empty at startup. Runtime changes to YAML `access` are ignored with warning.
  - `Declarative`: YAML is source of truth. On every `apply` and on startup, reconcile DB to YAML (insert/update/delete). Admin API user/group CRUD returns 403 with explanatory body.
- Canonical exporter emits long form; parser accepts both.
- Defaults pinning: `defaults: v1` at top level. Upgrading the server to a v2 defaults release requires either removing the key (opt-in new defaults) or bumping to `v2`.

**Key files**
- `src/config.rs` — large restructure; keep current fields behind the new sectioned types via `#[serde(flatten)]` initially to minimize blast radius on tests.
- New: `src/config/access.rs`, `src/config/storage.rs`, `src/config/advanced.rs` — each section's types + shorthand deserializers.
- New: `src/iam/presets.rs` — built-in preset expanders producing `iam_rs::PolicyDocument`.
- New: `src/iam/reconciler.rs` — declarative-mode DB reconciliation.
- `src/api/admin/users.rs` / `groups.rs` / `external_auth.rs` — gate write handlers on `iam_mode`; return 403 when declarative.
- `src/bucket_policy.rs` — wire `public: true` synthesis; keep legacy `public_prefixes` readable for migration.

**Acceptance**
- The 5-line T1 example (`name:` + `storage.s3: URL`) loads, starts, serves S3 traffic.
- The Acme T3 example (kitchen sink from prior discussion) loads and the trace tool reproduces the expected decisions.
- `access.iam_mode: declarative` switches GUI CRUD to 403 and reconciles DB to YAML on startup.
- `dgpctl config migrate` (from Phase 0) now outputs the new sectioned shape.
- Existing TOML configs still load (legacy path).

---

### Phase 4 — CLI (1 week)

> **🟡 STATUS: 20% done**
>
> Shipped (Phase 0 scaffolding):
> - `deltaglider_proxy config migrate <in> [--out <path>]` — converts TOML to canonical YAML, idempotent on YAML input.
> - `deltaglider_proxy config schema [--out <path>]` — emits the JSON Schema.
>
> Pending (blocked on Phase 3 for most):
> - `config init [--example NAME]` — needs the `examples/*.dgp.yaml` library which lands alongside the sectioned schema.
> - `config lint <file>` — needs the sectioned validator.
> - `config show [--for bucket/NAME|user/NAME] [--resolve]` — needs `/export?section=` from Phase 3.
> - `config defaults [--version v1]` — wrapper over the existing `/defaults` endpoint; could ship today but naturally pairs with the richer schema from Phase 3.
> - `config explain bucket <name>` / `explain user <name>` — needs preset expansion (Phase 3).
> - `admission trace --request '<method> <path> from <ip> as <principal>'` — wrapper over `/trace`; could ship today but the request grammar benefits from Phase 3's operator-facing admission vocabulary.
> - `config apply <file>` — wrapper over `/apply`; could ship today. Small independent win.

**Goal:** All tooling is usable from the terminal. Every CLI command has an equivalent admin-API endpoint from Phase 1/2.

**Scope**
Add subcommands to `src/main.rs:36` (existing `Cli` struct) — reshape current flags (`--init`, `--show-env`, `--show-toml`) into subcommands while keeping the old flags as hidden aliases for one release:

- `deltaglider_proxy config init [--example NAME]` — interactive wizard (5 questions) or dump a named example.
- `deltaglider_proxy config lint <file>` — schema + reference-resolution + dangerous-default warnings. Calls the in-process validator; works offline.
- `deltaglider_proxy config migrate <toml> [--out <yaml>]` — from Phase 0.
- `deltaglider_proxy config show [--for bucket/NAME|user/NAME] [--resolve]` — prints current effective config (hits `/export`) or a specific resource's resolved view.
- `deltaglider_proxy config defaults [--version v1]` — dumps every default with docstring. Hits `/defaults`.
- `deltaglider_proxy config explain bucket <name>` / `explain user <name>` — shows which settings applied and where each came from (default / preset / override).
- `deltaglider_proxy admission trace --request '<method> <path> from <ip> as <access_key|anonymous>'` — calls `/trace`; prints layer-by-layer decisions.
- `deltaglider_proxy config apply <file>` — push a YAML to a running server via `/apply`.

**Key files**
- New: `src/cli/config.rs`, `src/cli/admission.rs` — command dispatchers.
- `src/main.rs` — wire subcommands, keep `--init` as aliased entrypoint.
- Embedded examples via `rust-embed`: `examples/*.dgp.yaml` bundled into the binary.
- New directory: `examples/` at repo root with ~10 curated files (homelab, dev-local, single-tenant-s3, team-with-oauth, ci-artifact-storage, cdn-origin, homelab-nvme-cache, multi-tenant-saas, regulated-soc2, acme-robotics-kitchen-sink). Each opens with a `⚠️ Before production` comment banner.

**Acceptance**
- `deltaglider_proxy config init --example homelab > my.yaml && deltaglider_proxy --config my.yaml` starts successfully.
- CI runs `deltaglider_proxy config lint examples/*.dgp.yaml` in the workflow; failures block merge.
- `deltaglider_proxy admission trace --request 'GET /my-bucket/readme.md from 8.8.8.8 as anonymous'` emits expected decisions against a running local server.

---

### Phase 5 — GUI redesign (3–4 weeks, parallelizable with Phase 2/4)

> **❌ STATUS: Not started**
>
> Blocked on Phase 3 — the four-tab layout mirrors the four Config
> sections, so there's nothing to render until those sections exist.
> Some ancillary pieces (`TracePage`, `CopyAsYamlButton`) could be
> prototyped against the Phase 1 endpoints that already ship, but doing
> so before Phase 3 risks churn when the sectioned shape lands.

**Goal:** The admin GUI mirrors the 4-section mental model, every page offers a "Copy as YAML" button, and a first-run wizard bridges zero-to-working.

**Scope**
- **Tab consolidation** in `demo/s3-browser/ui/src/components/AdminPage.tsx`:
  - `Admission` (new) — ordered list editor with drag-to-reorder, match/action form, enable-toggle, live-test box.
  - `Access` (merges Users + Groups + Authentication) — master-detail: left column is sources (OIDC providers + local), right column toggles into users, groups, mapping rules with tabs. Group preset dropdown.
  - `Storage` (merges Backends + Backend + per-bucket settings) — master-detail: backends on the left, buckets on the right. Compression / encryption fields inline.
  - `Advanced` (merges Limits + Security + Logging) — grouped forms with defaults visually greyed out; only overrides are editable.
  - `Metrics` — stays as-is.
- **Copy as YAML** button on each tab header → calls `GET /api/admin/config/export?format=yaml&section=<tab>` and opens a modal with syntax-highlighted YAML + copy-to-clipboard.
- **First-run wizard** (new route `/_/setup`) — appears when config file is missing/empty at server startup. Five questions; writes the produced YAML via `/apply`.
- **Trace tool page** (`/_/trace`) — form for synthetic request, displays layer-by-layer decision panel (admission → identity → IAM → parameters → routing).
- **iam_mode badge**: when declarative mode is active, surface a persistent banner on Users/Groups/Auth tabs: "IAM is managed via YAML. Edit your config file to change users." Buttons/forms are read-only.
- **Cascade visualization** on the Storage tab: when a bucket overrides a global setting, show the inherited value struck through next to the override with a "revert to inherited" button.

**Key files**
- `demo/s3-browser/ui/src/components/AdminPage.tsx` — tab restructure.
- New: `demo/s3-browser/ui/src/components/AdmissionPanel.tsx`, `TracePage.tsx`, `SetupWizard.tsx`, `CopyAsYamlButton.tsx`.
- `demo/s3-browser/ui/src/adminApi.ts` — add client functions for `/export`, `/apply`, `/validate`, `/trace`, `/defaults`.
- Existing `UsersPanel` / `GroupsPanel` / `AuthenticationPanel` merge into `AccessTab.tsx`; existing `BackendsPanel` merges with bucket config into `StorageTab.tsx`.
- Keep `SimpleSelect` / `SimpleAutoComplete` patterns (the Ant Design popup bug workaround noted in CLAUDE.md).

**Acceptance**
- A new user with an empty config file lands on the setup wizard, answers 5 questions, ends on the dashboard with a working config.
- Clicking "Copy as YAML" on any page produces YAML that, when saved to a file and passed to `--config`, reproduces the same runtime state.
- Trace page given a real synthetic request mirrors what `deltaglider_proxy admission trace` returns.
- `iam_mode: declarative` disables all user/group/OIDC-provider editing buttons in the GUI.

---

### Phase 6 — Deprecation sweep & preset library expansion (1 week, done last)

> **❌ STATUS: Not started**
>
> Blocked on Phase 3/4/5: there's no canonical YAML shape to promote as
> the replacement for TOML yet, and the `examples/*.dgp.yaml` library
> lands in Phase 4 alongside `config lint`.

**Goal:** TOML becomes explicitly deprecated. The preset library is broad enough to cover the common cases.

**Scope**
- Emit a prominent WARN log line when TOML is loaded: "TOML config is deprecated and will be removed in vNEXT+2. Run `deltaglider_proxy config migrate` to convert."
- Docs updated: README, CLAUDE.md, `deltaglider_proxy.toml.example` stays but gets a deprecation banner at the top; add `deltaglider_proxy.yaml.example` as the new canonical.
- Ensure all ~10 example files under `examples/` are CI-linted AND CI-traced (run each one's documented expected decisions through `admission trace` to catch regressions).
- Schema version bump: introduce `defaults: v2` concept with the ability to document deltas in release notes.
- Removal deadline defined: TOML support removed in vNEXT+2 (roughly 2 minor versions post Phase-6 ship).

**Key files**
- `src/config.rs` — TOML load path emits deprecation warning.
- `CLAUDE.md` — update config section with YAML-first story.
- `README.md` — quickstart becomes 5-line YAML.
- `examples/*.dgp.yaml` — verify each file's banner and tested decisions.

**Acceptance**
- Every example file in `examples/` passes `config lint` and the documented trace scenarios.
- Fresh-install users starting from the quickstart never see TOML.
- Existing TOML users see a single WARN line explaining the migration path.

---

## Cross-cutting concerns

### Verification

End-to-end per phase:

- **Phase 0**: `cargo test --all --locked`; `cargo run -- config migrate deltaglider_proxy.toml.example` produces valid YAML that re-loads; schema JSON validates.
- **Phase 1**: `curl -X POST .../api/admin/config/validate` with good and bad YAML bodies returns expected codes; hot-reload test at `tests/admin_config_test.rs` still green.
- **Phase 2**: integration test harness at `tests/common/mod.rs` spins up with `admission:` section; hit endpoints with and without credentials, with IP constraints, verify the chain fires in order. Golden-file test for default chain JSON.
- **Phase 3**: round-trip tests: TOML → YAML via migrate → YAML re-load → exported YAML matches migrated input. Declarative-mode reconciliation test: start with DB containing extra user, YAML missing it, confirm user is removed.
- **Phase 4**: CLI binary smoke-tested in CI against a running in-process server (the `TestServer` harness already supports this).
- **Phase 5**: Playwright or manual checklist against the UI — each of the 4 tabs loads, Copy-as-YAML round-trips, wizard writes a valid file, trace page returns decisions.
- **Phase 6**: release-candidate build tested against real customer config (Acme example) start-to-finish.

### Backward compatibility guarantees (all phases)

- Every TOML config that works on vCURRENT must still work through Phase 5. Only Phase 6 introduces a deprecation warning; removal is ≥2 minor versions later.
- Env var overrides (`ENV_VAR_REGISTRY`) remain authoritative over file contents for operators who deploy via Docker/K8s with env injection.
- Bootstrap password handling unchanged — it remains the SQLCipher cipher and the admin-session signer in both IAM modes.
- Existing admin API field-level PATCH (`PUT /api/admin/config`) stays; new `/apply` endpoint is additive.

### Critical files (reference)

- `src/config.rs` — central config struct, Serde, env parsing, registry (lines 256–690, 965–1095).
- `src/api/admin/config.rs` — admin HTTP handlers, hot-reload dispatch, tainted-fields tracker (lines 174–863).
- `src/bucket_policy.rs` — per-bucket overrides and public_prefixes snapshot (lines 12–192).
- `src/iam/mod.rs` — `IamState`, hot-swap via `ArcSwap` (lines 32–119).
- `src/config_db/*.rs` — SQLCipher-backed IAM storage (users, groups, auth providers).
- `src/api/auth.rs` — where SigV4 middleware lives; admission chain plugs in just before it.
- `src/rate_limiter.rs` — existing per-IP rate limiter; extended with scope keys.
- `src/main.rs:36` — `Cli` struct; subcommands added here.
- `tests/admin_config_test.rs` — hot-reload regression tests.
- `demo/s3-browser/ui/src/components/AdminPage.tsx` — admin GUI main layout.
- `demo/s3-browser/ui/src/adminApi.ts` — typed admin API client.

### Dependency order

```
Phase 0 ─┬─ Phase 1 ─┬─ Phase 2 ──┐
         │           │            │
         └───────────┴─ Phase 3 ──┼─ Phase 4 ──┐
                                  │            │
                                  └─ Phase 5 ──┴─ Phase 6
```

Phase 2 (admission) and Phase 3 (schema refactor) are the heaviest. They can start in parallel once Phase 1 lands, provided the team splits backend work cleanly (admission module is self-contained; schema restructure touches nearly every file).

Phase 5 (GUI) can start against Phase 1's endpoints and iterate as Phase 2/3 land — the "Copy as YAML" and "Export" flow is functional from Phase 1 onward.

### Risks & mitigations

- **Risk: schema restructure breaks tests across the codebase.** Mitigation: Phase 3 uses `#[serde(flatten)]` internally so the on-disk YAML shape changes but the in-memory `Config` field access patterns stay similar for one release, minimizing test churn.
- **Risk: declarative IAM mode loses DB state on accidental YAML typo.** Mitigation: validator rejects apply if the resulting diff would delete >N users unless `--force`. GUI `apply` includes a diff-preview modal.
- **Risk: shorthand + longhand YAML creates two mental models.** Mitigation: the canonical exporter always emits longhand; `dgpctl config lint --strict` can flag shorthand in CI-enforced repos. Docs promote longhand from example #2 onward.
- **Risk: admission chain reorders request processing and breaks something subtle.** Mitigation: Phase 2 ships with default chain that exactly reproduces current behavior (public_prefixes synthesized, existing rate limits preserved); operator opt-in to write their own blocks.
- **Risk: YAML load performance on large configs.** Mitigation: `serde_yml` is fast enough for configs under ~10K lines; config is loaded once at startup + on explicit apply, not per-request.

### Out of scope (flagged for follow-ups, not this plan)

- Policy-as-code / Rego / OPA integration.
- Multi-file YAML includes (e.g., `include: buckets.yaml`). If needed, revisit for >500-bucket deployments.
- Presets as live-inheritable templates (deliberately rejected; presets are copy-paste examples).
- `dgpctl` as a separate distributable binary (per user decision, subcommands on existing binary).
- Terraform provider / Pulumi SDK for declarative config — downstream of YAML schema stabilization.

---

## Bonus: unplanned work that shipped (2026-04-20)

Three bodies of work landed alongside Phases 0/1/2 that weren't in the
original plan. Each is captured here so future readers don't spend time
wondering whether they were the plan's intent.

### Hygiene pass

The plan called for a Phase 3 restructure of `Config` and `admin/config.rs`;
we did an interim hygiene pass that shrank the surface area before that
bigger change lands:

- **Split `src/api/admin/config.rs` (1908 lines) into `src/api/admin/config/`** with cohesive submodules: `mod.rs` (shared helpers + `test_s3_connection`), `field_level.rs` (`get_config`, `update_config`, `apply_backend_patch`), `document_level.rs` (Phase 1 export/validate/apply + secret preservation), `password.rs` (`change_password`, `recover_db`), `trace.rs` (admission trace handler). Each submodule owns the request/response types for the handlers it contains.
- **New `apply_config_transition` helper** as the single source of truth for runtime side effects on config change (engine rebuild, log reload, IAM swap, snapshot rebuild, restart detection). Both the field-level PATCH (`update_config`) and document-level APPLY (`apply_config_doc`) now compose responses from it — ending the behaviour drift the plan's risk section warned about.
- **New `RateLimitGuard` RAII wrapper** in `src/rate_limiter.rs`. Replaces the "extract IP → is_limited → progressive-delay → record success/failure + SECURITY log" pattern at four call sites (admin login, login_as, oauth callback, recover_db).

### Correctness fixes (adversarial audits)

Three rounds of hostile audits surfaced real bugs, now fixed:

- **Atomic config persistence.** `persist_to_file` used `std::fs::write` (truncate-then-write); a crash mid-save truncated the file. Now write-to-tempfile + fsync + rename.
- **Backend-create persist-path bug** (data loss): `src/api/admin/backends.rs` hardcoded `DEFAULT_CONFIG_FILENAME` instead of `active_config_path`, silently redirecting backend CRUD writes to the wrong file when the server was launched with `--config /some/other/path`. Regression test in place.
- **IPv4-mapped IPv6 rate-limit bypass**: `::ffff:1.2.3.4` and `1.2.3.4` hashed to different buckets. An attacker could double their brute-force budget by alternating representations. Fixed with a normalise-on-extraction pass.
- **`log_level` PATCH poisoning**: the PATCH path wrote the new string into `cfg.log_level` BEFORE parsing it as an `EnvFilter`. Invalid strings persisted to disk. Fixed: parse first, mutate second (mirroring the APPLY path).
- **NaN / Infinity in `max_delta_ratio`**: NaN comparisons always false (old `< 0.0 || > 1.0` check missed it); Infinity passed the warning branch but survived as a value — every file would be stored as a delta regardless of size. Both now clamped to default.
- **Duplicate backend names**: `Config::check()` now warns on duplicates (routing silently shadowed the second entry before).
- **Defense-in-depth on `bootstrap_password_hash`**: `apply_config_doc` rejects YAML that tries to change the hash; legitimate path is `PUT /_/api/admin/password` which verifies the current password.
- **Empty `backend_path` rejection + case-insensitive `backend_type`**: small UX fixes in the PATCH handler.

### `examples/scrape_full_config.rs`

A read-only utility that opens the SQLCipher IAM DB with the bootstrap
hash, reads users/groups/providers/mapping rules, and combines with the
on-disk TOML or YAML config to emit:

- A new-style sectioned YAML on stdout (admission/access/storage/advanced) with every secret replaced by `!secret NAME`.
- A companion `.env` file (mode 0600, 1:1 with the YAML placeholders) to be fed into SOPS / Vault / CI secret providers.

The 1:1 YAML↔.env pairing is enforced at emission time via a
`SecretsDump::record()` call placed next to every `!secret NAME` emission,
so you can't leak a secret or reference a missing one.

**Why it matters for Phase 3**: this tool is effectively a living spec
of the target YAML shape. When you start Phase 3, running it against
the current prod deployment gives you the exact document the new
`Config` struct needs to accept.
