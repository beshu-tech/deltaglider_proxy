---
name: Architecture analysis - testability and modularity
description: Deep analysis of DeltaGlider Proxy architecture for testability and module decomposition opportunities (March 2026)
type: project
---

## Architecture Findings (updated 2026-03-27)

**Resolved issues (from prior analysis):**
- Duplicated `sanitize_audit` fn -- resolved, now centralized in `audit.rs`
- Duplicated `insert_permissions` / `insert_group_permissions` -- resolved, uses `insert_permission_rows` with table/fk params
- Triplicated interleave-sort-paginate for ListObjectsV2 -- resolved, now uses shared `interleave_and_paginate()` in engine.rs
- `eprintln!` in iam/permissions.rs evaluate_iam -- resolved, now uses `tracing::warn!`
- Inline IP extraction in api/auth.rs -- resolved, now uses `audit::extract_client_info()`

**Remaining structural observations:**
- main.rs orchestrates all initialization (806 lines), hard to test startup logic
- AdminState is a 12-field struct aggregating all infrastructure concerns
- DGP_SESSION_TTL_HOURS parsed independently in session.rs and api/admin/auth.rs
- Client IP extraction exists in two forms: `rate_limiter::extract_client_ip()` (returns `Option<IpAddr>`) and `audit::extract_client_info()` (returns `(String, String)` with UA). These serve different purposes (typed IP for rate limiting vs string for logging) and are reasonable to keep separate.

**Good patterns:**
- Engine is generic over StorageBackend (DeltaGliderEngine<S>)
- DynEngine type alias for trait objects provides good flexibility
- StoreContext parameter object avoids too_many_arguments
- TestServer builder pattern in tests/common/mod.rs is well-designed
- ConfigDb has in_memory() test constructor
- init.rs is testable (parameterized BufRead/Write)
- `interleave_and_paginate()` is the single source of truth for list pagination

**Why:** Understanding structural debt helps prioritize refactoring with maximum testability impact.
**How to apply:** Use this as a reference when planning refactoring work on main.rs or admin API.
