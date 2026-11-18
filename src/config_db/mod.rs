// SPDX-License-Identifier: BUSL-1.1

//! Encrypted configuration database backed by SQLCipher.
//!
//! Stores IAM users and permissions in an encrypted SQLite database.
//! The DB file is cached locally and synced to/from S3 for multi-instance
//! consistency. The encryption key comes from `DGP_CONFIG_DB_KEY` or the key
//! file next to the DB (see [`key`]), never from the admin password.

use crate::iam::{IamUser, Permission};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

/// Encrypted configuration database (SQLCipher).
pub struct ConfigDb {
    /// Raw SQLCipher connection. `pub(crate)` so sibling modules
    /// (e.g. `crate::replication::state_store`) can add IN-TREE
    /// extension methods via `impl ConfigDb` blocks without each
    /// reaching for a dedicated getter. External crates cannot
    /// depend on this — if that changes, gate behind an accessor.
    pub(crate) conn: Connection,
    local_path: PathBuf,
    /// ETag from last S3 download (for change detection during polling)
    s3_etag: Option<String>,
    /// Which connection `conn` is: a new one on every open and reopen (see
    /// [`ConfigDb::change_token`]).
    connection_id: u64,
}

/// Source of [`ConfigDb::connection_id`].
static NEXT_CONNECTION_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn next_connection_id() -> u64 {
    NEXT_CONNECTION_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// A value that changes whenever the DB file changes; see
/// [`ConfigDb::change_token`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ChangeToken {
    connection_id: u64,
    total_changes: u64,
    schema_version: i64,
    page_count: i64,
    freelist_count: i64,
    data_version: i64,
}

/// Schema version — bump when adding migrations.
pub(crate) const SCHEMA_VERSION: i32 = 29;

