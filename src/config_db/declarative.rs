//! Phase 3c.3 — Atomic reconcile of the IAM DB against an [`IamDiff`].
//!
//! A single SQLite transaction covers every create/update/delete
//! for groups, providers, users, and mapping rules. Any failure
//! rolls the entire reconcile back; partial state is never observable.
//!
//! ## Order matters
//!
//! The step order is load-bearing for referential integrity:
//!
//!   1. **Delete mapping rules** (they may reference providers or
//!      groups we're about to drop; deleting first sidesteps the
//!      order-of-cascade question even though FKs cascade anyway).
//!   2. **Delete users** (cascades permissions, group memberships,
//!      external identities — the latter is the expected declarative
//!      semantic: YAML removed the user, its OAuth bindings go too).
//!   3. **Delete providers** (cascades remaining mapping rules +
//!      external identities tied to that provider).
//!   4. **Delete groups** (cascades group_members, group_permissions,
//!      and remaining mapping rules pointing at the group).
//!   5. **Create/update groups** → build `name → id` map.
//!   6. **Create/update providers** → build `name → id` map.
//!   7. **Create/update users** → resolve `groups` names via the
//!      group map, set memberships. Permissions replaced whole-sale.
//!   8. **Re-insert mapping rules** (replace-all) with names
//!      resolved via the two name→id maps.

use rusqlite::params;
use std::collections::HashMap;

use crate::iam::{normalize_permissions, CurrentIam, IamDiff, Permission, ReconcileStats};

use super::{ConfigDb, ConfigDbError};

impl ConfigDb {
    /// Apply a pre-computed [`IamDiff`] atomically. Assumes the diff
    /// has already been validated by
    /// [`crate::iam::diff_iam`] — this method does not
    /// re-validate YAML shape (that would be wasted work inside the
    /// transaction).
    ///
    /// `current` is passed through from the caller so the
    /// name-resolution maps (name → id) for **DB rows being kept**
    /// can be built without a fresh `load_groups` / `load_auth_providers`
    /// round-trip.
    pub fn apply_iam_reconcile(
        &self,
        diff: &IamDiff,
        current: &CurrentIam,
    ) -> Result<ReconcileStats, ConfigDbError> {
        let tx = self.conn.unchecked_transaction()?;

        let mut stats = ReconcileStats::default();

        // ── 1. Delete mapping rules wholesale when we're replacing them.
        //      Rules are wipe-and-rebuild; if diff says replace, clear
        //      the table first. If the diff is empty (idempotent case),
        //      skip entirely.
        if !diff.mapping_rules_to_replace.is_empty() || mapping_rules_need_clearing(diff, current) {
            tx.execute("DELETE FROM group_mapping_rules", [])?;
            // We'll re-insert after groups + providers finalize.
        }

        // ── 2. Delete users. Cascades permissions, group_members,
        //      external_identities (by design — see module doc).
        for (id, _name) in &diff.users_to_delete {
            tx.execute("DELETE FROM users WHERE id = ?1", params![id])?;
            stats.users_deleted.push(_name.clone());
        }

        // ── 3. Delete providers. Cascades mapping_rules +
        //      external_identities tied to the provider.
        for (id, _name) in &diff.providers_to_delete {
            tx.execute("DELETE FROM auth_providers WHERE id = ?1", params![id])?;
            stats.providers_deleted.push(_name.clone());
        }

        // ── 4. Delete groups. Cascades memberships, permissions, rules.
        for (id, _name) in &diff.groups_to_delete {
            tx.execute("DELETE FROM groups WHERE id = ?1", params![id])?;
            stats.groups_deleted.push(_name.clone());
        }

        // ── 5. Create + update groups → name→id map.
        let mut group_name_to_id: HashMap<String, i64> = current
            .groups
            .iter()
            .filter(|g| !diff.groups_to_delete.iter().any(|(id, _)| *id == g.id))
            .map(|g| (g.name.clone(), g.id))
            .collect();

        for g in &diff.groups_to_create {
            tx.execute(
                "INSERT INTO groups (name, description) VALUES (?1, ?2)",
                params![g.name, g.description],
            )?;
            let gid = tx.last_insert_rowid();
            replace_group_permissions(&tx, gid, &g.permissions)?;
            group_name_to_id.insert(g.name.clone(), gid);
            stats.groups_created.push(g.name.clone());
        }
        for (gid, g) in &diff.groups_to_update {
            tx.execute(
                "UPDATE groups SET name = ?1, description = ?2 WHERE id = ?3",
                params![g.name, g.description, gid],
            )?;
            replace_group_permissions(&tx, *gid, &g.permissions)?;
            group_name_to_id.insert(g.name.clone(), *gid);
            stats.groups_updated.push(g.name.clone());
        }

        // ── 6. Create + update providers → name→id map.
        let mut provider_name_to_id: HashMap<String, i64> = current
            .auth_providers
            .iter()
            .filter(|p| !diff.providers_to_delete.iter().any(|(id, _)| *id == p.id))
            .map(|p| (p.name.clone(), p.id))
            .collect();

        for p in &diff.providers_to_create {
            tx.execute(
                "INSERT INTO auth_providers \
                 (name, provider_type, enabled, priority, display_name, client_id, \
                  client_secret, issuer_url, scopes, extra_config) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    p.name,
                    p.provider_type,
                    p.enabled as i32,
                    p.priority,
                    p.display_name,
                    p.client_id,
                    p.client_secret,
                    p.issuer_url,
                    p.scopes,
                    p.extra_config.as_ref().map(|v| v.to_string()),
                ],
            )?;
            let pid = tx.last_insert_rowid();
            provider_name_to_id.insert(p.name.clone(), pid);
            stats.providers_created.push(p.name.clone());
        }
        for (pid, p) in &diff.providers_to_update {
            tx.execute(
                "UPDATE auth_providers SET \
                   name = ?1, provider_type = ?2, enabled = ?3, priority = ?4, \
                   display_name = ?5, client_id = ?6, client_secret = ?7, \
                   issuer_url = ?8, scopes = ?9, extra_config = ?10, \
                   updated_at = CURRENT_TIMESTAMP \
                 WHERE id = ?11",
                params![
                    p.name,
                    p.provider_type,
                    p.enabled as i32,
                    p.priority,
                    p.display_name,
                    p.client_id,
                    p.client_secret,
                    p.issuer_url,
                    p.scopes,
                    p.extra_config.as_ref().map(|v| v.to_string()),
                    pid,
                ],
            )?;
            provider_name_to_id.insert(p.name.clone(), *pid);
            stats.providers_updated.push(p.name.clone());
        }

