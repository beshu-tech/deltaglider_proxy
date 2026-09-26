// SPDX-License-Identifier: BUSL-1.1

//! Restore of an IAM backup (`iam.json`) into the config DB, in ONE
//! transaction: a restore that fails half-way leaves the DB as it was.
//!
//! Two modes:
//!
//! - [`IamRestoreMode::Replace`] (the default) is point-in-time: every user,
//!   group, OAuth provider, mapping rule and external identity that the DB
//!   holds is deleted, then the backup's rows are written. The backup's row
//!   ids are kept where the backup has them, so external identities and live
//!   OAuth sessions keep naming the same user.
//! - [`IamRestoreMode::Merge`] adds what the DB does not have and keeps the
//!   rest: users match by access key, groups and providers by name.

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

use super::auth_providers::{AuthProviderConfig, ExternalIdentity, GroupMappingRule};
use super::{first_free_user_name, ConfigDb, ConfigDbError};
use crate::iam::{normalize_permissions, validate_permissions, Permission};

/// Full IAM backup: users (with credentials) + groups + memberships + external auth.
#[derive(Serialize, Deserialize, Clone)]
pub struct IamBackup {
    pub version: u32,
    pub users: Vec<BackupUser>,
    pub groups: Vec<BackupGroup>,
    /// External auth providers (v2+, optional for backward compat).
    #[serde(default)]
    pub auth_providers: Vec<AuthProviderConfig>,
    /// Group mapping rules (v2+, optional for backward compat).
    #[serde(default)]
    pub mapping_rules: Vec<GroupMappingRule>,
    /// External identities (v2+, optional for backward compat).
    #[serde(default)]
    pub external_identities: Vec<ExternalIdentity>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct BackupUser {
    /// Source user id. Present in exports so `external_identities.user_id`
    /// and `groups.member_ids` can be remapped by the importer. Optional
    /// for compatibility with older backups that never exposed it.
    #[serde(default)]
    pub id: Option<i64>,
    pub name: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub enabled: bool,
    pub permissions: Vec<Permission>,
    pub group_ids: Vec<i64>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct BackupGroup {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub permissions: Vec<Permission>,
    pub member_ids: Vec<i64>,
}

/// How a restore treats the IAM rows that the DB already holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum IamRestoreMode {
    /// Point-in-time: the DB ends up with the backup's rows only.
    #[default]
    Replace,
    /// Add what is missing; keep every existing row as it is.
    Merge,
}

#[derive(Serialize, Default, Debug)]
pub struct ImportResult {
    pub users_created: u32,
    pub users_skipped: u32,
    /// Users imported under a new name because another user had theirs
    /// (user names are unique): `"old -> new"`.
    pub users_renamed: Vec<String>,
    /// Users (and groups) removed because the backup does not hold them
    /// (replace mode only).
    pub users_deleted: u32,
    pub groups_created: u32,
    pub groups_skipped: u32,
    pub groups_deleted: u32,
    pub memberships_created: u32,
    /// External-identity rows successfully remapped + inserted.
    pub external_identities_created: u32,
    /// Skipped because the referenced user/provider didn't make it,
    /// or a matching (provider, external_sub) already exists.
    pub external_identities_skipped: u32,
    /// Local user ids that no longer name the same user (deleted, or now
    /// another user). The caller ends the OAuth sessions bound to them.
    #[serde(skip)]
    pub stale_user_ids: Vec<i64>,
}

/// Resolve a backup user's original database id, or `None` when the
/// backup does not PROVE it.
///
/// Old backups (before the Wave-11 fix) never carried `BackupUser.id`.
/// The id is only used to re-attach `external_identities`, so a wrong
/// guess binds one user's OAuth identity to another user (S16: Bob logs in
/// as Alice). A missing id only drops the binding, and the next OAuth login
/// re-provisions it. So only provable answers count:
///
///   1. `bu.id` — authoritative when present (new exports).
///   2. Every user is a member of some group: then the distinct
///      `groups[].member_ids` set IS the set of user ids, and the export
///      writes users in id order, so the `idx`-th smallest is this user's.
pub(crate) fn resolve_backup_user_id(
    bu: &BackupUser,
    idx: usize,
    backup: &IamBackup,
) -> Option<i64> {
    if let Some(id) = bu.id {
        return Some(id);
    }
    let member_ids: Vec<i64> = backup
        .groups
        .iter()
        .flat_map(|g| g.member_ids.iter().copied())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let every_user_is_a_member = member_ids.len() == backup.users.len()
        && backup.users.iter().all(|u| !u.group_ids.is_empty());
    if every_user_is_a_member {
        return member_ids.get(idx).copied();
    }
    tracing::warn!(
        "Backup user '{}' (access_key_id {}) has no id, and the backup does not prove it; \
         its external_identities are not restored (the next OAuth login re-creates them).",
        bu.name,
        bu.access_key_id,
    );
    None
}

/// `(id, access_key_id)` of every user: the identity a live session names.
fn user_keys(conn: &Connection) -> Result<HashSet<(i64, String)>, ConfigDbError> {
    Ok(conn
        .prepare("SELECT id, access_key_id FROM users")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?)
}

fn name_ids(conn: &Connection, table: &str) -> Result<HashMap<String, i64>, ConfigDbError> {
    debug_assert!(super::is_safe_sql_ident(table));
    Ok(conn
        .prepare(&format!("SELECT name, id FROM {table}"))?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?)
}

/// Keep the backup's id only when no row holds it (replace mode starts from
/// empty tables, so every backup id is free there).
fn keep_id(keep_ids: bool, id: Option<i64>, taken: &HashSet<i64>) -> Option<i64> {
    id.filter(|id| keep_ids && !taken.contains(id))
}

impl ConfigDb {
    /// Restore `backup` in one transaction (see the module doc).
    /// `bootstrap_key`: the bootstrap access key; a backup user with it is
    /// skipped, because the key would collide with the bootstrap login.
    pub fn restore_iam(
        &self,
        backup: &IamBackup,
        mode: IamRestoreMode,
        bootstrap_key: Option<&str>,
    ) -> Result<ImportResult, ConfigDbError> {
        let tx = self.conn.unchecked_transaction()?;
        let result = restore_on(&tx, backup, mode, bootstrap_key)?;
        tx.commit()?;
        Ok(result)
    }
}

fn restore_on(
    conn: &Connection,
    backup: &IamBackup,
    mode: IamRestoreMode,
    bootstrap_key: Option<&str>,
) -> Result<ImportResult, ConfigDbError> {
    let mut result = ImportResult::default();
    let before = user_keys(conn)?;
    let replace = mode == IamRestoreMode::Replace;
    let groups_before: HashSet<String> = name_ids(conn, "groups")?.into_keys().collect();
    if replace {
        // The cascades cover permissions, memberships, external identities
        // and mapping rules; the explicit deletes do not depend on them.
        conn.execute_batch(
            "DELETE FROM group_mapping_rules;
             DELETE FROM external_identities;
             DELETE FROM group_members;
             DELETE FROM group_permissions;
             DELETE FROM permissions;
             DELETE FROM users;
             DELETE FROM groups;
             DELETE FROM auth_providers;",
        )?;
    }

    // ── Groups (users reference them) ──
    let existing_groups = name_ids(conn, "groups")?;
    let mut group_ids_taken: HashSet<i64> = existing_groups.values().copied().collect();
    let mut group_id_map: HashMap<i64, i64> = HashMap::new();
    for bg in &backup.groups {
        if let Some(&gid) = existing_groups.get(&bg.name) {
            group_id_map.insert(bg.id, gid);
            result.groups_skipped += 1;
            continue;
        }
        let mut perms = bg.permissions.clone();
        normalize_permissions(&mut perms);
        if let Err(msg) = validate_permissions(&perms) {
            tracing::warn!("Skipping group '{}': invalid permissions: {}", bg.name, msg);
            result.groups_skipped += 1;
            continue;
        }
        let id = keep_id(replace, Some(bg.id), &group_ids_taken);
        conn.execute(
            "INSERT INTO groups (id, name, description) VALUES (?1, ?2, ?3)",
            params![id, bg.name, bg.description],
        )?;
        let gid = conn.last_insert_rowid();
        ConfigDb::insert_group_permissions(conn, gid, &perms)?;
        group_ids_taken.insert(gid);
        group_id_map.insert(bg.id, gid);
        result.groups_created += 1;
    }

    // ── Users ──
    let existing_users: Vec<(i64, String)> = conn
        .prepare("SELECT id, access_key_id FROM users")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    let existing_keys: HashMap<String, i64> = existing_users
        .iter()
        .map(|(id, k)| (k.clone(), *id))
        .collect();
    let mut user_ids_taken: HashSet<i64> = existing_users.iter().map(|(id, _)| *id).collect();
    let mut taken_names: HashSet<String> = name_ids(conn, "users")?.into_keys().collect();
    let mut user_id_map: HashMap<i64, i64> = HashMap::new();
    for (idx, bu) in backup.users.iter().enumerate() {
        if let Some(&uid) = existing_keys.get(&bu.access_key_id) {
            if let Some(old_id) = resolve_backup_user_id(bu, idx, backup) {
                user_id_map.insert(old_id, uid);
            }
        }
    }
    // Users that carry an id go first, so a user without one never takes an
    // id that a later user of the backup holds.
    let mut order: Vec<usize> = (0..backup.users.len()).collect();
    order.sort_by_key(|&i| backup.users[i].id.is_none());
    for idx in order {
        let bu = &backup.users[idx];
        if crate::iam::types::is_reserved_principal_name(&bu.name) {
            tracing::warn!("Skipping user '{}': reserved name", bu.name);
            result.users_skipped += 1;
            continue;
        }
        if bootstrap_key == Some(bu.access_key_id.as_str()) {
            tracing::warn!(
                "Skipping user '{}': access key conflicts with bootstrap credentials",
                bu.name
            );
            result.users_skipped += 1;
            continue;
        }
        if existing_keys.contains_key(&bu.access_key_id) {
            result.users_skipped += 1;
            continue;
        }
        let mut perms = bu.permissions.clone();
        normalize_permissions(&mut perms);
        if let Err(msg) = validate_permissions(&perms) {
            tracing::warn!("Skipping user '{}': invalid permissions: {}", bu.name, msg);
            result.users_skipped += 1;
            continue;
        }
        // User names are unique (`${iam:username}` isolation). A name in use
        // gets the same `-N` suffix as the v25 upgrade, not a silent skip.
        let name = first_free_user_name(&bu.name, |c| taken_names.contains(c));
        let id = keep_id(replace, bu.id, &user_ids_taken);
        conn.execute(
            "INSERT INTO users (id, name, access_key_id, secret_access_key, enabled) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                id,
                name,
                bu.access_key_id,
                bu.secret_access_key,
                bu.enabled as i32
            ],
        )?;
        let uid = conn.last_insert_rowid();
        ConfigDb::insert_permissions(conn, uid, &perms)?;
        user_ids_taken.insert(uid);
        if let Some(old_id) = resolve_backup_user_id(bu, idx, backup) {
            user_id_map.insert(old_id, uid);
        }
        for old_gid in &bu.group_ids {
            match group_id_map.get(old_gid) {
                Some(&gid) => {
                    conn.execute(
                        "INSERT OR IGNORE INTO group_members (group_id, user_id) VALUES (?1, ?2)",
                        params![gid, uid],
                    )?;
                    result.memberships_created += 1;
                }
                None => tracing::warn!(
                    "User '{}': group_id {} not found in backup, membership skipped",
                    bu.name,
                    old_gid
                ),
            }
        }
        result.users_created += 1;
        if name != bu.name {
            tracing::warn!(
                "Importing user '{}' as '{}': another user already has that name",
                bu.name,
                name
            );
            result
                .users_renamed
                .push(format!("{} -> {}", bu.name, name));
        }
        taken_names.insert(name);
    }

