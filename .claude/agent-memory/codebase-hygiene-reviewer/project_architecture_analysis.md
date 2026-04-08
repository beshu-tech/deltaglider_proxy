---
name: Architecture analysis - testability and modularity
description: Deep analysis of DeltaGlider Proxy architecture for testability and module decomposition opportunities (April 2026)
type: project
---

## Architecture Findings (updated 2026-04-06)

**Resolved issues (from prior analysis):**
- Duplicated `sanitize_audit` fn -- resolved, now centralized in `audit.rs`
- Duplicated `insert_permissions` / `insert_group_permissions` -- resolved, uses `insert_permission_rows` with table/fk params
- Triplicated interleave-sort-paginate for ListObjectsV2 -- resolved, now uses shared `interleave_and_paginate()` in engine.rs
- `eprintln!` in iam/permissions.rs evaluate_iam -- resolved, now uses `tracing::warn!`
- Inline IP extraction in api/auth.rs -- resolved, now uses `audit::extract_client_info()`
- Dead `http_put` in storage_resilience_test.rs -- resolved, removed (2026-03-29)
- 3x duplicated GetObject NoSuchKey error mapping in s3.rs -- resolved, extracted `classify_get_error()` helper (2026-03-30)
- 2x duplicated S3 body-to-stream unfold pattern in s3.rs -- resolved, extracted `s3_body_to_stream()` helper (2026-03-30)
- 3x duplicated S3 ListObjectsV2 pagination loops (total_size, list_directory_markers, list_objects_full) -- resolved, total_size and list_directory_markers now delegate to list_objects_full (2026-03-30)
- Dead `delete_prefix` trait method + filesystem override -- resolved, removed (2026-03-30)
- Dead `list_directory_markers` trait method + S3 override -- resolved, removed (2026-03-30)
- Duplicated IAM request/ARN building in `evaluate_iam`/`is_explicitly_denied_iam` -- resolved, extracted `build_iam_evaluator()` + `build_resource_arn()` helpers (2026-03-30)
- Duplicated dedup-by-key-keeping-latest in engine `list_objects_bulk` and S3 `resolve_classified_lite` -- resolved, extracted `dedup_keep_latest()` in types.rs (2026-03-30)
- `DGP_REPLAY_WINDOW_SECS` parsed from env twice -- resolved, only one occurrence remains in api/auth.rs:602
- `sessionApi.ts` raw fetch() duplication -- resolved, now uses shared `adminFetch()` from adminApi.ts
- 2x duplicated AuthProviderConfig row mapping in config_db/auth_providers.rs -- resolved, extracted `auth_provider_from_row()` (2026-04-06)
- 3x duplicated S3 credential auto-population in auth.rs (login, login_as) and external_auth.rs (oauth_callback) -- resolved, extracted `auto_populate_s3_creds()` helper (2026-04-06)
- Duplicated group mapping accumulation in mapping.rs (evaluate_mappings, preview_email_mappings) -- resolved, extracted `collect_matching_groups()` (2026-04-06)
- Dead `ProviderDisabled` variant in ExternalAuthError -- resolved, removed (2026-04-06)
- 2x duplicated GroupMappingRule row mapping in auth_providers.rs -- resolved, extracted `group_mapping_rule_from_row()` (2026-04-06)
- Inconsistent RNG: oidc.rs used `thread_rng()` for PKCE/nonce while rest of codebase uses `OsRng` for security tokens -- resolved, changed to `OsRng` (2026-04-06)
- Unused `_provider` binding in sync_memberships -- resolved, replaced with `any()` check (2026-04-06)
- Test duplication: inline `CreateAuthProviderRequest` in 3 tests not using `make_provider_req` helper -- resolved (2026-04-06)
- Test duplication: 4x inline `PendingAuth` construction in mod.rs tests -- resolved, extracted `make_pending_auth` helper (2026-04-06)
- SettingsPage ghost `bucketPolicies` state silently overwrote BackendsPanel edits on save -- resolved, removed dead state/serialization (2026-04-06)
- AdminPage `savingRef` was plain object (not useRef), never read -- resolved, removed dead code (2026-04-06)
- AdminPage `authMode` state was write-only -- resolved, removed (2026-04-06)
- OAuth provider buttons duplicated between ConnectPage and AdminPage (with e.target bug in AdminPage) -- resolved, extracted OAuthProviderList component (2026-04-06)

