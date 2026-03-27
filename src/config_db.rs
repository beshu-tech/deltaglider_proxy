//! Encrypted configuration database backed by SQLCipher.
//!
//! Stores IAM users and permissions in an encrypted SQLite database.
//! The DB file is cached locally and synced to/from S3 for multi-instance
//! consistency. Encryption key is derived from the admin GUI password.

use crate::iam::{Group, IamUser, Permission};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};
use tracing::{debug, info};

/// Encrypted configuration database (SQLCipher).
pub struct ConfigDb {
    conn: Connection,
    local_path: PathBuf,
    /// ETag from last S3 download (for change detection during polling)
    s3_etag: Option<String>,
}

/// Schema version — bump when adding migrations.
const SCHEMA_VERSION: i32 = 4;

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
    fn insert_permission_rows(
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

    // === User CRUD ===

    /// Load all users with their permissions.
    pub fn load_users(&self) -> Result<Vec<IamUser>, ConfigDbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, access_key_id, secret_access_key, enabled, created_at FROM users",
        )?;

        let users: Vec<IamUser> = stmt
            .query_map([], Self::user_from_row)?
            .collect::<Result<Vec<_>, _>>()?;

        let mut result = Vec::with_capacity(users.len());
        for mut user in users {
            user.permissions = self.load_permissions(user.id)?;
            user.group_ids = self.get_user_group_ids(user.id)?;
            result.push(user);
        }

        Ok(result)
    }

    /// Create a new user. Returns the user with generated ID.
    /// Wrapped in a transaction — if permission insertion fails, the user row is rolled back.
    pub fn create_user(
        &self,
        name: &str,
        access_key_id: &str,
        secret_access_key: &str,
        enabled: bool,
        permissions: &[Permission],
    ) -> Result<IamUser, ConfigDbError> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO users (name, access_key_id, secret_access_key, enabled) VALUES (?1, ?2, ?3, ?4)",
            params![name, access_key_id, secret_access_key, enabled as i32],
        )?;
        let user_id = tx.last_insert_rowid();
        Self::insert_permissions(&tx, user_id, permissions)?;
        tx.commit()?;
        // Read the committed user after the transaction is committed
        self.get_user_by_id(user_id)
    }

    /// Update an existing user by ID.
    /// Wrapped in a transaction — partial updates are rolled back on failure.
    pub fn update_user(
        &self,
        user_id: i64,
        name: Option<&str>,
        enabled: Option<bool>,
        permissions: Option<&[Permission]>,
    ) -> Result<IamUser, ConfigDbError> {
        let tx = self.conn.unchecked_transaction()?;
        if let Some(n) = name {
            tx.execute(
                "UPDATE users SET name = ?1 WHERE id = ?2",
                params![n, user_id],
            )?;
        }
        if let Some(e) = enabled {
            tx.execute(
                "UPDATE users SET enabled = ?1 WHERE id = ?2",
                params![e as i32, user_id],
            )?;
        }
        if let Some(perms) = permissions {
            tx.execute(
                "DELETE FROM permissions WHERE user_id = ?1",
                params![user_id],
            )?;
            Self::insert_permissions(&tx, user_id, perms)?;
        }
        tx.commit()?;
        // Read the committed user after the transaction is committed
        self.get_user_by_id(user_id)
    }

    /// Delete a user by ID. Permissions are cascade-deleted.
    pub fn delete_user(&self, user_id: i64) -> Result<(), ConfigDbError> {
        let rows = self
            .conn
            .execute("DELETE FROM users WHERE id = ?1", params![user_id])?;
        if rows == 0 {
            return Err(ConfigDbError::NotFound(format!("User ID {}", user_id)));
        }
        Ok(())
    }

    /// Rotate access keys for a user. Returns updated user with new keys.
    pub fn rotate_keys(
        &self,
        user_id: i64,
        new_access_key_id: &str,
        new_secret_access_key: &str,
    ) -> Result<IamUser, ConfigDbError> {
        let rows = self.conn.execute(
            "UPDATE users SET access_key_id = ?1, secret_access_key = ?2 WHERE id = ?3",
            params![new_access_key_id, new_secret_access_key, user_id],
        )?;
        if rows == 0 {
            return Err(ConfigDbError::NotFound(format!("User ID {}", user_id)));
        }
        self.get_user_by_id(user_id)
    }

    /// Find a user by access_key_id.
    pub fn get_user_by_access_key(
        &self,
        access_key_id: &str,
    ) -> Result<Option<IamUser>, ConfigDbError> {
        let user_id: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM users WHERE access_key_id = ?1",
                params![access_key_id],
                |r| r.get(0),
            )
            .optional()?;

        match user_id {
            Some(id) => Ok(Some(self.get_user_by_id(id)?)),
            None => Ok(None),
        }
    }

    fn get_user_by_id(&self, user_id: i64) -> Result<IamUser, ConfigDbError> {
        let mut user = self.conn.query_row(
            "SELECT id, name, access_key_id, secret_access_key, enabled, created_at FROM users WHERE id = ?1",
            params![user_id],
            Self::user_from_row,
        )?;
        user.permissions = self.load_permissions(user_id)?;
        user.group_ids = self.get_user_group_ids(user_id)?;
        Ok(user)
    }

    // === Group CRUD ===

    /// Load permissions for a group by ID.
    fn load_group_permissions(&self, group_id: i64) -> Result<Vec<Permission>, ConfigDbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, actions, resources, effect, conditions_json FROM group_permissions WHERE group_id = ?1",
        )?;
        let perms = stmt
            .query_map(params![group_id], Self::permission_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(perms)
    }

    /// Insert permission rows for a group.
    fn insert_group_permissions(
        conn: &Connection,
        group_id: i64,
        permissions: &[Permission],
    ) -> Result<(), ConfigDbError> {
        Self::insert_permission_rows(conn, "group_permissions", "group_id", group_id, permissions)
    }

    /// Get member user IDs for a group.
    pub fn get_group_members(&self, group_id: i64) -> Result<Vec<i64>, ConfigDbError> {
        let mut stmt = self
            .conn
            .prepare("SELECT user_id FROM group_members WHERE group_id = ?1")?;
        let ids = stmt
            .query_map(params![group_id], |row| row.get(0))?
            .collect::<Result<Vec<i64>, _>>()?;
        Ok(ids)
    }

    /// Get group IDs that a user belongs to.
    pub fn get_user_group_ids(&self, user_id: i64) -> Result<Vec<i64>, ConfigDbError> {
        let mut stmt = self
            .conn
            .prepare("SELECT group_id FROM group_members WHERE user_id = ?1")?;
        let ids = stmt
            .query_map(params![user_id], |row| row.get(0))?
            .collect::<Result<Vec<i64>, _>>()?;
        Ok(ids)
    }

    /// Load all groups with their permissions and member IDs.
    pub fn load_groups(&self) -> Result<Vec<Group>, ConfigDbError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name, description, created_at FROM groups")?;
        let groups: Vec<(i64, String, String, String)> = stmt
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get::<_, String>(2).unwrap_or_default(),
                    row.get(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut result = Vec::with_capacity(groups.len());
        for (id, name, description, created_at) in groups {
            let permissions = self.load_group_permissions(id)?;
            let member_ids = self.get_group_members(id)?;
            result.push(Group {
                id,
                name,
                description,
                permissions,
                member_ids,
                created_at,
            });
        }
        Ok(result)
    }

    /// Get a single group by ID with permissions and members.
    pub fn get_group_by_id(&self, group_id: i64) -> Result<Group, ConfigDbError> {
        let (id, name, description, created_at) = self.conn.query_row(
            "SELECT id, name, description, created_at FROM groups WHERE id = ?1",
            params![group_id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2).unwrap_or_default(),
                    row.get::<_, String>(3)?,
                ))
            },
        )?;
        let permissions = self.load_group_permissions(id)?;
        let member_ids = self.get_group_members(id)?;
        Ok(Group {
            id,
            name,
            description,
            permissions,
            member_ids,
            created_at,
        })
    }

    /// Create a new group. Returns the group with generated ID.
    pub fn create_group(
        &self,
        name: &str,
        description: &str,
        permissions: &[Permission],
    ) -> Result<Group, ConfigDbError> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO groups (name, description) VALUES (?1, ?2)",
            params![name, description],
        )?;
        let group_id = tx.last_insert_rowid();
        Self::insert_group_permissions(&tx, group_id, permissions)?;
        tx.commit()?;
        self.get_group_by_id(group_id)
    }

    /// Update an existing group by ID.
    pub fn update_group(
        &self,
        group_id: i64,
        name: Option<&str>,
        description: Option<&str>,
        permissions: Option<&[Permission]>,
    ) -> Result<Group, ConfigDbError> {
        let tx = self.conn.unchecked_transaction()?;
        if let Some(n) = name {
            tx.execute(
                "UPDATE groups SET name = ?1 WHERE id = ?2",
                params![n, group_id],
            )?;
        }
        if let Some(d) = description {
            tx.execute(
                "UPDATE groups SET description = ?1 WHERE id = ?2",
                params![d, group_id],
            )?;
        }
        if let Some(perms) = permissions {
            tx.execute(
                "DELETE FROM group_permissions WHERE group_id = ?1",
                params![group_id],
            )?;
            Self::insert_group_permissions(&tx, group_id, perms)?;
        }
        tx.commit()?;
        self.get_group_by_id(group_id)
    }

    /// Delete a group by ID. Permissions and memberships are cascade-deleted.
    pub fn delete_group(&self, group_id: i64) -> Result<(), ConfigDbError> {
        let rows = self
            .conn
            .execute("DELETE FROM groups WHERE id = ?1", params![group_id])?;
        if rows == 0 {
            return Err(ConfigDbError::NotFound(format!("Group ID {}", group_id)));
        }
        Ok(())
    }

    /// Add a user to a group.
    pub fn add_user_to_group(&self, group_id: i64, user_id: i64) -> Result<(), ConfigDbError> {
        self.conn.execute(
            "INSERT OR IGNORE INTO group_members (group_id, user_id) VALUES (?1, ?2)",
            params![group_id, user_id],
        )?;
        Ok(())
    }

    /// Remove a user from a group.
    pub fn remove_user_from_group(&self, group_id: i64, user_id: i64) -> Result<(), ConfigDbError> {
        self.conn.execute(
            "DELETE FROM group_members WHERE group_id = ?1 AND user_id = ?2",
            params![group_id, user_id],
        )?;
        Ok(())
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
}

impl std::fmt::Display for ConfigDbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sqlite(e) => write!(f, "SQLite error: {}", e),
            Self::WrongPassphrase(msg) => write!(f, "{}", msg),
            Self::NotFound(what) => write!(f, "Not found: {}", what),
            Self::Io(e) => write!(f, "I/O error: {}", e),
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
