// SPDX-License-Identifier: BUSL-1.1

//! Persistent runtime state for lifecycle rules.
//!
//! Rules are YAML-owned. The jobs DB stores only scheduler state,
//! execution history, per-object failures, and per-rule leases.

use crate::config_db::job_store;
use crate::config_db::{ConfigDbError, JobsDb};
use rusqlite::{params, OptionalExtension};

#[derive(Debug, Clone, PartialEq)]
pub struct LifecycleState {
    pub rule_name: String,
    pub last_run_at: Option<i64>,
    pub next_due_at: i64,
    pub last_status: String,
    pub objects_affected_lifetime: i64,
    pub bytes_affected_lifetime: i64,
    pub paused: bool,
    pub continuation_token: Option<String>,
    /// `bucket|prefix` the cursor was issued against. A token is only
    /// valid for the listing scope that produced it — if the operator
    /// redefines a same-named rule to a different bucket/prefix, replaying
    /// the old token would silently skip everything below it.
    pub cursor_scope: Option<String>,
    pub leader_instance_id: Option<String>,
    pub leader_expires_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LifecycleRunRecord {
    pub id: i64,
    pub rule_name: String,
    pub triggered_by: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub objects_scanned: i64,
    pub objects_affected: i64,
    pub objects_skipped: i64,
    pub bytes_affected: i64,
    pub errors: i64,
    pub status: String,
    /// The split of the run's counters (`lifecycle_run_counts`). `None` for
    /// a run that ended before the table existed, or that is still running.
    pub split: Option<LifecycleRunSplit>,
}

/// What a run did with the objects it acted on and why it skipped the
/// others. Stored next to the run-history row, in the jobs-only table
/// `lifecycle_run_counts`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LifecycleRunSplit {
    pub objects_deleted: i64,
    pub objects_moved: i64,
    pub objects_copied: i64,
    pub objects_excluded: i64,
    pub objects_not_due: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LifecycleFailureRecord {
    pub id: i64,
    pub rule_name: String,
    pub run_id: Option<i64>,
    pub occurred_at: i64,
    pub bucket: String,
    pub object_key: String,
    pub error_message: String,
}

pub struct LifecycleFailureInsert<'a> {
    pub run_id: Option<i64>,
    pub occurred_at: i64,
    pub bucket: &'a str,
    pub object_key: &'a str,
    pub error_message: &'a str,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LifecycleRunTotals {
    pub objects_scanned: i64,
    pub objects_affected: i64,
    pub objects_skipped: i64,
    pub bytes_affected: i64,
    pub errors: i64,
    pub objects_deleted: i64,
    pub objects_moved: i64,
    pub objects_copied: i64,
    pub objects_excluded: i64,
    pub objects_not_due: i64,
}

/// PURE: when a rule that has no state row yet first runs on schedule:
/// one tick after the proxy first sees it enabled (an apply, a boot, or a
/// scheduler check). A new rule never runs on the next check: the
/// operator has one tick to preview what it will do.
pub fn first_due_at(now: i64, tick_secs: i64) -> i64 {
    now.saturating_add(tick_secs.max(1))
}

impl JobsDb {
    /// Create the state row of a rule that has none, first due at
    /// `next_due_at` (see [`first_due_at`]). An existing row is unchanged.
    pub fn lifecycle_ensure_state(
        &self,
        rule_name: &str,
        next_due_at: i64,
    ) -> Result<(), ConfigDbError> {
        self.conn.execute(
            "INSERT OR IGNORE INTO lifecycle_state
                (rule_name, next_due_at, last_status)
             VALUES (?, ?, 'idle')",
            params![rule_name, next_due_at],
        )?;
        Ok(())
    }

    /// A resume after a pause that STOPPED a run: the run kept its cursor,
    /// so the rule is due now and the scheduler continues it at its next
    /// check. A rule paused while idle keeps its schedule.
    pub fn lifecycle_due_now_if_stopped_by_pause(
        &self,
        rule_name: &str,
        now: i64,
    ) -> Result<bool, ConfigDbError> {
        let n = self.conn.execute(
            "UPDATE lifecycle_state SET next_due_at = MIN(next_due_at, ?)
              WHERE rule_name = ? AND last_status = 'paused'",
            params![now, rule_name],
        )?;
        Ok(n > 0)
    }

