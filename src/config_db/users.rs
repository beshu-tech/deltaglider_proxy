// SPDX-License-Identifier: GPL-3.0-only

use std::collections::HashMap;

use rusqlite::params;
use rusqlite::OptionalExtension;

use crate::iam::{IamUser, Permission};

use super::{ConfigDb, ConfigDbError};

impl ConfigDb {
    /// Load all users with their permissions and group memberships.
    ///
    /// Hydrates with three queries total (users + all permissions + all
    /// memberships) and joins by `user_id` in memory, rather than running
    /// 1 + 2N queries (the per-user `load_permissions` / `get_user_group_ids`
    /// follow-ups). Keeps load cost flat as the user count grows.
    pub fn load_users(&self) -> Result<Vec<IamUser>, ConfigDbError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, name, access_key_id, secret_access_key, enabled, created_at, auth_source FROM users",
        )?;

        let mut users: Vec<IamUser> = stmt
            .query_map([], Self::user_from_row)?
            .collect::<Result<Vec<_>, _>>()?;

        let mut perms_by_user = self.load_all_permissions()?;
        let mut groups_by_user = self.load_all_user_group_ids()?;

        for user in &mut users {
            user.permissions = perms_by_user.remove(&user.id).unwrap_or_default();
            user.group_ids = groups_by_user.remove(&user.id).unwrap_or_default();
        }

        Ok(users)
    }

    /// Load every permission row keyed by `user_id` for batch hydration.
    fn load_all_permissions(&self) -> Result<HashMap<i64, Vec<Permission>>, ConfigDbError> {
        let mut stmt = self.conn.prepare(
            "SELECT user_id, id, actions, resources, effect, conditions_json FROM permissions",
        )?;
        // `permission_from_row` reads columns 0..=4; here user_id is column 0
        // and the permission fields are shifted one to the right.
        let rows = stmt
            .query_map([], |row| {
                let user_id: i64 = row.get(0)?;
                let perm = Permission {
                    id: row.get(1)?,
                    actions: serde_json::from_str(&row.get::<_, String>(2)?).unwrap_or_default(),
                    resources: serde_json::from_str(&row.get::<_, String>(3)?).unwrap_or_default(),
                    effect: row
                        .get::<_, String>(4)
                        .unwrap_or_else(|_| "Allow".to_string()),
                    conditions: row
                        .get::<_, Option<String>>(5)
                        .unwrap_or(None)
                        .and_then(|s| serde_json::from_str(&s).ok()),
                };
                Ok((user_id, perm))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let mut map: HashMap<i64, Vec<Permission>> = HashMap::new();
        for (user_id, perm) in rows {
            map.entry(user_id).or_default().push(perm);
        }
        Ok(map)
    }

    /// Load every group membership keyed by `user_id` for batch hydration.
    fn load_all_user_group_ids(&self) -> Result<HashMap<i64, Vec<i64>>, ConfigDbError> {
        let mut stmt = self
            .conn
            .prepare("SELECT user_id, group_id FROM group_members")?;
        let rows = stmt
            .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;

        let mut map: HashMap<i64, Vec<i64>> = HashMap::new();
        for (user_id, group_id) in rows {
            map.entry(user_id).or_default().push(group_id);
        }
        Ok(map)
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

    /// Clone a user atomically with fresh credentials.
    ///
    /// Copies direct permissions and, optionally, group memberships. External
    /// identities, sessions, and the original secret are intentionally not copied.
    pub fn clone_user(
        &self,
        source_user_id: i64,
        new_name: &str,
        new_access_key_id: &str,
        new_secret_access_key: &str,
        copy_group_memberships: bool,
    ) -> Result<IamUser, ConfigDbError> {
        let source = self.get_user_by_id(source_user_id)?;
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO users (name, access_key_id, secret_access_key, enabled, auth_source) \
             VALUES (?1, ?2, ?3, ?4, 'local')",
            params![
                new_name,
                new_access_key_id,
                new_secret_access_key,
                source.enabled as i32
            ],
        )?;
        let user_id = tx.last_insert_rowid();
        Self::insert_permissions(&tx, user_id, &source.permissions)?;

        if copy_group_memberships {
            for group_id in source.group_ids {
                tx.execute(
                    "INSERT OR IGNORE INTO group_members (group_id, user_id) VALUES (?1, ?2)",
                    params![group_id, user_id],
                )?;
            }
        }

        tx.commit()?;
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
