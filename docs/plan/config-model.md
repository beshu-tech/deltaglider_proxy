# One configuration, one declaration

Status: decided, Option C (keep the two serde shapes; declare the field
map once, and let the compiler check it). Progress in §7. Written
2026-10-30.

## 1. The problem

One configuration exists in many forms. Most forms keep their own list of
the fields, and only some lists have a check that they agree. Adding one
field touches 10 to 16 files.

## 2. Every representation

| # | Representation | Where | What it is for | Verdict |
|---|---|---|---|---|
| 1 | `Config` (38 fields: 35 on disk, 3 runtime-only) | `src/config/mod.rs` | THE in-memory model, and the legacy FLAT on-disk shape (serde) | stays: several hundred field reads in `src/`; the flat shape must load |
| 2 | `SectionedConfig` + `AdmissionSection` / `AccessSection` / `StorageSection` / `AdvancedSection` | `src/config_sections.rs` | the canonical wire shape: export, persist, JSON schema, section API, the GUI's YAML editor | stays: a wire type (`deny_unknown_fields`, `skip_serializing_if`, shorthands, `Option` for default elision) |
| 3 | `from_flat` / `into_flat_unchecked` | `src/config_sections.rs` | the two projections between 1 and 2 | accidental: about 180 lines of field moves, written twice |
| 4 | `FLAT_ONLY_KEYS` | `src/config/mod.rs` | shape classifier (flat / sectioned / mixed) | accidental: a copy of the serde names of 1 (guarded by `flat_only_keys_cover_every_config_key`) |
| 5 | `default_*()` fns, `impl Default for Config` | `src/config/mod.rs` | defaults | stays (one fn per default); `Default` repeats the field list |
| 6 | `apply_env_overrides_with` | `src/config/mod.rs` | `DGP_*` → flat field | stays as code; its env→field map repeats 7 |
| 7 | `ENV_FIELD_BINDINGS`, `S3_BLOCK`, `TLS_BLOCK` | `src/config/env_overrides.rs` | env → YAML path, for the GUI's "from env" badge | partly accidental (repeats 6); guarded by a drift test |
| 8 | `EnvShadow`, `file_view()`, `env_refs`, `with_env_refs_reinserted` | `src/config/env_shadow.rs`, `mod.rs` | persist/export write the FILE's values, not env values | stays: generic over the serialized tree, no field list |
| 9 | `ENV_VAR_REGISTRY` | `src/config/registry.rs` | descriptions, examples, categories of every `DGP_*` | stays (prose); guarded by a literal scan |
| 10 | `RuntimeTuning` | `src/config/tuning.rs` | env-only settings that have no YAML field | stays: a different concern (no file, no section) |
| 11 | `engine_affecting_fields_changed` | `src/api/admin/config/transition.rs` | which changes rebuild the engine | accidental and UNGUARDED: a field the engine reads but this list misses is a silent no-op apply |
| 12 | `requires_restart_warnings` + `RESTART_ONLY_YAML_PATHS` + GUI `RestartChip` | `transition.rs`, `advancedPanels.tsx` | restart class | three copies, guarded by `restart_chip_parity_tests` |
| 13 | `compute_tainted_fields` | `src/api/admin/config/field_level.rs` | "unsaved" markers of the legacy GUI | a chosen subset (legacy GUI fields) |
| 14 | `ConfigResponse`, `ConfigUpdateRequest` | `field_level.rs` | `GET`/`PUT /api/admin/config` | stays: a GUI wire contract, a chosen subset with derived values (`auth_enabled`, `env_overrides`, tuning values), not a mirror |
| 15 | redactors: `redact_for_persist`, `redact_for_export`, `redact_all_secrets`, `BackendEncryptionConfig::redact_secrets` | `src/config/mod.rs` | secret removal | stays: lists the SECRET fields only, by design |
| 16 | `init.rs` struct literal | `src/init.rs` | `--init` wizard | accidental: names every field |
| 17 | TS section types, `advancedPanels.tsx`, `NO_GUI_CONTROL` | `demo/s3-browser/ui/src/` | GUI | stays (a different language); guarded by `envOverrides.test.ts` and the restart-chip test |
| 18 | `deltaglider_proxy.example.yaml`, `docs/product/reference/configuration.md` | repo root, docs | documentation | stays; the example is guarded by a schema walk (`src/lib.rs`) |
| 19 | `PublicPrefixSnapshot`, `AdmissionChain`, `BucketPolicyRegistry` | runtime | derived indexes | stay: derived, rebuilt on apply |