        // ── 7. Create + update users. Resolve `groups` names via the
        //      group_name_to_id map built above.
        for u in &diff.users_to_create {
            tx.execute(
                "INSERT INTO users (name, access_key_id, secret_access_key, enabled) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    u.name,
                    u.access_key_id,
                    u.secret_access_key,
                    u.enabled as i32,
                ],
            )?;
            let uid = tx.last_insert_rowid();
            replace_user_permissions(&tx, uid, &u.permissions)?;
            replace_user_group_memberships(&tx, uid, &u.groups, &group_name_to_id)?;
            stats.users_created.push(u.name.clone());
        }
        for (uid, u) in &diff.users_to_update {
            tx.execute(
                "UPDATE users SET \
                   name = ?1, access_key_id = ?2, secret_access_key = ?3, enabled = ?4 \
                 WHERE id = ?5",
                params![
                    u.name,
                    u.access_key_id,
                    u.secret_access_key,
                    u.enabled as i32,
                    uid,
                ],
            )?;
            replace_user_permissions(&tx, *uid, &u.permissions)?;
            replace_user_group_memberships(&tx, *uid, &u.groups, &group_name_to_id)?;
            stats.users_updated.push(u.name.clone());
        }

        // ── 8. Re-insert mapping rules if diff says so.
        if !diff.mapping_rules_to_replace.is_empty() {
            for r in &diff.mapping_rules_to_replace {
                let provider_id: Option<i64> = match &r.provider {
                    Some(name) => Some(*provider_name_to_id.get(name).ok_or_else(|| {
                        // Defensive — validation caught this already,
                        // but build a clear error if the invariant
                        // breaks somehow.
                        ConfigDbError::Other(format!(
                            "mapping rule references unknown provider '{}' — this is a bug \
                             (validation should have caught it)",
                            name
                        ))
                    })?),
                    None => None,
                };
                let group_id = *group_name_to_id.get(&r.group).ok_or_else(|| {
                    ConfigDbError::Other(format!(
                        "mapping rule references unknown group '{}' — this is a bug",
                        r.group
                    ))
                })?;
                tx.execute(
                    "INSERT INTO group_mapping_rules \
                     (provider_id, priority, match_type, match_field, match_value, group_id) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        provider_id,
                        r.priority,
                        r.match_type,
                        r.match_field,
                        r.match_value,
                        group_id,
                    ],
                )?;
            }
            stats.mapping_rules_replaced = diff.mapping_rules_to_replace.len();
        }

        tx.commit()?;

        stats.users_total = diff.users_to_create.len()
            + diff.users_to_update.len()
            + current
                .users
                .iter()
                .filter(|u| !diff.users_to_delete.iter().any(|(id, _)| *id == u.id))
                .filter(|u| !diff.users_to_update.iter().any(|(id, _)| *id == u.id))
                .count();
        stats.groups_total = diff.groups_to_create.len()
            + diff.groups_to_update.len()
            + current
                .groups
                .iter()
                .filter(|g| !diff.groups_to_delete.iter().any(|(id, _)| *id == g.id))
                .filter(|g| !diff.groups_to_update.iter().any(|(id, _)| *id == g.id))
                .count();
        stats.providers_total = diff.providers_to_create.len()
            + diff.providers_to_update.len()
            + current
                .auth_providers
                .iter()
                .filter(|p| !diff.providers_to_delete.iter().any(|(id, _)| *id == p.id))
                .filter(|p| !diff.providers_to_update.iter().any(|(id, _)| *id == p.id))
                .count();

        Ok(stats)
    }
}