**Open issues (2026-04-06 hygiene review):**
- 2x duplicated `BackendConfig` -> `BackendInfoResponse` conversion in config.rs:331-364 and backends.rs:69-101. Fix: add `From<&NamedBackendConfig>` impl.
- 2x duplicated engine rebuild pattern in backends.rs (lines 167-185 and 258-275) instead of using `rebuild_engine()` from config.rs. Fix: make helper pub(super).
- backendTab in SettingsPage.tsx duplicates saveSection inline (lines 359-397 vs 422-442).
- `update_config` handler in api/admin/config.rs is ~260 lines. Document-only for now.
- `update_auth_provider` in config_db/auth_providers.rs uses 11 individual UPDATE statements per field. Works but verbose. Document-only.

**Remaining structural observations:**
- `session.rs` parses `DGP_SESSION_TTL_HOURS` independently from config's `env_parse()` helper. Low impact.
- s3.rs is 1552 lines. Dense but well-structured with clear internal helper grouping.
- Client IP extraction exists in two forms: `rate_limiter::extract_client_ip()` and `audit::extract_client_info()`. Different purposes, reasonable to keep separate.
- `paginate_sorted` in engine/mod.rs has only one caller. Clear function, may gain more callers.
- `admin/config.rs` is 1122 lines with 5 handlers -- splitting would add files without proportional benefit.
- `list_buckets` and `list_buckets_with_dates` in routing.rs share structure but differ enough that unifying adds complexity without proportional benefit.
- `Arc<Box<dyn StorageBackend>>` double indirection in routing.rs -- minor, comes from construction chain.
- `config.backend` (singular, legacy) vs `config.backends` (Vec, multi) naming is confusing but documented and renaming breaks TOML compat.

**Good patterns:**
- Engine is generic over StorageBackend (DeltaGliderEngine<S>)
- DynEngine type alias for trait objects provides good flexibility
- StoreContext parameter object avoids too_many_arguments
- TestServer builder pattern in tests/common/mod.rs is well-designed
- ConfigDb has in_memory() test constructor
- init.rs is testable (parameterized BufRead/Write)
- `interleave_and_paginate()` is the single source of truth for list pagination
- Clippy is fully clean with -D warnings
- `eprintln!`/`println!` usage in config.rs and main.rs is all intentional (pre-logger startup output)
- `ref_key` serde alias in types.rs is needed for backwards-compat with existing .meta files
- `classify_get_error()` and `s3_body_to_stream()` centralize GetObject error handling and body streaming
- `dedup_keep_latest()` is the single source of truth for version deduplication
- `build_iam_evaluator()` centralizes IAM request construction
- `adminFetch()` in adminApi.ts is a well-designed shared fetch wrapper with credential handling
- `route!` macro in routing.rs cleanly abstracts virtual->real bucket dispatch
- `auth_provider_from_row()` / `external_identity_from_row()` / `group_mapping_rule_from_row()` follow same pattern as `user_from_row()`
- `auto_populate_s3_creds()` centralizes the "login IS connect" S3 credential setup
- `collect_matching_groups()` is the single source of truth for group mapping evaluation
- `OAuthProviderList` component is the single source of truth for OAuth sign-in buttons (ConnectPage + AdminPage)

**Why:** Understanding structural debt helps prioritize refactoring with maximum testability impact.
**How to apply:** Use this as a reference when planning refactoring work on admin API or auth middleware.