    // ── Auth providers ──
    let existing_providers = name_ids(conn, "auth_providers")?;
    let mut provider_ids_taken: HashSet<i64> = existing_providers.values().copied().collect();
    let mut provider_id_map: HashMap<i64, i64> = HashMap::new();
    for bp in &backup.auth_providers {
        if let Some(&pid) = existing_providers.get(&bp.name) {
            provider_id_map.insert(bp.id, pid);
            continue;
        }
        let extra_json: Option<String> = bp
            .extra_config
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_default());
        let id = keep_id(replace, Some(bp.id), &provider_ids_taken);
        conn.execute(
            "INSERT INTO auth_providers (id, name, provider_type, enabled, priority, \
             display_name, client_id, client_secret, issuer_url, scopes, extra_config) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                id,
                bp.name,
                bp.provider_type,
                bp.enabled as i32,
                bp.priority,
                bp.display_name,
                bp.client_id,
                bp.client_secret,
                bp.issuer_url,
                bp.scopes,
                extra_json,
            ],
        )?;
        let pid = conn.last_insert_rowid();
        provider_ids_taken.insert(pid);
        provider_id_map.insert(bp.id, pid);
    }

    // ── Mapping rules (remap provider + group) ──
    for rule in &backup.mapping_rules {
        let Some(&gid) = group_id_map.get(&rule.group_id) else {
            tracing::warn!(
                "Skipping mapping rule: group_id {} not found in backup",
                rule.group_id
            );
            continue;
        };
        let pid = rule
            .provider_id
            .and_then(|old| provider_id_map.get(&old).copied());
        conn.execute(
            "INSERT INTO group_mapping_rules (provider_id, priority, match_type, match_field, \
             match_value, group_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                pid,
                rule.priority,
                rule.match_type,
                rule.match_field,
                rule.match_value,
                gid
            ],
        )?;
    }

    // ── External identities (remap user + provider; drop dangling ones) ──
    for ident in &backup.external_identities {
        let (Some(&uid), Some(&pid)) = (
            user_id_map.get(&ident.user_id),
            provider_id_map.get(&ident.provider_id),
        ) else {
            tracing::warn!(
                "Skipping external_identity for external_sub '{}': its user or provider is not imported",
                ident.external_sub
            );
            result.external_identities_skipped += 1;
            continue;
        };
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM external_identities \
             WHERE provider_id = ?1 AND external_sub = ?2)",
            params![pid, ident.external_sub],
            |r| r.get(0),
        )?;
        if exists {
            result.external_identities_skipped += 1;
            continue;
        }
        ConfigDb::insert_external_identity(
            conn,
            uid,
            pid,
            &ident.external_sub,
            ident.email.as_deref(),
            ident.display_name.as_deref(),
            ident.raw_claims.as_ref(),
            ident.email_verified,
        )?;
        result.external_identities_created += 1;
    }

    // "Deleted" means gone after the restore: a user (by access key) or a
    // group (by name) that the backup brings back does not count.
    let after = user_keys(conn)?;
    let keys_after: HashSet<&String> = after.iter().map(|(_, k)| k).collect();
    result.users_deleted = before
        .iter()
        .filter(|(_, k)| !keys_after.contains(k))
        .count() as u32;
    let groups_after: HashSet<String> = name_ids(conn, "groups")?.into_keys().collect();
    result.groups_deleted = groups_before.difference(&groups_after).count() as u32;
    result.stale_user_ids = before.difference(&after).map(|(id, _)| *id).collect();
    result.stale_user_ids.sort_unstable();
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn perm() -> Permission {
        Permission {
            id: 0,
            effect: "Allow".into(),
            actions: vec!["read".into()],
            resources: vec!["releases/*".into()],
            conditions: None,
        }
    }

    fn open() -> (tempfile::TempDir, ConfigDb) {
        let dir = tempfile::tempdir().unwrap();
        let db = ConfigDb::open_or_create(&dir.path().join("c.db"), "pw").unwrap();
        (dir, db)
    }

    fn backup_user(id: i64, name: &str, key: &str, groups: Vec<i64>) -> BackupUser {
        BackupUser {
            id: Some(id),
            name: name.into(),
            access_key_id: key.into(),
            secret_access_key: format!("{key}-secret"),
            enabled: true,
            permissions: vec![perm()],
            group_ids: groups,
        }
    }

    fn names(db: &ConfigDb) -> Vec<String> {
        let mut n: Vec<String> = db
            .load_users()
            .unwrap()
            .into_iter()
            .map(|u| u.name)
            .collect();
        n.sort();
        n
    }

    #[test]
    fn replace_deletes_what_the_backup_lacks_and_keeps_ids() {
        let (_d, db) = open();
        db.create_user("dana", "AKDANA", "s", true, &[perm()])
            .unwrap();
        let backup = IamBackup {
            version: 2,
            users: vec![backup_user(7, "ci-uploader", "AKCI", vec![3])],
            groups: vec![BackupGroup {
                id: 3,
                name: "Engineering".into(),
                description: String::new(),
                permissions: vec![],
                member_ids: vec![7],
            }],
            auth_providers: vec![],
            mapping_rules: vec![],
            external_identities: vec![],
        };
        let r = db
            .restore_iam(&backup, IamRestoreMode::Replace, None)
            .unwrap();
        assert_eq!(names(&db), ["ci-uploader"]);
        assert_eq!(r.users_deleted, 1);
        let u = db.get_user_by_access_key("AKCI").unwrap().unwrap();
        assert_eq!(u.id, 7, "the backup's id is kept");
        assert_eq!(u.group_ids, [3]);

        // Merge keeps an extra user.
        db.create_user("dana", "AKDANA", "s", true, &[perm()])
            .unwrap();
        let r = db
            .restore_iam(&backup, IamRestoreMode::Merge, None)
            .unwrap();
        assert_eq!(names(&db), ["ci-uploader", "dana"]);
        assert_eq!((r.users_deleted, r.users_skipped), (0, 1));
        assert!(r.stale_user_ids.is_empty());
    }

    #[test]
    fn a_failed_restore_changes_nothing() {
        let (_d, db) = open();
        db.create_user("dana", "AKDANA", "s", true, &[perm()])
            .unwrap();
        let dup = |id| BackupGroup {
            id,
            name: "dup".into(),
            description: String::new(),
            permissions: vec![],
            member_ids: vec![],
        };
        let backup = IamBackup {
            version: 2,
            users: vec![],
            groups: vec![dup(1), dup(2)],
            auth_providers: vec![],
            mapping_rules: vec![],
            external_identities: vec![],
        };
        assert!(db
            .restore_iam(&backup, IamRestoreMode::Replace, None)
            .is_err());
        assert_eq!(names(&db), ["dana"]);
    }
}
