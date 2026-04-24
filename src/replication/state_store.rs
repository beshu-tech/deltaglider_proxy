//! Persistent state for replication — wraps the v6 SQLCipher tables:
//!
//! - `replication_state`: one row per rule. Current scheduling state,
//!   pause flag, leader lease, continuation token. Keyed by rule name.
//! - `replication_run_history`: one row per completed or in-progress
//!   run. Append-only; the worker inserts on begin, updates on finish.
//! - `replication_failures`: per-object errors. Ring-bounded per
//!   `record_failure`'s `max_retained`.
//!
//! Implemented as methods on [`ConfigDb`] so the SQLCipher mutex (held
//! at the type boundary) serialises all replication state mutations
//! alongside IAM mutations. Matches the pattern in
//! `src/config_db/users.rs`.

use crate::config_db::{ConfigDb, ConfigDbError};
use rusqlite::{params, OptionalExtension};

/// Scheduling + lifetime state for a single rule.
#[derive(Debug, Clone, PartialEq)]
pub struct ReplicationState {
    pub rule_name: String,
    pub last_run_at: Option<i64>,
    pub next_due_at: i64,
    pub last_status: String,
    pub objects_copied_lifetime: i64,
    pub bytes_copied_lifetime: i64,
    pub paused: bool,
    pub continuation_token: Option<String>,
    pub leader_instance_id: Option<String>,
    pub leader_expires_at: Option<i64>,
}

/// A completed or in-progress run entry.
#[derive(Debug, Clone, PartialEq)]
pub struct RunRecord {
    pub id: i64,
    pub rule_name: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub objects_scanned: i64,
    pub objects_copied: i64,
    pub objects_skipped: i64,
    pub objects_deleted: i64,
    pub bytes_copied: i64,
    pub errors: i64,
    pub status: String,
}

/// A per-object failure row.
#[derive(Debug, Clone, PartialEq)]
pub struct FailureRecord {
    pub id: i64,
    pub rule_name: String,
    pub occurred_at: i64,
    pub source_key: String,
    pub dest_key: String,
    pub error_message: String,
}

/// Totals emitted by the worker at run termination.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunTotals {
    pub objects_scanned: i64,
    pub objects_copied: i64,
    pub objects_skipped: i64,
    pub objects_deleted: i64,
    pub bytes_copied: i64,
    pub errors: i64,
}

