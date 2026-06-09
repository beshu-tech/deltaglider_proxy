// SPDX-License-Identifier: GPL-3.0-only

//! Persistent runtime state for lifecycle rules.
//!
//! Rules are YAML-owned. The config DB stores only scheduler state,
//! execution history, per-object failures, and per-rule leases.

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

    pub fn lifecycle_reconcile_on_boot(&self) -> Result<usize, ConfigDbError> {
        let now = crate::lifecycle::current_unix_seconds();
        let mut stmt = self.conn.prepare(
            "SELECT id, rule_name, started_at
             FROM lifecycle_run_history
             WHERE status = 'running'",
        )?;
        let rows: Vec<(i64, String, i64)> = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);

        let mut count = 0usize;
        for (id, rule_name, started_at) in &rows {
            self.conn.execute(
                "UPDATE lifecycle_run_history
                    SET status = 'failed', finished_at = ?
                  WHERE id = ?",
                params![now, id],
            )?;
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
            count += 1;
        }

        self.conn.execute(
            "UPDATE lifecycle_state
                SET leader_instance_id = NULL,
                    leader_expires_at  = NULL
              WHERE leader_expires_at IS NOT NULL AND leader_expires_at < ?",
            params![now],
        )?;

        Ok(count)
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
                        leader_instance_id: r.get(6)?,
                        leader_expires_at: r.get(7)?,
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
        let expires_at = now.saturating_add(ttl_secs.max(1));
        let n = self.conn.execute(
            "UPDATE lifecycle_state
                SET leader_instance_id = ?,
                    leader_expires_at  = ?
              WHERE rule_name = ?
                AND (
                    leader_instance_id IS NULL
                    OR leader_expires_at IS NULL
                    OR leader_expires_at <= ?
                )",
            params![owner, expires_at, rule_name, now],
        )?;
        Ok(n > 0)
    }

    pub fn lifecycle_release_lease(
        &self,
        rule_name: &str,
        owner: &str,
    ) -> Result<bool, ConfigDbError> {
        let n = self.conn.execute(
            "UPDATE lifecycle_state
                SET leader_instance_id = NULL,
                    leader_expires_at  = NULL
              WHERE rule_name = ?
                AND leader_instance_id = ?",
            params![rule_name, owner],
        )?;
        Ok(n > 0)
    }

    pub fn lifecycle_renew_lease(
        &self,
        rule_name: &str,
        owner: &str,
        now: i64,
        ttl_secs: i64,
    ) -> Result<bool, ConfigDbError> {
        let expires_at = now.saturating_add(ttl_secs.max(1));
        // `>=` (not `>`): a renewal landing exactly on the expiry instant still
        // belongs to the current owner, so it must succeed. Acquisition uses the
        // mirror-image `<=` boundary (an expired lease is up for grabs at `now`),
        // and this keeps the two sides from disagreeing about the boundary tick.
        let n = self.conn.execute(
            "UPDATE lifecycle_state
                SET leader_expires_at = ?
              WHERE rule_name = ?
                AND leader_instance_id = ?
                AND (
                    leader_expires_at IS NULL
                    OR leader_expires_at >= ?
                )",
            params![expires_at, rule_name, owner, now],
        )?;
        Ok(n > 0)
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
        self.conn.execute(
            "DELETE FROM lifecycle_failures
              WHERE rule_name = ?
                AND id NOT IN (
                    SELECT id FROM lifecycle_failures
                    WHERE rule_name = ?
                    ORDER BY occurred_at DESC
                    LIMIT ?
                )",
            params![rule_name, rule_name, max_retained],
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

        assert!(db
            .lifecycle_try_acquire_lease("r", "owner-b", 130, 30)
            .unwrap());
        let state = db.lifecycle_load_state("r").unwrap().unwrap();
        assert_eq!(state.leader_instance_id.as_deref(), Some("owner-b"));
        assert_eq!(state.leader_expires_at, Some(160));
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