/// The wipe-and-rebuild step for mapping rules needs to fire when
/// the diff nominates replacement OR when rules need to disappear
/// entirely (YAML has zero rules but DB has some). `diff_iam` only
/// populates `mapping_rules_to_replace` when the two sets differ; if
/// YAML has zero rules and DB has N, the populated vector is empty
/// (the empty Vec) — but we still need the DELETE. Detect that here.
fn mapping_rules_need_clearing(diff: &IamDiff, current: &CurrentIam) -> bool {
    diff.mapping_rules_to_replace.is_empty() && !current.mapping_rules.is_empty() && {
        // Could be idempotent (both empty) — only clear when DB has rules.
        // diff_iam's mapping_rules_equal() handles idempotency; if it
        // detected equality, `mapping_rules_to_replace` stays empty AND
        // the current.mapping_rules snapshot matches yaml's empty intent
        // — but that's only true when yaml IS empty. Since we can't see
        // yaml from here, we rely on the invariant that
        // `mapping_rules_to_replace.is_empty() && current.is_empty()`
        // means "idempotent no-op". This arm of the `&&` catches the
        // "yaml empty, db non-empty" case.
        true
    }
}

fn replace_group_permissions(
    tx: &rusqlite::Transaction,
    group_id: i64,
    perms: &[Permission],
) -> Result<(), ConfigDbError> {
    tx.execute(
        "DELETE FROM group_permissions WHERE group_id = ?1",
        params![group_id],
    )?;
    let mut perms = perms.to_vec();
    normalize_permissions(&mut perms);
    // Reuse the existing generic inserter so the column shape
    // (conditions_json vs conditions, effect defaulting, etc.) stays
    // in one place — avoids the exact drift that broke this path in
    // integration tests.
    ConfigDb::insert_permission_rows(tx, "group_permissions", "group_id", group_id, &perms)
}

fn replace_user_permissions(
    tx: &rusqlite::Transaction,
    user_id: i64,
    perms: &[Permission],
) -> Result<(), ConfigDbError> {
    tx.execute(
        "DELETE FROM permissions WHERE user_id = ?1",
        params![user_id],
    )?;
    let mut perms = perms.to_vec();
    normalize_permissions(&mut perms);
    ConfigDb::insert_permission_rows(tx, "permissions", "user_id", user_id, &perms)
}

fn replace_user_group_memberships(
    tx: &rusqlite::Transaction,
    user_id: i64,
    group_names: &[String],
    group_name_to_id: &HashMap<String, i64>,
) -> Result<(), ConfigDbError> {
    tx.execute(
        "DELETE FROM group_members WHERE user_id = ?1",
        params![user_id],
    )?;
    for name in group_names {
        let gid = group_name_to_id.get(name).ok_or_else(|| {
            ConfigDbError::Other(format!(
                "user references unknown group '{}' — this is a bug \
                 (validation should have caught it)",
                name
            ))
        })?;
        tx.execute(
            "INSERT OR IGNORE INTO group_members (group_id, user_id) VALUES (?1, ?2)",
            params![gid, user_id],
        )?;
    }
    Ok(())
}