    pub fn lifecycle_reconcile_rules(
        &self,
        known_rule_names: &[String],
    ) -> Result<usize, ConfigDbError> {
        let mut stmt = self.conn.prepare("SELECT rule_name FROM lifecycle_state")?;
        let rows: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);

        let known: std::collections::HashSet<&str> =
            known_rule_names.iter().map(|s| s.as_str()).collect();
        let mut removed = 0usize;
        // One transaction for all orphans' multi-table deletes (see replication twin).
        let tx = self.conn.unchecked_transaction()?;
        for existing in rows {
            if !known.contains(existing.as_str()) {
                let n = tx.execute(
                    "DELETE FROM lifecycle_state WHERE rule_name = ?",
                    params![existing],
                )?;
                // Run history + failure ring are keyed by rule_name too; drop
                // them so a deleted rule leaves no orphaned rows in the GUI.
                tx.execute(
                    "DELETE FROM lifecycle_run_counts WHERE run_id IN
                        (SELECT id FROM lifecycle_run_history WHERE rule_name = ?)",
                    params![existing],
                )?;
                tx.execute(
                    "DELETE FROM lifecycle_run_history WHERE rule_name = ?",
                    params![existing],
                )?;
                tx.execute(
                    "DELETE FROM lifecycle_failures WHERE rule_name = ?",
                    params![existing],
                )?;
                removed += n;
            }
        }
        tx.commit()?;
        Ok(removed)
    }

    /// Boot reconcile: zombie runs are marked failed (honest reporting),
    /// but the rule's `continuation_token` is deliberately PRESERVED — the
    /// next periodic tick resumes mid-bucket instead of re-listing from
    /// page 0. Correct because `expire_before` is recomputed at the resumed
    /// run's start (a later cutoff only ever adds candidates) and every
    /// plan decision is per-object.
    ///
    /// `max_failures_retained` is the same per-rule ring bound the normal
    /// `lifecycle_record_failure` path uses — after inserting a failure
    /// row per zombie the ring is pruned to it per affected rule, so a
    /// rapid-restart loop can't grow `lifecycle_failures` unbounded.
    pub fn lifecycle_reconcile_on_boot(
        &self,
        max_failures_retained: u32,
    ) -> Result<usize, ConfigDbError> {
        let now = crate::lifecycle::current_unix_seconds();
        // Boot: every local lease belongs to a dead process of this node —
        // lapse them all so a run whose holder died INSIDE its lease TTL
        // can't dodge the zombie scan (same hole as replication's).
        job_store::lapse_all_leases(&self.conn, "lifecycle_state")?;
        let zombies = job_store::find_zombie_runs(
            &self.conn,
            "lifecycle_run_history",
            "lifecycle_state",
            now,
        )?;
        // Collect the distinct affected rules so the ring is pruned once per
        // rule after the inserts (a rule with several zombie runs gets one prune).
        let mut affected_rules: Vec<String> = Vec::new();
        for (id, rule_name, started_at) in &zombies {
            job_store::mark_run_failed(&self.conn, "lifecycle_run_history", *id, now)?;
            self.conn.execute(
                "INSERT INTO lifecycle_failures
                    (rule_name, run_id, occurred_at, bucket, object_key, error_message)
                 VALUES (?, ?, ?, '', '', ?)",
                params![
                    rule_name,
                    id,
                    now,
                    format!(
                        "proxy process died mid-lifecycle-run (was running since unix={})",
                        started_at
                    )
                ],
            )?;
            self.conn.execute(
                "UPDATE lifecycle_state
                    SET last_status = 'failed',
                        last_run_at = ?
                  WHERE rule_name = ?",
                params![now, rule_name],
            )?;
            if !affected_rules.contains(rule_name) {
                affected_rules.push(rule_name.clone());
            }
        }
        for rule_name in &affected_rules {
            job_store::prune_failure_ring(
                &self.conn,
                "lifecycle_failures",
                "rule_name",
                rule_name,
                max_failures_retained,
            )?;
        }
        job_store::clear_stale_leases(&self.conn, "lifecycle_state", now)?;
        Ok(zombies.len())
    }

    pub fn lifecycle_load_state(
        &self,
        rule_name: &str,
    ) -> Result<Option<LifecycleState>, ConfigDbError> {
        let state = self
            .conn
            .query_row(
                "SELECT rule_name, last_run_at, next_due_at, last_status,
                        objects_affected_lifetime, bytes_affected_lifetime,
                        paused, continuation_token, cursor_scope,
                        leader_instance_id, leader_expires_at
                 FROM lifecycle_state WHERE rule_name = ?",
                params![rule_name],
                |r| {
                    Ok(LifecycleState {
                        rule_name: r.get(0)?,
                        last_run_at: r.get(1)?,
                        next_due_at: r.get(2)?,
                        last_status: r.get(3)?,
                        objects_affected_lifetime: r.get(4)?,
                        bytes_affected_lifetime: r.get(5)?,
                        paused: r.get::<_, i64>(6)? != 0,
                        continuation_token: r.get(7)?,
                        cursor_scope: r.get(8)?,
                        leader_instance_id: r.get(9)?,
                        leader_expires_at: r.get(10)?,
                    })
                },
            )
            .optional()?;
        Ok(state)
    }

    pub fn lifecycle_try_acquire_lease(
        &self,
        rule_name: &str,
        owner: &str,
        now: i64,
        ttl_secs: i64,
    ) -> Result<bool, ConfigDbError> {
        job_store::try_acquire_leader_lease(
            &self.conn,
            "lifecycle_state",
            "rule_name",
            &rule_name,
            owner,
            now,
            ttl_secs,
        )
    }

    /// True when SOME owner holds an unexpired lifecycle lease on the rule —
    /// the H2 delete-guard anchor for lifecycle (mirrors replication, #10).
    pub fn lifecycle_lease_is_held(
        &self,
        rule_name: &str,
        now: i64,
    ) -> Result<bool, ConfigDbError> {
        job_store::lease_is_held(&self.conn, "lifecycle_state", "rule_name", &rule_name, now)
    }

    pub fn lifecycle_release_lease(
        &self,
        rule_name: &str,
        owner: &str,
    ) -> Result<bool, ConfigDbError> {
        job_store::release_leader_lease(
            &self.conn,
            "lifecycle_state",
            "rule_name",
            &rule_name,
            owner,
        )
    }

    /// Persist the operator pause flag. Returns false for unknown rules.
    pub fn lifecycle_set_paused(
        &self,
        rule_name: &str,
        paused: bool,
    ) -> Result<bool, ConfigDbError> {
        let n = self.conn.execute(
            "UPDATE lifecycle_state SET paused = ? WHERE rule_name = ?",
            params![paused as i64, rule_name],
        )?;
        Ok(n > 0)
    }

    /// Persist the resumable LIST cursor (None = complete pass / reset),
    /// stamped with the `bucket|prefix` scope that produced it. Loaders
    /// must ignore (and clear) a token whose scope doesn't match the
    /// rule's CURRENT scope — see [`LifecycleState::cursor_scope`].
    pub fn lifecycle_set_continuation_token(
        &self,
        rule_name: &str,
        token: Option<&str>,
        scope: &str,
    ) -> Result<(), ConfigDbError> {
        self.conn.execute(
            "UPDATE lifecycle_state SET continuation_token = ?, cursor_scope = ?
              WHERE rule_name = ?",
            params![token, token.is_some().then_some(scope), rule_name],
        )?;
        Ok(())
    }

    pub fn lifecycle_renew_lease(
        &self,
        rule_name: &str,
        owner: &str,
        now: i64,
        ttl_secs: i64,
    ) -> Result<bool, ConfigDbError> {
        // Boundary semantics live in `config_db::job_store` (canonical for all
        // three subsystems): renewal at exactly the expiry instant succeeds;
        // a lapsed lease never resurrects.
        job_store::renew_leader_lease(
            &self.conn,
            "lifecycle_state",
            "rule_name",
            &rule_name,
            owner,
            now,
            ttl_secs,
        )
    }

    pub fn lifecycle_begin_run(
        &self,
        rule_name: &str,
        started_at: i64,
        triggered_by: &str,
    ) -> Result<i64, ConfigDbError> {
        self.conn.execute(
            "INSERT INTO lifecycle_run_history (rule_name, triggered_by, started_at, status)
             VALUES (?, ?, ?, 'running')",
            params![rule_name, triggered_by, started_at],
        )?;
        let id = self.conn.last_insert_rowid();
        self.conn.execute(
            "UPDATE lifecycle_state SET last_status = 'running' WHERE rule_name = ?",
            params![rule_name],
        )?;
        Ok(id)
    }

    pub fn lifecycle_finish_run(
        &self,
        run_id: i64,
        rule_name: &str,
        status: &str,
        finished_at: i64,
        totals: LifecycleRunTotals,
        next_due_at: i64,
    ) -> Result<(), ConfigDbError> {
        self.conn.execute(
            "UPDATE lifecycle_run_history
                SET finished_at     = ?,
                    objects_scanned = ?,
                    objects_affected = ?,
                    objects_skipped = ?,
                    bytes_affected   = ?,
                    errors          = ?,
                    status          = ?
              WHERE id = ?",
            params![
                finished_at,
                totals.objects_scanned,
                totals.objects_affected,
                totals.objects_skipped,
                totals.bytes_affected,
                totals.errors,
                status,
                run_id
            ],
        )?;
        self.conn.execute(
            "INSERT OR REPLACE INTO lifecycle_run_counts
                (run_id, objects_deleted, objects_moved, objects_copied,
                 objects_excluded, objects_not_due)
             VALUES (?, ?, ?, ?, ?, ?)",
            params![
                run_id,
                totals.objects_deleted,
                totals.objects_moved,
                totals.objects_copied,
                totals.objects_excluded,
                totals.objects_not_due
            ],
        )?;
        self.conn.execute(
            "UPDATE lifecycle_state
                SET last_run_at = ?,
                    last_status = ?,
                    next_due_at = ?,
                    objects_affected_lifetime = objects_affected_lifetime + ?,
                    bytes_affected_lifetime   = bytes_affected_lifetime + ?
              WHERE rule_name = ?",
            params![
                finished_at,
                status,
                next_due_at,
                totals.objects_affected,
                totals.bytes_affected,
                rule_name
            ],
        )?;
        Ok(())
    }

    pub fn lifecycle_record_failure(
        &self,
        rule_name: &str,
        failure: LifecycleFailureInsert<'_>,
        max_retained: u32,
    ) -> Result<(), ConfigDbError> {
        self.conn.execute(
            "INSERT INTO lifecycle_failures
                (rule_name, run_id, occurred_at, bucket, object_key, error_message)
             VALUES (?, ?, ?, ?, ?, ?)",
            params![
                rule_name,
                failure.run_id,
                failure.occurred_at,
                failure.bucket,
                failure.object_key,
                failure.error_message
            ],
        )?;
        job_store::prune_failure_ring(
            &self.conn,
            "lifecycle_failures",
            "rule_name",
            &rule_name,
            max_retained,
        )?;
        Ok(())
    }

    pub fn lifecycle_recent_failures(
        &self,
        rule_name: &str,
        limit: u32,
    ) -> Result<Vec<LifecycleFailureRecord>, ConfigDbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, rule_name, run_id, occurred_at, bucket, object_key, error_message
             FROM lifecycle_failures
             WHERE rule_name = ?
             ORDER BY occurred_at DESC, id DESC
             LIMIT ?",
        )?;
        let rows = stmt
            .query_map(params![rule_name, limit], |r| {
                Ok(LifecycleFailureRecord {
                    id: r.get(0)?,
                    rule_name: r.get(1)?,
                    run_id: r.get(2)?,
                    occurred_at: r.get(3)?,
                    bucket: r.get(4)?,
                    object_key: r.get(5)?,
                    error_message: r.get(6)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// How many failures the ring keeps for `rule_name` (the failures
    /// endpoint lists the most recent of them).
    pub fn lifecycle_failure_count(&self, rule_name: &str) -> Result<i64, ConfigDbError> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM lifecycle_failures WHERE rule_name = ?",
            params![rule_name],
            |r| r.get(0),
        )?)
    }

    /// The newest failure of the run `run_id`, if any.
    pub fn lifecycle_last_failure_of_run(
        &self,
        rule_name: &str,
        run_id: i64,
    ) -> Result<Option<LifecycleFailureRecord>, ConfigDbError> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, rule_name, run_id, occurred_at, bucket, object_key, error_message
                 FROM lifecycle_failures
                 WHERE rule_name = ? AND run_id = ?
                 ORDER BY occurred_at DESC, id DESC
                 LIMIT 1",
                params![rule_name, run_id],
                |r| {
                    Ok(LifecycleFailureRecord {
                        id: r.get(0)?,
                        rule_name: r.get(1)?,
                        run_id: r.get(2)?,
                        occurred_at: r.get(3)?,
                        bucket: r.get(4)?,
                        object_key: r.get(5)?,
                        error_message: r.get(6)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn lifecycle_recent_runs(
        &self,
        rule_name: &str,
        limit: u32,
    ) -> Result<Vec<LifecycleRunRecord>, ConfigDbError> {
        let mut stmt = self.conn.prepare(
            "SELECT h.id, h.rule_name, h.triggered_by, h.started_at, h.finished_at,
                    h.objects_scanned, h.objects_affected, h.objects_skipped,
                    h.bytes_affected, h.errors, h.status,
                    c.objects_deleted, c.objects_moved, c.objects_copied,
                    c.objects_excluded, c.objects_not_due
             FROM lifecycle_run_history h
             LEFT JOIN lifecycle_run_counts c ON c.run_id = h.id
             WHERE h.rule_name = ?
             ORDER BY h.started_at DESC, h.id DESC
             LIMIT ?",
        )?;
        let rows = stmt
            .query_map(params![rule_name, limit], |r| {
                Ok(LifecycleRunRecord {
                    id: r.get(0)?,
                    rule_name: r.get(1)?,
                    triggered_by: r.get(2)?,
                    started_at: r.get(3)?,
                    finished_at: r.get(4)?,
                    objects_scanned: r.get(5)?,
                    objects_affected: r.get(6)?,
                    objects_skipped: r.get(7)?,
                    bytes_affected: r.get(8)?,
                    errors: r.get(9)?,
                    status: r.get(10)?,
                    split: match r.get::<_, Option<i64>>(11)? {
                        None => None,
                        Some(objects_deleted) => Some(LifecycleRunSplit {
                            objects_deleted,
                            objects_moved: r.get(12)?,
                            objects_copied: r.get(13)?,
                            objects_excluded: r.get(14)?,
                            objects_not_due: r.get(15)?,
                        }),
                    },
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> JobsDb {
        JobsDb::in_memory("testpass").expect("open in-memory db")
    }

    #[test]
    fn paused_and_cursor_round_trip() {
        let db = db();
        db.lifecycle_ensure_state("r", 100).unwrap();
        let st = db.lifecycle_load_state("r").unwrap().unwrap();
        assert!(!st.paused);
        assert_eq!(st.continuation_token, None);

        assert!(db.lifecycle_set_paused("r", true).unwrap());
        db.lifecycle_set_continuation_token("r", Some("page-7"), "b|logs/")
            .unwrap();
        let st = db.lifecycle_load_state("r").unwrap().unwrap();
        assert!(st.paused);
        assert_eq!(st.continuation_token.as_deref(), Some("page-7"));
        assert_eq!(st.cursor_scope.as_deref(), Some("b|logs/"));

        assert!(db.lifecycle_set_paused("r", false).unwrap());
        db.lifecycle_set_continuation_token("r", None, "b|logs/")
            .unwrap();
        let st = db.lifecycle_load_state("r").unwrap().unwrap();
        assert!(!st.paused);
        assert_eq!(st.continuation_token, None);
        assert_eq!(st.cursor_scope, None, "cleared cursor carries no scope");
        // unknown rule → false
        assert!(!db.lifecycle_set_paused("ghost", true).unwrap());
    }

    #[test]
    fn reconcile_preserves_continuation_token() {
        let db = db();
        db.lifecycle_ensure_state("r", 100).unwrap();
        db.lifecycle_set_continuation_token("r", Some("page-3"), "b|")
            .unwrap();
        let run = db.lifecycle_begin_run("r", 10, "scheduler").unwrap();
        let n = db.lifecycle_reconcile_on_boot(100).unwrap();
        assert_eq!(n, 1);
        let runs = db.lifecycle_recent_runs("r", 5).unwrap();
        assert_eq!(runs[0].id, run);
        assert_eq!(runs[0].status, "failed");
        // The cursor survives — the next tick resumes mid-bucket.
        let st = db.lifecycle_load_state("r").unwrap().unwrap();
        assert_eq!(st.continuation_token.as_deref(), Some("page-3"));
    }

    #[test]
    fn ensure_state_row_is_idempotent() {
        let db = db();
        db.lifecycle_ensure_state("expire-old", 100).unwrap();
        db.lifecycle_ensure_state("expire-old", 200).unwrap();
        let state = db.lifecycle_load_state("expire-old").unwrap().unwrap();
        assert_eq!(state.next_due_at, 100);
        assert_eq!(state.last_status, "idle");
    }

    #[test]
    fn lease_acquire_rejects_second_owner_until_expiry() {
        let db = db();
        db.lifecycle_ensure_state("r", 100).unwrap();

        assert!(db
            .lifecycle_try_acquire_lease("r", "owner-a", 100, 30)
            .unwrap());
        assert!(!db
            .lifecycle_try_acquire_lease("r", "owner-b", 120, 30)
            .unwrap());

        // AT exact expiry the owner still wins (renew/steal predicates tile
        // — see config_db::job_store); strictly past it the steal succeeds.
        assert!(!db
            .lifecycle_try_acquire_lease("r", "owner-b", 130, 30)
            .unwrap());
        assert!(db
            .lifecycle_try_acquire_lease("r", "owner-b", 131, 30)
            .unwrap());
        let state = db.lifecycle_load_state("r").unwrap().unwrap();
        assert_eq!(state.leader_instance_id.as_deref(), Some("owner-b"));
        assert_eq!(state.leader_expires_at, Some(161));
    }

    #[test]
    fn lease_renewal_succeeds_on_expiry_boundary_but_rejects_past_it() {
        let db = db();
        db.lifecycle_ensure_state("r", 100).unwrap();

        assert!(db
            .lifecycle_try_acquire_lease("r", "owner-a", 100, 10)
            .unwrap());
        // expires_at == 110; renew a tick before expiry.
        assert!(db.lifecycle_renew_lease("r", "owner-a", 109, 10).unwrap());
        // expires_at == 119; renew *exactly* at the expiry instant still belongs
        // to the owner, so it must succeed (the `>=` boundary).
        assert!(db.lifecycle_renew_lease("r", "owner-a", 119, 10).unwrap());
        // expires_at == 129; one tick past expiry the lease is gone — reject.
        assert!(!db.lifecycle_renew_lease("r", "owner-a", 130, 10).unwrap());

        let state = db.lifecycle_load_state("r").unwrap().unwrap();
        assert_eq!(state.leader_instance_id.as_deref(), Some("owner-a"));
        assert_eq!(state.leader_expires_at, Some(129));
    }

    #[test]
    fn begin_finish_run_updates_history_and_state() {
        let db = db();
        db.lifecycle_ensure_state("r", 100).unwrap();
        let run_id = db.lifecycle_begin_run("r", 200, "run-now").unwrap();

        let running = db.lifecycle_load_state("r").unwrap().unwrap();
        assert_eq!(running.last_status, "running");

        db.lifecycle_finish_run(
            run_id,
            "r",
            "succeeded",
            300,
            LifecycleRunTotals {
                objects_scanned: 10,
                objects_affected: 2,
                objects_skipped: 8,
                bytes_affected: 1234,
                errors: 0,
                objects_deleted: 1,
                objects_moved: 1,
                objects_not_due: 5,
                objects_excluded: 3,
                ..LifecycleRunTotals::default()
            },
            900,
        )
        .unwrap();

        let done = db.lifecycle_load_state("r").unwrap().unwrap();
        assert_eq!(done.last_status, "succeeded");
        assert_eq!(done.last_run_at, Some(300));
        assert_eq!(done.next_due_at, 900);
        assert_eq!(done.objects_affected_lifetime, 2);
        assert_eq!(done.bytes_affected_lifetime, 1234);

        let runs = db.lifecycle_recent_runs("r", 10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].triggered_by, "run-now");
        assert_eq!(runs[0].objects_affected, 2);
        assert_eq!(
            runs[0].split,
            Some(LifecycleRunSplit {
                objects_deleted: 1,
                objects_moved: 1,
                objects_copied: 0,
                objects_excluded: 3,
                objects_not_due: 5,
            }),
            "the split is stored with the run"
        );
        // A running run has no split yet; a removed rule takes its splits.
        db.lifecycle_begin_run("r", 400, "scheduler").unwrap();
        assert_eq!(db.lifecycle_recent_runs("r", 1).unwrap()[0].split, None);
        db.lifecycle_reconcile_rules(&[]).unwrap();
        let left: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM lifecycle_run_counts", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(left, 0);
    }

    /// #6: a new rule first runs one tick after the proxy sees it, never on
    /// the next check.
    #[test]
    fn a_new_rule_is_first_due_one_tick_later() {
        assert_eq!(first_due_at(1_000, 3_600), 4_600);
        assert_eq!(first_due_at(1_000, 0), 1_001, "never due at once");
        let db = db();
        db.lifecycle_ensure_state("new", first_due_at(1_000, 3_600))
            .unwrap();
        assert_eq!(
            db.lifecycle_load_state("new").unwrap().unwrap().next_due_at,
            4_600
        );
    }

    /// #2: a resume continues a run that a pause stopped at once; a rule
    /// paused while idle keeps its schedule.
    #[test]
    fn a_resume_makes_a_pause_stopped_rule_due_now() {
        let db = db();
        db.lifecycle_ensure_state("stopped", 100).unwrap();
        let run = db.lifecycle_begin_run("stopped", 10, "run-now").unwrap();
        db.lifecycle_finish_run(
            run,
            "stopped",
            "paused",
            20,
            LifecycleRunTotals::default(),
            5_000,
        )
        .unwrap();
        assert!(db
            .lifecycle_due_now_if_stopped_by_pause("stopped", 30)
            .unwrap());
        assert_eq!(
            db.lifecycle_load_state("stopped")
                .unwrap()
                .unwrap()
                .next_due_at,
            30
        );
        db.lifecycle_ensure_state("idle", 5_000).unwrap();
        assert!(!db
            .lifecycle_due_now_if_stopped_by_pause("idle", 30)
            .unwrap());
        assert_eq!(
            db.lifecycle_load_state("idle")
                .unwrap()
                .unwrap()
                .next_due_at,
            5_000
        );
    }

    #[test]
    fn record_failure_rings_and_keeps_run_id() {
        let db = db();
        db.lifecycle_ensure_state("r", 100).unwrap();
        let run_id = db.lifecycle_begin_run("r", 200, "scheduler").unwrap();
        for i in 0..5 {
            db.lifecycle_record_failure(
                "r",
                LifecycleFailureInsert {
                    run_id: Some(run_id),
                    occurred_at: 100 + i,
                    bucket: "b",
                    object_key: &format!("k-{i}"),
                    error_message: &format!("err-{i}"),
                },
                3,
            )
            .unwrap();
        }

        let failures = db.lifecycle_recent_failures("r", 10).unwrap();
        assert_eq!(failures.len(), 3);
        assert!(failures.iter().all(|f| f.run_id == Some(run_id)));
        let keys: Vec<_> = failures.iter().map(|f| f.object_key.clone()).collect();
        assert_eq!(keys, vec!["k-4", "k-3", "k-2"]);
    }

    #[test]
    fn reconcile_rules_removes_orphans() {
        let db = db();
        db.lifecycle_ensure_state("r1", 100).unwrap();
        db.lifecycle_ensure_state("r2", 100).unwrap();

        let removed = db.lifecycle_reconcile_rules(&["r1".to_string()]).unwrap();
        assert_eq!(removed, 1);
        assert!(db.lifecycle_load_state("r2").unwrap().is_none());
    }

    #[test]
    fn reconcile_on_boot_flips_running_to_failed() {
        let db = db();
        db.lifecycle_ensure_state("r", 100).unwrap();
        db.lifecycle_begin_run("r", 200, "scheduler").unwrap();

        let count = db.lifecycle_reconcile_on_boot(100).unwrap();
        assert_eq!(count, 1);

        let runs = db.lifecycle_recent_runs("r", 10).unwrap();
        assert_eq!(runs[0].status, "failed");
        let failures = db.lifecycle_recent_failures("r", 10).unwrap();
        assert_eq!(failures.len(), 1);
        assert!(failures[0].error_message.contains("died mid-lifecycle-run"));
    }

    #[test]
    fn reconcile_on_boot_prunes_failure_ring_per_affected_rule() {
        // A rapid-restart loop inserts a failure row per zombie run but the
        // ring must stay bounded to `max_failures_retained` per rule — the
        // boot path now prunes, matching the normal record_failure path.
        let db = db();
        db.lifecycle_ensure_state("r", 100).unwrap();
        const MAX: u32 = 3;
        for _ in 0..(MAX as usize + 5) {
            db.lifecycle_begin_run("r", 200, "scheduler").unwrap();
        }

        let count = db.lifecycle_reconcile_on_boot(MAX).unwrap();
        assert_eq!(count, MAX as usize + 5);

        let failures = db.lifecycle_recent_failures("r", 100).unwrap();
        assert_eq!(
            failures.len(),
            MAX as usize,
            "boot reconcile must prune the failure ring to max_failures_retained"
        );
        assert!(failures
            .iter()
            .all(|f| f.error_message.contains("died mid-lifecycle-run")));
    }
}
