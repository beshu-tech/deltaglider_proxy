// SPDX-License-Identifier: GPL-3.0-only

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

use crate::config_db::job_store;
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
    pub triggered_by: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub objects_scanned: i64,
    pub objects_copied: i64,
    pub objects_skipped: i64,
    pub objects_deleted: i64,
    pub bytes_copied: i64,
    pub errors: i64,
    pub status: String,
    // Fast-path run stats (v17).
    pub delta_passthrough: i64,
    pub bytes_egress_saved: i64,
}

/// A per-object failure row.
#[derive(Debug, Clone, PartialEq)]
pub struct FailureRecord {
    pub id: i64,
    pub rule_name: String,
    pub run_id: Option<i64>,
    pub occurred_at: i64,
    pub source_key: String,
    pub dest_key: String,
    pub error_message: String,
}

pub struct FailureInsert<'a> {
    pub run_id: Option<i64>,
    pub occurred_at: i64,
    pub source_key: &'a str,
    pub dest_key: &'a str,
    pub error_message: &'a str,
}

/// One row of the consecutive-failure ledger (`replication_object_failures`).
/// Distinct from `FailureRecord` (the append-only run-history table).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectFailure {
    pub consecutive_failures: u32,
    pub last_error: String,
    pub last_failed_at: i64,
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
    // Fast-path run stats (v17). `delta_passthrough` = objects that shipped
    // their `.delta` verbatim; `bytes_egress_saved` = Σ(logical − delta). Other
    // strategy counts are derivable (objects_copied − delta_passthrough).
    pub delta_passthrough: i64,
    pub bytes_egress_saved: i64,
}

pub fn current_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

use std::sync::atomic::{AtomicU64, Ordering};

/// Monotonic counters bumped when a replication run settles (scheduled path) or
/// an event-driven drain advances its cursor (event path). Mirror
/// `PARITY_VERSION` / `IAM_VERSION` — let integration tests poll
/// `GET …/jobs/replication-run-version` / `…/replication-event-version` for a
/// deterministic barrier instead of sleeping. Process-local (one per proxy).
static REPLICATION_RUN_VERSION: AtomicU64 = AtomicU64::new(0);
static REPLICATION_EVENT_VERSION: AtomicU64 = AtomicU64::new(0);

// ponytail: REPLICATION_RUN_VERSION ships ahead of its first consumer — it's the
// convention-aligned scheduled-run barrier (mirrors PARITY_VERSION) for the next
// scheduled-run test that would otherwise reach for sleep. The event-driven sibling
// below already has a consumer (wait_for_replication_event).
pub fn bump_replication_run_version() -> u64 {
    REPLICATION_RUN_VERSION.fetch_add(1, Ordering::SeqCst) + 1
}
pub fn current_replication_run_version() -> u64 {
    REPLICATION_RUN_VERSION.load(Ordering::SeqCst)
}
pub fn bump_replication_event_version() -> u64 {
    REPLICATION_EVENT_VERSION.fetch_add(1, Ordering::SeqCst) + 1
}
pub fn current_replication_event_version() -> u64 {
    REPLICATION_EVENT_VERSION.load(Ordering::SeqCst)
}

impl ConfigDb {
    /// Upsert the initial state row for a rule. Idempotent: existing
    /// rows (including `paused` flag + lifetime counters) are preserved.
    pub fn replication_ensure_state(&self, rule_name: &str, now: i64) -> Result<(), ConfigDbError> {
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
        let mut stmt = self
            .conn
            .prepare("SELECT rule_name FROM replication_state")?;
        let rows: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);