/// The whole v29 schema. `tests/fixtures/config_db_schema_v29.sql` is its
/// golden dump: a fresh DB must equal it (whitespace aside).
const SCHEMA_V29: &str = r#"
CREATE TABLE auth_providers (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL UNIQUE,
    provider_type TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1,
    priority INTEGER NOT NULL DEFAULT 0,
    display_name TEXT,
    client_id TEXT,
    client_secret TEXT,
    issuer_url TEXT,
    scopes TEXT DEFAULT 'openid email profile',
    extra_config TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    sync_mtime INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE event_deliveries (
    outbox_id INTEGER NOT NULL REFERENCES event_outbox(id) ON DELETE CASCADE,
    endpoint_id TEXT NOT NULL,
    status TEXT NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (outbox_id,endpoint_id)
);
CREATE TABLE event_outbox (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL,
    bucket TEXT NOT NULL,
    object_key TEXT NOT NULL,
    source TEXT NOT NULL,
    occurred_at INTEGER NOT NULL,
    payload_json TEXT NOT NULL DEFAULT '{}',
    status TEXT NOT NULL DEFAULT 'pending',
    attempts INTEGER NOT NULL DEFAULT 0,
    next_attempt_at INTEGER,
    claimed_by TEXT,
    claimed_at INTEGER,
    delivered_at INTEGER,
    last_error TEXT,
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE TABLE external_identities (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    provider_id INTEGER NOT NULL REFERENCES auth_providers(id) ON DELETE CASCADE,
    external_sub TEXT NOT NULL,
    email TEXT,
    display_name TEXT,
    last_login TEXT,
    raw_claims TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    email_verified INTEGER NOT NULL DEFAULT 0,
    sync_mtime INTEGER NOT NULL DEFAULT 0,
    UNIQUE(provider_id,external_sub)
);
CREATE TABLE group_mapping_rules (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    provider_id INTEGER REFERENCES auth_providers(id) ON DELETE CASCADE,
    priority INTEGER NOT NULL DEFAULT 0,
    match_type TEXT NOT NULL,
    match_field TEXT NOT NULL DEFAULT 'email',
    match_value TEXT NOT NULL,
    group_id INTEGER NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    sync_mtime INTEGER NOT NULL DEFAULT 0,
    rule_uid TEXT
);
CREATE TABLE group_members (
    group_id INTEGER NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    PRIMARY KEY (group_id,user_id)
);
CREATE TABLE group_permissions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    group_id INTEGER NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
    actions TEXT NOT NULL,
    resources TEXT NOT NULL,
    effect TEXT NOT NULL DEFAULT 'Allow',
    conditions_json TEXT
);
CREATE TABLE groups (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL UNIQUE,
    description TEXT DEFAULT '',
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    sync_mtime INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE lifecycle_failures (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    rule_name TEXT NOT NULL,
    run_id INTEGER,
    occurred_at INTEGER NOT NULL,
    bucket TEXT NOT NULL,
    object_key TEXT NOT NULL,
    error_message TEXT NOT NULL,
    FOREIGN KEY (rule_name) REFERENCES lifecycle_state(rule_name) ON DELETE CASCADE,
    FOREIGN KEY (run_id) REFERENCES lifecycle_run_history(id) ON DELETE SET NULL
);
CREATE TABLE lifecycle_run_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    rule_name TEXT NOT NULL,
    triggered_by TEXT NOT NULL DEFAULT 'unknown',
    started_at INTEGER NOT NULL,
    finished_at INTEGER,
    objects_scanned INTEGER NOT NULL DEFAULT 0,
    objects_affected INTEGER NOT NULL DEFAULT 0,
    objects_skipped INTEGER NOT NULL DEFAULT 0,
    bytes_affected INTEGER NOT NULL DEFAULT 0,
    errors INTEGER NOT NULL DEFAULT 0,
    status TEXT NOT NULL,
    FOREIGN KEY (rule_name) REFERENCES lifecycle_state(rule_name) ON DELETE CASCADE
);
CREATE TABLE lifecycle_state (
    rule_name TEXT PRIMARY KEY,
    last_run_at INTEGER,
    next_due_at INTEGER NOT NULL,
    last_status TEXT NOT NULL,
    objects_affected_lifetime INTEGER NOT NULL DEFAULT 0,
    bytes_affected_lifetime INTEGER NOT NULL DEFAULT 0,
    leader_instance_id TEXT,
    leader_expires_at INTEGER,
    continuation_token TEXT,
    paused INTEGER NOT NULL DEFAULT 0,
    cursor_scope TEXT
);
CREATE TABLE listener_cursors (
    listener_name TEXT PRIMARY KEY,
    last_event_id INTEGER NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL DEFAULT (unixepoch())
);
CREATE TABLE maintenance_failures (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    job_id INTEGER NOT NULL,
    object_key TEXT NOT NULL,
    error TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE TABLE maintenance_jobs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL DEFAULT 'reencrypt',
    bucket TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'queued',
    phase TEXT NOT NULL DEFAULT 'counting',
    objects_total INTEGER,
    objects_done INTEGER NOT NULL DEFAULT 0,
    objects_skipped INTEGER NOT NULL DEFAULT 0,
    objects_failed INTEGER NOT NULL DEFAULT 0,
    bytes_done INTEGER NOT NULL DEFAULT 0,
    continuation_token TEXT,
    last_error TEXT,
    triggered_by TEXT,
    leader_instance_id TEXT,
    leader_expires_at INTEGER,
    created_at INTEGER NOT NULL,
    started_at INTEGER,
    finished_at INTEGER,
    updated_at INTEGER NOT NULL,
    params TEXT
);
CREATE TABLE permissions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    actions TEXT NOT NULL,
    resources TEXT NOT NULL,
    effect TEXT NOT NULL DEFAULT 'Allow',
    conditions_json TEXT
);
CREATE TABLE replication_failures (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    rule_name TEXT NOT NULL,
    run_id INTEGER,
    occurred_at INTEGER NOT NULL,
    source_key TEXT NOT NULL,
    dest_key TEXT NOT NULL,
    error_message TEXT NOT NULL,
    FOREIGN KEY (rule_name) REFERENCES replication_state(rule_name) ON DELETE CASCADE
);
CREATE TABLE replication_object_failures (
    rule_name TEXT NOT NULL,
    source_key TEXT NOT NULL,
    consecutive_failures INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    last_failed_at INTEGER,
    PRIMARY KEY (rule_name,source_key)
);
CREATE TABLE replication_parity (
    rule_name TEXT PRIMARY KEY,
    status TEXT NOT NULL DEFAULT 'idle',
    scanned_at INTEGER,
    progress_scanned INTEGER NOT NULL DEFAULT 0,
    in_sync INTEGER NOT NULL DEFAULT 0,
    outcome_json TEXT,
    last_error TEXT,
    leader_instance_id TEXT,
    leader_expires_at INTEGER,
    updated_at INTEGER,
    progress_total INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE replication_parity_objects (
    rule_name TEXT NOT NULL,
    side TEXT NOT NULL,
    dest_key TEXT NOT NULL,
    sha256 TEXT,
    size INTEGER NOT NULL,
    etag TEXT,
    stored_etag TEXT,
    updated_at INTEGER NOT NULL,
    created_at INTEGER,
    PRIMARY KEY (rule_name,side,dest_key)
);
CREATE TABLE replication_run_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    rule_name TEXT NOT NULL,
    triggered_by TEXT NOT NULL DEFAULT 'unknown',
    started_at INTEGER NOT NULL,
    finished_at INTEGER,
    objects_scanned INTEGER NOT NULL DEFAULT 0,
    objects_copied INTEGER NOT NULL DEFAULT 0,
    objects_skipped INTEGER NOT NULL DEFAULT 0,
    objects_deleted INTEGER NOT NULL DEFAULT 0,
    bytes_copied INTEGER NOT NULL DEFAULT 0,
    errors INTEGER NOT NULL DEFAULT 0,
    status TEXT NOT NULL,
    delta_passthrough INTEGER NOT NULL DEFAULT 0,
    bytes_egress_saved INTEGER NOT NULL DEFAULT 0,
    reconstructed INTEGER NOT NULL DEFAULT 0,
    FOREIGN KEY (rule_name) REFERENCES replication_state(rule_name) ON DELETE CASCADE
);
CREATE TABLE replication_state (
    rule_name TEXT PRIMARY KEY,
    last_run_at INTEGER,
    next_due_at INTEGER NOT NULL,
    last_status TEXT NOT NULL,
    objects_copied_lifetime INTEGER NOT NULL DEFAULT 0,
    bytes_copied_lifetime INTEGER NOT NULL DEFAULT 0,
    paused INTEGER NOT NULL DEFAULT 0,
    continuation_token TEXT,
    leader_instance_id TEXT,
    leader_expires_at INTEGER
);
CREATE TABLE session_revocations (
    identity TEXT PRIMARY KEY,
    revoked_since INTEGER NOT NULL
);
CREATE TABLE sync_state (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    sync_generation INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE users (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL,
    access_key_id TEXT NOT NULL UNIQUE,
    secret_access_key TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    auth_source TEXT NOT NULL DEFAULT 'local',
    sync_mtime INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX idx_event_outbox_object ON event_outbox(bucket,object_key,occurred_at DESC);
CREATE INDEX idx_event_outbox_recent ON event_outbox(occurred_at DESC,id DESC);
CREATE INDEX idx_event_outbox_status_due ON event_outbox(status,next_attempt_at,occurred_at,id);
CREATE INDEX idx_ext_identity_lookup ON external_identities(provider_id,external_sub);
CREATE INDEX idx_ext_identity_user ON external_identities(user_id);
CREATE INDEX idx_failures_rule ON replication_failures(rule_name,occurred_at DESC);
CREATE INDEX idx_group_mapping_provider ON group_mapping_rules(provider_id);
CREATE INDEX idx_lifecycle_failures_rule ON lifecycle_failures(rule_name,occurred_at DESC);
CREATE INDEX idx_lifecycle_failures_run ON lifecycle_failures(rule_name,run_id,occurred_at DESC);
CREATE INDEX idx_lifecycle_run_history_rule ON lifecycle_run_history(rule_name,started_at DESC);
CREATE UNIQUE INDEX idx_maint_active_bucket ON maintenance_jobs(bucket) WHERE status IN ('queued','running','cancelling');
CREATE INDEX idx_maint_failures_job ON maintenance_failures(job_id,created_at DESC);
CREATE UNIQUE INDEX idx_mapping_rule_uid ON group_mapping_rules(rule_uid);
CREATE INDEX idx_permissions_user ON permissions(user_id);
CREATE INDEX idx_run_history_rule ON replication_run_history(rule_name,started_at DESC);
CREATE INDEX idx_users_access_key ON users(access_key_id);
CREATE UNIQUE INDEX idx_users_name ON users(name);
CREATE TRIGGER trg_auth_providers_mtime_ins AFTER INSERT ON auth_providers WHEN NEW.sync_mtime = 0 BEGIN UPDATE auth_providers SET sync_mtime = CAST((julianday('now') - 2440587.5) * 86400000.0 AS INTEGER) WHERE id = NEW.id; END;
CREATE TRIGGER trg_auth_providers_mtime_upd AFTER UPDATE ON auth_providers WHEN NEW.sync_mtime = OLD.sync_mtime BEGIN UPDATE auth_providers SET sync_mtime = CAST((julianday('now') - 2440587.5) * 86400000.0 AS INTEGER) WHERE id = NEW.id; END;
CREATE TRIGGER trg_external_identities_mtime_ins AFTER INSERT ON external_identities WHEN NEW.sync_mtime = 0 BEGIN UPDATE external_identities SET sync_mtime = CAST((julianday('now') - 2440587.5) * 86400000.0 AS INTEGER) WHERE id = NEW.id; END;
CREATE TRIGGER trg_external_identities_mtime_upd AFTER UPDATE ON external_identities WHEN NEW.sync_mtime = OLD.sync_mtime BEGIN UPDATE external_identities SET sync_mtime = CAST((julianday('now') - 2440587.5) * 86400000.0 AS INTEGER) WHERE id = NEW.id; END;
CREATE TRIGGER trg_group_mapping_rules_mtime_ins AFTER INSERT ON group_mapping_rules WHEN NEW.sync_mtime = 0 BEGIN UPDATE group_mapping_rules SET sync_mtime = CAST((julianday('now') - 2440587.5) * 86400000.0 AS INTEGER) WHERE id = NEW.id; END;
CREATE TRIGGER trg_group_mapping_rules_mtime_upd AFTER UPDATE ON group_mapping_rules WHEN NEW.sync_mtime = OLD.sync_mtime BEGIN UPDATE group_mapping_rules SET sync_mtime = CAST((julianday('now') - 2440587.5) * 86400000.0 AS INTEGER) WHERE id = NEW.id; END;
CREATE TRIGGER trg_group_mapping_rules_uid AFTER INSERT ON group_mapping_rules WHEN NEW.rule_uid IS NULL BEGIN UPDATE group_mapping_rules SET rule_uid = lower(hex(randomblob(16))) WHERE id = NEW.id; END;
CREATE TRIGGER trg_group_permissions_delete_mtime AFTER DELETE ON group_permissions BEGIN UPDATE groups SET sync_mtime = CAST((julianday('now') - 2440587.5) * 86400000.0 AS INTEGER) WHERE id = OLD.group_id; END;
CREATE TRIGGER trg_group_permissions_insert_mtime AFTER INSERT ON group_permissions BEGIN UPDATE groups SET sync_mtime = CAST((julianday('now') - 2440587.5) * 86400000.0 AS INTEGER) WHERE id = NEW.group_id; END;
CREATE TRIGGER trg_group_permissions_update_mtime AFTER UPDATE ON group_permissions BEGIN UPDATE groups SET sync_mtime = CAST((julianday('now') - 2440587.5) * 86400000.0 AS INTEGER) WHERE id = NEW.group_id; END;
CREATE TRIGGER trg_groups_mtime_ins AFTER INSERT ON groups WHEN NEW.sync_mtime = 0 BEGIN UPDATE groups SET sync_mtime = CAST((julianday('now') - 2440587.5) * 86400000.0 AS INTEGER) WHERE id = NEW.id; END;
CREATE TRIGGER trg_groups_mtime_upd AFTER UPDATE ON groups WHEN NEW.sync_mtime = OLD.sync_mtime BEGIN UPDATE groups SET sync_mtime = CAST((julianday('now') - 2440587.5) * 86400000.0 AS INTEGER) WHERE id = NEW.id; END;
CREATE TRIGGER trg_permissions_delete_mtime AFTER DELETE ON permissions BEGIN UPDATE users SET sync_mtime = CAST((julianday('now') - 2440587.5) * 86400000.0 AS INTEGER) WHERE id = OLD.user_id; END;
CREATE TRIGGER trg_permissions_insert_mtime AFTER INSERT ON permissions BEGIN UPDATE users SET sync_mtime = CAST((julianday('now') - 2440587.5) * 86400000.0 AS INTEGER) WHERE id = NEW.user_id; END;
CREATE TRIGGER trg_permissions_update_mtime AFTER UPDATE ON permissions BEGIN UPDATE users SET sync_mtime = CAST((julianday('now') - 2440587.5) * 86400000.0 AS INTEGER) WHERE id = NEW.user_id; END;
CREATE TRIGGER trg_users_mtime_ins AFTER INSERT ON users WHEN NEW.sync_mtime = 0 BEGIN UPDATE users SET sync_mtime = CAST((julianday('now') - 2440587.5) * 86400000.0 AS INTEGER) WHERE id = NEW.id; END;
CREATE TRIGGER trg_users_mtime_upd AFTER UPDATE ON users WHEN NEW.sync_mtime = OLD.sync_mtime BEGIN UPDATE users SET sync_mtime = CAST((julianday('now') - 2440587.5) * 86400000.0 AS INTEGER) WHERE id = NEW.id; END;
INSERT INTO sync_state (id, sync_generation) VALUES (1, 0);
"#;

pub(crate) mod auth_providers;
mod declarative;
mod groups;
pub(crate) mod iam_merge;
pub(crate) mod job_store;
pub mod jobs_db;
pub mod key;
pub mod restore;
mod users;
pub(crate) use users::first_free_user_name;

pub use jobs_db::{jobs_db_path, JobsDb};
pub use key::{ConfigDbKeys, DbKeySource, DbSecret, FallbackKind};

/// How `ConfigDb::open_with_keys` opened the DB.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenedWith {
    /// The file did not exist; it is new, keyed with the primary key.
    Created,
    /// The file opened with the primary key.
    Primary,
    /// The file opened with a fallback key and is now re-encrypted with the
    /// primary key.
    Migrated(FallbackKind),
}

/// Free pages one [`reclaim_free_pages`] call gives back at most (4 MiB at
/// the default page size): the vacuum runs under the DB lock, so each call
/// stays short, and a backlog drains over the next prunes.
const RECLAIM_PAGES_PER_CALL: u32 = 1024;

/// Give free pages back to the disk after a prune. The outbox churns (rows
/// deleted after 24 h) and the parity cache is rewritten: without this the
/// file stayed at its peak size (prod: 338 MB, 58,308 of 82,512 pages
/// free). Needs `auto_vacuum = INCREMENTAL` (see
/// [`convert_to_incremental_vacuum`]); a no-op before. `freelist_count`
/// reads the DB header, so a call without free pages costs nothing.
/// Best-effort: a failure is logged, never returned.
pub(crate) fn reclaim_free_pages(conn: &Connection) {
    let free: i64 = match conn.pragma_query_value(None, "freelist_count", |r| r.get(0)) {
        Ok(n) => n,
        Err(e) => {
            warn!("Config DB: cannot read the free page count: {e}");
            return;
        }
    };
    if free == 0 {
        return;
    }
    // The pragma frees one page per step (one result row each), so it runs
    // to its end here; `execute_batch` would step it once.
    let vacuum = || -> rusqlite::Result<()> {
        let mut stmt = conn.prepare(&format!(
            "PRAGMA incremental_vacuum({RECLAIM_PAGES_PER_CALL})"
        ))?;
        let mut rows = stmt.query([])?;
        while rows.next()?.is_some() {}
        Ok(())
    };
    if let Err(e) = vacuum() {
        warn!("Config DB: incremental vacuum of {free} free pages failed: {e}");
    }
}

/// Whether opening `path` converts it to incremental auto-vacuum: only a
/// `.db` file. The copies that the sync keeps beside the DB
/// (`.db.tmp.<id>`, `.db.sync-base`, `.db.bak`, `.db.rekey.tmp`) are
/// replaced or deleted soon; a VACUUM of each would only cost time.
fn converts_on_open(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "db")
}

