---
name: migrate-job-machinery
description: Migrate-as-job (kind=migrate) phase/crash-window hazards and the maintenance double-finish pattern
metadata:
  type: project
---

The migrate-as-job machinery (src/maintenance/migrate.rs, worker.rs) has two structural hazards worth re-checking on any change:

1. **Flip persist is warn-only → crash-window data-loss.** `ConfigMutator::mutate_and_apply` (src/config_apply.rs) treats `persist_to_file` failure as warn-only and returns Ok. The migrate `flip` phase mutates in-memory routing + advances DB phase to `cleanup`. If the file persist failed and the process crashes, boot reconcile resumes the `cleanup` phase from the DB while the config FILE still routes the live bucket to the SOURCE backend — and `delete_source` cleanup then deletes live data off source. There is NO routing re-verification on resume. **Why:** flip must persist-or-fail, or cleanup must re-verify the bucket actually routes to target before deleting. **How to apply:** if you touch flip/cleanup/mutate_and_apply, preserve a persist-or-fail invariant for the flip specifically.

2. **maintenance_finish has no status guard → double-finish clobbers notes.** `maintenance_finish` (src/maintenance/store.rs) updates `WHERE id = ?` unconditionally. The migrate cleanup path calls it with `last_error=Some(note)` then returns Ok; worker `run_job` then calls it AGAIN with `last_error=None`, NULLing the note. **How to apply:** any handler that pre-settles a job inside the phase fn will have its note overwritten by run_job's settle. Either make finish idempotent/guarded or don't pre-settle.

Other notes: flip-phase mutate failure leaks the `__dgmigrate_*` transient route until next boot reconcile (is_pre_flip("flip")==false, no unwind). Cleanup re-LIST loop is bounded by MAX_PAGES (10k) but churns the failure ring if deletes keep failing.
