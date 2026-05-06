---
name: Replication module patterns
description: Structure of replication worker/scheduler/planner/state_store and common code patterns after hygiene pass
type: project
---

The replication subsystem lives in `src/replication/` with four modules:
- `planner.rs` — pure functions (normalize_prefix, rewrite_key, should_replicate, plan_batch)
- `state_store.rs` — ConfigDb methods for replication state (ensure/load/finish run, failures, leases)
- `worker.rs` — the run_rule entry point that orchestrates forward-copy + delete-pass
- `scheduler.rs` — periodic background loop calling run_rule for due rules

**Why:** The single entry point is now `run_rule(db, engine, rule, max_failures, triggered_by, lease)`. Two earlier telescoping wrappers (`run_rule_with_trigger`, legacy `run_rule`) were dead code and removed in the May 2026 hygiene pass.

**How to apply:** New features calling the worker should use `run_rule` directly with all 6 args. The `log_failure` helper in worker.rs handles lock→record→unlock for all failure recording sites.