/// Convert a DB from an older binary (`auto_vacuum = NONE`, which never
/// gives a page back) to incremental auto-vacuum. That takes one VACUUM:
/// it rewrites the live pages to a temporary file (about the size of the
/// live data, not of the file) and copies them back in one transaction, so
/// a failure (a full disk) leaves the DB as it was, and the next boot
/// tries again. Runs once per DB; the duration is logged.
fn convert_to_incremental_vacuum(conn: &Connection, path: &Path) {
    let pragma = |name: &str| -> i64 {
        conn.pragma_query_value(None, name, |r| r.get(0))
            .unwrap_or(-1)
    };
    if pragma("auto_vacuum") == 2 {
        return;
    }
    let (pages, free) = (pragma("page_count"), pragma("freelist_count"));
    info!(
        "Config DB {}: converting to incremental auto-vacuum ({pages} pages, {free} free); \
         this runs once",
        path.display()
    );
    let started = std::time::Instant::now();
    match conn.execute_batch("PRAGMA auto_vacuum = INCREMENTAL; VACUUM;") {
        Ok(()) => info!(
            "Config DB {}: converted to incremental auto-vacuum in {:.1?} ({pages} -> {} pages)",
            path.display(),
            started.elapsed(),
            pragma("page_count")
        ),
        Err(e) => warn!(
            "Config DB {}: conversion to incremental auto-vacuum failed after {:.1?} (the DB \
             is unchanged; the next start tries again): {e}",
            path.display(),
            started.elapsed()
        ),
    }
}

/// Open the SQLCipher file at `local_path` with `passphrase` and check
/// the key. Returns the connection (foreign keys on, 5 s busy timeout) and
/// the number of schema objects (0 = a new, empty file). A key that does not
/// decrypt the file is `WrongPassphrase` with `wrong_key_context`.
pub(crate) fn open_encrypted(
    local_path: &Path,
    passphrase: &str,
    wrong_key_context: &str,
) -> Result<(Connection, i32), ConfigDbError> {
    if passphrase.is_empty() {
        return Err(ConfigDbError::WrongPassphrase(
            "Config database passphrase must not be empty".to_string(),
        ));
    }

    let conn = crate::sqlite_open::open(local_path)?;

    // Set the encryption key (PRAGMA key must be the first statement)
    conn.pragma_update(None, "key", passphrase)?;
    // Wait up to 5s for locks instead of failing immediately (also for the
    // key check below: a busy DB must not read as a wrong key).
    // Prevents "database is locked" errors during concurrent S3 sync + admin ops.
    conn.pragma_update(None, "busy_timeout", "5000")?;

    // Test that the key is correct by reading the schema
    let schema_objects = conn
        .query_row("SELECT count(*) FROM sqlite_master", [], |r| {
            r.get::<_, i32>(0)
        })
        .map_err(|e| key_check_error(e, wrong_key_context))?;

    // Enable foreign keys (per-connection setting, not persisted)
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok((conn, schema_objects))
}

/// Open (or create) the file at `local_path` with `keys.primary`, through
/// `open`. A file that opens only with a fallback key (a key file that an
/// env key replaces, `DGP_CONFIG_DB_KEY_PREVIOUS`)
/// is re-encrypted with the primary key first, see [`rekey_file`], and its
/// companions follow ([`heal_companions`]). A file that opens with no key is
/// `WrongPassphrase`. The config DB and the jobs DB both open here, so a
/// key rotation moves both.
pub(crate) fn open_keyed<T>(
    local_path: &Path,
    keys: &ConfigDbKeys,
    open: impl Fn(&Path, &str) -> Result<T, ConfigDbError>,
    before_swap: impl FnOnce(&Path) -> Result<(), ConfigDbError>,
) -> Result<(T, OpenedWith), ConfigDbError> {
    let existed = local_path.exists();
    let primary = keys.primary.expose();
    let wrong = match open(local_path, primary) {
        Ok(db) => {
            let how = if existed {
                OpenedWith::Primary
            } else {
                OpenedWith::Created
            };
            // A crash after the DB moved to the primary key, but before
            // its companions did, heals here.
            heal_companions(local_path, keys);
            return Ok((db, how));
        }
        Err(ConfigDbError::WrongPassphrase(msg)) => msg,
        Err(e) => return Err(e),
    };
    for (kind, old) in &keys.fallbacks {
        if !probe_key(local_path, old.expose())? {
            continue;
        }
        rekey_file_hooked(local_path, old.expose(), primary, before_swap)?;
        heal_companions(local_path, keys);
        warn!(
            "{} opened with {}; it is now re-encrypted with {}",
            local_path.display(),
            kind.describe(),
            keys.source.describe()
        );
        let db = open(local_path, primary)?;
        return Ok((db, OpenedWith::Migrated(*kind)));
    }
    Err(ConfigDbError::WrongPassphrase(wrong))
}

