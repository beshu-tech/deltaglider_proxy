// SPDX-License-Identifier: GPL-3.0-only

//! Persistent state for one-off maintenance jobs — wraps the v13
//! SQLCipher tables:
//!
//! - `maintenance_jobs`: one row per job (initially: kind `reencrypt`).
//!   Carries the resumable cursor (`phase` + `continuation_token`),
//!   progress counters, and a leader lease. A partial unique index
//!   enforces at most ONE active (queued/running/cancelling) job per
//!   bucket.
//! - `maintenance_failures`: per-object errors, ring-bounded.
//!
//! Implemented as methods on [`ConfigDb`] so the SQLCipher mutex
//! serialises all maintenance mutations alongside IAM/replication state
//! (same pattern as `src/replication/state_store.rs`).
//!
//! ## Restart semantics (deliberate deviation from replication)
//!
//! Replication flips zombie `running` runs to `failed` on boot because a
//! new periodic run will follow anyway. A maintenance job is a one-off
//! the operator explicitly started, so [`ConfigDb::maintenance_reconcile_on_boot`]
//! flips `running`/`cancelling` back to **`queued`** with the phase and
//! continuation token preserved — the worker resumes where the previous
//! process died. This is what makes the server-side state "stable":
//! a restart pauses the job, never loses it.

use crate::config_db::job_store;
use crate::config_db::{ConfigDb, ConfigDbError};
use rusqlite::{params, OptionalExtension};

pub use crate::replication::state_store::current_unix_seconds;

/// One maintenance job row.
#[derive(Debug, Clone, PartialEq)]
pub struct MaintenanceJob {
    pub id: i64,
    pub kind: String,
    pub bucket: String,
    pub status: String,
    pub phase: String,
    pub objects_total: Option<i64>,
    pub objects_done: i64,
    pub objects_skipped: i64,
    pub objects_failed: i64,
    pub bytes_done: i64,
    pub continuation_token: Option<String>,
    pub last_error: Option<String>,
    pub triggered_by: Option<String>,
    /// Kind-specific JSON parameters (e.g. migrate: target backend,
    /// delete_source, transient route key). None for reencrypt.
    pub params: Option<String>,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub updated_at: i64,
}

/// A per-object failure row.
#[derive(Debug, Clone, PartialEq)]
pub struct MaintenanceFailure {
    pub id: i64,
    pub job_id: i64,
    pub object_key: String,
    pub error: String,
    pub created_at: i64,
}

/// Outcome of a cancel request — the caller's follow-up differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    /// Job was still queued: cancelled immediately, gate can be released.
    CancelledImmediately,
    /// Job is running: marked `cancelling`; the worker finishes the
    /// current page, then settles it to `cancelled` and releases the gate.
    CancelRequested,
    /// No active job with that id.
    NotActive,
}

const ACTIVE_STATUSES: &str = "('queued','running','cancelling')";

fn row_to_job(r: &rusqlite::Row<'_>) -> rusqlite::Result<MaintenanceJob> {
    Ok(MaintenanceJob {
        id: r.get(0)?,
        kind: r.get(1)?,
        bucket: r.get(2)?,
        status: r.get(3)?,
        phase: r.get(4)?,
        objects_total: r.get(5)?,
        objects_done: r.get(6)?,
        objects_skipped: r.get(7)?,
        objects_failed: r.get(8)?,
        bytes_done: r.get(9)?,
        continuation_token: r.get(10)?,
        last_error: r.get(11)?,
        triggered_by: r.get(12)?,
        params: r.get(13)?,
        created_at: r.get(14)?,
        started_at: r.get(15)?,
        finished_at: r.get(16)?,
        updated_at: r.get(17)?,
    })
}

const JOB_COLUMNS: &str = "id, kind, bucket, status, phase, objects_total, objects_done, \
     objects_skipped, objects_failed, bytes_done, continuation_token, last_error, \
     triggered_by, params, created_at, started_at, finished_at, updated_at";