pub fn current_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl ConfigDb {
    /// Upsert the initial state row for a rule. Idempotent: existing
    /// rows (including `paused` flag + lifetime counters) are preserved.
    pub fn replication_ensure_state(
        &self,
        rule_name: &str,
        now: i64,
    ) -> Result<(), ConfigDbError> {
        self.conn.execute(
            "INSERT OR IGNORE INTO replication_state
                (rule_name, next_due_at, last_status)
             VALUES (?, ?, 'idle')",
            params![rule_name, now],
        )?;
        Ok(())
    }

    /// Delete state rows for rules no longer present in YAML. Called
    /// after a successful apply to clean up orphaned runtime state.
    /// Returns the number of rows removed.
    pub fn replication_reconcile_rules(
        &self,
        known_rule_names: &[String],
    ) -> Result<usize, ConfigDbError> {
        let mut stmt = self.conn.prepare("SELECT rule_name FROM replication_state")?;
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
                    "DELETE FROM replication_state WHERE rule_name = ?",
                    params![existing],
                )?;
                removed += n;
            }
        }
        Ok(removed)
    }

    /// Boot-time zombie reconciliation: every run row left in `running`
    /// state must be from a previous process that died mid-run. Mark
    /// them `failed` and log a per-rule failure row for operator
    /// visibility. Returns the number of zombie runs reconciled. Also
    /// clears stale leader leases.
    pub fn replication_reconcile_on_boot(&self) -> Result<usize, ConfigDbError> {
        let now = current_unix_seconds();

        let mut stmt = self.conn.prepare(
            "SELECT id, rule_name, started_at
             FROM replication_run_history
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
                "UPDATE replication_run_history
                    SET status = 'failed', finished_at = ?
                  WHERE id = ?",
                params![now, id],
            )?;
            self.conn.execute(
                "INSERT INTO replication_failures
                    (rule_name, occurred_at, source_key, dest_key, error_message)
                 VALUES (?, ?, '', '', ?)",
                params![
                    rule_name,
                    now,
                    format!(
                        "proxy process died mid-run (was running since unix={})",
                        started_at
                    )
                ],
            )?;
            count += 1;
        }

        self.conn.execute(
            "UPDATE replication_state
                SET leader_instance_id = NULL,
                    leader_expires_at  = NULL
              WHERE leader_expires_at IS NOT NULL AND leader_expires_at < ?",
            params![now],
        )?;

        Ok(count)
    }

    /// Load the state row for a given rule.
    pub fn replication_load_state(
        &self,
        rule_name: &str,
    ) -> Result<Option<ReplicationState>, ConfigDbError> {
        let state = self
            .conn
            .query_row(
                "SELECT rule_name, last_run_at, next_due_at, last_status,
                        objects_copied_lifetime, bytes_copied_lifetime,
                        paused, continuation_token,
                        leader_instance_id, leader_expires_at
                 FROM replication_state WHERE rule_name = ?",
                params![rule_name],
                |r| {
                    Ok(ReplicationState {
                        rule_name: r.get(0)?,
                        last_run_at: r.get(1)?,
                        next_due_at: r.get(2)?,
                        last_status: r.get(3)?,
                        objects_copied_lifetime: r.get(4)?,
                        bytes_copied_lifetime: r.get(5)?,
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

    /// Set the paused flag for a rule. Returns `true` if the row existed.
    pub fn replication_set_paused(
        &self,
        rule_name: &str,
        paused: bool,
    ) -> Result<bool, ConfigDbError> {
        let n = self.conn.execute(
            "UPDATE replication_state SET paused = ? WHERE rule_name = ?",
            params![if paused { 1 } else { 0 }, rule_name],
        )?;
        Ok(n > 0)
    }

    /// Begin a new run. Returns the newly-assigned history row id.
    pub fn replication_begin_run(
        &self,
        rule_name: &str,
        started_at: i64,
    ) -> Result<i64, ConfigDbError> {
        self.conn.execute(
            "INSERT INTO replication_run_history (rule_name, started_at, status)
             VALUES (?, ?, 'running')",
            params![rule_name, started_at],
        )?;
        let id = self.conn.last_insert_rowid();

        self.conn.execute(
            "UPDATE replication_state SET last_status = 'running' WHERE rule_name = ?",
            params![rule_name],
        )?;
        Ok(id)
    }

    /// Finish a run with the given status + totals. Updates both the
    /// history row and the state row's lifetime counters.
    #[allow(clippy::too_many_arguments)]
    pub fn replication_finish_run(
        &self,
        run_id: i64,
        rule_name: &str,
        status: &str,
        finished_at: i64,
        totals: RunTotals,
        next_due_at: i64,
    ) -> Result<(), ConfigDbError> {
        self.conn.execute(
            "UPDATE replication_run_history
                SET finished_at     = ?,
                    objects_scanned = ?,
                    objects_copied  = ?,
                    objects_skipped = ?,
                    objects_deleted = ?,
                    bytes_copied    = ?,
                    errors          = ?,
                    status          = ?
              WHERE id = ?",
            params![
                finished_at,
                totals.objects_scanned,
                totals.objects_copied,
                totals.objects_skipped,
                totals.objects_deleted,
                totals.bytes_copied,
                totals.errors,
                status,
                run_id
            ],
        )?;
        self.conn.execute(
            "UPDATE replication_state
                SET last_run_at = ?,
                    last_status = ?,
                    next_due_at = ?,
                    objects_copied_lifetime = objects_copied_lifetime + ?,
                    bytes_copied_lifetime   = bytes_copied_lifetime + ?
              WHERE rule_name = ?",
            params![
                finished_at,
                status,
                next_due_at,
                totals.objects_copied,
                totals.bytes_copied,
                rule_name
            ],
        )?;
        Ok(())
    }

    /// Persist the continuation token for a rule.
    pub fn replication_set_continuation_token(
        &self,
        rule_name: &str,
        token: Option<&str>,
    ) -> Result<(), ConfigDbError> {
        self.conn.execute(
            "UPDATE replication_state SET continuation_token = ? WHERE rule_name = ?",
            params![token, rule_name],
        )?;
        Ok(())
    }

    /// Record a per-object failure, pruning oldest beyond `max_retained`.
    pub fn replication_record_failure(
        &self,
        rule_name: &str,
        occurred_at: i64,
        source_key: &str,
        dest_key: &str,
        error_message: &str,
        max_retained: u32,
    ) -> Result<(), ConfigDbError> {
        self.conn.execute(
            "INSERT INTO replication_failures
                (rule_name, occurred_at, source_key, dest_key, error_message)
             VALUES (?, ?, ?, ?, ?)",
            params![rule_name, occurred_at, source_key, dest_key, error_message],
        )?;
        self.conn.execute(
            "DELETE FROM replication_failures
              WHERE rule_name = ?
                AND id NOT IN (
                    SELECT id FROM replication_failures
                    WHERE rule_name = ?
                    ORDER BY occurred_at DESC
                    LIMIT ?
                )",
            params![rule_name, rule_name, max_retained],
        )?;
        Ok(())
    }

    /// Recent failures for a rule, newest-first, capped at `limit`.
    pub fn replication_recent_failures(
        &self,
        rule_name: &str,
        limit: u32,
    ) -> Result<Vec<FailureRecord>, ConfigDbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, rule_name, occurred_at, source_key, dest_key, error_message
             FROM replication_failures
             WHERE rule_name = ?
             ORDER BY occurred_at DESC
             LIMIT ?",
        )?;
        let rows = stmt
            .query_map(params![rule_name, limit], |r| {
                Ok(FailureRecord {
                    id: r.get(0)?,
                    rule_name: r.get(1)?,
                    occurred_at: r.get(2)?,
                    source_key: r.get(3)?,
                    dest_key: r.get(4)?,
                    error_message: r.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Recent run history for a rule, newest-first.
    pub fn replication_recent_runs(
        &self,
        rule_name: &str,
        limit: u32,
    ) -> Result<Vec<RunRecord>, ConfigDbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, rule_name, started_at, finished_at,
                    objects_scanned, objects_copied, objects_skipped, objects_deleted,
                    bytes_copied, errors, status
             FROM replication_run_history
             WHERE rule_name = ?
             ORDER BY started_at DESC
             LIMIT ?",
        )?;
        let rows = stmt
            .query_map(params![rule_name, limit], |r| {
                Ok(RunRecord {
                    id: r.get(0)?,
                    rule_name: r.get(1)?,
                    started_at: r.get(2)?,
                    finished_at: r.get(3)?,
                    objects_scanned: r.get(4)?,
                    objects_copied: r.get(5)?,
                    objects_skipped: r.get(6)?,
                    objects_deleted: r.get(7)?,
                    bytes_copied: r.get(8)?,
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
        db.replication_ensure_state("r1", 100).unwrap();
        db.replication_ensure_state("r1", 200).unwrap();
        let s = db.replication_load_state("r1").unwrap().unwrap();
        assert_eq!(s.next_due_at, 100);
        assert_eq!(s.last_status, "idle");
        assert!(!s.paused);
    }

    #[test]
    fn set_paused_round_trips() {
        let db = db();
        db.replication_ensure_state("r", 100).unwrap();
        assert!(db.replication_set_paused("r", true).unwrap());
        assert!(db.replication_load_state("r").unwrap().unwrap().paused);
        assert!(db.replication_set_paused("r", false).unwrap());
        assert!(!db.replication_load_state("r").unwrap().unwrap().paused);
    }

    #[test]
    fn set_paused_nonexistent_rule_returns_false() {
        let db = db();
        assert!(!db.replication_set_paused("ghost", true).unwrap());
    }

    #[test]
    fn begin_and_finish_run_update_both_tables() {
        let db = db();
        db.replication_ensure_state("r", 100).unwrap();
        let id = db.replication_begin_run("r", 200).unwrap();
        assert!(id > 0);

        let state_running = db.replication_load_state("r").unwrap().unwrap();
        assert_eq!(state_running.last_status, "running");

        let totals = RunTotals {
            objects_scanned: 10,
            objects_copied: 7,
            objects_skipped: 3,
            objects_deleted: 0,
            bytes_copied: 1024,
            errors: 0,
        };
        db.replication_finish_run(id, "r", "succeeded", 300, totals, 900)
            .unwrap();

        let state_done = db.replication_load_state("r").unwrap().unwrap();
        assert_eq!(state_done.last_status, "succeeded");
        assert_eq!(state_done.last_run_at, Some(300));
        assert_eq!(state_done.next_due_at, 900);
        assert_eq!(state_done.objects_copied_lifetime, 7);
        assert_eq!(state_done.bytes_copied_lifetime, 1024);

        let runs = db.replication_recent_runs("r", 10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].status, "succeeded");
        assert_eq!(runs[0].objects_copied, 7);
        assert_eq!(runs[0].bytes_copied, 1024);
    }

    #[test]
    fn record_failure_rings_by_max_retained() {
        let db = db();
        db.replication_ensure_state("r", 100).unwrap();
        for i in 0..5 {
            db.replication_record_failure("r", 100 + i, "src", "dst", &format!("err-{i}"), 3)
                .unwrap();
        }
        let recent = db.replication_recent_failures("r", 10).unwrap();
        assert_eq!(recent.len(), 3);
        let msgs: Vec<_> = recent.iter().map(|f| f.error_message.clone()).collect();
        assert_eq!(msgs, vec!["err-4", "err-3", "err-2"]);
    }

    #[test]
    fn reconcile_rules_removes_orphans() {
        let db = db();
        db.replication_ensure_state("r1", 100).unwrap();
        db.replication_ensure_state("r2", 100).unwrap();
        db.replication_ensure_state("r3", 100).unwrap();
        let removed = db
            .replication_reconcile_rules(&["r1".to_string(), "r2".to_string()])
            .unwrap();
        assert_eq!(removed, 1);
        assert!(db.replication_load_state("r3").unwrap().is_none());
    }

    #[test]
    fn reconcile_on_boot_flips_running_to_failed() {
        let db = db();
        db.replication_ensure_state("r", 100).unwrap();
        db.replication_begin_run("r", 200).unwrap();

        let count = db.replication_reconcile_on_boot().unwrap();
        assert_eq!(count, 1);

        let runs = db.replication_recent_runs("r", 10).unwrap();
        assert_eq!(runs[0].status, "failed");
        let fails = db.replication_recent_failures("r", 10).unwrap();
        assert_eq!(fails.len(), 1);
        assert!(fails[0].error_message.contains("died mid-run"));
    }

    #[test]
    fn continuation_token_round_trips() {
        let db = db();
        db.replication_ensure_state("r", 100).unwrap();
        db.replication_set_continuation_token("r", Some("cursor-xyz"))
            .unwrap();
        assert_eq!(
            db.replication_load_state("r")
                .unwrap()
                .unwrap()
                .continuation_token,
            Some("cursor-xyz".to_string())
        );
        db.replication_set_continuation_token("r", None).unwrap();
        assert!(db
            .replication_load_state("r")
            .unwrap()
            .unwrap()
            .continuation_token
            .is_none());
    }
}