/// Compute the path to the IAM config database file.
///
/// Derives the directory from `DGP_CONFIG` (parent of the config file)
/// or falls back to the current working directory.
pub fn config_db_path() -> PathBuf {
    let db_dir = crate::config::process_env("DGP_CONFIG")
        .and_then(|p| std::path::Path::new(&p).parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    db_dir.join("deltaglider_config.db")
}

/// True if `ident` is a safe bare SQL identifier (`[A-Za-z_][A-Za-z0-9_]*`).
///
/// SQLite identifiers (table/column names) cannot be bound as `?` parameters,
/// so migration DDL has to interpolate them into the statement string. All
/// current call sites pass hardcoded literals, but this gate is the
/// defense-in-depth contract: identifiers MUST match this pattern and are
/// never sourced from external input. Refactors that would feed a non-literal
/// here will fail loudly via `ConfigDbError::Other` rather than risk injection.
pub(crate) fn is_safe_sql_ident(ident: &str) -> bool {
    let mut chars = ident.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

impl ConfigDb {
    /// Open an existing DB or create a new one at `local_path`.
    /// The `passphrase` is used as the SQLCipher encryption key.
    pub fn open_or_create(local_path: &Path, passphrase: &str) -> Result<Self, ConfigDbError> {
        let (conn, schema_objects) = open_encrypted(
            local_path,
            passphrase,
            "Cannot decrypt config database (wrong DGP_CONFIG_DB_KEY or key file?)",
        )?;

        // A new DB takes incremental auto-vacuum before its first table; an
        // older one converts after its migration (one VACUUM).
        if schema_objects == 0 {
            conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
        }
        Self::migrate(&conn)?;
        if schema_objects > 0 && converts_on_open(local_path) {
            convert_to_incremental_vacuum(&conn, local_path);
        }

        info!("Config database opened: {}", local_path.display());

        Ok(Self {
            conn,
            local_path: local_path.to_path_buf(),
            s3_etag: None,
            connection_id: next_connection_id(),
        })
    }

    /// Open (or create) the DB with `keys.primary`. A DB that opens only with a
    /// fallback key (a key file that an env key
    /// replaces, or `DGP_CONFIG_DB_KEY_PREVIOUS`) is re-encrypted with the primary key first, see
    /// [`rekey_file`]. A DB that opens with no key is `WrongPassphrase`.
    pub(crate) fn open_with_keys(
        local_path: &Path,
        keys: &ConfigDbKeys,
    ) -> Result<(Self, OpenedWith), ConfigDbError> {
        Self::open_with_keys_hooked(local_path, keys, |_| Ok(()))
    }

    fn open_with_keys_hooked(
        local_path: &Path,
        keys: &ConfigDbKeys,
        before_swap: impl FnOnce(&Path) -> Result<(), ConfigDbError>,
    ) -> Result<(Self, OpenedWith), ConfigDbError> {
        open_keyed(local_path, keys, Self::open_or_create, before_swap)
    }

    /// Create an in-memory DB for testing.
    pub fn in_memory(passphrase: &str) -> Result<Self, ConfigDbError> {
        let conn = crate::sqlite_open::open_in_memory()?;
        conn.pragma_update(None, "key", passphrase)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "auto_vacuum", "INCREMENTAL")?;
        Self::migrate(&conn)?;
        Ok(Self {
            conn,
            local_path: PathBuf::from(":memory:"),
            s3_etag: None,
            connection_id: next_connection_id(),
        })
    }

    /// Create the schema of an empty DB in ONE transaction (a crash leaves it
    /// empty). A DB below `SCHEMA_VERSION` is refused: open it with 2.2.3
    /// first. A DB from a newer binary is refused, never stamped down.
    fn migrate(conn: &Connection) -> Result<(), ConfigDbError> {
        let version: i32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        match migration_plan(version) {
            MigrationPlan::UpToDate => return Ok(()),
            MigrationPlan::TooOld => return Err(ConfigDbError::SchemaTooOld { found: version }),
            MigrationPlan::TooNew => {
                return Err(ConfigDbError::SchemaTooNew {
                    found: version,
                    supported: SCHEMA_VERSION,
                })
            }
            MigrationPlan::Create => {}
        }
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(SCHEMA_V29)?;
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        tx.commit()?;
        debug!("Config DB schema created at version {}", SCHEMA_VERSION);
        Ok(())
    }

    // === Row mapping helpers (single source of truth for field order) ===

    /// Map a row from the users table to an IamUser (without permissions).
    fn user_from_row(row: &rusqlite::Row) -> rusqlite::Result<IamUser> {
        Ok(IamUser {
            id: row.get(0)?,
            name: row.get(1)?,
            access_key_id: row.get(2)?,
            secret_access_key: row.get(3)?,
            enabled: row.get::<_, i32>(4)? != 0,
            created_at: row.get(5)?,
            auth_source: row
                .get::<_, String>(6)
                .unwrap_or_else(|_| "local".to_string()),
            permissions: Vec::new(),
            group_ids: Vec::new(),
            iam_policies: Vec::new(),
        })
    }

    /// Map a row from the permissions table to a Permission.
    fn permission_from_row(row: &rusqlite::Row) -> rusqlite::Result<Permission> {
        let actions_json: String = row.get(1)?;
        let resources_json: String = row.get(2)?;
        let effect: String = row
            .get::<_, String>(3)
            .unwrap_or_else(|_| "Allow".to_string());
        let conditions: Option<serde_json::Value> = row
            .get::<_, Option<String>>(4)
            .unwrap_or(None)
            .and_then(|s| serde_json::from_str(&s).ok());
        Ok(Permission {
            id: row.get(0)?,
            effect,
            actions: serde_json::from_str(&actions_json).unwrap_or_default(),
            resources: serde_json::from_str(&resources_json).unwrap_or_default(),
            conditions,
        })
    }

    /// Load permissions for a user by ID.
    fn load_permissions(&self, user_id: i64) -> Result<Vec<Permission>, ConfigDbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, actions, resources, effect, conditions_json FROM permissions WHERE user_id = ?1",
        )?;
        let perms = stmt
            .query_map(params![user_id], Self::permission_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(perms)
    }

    /// Insert permission rows for a user.
    /// Accepts a `conn` parameter so it can operate within a transaction.
    /// Insert permission rows into a table. Used for both user and group permissions.
    pub(crate) fn insert_permission_rows(
        conn: &Connection,
        table: &str,
        fk_column: &str,
        fk_value: i64,
        permissions: &[Permission],
    ) -> Result<(), ConfigDbError> {
        let sql = format!(
            "INSERT INTO {} ({}, actions, resources, effect, conditions_json) VALUES (?1, ?2, ?3, ?4, ?5)",
            table, fk_column
        );
        for perm in permissions {
            let actions_json = serde_json::to_string(&perm.actions).unwrap_or_default();
            let resources_json = serde_json::to_string(&perm.resources).unwrap_or_default();
            let effect = if perm.effect.is_empty() {
                "Allow"
            } else {
                &perm.effect
            };
            let conditions_json: Option<String> = perm
                .conditions
                .as_ref()
                .map(|c| serde_json::to_string(c).unwrap_or_default());
            conn.execute(
                &sql,
                params![
                    fk_value,
                    actions_json,
                    resources_json,
                    effect,
                    conditions_json
                ],
            )?;
        }
        Ok(())
    }

    fn insert_permissions(
        conn: &Connection,
        user_id: i64,
        permissions: &[Permission],
    ) -> Result<(), ConfigDbError> {
        Self::insert_permission_rows(conn, "permissions", "user_id", user_id, permissions)
    }

    // === S3 Sync ===

    /// Get the local DB file path for uploading to S3.
    pub fn local_path(&self) -> &Path {
        &self.local_path
    }

    /// Get/set the S3 ETag for change detection.
    pub fn s3_etag(&self) -> Option<&str> {
        self.s3_etag.as_deref()
    }

    pub fn set_s3_etag(&mut self, etag: String) {
        self.s3_etag = Some(etag);
    }

    /// Re-open the DB from the local file (after downloading a new version from S3).
    pub fn reopen(&mut self, passphrase: &str) -> Result<(), ConfigDbError> {
        let conn = crate::sqlite_open::open(&self.local_path)?;
        conn.pragma_update(None, "key", passphrase)?;
        // Verify key works
        conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
            r.get::<_, i32>(0)
        })
        .map_err(|e| key_check_error(e, "Cannot decrypt after re-download"))?;
        // Per-connection settings (not persisted in DB)
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "busy_timeout", "5000")?;
        self.conn = conn;
        self.connection_id = next_connection_id();
        info!("Config database re-opened after S3 sync");
        Ok(())
    }

    /// A value that changes whenever the DB file changes, read under the
    /// DB lock. Every write goes through `conn`: a row change moves
    /// `total_changes`, DDL the schema cookie, a vacuum the page and free
    /// counts, a commit by another connection `data_version`, and a reopen
    /// the connection id. The sync upload reads the file without the lock
    /// and keeps the bytes only when the token did not move meanwhile.
    pub(crate) fn change_token(&self) -> Result<ChangeToken, ConfigDbError> {
        let pragma = |name: &str| -> Result<i64, ConfigDbError> {
            Ok(self.conn.pragma_query_value(None, name, |r| r.get(0))?)
        };
        Ok(ChangeToken {
            connection_id: self.connection_id,
            total_changes: self.conn.total_changes(),
            schema_version: pragma("schema_version")?,
            page_count: pragma("page_count")?,
            freelist_count: pragma("freelist_count")?,
            data_version: pragma("data_version")?,
        })
    }

    /// Revoke every session of `identity` (access_key_id or `provider:user_id`)
    /// created at or before `now`. Monotonic MAX-upsert so a later revocation
    /// never moves the epoch backward. Synced cross-instance via `merge_iam_from`.
    pub fn revoke_identity_sessions(&self, identity: &str, now: i64) -> Result<(), ConfigDbError> {
        self.conn.execute(
            "INSERT INTO session_revocations (identity, revoked_since) VALUES (?, ?)
             ON CONFLICT(identity) DO UPDATE SET revoked_since = MAX(revoked_since, excluded.revoked_since)",
            params![identity, now],
        )?;
        Ok(())
    }

    /// The revoke epoch for `identity`, if any. A session is invalid when its
    /// `created_at <= revoked_since`. Cheap PK lookup (called on the auth path).
    pub fn session_revoked_since(&self, identity: &str) -> Result<Option<i64>, ConfigDbError> {
        let v: Option<i64> = self
            .conn
            .query_row(
                "SELECT revoked_since FROM session_revocations WHERE identity = ?",
                params![identity],
                |r| r.get(0),
            )
            .optional()?;
        Ok(v)
    }

    /// Load ALL revocations into a map for the in-memory session-store snapshot
    /// (so `validate()` stays a pure map lookup, no DB hit per request).
    pub fn load_session_revocations(&self) -> Result<Vec<(String, i64)>, ConfigDbError> {
        let mut stmt = self
            .conn
            .prepare("SELECT identity, revoked_since FROM session_revocations")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Rows of IAM state: users, groups, auth providers, mapping rules. Zero
    /// only for a DB that holds no IAM at all (the fresh DB that a key
    /// mismatch boot creates), so recovery can tell it from a working one.
    pub fn iam_row_count(&self) -> Result<usize, ConfigDbError> {
        let n: i64 = self.conn.query_row(
            "SELECT (SELECT count(*) FROM users) + (SELECT count(*) FROM groups)
                  + (SELECT count(*) FROM auth_providers)
                  + (SELECT count(*) FROM group_mapping_rules)",
            [],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }

    /// Re-encrypt the open database in place with a new key.
    pub fn rekey(&self, new_passphrase: &str) -> Result<(), ConfigDbError> {
        self.conn.pragma_update(None, "rekey", new_passphrase)?;
        info!("Config database re-encrypted with new passphrase");
        Ok(())
    }
}

/// True if `key` decrypts the existing DB file at `path`. Reads only: the
/// file is never created or migrated. An empty file holds no DB, so no key
/// opens it. A busy or unreadable file is `Err`, not
/// `false`, so it is never taken for a wrong key.
pub fn probe_key(path: &Path, key: &str) -> Result<bool, ConfigDbError> {
    Ok(probe_schema_version(path, key)?.is_some())
}

/// The schema version of the DB file at `path` when `key` opens it, `None`
/// when it does not. Read-only, like [`probe_key`]: it reads the version a
/// peer wrote BEFORE any migration touches the file.
pub fn probe_schema_version(path: &Path, key: &str) -> Result<Option<i32>, ConfigDbError> {
    use rusqlite::OpenFlags;
    // SQLite reads a zero-byte file as a fresh DB under ANY key, so without
    // this every candidate would "open" an empty backup.
    if std::fs::metadata(path).map_err(ConfigDbError::Io)?.len() == 0 {
        return Ok(None);
    }
    let conn = crate::sqlite_open::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.pragma_update(None, "key", key)?;
    conn.pragma_update(None, "busy_timeout", "5000")?;
    match conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
        r.get::<_, i32>(0)
    }) {
        Ok(_) => Ok(Some(conn.pragma_query_value(
            None,
            "user_version",
            |r| r.get(0),
        )?)),
        Err(e) if is_not_a_database(&e) => Ok(None),
        Err(e) => Err(ConfigDbError::Sqlite(e)),
    }
}

/// Files next to the DB that are encrypted with its key: the config sync's
/// merge base (`config_db_sync::sync_base_path`), and the `.db.bak` that a
/// key-mismatch boot parks. `true` = the file may be removed when no key
/// opens it (a missing merge base only makes the next merge a union; a
/// parked `.db.bak` is recovery data and is never removed).
fn key_companions(db_path: &Path) -> Vec<(PathBuf, bool)> {
    vec![
        (db_path.with_extension("db.sync-base"), true),
        (db_path.with_extension("db.bak"), false),
    ]
}

/// Bring every companion of the DB to the primary key: a companion that
/// opens only with a fallback key is re-encrypted. Runs on EVERY open, so a
/// crash between the DB rekey and the companion rekey heals on the next boot
/// instead of leaving the merge base unreadable for good.
fn heal_companions(db_path: &Path, keys: &ConfigDbKeys) {
    let primary = keys.primary.expose();
    for (c, removable) in key_companions(db_path) {
        // A zero-byte file holds no DB: nothing to re-encrypt.
        let empty = std::fs::metadata(&c).map_or(true, |m| m.len() == 0);
        if empty || probe_key(&c, primary).unwrap_or(true) {
            continue;
        }
        let old = keys
            .fallbacks
            .iter()
            .find(|(_, k)| probe_key(&c, k.expose()).unwrap_or(false));
        let result = match old {
            Some((_, k)) => rekey_file(&c, k.expose(), primary),
            None => Err(ConfigDbError::WrongPassphrase(
                "no config DB key opens it".to_string(),
            )),
        };
        match result {
            Ok(()) => info!("{} is now re-encrypted with the config DB key", c.display()),
            Err(e) if removable => {
                warn!(
                    "{} did not re-encrypt with the config DB key ({e}); removing it",
                    c.display()
                );
                let _ = std::fs::remove_file(&c);
            }
            Err(e) => warn!(
                "{} did not re-encrypt with the config DB key ({e}); it stays as it is",
                c.display()
            ),
        }
    }
}

/// Re-encrypt the DB file at `path` from `old` to `new` without a window in
/// which the original is at risk: rekey a COPY, check that the copy opens
/// with `new`, then rename it over the original (atomic on one filesystem).
/// On any failure the copy is removed and the original stays as it was.
pub fn rekey_file(path: &Path, old: &str, new: &str) -> Result<(), ConfigDbError> {
    rekey_file_hooked(path, old, new, |_| Ok(()))
}