So two forms are real wire shapes (1, 2), one is a GUI contract (14), and
the rest are derived indexes, prose, or per-field LISTS. The lists (3, 4,
6/7, 11, 12, 16) are the duplication.

## 3. Cost of one field today

`advanced.range_spool_ttl_secs` (commit `55416aa4`, plus `42ce8f37`). The
config plumbing alone, without the engine code that reads it:

| File | Sites |
|---|---|
| `src/config/mod.rs` | field, `default_*` fn, `impl Default`, `FLAT_ONLY_KEYS`, env override |
| `src/config_sections.rs` | `AdvancedSection` field, `from_flat`, `into_flat_unchecked`, import of the default fn |
| `src/config/env_overrides.rs` | `ENV_FIELD_BINDINGS` row |
| `src/config/registry.rs` | `ENV_VAR_REGISTRY` row |
| `src/init.rs` | struct literal |
| `src/api/admin/config/transition.rs` | `engine_affecting_fields_changed` |
| `deltaglider_proxy.example.yaml` | example line |
| `docs/product/reference/configuration.md` | field section, env table row |
| `demo/s3-browser/ui/src/__tests__/envOverrides.test.ts` | `NO_GUI_CONTROL` |
| `CHANGELOG.md` | entry |

10 files, 17 sites. `filtered_list_max_engine_pages` (`873816b7`) has the
same shape. A field with a GUI control adds `adminApi/core.ts` and
`advancedPanels.tsx`, and a restart-class field adds `transition.rs` twice
and a `RestartChip`: 13 to 16 files.

Of the 17 sites, the compiler finds 3 when they are missing (the two
struct literals in the projections, the `init.rs` literal). Tests find 4
(`FLAT_ONLY_KEYS`, the env drift test, the registry scan, the example
walk). Nothing finds a missing `from_flat` line for a field that is
`Option` on both sides (it exports as `None`, silently), nor a missing
`engine_affecting_fields_changed` line (the apply reports success and has
no effect).

## 4. Constraints every option must keep

- **On-disk YAML.** Both the flat and the sectioned shape load; a mixed
  document is an error that names both halves. The canonical export is
  byte-stable: section and field ORDER come from the struct declarations,
  default elision from `skip_serializing_if`.
- **Env refs.** `env_refs` / `with_env_refs_reinserted` work on the
  serialized tree by path; `EnvShadow` paths are FLAT paths
  (`["listen_addr"]`, `["backends", Named, "encryption", "key"]`).
- **Redaction** names secret fields by hand on purpose: a new field is not
  a secret unless someone says so.
- **JSON schema** is `schema_for!(SectionedConfig)` and the four sections
  (`cli/config.rs`), served to the GUI's Monaco editor and `config schema`.
- **`GET /config`** (`ConfigResponse`) and the golden fixtures
  `tests/fixtures/admin_config_*.json`.
- **Prod-shape tests** (`prod_shape_tests`, `tests/prod_shape_matrix_test.rs`)
  round-trip `tests/fixtures/prod_shape_config.yaml`.
- **Source guards that read source text:** `envOverrides.test.ts` parses
  the `"advanced.…"` / `"storage.…"` / `"access.…"` string LITERALS in
  `env_overrides.rs` (so those paths cannot be built with `concat!`);
  `gui_restart_chips_match_the_server_list` parses `advancedPanels.tsx`;
  `every_dgp_literal_in_src_is_registered` scans for `DGP_*` literals;
  the example-YAML walk reads the section schemas.

## 5. Options

### Option A: one field table generates everything

A declarative (or proc) macro declares each field once: its type, its
section, its default, its env var, its GUI exposure, its restart class,
and generates `Config`, the four sections, the projections, the env
tables and the class predicates.

- **YAML / export.** Possible to keep byte-stable, but the two shapes
  carry DIFFERENT attributes per field (flat: `T` with
  `#[serde(default = "…")]`; sectioned: `Option<T>` with
  `skip_serializing_if`), and many fields are not scalars (`buckets`
  collapses to the `public: true` shorthand on export, `admission_blocks`
  becomes `admission.blocks`, the `storage.s3:` shorthand has no flat
  field). The macro grows a case per exception.
