use rusqlite::{params, Connection};

use crate::iam::Group;
use crate::iam::Permission;

use super::{ConfigDb, ConfigDbError};

impl ConfigDb {
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
}
