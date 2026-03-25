//! Encrypted configuration database backed by SQLCipher.
//!
//! Stores IAM users and permissions in an encrypted SQLite database.
//! The DB file is cached locally and synced to/from S3 for multi-instance
//! consistency. Encryption key is derived from the admin GUI password.

use crate::iam::{IamUser, Permission};
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
const SCHEMA_VERSION: i32 = 1;

impl ConfigDb {
    /// Open an existing DB or create a new one at `local_path`.
    /// The `passphrase` is used as the SQLCipher encryption key.
    pub fn open_or_create(local_path: &Path, passphrase: &str) -> Result<Self, ConfigDbError> {
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
                    "Cannot decrypt config database (wrong admin password?): {}",
                    e
                )));
            }
        }

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
    #[cfg(test)]
    pub fn in_memory(passphrase: &str) -> Result<Self, ConfigDbError> {
        let conn = Connection::open_in_memory()?;
        conn.pragma_update(None, "key", passphrase)?;
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
                CREATE INDEX IF NOT EXISTS idx_permissions_user ON permissions(user_id);

                PRAGMA foreign_keys = ON;",
            )?;
        }

        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        debug!("Config DB schema at version {}", SCHEMA_VERSION);
        Ok(())
    }

    // === User CRUD ===

    /// Load all users with their permissions.
    pub fn load_users(&self) -> Result<Vec<IamUser>, ConfigDbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, access_key_id, secret_access_key, enabled, created_at FROM users",
        )?;

        let users: Vec<IamUser> = stmt
            .query_map([], |row| {
                Ok(IamUser {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    access_key_id: row.get(2)?,
                    secret_access_key: row.get(3)?,
                    enabled: row.get::<_, i32>(4)? != 0,
                    created_at: row.get(5)?,
                    permissions: Vec::new(), // filled below
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        // Load permissions for each user
        let mut perm_stmt = self
            .conn
            .prepare("SELECT id, actions, resources FROM permissions WHERE user_id = ?")?;

        let mut result = Vec::with_capacity(users.len());
        for mut user in users {
            let perms: Vec<Permission> = perm_stmt
                .query_map(params![user.id], |row| {
                    let actions_json: String = row.get(1)?;
                    let resources_json: String = row.get(2)?;
                    Ok(Permission {
                        id: row.get(0)?,
                        actions: serde_json::from_str(&actions_json).unwrap_or_default(),
                        resources: serde_json::from_str(&resources_json).unwrap_or_default(),
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            user.permissions = perms;
            result.push(user);
        }

        Ok(result)
    }

    /// Create a new user. Returns the user with generated ID.
    pub fn create_user(
        &self,
        name: &str,
        access_key_id: &str,
        secret_access_key: &str,
        enabled: bool,
        permissions: &[Permission],
    ) -> Result<IamUser, ConfigDbError> {
        self.conn.execute(
            "INSERT INTO users (name, access_key_id, secret_access_key, enabled) VALUES (?1, ?2, ?3, ?4)",
            params![name, access_key_id, secret_access_key, enabled as i32],
        )?;
        let user_id = self.conn.last_insert_rowid();

        for perm in permissions {
            let actions_json = serde_json::to_string(&perm.actions).unwrap_or_default();
            let resources_json = serde_json::to_string(&perm.resources).unwrap_or_default();
            self.conn.execute(
                "INSERT INTO permissions (user_id, actions, resources) VALUES (?1, ?2, ?3)",
                params![user_id, actions_json, resources_json],
            )?;
        }

        // Reload to get all fields including generated ones
        self.get_user_by_id(user_id)
    }

    /// Update an existing user by ID.
    pub fn update_user(
        &self,
        user_id: i64,
        name: Option<&str>,
        enabled: Option<bool>,
        permissions: Option<&[Permission]>,
    ) -> Result<IamUser, ConfigDbError> {
        if let Some(n) = name {
            self.conn.execute(
                "UPDATE users SET name = ?1 WHERE id = ?2",
                params![n, user_id],
            )?;
        }
        if let Some(e) = enabled {
            self.conn.execute(
                "UPDATE users SET enabled = ?1 WHERE id = ?2",
                params![e as i32, user_id],
            )?;
        }
        if let Some(perms) = permissions {
            // Replace all permissions
            self.conn.execute(
                "DELETE FROM permissions WHERE user_id = ?1",
                params![user_id],
            )?;
            for perm in perms {
                let actions_json = serde_json::to_string(&perm.actions).unwrap_or_default();
                let resources_json = serde_json::to_string(&perm.resources).unwrap_or_default();
                self.conn.execute(
                    "INSERT INTO permissions (user_id, actions, resources) VALUES (?1, ?2, ?3)",
                    params![user_id, actions_json, resources_json],
                )?;
            }
        }

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
        let user = self.conn.query_row(
            "SELECT id, name, access_key_id, secret_access_key, enabled, created_at FROM users WHERE id = ?1",
            params![user_id],
            |row| {
                Ok(IamUser {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    access_key_id: row.get(2)?,
                    secret_access_key: row.get(3)?,
                    enabled: row.get::<_, i32>(4)? != 0,
                    created_at: row.get(5)?,
                    permissions: Vec::new(),
                })
            },
        )?;

        let mut perm_stmt = self
            .conn
            .prepare("SELECT id, actions, resources FROM permissions WHERE user_id = ?1")?;
        let perms: Vec<Permission> = perm_stmt
            .query_map(params![user_id], |row| {
                let actions_json: String = row.get(1)?;
                let resources_json: String = row.get(2)?;
                Ok(Permission {
                    id: row.get(0)?,
                    actions: serde_json::from_str(&actions_json).unwrap_or_default(),
                    resources: serde_json::from_str(&resources_json).unwrap_or_default(),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(IamUser {
            permissions: perms,
            ..user
        })
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
        self.conn = conn;
        info!("Config database re-opened after S3 sync");
        Ok(())
    }

    /// Re-encrypt the database with a new passphrase (after admin password change).
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
            actions: vec!["read".into(), "write".into()],
            resources: vec!["releases/*".into()],
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
            actions: vec!["read".into()],
            resources: vec!["*".into()],
        }];
        let user = db
            .create_user("user1", "AKUSER01", "secret", true, &initial_perms)
            .unwrap();

        // Replace with new permissions
        let new_perms = vec![
            Permission {
                id: 0,
                actions: vec!["read".into(), "write".into()],
                resources: vec!["releases/*".into()],
            },
            Permission {
                id: 0,
                actions: vec!["list".into()],
                resources: vec!["*".into()],
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
            actions: vec!["read".into()],
            resources: vec!["*".into()],
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
}