- **Env.** Overrides are not one-per-field: `DGP_S3_ENDPOINT` replaces
  the whole `backend` block, `DGP_TLS_ENABLED` the whole `tls` block,
  `RUST_LOG` beats `DGP_LOG_LEVEL`, per-backend encryption variables
  depend on the mode. A column covers about 18 of them.
- **Schema / docs.** schemars and rustdoc work on macro output, but every
  field doc moves into a macro body, and an error in one row points at
  the macro.
- **Source guards.** The UI test needs the YAML-path literals in
  `env_overrides.rs`; a generated table breaks it.

Verdict: rejected as a first move. The highest gain, but one large,
hard-to-review change, and the exceptions are a third of the fields.

### Option B: `SectionedConfig` becomes the in-memory model

- **Call sites.** Several hundred reads change (`cfg.max_object_size` →
  `cfg.advanced.max_object_size()`), because `AdvancedSection` uses
  `Option<T>` for default elision and needs accessors.
- **Flat YAML.** Still loads, so a flat struct and one projection remain:
  the duplication is moved, not removed.
- **Env shadow.** Every `EnvShadow` slot and the drift tests move from flat
  to sectioned paths.
- **GUI contract.** Unchanged if `ConfigResponse` reads accessors.

Verdict: rejected. Very large, and it keeps two shapes.

### Option C (recommended): keep both types; one field map, checked by the compiler

Keep `Config` and the sections as they are (no serde, schema, export or
GUI change). Replace the per-field LISTS by one declaration each, written
as exhaustive destructuring, so that a field that is not in the
declaration is a COMPILE error:

1. The projections: one macro invocation lists each flat field under its
   section, with its mapping kind (`same`: the same type on both sides;
   `default-elided`: `T` flat, `Option<T>` sectioned, `None` when equal to
   `Config::default()`). It generates `from_flat`, `into_flat_unchecked`
   and the flat root-key list. Both functions destructure their input
   with no `..`, so a field missing on either side does not compile. The
   few special fields (`admission_blocks`, `buckets`, the storage
   shorthands, `external_identities`, the runtime-only fields) are written
   once in the macro body.
2. The change classes (engine rebuild, restart, hot): one exhaustive
   destructure of `Config` that puts every field in a class. A new field
   does not compile until someone classifies it.
3. The env scalar overrides: one table of `(env, flat field, YAML path
   literal, kind, secret)` drives both `apply_env_overrides_with`'s scalar
   cases and `ENV_FIELD_BINDINGS`; the block overrides (S3, TLS, data dir,
   per-backend encryption, `RUST_LOG`) stay as code.

- **YAML / export.** Unchanged: the struct declarations still give the
  order; the projection logic is the same per field.
- **Env refs, redaction, schema, `GET /config`, fixtures.** Unchanged
  (no type changes).
- **Source guards.** The YAML-path literals stay literals;
  `flat_only_keys_cover_every_config_key` stays and now checks the
  generated list against the schema (it catches a serde rename).

Verdict: recommended. Each step is small, local and green on its own,
and it turns the unguarded lists (3, 11) into compile errors.

### Option D: keep everything, add completeness tests only

Cheapest; already done for 4, 7, 9, 12, 18. It finds a missing site but
does not remove one. Option C does both for the lists it touches.

## 6. Migration, one green step each

1. **Projections from one table.** `config_projection!` in
   `config_sections.rs` generates `from_flat`, `into_flat_unchecked` and
   `FLAT_ROOT_KEYS`; `FLAT_ONLY_KEYS` becomes that list. Removes the
   import of the default fns and `some_if_nondefault_str`. Gate: the
   canonical-export tests, `prod_shape_tests`, the admin config golden
   fixtures.
2. **Struct literals in production code use `..Config::default()`**
   (`init.rs`), so a new field with a default is not an edit there.
3. **Change classes.** `engine_affecting_fields_changed` becomes an
   exhaustive destructure that classifies every field (engine / not
   engine). Same result for today's fields. Later, the restart class joins
   the same declaration and `RESTART_ONLY_YAML_PATHS` is derived.
4. **Env scalar table** (see 5.C.3). Stop if it would move a YAML-path
   literal out of `env_overrides.rs`.
5. **Re-measure** the cost of one field. Then decide on GUI types
   generated from the section schemas (`adminApi/core.ts`).

Each step stops and is re-planned if it would change the canonical YAML
export, a golden fixture, or `GET /config`.

## 7. Progress