/// `before_swap` runs on the re-encrypted copy before the check and the
/// rename (tests inject failures there).
fn rekey_file_hooked(
    path: &Path,
    old: &str,
    new: &str,
    before_swap: impl FnOnce(&Path) -> Result<(), ConfigDbError>,
) -> Result<(), ConfigDbError> {
    if new.is_empty() {
        return Err(ConfigDbError::WrongPassphrase(
            "Config database key must not be empty".to_string(),
        ));
    }
    let mut tmp_name = path.file_name().unwrap_or_default().to_os_string();
    tmp_name.push(".rekey.tmp");
    let tmp = path.with_file_name(tmp_name);
    // A copy left by an interrupted attempt is never the live DB.
    let _ = std::fs::remove_file(&tmp);
    // Open with the old key first: a read rolls back a hot journal, so the
    // copy below sees a consistent file.
    if !probe_key(path, old)? {
        return Err(ConfigDbError::WrongPassphrase(format!(
            "Cannot re-encrypt {}: the old key does not open it",
            path.display()
        )));
    }
    // A WAL file (the jobs DB) keeps committed pages in `<path>-wal` until a
    // checkpoint: a copy of the main file alone would miss them. Move them
    // into the main file and empty the WAL BEFORE the copy.
    checkpoint_wal(path, old)?;
    let result = (|| -> Result<(), ConfigDbError> {
        std::fs::copy(path, &tmp).map_err(ConfigDbError::Io)?;
        {
            let conn = crate::sqlite_open::open(&tmp)?;
            conn.pragma_update(None, "key", old)?;
            conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
                r.get::<_, i32>(0)
            })?;
            // The copy is rekeyed on the rollback journal: it leaves no
            // `-wal` of its own beside it, and the next open of the live
            // file sets WAL again.
            conn.pragma_update_and_check(None, "journal_mode", "DELETE", |r| {
                r.get::<_, String>(0)
            })?;
            conn.pragma_update(None, "rekey", new)?;
        }
        before_swap(&tmp)?;
        if !probe_key(&tmp, new)? {
            return Err(ConfigDbError::Other(
                "the re-encrypted copy does not open with the new key".to_string(),
            ));
        }
        std::fs::File::open(&tmp)
            .and_then(|f| f.sync_all())
            .map_err(ConfigDbError::Io)?;
        std::fs::rename(&tmp, path).map_err(ConfigDbError::Io)?;
        // The old file's WAL and shared-memory index (empty after the
        // checkpoint) do not belong to the new file: SQLite would replay
        // frames of another file into it.
        for suffix in ["-wal", "-shm"] {
            let mut name = path.file_name().unwrap_or_default().to_os_string();
            name.push(suffix);
            let _ = std::fs::remove_file(path.with_file_name(name));
        }
        if let Some(dir) = path.parent() {
            // Make the rename durable; best effort (not every FS allows it).
            let _ = std::fs::File::open(dir).and_then(|d| d.sync_all());
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Move every committed page of a WAL file into the main file and empty the
/// `-wal` (`wal_checkpoint(TRUNCATE)`). A no-op for a file in the rollback
/// journal. A checkpoint that cannot finish (a reader holds the WAL: `busy`)
/// is an error: the caller must not copy a main file that lacks pages.
fn checkpoint_wal(path: &Path, key: &str) -> Result<(), ConfigDbError> {
    let conn = crate::sqlite_open::open(path)?;
    conn.pragma_update(None, "key", key)?;
    conn.pragma_update(None, "busy_timeout", "5000")?;
    let (busy, _log, _moved): (i64, i64, i64) =
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
    if busy != 0 {
        return Err(ConfigDbError::Other(format!(
            "Cannot re-encrypt {}: its write-ahead log is in use and could not be \
             checkpointed",
            path.display()
        )));
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum MigrationPlan {
    UpToDate,
    /// An empty DB (`user_version` 0).
    Create,
    TooOld,
    TooNew,
}

/// Pure: what `migrate` does for a DB at `version`.
fn migration_plan(version: i32) -> MigrationPlan {
    match version.cmp(&SCHEMA_VERSION) {
        std::cmp::Ordering::Equal => MigrationPlan::UpToDate,
        std::cmp::Ordering::Less if version == 0 => MigrationPlan::Create,
        std::cmp::Ordering::Less => MigrationPlan::TooOld,
        std::cmp::Ordering::Greater => MigrationPlan::TooNew,
    }
}

/// Map a failed key-check read. Only "file is not a database" means the key is
/// wrong (SQLCipher cannot decrypt page 1). Anything else (busy, I/O) is a
/// plain SQLite error: the caller must NOT park the DB as a mismatch backup.
fn key_check_error(e: rusqlite::Error, context: &str) -> ConfigDbError {
    if is_not_a_database(&e) {
        ConfigDbError::WrongPassphrase(format!("{context}: {e}"))
    } else {
        ConfigDbError::Sqlite(e)
    }
}

fn is_not_a_database(e: &rusqlite::Error) -> bool {
    matches!(e, rusqlite::Error::SqliteFailure(err, _)
        if err.code == rusqlite::ffi::ErrorCode::NotADatabase)
}

/// Errors from the config database.
#[derive(Debug)]
pub enum ConfigDbError {
    Sqlite(rusqlite::Error),
    WrongPassphrase(String),
    /// The DB was written by a newer binary (schema `found` > `supported`).
    SchemaTooNew {
        found: i32,
        supported: i32,
    },
    /// The DB was written by a release before schema v29 (`found` < 29).
    SchemaTooOld {
        found: i32,
    },
    NotFound(String),
    Io(std::io::Error),
    /// Structural / invariant violations detected by reconcile helpers.
    /// Used for "validation should have caught this" defence-in-depth
    /// cases inside the transaction.
    Other(String),
    /// The write would leave the DB in a state the caller refuses (HTTP
    /// 409): nothing was committed.
    Refused(String),
    /// A synced copy with a lower sync generation than the last one this node
    /// merged: not merged.
    RollbackRefused {
        detail: String,
    },
}

impl std::fmt::Display for ConfigDbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sqlite(e) => write!(f, "SQLite error: {}", e),
            Self::WrongPassphrase(msg) => write!(f, "{}", msg),
            Self::SchemaTooNew { found, supported } => write!(
                f,
                "config DB schema v{found} is newer than this binary supports (v{supported}); \
                 upgrade the binary or restore a backup"
            ),
            Self::SchemaTooOld { found } => write!(
                f,
                "config DB schema v{found} is older than this binary supports (v{SCHEMA_VERSION}); \
                 open it with DeltaGlider Proxy 2.2.3 first, then upgrade"
            ),
            Self::NotFound(what) => write!(f, "Not found: {}", what),
            Self::Io(e) => write!(f, "I/O error: {}", e),
            Self::Other(msg) | Self::Refused(msg) => write!(f, "{}", msg),
            Self::RollbackRefused { detail } => {
                write!(f, "{detail}: refusing it as a rollback")
            }
        }
    }
}

impl std::error::Error for ConfigDbError {}

/// Coarse classification of a `rusqlite::Error` at a query site.
///
/// Pure: maps the raw error to the three categories call sites actually
/// branch on — "row not found", "UNIQUE constraint conflict", and
/// "everything else". Lets the per-query handlers attach their own
/// context string (`"Auth provider ID 3"`) without each re-implementing
/// the `match e { QueryReturnedNoRows => …, … }` boilerplate, and gives
/// us one place to unit-test the discrimination truth table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqliteErrorClass {
    /// `SELECT … query_row` matched zero rows.
    NotFound,
    /// A UNIQUE constraint was violated (duplicate key on INSERT/UPDATE).
    Conflict,
    /// Any other SQLite error.
    Other,
}

/// Classify a `rusqlite::Error` into [`SqliteErrorClass`]. Pure fn —
/// see the variant docs. The UNIQUE-constraint detection inspects the
/// extended error code (`ErrorCode::ConstraintViolation`) so it doesn't
/// depend on the human-readable message text.
pub fn classify_sqlite_error(e: &rusqlite::Error) -> SqliteErrorClass {
    use rusqlite::ffi::ErrorCode;
    match e {
        rusqlite::Error::QueryReturnedNoRows => SqliteErrorClass::NotFound,
        rusqlite::Error::SqliteFailure(err, msg) => {
            // A UNIQUE/PRIMARY-KEY violation surfaces as a
            // ConstraintViolation extended code; the message (when
            // present) contains "UNIQUE constraint failed".
            let is_unique = err.code == ErrorCode::ConstraintViolation
                && msg
                    .as_deref()
                    .map(|m| m.contains("UNIQUE constraint failed"))
                    .unwrap_or(true);
            if is_unique {
                SqliteErrorClass::Conflict
            } else {
                SqliteErrorClass::Other
            }
        }
        _ => SqliteErrorClass::Other,
    }
}

impl From<rusqlite::Error> for ConfigDbError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Sqlite(e)
    }
}