impl ConfigDb {
    /// Create a queued job for `bucket`. Returns `Ok(None)` when the
    /// bucket already has an active job (the partial unique index fires) —
    /// the caller turns that into a 409. Returns the new job id otherwise.
    pub fn maintenance_create_job(
        &self,
        kind: &str,
        bucket: &str,
        initial_phase: &str,
        params_json: Option<&str>,
        triggered_by: &str,
        now: i64,
    ) -> Result<Option<i64>, ConfigDbError> {
        use crate::config_db::{classify_sqlite_error, SqliteErrorClass};
        match self.conn.execute(
            "INSERT INTO maintenance_jobs
                (kind, bucket, status, phase, params, triggered_by, created_at, updated_at)
             VALUES (?, ?, 'queued', ?, ?, ?, ?, ?)",
            params![
                kind,
                bucket,
                initial_phase,
                params_json,
                triggered_by,
                now,
                now
            ],
        ) {
            Ok(_) => Ok(Some(self.conn.last_insert_rowid())),
            Err(e) if classify_sqlite_error(&e) == SqliteErrorClass::Conflict => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Newest-first job listing (active and terminal), capped at `limit`.
    pub fn maintenance_list_jobs(
        &self,
        limit: usize,
    ) -> Result<Vec<MaintenanceJob>, ConfigDbError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {JOB_COLUMNS} FROM maintenance_jobs ORDER BY id DESC LIMIT ?"
        ))?;
        let rows = stmt
            .query_map(params![limit as i64], row_to_job)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The bucket's active job (queued/running/cancelling), if any.
    pub fn maintenance_active_job_for_bucket(
        &self,
        bucket: &str,
    ) -> Result<Option<MaintenanceJob>, ConfigDbError> {
        let job = self
            .conn
            .query_row(
                &format!(
                    "SELECT {JOB_COLUMNS} FROM maintenance_jobs
                     WHERE bucket = ? AND status IN {ACTIVE_STATUSES}"
                ),
                params![bucket],
                row_to_job,
            )
            .optional()?;
        Ok(job)
    }

    /// Load one job by id (any status).
    pub fn maintenance_job_by_id(&self, id: i64) -> Result<Option<MaintenanceJob>, ConfigDbError> {
        let job = self
            .conn
            .query_row(
                &format!("SELECT {JOB_COLUMNS} FROM maintenance_jobs WHERE id = ?"),
                params![id],
                row_to_job,
            )
            .optional()?;
        Ok(job)
    }

