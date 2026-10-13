// SPDX-License-Identifier: BUSL-1.1

//! The node-local job database (`deltaglider_jobs.db`).
//!
//! The job plane of a node — replication, lifecycle and maintenance state,
//! run history, failure rings, leases, the parity cache, the event outbox and
//! its delivery rows and listener cursors — lives here, NOT in the synced
//! config DB. The config DB is uploaded to the sync bucket after every IAM
//! change; these rows are per node and grow with the object count (a prod
//! config DB reached 338 MB with them), so they stay on the node.
//!
//! Same SQLCipher key as the config DB ([`ConfigDbKeys`]), opened through the
//! same keyed open (`config_db::open_keyed`), so a key rotation moves both.
//!
//! An older release kept these tables in the config DB. [`JobsDb::adopt_from`]
//! moves the rows over once, at boot, from the LIVE config DB only. The
//! config DB keeps the tables, empty: an older peer that downloads the synced
//! copy, or an older binary after a downgrade, still finds them. See
//! `docs/plan/node-local-db-split.md`.

use super::{open_encrypted, open_keyed, ConfigDb, ConfigDbError, ConfigDbKeys, OpenedWith};
use rusqlite::{Connection, OptionalExtension};
use std::path::{Path, PathBuf};
use tracing::{error, info, warn};

/// Schema version of the jobs DB (independent of the config DB's).
pub(crate) const JOBS_SCHEMA_VERSION: i32 = 1;

/// The node-local tables, parents before children (the copy order; the
/// delete order is the reverse).
pub(crate) const NODE_LOCAL_TABLES: &[&str] = &[
    "replication_state",
    "replication_run_history",
    "replication_failures",
    "replication_object_failures",
    "replication_parity_objects",
    "replication_parity",
    "event_outbox",
    "event_deliveries",
    "listener_cursors",
    "lifecycle_state",
    "lifecycle_run_history",
    "lifecycle_failures",
    "maintenance_jobs",
    "maintenance_failures",
];

/// The node-local job database. Shared as `Arc<tokio::sync::Mutex<JobsDb>>`;
/// its own lock, so a job write never waits for an IAM read.
pub struct JobsDb {
    /// `pub(crate)` for the in-tree `impl JobsDb` blocks of the job stores
    /// (`event_outbox`, `replication::state_store`, `lifecycle::state_store`,
    /// `maintenance::store`).
    pub(crate) conn: Connection,
    local_path: PathBuf,
}

/// What [`JobsDb::adopt_from`] did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AdoptReport {
    /// Rows copied from the config DB (first adoption only), per table.
    pub copied: Vec<(String, i64)>,
    /// Rows deleted from the config DB's node-local tables, per table.
    pub cleared: Vec<(String, i64)>,
    /// The leftovers were deleted after an earlier adoption (a crash between
    /// the copy and the delete, or rows an older binary wrote after a
    /// downgrade).
    pub leftovers: bool,
}

/// The path of the jobs DB: next to the config DB.
pub fn jobs_db_path() -> PathBuf {
    super::config_db_path().with_file_name("deltaglider_jobs.db")
}

fn sql_err(e: impl std::fmt::Display) -> ConfigDbError {
    ConfigDbError::Other(e.to_string())
}

impl JobsDb {
    /// Open an existing jobs DB or create a new one at `local_path`.
    pub fn open_or_create(local_path: &Path, passphrase: &str) -> Result<Self, ConfigDbError> {
        let (conn, schema_objects) = open_encrypted(
            local_path,
            passphrase,
            "Cannot decrypt the jobs database (wrong DGP_CONFIG_DB_KEY or key file?)",
        )?;
        if schema_objects == 0 {
            conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
        }
        Self::migrate(&conn)?;
        Ok(Self {
            conn,
            local_path: local_path.to_path_buf(),
        })
    }

