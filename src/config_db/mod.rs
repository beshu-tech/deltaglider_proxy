//! Encrypted configuration database backed by SQLCipher.
//!
//! Stores IAM users and permissions in an encrypted SQLite database.
//! The DB file is cached locally and synced to/from S3 for multi-instance
//! consistency. Encryption key is derived from the admin GUI password.

use crate::iam::{IamUser, Permission};
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};
use tracing::{debug, info};

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
}

/// Schema version — bump when adding migrations.
const SCHEMA_VERSION: i32 = 6;

pub(crate) mod auth_providers;
mod declarative;
mod groups;
mod users;

/// Compute the path to the IAM config database file.
///
/// Derives the directory from `DGP_CONFIG` (parent of the TOML config file)
/// or falls back to the current working directory.
pub fn config_db_path() -> PathBuf {
    let db_dir = std::env::var("DGP_CONFIG")
        .ok()
        .and_then(|p| std::path::Path::new(&p).parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."));
    db_dir.join("deltaglider_config.db")
}

impl ConfigDb {
    /// Open an existing DB or create a new one at `local_path`.
    /// The `passphrase` is used as the SQLCipher encryption key.
    pub fn open_or_create(local_path: &Path, passphrase: &str) -> Result<Self, ConfigDbError> {
        if passphrase.is_empty() {
            return Err(ConfigDbError::WrongPassphrase(
                "Config database passphrase must not be empty".to_string(),
            ));
        }

        let conn = Connection::open(local_path)?;

        // Set the encryption key (PRAGMA key must be the first statement)
        conn.pragma_update(None, "key", passphrase)?;

        // Test that the key is correct by reading the schema
        match conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
            r.get::<_, i32>(0)
        }) {
            Ok(_) => {}
            Err(e) => {
                return Err(ConfigDbError::WrongPassphrase(format!(
                    "Cannot decrypt config database (wrong bootstrap password?): {}",
                    e
                )));
            }
        }

        // Enable foreign keys (per-connection setting, not persisted)
        conn.pragma_update(None, "foreign_keys", "ON")?;

        // Wait up to 5s for locks instead of failing immediately.
        // Prevents "database is locked" errors during concurrent S3 sync + admin ops.
        conn.pragma_update(None, "busy_timeout", "5000")?;

        // Run migrations
        Self::migrate(&conn)?;

        info!("Config database opened: {}", local_path.display());

        Ok(Self {
            conn,
            local_path: local_path.to_path_buf(),
            s3_etag: None,
        })
    }

    /// Create an in-memory DB for testing.
    pub fn in_memory(passphrase: &str) -> Result<Self, ConfigDbError> {
        let conn = Connection::open_in_memory()?;
        conn.pragma_update(None, "key", passphrase)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        Self::migrate(&conn)?;
        Ok(Self {
            conn,
            local_path: PathBuf::from(":memory:"),
            s3_etag: None,
        })
    }

    fn migrate(conn: &Connection) -> Result<(), ConfigDbError> {
        let version: i32 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap_or(0);

        if version < 1 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS users (
                    id                INTEGER PRIMARY KEY AUTOINCREMENT,
                    name              TEXT NOT NULL,
                    access_key_id     TEXT NOT NULL UNIQUE,
                    secret_access_key TEXT NOT NULL,
                    enabled           INTEGER NOT NULL DEFAULT 1,
                    created_at        TEXT NOT NULL DEFAULT (datetime('now'))
                );

                CREATE TABLE IF NOT EXISTS permissions (
                    id        INTEGER PRIMARY KEY AUTOINCREMENT,
                    user_id   INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                    actions   TEXT NOT NULL,
                    resources TEXT NOT NULL
                );

                CREATE INDEX IF NOT EXISTS idx_users_access_key ON users(access_key_id);
                CREATE INDEX IF NOT EXISTS idx_permissions_user ON permissions(user_id);",
            )?;
        }

        if version < 2 {
            conn.execute_batch(
                "ALTER TABLE permissions ADD COLUMN effect TEXT NOT NULL DEFAULT 'Allow';",
            )?;
            info!(
                "Migrated config DB schema from v{} to v2 (added effect column)",
                version
            );
        }

        if version < 3 {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS groups (
                    id          INTEGER PRIMARY KEY AUTOINCREMENT,
                    name        TEXT NOT NULL UNIQUE,
                    description TEXT DEFAULT '',
                    created_at  TEXT NOT NULL DEFAULT (datetime('now'))
                );

                CREATE TABLE IF NOT EXISTS group_members (
                    group_id INTEGER NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
                    user_id  INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                    PRIMARY KEY (group_id, user_id)
                );

                CREATE TABLE IF NOT EXISTS group_permissions (
                    id        INTEGER PRIMARY KEY AUTOINCREMENT,
                    group_id  INTEGER NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
                    actions   TEXT NOT NULL,
                    resources TEXT NOT NULL,
                    effect    TEXT NOT NULL DEFAULT 'Allow'
                );",
            )?;
            info!(
                "Migrated config DB schema from v{} to v3 (added groups tables)",
                version
            );
        }

        if version < 4 {
            conn.execute_batch(
                "ALTER TABLE permissions ADD COLUMN conditions_json TEXT;
                 ALTER TABLE group_permissions ADD COLUMN conditions_json TEXT;",
            )?;
            info!(
                "Migrated config DB schema from v{} to v4 (added conditions column)",
                version
            );
        }

        if version < 5 {
            conn.execute_batch(
                "ALTER TABLE users ADD COLUMN auth_source TEXT NOT NULL DEFAULT 'local';

                CREATE TABLE IF NOT EXISTS auth_providers (
                    id            INTEGER PRIMARY KEY AUTOINCREMENT,
                    name          TEXT NOT NULL UNIQUE,
                    provider_type TEXT NOT NULL,
                    enabled       INTEGER NOT NULL DEFAULT 1,
                    priority      INTEGER NOT NULL DEFAULT 0,
                    display_name  TEXT,
                    client_id     TEXT,
                    client_secret TEXT,
                    issuer_url    TEXT,
                    scopes        TEXT DEFAULT 'openid email profile',
                    extra_config  TEXT,
                    created_at    TEXT NOT NULL DEFAULT (datetime('now')),
                    updated_at    TEXT NOT NULL DEFAULT (datetime('now'))
                );

                CREATE TABLE IF NOT EXISTS group_mapping_rules (
                    id           INTEGER PRIMARY KEY AUTOINCREMENT,
                    provider_id  INTEGER REFERENCES auth_providers(id) ON DELETE CASCADE,
                    priority     INTEGER NOT NULL DEFAULT 0,
                    match_type   TEXT NOT NULL,
                    match_field  TEXT NOT NULL DEFAULT 'email',
                    match_value  TEXT NOT NULL,
                    group_id     INTEGER NOT NULL REFERENCES groups(id) ON DELETE CASCADE,
                    created_at   TEXT NOT NULL DEFAULT (datetime('now'))
                );
                CREATE INDEX IF NOT EXISTS idx_group_mapping_provider ON group_mapping_rules(provider_id);

                CREATE TABLE IF NOT EXISTS external_identities (
                    id             INTEGER PRIMARY KEY AUTOINCREMENT,
                    user_id        INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                    provider_id    INTEGER NOT NULL REFERENCES auth_providers(id) ON DELETE CASCADE,
                    external_sub   TEXT NOT NULL,
                    email          TEXT,
                    display_name   TEXT,
                    last_login     TEXT,
                    raw_claims     TEXT,
                    created_at     TEXT NOT NULL DEFAULT (datetime('now')),
                    UNIQUE(provider_id, external_sub)
                );
                CREATE INDEX IF NOT EXISTS idx_ext_identity_user ON external_identities(user_id);
                CREATE INDEX IF NOT EXISTS idx_ext_identity_lookup ON external_identities(provider_id, external_sub);",
            )?;
            info!(
                "Migrated config DB schema from v{} to v5 (added external auth tables)",
                version
            );
        }

        if version < 6 {
            // v6: Replication runtime state. Rules themselves live in
            // YAML; only progress/history/failures land here.
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS replication_state (
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
                    id              INTEGER PRIMARY KEY AUTOINCREMENT,
                    rule_name       TEXT NOT NULL,
                    started_at      INTEGER NOT NULL,
                    finished_at     INTEGER,
                    objects_scanned INTEGER NOT NULL DEFAULT 0,
                    objects_copied  INTEGER NOT NULL DEFAULT 0,
                    objects_skipped INTEGER NOT NULL DEFAULT 0,
                    objects_deleted INTEGER NOT NULL DEFAULT 0,
                    bytes_copied    INTEGER NOT NULL DEFAULT 0,
                    errors          INTEGER NOT NULL DEFAULT 0,
                    status          TEXT NOT NULL,
                    FOREIGN KEY (rule_name) REFERENCES replication_state(rule_name) ON DELETE CASCADE
                );
                CREATE INDEX IF NOT EXISTS idx_run_history_rule
                    ON replication_run_history(rule_name, started_at DESC);

                CREATE TABLE IF NOT EXISTS replication_failures (
                    id            INTEGER PRIMARY KEY AUTOINCREMENT,
                    rule_name     TEXT NOT NULL,
                    occurred_at   INTEGER NOT NULL,
                    source_key    TEXT NOT NULL,
                    dest_key      TEXT NOT NULL,
                    error_message TEXT NOT NULL,
                    FOREIGN KEY (rule_name) REFERENCES replication_state(rule_name) ON DELETE CASCADE
                );
                CREATE INDEX IF NOT EXISTS idx_failures_rule
                    ON replication_failures(rule_name, occurred_at DESC);",
            )?;
            info!(
                "Migrated config DB schema from v{} to v6 (added replication state tables)",
                version
            );
        }

        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        debug!("Config DB schema at version {}", SCHEMA_VERSION);
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
        let conn = Connection::open(&self.local_path)?;
        conn.pragma_update(None, "key", passphrase)?;
        // Verify key works
        conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
            r.get::<_, i32>(0)
        })
        .map_err(|e| {
            ConfigDbError::WrongPassphrase(format!("Cannot decrypt after re-download: {}", e))
        })?;
        // Per-connection settings (not persisted in DB)
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "busy_timeout", "5000")?;
        self.conn = conn;
        info!("Config database re-opened after S3 sync");
        Ok(())
    }

    /// Re-encrypt the database with a new passphrase (after bootstrap password change).
    pub fn rekey(&self, new_passphrase: &str) -> Result<(), ConfigDbError> {
        self.conn.pragma_update(None, "rekey", new_passphrase)?;
        info!("Config database re-encrypted with new passphrase");
        Ok(())
    }
}

/// Errors from the config database.
#[derive(Debug)]
pub enum ConfigDbError {
    Sqlite(rusqlite::Error),
    WrongPassphrase(String),
    NotFound(String),
    Io(std::io::Error),
    /// Structural / invariant violations detected by reconcile helpers.
    /// Used for "validation should have caught this" defence-in-depth
    /// cases inside the transaction.
    Other(String),
}

impl std::fmt::Display for ConfigDbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sqlite(e) => write!(f, "SQLite error: {}", e),
            Self::WrongPassphrase(msg) => write!(f, "{}", msg),
            Self::NotFound(what) => write!(f, "Not found: {}", what),
            Self::Io(e) => write!(f, "I/O error: {}", e),
            Self::Other(msg) => write!(f, "{}", msg),
        }
    }
}

impl std::error::Error for ConfigDbError {}

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

        let rotated = db.rotate_keys(user.id, "AKNEW001", "new-secret").unwrap();

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
}
