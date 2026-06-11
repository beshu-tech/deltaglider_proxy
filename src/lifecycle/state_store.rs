// SPDX-License-Identifier: GPL-3.0-only

//! Persistent runtime state for lifecycle rules.
//!
//! Rules are YAML-owned. The config DB stores only scheduler state,
//! execution history, per-object failures, and per-rule leases.

use crate::config_db::job_store;
use crate::config_db::{ConfigDb, ConfigDbError};
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
}

impl ConfigDb {
    pub fn lifecycle_ensure_state(&self, rule_name: &str, now: i64) -> Result<(), ConfigDbError> {
        self.conn.execute(
            "INSERT OR IGNORE INTO lifecycle_state
                (rule_name, next_due_at, last_status)
             VALUES (?, ?, 'idle')",
            params![rule_name, now],
        )?;
        Ok(())
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
        for existing in rows {
            if !known.contains(existing.as_str()) {
                let n = self.conn.execute(
                    "DELETE FROM lifecycle_state WHERE rule_name = ?",
                    params![existing],
                )?;
                removed += n;
            }
        }
        Ok(removed)
    }

    /// Boot reconcile: zombie runs are marked failed (honest reporting),
    /// but the rule's `continuation_token` is deliberately PRESERVED — the
    /// next periodic tick resumes mid-bucket instead of re-listing from
    /// page 0. Correct because `expire_before` is recomputed at the resumed
    /// run's start (a later cutoff only ever adds candidates) and every
    /// plan decision is per-object.
    pub fn lifecycle_reconcile_on_boot(&self) -> Result<usize, ConfigDbError> {
        let now = crate::lifecycle::current_unix_seconds();
        let zombies = job_store::find_zombie_runs(&self.conn, "lifecycle_run_history")?;
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
                        paused, continuation_token,
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
                        leader_instance_id: r.get(8)?,
                        leader_expires_at: r.get(9)?,
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

    /// Persist the resumable LIST cursor (None = complete pass / reset).
    pub fn lifecycle_set_continuation_token(
        &self,
        rule_name: &str,
        token: Option<&str>,
    ) -> Result<(), ConfigDbError> {
        self.conn.execute(
            "UPDATE lifecycle_state SET continuation_token = ? WHERE rule_name = ?",
            params![token, rule_name],
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

    pub fn lifecycle_recent_runs(
        &self,
        rule_name: &str,
        limit: u32,
    ) -> Result<Vec<LifecycleRunRecord>, ConfigDbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, rule_name, triggered_by, started_at, finished_at,
                    objects_scanned, objects_affected, objects_skipped,
                    bytes_affected, errors, status
             FROM lifecycle_run_history
             WHERE rule_name = ?
             ORDER BY started_at DESC, id DESC
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
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> ConfigDb {
        ConfigDb::in_memory("testpass").expect("open in-memory db")
    }

    #[test]
    fn paused_and_cursor_round_trip() {
        let db = db();
        db.lifecycle_ensure_state("r", 100).unwrap();
        let st = db.lifecycle_load_state("r").unwrap().unwrap();
        assert!(!st.paused);
        assert_eq!(st.continuation_token, None);

        assert!(db.lifecycle_set_paused("r", true).unwrap());
        db.lifecycle_set_continuation_token("r", Some("page-7"))
            .unwrap();
        let st = db.lifecycle_load_state("r").unwrap().unwrap();
        assert!(st.paused);
        assert_eq!(st.continuation_token.as_deref(), Some("page-7"));

        assert!(db.lifecycle_set_paused("r", false).unwrap());
        db.lifecycle_set_continuation_token("r", None).unwrap();
        let st = db.lifecycle_load_state("r").unwrap().unwrap();
        assert!(!st.paused);
        assert_eq!(st.continuation_token, None);
        // unknown rule → false
        assert!(!db.lifecycle_set_paused("ghost", true).unwrap());
    }

    #[test]
    fn reconcile_preserves_continuation_token() {
        let db = db();
        db.lifecycle_ensure_state("r", 100).unwrap();
        db.lifecycle_set_continuation_token("r", Some("page-3"))
            .unwrap();
        let run = db.lifecycle_begin_run("r", 10, "scheduler").unwrap();
        let n = db.lifecycle_reconcile_on_boot().unwrap();
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

        let count = db.lifecycle_reconcile_on_boot().unwrap();
        assert_eq!(count, 1);

        let runs = db.lifecycle_recent_runs("r", 10).unwrap();
        assert_eq!(runs[0].status, "failed");
        let failures = db.lifecycle_recent_failures("r", 10).unwrap();
        assert_eq!(failures.len(), 1);
        assert!(failures[0].error_message.contains("died mid-lifecycle-run"));
    }
}