    /// Open the LIVE jobs DB with the config DB keys (a fallback-key file is
    /// re-encrypted with the primary key). A file that no key opens is
    /// renamed to `<name>.unreadable-<unix time>` (kept, never deleted) and
    /// a new DB is created: job state is node-local and rebuilds; IAM is not
    /// in this file.
    pub fn open_live(local_path: &Path, keys: &ConfigDbKeys) -> Result<Self, ConfigDbError> {
        match open_keyed(local_path, keys, Self::open_or_create, |_| Ok(())) {
            Ok((db, OpenedWith::Created)) => {
                info!("Jobs database created: {}", local_path.display());
                Ok(db)
            }
            Ok((db, _)) => Ok(db),
            Err(ConfigDbError::WrongPassphrase(why)) => {
                let parked =
                    unreadable_path(local_path, crate::event_outbox::current_unix_seconds());
                std::fs::rename(local_path, &parked)?;
                error!(
                    "The jobs database {} does not open with the config DB key ({why}); it is \
                     kept as {} and a new one is created. Job history and cursors start empty; \
                     IAM is not affected.",
                    local_path.display(),
                    parked.display()
                );
                Self::open_or_create(local_path, keys.primary.expose())
            }
            Err(e) => Err(e),
        }
    }

    /// A jobs DB in memory, for tests.
    pub fn in_memory(passphrase: &str) -> Result<Self, ConfigDbError> {
        let conn = crate::sqlite_open::open_in_memory()?;
        conn.pragma_update(None, "key", passphrase)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
        Self::migrate(&conn)?;
        Ok(Self {
            conn,
            local_path: PathBuf::from(":memory:"),
        })
    }

    pub fn local_path(&self) -> &Path {
        &self.local_path
    }

    fn migrate(conn: &Connection) -> Result<(), ConfigDbError> {
        let version: i32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version > JOBS_SCHEMA_VERSION {
            return Err(ConfigDbError::SchemaTooNew {
                found: version,
                supported: JOBS_SCHEMA_VERSION,
            });
        }
        if version == JOBS_SCHEMA_VERSION {
            return Ok(());
        }
        let tx = conn.unchecked_transaction()?;
        if version < 1 {
            tx.execute_batch(SCHEMA_V1)?;
        }
        tx.pragma_update(None, "user_version", JOBS_SCHEMA_VERSION)?;
        tx.commit()?;
        Ok(())
    }

    fn adopted(&self) -> Result<bool, ConfigDbError> {
        let v: i64 =
            self.conn
                .query_row("SELECT adopted FROM jobs_meta WHERE id = 1", [], |r| {
                    r.get(0)
                })?;
        Ok(v != 0)
    }

    /// Move the node-local rows of the LIVE config DB `config` (opened with
    /// `key`) into this DB, once, and empty the config DB's copies. Boot
    /// only, before any job subsystem runs. Crash-safe in any order:
    ///
    /// 1. one transaction HERE: clear this DB's tables (not authoritative
    ///    before adoption), copy every row (columns matched by name), raise
    ///    the AUTOINCREMENT sequences to the config DB's, set `adopted`;
    /// 2. one transaction in the config DB: delete those rows;
    /// 3. VACUUM the config DB (best effort).
    ///
    /// A crash before 1 commits re-runs 1 from scratch. After 1 commits,
    /// this DB is the truth: any row left in the config DB (a crash before 2,
    /// or rows an older binary wrote after a downgrade) is deleted.
    pub fn adopt_from(&self, config: &ConfigDb, key: &str) -> Result<AdoptReport, ConfigDbError> {
        let mut report = AdoptReport::default();
        if !self.adopted()? {
            report.copied = self.copy_from_config(config.local_path(), key)?;
        } else {
            report.leftovers = true;
        }
        report.cleared = config.clear_node_local_tables()?;
        let cleared: i64 = report.cleared.iter().map(|(_, n)| n).sum();
        if report.leftovers && cleared == 0 {
            report.leftovers = false;
            return Ok(report);
        }
        if report.leftovers {
            warn!(
                "Config DB {}: deleted {cleared} job row(s) left after the move to {} ({}): \
                 the jobs DB is the source of job state",
                config.local_path().display(),
                self.local_path.display(),
                describe(&report.cleared)
            );
        } else {
            let copied: i64 = report.copied.iter().map(|(_, n)| n).sum();
            info!(
                "Moved {copied} job row(s) from {} to {} ({})",
                config.local_path().display(),
                self.local_path.display(),
                describe(&report.copied)
            );
        }
        if cleared > 0 {
            config.vacuum();
        }
        Ok(report)
    }