        let known: std::collections::HashSet<&str> =
            known_rule_names.iter().map(|s| s.as_str()).collect();
        let mut removed = 0usize;
        // One transaction for all orphans' multi-table deletes: a crash mid-purge
        // must not leak the run_history/failures rows this is meant to clear.
        let tx = self.conn.unchecked_transaction()?;
        for existing in rows {
            if !known.contains(existing.as_str()) {
                let n = tx.execute(
                    "DELETE FROM replication_state WHERE rule_name = ?",
                    params![existing],
                )?;
                // Drop the rule's parity object-cache AND result row, run history,
                // and failure ring too — all keyed by rule_name, else orphaned.
                tx.execute(
                    "DELETE FROM replication_parity_objects WHERE rule_name = ?",
                    params![existing],
                )?;
                tx.execute(
                    "DELETE FROM replication_parity WHERE rule_name = ?",
                    params![existing],
                )?;
                tx.execute(
                    "DELETE FROM replication_run_history WHERE rule_name = ?",
                    params![existing],
                )?;
                tx.execute(
                    "DELETE FROM replication_failures WHERE rule_name = ?",
                    params![existing],
                )?;
                removed += n;
            }
        }
        tx.commit()?;
        Ok(removed)
    }

    /// Boot-time zombie reconciliation: every run row left in `running`
    /// state must be from a previous process that died mid-run. Mark
    /// them `failed` and log a per-rule failure row for operator
    /// visibility. Returns the number of zombie runs reconciled. Also
    /// clears stale leader leases.
    ///
    /// `max_failures_retained` is the same per-rule ring bound the normal
    /// `replication_record_failure` path uses — after inserting a failure
    /// row per zombie the ring is pruned to it per affected rule, so a
    /// rapid-restart loop can't grow `replication_failures` unbounded
    /// (only the normal failure-insert path pruned before).
    pub fn replication_reconcile_on_boot(
        &self,
        max_failures_retained: u32,
    ) -> Result<usize, ConfigDbError> {
        let now = current_unix_seconds();
        let zombies = job_store::find_zombie_runs(
            &self.conn,
            "replication_run_history",
            "replication_state",
            now,
        )?;
        // Collect the distinct affected rules so the ring is pruned once per
        // rule after the inserts (a rule with several zombie runs gets one prune,
        // not one per row).
        let mut affected_rules: Vec<String> = Vec::new();
        for (id, rule_name, started_at) in &zombies {
            job_store::mark_run_failed(&self.conn, "replication_run_history", *id, now)?;
            self.conn.execute(
                "INSERT INTO replication_failures
                    (rule_name, run_id, occurred_at, source_key, dest_key, error_message)
                 VALUES (?, ?, ?, '', '', ?)",
                params![
                    rule_name,
                    id,
                    now,
                    format!(
                        "proxy process died mid-run (was running since unix={})",
                        started_at
                    )
                ],
            )?;
            // Stamp the per-rule state row too, so the admin UI doesn't keep
            // reporting the rule as "running" for up to a full schedule interval
            // after the process actually died. (Lifecycle's twin already does
            // this; this closes the asymmetry.)
            self.conn.execute(
                "UPDATE replication_state
                    SET last_status = 'failed', last_run_at = ?
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
                "replication_failures",
                "rule_name",
                rule_name,
                max_failures_retained,
            )?;
        }
        job_store::clear_stale_leases(&self.conn, "replication_state", now)?;
        Ok(zombies.len())
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

    /// Try to acquire the per-rule run lease.
    ///
    /// This is the single-flight guard shared by the periodic scheduler
    /// and the admin "run now" endpoint. The caller must already have
    /// ensured the state row exists. Existing unexpired leases win; expired
    /// leases are stolen by the new owner.
    pub fn replication_try_acquire_lease(
        &self,
        rule_name: &str,
        owner: &str,
        now: i64,
        ttl_secs: i64,
    ) -> Result<bool, ConfigDbError> {
        job_store::try_acquire_leader_lease(
            &self.conn,
            "replication_state",
            "rule_name",
            &rule_name,
            owner,
            now,
            ttl_secs,
        )
    }

    /// Release the per-rule run lease if this owner still holds it.
    ///
    /// Returns false if the lease had expired and another owner acquired it,
    /// or if the rule row disappeared.
    pub fn replication_release_lease(
        &self,
        rule_name: &str,
        owner: &str,
    ) -> Result<bool, ConfigDbError> {
        job_store::release_leader_lease(
            &self.conn,
            "replication_state",
            "rule_name",
            &rule_name,
            owner,
        )
    }

    /// Extend the per-rule run lease if this owner still holds it.
    ///
    /// Returns false when another process has stolen/expired the lease or the
    /// rule row disappeared. Callers should stop work before starting another
    /// object/page when renewal fails. Canonical semantics (see
    /// `config_db::job_store`): a lease that has ALREADY lapsed cannot be
    /// renewed — the lapsed worker must stop rather than resurrect a lease
    /// another instance may have stolen.
    pub fn replication_renew_lease(
        &self,
        rule_name: &str,
        owner: &str,
        now: i64,
        ttl_secs: i64,
    ) -> Result<bool, ConfigDbError> {
        job_store::renew_leader_lease(
            &self.conn,
            "replication_state",
            "rule_name",
            &rule_name,
            owner,
            now,
            ttl_secs,
        )
    }

    /// True when ANY owner holds an unexpired lease on the rule — the liveness
    /// anchor for delete-vs-run: a lease is taken BEFORE the run row exists,
    /// so a run-row check alone races the acquire-then-spawn gap.
    pub fn replication_lease_is_held(
        &self,
        rule_name: &str,
        now: i64,
    ) -> Result<bool, ConfigDbError> {
        job_store::lease_is_held(
            &self.conn,
            "replication_state",
            "rule_name",
            &rule_name,
            now,
        )
    }

    /// Begin a new run. Returns the newly-assigned history row id.
    pub fn replication_begin_run(
        &self,
        rule_name: &str,
        started_at: i64,
        triggered_by: &str,
    ) -> Result<i64, ConfigDbError> {
        self.conn.execute(
            "INSERT INTO replication_run_history (rule_name, triggered_by, started_at, status)
             VALUES (?, ?, ?, 'running')",
            params![rule_name, triggered_by, started_at],
        )?;
        let id = self.conn.last_insert_rowid();

        self.conn.execute(
            "UPDATE replication_state SET last_status = 'running' WHERE rule_name = ?",
            params![rule_name],
        )?;
        Ok(id)
    }

    /// Finish a run with the given status + totals. Updates both the
    /// history row and the state row's lifetime counters. The UPDATE is
    /// guarded on a non-terminal status so a late settle can never overwrite
    /// an already-terminal row (H1 shape); returns false when it was skipped.
    #[allow(clippy::too_many_arguments)]
    pub fn replication_finish_run(
        &self,
        run_id: i64,
        rule_name: &str,
        status: &str,
        finished_at: i64,
        totals: RunTotals,
        next_due_at: i64,
    ) -> Result<bool, ConfigDbError> {
        let changed = self.conn.execute(
            "UPDATE replication_run_history
                SET finished_at        = ?,
                    objects_scanned    = ?,
                    objects_copied     = ?,
                    objects_skipped    = ?,
                    objects_deleted    = ?,
                    bytes_copied       = ?,
                    errors             = ?,
                    status             = ?,
                    delta_passthrough  = ?,
                    bytes_egress_saved = ?
              WHERE id = ? AND status IN ('running', 'cancelling')",
            params![
                finished_at,
                totals.objects_scanned,
                totals.objects_copied,
                totals.objects_skipped,
                totals.objects_deleted,
                totals.bytes_copied,
                totals.errors,
                status,
                totals.delta_passthrough,
                totals.bytes_egress_saved,
                run_id
            ],
        )?;
        if changed == 0 {
            return Ok(false);
        }
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
        Ok(true)
    }

    /// Update the in-progress counters for a running replication row.
    ///
    /// Used by long scheduler runs so the admin UI can show live
    /// progress instead of `0/0` until the run finishes.
    pub fn replication_update_run_progress(
        &self,
        run_id: i64,
        totals: RunTotals,
    ) -> Result<(), ConfigDbError> {
        self.conn.execute(
            "UPDATE replication_run_history
                SET objects_scanned    = ?,
                    objects_copied     = ?,
                    objects_skipped    = ?,
                    objects_deleted    = ?,
                    bytes_copied       = ?,
                    errors             = ?,
                    delta_passthrough  = ?,
                    bytes_egress_saved = ?
              WHERE id = ?
                AND status = 'running'",
            params![
                totals.objects_scanned,
                totals.objects_copied,
                totals.objects_skipped,
                totals.objects_deleted,
                totals.bytes_copied,
                totals.errors,
                totals.delta_passthrough,
                totals.bytes_egress_saved,
                run_id,
            ],
        )?;
        Ok(())
    }

    /// Operator KILL: flip a rule's RUNNING run to `cancelling`. The worker
    /// polls this between objects (and aborts in-flight copies), then settles
    /// the run to `cancelled`. Returns true if a running run was flipped.
    pub fn replication_request_run_cancel(&self, rule_name: &str) -> Result<bool, ConfigDbError> {
        let n = self.conn.execute(
            "UPDATE replication_run_history SET status = 'cancelling'
              WHERE rule_name = ? AND status = 'running'",
            params![rule_name],
        )?;
        Ok(n > 0)
    }

    /// Status of the most recent run-history row for a rule, if any. The jobs
    /// list uses this to surface a `cancelling` run-in-flight that hasn't settled
    /// `replication_state.last_status` yet — otherwise a killed run keeps showing
    /// as plain `running` until the worker reaches a cancel checkpoint.
    pub fn replication_latest_run_status(
        &self,
        rule_name: &str,
    ) -> Result<Option<String>, ConfigDbError> {
        let v: Option<String> = self
            .conn
            .query_row(
                "SELECT status FROM replication_run_history
                  WHERE rule_name = ? ORDER BY id DESC LIMIT 1",
                params![rule_name],
                |r| r.get(0),
            )
            .optional()?;
        Ok(v)
    }

    /// Whether a kill has been requested for this run (status == 'cancelling').
    pub fn replication_run_cancel_requested(&self, run_id: i64) -> Result<bool, ConfigDbError> {
        let v: Option<String> = self
            .conn
            .query_row(
                "SELECT status FROM replication_run_history WHERE id = ?",
                params![run_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(matches!(v.as_deref(), Some("cancelling")))
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
        failure: FailureInsert<'_>,
        max_retained: u32,
    ) -> Result<(), ConfigDbError> {
        self.conn.execute(
            "INSERT INTO replication_failures
                (rule_name, run_id, occurred_at, source_key, dest_key, error_message)
             VALUES (?, ?, ?, ?, ?, ?)",
            params![
                rule_name,
                failure.run_id,
                failure.occurred_at,
                failure.source_key,
                failure.dest_key,
                failure.error_message
            ],
        )?;
        job_store::prune_failure_ring(
            &self.conn,
            "replication_failures",
            "rule_name",
            &rule_name,
            max_retained,
        )?;
        Ok(())
    }

    /// Record one consecutive failure for a single (rule, source_key).
    /// Upserts, incrementing the consecutive counter, and returns the new
    /// count. Cleared by `replication_clear_object_failure` on any success.
    pub fn replication_record_object_failure(
        &self,
        rule: &str,
        key: &str,
        err: &str,
        now: i64,
    ) -> Result<u32, ConfigDbError> {
        self.conn.execute(
            "INSERT INTO replication_object_failures
                (rule_name, source_key, consecutive_failures, last_error, last_failed_at)
             VALUES (?, ?, 1, ?, ?)
             ON CONFLICT(rule_name, source_key) DO UPDATE SET
                consecutive_failures = consecutive_failures + 1,
                last_error           = excluded.last_error,
                last_failed_at       = excluded.last_failed_at",
            params![rule, key, err, now],
        )?;
        let count: i64 = self.conn.query_row(
            "SELECT consecutive_failures FROM replication_object_failures
             WHERE rule_name = ? AND source_key = ?",
            params![rule, key],
            |r| r.get(0),
        )?;
        Ok(count.max(0) as u32)
    }

    /// Clear the consecutive-failure ledger row for a (rule, source_key).
    /// Called on a successful copy so a transient blip can't permanently
    /// skip the object.
    pub fn replication_clear_object_failure(
        &self,
        rule: &str,
        key: &str,
    ) -> Result<(), ConfigDbError> {
        self.conn.execute(
            "DELETE FROM replication_object_failures
             WHERE rule_name = ? AND source_key = ?",
            params![rule, key],
        )?;
        Ok(())
    }

    /// True when this (rule, source_key) has reached the skip threshold:
    /// `consecutive_failures >= threshold AND threshold > 0` (0 = never skip).
    pub fn replication_object_skipped(
        &self,
        rule: &str,
        key: &str,
        threshold: u32,
    ) -> Result<bool, ConfigDbError> {
        if threshold == 0 {
            return Ok(false);
        }
        let count: i64 = self
            .conn
            .query_row(
                "SELECT consecutive_failures FROM replication_object_failures
                 WHERE rule_name = ? AND source_key = ?",
                params![rule, key],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        Ok(count >= threshold as i64)
    }

    /// Bounded join over the per-object failure ledger for a fixed set of
    /// SAMPLED keys (≤300 from a parity audit). Chunks the `IN (…)` list at
    /// 500 params; empty `keys` returns an empty map WITHOUT touching the DB.
    pub fn replication_object_failures_for_keys(
        &self,
        rule: &str,
        keys: &[&str],
    ) -> Result<std::collections::HashMap<String, ObjectFailure>, ConfigDbError> {
        let mut out = std::collections::HashMap::new();
        if keys.is_empty() {
            return Ok(out);
        }
        for chunk in keys.chunks(500) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let sql = format!(
                "SELECT source_key, consecutive_failures, last_error, last_failed_at
                 FROM replication_object_failures
                 WHERE rule_name = ? AND source_key IN ({placeholders})"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let mut binds: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(chunk.len() + 1);
            binds.push(&rule);
            for k in chunk {
                binds.push(k);
            }
            let rows = stmt.query_map(binds.as_slice(), |r| {
                let key: String = r.get(0)?;
                let count: i64 = r.get(1)?;
                Ok((
                    key,
                    ObjectFailure {
                        consecutive_failures: count.max(0) as u32,
                        last_error: r.get(2)?,
                        last_failed_at: r.get(3)?,
                    },
                ))
            })?;
            for row in rows {
                let (k, v) = row?;
                out.insert(k, v);
            }
        }
        Ok(out)
    }

    /// Recent failures for a rule, newest-first, capped at `limit`.
    pub fn replication_recent_failures(
        &self,
        rule_name: &str,
        limit: u32,
    ) -> Result<Vec<FailureRecord>, ConfigDbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, rule_name, run_id, occurred_at, source_key, dest_key, error_message
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
                    run_id: r.get(2)?,
                    occurred_at: r.get(3)?,
                    source_key: r.get(4)?,
                    dest_key: r.get(5)?,
                    error_message: r.get(6)?,
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
            "SELECT id, rule_name, triggered_by, started_at, finished_at,
                    objects_scanned, objects_copied, objects_skipped, objects_deleted,
                    bytes_copied, errors, status,
                    delta_passthrough, bytes_egress_saved
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
                    triggered_by: r.get(2)?,
                    started_at: r.get(3)?,
                    finished_at: r.get(4)?,
                    objects_scanned: r.get(5)?,
                    objects_copied: r.get(6)?,
                    objects_skipped: r.get(7)?,
                    objects_deleted: r.get(8)?,
                    bytes_copied: r.get(9)?,
                    errors: r.get(10)?,
                    status: r.get(11)?,
                    delta_passthrough: r.get(12)?,
                    bytes_egress_saved: r.get(13)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ── Parity per-object logical-metadata cache (v18) ────────────────────────
    // The expensive part of a parity audit is recovering each DELTA object's
    // LOGICAL (sha256, size, etag) — only a per-object HEAD gives it, since the
    // lite LIST carries the delta-blob size/etag. This cache stores that logical
    // metadata keyed by the DEST-namespace key so a re-verify (and the first
    // verify of anything the replication worker copied) is HEAD-free.

    /// Bulk-lookup cached logical metadata for `dest_keys` under a rule.
    /// Returns only the keys present in the cache (a miss → HEAD needed).
    pub fn parity_cache_get_many(
        &self,
        rule: &str,
        side: ParitySide,
        dest_keys: &[&str],
    ) -> Result<std::collections::HashMap<String, ParityCacheEntry>, ConfigDbError> {
        let mut out = std::collections::HashMap::new();
        if dest_keys.is_empty() {
            return Ok(out);
        }
        let side = side.as_str();
        for chunk in dest_keys.chunks(500) {
            let placeholders = vec!["?"; chunk.len()].join(",");
            let sql = format!(
                "SELECT dest_key, sha256, size, etag, stored_etag
                 FROM replication_parity_objects
                 WHERE rule_name = ? AND side = ? AND dest_key IN ({placeholders})"
            );
            let mut stmt = self.conn.prepare(&sql)?;
            let mut binds: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(chunk.len() + 2);
            binds.push(&rule);
            binds.push(&side);
            for k in chunk {
                binds.push(k);
            }
            let rows = stmt.query_map(binds.as_slice(), |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    ParityCacheEntry {
                        sha256: r.get(1)?,
                        size: r.get::<_, i64>(2)? as u64,
                        etag: r.get(3)?,
                        stored_etag: r.get(4)?,
                    },
                ))
            })?;
            for row in rows {
                let (k, v) = row?;
                out.insert(k, v);
            }
        }
        Ok(out)
    }

    /// Batch-upsert logical metadata for objects on ONE side of a rule (one
    /// transaction). Source and dest are SEPARATE rows (the `side` discriminator)
    /// — they must never share a cache row even when keys coincide (whole-bucket
    /// mirror where source.prefix == dest.prefix), or a source read would get the
    /// dest's metadata and vice versa (a structural false "in sync").
    pub fn parity_cache_put_many(
        &mut self,
        rule: &str,
        side: ParitySide,
        entries: &[(String, ParityCacheEntry)],
        now: i64,
    ) -> Result<(), ConfigDbError> {
        if entries.is_empty() {
            return Ok(());
        }
        let side = side.as_str();
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO replication_parity_objects
                    (rule_name, side, dest_key, sha256, size, etag, stored_etag, updated_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(rule_name, side, dest_key) DO UPDATE SET
                    sha256 = excluded.sha256,
                    size = excluded.size,
                    etag = excluded.etag,
                    stored_etag = excluded.stored_etag,
                    updated_at = excluded.updated_at",
            )?;
            for (key, e) in entries {
                stmt.execute(rusqlite::params![
                    rule,
                    side,
                    key,
                    e.sha256,
                    e.size as i64,
                    e.etag,
                    e.stored_etag,
                    now
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Drop the entire parity cache for a rule (both sides). Called on rule
    /// removal (orphan cleanup).
    pub fn parity_cache_clear(&self, rule: &str) -> Result<(), ConfigDbError> {
        self.conn.execute(
            "DELETE FROM replication_parity_objects WHERE rule_name = ?",
            [rule],
        )?;
        Ok(())
    }

    /// Prune cache rows for a rule whose `dest_key` was NOT seen in the last
    /// (complete) scan — bounds growth to the live object set and evicts
    /// stale rows for deleted objects. Called only after a NON-truncated scan
    /// (a truncated scan didn't see every key, so it can't safely prune).
    /// `ponytail`: local diagnostic cache — keep it bounded; a future move to a
    /// separate local-only DB would also keep it out of the HA config-sync blob.
    pub fn parity_cache_retain(
        &mut self,
        rule: &str,
        side: ParitySide,
        live_dest_keys: &[String],
    ) -> Result<usize, ConfigDbError> {
        let side = side.as_str();
        // Build a temp set of live keys, delete everything else for (rule, side).
        let tx = self.conn.transaction()?;
        tx.execute_batch(
            "CREATE TEMP TABLE IF NOT EXISTS _parity_live (k TEXT PRIMARY KEY);
             DELETE FROM _parity_live;",
        )?;
        {
            let mut ins = tx.prepare("INSERT OR IGNORE INTO _parity_live (k) VALUES (?)")?;
            for k in live_dest_keys {
                ins.execute([k])?;
            }
        }
        let removed = tx.execute(
            "DELETE FROM replication_parity_objects
             WHERE rule_name = ? AND side = ?
               AND dest_key NOT IN (SELECT k FROM _parity_live)",
            rusqlite::params![rule, side],
        )?;
        tx.commit()?;
        Ok(removed)
    }

    // ── Parity RESULT cache + background-job lease (v19) ──────────────────────

    /// Load the cached parity result row for a rule (status + last outcome).
    pub fn parity_result_load(&self, rule: &str) -> Result<Option<ParityResultRow>, ConfigDbError> {
        let row = self
            .conn
            .query_row(
                "SELECT status, scanned_at, progress_scanned, outcome_json, last_error
                 FROM replication_parity WHERE rule_name = ?",
                [rule],
                |r| {
                    Ok(ParityResultRow {
                        status: r.get(0)?,
                        scanned_at: r.get(1)?,
                        progress_scanned: r.get(2)?,
                        outcome_json: r.get(3)?,
                        last_error: r.get(4)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// Mark a rule's parity audit as RUNNING (clears the prior outcome's error,
    /// keeps the last outcome_json visible while the new scan runs).
    pub fn parity_result_set_running(&self, rule: &str, now: i64) -> Result<(), ConfigDbError> {
        self.conn.execute(
            "INSERT INTO replication_parity (rule_name, status, progress_scanned, updated_at)
             VALUES (?, 'running', 0, ?)
             ON CONFLICT(rule_name) DO UPDATE SET
                status = 'running', progress_scanned = 0, last_error = NULL, updated_at = ?",
            params![rule, now, now],
        )?;
        Ok(())
    }

    /// Update the live progress counter of a running parity audit.
    /// `ponytail`: the audit doesn't yet call this per-page — the scan is now
    /// metadata-only + cache-warm (seconds), so the UI shows a spinner, not a
    /// live count. This is the hook for a future streaming-progress pass; the UI
    /// already degrades gracefully (it only renders the count when > 0).
    pub fn parity_result_progress(
        &self,
        rule: &str,
        scanned: i64,
        now: i64,
    ) -> Result<(), ConfigDbError> {
        self.conn.execute(
            "UPDATE replication_parity SET progress_scanned = ?, updated_at = ?
             WHERE rule_name = ?",
            params![scanned, now, rule],
        )?;
        Ok(())
    }

    /// Persist a COMPLETED parity audit (the serialized outcome + verdict).
    pub fn parity_result_done(
        &self,
        rule: &str,
        in_sync: bool,
        outcome_json: &str,
        now: i64,
    ) -> Result<(), ConfigDbError> {
        self.conn.execute(
            "INSERT INTO replication_parity
                (rule_name, status, scanned_at, in_sync, outcome_json, last_error, updated_at)
             VALUES (?, 'done', ?, ?, ?, NULL, ?)
             ON CONFLICT(rule_name) DO UPDATE SET
                status = 'done', scanned_at = ?, in_sync = ?,
                outcome_json = ?, last_error = NULL, updated_at = ?",
            params![
                rule,
                now,
                in_sync as i64,
                outcome_json,
                now,
                now,
                in_sync as i64,
                outcome_json,
                now
            ],
        )?;
        Ok(())
    }

    /// Mark a parity audit FAILED with the error message.
    pub fn parity_result_failed(
        &self,
        rule: &str,
        error: &str,
        now: i64,
    ) -> Result<(), ConfigDbError> {
        self.conn.execute(
            "INSERT INTO replication_parity (rule_name, status, last_error, updated_at)
             VALUES (?, 'failed', ?, ?)
             ON CONFLICT(rule_name) DO UPDATE SET
                status = 'failed', last_error = ?, updated_at = ?",
            params![rule, error, now, error, now],
        )?;
        Ok(())
    }

    /// Request cancellation: flip a RUNNING audit to 'cancelling'. The scan loop
    /// polls this between pages and bails. No-op (returns false) if the row isn't
    /// currently running. Returns true if a running audit was signalled.
    pub fn parity_request_cancel(&self, rule: &str, now: i64) -> Result<bool, ConfigDbError> {
        let n = self.conn.execute(
            "UPDATE replication_parity SET status = 'cancelling', updated_at = ?
             WHERE rule_name = ? AND status = 'running'",
            params![now, rule],
        )?;
        Ok(n > 0)
    }

    /// Settle a cancelled audit to its terminal 'cancelled' state.
    pub fn parity_result_cancelled(&self, rule: &str, now: i64) -> Result<(), ConfigDbError> {
        self.conn.execute(
            "UPDATE replication_parity
             SET status = 'cancelled', last_error = NULL, updated_at = ?
             WHERE rule_name = ?",
            params![now, rule],
        )?;
        Ok(())
    }

    /// Try to acquire the parity background-job lease for a rule.
    pub fn parity_try_acquire_lease(
        &self,
        rule: &str,
        owner: &str,
        now: i64,
        ttl_secs: i64,
    ) -> Result<bool, ConfigDbError> {
        // Ensure a row exists so the lease UPDATE has a target.
        self.conn.execute(
            "INSERT OR IGNORE INTO replication_parity (rule_name, updated_at) VALUES (?, ?)",
            params![rule, now],
        )?;
        job_store::try_acquire_leader_lease(
            &self.conn,
            "replication_parity",
            "rule_name",
            &rule,
            owner,
            now,
            ttl_secs,
        )
    }

    /// Renew the parity lease this owner still holds (heartbeat during a long
    /// scan). Returns false if the owner lost it (lapsed / taken) — the caller
    /// should then stop, since another owner may have started a fresh scan.
    pub fn parity_renew_lease(
        &self,
        rule: &str,
        owner: &str,
        now: i64,
        ttl_secs: i64,
    ) -> Result<bool, ConfigDbError> {
        job_store::renew_leader_lease(
            &self.conn,
            "replication_parity",
            "rule_name",
            &rule,
            owner,
            now,
            ttl_secs,
        )
    }

    /// Release the parity lease this owner holds.
    pub fn parity_release_lease(&self, rule: &str, owner: &str) -> Result<(), ConfigDbError> {
        job_store::release_leader_lease(
            &self.conn,
            "replication_parity",
            "rule_name",
            &rule,
            owner,
        )?;
        Ok(())
    }

    /// Boot reconcile: mark any in-flight row ('running' OR 'cancelling') failed
    /// (a previous process died mid-audit) AND clear its lease UNCONDITIONALLY.
    /// A process restart means the holding process is gone, so the lease is dead
    /// even if it hasn't expired — leaving it would block re-verify for the full
    /// TTL; and a row stuck at 'cancelling' would make the UI poll forever.
    /// Returns rows reconciled.
    pub fn parity_reconcile_on_boot(&self) -> Result<usize, ConfigDbError> {
        let now = current_unix_seconds();
        let n = self.conn.execute(
            "UPDATE replication_parity
             SET status = 'failed',
                 last_error = 'proxy restarted mid-audit',
                 leader_instance_id = NULL,
                 leader_expires_at = NULL,
                 updated_at = ?
             WHERE status IN ('running', 'cancelling')",
            params![now],
        )?;
        // Also clear any other stale (expired) leases on non-running rows.
        job_store::clear_stale_leases(&self.conn, "replication_parity", now)?;
        Ok(n)
    }

    /// Cheap status-only read for the per-page cancel check — avoids loading the
    /// (possibly large) outcome_json just to compare the status string.
    pub fn parity_status(&self, rule: &str) -> Result<Option<String>, ConfigDbError> {
        Ok(self
            .conn
            .query_row(
                "SELECT status FROM replication_parity WHERE rule_name = ?",
                [rule],
                |r| r.get::<_, String>(0),
            )
            .optional()?)
    }
}

/// The cached parity result row (the background-job state the UI polls).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParityResultRow {
    pub status: String,
    pub scanned_at: Option<i64>,
    pub progress_scanned: i64,
    pub outcome_json: Option<String>,
    pub last_error: Option<String>,
}

/// Which side of a replication rule a cached parity entry describes. Source and
/// dest rows are kept distinct so a whole-bucket mirror (source.prefix ==
/// dest.prefix) can't collide them into one row → false "in sync".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParitySide {
    Source,
    Dest,
}

impl ParitySide {
    fn as_str(self) -> &'static str {
        match self {
            ParitySide::Source => "src",
            ParitySide::Dest => "dst",
        }
    }
}

/// Cached logical metadata for one object (what a parity compare needs), plus
/// `stored_etag` — the etag/md5 of the STORED blob as the lite LIST reports it
/// (delta-blob etag for a delta object; the real etag for passthrough). It is
/// the cheap CONTENT-VERSION token: a cache hit is only trusted when the current
/// lite `stored_etag` still matches, so an in-place overwrite (new bytes → new
/// stored etag) misses the cache and is re-read instead of reporting stale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParityCacheEntry {
    pub sha256: Option<String>,
    pub size: u64,
    pub etag: Option<String>,
    pub stored_etag: Option<String>,
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
    fn lease_acquire_rejects_second_owner_until_expiry() {
        let db = db();
        db.replication_ensure_state("r", 100).unwrap();

        assert!(db
            .replication_try_acquire_lease("r", "owner-a", 100, 30)
            .unwrap());
        assert!(!db
            .replication_try_acquire_lease("r", "owner-b", 120, 30)
            .unwrap());

        let state = db.replication_load_state("r").unwrap().unwrap();
        assert_eq!(state.leader_instance_id.as_deref(), Some("owner-a"));
        assert_eq!(state.leader_expires_at, Some(130));

        // AT exact expiry the owner still wins (renew/steal predicates tile
        // — see config_db::job_store); strictly past it the steal succeeds.
        assert!(!db
            .replication_try_acquire_lease("r", "owner-b", 130, 30)
            .unwrap());
        assert!(db
            .replication_try_acquire_lease("r", "owner-b", 131, 30)
            .unwrap());
        let state = db.replication_load_state("r").unwrap().unwrap();
        assert_eq!(state.leader_instance_id.as_deref(), Some("owner-b"));
        assert_eq!(state.leader_expires_at, Some(161));
    }

    #[test]
    fn lease_release_only_by_current_owner() {
        let db = db();
        db.replication_ensure_state("r", 100).unwrap();
        assert!(db
            .replication_try_acquire_lease("r", "owner-a", 100, 30)
            .unwrap());

        assert!(!db.replication_release_lease("r", "owner-b").unwrap());
        assert!(db
            .replication_load_state("r")
            .unwrap()
            .unwrap()
            .leader_instance_id
            .is_some());

        assert!(db.replication_release_lease("r", "owner-a").unwrap());
        let state = db.replication_load_state("r").unwrap().unwrap();
        assert!(state.leader_instance_id.is_none());
        assert!(state.leader_expires_at.is_none());
    }

    #[test]
    fn lease_renew_only_by_current_owner() {
        let db = db();
        db.replication_ensure_state("r", 100).unwrap();
        assert!(db
            .replication_try_acquire_lease("r", "owner-a", 100, 30)
            .unwrap());

        assert!(!db.replication_renew_lease("r", "owner-b", 110, 30).unwrap());
        assert!(db.replication_renew_lease("r", "owner-a", 110, 30).unwrap());

        let state = db.replication_load_state("r").unwrap().unwrap();
        assert_eq!(state.leader_instance_id.as_deref(), Some("owner-a"));
        assert_eq!(state.leader_expires_at, Some(140));
    }

    #[test]
    fn begin_and_finish_run_update_both_tables() {
        let db = db();
        db.replication_ensure_state("r", 100).unwrap();
        let id = db.replication_begin_run("r", 200, "run-now").unwrap();
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
            ..Default::default()
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
        assert_eq!(runs[0].triggered_by, "run-now");
        assert_eq!(runs[0].status, "succeeded");
        assert_eq!(runs[0].objects_copied, 7);
        assert_eq!(runs[0].bytes_copied, 1024);
    }

    #[test]
    fn request_run_cancel_flips_running_and_is_observable() {
        let db = db();
        db.replication_ensure_state("r", 100).unwrap();
        let id = db.replication_begin_run("r", 200, "scheduler").unwrap();
        assert!(!db.replication_run_cancel_requested(id).unwrap());
        // Flip the running run → cancelling.
        assert!(db.replication_request_run_cancel("r").unwrap());
        assert!(db.replication_run_cancel_requested(id).unwrap());
        // The jobs list reads the live history status (cancelling), not the
        // stale state.last_status (still 'running' until the worker settles).
        assert_eq!(
            db.replication_latest_run_status("r").unwrap().as_deref(),
            Some("cancelling")
        );
        // No running run left after settle → second request is a no-op.
        db.replication_finish_run(id, "r", "cancelled", 300, Default::default(), 900)
            .unwrap();
        assert!(!db.replication_request_run_cancel("r").unwrap());
        assert_eq!(
            db.replication_latest_run_status("r").unwrap().as_deref(),
            Some("cancelled")
        );
    }

    #[test]
    fn record_failure_rings_by_max_retained() {
        let db = db();
        db.replication_ensure_state("r", 100).unwrap();
        for i in 0..5 {
            db.replication_record_failure(
                "r",
                FailureInsert {
                    run_id: Some(42),
                    occurred_at: 100 + i,
                    source_key: "src",
                    dest_key: "dst",
                    error_message: &format!("err-{i}"),
                },
                3,
            )
            .unwrap();
        }
        let recent = db.replication_recent_failures("r", 10).unwrap();
        assert_eq!(recent.len(), 3);
        assert!(recent.iter().all(|f| f.run_id == Some(42)));
        let msgs: Vec<_> = recent.iter().map(|f| f.error_message.clone()).collect();
        assert_eq!(msgs, vec!["err-4", "err-3", "err-2"]);
    }

    #[test]
    fn object_failure_record_increments_and_returns_count() {
        let db = db();
        assert_eq!(
            db.replication_record_object_failure("r", "k", "boom", 100)
                .unwrap(),
            1
        );
        assert_eq!(
            db.replication_record_object_failure("r", "k", "boom-again", 101)
                .unwrap(),
            2
        );
        // A different key has its own independent counter.
        assert_eq!(
            db.replication_record_object_failure("r", "other", "x", 102)
                .unwrap(),
            1
        );
    }

    #[test]
    fn object_failure_clear_resets_count() {
        let db = db();
        db.replication_record_object_failure("r", "k", "boom", 100)
            .unwrap();
        db.replication_record_object_failure("r", "k", "boom", 101)
            .unwrap();
        db.replication_clear_object_failure("r", "k").unwrap();
        // After clear, the next failure starts fresh at 1.
        assert_eq!(
            db.replication_record_object_failure("r", "k", "boom", 102)
                .unwrap(),
            1
        );
    }

    #[test]
    fn object_skipped_honours_threshold_and_zero_never() {
        let db = db();
        // No row yet → never skipped.
        assert!(!db.replication_object_skipped("r", "k", 3).unwrap());

        for i in 0..3 {
            db.replication_record_object_failure("r", "k", "boom", 100 + i)
                .unwrap();
        }
        // 3 >= 3 → skipped; 3 < 4 → not yet.
        assert!(db.replication_object_skipped("r", "k", 3).unwrap());
        assert!(!db.replication_object_skipped("r", "k", 4).unwrap());

        // threshold 0 = never skip, even with failures recorded.
        assert!(!db.replication_object_skipped("r", "k", 0).unwrap());

        // Clearing resets the predicate.
        db.replication_clear_object_failure("r", "k").unwrap();
        assert!(!db.replication_object_skipped("r", "k", 3).unwrap());
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
        db.replication_begin_run("r", 200, "scheduler").unwrap();

        let count = db.replication_reconcile_on_boot(100).unwrap();
        assert_eq!(count, 1);

        let runs = db.replication_recent_runs("r", 10).unwrap();
        assert_eq!(runs[0].status, "failed");
        let fails = db.replication_recent_failures("r", 10).unwrap();
        assert_eq!(fails.len(), 1);
        assert!(fails[0].error_message.contains("died mid-run"));

        // C8: the per-rule state row's last_status is also flipped to 'failed',
        // so the admin UI doesn't keep reporting the rule as 'running' after the
        // crash (matches lifecycle's twin).
        let st = db.replication_load_state("r").unwrap().unwrap();
        assert_eq!(st.last_status, "failed");
    }

    #[test]
    fn reconcile_on_boot_prunes_failure_ring_per_affected_rule() {
        // A rapid-restart loop inserts a failure row per zombie run but the
        // ring must stay bounded to `max_failures_retained` per rule — the
        // boot path now prunes, matching the normal record_failure path.
        let db = db();
        db.replication_ensure_state("r", 100).unwrap();
        const MAX: u32 = 3;
        // Start (MAX + 5) zombie runs for the same rule — each boot reconcile
        // would otherwise accumulate one failure row, growing unbounded.
        for _ in 0..(MAX as usize + 5) {
            db.replication_begin_run("r", 200, "scheduler").unwrap();
        }
        // Sanity: that many running rows exist before reconcile.
        let running = db
            .replication_recent_runs("r", 100)
            .unwrap()
            .iter()
            .filter(|r| r.status == "running")
            .count();
        assert_eq!(running, MAX as usize + 5);

        let count = db.replication_reconcile_on_boot(MAX).unwrap();
        assert_eq!(count, MAX as usize + 5);

        // The ring is pruned to MAX for rule "r".
        let fails = db.replication_recent_failures("r", 100).unwrap();
        assert_eq!(
            fails.len(),
            MAX as usize,
            "boot reconcile must prune the failure ring to max_failures_retained"
        );
        assert!(fails
            .iter()
            .all(|f| f.error_message.contains("died mid-run")));
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

    #[test]
    fn object_failures_for_keys_returns_present_only() {
        let db = db();
        db.replication_record_object_failure("r", "a", "err-a", 10)
            .unwrap();
        db.replication_record_object_failure("r", "a", "err-a2", 20)
            .unwrap();
        db.replication_record_object_failure("r", "b", "err-b", 30)
            .unwrap();

        let got = db
            .replication_object_failures_for_keys("r", &["a", "b", "c"])
            .unwrap();
        assert_eq!(got.len(), 2, "absent key 'c' is omitted");
        let a = got.get("a").unwrap();
        assert_eq!(a.consecutive_failures, 2);
        assert_eq!(a.last_error, "err-a2");
        assert_eq!(a.last_failed_at, 20);
        assert_eq!(got.get("b").unwrap().consecutive_failures, 1);
        assert!(!got.contains_key("c"));
    }

    #[test]
    fn object_failures_for_keys_empty_skips_query() {
        let db = db();
        let got = db.replication_object_failures_for_keys("r", &[]).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn parity_result_lifecycle_running_then_done() {
        let db = db();
        assert!(
            db.parity_result_load("r").unwrap().is_none(),
            "idle = no row"
        );
        db.parity_result_set_running("r", 100).unwrap();
        let row = db.parity_result_load("r").unwrap().unwrap();
        assert_eq!(row.status, "running");
        assert_eq!(row.progress_scanned, 0);

        db.parity_result_progress("r", 4200, 110).unwrap();
        assert_eq!(
            db.parity_result_load("r")
                .unwrap()
                .unwrap()
                .progress_scanned,
            4200
        );

        db.parity_result_done("r", true, "{\"in_sync\":true}", 120)
            .unwrap();
        let row = db.parity_result_load("r").unwrap().unwrap();
        assert_eq!(row.status, "done");
        assert_eq!(row.scanned_at, Some(120));
        assert_eq!(row.outcome_json.as_deref(), Some("{\"in_sync\":true}"));
        assert!(row.last_error.is_none());

        db.parity_result_failed("r", "boom", 130).unwrap();
        let row = db.parity_result_load("r").unwrap().unwrap();
        assert_eq!(row.status, "failed");
        assert_eq!(row.last_error.as_deref(), Some("boom"));
    }

    #[test]
    fn parity_cancel_flow_running_to_cancelled() {
        let db = db();
        // Cancelling an idle audit is a no-op.
        assert!(!db.parity_request_cancel("r", 100).unwrap());
        db.parity_result_set_running("r", 100).unwrap();
        // Now a running audit can be signalled.
        assert!(db.parity_request_cancel("r", 110).unwrap());
        assert_eq!(
            db.parity_result_load("r").unwrap().unwrap().status,
            "cancelling"
        );
        // The scan settles it.
        db.parity_result_cancelled("r", 120).unwrap();
        let row = db.parity_result_load("r").unwrap().unwrap();
        assert_eq!(row.status, "cancelled");
        assert!(row.last_error.is_none());
        // Cancelling a non-running (cancelled) row is again a no-op.
        assert!(!db.parity_request_cancel("r", 130).unwrap());
    }

    #[test]
    fn parity_lease_serialises_and_boot_reconcile_clears_running() {
        let db = db();
        assert!(db
            .parity_try_acquire_lease("r", "owner-a", 100, 60)
            .unwrap());
        // A second owner can't take a live lease.
        assert!(!db
            .parity_try_acquire_lease("r", "owner-b", 110, 60)
            .unwrap());
        db.parity_result_set_running("r", 110).unwrap();

        // Boot reconcile: stale-lease clear + mark a 'running' row failed.
        let n = db.parity_reconcile_on_boot().unwrap();
        assert_eq!(n, 1, "the running audit is reconciled to failed");
        assert_eq!(
            db.parity_result_load("r").unwrap().unwrap().status,
            "failed"
        );

        db.parity_release_lease("r", "owner-a").unwrap();
        // After release a fresh owner can acquire.
        assert!(db
            .parity_try_acquire_lease("r", "owner-c", 200, 60)
            .unwrap());
    }

    #[test]
    fn parity_cache_round_trips_and_isolates_by_rule() {
        let mut db = db();
        let e = |sha: Option<&str>, size: u64, etag: Option<&str>| ParityCacheEntry {
            sha256: sha.map(str::to_string),
            size,
            etag: etag.map(str::to_string),
            stored_etag: Some("blob-etag".to_string()),
        };
        let src = ParitySide::Source;
        db.parity_cache_put_many(
            "r1",
            src,
            &[
                ("a/x.zip".into(), e(Some("sha-a"), 100, Some("etag-a"))),
                ("a/y.zip".into(), e(None, 50, Some("etag-y-2"))),
            ],
            1000,
        )
        .unwrap();
        // Different rule must not see r1's entries.
        db.parity_cache_put_many(
            "r2",
            src,
            &[("a/x.zip".into(), e(Some("other"), 9, None))],
            1000,
        )
        .unwrap();

        let got = db
            .parity_cache_get_many("r1", src, &["a/x.zip", "a/y.zip", "missing"])
            .unwrap();
        assert_eq!(got.len(), 2, "only the two present keys, not the miss");
        assert_eq!(got["a/x.zip"], e(Some("sha-a"), 100, Some("etag-a")));
        assert_eq!(
            got["a/x.zip"].stored_etag.as_deref(),
            Some("blob-etag"),
            "the content-version token round-trips"
        );
        assert_eq!(got["a/y.zip"].size, 50);
        assert_eq!(
            db.parity_cache_get_many("r2", src, &["a/x.zip"]).unwrap()["a/x.zip"].size,
            9
        );
    }

    #[test]
    fn parity_cache_source_and_dest_never_collide() {
        // The structural false-"in-sync" guard: same rule + same key on both
        // sides (a whole-bucket mirror) must keep SEPARATE rows. If source read
        // dest's logical metadata they'd always "match".
        let mut db = db();
        let mk = |sha: &str| ParityCacheEntry {
            sha256: Some(sha.into()),
            size: 1,
            etag: None,
            stored_etag: Some("blob".into()),
        };
        db.parity_cache_put_many("r", ParitySide::Source, &[("k".into(), mk("SRC"))], 1)
            .unwrap();
        db.parity_cache_put_many("r", ParitySide::Dest, &[("k".into(), mk("DST"))], 1)
            .unwrap();
        assert_eq!(
            db.parity_cache_get_many("r", ParitySide::Source, &["k"])
                .unwrap()["k"]
                .sha256
                .as_deref(),
            Some("SRC")
        );
        assert_eq!(
            db.parity_cache_get_many("r", ParitySide::Dest, &["k"])
                .unwrap()["k"]
                .sha256
                .as_deref(),
            Some("DST"),
            "dest row must be distinct from the source row"
        );
    }

    #[test]
    fn parity_cache_retain_prunes_deleted_and_respects_side() {
        let mut db = db();
        let mk = ParityCacheEntry {
            sha256: None,
            size: 1,
            etag: None,
            stored_etag: None,
        };
        let s = ParitySide::Source;
        db.parity_cache_put_many(
            "r",
            s,
            &[("keep".into(), mk.clone()), ("gone".into(), mk.clone())],
            1,
        )
        .unwrap();
        // Dest side has its own "gone" — must NOT be pruned by a source retain.
        db.parity_cache_put_many("r", ParitySide::Dest, &[("gone".into(), mk.clone())], 1)
            .unwrap();

        let removed = db
            .parity_cache_retain("r", s, &["keep".to_string()])
            .unwrap();
        assert_eq!(removed, 1, "only the source 'gone' row is pruned");
        assert!(db
            .parity_cache_get_many("r", s, &["gone"])
            .unwrap()
            .is_empty());
        assert!(!db
            .parity_cache_get_many("r", s, &["keep"])
            .unwrap()
            .is_empty());
        assert!(
            !db.parity_cache_get_many("r", ParitySide::Dest, &["gone"])
                .unwrap()
                .is_empty(),
            "dest side untouched by a source-side retain"
        );
    }

    #[test]
    fn parity_cache_upsert_overwrites_logical_metadata() {
        let mut db = db();
        let mk = |size| ParityCacheEntry {
            sha256: Some("h".into()),
            size,
            etag: None,
            stored_etag: None,
        };
        let s = ParitySide::Source;
        db.parity_cache_put_many("r", s, &[("k".into(), mk(10))], 1)
            .unwrap();
        db.parity_cache_put_many("r", s, &[("k".into(), mk(20))], 2)
            .unwrap();
        let got = db.parity_cache_get_many("r", s, &["k"]).unwrap();
        assert_eq!(got["k"].size, 20, "second write wins");
    }

    #[test]
    fn parity_cache_clear_and_empty_get() {
        let mut db = db();
        let e = ParityCacheEntry {
            sha256: None,
            size: 1,
            etag: None,
            stored_etag: None,
        };
        let s = ParitySide::Source;
        db.parity_cache_put_many("r", s, &[("k".into(), e)], 1)
            .unwrap();
        db.parity_cache_clear("r").unwrap();
        assert!(db.parity_cache_get_many("r", s, &["k"]).unwrap().is_empty());
        // Empty key list short-circuits without a query.
        assert!(db.parity_cache_get_many("r", s, &[]).unwrap().is_empty());
    }
}
