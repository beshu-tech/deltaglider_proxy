---
name: job-pager
description: job_loop.rs::Pager canonical pagination state machine — semantics, equivalence deltas vs the old hand-rolled loops, and the cosmetic counter wrinkle on poison restart
metadata:
  type: project
---

`src/job_loop.rs::Pager` (added e3c6c29) is the single pagination state machine for replication, lifecycle, maintenance (reencrypt) and migrate worker loops. Owns ONLY: token threading, resume detection, poison-token guard, page cap (`MAX_JOB_PAGES=10_000`). Per-loop side effects (persist/heartbeat/fail) stay in callers.

Key semantics to remember when reviewing changes here:
- `advance(is_truncated, next)` returns `more = is_truncated && next.is_some()`; on `!more` it normalizes visible `token()` to `None`, so callers persisting `token()` clear their cursor for free on a complete pass.
- The pathological `(is_truncated=false, Some(token))` pairing is normalized to COMPLETE (anti-loop). The engine never emits it. This is the ONE intentional behavior delta:
  - replication OLD persisted Some(token) then broke (left a stale cursor → next tick would wrongly resume mid-bucket); NEW persists None. NEW is strictly more correct.
  - maintenance OLD exit was `if token.is_none()` (IGNORED is_truncated) → could loop forever on that pairing; NEW breaks. Improvement.
  - lifecycle OLD already used `!is_truncated ||` so it had NO delta here.
- Phase-scoped resume (migrate copy/verify, maintenance objects): guard is `if job.phase == "<phase>" { resume_token } else { None }`, keyed on the ORIGINAL `job.phase`, not the mutable local `phase`. Equivalent to OLD which nulled the single `token` var at every internal phase transition. phase+token persist atomically in `maintenance_update_progress` (single UPDATE in maintenance/store.rs), so a token can never belong to a different phase than persisted.
- `restart_fresh()` (maintenance/migrate poison self-heal only) resets `pages_started=0` → a poison restart gets a FULL fresh page budget (old single-budget behavior changed; acceptable). lifecycle + replication poison guards still just clear-cursor + break (no restart), matching pre-refactor.
- counting-phase poison restart writes `Some(0)` total — invisible in UI because `progress_percent` returns None for phase "counting" (and "stage"/"copy"/"verify"). Safe.

COSMETIC WRINKLE (benign, not a bug): objects/copy/verify poison restart preserves done/skipped/failed/bytes accumulators while re-scanning from page 0. Idempotent skips (needs_rewrite=false / HEAD-skip) re-increment `skipped`/`done` → counter inflation in the UI for that rare crash-then-token-invalidation case. No effect on correctness or the flip decision.

Cleanup sweep in migrate.rs (`'cleanup:`) is intentionally NOT on Pager (delete-then-restart-from-top model); it only adopted the `MAX_JOB_PAGES` cap. Stale comment at migrate.rs:~507 still says "MAX_PAGES times".