    /// Step 1 of [`Self::adopt_from`]: copy, in ONE transaction of this DB.
    pub(crate) fn copy_from_config(
        &self,
        config_path: &Path,
        key: &str,
    ) -> Result<Vec<(String, i64)>, ConfigDbError> {
        let path = config_path.to_string_lossy().replace('\'', "''");
        self.conn.execute_batch(&format!(
            "ATTACH DATABASE '{path}' AS cfg KEY '{}';",
            key.replace('\'', "''")
        ))?;
        // The copy keeps the config DB's rows as they are: a dangling
        // reference there (a row from a binary without FK checks) must not
        // fail the boot. Outside the transaction: the pragma is a no-op
        // inside one.
        self.conn.pragma_update(None, "foreign_keys", "OFF")?;
        let result = super::iam_merge::immediate_tx(&self.conn, |conn| {
            for t in NODE_LOCAL_TABLES.iter().rev() {
                conn.execute(&format!("DELETE FROM main.{t}"), [])?;
            }
            let mut copied = Vec::new();
            for t in NODE_LOCAL_TABLES {
                let cfg_cols = columns(conn, "cfg", t)?;
                if cfg_cols.is_empty() {
                    continue; // a config DB from before this table existed
                }
                let main_cols = columns(conn, "main", t)?;
                let common: Vec<&String> =
                    cfg_cols.iter().filter(|c| main_cols.contains(c)).collect();
                if common.is_empty() {
                    continue;
                }
                let list = common
                    .iter()
                    .map(|c| format!("\"{c}\""))
                    .collect::<Vec<_>>()
                    .join(", ");
                let n = conn.execute(
                    &format!("INSERT INTO main.{t} ({list}) SELECT {list} FROM cfg.{t}"),
                    [],
                )?;
                copied.push((t.to_string(), n as i64));
            }
            // Ids keep growing past every id the config DB handed out: a
            // listener cursor on a pruned outbox points above the last row.
            let has_seq: bool = conn
                .query_row(
                    "SELECT 1 FROM cfg.sqlite_master WHERE name = 'sqlite_sequence'",
                    [],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if has_seq {
                let mut stmt = conn.prepare("SELECT name, seq FROM cfg.sqlite_sequence")?;
                let seqs = stmt
                    .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
                    .collect::<Result<Vec<_>, _>>()?;
                drop(stmt);
                for (name, seq) in seqs {
                    if !NODE_LOCAL_TABLES.contains(&name.as_str()) {
                        continue;
                    }
                    let updated = conn.execute(
                        "UPDATE main.sqlite_sequence SET seq = MAX(seq, ?2) WHERE name = ?1",
                        rusqlite::params![name, seq],
                    )?;
                    if updated == 0 {
                        conn.execute(
                            "INSERT INTO main.sqlite_sequence (name, seq) VALUES (?1, ?2)",
                            rusqlite::params![name, seq],
                        )?;
                    }
                }
            }
            conn.execute("UPDATE main.jobs_meta SET adopted = 1 WHERE id = 1", [])?;
            Ok(copied)
        });
        let _ = self.conn.pragma_update(None, "foreign_keys", "ON");
        let _ = self.conn.execute_batch("DETACH DATABASE cfg;");
        result
    }
}

impl ConfigDb {
    /// Step 2 of [`JobsDb::adopt_from`]: delete every row of the node-local
    /// tables of this (live) config DB, in one transaction, children first.
    /// The tables stay (empty) for older peers and downgrades. Returns the
    /// non-zero counts.
    pub(crate) fn clear_node_local_tables(&self) -> Result<Vec<(String, i64)>, ConfigDbError> {
        super::iam_merge::immediate_tx(&self.conn, |conn| {
            let mut cleared = Vec::new();
            for t in NODE_LOCAL_TABLES.iter().rev() {
                if columns(conn, "main", t)?.is_empty() {
                    continue;
                }
                let n = conn.execute(&format!("DELETE FROM main.{t}"), [])?;
                if n > 0 {
                    cleared.push((t.to_string(), n as i64));
                }
            }
            Ok(cleared)
        })
    }

