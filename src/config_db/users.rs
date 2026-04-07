use rusqlite::params;
use rusqlite::OptionalExtension;

use crate::iam::{IamUser, Permission};

use super::{ConfigDb, ConfigDbError};

impl ConfigDb {
    /// Load all users with their permissions.
    pub fn load_users(&self) -> Result<Vec<IamUser>, ConfigDbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, access_key_id, secret_access_key, enabled, created_at, auth_source FROM users",
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

    pub(crate) fn get_user_by_id(&self, user_id: i64) -> Result<IamUser, ConfigDbError> {
        let mut user = self.conn.query_row(
            "SELECT id, name, access_key_id, secret_access_key, enabled, created_at, auth_source FROM users WHERE id = ?1",
            params![user_id],
            Self::user_from_row,
        )?;
        user.permissions = self.load_permissions(user_id)?;
        user.group_ids = self.get_user_group_ids(user_id)?;
        Ok(user)
    }
}