impl From<std::io::Error> for ConfigDbError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_sqlite_error_not_found() {
        let e = rusqlite::Error::QueryReturnedNoRows;
        assert_eq!(classify_sqlite_error(&e), SqliteErrorClass::NotFound);
    }

    #[test]
    fn is_safe_sql_ident_accepts_bare_identifiers() {
        for ok in [
            "lifecycle_state",
            "objects_affected_lifetime",
            "_leading_underscore",
            "Mixed_Case123",
            "a",
        ] {
            assert!(is_safe_sql_ident(ok), "{ok:?} should be accepted");
        }
    }

    #[test]
    fn is_safe_sql_ident_rejects_unsafe_input() {
        for bad in [
            "",               // empty
            "1leading_digit", // can't start with a digit
            "has space",      // whitespace
            "drop;table",     // statement separator
            "col\"quoted",    // quote
            "name--comment",  // SQL comment dashes
            "tbl(arg)",       // parens
            "naïve",          // non-ascii
        ] {
            assert!(!is_safe_sql_ident(bad), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn classify_sqlite_error_conflict_on_unique_violation() {
        // Drive a real UNIQUE(access_key_id) violation through the DB so we
        // exercise the genuine rusqlite error shape, not a hand-built one.
        let db = ConfigDb::in_memory("test-pass").unwrap();
        let perms = vec![];
        db.create_user("alice", "AKDUP1234567", "secret123456", true, &perms)
            .unwrap();
        // Same access_key_id → UNIQUE constraint failure.
        let err = db
            .create_user("bob", "AKDUP1234567", "secret654321", true, &perms)
            .expect_err("duplicate access key must fail");
        match err {
            ConfigDbError::Sqlite(ref sqlite_err) => {
                assert_eq!(
                    classify_sqlite_error(sqlite_err),
                    SqliteErrorClass::Conflict,
                    "duplicate key should classify as Conflict (got {sqlite_err:?})"
                );
            }
            other => panic!("expected ConfigDbError::Sqlite, got {other:?}"),
        }
    }

    #[test]
    fn session_revocation_is_monotonic_max_upsert() {
        let db = ConfigDb::in_memory("test-pass").unwrap();
        assert_eq!(db.session_revoked_since("AKIA1").unwrap(), None);

        db.revoke_identity_sessions("AKIA1", 100).unwrap();
        assert_eq!(db.session_revoked_since("AKIA1").unwrap(), Some(100));

        // A later revoke moves the epoch forward.
        db.revoke_identity_sessions("AKIA1", 200).unwrap();
        assert_eq!(db.session_revoked_since("AKIA1").unwrap(), Some(200));

        // An EARLIER timestamp never moves it backward (monotonic MAX).
        db.revoke_identity_sessions("AKIA1", 150).unwrap();
        assert_eq!(db.session_revoked_since("AKIA1").unwrap(), Some(200));

        // Distinct identities are independent; load returns all.
        db.revoke_identity_sessions("goog:7", 50).unwrap();
        let mut rows = db.load_session_revocations().unwrap();
        rows.sort();
        assert_eq!(
            rows,
            vec![("AKIA1".to_string(), 200), ("goog:7".to_string(), 50)]
        );
    }

    #[test]
    fn classify_sqlite_error_other_for_non_constraint() {
        // A non-constraint failure (e.g. malformed SQL) classifies as Other.
        let db = ConfigDb::in_memory("test-pass").unwrap();
        let err = db
            .conn
            .execute("SELECT * FROM definitely_not_a_table", [])
            .expect_err("bad SQL must fail");
        assert_eq!(classify_sqlite_error(&err), SqliteErrorClass::Other);
    }

    #[test]
    fn test_create_and_load_user() {
        let db = ConfigDb::in_memory("test-pass").unwrap();

        let perms = vec![Permission {
            id: 0,
            effect: "Allow".into(),
            actions: vec!["read".into(), "write".into()],
            resources: vec!["releases/*".into()],
            conditions: None,
        }];

        let user = db
            .create_user("ci-bot", "AKCIBOT12345", "secret123", true, &perms)
            .unwrap();

        assert_eq!(user.name, "ci-bot");
        assert_eq!(user.access_key_id, "AKCIBOT12345");
        assert!(user.enabled);
        assert_eq!(user.permissions.len(), 1);
        assert_eq!(user.permissions[0].actions, vec!["read", "write"]);
        assert_eq!(user.permissions[0].resources, vec!["releases/*"]);
    }

    #[test]
    fn test_load_all_users() {
        let db = ConfigDb::in_memory("test-pass").unwrap();

        db.create_user("admin", "AKADMIN1", "s1", true, &[])
            .unwrap();
        db.create_user("viewer", "AKVIEW01", "s2", false, &[])
            .unwrap();

        let users = db.load_users().unwrap();
        assert_eq!(users.len(), 2);
        assert_eq!(users[0].name, "admin");
        assert_eq!(users[1].name, "viewer");
        assert!(!users[1].enabled);
    }

    #[test]
    fn test_update_user() {
        let db = ConfigDb::in_memory("test-pass").unwrap();

        let user = db
            .create_user("old-name", "AKTEST01", "secret", true, &[])
            .unwrap();

        let updated = db
            .update_user(user.id, Some("new-name"), Some(false), None)
            .unwrap();

        assert_eq!(updated.name, "new-name");
        assert!(!updated.enabled);
    }

    #[test]
    fn test_update_permissions() {
        let db = ConfigDb::in_memory("test-pass").unwrap();

        let initial_perms = vec![Permission {
            id: 0,
            effect: "Allow".into(),
            actions: vec!["read".into()],
            resources: vec!["*".into()],
            conditions: None,
        }];
        let user = db
            .create_user("user1", "AKUSER01", "secret", true, &initial_perms)
            .unwrap();

        // Replace with new permissions
        let new_perms = vec![
            Permission {
                id: 0,
                effect: "Allow".into(),
                actions: vec!["read".into(), "write".into()],
                resources: vec!["releases/*".into()],
                conditions: None,
            },
            Permission {
                id: 0,
                effect: "Allow".into(),
                actions: vec!["list".into()],
                resources: vec!["*".into()],
                conditions: None,
            },
        ];
        let updated = db
            .update_user(user.id, None, None, Some(&new_perms))
            .unwrap();

        assert_eq!(updated.permissions.len(), 2);
        assert_eq!(updated.permissions[0].actions, vec!["read", "write"]);
    }

    #[test]
    fn test_delete_user_cascades_permissions() {
        let db = ConfigDb::in_memory("test-pass").unwrap();

        let perms = vec![Permission {
            id: 0,
            effect: "Allow".into(),
            actions: vec!["read".into()],
            resources: vec!["*".into()],
            conditions: None,
        }];
        let user = db
            .create_user("to-delete", "AKDEL001", "secret", true, &perms)
            .unwrap();

        db.delete_user(user.id).unwrap();

        let users = db.load_users().unwrap();
        assert!(users.is_empty());

        // Verify permissions were cascade-deleted
        let perm_count: i32 = db
            .conn
            .query_row("SELECT count(*) FROM permissions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(perm_count, 0);
    }

    #[test]
    fn test_rotate_keys() {
        let db = ConfigDb::in_memory("test-pass").unwrap();

        let user = db
            .create_user("user1", "AKOLD001", "old-secret", true, &[])
            .unwrap();

        let rotated = db
            .update_user_with_keys(user.id, None, None, None, Some(("AKNEW001", "new-secret")))
            .unwrap();

        assert_eq!(rotated.access_key_id, "AKNEW001");
        assert_eq!(rotated.secret_access_key, "new-secret");
    }

    #[test]
    fn test_lookup_by_access_key() {
        let db = ConfigDb::in_memory("test-pass").unwrap();

        db.create_user("found-user", "AKFIND01", "secret", true, &[])
            .unwrap();

        let found = db.get_user_by_access_key("AKFIND01").unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "found-user");

        let missing = db.get_user_by_access_key("AKNOTHERE").unwrap();
        assert!(missing.is_none());
    }

    #[test]
    fn test_duplicate_access_key_rejected() {
        let db = ConfigDb::in_memory("test-pass").unwrap();

        db.create_user("user1", "AKDUPE01", "s1", true, &[])
            .unwrap();
        let result = db.create_user("user2", "AKDUPE01", "s2", true, &[]);

        assert!(result.is_err(), "Duplicate access_key_id should fail");
    }

    #[test]
    fn test_wrong_passphrase_detected() {
        // Create a DB with one passphrase
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");

        {
            let _db = ConfigDb::open_or_create(&path, "correct-password").unwrap();
        }

        // Try to open with wrong passphrase
        let result = ConfigDb::open_or_create(&path, "wrong-password");
        assert!(
            matches!(result, Err(ConfigDbError::WrongPassphrase(_))),
            "Wrong passphrase should be detected, got: {}",
            result
                .err()
                .map(|e| e.to_string())
                .unwrap_or_else(|| "Ok".into())
        );
    }

    /// Normalised `sqlite_master` of a DB: one `;`-ended statement per line,
    /// whitespace collapsed and removed around `(`, `)` and `,`.
    fn schema_dump(conn: &Connection) -> String {
        let mut stmt = conn
            .prepare(
                "SELECT type, sql FROM sqlite_master
                  WHERE name NOT LIKE 'sqlite_%' AND sql IS NOT NULL
                  ORDER BY CASE type WHEN 'table' THEN 0 WHEN 'index' THEN 1 ELSE 2 END, name",
            )
            .unwrap();
        let rows: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .map(|sql| normalise_sql(&sql.unwrap()))
            .collect();
        let mut out = rows.join(";\n");
        out.push_str(";\n");
        out
    }

    fn normalise_sql(sql: &str) -> String {
        let mut s = sql.split_whitespace().collect::<Vec<_>>().join(" ");
        for p in ["( ", " )", " ,", ", "] {
            s = s.replace(p, p.trim());
        }
        s
    }

    /// A fresh DB has exactly the schema of the golden v29 dump.
    #[test]
    fn fresh_db_equals_the_golden_v29_schema() {
        let db = ConfigDb::in_memory("pw").unwrap();
        let golden = include_str!("../../tests/fixtures/config_db_schema_v29.sql");
        assert_eq!(schema_dump(&db.conn), golden);
    }

    #[test]
    fn migration_plan_truth_table() {
        assert_eq!(migration_plan(0), MigrationPlan::Create);
        assert_eq!(migration_plan(1), MigrationPlan::TooOld);
        assert_eq!(migration_plan(SCHEMA_VERSION - 1), MigrationPlan::TooOld);
        assert_eq!(migration_plan(SCHEMA_VERSION), MigrationPlan::UpToDate);
        assert_eq!(migration_plan(SCHEMA_VERSION + 1), MigrationPlan::TooNew);
    }

    /// D14: a DB written by a NEWER binary must not open (and must not be
    /// stamped down to our version — the newer binary would then skip its
    /// own migrations on the next upgrade).
    #[test]
    fn open_refuses_newer_schema_and_keeps_its_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("newer.db");
        {
            let db = ConfigDb::open_or_create(&path, "pw").unwrap();
            db.conn
                .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
                .unwrap();
        }
        let err = ConfigDb::open_or_create(&path, "pw")
            .err()
            .expect("must refuse");
        assert!(
            matches!(err, ConfigDbError::SchemaTooNew { .. }),
            "expected SchemaTooNew, got: {err}"
        );
        let conn = crate::sqlite_open::open(&path).unwrap();
        conn.pragma_update(None, "key", "pw").unwrap();
        let v: i32 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION + 1, "version must not be stamped down");
    }

    /// Build a fully migrated DB, then stamp `version` on it (simulates an
    /// older DB, or a crash after some DDL ran but before the stamp).
    fn db_stamped_at(path: &Path, version: i32) -> Connection {
        drop(ConfigDb::open_or_create(path, "pw").unwrap());
        let conn = crate::sqlite_open::open(path).unwrap();
        conn.pragma_update(None, "key", "pw").unwrap();
        conn.pragma_update(None, "user_version", version).unwrap();
        conn
    }

    /// A DB below v29 is refused with `SchemaTooOld`, and its version stays.
    #[test]
    fn open_refuses_an_older_schema_and_keeps_its_version() {
        for old in [1, 21, SCHEMA_VERSION - 1] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("old.db");
            drop(db_stamped_at(&path, old));
            let err = ConfigDb::open_or_create(&path, "pw")
                .err()
                .expect("must refuse");
            assert!(
                matches!(err, ConfigDbError::SchemaTooOld { found } if found == old),
                "expected SchemaTooOld for v{old}, got: {err}"
            );
            assert!(err.to_string().contains("2.2.3"), "{err}");
            let conn = crate::sqlite_open::open(&path).unwrap();
            conn.pragma_update(None, "key", "pw").unwrap();
            let v: i32 = conn
                .pragma_query_value(None, "user_version", |r| r.get(0))
                .unwrap();
            assert_eq!(v, old, "version must not be stamped up");
        }
    }

    /// A new DB starts with its one `sync_state` row at generation 0.
    #[test]
    fn a_new_db_has_the_sync_state_row() {
        let db = ConfigDb::in_memory("pw").unwrap();
        assert_eq!(db.sync_generation().unwrap(), 0);
        assert_eq!(db.next_sync_generation(None, "pw").unwrap(), 1);
    }

    fn auto_vacuum_mode(conn: &Connection) -> i64 {
        conn.pragma_query_value(None, "auto_vacuum", |r| r.get(0))
            .unwrap()
    }

    /// A new DB is created with incremental auto-vacuum, so a prune can
    /// give pages back without a full VACUUM.
    #[test]
    fn a_new_db_has_incremental_auto_vacuum() {
        let dir = tempfile::tempdir().unwrap();
        let db = ConfigDb::open_or_create(&dir.path().join("new.db"), "pw").unwrap();
        assert_eq!(auto_vacuum_mode(&db.conn), 2, "auto_vacuum=INCREMENTAL");
    }

    /// A DB from an older binary (auto_vacuum=NONE, its free pages never
    /// returned) is converted once when it opens: one VACUUM, which also
    /// drops the free pages it collected.
    #[test]
    fn an_old_db_converts_to_incremental_auto_vacuum_on_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.db");
        {
            let db = ConfigDb::open_or_create(&path, "pw").unwrap();
            db.conn
                .execute_batch(
                    "PRAGMA auto_vacuum = NONE; VACUUM;
                     CREATE TABLE junk (b BLOB);
                     WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 500)
                     INSERT INTO junk SELECT randomblob(4000) FROM n;
                     DROP TABLE junk;",
                )
                .unwrap();
            assert_eq!(auto_vacuum_mode(&db.conn), 0);
            let free: i64 = db
                .conn
                .pragma_query_value(None, "freelist_count", |r| r.get(0))
                .unwrap();
            assert!(free >= 500, "{free}");
            db.create_user("u1", "AKU1000000001", "s1", true, &[])
                .unwrap();
        }
        let db = ConfigDb::open_or_create(&path, "pw").unwrap();
        assert_eq!(auto_vacuum_mode(&db.conn), 2, "not converted");
        let free: i64 = db
            .conn
            .pragma_query_value(None, "freelist_count", |r| r.get(0))
            .unwrap();
        assert_eq!(free, 0, "the conversion VACUUM keeps no free page");
        assert_eq!(db.load_users().unwrap().len(), 1, "rows survive");
    }

    /// D14: only "not a database" (wrong key / not SQLCipher) is a passphrase
    /// mismatch. A busy DB is NOT — treating it as one moves the good DB to
    /// `.db.bak` and locks the S3 API.
    #[test]
    fn busy_db_is_not_a_wrong_passphrase() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("busy.db");
        drop(ConfigDb::open_or_create(&path, "pw").unwrap());
        let holder = crate::sqlite_open::open(&path).unwrap();
        holder.pragma_update(None, "key", "pw").unwrap();
        holder.execute_batch("BEGIN EXCLUSIVE;").unwrap();
        let err = ConfigDb::open_or_create(&path, "pw")
            .err()
            .expect("locked DB must not open");
        assert!(
            !matches!(err, ConfigDbError::WrongPassphrase(_)),
            "a busy DB was reported as a wrong passphrase: {err}"
        );
        holder.execute_batch("ROLLBACK;").unwrap();
    }

    #[test]
    fn merge_iam_from_rejects_wrong_passphrase() {
        let dir = tempfile::tempdir().unwrap();
        let local_path = dir.path().join("local.db");
        let peer_path = dir.path().join("peer.db");
        let local = ConfigDb::open_or_create(&local_path, "local-pass").unwrap();
        local
            .create_user("keep", "AKKEEP000001", "secret0000001", true, &[])
            .unwrap();
        {
            ConfigDb::open_or_create(&peer_path, "DIFFERENT-pass").unwrap();
        }
        // Merging a peer encrypted with a different key must fail and leave the
        // local IAM untouched (no half-applied wipe).
        assert!(local
            .merge_iam_from(&peer_path, None, "local-pass")
            .is_err());
        assert_eq!(
            local.load_users().unwrap().len(),
            1,
            "local IAM intact on failure"
        );
    }

    #[test]
    fn merge_iam_from_rejects_schema_version_drift() {
        // Review fix: INSERT..SELECT * is positional, so a peer on a different
        // schema version (rolling upgrade) must be REFUSED, not corrupt the copy.
        let dir = tempfile::tempdir().unwrap();
        let local_path = dir.path().join("local.db");
        let peer_path = dir.path().join("peer.db");
        let pass = "shared";
        let local = ConfigDb::open_or_create(&local_path, pass).unwrap();
        local
            .create_user("keep", "AKKEEP000001", "secret0000001", true, &[])
            .unwrap();
        {
            let peer = ConfigDb::open_or_create(&peer_path, pass).unwrap();
            peer.conn
                .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
                .unwrap();
        }
        let err = local.merge_iam_from(&peer_path, None, pass).unwrap_err();
        assert!(
            err.to_string().contains("schema"),
            "expected a schema-version rejection, got: {err}"
        );
        assert_eq!(
            local.load_users().unwrap().len(),
            1,
            "local IAM intact on drift"
        );
    }

    #[test]
    fn test_delete_nonexistent_user_returns_error() {
        let db = ConfigDb::in_memory("test-pass").unwrap();
        let result = db.delete_user(99999);
        assert!(matches!(result, Err(ConfigDbError::NotFound(_))));
    }

    #[test]
    fn test_empty_passphrase_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let result = ConfigDb::open_or_create(&path, "");
        assert!(
            matches!(result, Err(ConfigDbError::WrongPassphrase(_))),
            "Empty passphrase should be rejected"
        );
    }

    #[test]
    fn test_create_and_load_group() {
        let db = ConfigDb::in_memory("test-pass").unwrap();

        let perms = vec![Permission {
            id: 0,
            effect: "Allow".into(),
            actions: vec!["read".into(), "list".into()],
            resources: vec!["*".into()],
            conditions: None,
        }];

        let group = db
            .create_group("readers", "Read-only access", &perms)
            .unwrap();

        assert_eq!(group.name, "readers");
        assert_eq!(group.description, "Read-only access");
        assert_eq!(group.permissions.len(), 1);
        assert_eq!(group.permissions[0].actions, vec!["read", "list"]);
        assert!(group.member_ids.is_empty());

        let groups = db.load_groups().unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, "readers");
    }

    #[test]
    fn test_group_membership() {
        let db = ConfigDb::in_memory("test-pass").unwrap();

        let group = db.create_group("devs", "", &[]).unwrap();
        let user = db
            .create_user("alice", "AKALICE1", "secret", true, &[])
            .unwrap();

        db.add_user_to_group(group.id, user.id).unwrap();

        let members = db.get_group_members(group.id).unwrap();
        assert_eq!(members, vec![user.id]);

        let user_groups = db.get_user_group_ids(user.id).unwrap();
        assert_eq!(user_groups, vec![group.id]);

        // Reload user and verify group_ids populated
        let reloaded = db.load_users().unwrap();
        assert_eq!(reloaded[0].group_ids, vec![group.id]);

        // Remove membership
        db.remove_user_from_group(group.id, user.id).unwrap();
        let members = db.get_group_members(group.id).unwrap();
        assert!(members.is_empty());
    }

    #[test]
    fn test_update_group() {
        let db = ConfigDb::in_memory("test-pass").unwrap();

        let perms = vec![Permission {
            id: 0,
            effect: "Allow".into(),
            actions: vec!["read".into()],
            resources: vec!["*".into()],
            conditions: None,
        }];
        let group = db.create_group("old-name", "old desc", &perms).unwrap();

        let new_perms = vec![Permission {
            id: 0,
            effect: "Allow".into(),
            actions: vec!["read".into(), "write".into()],
            resources: vec!["releases/*".into()],
            conditions: None,
        }];
        let updated = db
            .update_group(
                group.id,
                Some("new-name"),
                Some("new desc"),
                Some(&new_perms),
            )
            .unwrap();

        assert_eq!(updated.name, "new-name");
        assert_eq!(updated.description, "new desc");
        assert_eq!(updated.permissions.len(), 1);
        assert_eq!(updated.permissions[0].actions, vec!["read", "write"]);
    }

    #[test]
    fn test_delete_group_cascades() {
        let db = ConfigDb::in_memory("test-pass").unwrap();

        let perms = vec![Permission {
            id: 0,
            effect: "Allow".into(),
            actions: vec!["read".into()],
            resources: vec!["*".into()],
            conditions: None,
        }];
        let group = db.create_group("to-delete", "", &perms).unwrap();
        let user = db
            .create_user("bob", "AKBOB001", "secret", true, &[])
            .unwrap();
        db.add_user_to_group(group.id, user.id).unwrap();

        db.delete_group(group.id).unwrap();

        // Group gone
        let groups = db.load_groups().unwrap();
        assert!(groups.is_empty());

        // Membership gone
        let user_groups = db.get_user_group_ids(user.id).unwrap();
        assert!(user_groups.is_empty());

        // Group permissions gone
        let perm_count: i32 = db
            .conn
            .query_row("SELECT count(*) FROM group_permissions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(perm_count, 0);
    }

    #[test]
    fn test_delete_user_removes_group_membership() {
        let db = ConfigDb::in_memory("test-pass").unwrap();

        let group = db.create_group("team", "", &[]).unwrap();
        let user = db
            .create_user("temp", "AKTEMP01", "secret", true, &[])
            .unwrap();
        db.add_user_to_group(group.id, user.id).unwrap();

        db.delete_user(user.id).unwrap();

        // Membership should be cascade-deleted
        let members = db.get_group_members(group.id).unwrap();
        assert!(members.is_empty());
    }

    #[test]
    fn test_transaction_rollback_on_duplicate_key() {
        let db = ConfigDb::in_memory("test-pass").unwrap();

        // Create first user
        db.create_user("user1", "AKFIRST1", "secret1", true, &[])
            .unwrap();

        // Try to create second user with same access_key_id — should fail
        let perms = vec![Permission {
            id: 0,
            effect: "Allow".into(),
            actions: vec!["read".into()],
            resources: vec!["*".into()],
            conditions: None,
        }];
        let result = db.create_user("user2", "AKFIRST1", "secret2", true, &perms);
        assert!(result.is_err());

        // Verify no partial state: still exactly 1 user, 0 permissions
        let users = db.load_users().unwrap();
        assert_eq!(users.len(), 1);
        assert_eq!(users[0].name, "user1");

        let perm_count: i32 = db
            .conn
            .query_row("SELECT count(*) FROM permissions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(perm_count, 0, "No orphaned permissions should exist");
    }

    // === S8: key migration ===

    const OLD_KEY: &str = "old-config-db-key-0123456789abcdef0123456789";
    const NEW_KEY: &str = "new-config-db-key-0123456789abcdef0123456789";

    fn old_key_db(dir: &std::path::Path) -> std::path::PathBuf {
        let path = dir.join("deltaglider_config.db");
        let db = ConfigDb::open_or_create(&path, OLD_KEY).unwrap();
        db.create_user("alice", "AKALICE1", "secret", true, &[])
            .unwrap();
        path
    }

    fn keys() -> ConfigDbKeys {
        ConfigDbKeys::primary_only(NEW_KEY).with_fallback(FallbackKind::PreviousKey, OLD_KEY)
    }

    #[test]
    fn previous_key_db_migrates_to_the_primary_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = old_key_db(dir.path());
        let (db, how) = ConfigDb::open_with_keys(&path, &keys()).unwrap();
        assert_eq!(how, OpenedWith::Migrated(FallbackKind::PreviousKey));
        assert_eq!(db.load_users().unwrap()[0].name, "alice");
        drop(db);
        assert!(probe_key(&path, NEW_KEY).unwrap());
        assert!(!probe_key(&path, OLD_KEY).unwrap());
        assert!(!dir.path().join("deltaglider_config.db.rekey.tmp").exists());
        // A second open is a plain primary open.
        let (_, how) = ConfigDb::open_with_keys(&path, &keys()).unwrap();
        assert_eq!(how, OpenedWith::Primary);
    }

    #[test]
    fn a_failed_rekey_leaves_the_original_intact() {
        let dir = tempfile::tempdir().unwrap();
        let path = old_key_db(dir.path());
        let before = std::fs::read(&path).unwrap();
        // The rekey step fails after the copy is re-encrypted.
        let err = ConfigDb::open_with_keys_hooked(&path, &keys(), |_| {
            Err(ConfigDbError::Other("injected".into()))
        })
        .err()
        .expect("the injected failure must surface");
        assert!(err.to_string().contains("injected"), "{err}");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "original bytes changed"
        );
        assert!(!dir.path().join("deltaglider_config.db.rekey.tmp").exists());
        // A copy that is corrupt after the rekey fails the check, not the DB.
        let err = ConfigDb::open_with_keys_hooked(&path, &keys(), |tmp| {
            std::fs::write(tmp, b"garbage").map_err(ConfigDbError::Io)
        })
        .err()
        .expect("a corrupt copy must not be swapped in");
        assert!(!matches!(err, ConfigDbError::Other(ref m) if m.contains("injected")));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        // The original still opens with the previous key, and a clean retry
        // migrates it.
        let db = ConfigDb::open_or_create(&path, OLD_KEY).unwrap();
        assert_eq!(db.load_users().unwrap().len(), 1);
        drop(db);
        let (_, how) = ConfigDb::open_with_keys(&path, &keys()).unwrap();
        assert_eq!(how, OpenedWith::Migrated(FallbackKind::PreviousKey));
    }

    #[test]
    fn a_db_under_an_unknown_key_is_wrong_passphrase_and_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deltaglider_config.db");
        drop(ConfigDb::open_or_create(&path, "some-other-key").unwrap());
        let before = std::fs::read(&path).unwrap();
        let err = ConfigDb::open_with_keys(&path, &keys()).err().unwrap();
        assert!(matches!(err, ConfigDbError::WrongPassphrase(_)), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn a_missing_db_is_created_with_the_primary_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deltaglider_config.db");
        let (_, how) = ConfigDb::open_with_keys(&path, &keys()).unwrap();
        assert_eq!(how, OpenedWith::Created);
        assert!(probe_key(&path, NEW_KEY).unwrap());
    }

    #[test]
    fn migration_rekeys_the_sync_merge_base() {
        let dir = tempfile::tempdir().unwrap();
        let path = old_key_db(dir.path());
        let base = crate::config_db_sync::sync_base_path(&path);
        assert_eq!(key_companions(&path)[0], (base.clone(), true));
        std::fs::copy(&path, &base).unwrap();
        ConfigDb::open_with_keys(&path, &keys()).unwrap();
        assert!(
            probe_key(&base, NEW_KEY).unwrap(),
            "the base must follow the DB key"
        );
        // A base that no key opens is removed, not left unreadable.
        std::fs::remove_file(&base).unwrap();
        drop(ConfigDb::open_or_create(&base, "a-foreign-key").unwrap());
        let db2 = dir.path().join("second.db");
        drop(ConfigDb::open_or_create(&db2, OLD_KEY).unwrap());
        let base2 = crate::config_db_sync::sync_base_path(&db2);
        std::fs::copy(&base, &base2).unwrap();
        ConfigDb::open_with_keys(&db2, &keys()).unwrap();
        assert!(!base2.exists());
    }

    /// Rotation end to end: DB and sync-base under the previous key, the new
    /// key primary, the previous key a fallback → both move to the new key.
    #[test]
    fn rotation_moves_the_db_and_base_to_the_new_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deltaglider_config.db");
        let old = "old-config-db-key-0123456789abcdef0123456789";
        let db = ConfigDb::open_or_create(&path, old).unwrap();
        db.create_user("alice", "AKALICE1", "s", true, &[]).unwrap();
        drop(db);
        let base = crate::config_db_sync::sync_base_path(&path);
        std::fs::copy(&path, &base).unwrap();
        let keys =
            ConfigDbKeys::primary_only(NEW_KEY).with_fallback(FallbackKind::PreviousKey, old);
        let (db, how) = ConfigDb::open_with_keys(&path, &keys).unwrap();
        assert_eq!(how, OpenedWith::Migrated(FallbackKind::PreviousKey));
        assert_eq!(db.load_users().unwrap()[0].name, "alice");
        assert!(probe_key(&path, NEW_KEY).unwrap());
        assert!(probe_key(&base, NEW_KEY).unwrap());
    }

    #[test]
    fn a_leftover_rekey_copy_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = old_key_db(dir.path());
        std::fs::write(dir.path().join("deltaglider_config.db.rekey.tmp"), b"stale").unwrap();
        let (db, _) = ConfigDb::open_with_keys(&path, &keys()).unwrap();
        assert_eq!(db.load_users().unwrap().len(), 1);
    }
}