    /// All buckets with an active job — the boot-time gate re-arm input.
    pub fn maintenance_active_buckets(&self) -> Result<Vec<String>, ConfigDbError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT bucket FROM maintenance_jobs WHERE status IN {ACTIVE_STATUSES}"
        ))?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Transient route keys (`__dgmigrate_*`) referenced by ACTIVE migrate
    /// jobs — the boot reconcile must leave these in the config for the
    /// resumed job to reuse; anything else `__dgmigrate_*` is an orphan.
    pub fn maintenance_active_transient_keys(&self) -> Result<Vec<String>, ConfigDbError> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT params FROM maintenance_jobs
              WHERE kind = 'migrate' AND status IN {ACTIVE_STATUSES}"
        ))?;
        let rows: Vec<Option<String>> = stmt
            .query_map([], |r| r.get(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows
            .into_iter()
            .flatten()
            .filter_map(|p| {
                serde_json::from_str::<serde_json::Value>(&p)
                    .ok()
                    .and_then(|v| {
                        v.get("transient_key")
                            .and_then(|t| t.as_str().map(String::from))
                    })
            })
            .collect())
    }

    /// Claim the oldest queued job: set it `running`, stamp `started_at`
    /// (first claim only — a resumed job keeps the original), and take
    /// the leader lease. Returns the claimed job.
    pub fn maintenance_claim_next_job(
        &self,
        instance_id: &str,
        now: i64,
        lease_ttl_secs: i64,
    ) -> Result<Option<MaintenanceJob>, ConfigDbError> {
        let candidate: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM maintenance_jobs
                 WHERE status = 'queued'
                   AND (leader_instance_id IS NULL
                        OR leader_expires_at IS NULL
                        OR leader_expires_at <= ?)
                 ORDER BY id ASC LIMIT 1",
                params![now],
                |r| r.get(0),
            )
            .optional()?;
        let Some(id) = candidate else {
            return Ok(None);
        };
        let updated = self.conn.execute(
            "UPDATE maintenance_jobs
                SET status = 'running',
                    started_at = COALESCE(started_at, ?),
                    leader_instance_id = ?,
                    leader_expires_at = ?,
                    updated_at = ?
              WHERE id = ? AND status = 'queued'",
            params![now, instance_id, now + lease_ttl_secs.max(1), now, id],
        )?;
        if updated == 0 {
            return Ok(None); // raced by another claimer
        }
        let job = self.conn.query_row(
            &format!("SELECT {JOB_COLUMNS} FROM maintenance_jobs WHERE id = ?"),
            params![id],
            row_to_job,
        )?;
        Ok(Some(job))
    }

    /// Renew the running job's lease (canonical semantics in
    /// `config_db::job_store`: a lapsed lease never resurrects).
    pub fn maintenance_heartbeat(
        &self,
        job_id: i64,
        instance_id: &str,
        now: i64,
        lease_ttl_secs: i64,
    ) -> Result<(), ConfigDbError> {
        job_store::renew_leader_lease(
            &self.conn,
            "maintenance_jobs",
            "id",
            &job_id,
            instance_id,
            now,
            lease_ttl_secs,
        )?;
        Ok(())
    }

    /// Persist the resumable cursor + live progress. Called once per page
    /// so the admin UI reads near-real-time counts and a crash resumes
    /// from the last persisted token.
    #[allow(clippy::too_many_arguments)]
    pub fn maintenance_update_progress(
        &self,
        job_id: i64,
        phase: &str,
        objects_total: Option<i64>,
        objects_done: i64,
        objects_skipped: i64,
        objects_failed: i64,
        bytes_done: i64,
        continuation_token: Option<&str>,
    ) -> Result<(), ConfigDbError> {
        self.conn.execute(
            "UPDATE maintenance_jobs
                SET phase = ?, objects_total = ?, objects_done = ?,
                    objects_skipped = ?, objects_failed = ?, bytes_done = ?,
                    continuation_token = ?, updated_at = ?
              WHERE id = ? AND status IN ('running','cancelling')",
            params![
                phase,
                objects_total,
                objects_done,
                objects_skipped,
                objects_failed,
                bytes_done,
                continuation_token,
                current_unix_seconds(),
                job_id,
            ],
        )?;
        Ok(())
    }

    /// Request cancellation. Queued jobs cancel immediately; running jobs
    /// flip to `cancelling` and the worker settles them.
    pub fn maintenance_request_cancel(&self, job_id: i64) -> Result<CancelOutcome, ConfigDbError> {
        let now = current_unix_seconds();
        let n = self.conn.execute(
            "UPDATE maintenance_jobs
                SET status = 'cancelled', finished_at = ?, updated_at = ?
              WHERE id = ? AND status = 'queued'",
            params![now, now, job_id],
        )?;
        if n > 0 {
            return Ok(CancelOutcome::CancelledImmediately);
        }
        let n = self.conn.execute(
            "UPDATE maintenance_jobs
                SET status = 'cancelling', updated_at = ?
              WHERE id = ? AND status = 'running'",
            params![now, job_id],
        )?;
        if n > 0 {
            return Ok(CancelOutcome::CancelRequested);
        }
        Ok(CancelOutcome::NotActive)
    }

    /// True when a cancel has been requested for this job.
    pub fn maintenance_cancel_requested(&self, job_id: i64) -> Result<bool, ConfigDbError> {
        let status: Option<String> = self
            .conn
            .query_row(
                "SELECT status FROM maintenance_jobs WHERE id = ?",
                params![job_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(matches!(
            status.as_deref(),
            Some("cancelling") | Some("cancelled")
        ))
    }

    /// Settle a job to a terminal status (`completed`/`failed`/`cancelled`)
    /// and clear the lease. TERMINAL ROWS ARE IMMUTABLE: re-settling is a
    /// no-op, so a phase that pre-settles with an operator note (migrate's
    /// "source cleanup incomplete") can't have that note clobbered by the
    /// worker's generic settle that follows.
    pub fn maintenance_finish(
        &self,
        job_id: i64,
        status: &str,
        last_error: Option<&str>,
    ) -> Result<(), ConfigDbError> {
        let now = current_unix_seconds();
        self.conn.execute(
            "UPDATE maintenance_jobs
                SET status = ?, last_error = ?, finished_at = ?, updated_at = ?,
                    leader_instance_id = NULL, leader_expires_at = NULL
              WHERE id = ?
                AND status IN ('queued','running','cancelling')",
            params![status, last_error, now, now, job_id],
        )?;
        Ok(())
    }

    /// Record a per-object failure, ring-bounded to `max_retained` rows
    /// per job (oldest evicted first).
    pub fn maintenance_record_failure(
        &self,
        job_id: i64,
        object_key: &str,
        error: &str,
        max_retained: usize,
    ) -> Result<(), ConfigDbError> {
        self.conn.execute(
            "INSERT INTO maintenance_failures (job_id, object_key, error, created_at)
             VALUES (?, ?, ?, ?)",
            params![job_id, object_key, error, current_unix_seconds()],
        )?;
        job_store::prune_failure_ring(
            &self.conn,
            "maintenance_failures",
            "job_id",
            &job_id,
            max_retained as u32,
        )?;
        Ok(())
    }

    /// Newest-first failures for a job.
    pub fn maintenance_list_failures(
        &self,
        job_id: i64,
        limit: usize,
    ) -> Result<Vec<MaintenanceFailure>, ConfigDbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, job_id, object_key, error, created_at
               FROM maintenance_failures
              WHERE job_id = ? ORDER BY id DESC LIMIT ?",
        )?;
        let rows = stmt
            .query_map(params![job_id, limit as i64], |r| {
                Ok(MaintenanceFailure {
                    id: r.get(0)?,
                    job_id: r.get(1)?,
                    object_key: r.get(2)?,
                    error: r.get(3)?,
                    created_at: r.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Boot reconciliation: jobs left `running`/`cancelling` by a dead
    /// process go back to `queued` with phase + continuation token
    /// PRESERVED, so the worker resumes them. Stale leases are cleared.
    /// Returns the number of jobs re-queued.
    pub fn maintenance_reconcile_on_boot(&self) -> Result<usize, ConfigDbError> {
        let now = current_unix_seconds();
        let n = self.conn.execute(
            "UPDATE maintenance_jobs
                SET status = 'queued',
                    leader_instance_id = NULL,
                    leader_expires_at = NULL,
                    updated_at = ?
              WHERE status IN ('running','cancelling')",
            params![now],
        )?;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> ConfigDb {
        ConfigDb::in_memory("testpass").expect("open in-memory db")
    }

    #[test]
    fn create_and_load_active_job() {
        let db = db();
        let id = db
            .maintenance_create_job("reencrypt", "pippo", "counting", None, "admin", 100)
            .unwrap()
            .unwrap();
        let job = db
            .maintenance_active_job_for_bucket("pippo")
            .unwrap()
            .unwrap();
        assert_eq!(job.id, id);
        assert_eq!(job.status, "queued");
        assert_eq!(job.phase, "counting");
        assert_eq!(job.kind, "reencrypt");
        assert_eq!(job.objects_total, None);
        assert_eq!(db.maintenance_active_buckets().unwrap(), vec!["pippo"]);
    }

    #[test]
    fn second_active_job_for_same_bucket_conflicts() {
        let db = db();
        db.maintenance_create_job("reencrypt", "b", "counting", None, "admin", 1)
            .unwrap()
            .unwrap();
        assert!(
            db.maintenance_create_job("reencrypt", "b", "counting", None, "admin", 2)
                .unwrap()
                .is_none(),
            "active job must block a second one"
        );
        // A terminal job frees the slot.
        let job = db.maintenance_active_job_for_bucket("b").unwrap().unwrap();
        db.maintenance_finish(job.id, "completed", None).unwrap();
        assert!(db
            .maintenance_create_job("reencrypt", "b", "counting", None, "admin", 3)
            .unwrap()
            .is_some());
    }

    #[test]
    fn claim_marks_running_and_takes_lease() {
        let db = db();
        let id = db
            .maintenance_create_job("reencrypt", "b", "counting", None, "admin", 1)
            .unwrap()
            .unwrap();
        let job = db
            .maintenance_claim_next_job("worker-1", 10, 60)
            .unwrap()
            .unwrap();
        assert_eq!(job.id, id);
        assert_eq!(job.status, "running");
        assert_eq!(job.started_at, Some(10));
        // Nothing left to claim.
        assert!(db
            .maintenance_claim_next_job("worker-2", 11, 60)
            .unwrap()
            .is_none());
    }

    #[test]
    fn progress_and_cursor_round_trip() {
        let db = db();
        let id = db
            .maintenance_create_job("reencrypt", "b", "counting", None, "admin", 1)
            .unwrap()
            .unwrap();
        db.maintenance_claim_next_job("w", 2, 60).unwrap().unwrap();
        db.maintenance_update_progress(id, "objects", Some(100), 40, 10, 1, 12345, Some("tok"))
            .unwrap();
        let job = db.maintenance_active_job_for_bucket("b").unwrap().unwrap();
        assert_eq!(job.phase, "objects");
        assert_eq!(job.objects_total, Some(100));
        assert_eq!(job.objects_done, 40);
        assert_eq!(job.objects_skipped, 10);
        assert_eq!(job.objects_failed, 1);
        assert_eq!(job.bytes_done, 12345);
        assert_eq!(job.continuation_token.as_deref(), Some("tok"));
    }

    #[test]
    fn cancel_queued_is_immediate_running_is_deferred() {
        let db = db();
        let q = db
            .maintenance_create_job("reencrypt", "a", "counting", None, "admin", 1)
            .unwrap()
            .unwrap();
        assert_eq!(
            db.maintenance_request_cancel(q).unwrap(),
            CancelOutcome::CancelledImmediately
        );
        let r = db
            .maintenance_create_job("reencrypt", "b", "counting", None, "admin", 2)
            .unwrap()
            .unwrap();
        db.maintenance_claim_next_job("w", 3, 60).unwrap().unwrap();
        assert_eq!(
            db.maintenance_request_cancel(r).unwrap(),
            CancelOutcome::CancelRequested
        );
        assert!(db.maintenance_cancel_requested(r).unwrap());
        assert_eq!(
            db.maintenance_request_cancel(r).unwrap(),
            CancelOutcome::NotActive
        );
        // Worker settles it.
        db.maintenance_finish(r, "cancelled", None).unwrap();
        assert!(db.maintenance_active_job_for_bucket("b").unwrap().is_none());
    }

    #[test]
    fn reconcile_on_boot_requeues_with_cursor_preserved() {
        let db = db();
        let id = db
            .maintenance_create_job("reencrypt", "b", "counting", None, "admin", 1)
            .unwrap()
            .unwrap();
        db.maintenance_claim_next_job("w", 2, 60).unwrap().unwrap();
        db.maintenance_update_progress(id, "objects", Some(50), 20, 5, 0, 999, Some("page-3"))
            .unwrap();
        let n = db.maintenance_reconcile_on_boot().unwrap();
        assert_eq!(n, 1);
        let job = db.maintenance_active_job_for_bucket("b").unwrap().unwrap();
        assert_eq!(job.status, "queued");
        assert_eq!(job.phase, "objects");
        assert_eq!(job.continuation_token.as_deref(), Some("page-3"));
        assert_eq!(job.objects_done, 20);
        // And it is claimable again, keeping the original started_at.
        let claimed = db
            .maintenance_claim_next_job("w2", 99, 60)
            .unwrap()
            .unwrap();
        assert_eq!(claimed.id, id);
        assert_eq!(claimed.started_at, Some(2));
    }

    #[test]
    fn finish_is_terminal_and_immutable() {
        let db = db();
        let id = db
            .maintenance_create_job("migrate", "b", "stage", None, "admin", 1)
            .unwrap()
            .unwrap();
        db.maintenance_claim_next_job("w", 2, 60).unwrap().unwrap();
        // Pre-settle with a note (migrate's cleanup-incomplete path) …
        db.maintenance_finish(id, "completed", Some("source cleanup incomplete"))
            .unwrap();
        // … the worker's generic settle afterwards must be a NO-OP.
        db.maintenance_finish(id, "completed", None).unwrap();
        let job = db.maintenance_job_by_id(id).unwrap().unwrap();
        assert_eq!(job.status, "completed");
        assert_eq!(
            job.last_error.as_deref(),
            Some("source cleanup incomplete"),
            "the operator note must survive the double-settle"
        );
        // And a terminal row can't be flipped to another terminal state.
        db.maintenance_finish(id, "failed", Some("nope")).unwrap();
        assert_eq!(
            db.maintenance_job_by_id(id).unwrap().unwrap().status,
            "completed"
        );
    }

    #[test]
    fn failures_ring_is_bounded() {
        let db = db();
        let id = db
            .maintenance_create_job("reencrypt", "b", "counting", None, "admin", 1)
            .unwrap()
            .unwrap();
        for i in 0..10 {
            db.maintenance_record_failure(id, &format!("k{i}"), "boom", 3)
                .unwrap();
        }
        let rows = db.maintenance_list_failures(id, 50).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].object_key, "k9"); // newest first
    }

    #[test]
    fn params_round_trip_and_active_transient_keys() {
        let db = db();
        let params = r#"{"target_backend":"hz","delete_source":false,"transient_key":"__dgmigrate_b_1","from_backend":"local"}"#;
        let id = db
            .maintenance_create_job("migrate", "b", "stage", Some(params), "admin", 1)
            .unwrap()
            .unwrap();
        let job = db.maintenance_job_by_id(id).unwrap().unwrap();
        assert_eq!(job.kind, "migrate");
        assert_eq!(job.phase, "stage");
        assert_eq!(job.params.as_deref(), Some(params));
        assert_eq!(
            db.maintenance_active_transient_keys().unwrap(),
            vec!["__dgmigrate_b_1"]
        );
        // Terminal job no longer counts; malformed params tolerated.
        db.maintenance_finish(id, "completed", None).unwrap();
        db.maintenance_create_job("migrate", "c", "stage", Some("not-json"), "admin", 2)
            .unwrap()
            .unwrap();
        assert!(db.maintenance_active_transient_keys().unwrap().is_empty());
    }

    #[test]
    fn list_jobs_newest_first() {
        let db = db();
        db.maintenance_create_job("reencrypt", "a", "counting", None, "admin", 1)
            .unwrap()
            .unwrap();
        let b = db
            .maintenance_create_job("reencrypt", "b", "counting", None, "admin", 2)
            .unwrap()
            .unwrap();
        let jobs = db.maintenance_list_jobs(10).unwrap();
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].id, b);
    }
}