    /// Rewrite the file without its free pages (once, after the move).
    /// Atomic: a failure (a full disk) leaves the file as it was.
    pub(crate) fn vacuum(&self) {
        let started = std::time::Instant::now();
        let pages = |c: &Connection| -> i64 {
            c.pragma_query_value(None, "page_count", |r| r.get(0))
                .unwrap_or(-1)
        };
        let before = pages(&self.conn);
        match self.conn.execute_batch("VACUUM;") {
            Ok(()) => info!(
                "Config DB {}: vacuumed after the job rows moved out ({before} -> {} pages, {:.1?})",
                self.local_path().display(),
                pages(&self.conn),
                started.elapsed()
            ),
            Err(e) => warn!(
                "Config DB {}: VACUUM after the move failed (the file keeps its free pages): {e}",
                self.local_path().display()
            ),
        }
    }
}

/// The column names of `schema.table`, empty when the table does not exist.
fn columns(conn: &Connection, schema: &str, table: &str) -> Result<Vec<String>, ConfigDbError> {
    if !super::is_safe_sql_ident(schema) || !super::is_safe_sql_ident(table) {
        return Err(sql_err(format!("unsafe SQL identifier: {schema}.{table}")));
    }
    let mut stmt = conn.prepare(&format!("PRAGMA {schema}.table_info({table})"))?;
    let cols = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(cols)
}

fn describe(counts: &[(String, i64)]) -> String {
    counts
        .iter()
        .filter(|(_, n)| *n > 0)
        .map(|(t, n)| format!("{t}: {n}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Pure: where an unreadable jobs DB is kept.
fn unreadable_path(path: &Path, now: i64) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".unreadable-{now}"));
    path.with_file_name(name)
}

/// The final shape of the node-local tables as the config DB had them at
/// schema v29 (same column order, indexes and FKs; `jobs_tables_match_the_
/// config_db` checks it), plus `jobs_meta`.
const SCHEMA_V1: &str = "
CREATE TABLE IF NOT EXISTS jobs_meta (
    id      INTEGER PRIMARY KEY CHECK (id = 1),
    adopted INTEGER NOT NULL DEFAULT 0
);
INSERT OR IGNORE INTO jobs_meta (id, adopted) VALUES (1, 0);

CREATE TABLE IF NOT EXISTS replication_state (
    rule_name               TEXT PRIMARY KEY,
    last_run_at             INTEGER,
    next_due_at             INTEGER NOT NULL,
    last_status             TEXT NOT NULL,
    objects_copied_lifetime INTEGER NOT NULL DEFAULT 0,
    bytes_copied_lifetime   INTEGER NOT NULL DEFAULT 0,
    paused                  INTEGER NOT NULL DEFAULT 0,
    continuation_token      TEXT,
    leader_instance_id      TEXT,
    leader_expires_at       INTEGER
);

CREATE TABLE IF NOT EXISTS replication_run_history (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    rule_name          TEXT NOT NULL,
    triggered_by       TEXT NOT NULL DEFAULT 'unknown',
    started_at         INTEGER NOT NULL,
    finished_at        INTEGER,
    objects_scanned    INTEGER NOT NULL DEFAULT 0,
    objects_copied     INTEGER NOT NULL DEFAULT 0,
    objects_skipped    INTEGER NOT NULL DEFAULT 0,
    objects_deleted    INTEGER NOT NULL DEFAULT 0,
    bytes_copied       INTEGER NOT NULL DEFAULT 0,
    errors             INTEGER NOT NULL DEFAULT 0,
    status             TEXT NOT NULL,
    delta_passthrough  INTEGER NOT NULL DEFAULT 0,
    bytes_egress_saved INTEGER NOT NULL DEFAULT 0,
    reconstructed      INTEGER NOT NULL DEFAULT 0,
    FOREIGN KEY (rule_name) REFERENCES replication_state(rule_name) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_run_history_rule
    ON replication_run_history(rule_name, started_at DESC);

CREATE TABLE IF NOT EXISTS replication_failures (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    rule_name     TEXT NOT NULL,
    run_id        INTEGER,
    occurred_at   INTEGER NOT NULL,
    source_key    TEXT NOT NULL,
    dest_key      TEXT NOT NULL,
    error_message TEXT NOT NULL,
    FOREIGN KEY (rule_name) REFERENCES replication_state(rule_name) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_failures_rule
    ON replication_failures(rule_name, occurred_at DESC);
CREATE INDEX IF NOT EXISTS idx_failures_run
    ON replication_failures(rule_name, run_id, occurred_at DESC);

CREATE TABLE IF NOT EXISTS replication_object_failures (
    rule_name            TEXT NOT NULL,
    source_key           TEXT NOT NULL,
    consecutive_failures INTEGER NOT NULL DEFAULT 0,
    last_error           TEXT,
    last_failed_at       INTEGER,
    PRIMARY KEY (rule_name, source_key)
);

CREATE TABLE IF NOT EXISTS replication_parity_objects (
    rule_name   TEXT NOT NULL,
    side        TEXT NOT NULL,
    dest_key    TEXT NOT NULL,
    sha256      TEXT,
    size        INTEGER NOT NULL,
    etag        TEXT,
    stored_etag TEXT,
    updated_at  INTEGER NOT NULL,
    created_at  INTEGER,
    PRIMARY KEY (rule_name, side, dest_key)
);

CREATE TABLE IF NOT EXISTS replication_parity (
    rule_name           TEXT PRIMARY KEY,
    status              TEXT NOT NULL DEFAULT 'idle',
    scanned_at          INTEGER,
    progress_scanned    INTEGER NOT NULL DEFAULT 0,
    in_sync             INTEGER NOT NULL DEFAULT 0,
    outcome_json        TEXT,
    last_error          TEXT,
    leader_instance_id  TEXT,
    leader_expires_at   INTEGER,
    updated_at          INTEGER,
    progress_total      INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS event_outbox (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    kind            TEXT NOT NULL,
    bucket          TEXT NOT NULL,
    object_key      TEXT NOT NULL,
    source          TEXT NOT NULL,
    occurred_at     INTEGER NOT NULL,
    payload_json    TEXT NOT NULL DEFAULT '{}',
    status          TEXT NOT NULL DEFAULT 'pending',
    attempts        INTEGER NOT NULL DEFAULT 0,
    next_attempt_at INTEGER,
    claimed_by      TEXT,
    claimed_at      INTEGER,
    delivered_at    INTEGER,
    last_error      TEXT,
    created_at      INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE INDEX IF NOT EXISTS idx_event_outbox_status_due
    ON event_outbox(status, next_attempt_at, occurred_at, id);
CREATE INDEX IF NOT EXISTS idx_event_outbox_recent
    ON event_outbox(occurred_at DESC, id DESC);
CREATE INDEX IF NOT EXISTS idx_event_outbox_object
    ON event_outbox(bucket, object_key, occurred_at DESC);

CREATE TABLE IF NOT EXISTS event_deliveries (
    outbox_id   INTEGER NOT NULL REFERENCES event_outbox(id) ON DELETE CASCADE,
    endpoint_id TEXT NOT NULL,
    status      TEXT NOT NULL,
    attempts    INTEGER NOT NULL DEFAULT 0,
    last_error  TEXT,
    updated_at  INTEGER NOT NULL,
    PRIMARY KEY (outbox_id, endpoint_id)
);

CREATE TABLE IF NOT EXISTS listener_cursors (
    listener_name TEXT PRIMARY KEY,
    last_event_id INTEGER NOT NULL DEFAULT 0,
    updated_at    INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE TABLE IF NOT EXISTS lifecycle_state (
    rule_name                 TEXT PRIMARY KEY,
    last_run_at               INTEGER,
    next_due_at               INTEGER NOT NULL,
    last_status               TEXT NOT NULL,
    objects_affected_lifetime INTEGER NOT NULL DEFAULT 0,
    bytes_affected_lifetime   INTEGER NOT NULL DEFAULT 0,
    leader_instance_id        TEXT,
    leader_expires_at         INTEGER,
    continuation_token        TEXT,
    paused                    INTEGER NOT NULL DEFAULT 0,
    cursor_scope              TEXT
);

CREATE TABLE IF NOT EXISTS lifecycle_run_history (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    rule_name        TEXT NOT NULL,
    triggered_by     TEXT NOT NULL DEFAULT 'unknown',
    started_at       INTEGER NOT NULL,
    finished_at      INTEGER,
    objects_scanned  INTEGER NOT NULL DEFAULT 0,
    objects_affected INTEGER NOT NULL DEFAULT 0,
    objects_skipped  INTEGER NOT NULL DEFAULT 0,
    bytes_affected   INTEGER NOT NULL DEFAULT 0,
    errors           INTEGER NOT NULL DEFAULT 0,
    status           TEXT NOT NULL,
    FOREIGN KEY (rule_name) REFERENCES lifecycle_state(rule_name) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_lifecycle_run_history_rule
    ON lifecycle_run_history(rule_name, started_at DESC);

CREATE TABLE IF NOT EXISTS lifecycle_failures (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    rule_name     TEXT NOT NULL,
    run_id        INTEGER,
    occurred_at   INTEGER NOT NULL,
    bucket        TEXT NOT NULL,
    object_key    TEXT NOT NULL,
    error_message TEXT NOT NULL,
    FOREIGN KEY (rule_name) REFERENCES lifecycle_state(rule_name) ON DELETE CASCADE,
    FOREIGN KEY (run_id) REFERENCES lifecycle_run_history(id) ON DELETE SET NULL
);
CREATE INDEX IF NOT EXISTS idx_lifecycle_failures_rule
    ON lifecycle_failures(rule_name, occurred_at DESC);
CREATE INDEX IF NOT EXISTS idx_lifecycle_failures_run
    ON lifecycle_failures(rule_name, run_id, occurred_at DESC);

CREATE TABLE IF NOT EXISTS maintenance_jobs (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    kind               TEXT NOT NULL DEFAULT 'reencrypt',
    bucket             TEXT NOT NULL,
    status             TEXT NOT NULL DEFAULT 'queued',
    phase              TEXT NOT NULL DEFAULT 'counting',
    objects_total      INTEGER,
    objects_done       INTEGER NOT NULL DEFAULT 0,
    objects_skipped    INTEGER NOT NULL DEFAULT 0,
    objects_failed     INTEGER NOT NULL DEFAULT 0,
    bytes_done         INTEGER NOT NULL DEFAULT 0,
    continuation_token TEXT,
    last_error         TEXT,
    triggered_by       TEXT,
    leader_instance_id TEXT,
    leader_expires_at  INTEGER,
    created_at         INTEGER NOT NULL,
    started_at         INTEGER,
    finished_at        INTEGER,
    updated_at         INTEGER NOT NULL,
    params             TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_maint_active_bucket
    ON maintenance_jobs(bucket)
    WHERE status IN ('queued','running','cancelling');

CREATE TABLE IF NOT EXISTS maintenance_failures (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    job_id      INTEGER NOT NULL,
    object_key  TEXT NOT NULL,
    error       TEXT NOT NULL,
    created_at  INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_maint_failures_job
    ON maintenance_failures(job_id, created_at DESC);
";

#[cfg(test)]
mod tests;