#[cfg(test)]
mod review3_tests {
    use super::*;

    const OLD_KEY: &str = "old-config-db-key-0123456789abcdef0123456789";
    const NEW_KEY: &str = "new-config-db-key-0123456789abcdef0123456789";

    /// Kill between the DB rename and `rekey_companions`: the DB is under the
    /// new key, the merge base still under the previous key. The next boot
    /// opens the DB with the primary key and never looks at the companion
    /// again, so the base stays unreadable for good and every later 412
    /// reconcile runs base-less (remote wins, local unsynced edits lost).
    #[test]
    fn review3_a_crash_before_the_companion_rekey_heals_on_the_next_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deltaglider_config.db");
        ConfigDb::open_or_create(&path, OLD_KEY)
            .unwrap()
            .create_user("alice", "AKALICE1", "secret", true, &[])
            .unwrap();
        let base = crate::config_db_sync::sync_base_path(&path);
        drop(ConfigDb::open_or_create(&base, OLD_KEY).unwrap());
        // The crash: the DB moved, its companion did not.
        rekey_file(&path, OLD_KEY, NEW_KEY).unwrap();
        let keys = ConfigDbKeys::primary_only(NEW_KEY)
            .with_fallback(key::FallbackKind::PreviousKey, OLD_KEY);
        let (_db, _how) = ConfigDb::open_with_keys(&path, &keys).unwrap();
        assert!(
            probe_key(&base, NEW_KEY).unwrap(),
            "the merge base is still under the old key after the next boot"
        );
    }

    /// A parked `.db.bak` under a fallback key follows the DB to the primary
    /// key; one that no key opens (the incident shape) is never touched.
    #[test]
    fn a_parked_backup_follows_the_key_and_an_unknown_one_stays() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deltaglider_config.db");
        let bak = path.with_extension("db.bak");
        ConfigDb::open_or_create(&bak, OLD_KEY)
            .unwrap()
            .create_user("alice", "AKALICE1", "secret", true, &[])
            .unwrap();
        let keys = ConfigDbKeys::primary_only(NEW_KEY)
            .with_fallback(key::FallbackKind::PreviousKey, OLD_KEY);
        drop(ConfigDb::open_with_keys(&path, &keys).unwrap());
        assert!(probe_key(&bak, NEW_KEY).unwrap());

        std::fs::remove_file(&bak).unwrap();
        drop(ConfigDb::open_or_create(&bak, "a-foreign-key").unwrap());
        let before = std::fs::read(&bak).unwrap();
        drop(ConfigDb::open_with_keys(&path, &keys).unwrap());
        assert_eq!(
            std::fs::read(&bak).unwrap(),
            before,
            "never removed or changed"
        );
    }

    /// `recover-db` asks `probe_key` whether a candidate opens `.db.bak`. An
    /// empty (or junk-free zero-byte) file reads as a fresh DB under ANY key,
    /// so every candidate "matches" and the operator is told to use it.
    #[test]
    fn review3_probe_key_does_not_accept_any_key_for_an_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let bak = dir.path().join("deltaglider_config.db.bak");
        std::fs::write(&bak, b"").unwrap();
        assert!(
            !probe_key(&bak, "any-candidate-at-all").unwrap(),
            "an empty backup matches every candidate key"
        );
    }
}
