---
name: Architecture analysis - testability and modularity
description: Deep analysis of DeltaGlider Proxy architecture for testability and module decomposition opportunities (March 2026)
type: project
---

## Architecture Findings (updated 2026-03-29)

**Resolved issues (from prior analysis):**
- Duplicated `sanitize_audit` fn -- resolved, now centralized in `audit.rs`
- Duplicated `insert_permissions` / `insert_group_permissions` -- resolved, uses `insert_permission_rows` with table/fk params
- Triplicated interleave-sort-paginate for ListObjectsV2 -- resolved, now uses shared `interleave_and_paginate()` in engine.rs
- `eprintln!` in iam/permissions.rs evaluate_iam -- resolved, now uses `tracing::warn!`
- Inline IP extraction in api/auth.rs -- resolved, now uses `audit::extract_client_info()`
- Dead `http_put` in storage_resilience_test.rs -- resolved, removed (2026-03-29)

**Remaining structural observations:**
- `Engine::delete_prefix()` and its StorageBackend trait method are never called from any handler, test, or admin route. They exist as internal code only. The filesystem impl does rm -rf. If this is planned functionality, it needs a caller; if not, it's dead code.
- `session.rs` parses `DGP_SESSION_TTL_HOURS` independently from config's `env_parse()` helper. Low impact -- `env_parse` is private and session module runs before config is fully loaded.
- s3.rs is the largest file (1646 lines of production code, no test module). It's dense but well-structured with clear internal helper grouping.
- Client IP extraction exists in two forms: `rate_limiter::extract_client_ip()` (returns `Option<IpAddr>`) and `audit::extract_client_info()` (returns `(String, String)` with UA). These serve different purposes and are reasonable to keep separate.

**Good patterns:**
- Engine is generic over StorageBackend (DeltaGliderEngine<S>)
- DynEngine type alias for trait objects provides good flexibility
- StoreContext parameter object avoids too_many_arguments
- TestServer builder pattern in tests/common/mod.rs is well-designed
- ConfigDb has in_memory() test constructor
- init.rs is testable (parameterized BufRead/Write)
- `interleave_and_paginate()` is the single source of truth for list pagination
- Clippy is fully clean with -D warnings, 182 unit tests pass
- `eprintln!`/`println!` usage in config.rs and main.rs is all intentional (pre-logger startup output)
- `ref_key` serde alias in types.rs is needed for backwards-compat with existing .meta files

**Why:** Understanding structural debt helps prioritize refactoring with maximum testability impact.
**How to apply:** Use this as a reference when planning refactoring work on main.rs or admin API.
