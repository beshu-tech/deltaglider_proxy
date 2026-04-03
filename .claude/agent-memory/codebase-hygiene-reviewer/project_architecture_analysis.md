---
name: Architecture analysis - testability and modularity
description: Deep analysis of DeltaGlider Proxy architecture for testability and module decomposition opportunities (March 2026)
type: project
---

## Architecture Findings (updated 2026-03-30)

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

**Remaining structural observations:**
- `session.rs` parses `DGP_SESSION_TTL_HOURS` independently from config's `env_parse()` helper. Low impact -- `env_parse` is private and session module runs before config is fully loaded.
- s3.rs is 1554 lines (down from 1578). It's dense but well-structured with clear internal helper grouping.
- Client IP extraction exists in two forms: `rate_limiter::extract_client_ip()` (returns `Option<IpAddr>`) and `audit::extract_client_info()` (returns `(String, String)` with UA). These serve different purposes and are reasonable to keep separate.
- `paginate_sorted` in engine/mod.rs has only one caller. Not worth inlining -- the function is clear and may gain more callers if delimiter-less listing grows.
- SettingsPage.tsx has a dead standalone rendering path (lines 555-585) that's never reached since AdminPage always passes `embeddedTab`.

**Good patterns:**
- Engine is generic over StorageBackend (DeltaGliderEngine<S>)
- DynEngine type alias for trait objects provides good flexibility
- StoreContext parameter object avoids too_many_arguments
- TestServer builder pattern in tests/common/mod.rs is well-designed
- ConfigDb has in_memory() test constructor
- init.rs is testable (parameterized BufRead/Write)
- `interleave_and_paginate()` is the single source of truth for list pagination
- Clippy is fully clean with -D warnings, 185 unit tests pass
- `eprintln!`/`println!` usage in config.rs and main.rs is all intentional (pre-logger startup output)
- `ref_key` serde alias in types.rs is needed for backwards-compat with existing .meta files
- `classify_get_error()` and `s3_body_to_stream()` centralize GetObject error handling and body streaming
- `dedup_keep_latest()` is the single source of truth for version deduplication
- `build_iam_evaluator()` centralizes IAM request construction

**Why:** Understanding structural debt helps prioritize refactoring with maximum testability impact.
**How to apply:** Use this as a reference when planning refactoring work on main.rs or admin API.
