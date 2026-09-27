// SPDX-License-Identifier: BUSL-1.1

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

use rusqlite::{params, OptionalExtension};
use std::collections::HashMap;

use crate::iam::{
    normalize_permissions, CurrentIam, IamDiff, MappingRulesAction, Permission, ReconcileStats,
};

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

        delete_rows(&tx, diff, &mut stats)?;
        let group_name_to_id = upsert_groups(&tx, diff, current, &mut stats)?;
        let provider_name_to_id = upsert_providers(&tx, diff, current, &mut stats)?;
        upsert_users(&tx, diff, &group_name_to_id, &mut stats)?;
        replace_mapping_rules(
            &tx,
            diff,
            current,
            &provider_name_to_id,
            &group_name_to_id,
            &mut stats,
        )?;
        upsert_external_identities(&tx, diff, current, &provider_name_to_id, &mut stats)?;

        tx.commit()?;

        count_totals(diff, current, &mut stats);
        Ok(stats)
    }
}

/// Steps 1–4 of [`ConfigDb::apply_iam_reconcile`]: the deletes.
fn delete_rows(
    tx: &rusqlite::Transaction,
    diff: &IamDiff,
    stats: &mut ReconcileStats,
) -> Result<(), ConfigDbError> {
    // ── 1. Delete mapping rules wholesale IFF the diff says we
    //      must (ClearAll or ReplaceWith). `Keep` is the idempotent
    //      no-op path — never touches the table. This is the
    //      post-C1 shape: the old `Vec + helper` form couldn't
    //      distinguish "YAML matches non-empty DB, keep" from
    //      "YAML empty, wipe" and silently wiped on every
    //      idempotent re-apply.
    match &diff.mapping_rules {
        MappingRulesAction::Keep => {}
        MappingRulesAction::ClearAll | MappingRulesAction::ReplaceWith(_) => {
            tx.execute("DELETE FROM group_mapping_rules", [])?;
            // We'll re-insert in step 8 iff ReplaceWith.
        }
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
    Ok(())
}

/// Step 5: create + update groups; the `name → id` map of every group
/// that stays.
fn upsert_groups(
    tx: &rusqlite::Transaction,
    diff: &IamDiff,
    current: &CurrentIam,
    stats: &mut ReconcileStats,
) -> Result<HashMap<String, i64>, ConfigDbError> {
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
        replace_group_permissions(tx, gid, &g.permissions)?;
        group_name_to_id.insert(g.name.clone(), gid);
        stats.groups_created.push(g.name.clone());
    }
    for (gid, g) in &diff.groups_to_update {
        tx.execute(
            "UPDATE groups SET name = ?1, description = ?2 WHERE id = ?3",
            params![g.name, g.description, gid],
        )?;
        replace_group_permissions(tx, *gid, &g.permissions)?;
        group_name_to_id.insert(g.name.clone(), *gid);
        stats.groups_updated.push(g.name.clone());
    }
    Ok(group_name_to_id)
}

/// Step 6: create + update providers; the `name → id` map of every
/// provider that stays.
fn upsert_providers(
    tx: &rusqlite::Transaction,
    diff: &IamDiff,
    current: &CurrentIam,
    stats: &mut ReconcileStats,
) -> Result<HashMap<String, i64>, ConfigDbError> {
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
    Ok(provider_name_to_id)
}

/// Step 7: create + update users with their permissions and memberships.
fn upsert_users(
    tx: &rusqlite::Transaction,
    diff: &IamDiff,
    group_name_to_id: &HashMap<String, i64>,
    stats: &mut ReconcileStats,
) -> Result<(), ConfigDbError> {
    // ── 7. Create + update users. Resolve `groups` names via the
    //      group_name_to_id map built above. `auth_source` rides the
    //      row verbatim (default 'local' when the YAML was silent) so
    //      a full-IAM round-trip restores OAuth rows as external (#71).
    for u in &diff.users_to_create {
        tx.execute(
            "INSERT INTO users (name, access_key_id, secret_access_key, enabled, auth_source) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                u.name,
                u.access_key_id,
                u.secret_access_key,
                u.enabled as i32,
                u.auth_source.clone().unwrap_or_else(|| "local".into()),
            ],
        )?;
        let uid = tx.last_insert_rowid();
        replace_user_permissions(tx, uid, &u.permissions)?;
        replace_user_group_memberships(tx, uid, &u.groups, group_name_to_id)?;
        stats.users_created.push(u.name.clone());
    }
    for (uid, u) in &diff.users_to_update {
        tx.execute(
            "UPDATE users SET \
               name = ?1, access_key_id = ?2, secret_access_key = ?3, enabled = ?4, \
               auth_source = ?5 \
             WHERE id = ?6",
            params![
                u.name,
                u.access_key_id,
                u.secret_access_key,
                u.enabled as i32,
                u.auth_source.clone().unwrap_or_else(|| "local".into()),
                uid,
            ],
        )?;
        replace_user_permissions(tx, *uid, &u.permissions)?;
        replace_user_group_memberships(tx, *uid, &u.groups, group_name_to_id)?;
        stats.users_updated.push(u.name.clone());
    }
    Ok(())
}

/// Step 8: re-insert the mapping rules (`ReplaceWith`), or count the
/// rules that step 1 cleared (`ClearAll`).
fn replace_mapping_rules(
    tx: &rusqlite::Transaction,
    diff: &IamDiff,
    current: &CurrentIam,
    provider_name_to_id: &HashMap<String, i64>,
    group_name_to_id: &HashMap<String, i64>,
    stats: &mut ReconcileStats,
) -> Result<(), ConfigDbError> {
    // ── 8. Re-insert mapping rules if diff says ReplaceWith.
    //      ClearAll has already done its DELETE in step 1; Keep
    //      is a no-op.
    if let MappingRulesAction::ReplaceWith(ref rules) = diff.mapping_rules {
        // Content uids: every node that applies this YAML writes the same
        // uids, so the sync merge sees one rule set, not two.
        let mut taken = std::collections::HashSet::new();
        for r in rules {
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
            let uid = super::iam_merge::unique_rule_uid(
                super::iam_merge::content_rule_uid(
                    r.provider.as_deref(),
                    r.priority,
                    &r.match_type,
                    &r.match_field,
                    &r.match_value,
                    &r.group,
                ),
                &mut taken,
            );
            tx.execute(
                "INSERT INTO group_mapping_rules \
                 (provider_id, priority, match_type, match_field, match_value, group_id, rule_uid) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    provider_id,
                    r.priority,
                    r.match_type,
                    r.match_field,
                    r.match_value,
                    group_id,
                    uid,
                ],
            )?;
        }
        stats.mapping_rules_replaced = rules.len();
    } else if matches!(diff.mapping_rules, MappingRulesAction::ClearAll) {
        // Track the clear for audit accuracy — stats previously
        // showed 0 even when rules were wiped. We don't know the
        // old count cheaply (it's in `current.mapping_rules` but
        // re-reading after DELETE would be silly); use its len.
        stats.mapping_rules_replaced = current.mapping_rules.len();
    }
    Ok(())
}

/// Step 9: upsert the OAuth login bindings.
fn upsert_external_identities(
    tx: &rusqlite::Transaction,
    diff: &IamDiff,
    current: &CurrentIam,
    provider_name_to_id: &HashMap<String, i64>,
    stats: &mut ReconcileStats,
) -> Result<(), ConfigDbError> {
    // ── 9. Upsert OAuth login bindings (#71). Keyed by
    //      (provider_id, external_sub) — the pair the OAuth callback
    //      looks up, so an upsert of the SAME subject can never
    //      create a duplicate binding, and a restore after a DB wipe
    //      re-links to the freshly-created user id. Bindings absent
    //      from the YAML are left alone (never deleted — hand-
    //      authored YAML and redacted exports don't carry them, and
    //      deletes already cascade through user/provider deletes).
    if !diff.external_identities.is_empty() {
        // Post-commit-writes state: created users/providers are in the
        // maps from steps 6-7; deleted ones were filtered out there.
        // Created users' ids resolve by UNIQUE access_key_id (rule INSERTs
        // interleave, so last_insert_rowid is unreliable here).
        let mut user_name_to_id: HashMap<String, i64> = current
            .users
            .iter()
            .filter(|u| !diff.users_to_delete.iter().any(|(id, _)| *id == u.id))
            .map(|u| (u.name.clone(), u.id))
            .collect();
        // Users UPDATED by step 7 may have been renamed — their new name
        // only exists in the diff, not in `current`. Insert it so a
        // binding referencing the new name resolves.
        for (uid, u) in &diff.users_to_update {
            user_name_to_id.insert(u.name.clone(), *uid);
        }
        for u in &diff.users_to_create {
            if let Some(uid) = query_user_id_by_access_key(tx, &u.access_key_id)? {
                user_name_to_id.insert(u.name.clone(), uid);
            }
        }
        for ident in &diff.external_identities {
            let Some(uid) = user_name_to_id.get(&ident.user) else {
                // Validation rejects unknown refs; defensive skip keeps
                // a restore applying even if a binding names a user the
                // snapshot omitted.
                continue;
            };
            let Some(pid) = provider_name_to_id.get(&ident.provider) else {
                continue;
            };
            let claims_json: Option<String> = ident
                .raw_claims
                .as_ref()
                .map(|v| serde_json::to_string(v).unwrap_or_default());
            let verified = ident.email_verified.unwrap_or(false);
            // Idempotency: only count (and write) a binding whose stored
            // row differs. Re-applying an unchanged lossless export must
            // stay a no-op — the docstring promises it.
            if external_identity_matches(
                tx,
                *pid,
                &ident.subject,
                *uid,
                ident.email.as_deref(),
                ident.display_name.as_deref(),
                claims_json.as_deref(),
                verified,
            )? {
                continue;
            }
            tx.execute(
                "INSERT INTO external_identities \
                 (user_id, provider_id, external_sub, email, display_name, raw_claims, email_verified) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
                 ON CONFLICT(provider_id, external_sub) DO UPDATE SET \
                   user_id = excluded.user_id, \
                   email = excluded.email, \
                   display_name = excluded.display_name, \
                   raw_claims = excluded.raw_claims, \
                   email_verified = excluded.email_verified",
                params![
                    uid,
                    pid,
                    ident.subject,
                    ident.email,
                    ident.display_name,
                    claims_json,
                    verified as i32,
                ],
            )?;
            stats.external_identities_applied += 1;
        }
    }
    Ok(())
}

/// Pure: the post-reconcile row totals (created + updated + kept).
fn count_totals(diff: &IamDiff, current: &CurrentIam, stats: &mut ReconcileStats) {
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
}

/// Replace permission rows for a given owner (user or group). The
/// table + FK column varies with the owner type; everything else is
/// identical. Reuses the generic `insert_permission_rows` helper so
/// the column shape (conditions_json vs conditions, effect defaulting)
/// stays in one place.
///
/// Two callers (`replace_group_permissions`, `replace_user_permissions`
/// below) are thin delegates to this — hygiene #4 collapsed the two
/// parallel 10-line functions that used to clone each other.
fn replace_permissions(
    tx: &rusqlite::Transaction,
    table: &str,
    fk_col: &str,
    owner_id: i64,
    perms: &[Permission],
) -> Result<(), ConfigDbError> {
    tx.execute(
        &format!("DELETE FROM {} WHERE {} = ?1", table, fk_col),
        params![owner_id],
    )?;
    let mut perms = perms.to_vec();
    normalize_permissions(&mut perms);
    ConfigDb::insert_permission_rows(tx, table, fk_col, owner_id, &perms)
}

fn replace_group_permissions(
    tx: &rusqlite::Transaction,
    group_id: i64,
    perms: &[Permission],
) -> Result<(), ConfigDbError> {
    replace_permissions(tx, "group_permissions", "group_id", group_id, perms)
}

fn replace_user_permissions(
    tx: &rusqlite::Transaction,
    user_id: i64,
    perms: &[Permission],
) -> Result<(), ConfigDbError> {
    replace_permissions(tx, "permissions", "user_id", user_id, perms)
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

/// Resolve a user's id by their UNIQUE access_key_id, inside the reconcile
/// transaction. Used by the external-identity upsert to map a created user's
/// NAME to its fresh id — `last_insert_rowid` is unreliable there because
/// group/rule INSERTs interleave between user INSERTs.
fn query_user_id_by_access_key(
    tx: &rusqlite::Transaction<'_>,
    access_key_id: &str,
) -> Result<Option<i64>, ConfigDbError> {
    tx.query_row(
        "SELECT id FROM users WHERE access_key_id = ?1",
        params![access_key_id],
        |r| r.get(0),
    )
    .optional()
    .map_err(ConfigDbError::from)
}

/// True when the stored external identity for `(provider_id, subject)` already
/// equals the incoming fields — the reconcile then skips the write so a
/// re-apply of an unchanged full-IAM export stays a true no-op (#71 review).
#[allow(clippy::too_many_arguments)]
fn external_identity_matches(
    tx: &rusqlite::Transaction<'_>,
    provider_id: i64,
    subject: &str,
    user_id: i64,
    email: Option<&str>,
    display_name: Option<&str>,
    raw_claims: Option<&str>,
    email_verified: bool,
) -> Result<bool, ConfigDbError> {
    let existing = tx
        .query_row(
            "SELECT user_id, email, display_name, raw_claims, email_verified \
             FROM external_identities WHERE provider_id = ?1 AND external_sub = ?2",
            params![provider_id, subject],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, i64>(4)? != 0,
                ))
            },
        )
        .optional()?;
    Ok(match existing {
        Some((u, e, d, c, v)) => {
            u == user_id
                && e == email.map(str::to_string)
                && d == display_name.map(str::to_string)
                && c == raw_claims.map(str::to_string)
                && v == email_verified
        }
        None => false,
    })
}

#[cfg(test)]
mod tests {
    use crate::config_db::ConfigDb;
    use crate::iam::{reconcile_declarative_iam_at_boot, DeclarativeIam};

    fn snapshot(yaml: &str) -> DeclarativeIam {
        #[derive(serde::Deserialize)]
        struct Doc {
            #[serde(default)]
            users: Vec<crate::iam::DeclarativeUser>,
            #[serde(default)]
            groups: Vec<crate::iam::DeclarativeGroup>,
            #[serde(default)]
            auth_providers: Vec<crate::iam::DeclarativeAuthProvider>,
            #[serde(default)]
            mapping_rules: Vec<crate::iam::DeclarativeMappingRule>,
            #[serde(default)]
            external_identities: Vec<crate::iam::DeclarativeExternalIdentity>,
        }
        let d: Doc = serde_yaml::from_str(yaml).unwrap();
        DeclarativeIam {
            users: d.users,
            groups: d.groups,
            auth_providers: d.auth_providers,
            mapping_rules: d.mapping_rules,
            external_identities: d.external_identities,
        }
    }

    fn names<T>(rows: Vec<T>, name: impl Fn(&T) -> String) -> Vec<String> {
        let mut v: Vec<String> = rows.iter().map(name).collect();
        v.sort();
        v
    }

    /// Totals count created + updated + the current rows that the diff
    /// neither deletes nor updates.
    #[test]
    fn count_totals_adds_kept_rows() {
        use crate::iam::{CurrentIam, IamDiff, IamUser, ReconcileStats};
        let user = |id: i64, name: &str| IamUser {
            id,
            name: name.into(),
            access_key_id: format!("AK{id}"),
            secret_access_key: "sk".into(),
            enabled: true,
            created_at: String::new(),
            permissions: vec![],
            group_ids: vec![],
            auth_source: "local".into(),
            iam_policies: vec![],
        };
        let decl = |name: &str| -> crate::iam::DeclarativeUser {
            serde_yaml::from_str(&format!("{{ name: {name}, access_key_id: AK{name} }}")).unwrap()
        };
        let current = CurrentIam {
            users: vec![user(1, "a"), user(2, "b"), user(3, "c")],
            ..Default::default()
        };
        let diff = IamDiff {
            users_to_delete: vec![(1, "a".into())],
            users_to_update: vec![(2, decl("b"))],
            users_to_create: vec![decl("x"), decl("y")],
            ..Default::default()
        };
        let mut stats = ReconcileStats::default();
        super::count_totals(&diff, &current, &mut stats);
        assert_eq!(
            (stats.users_total, stats.groups_total, stats.providers_total),
            (4, 0, 0)
        );
    }

    /// Every step of the reconcile, in two applies: the first creates
    /// groups, a provider, users with memberships, a mapping rule and an
    /// OAuth binding; the second updates, deletes and creates, and
    /// replaces the rules. Pins the DB state and the stats.
    #[test]
    fn apply_iam_reconcile_runs_every_step() {
        let db = ConfigDb::in_memory("test-pass").unwrap();
        let first = snapshot(
            r#"
groups:
  - { name: Engineering, description: eng, permissions: [{ actions: [read], resources: ["releases/*"] }] }
  - { name: Old }
auth_providers:
  - { name: corp, provider_type: oidc, issuer_url: "https://idp.example.com", client_id: cid, client_secret: cs }
users:
  - { name: dana, access_key_id: AKDANA, secret_access_key: s1, groups: [Engineering] }
  - { name: ci-uploader, access_key_id: AKCI, secret_access_key: s2, permissions: [{ actions: [write], resources: ["releases/*"] }] }
mapping_rules:
  - { provider: corp, match_type: email_domain, match_value: example.com, group: Engineering }
external_identities:
  - { user: dana, provider: corp, subject: sub-dana, email: dana@example.com }
"#,
        );
        let s = reconcile_declarative_iam_at_boot(&db, &first).unwrap();
        assert_eq!(s.groups_created, ["Engineering", "Old"]);
        assert_eq!(s.providers_created, ["corp"]);
        assert_eq!(s.users_created, ["dana", "ci-uploader"]);
        assert_eq!(s.mapping_rules_replaced, 1);
        assert_eq!(s.external_identities_applied, 1);
        assert_eq!(
            (s.users_total, s.groups_total, s.providers_total),
            (2, 2, 1)
        );
        let users = db.load_users().unwrap();
        let dana = users.iter().find(|u| u.name == "dana").unwrap();
        let groups = db.load_groups().unwrap();
        let eng = groups.iter().find(|g| g.name == "Engineering").unwrap();
        assert_eq!(eng.member_ids, [dana.id]);
        assert_eq!(eng.permissions.len(), 1);
        let rules = db.load_group_mapping_rules().unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].group_id, eng.id);
        let ids = db.list_external_identities().unwrap();
        assert_eq!(ids.len(), 1);
        assert_eq!(ids[0].user_id, dana.id);

        // Re-applying the same snapshot changes nothing.
        let again = reconcile_declarative_iam_at_boot(&db, &first).unwrap();
        assert!(again.users_created.is_empty() && again.users_updated.is_empty());
        assert_eq!(again.external_identities_applied, 0);

        let second = snapshot(
            r#"
groups:
  - { name: Engineering, description: engineering }
auth_providers:
  - { name: corp, provider_type: oidc, issuer_url: "https://idp.example.com", client_id: cid2, client_secret: cs }
users:
  - { name: dana, access_key_id: AKDANA, secret_access_key: s1, enabled: false, groups: [Engineering] }
  - { name: backup-bot, access_key_id: AKBOT, secret_access_key: s3 }
mapping_rules:
  - { provider: corp, match_type: email_domain, match_value: example.org, group: Engineering }
  - { match_type: email_exact, match_value: dana@example.com, group: Engineering }
"#,
        );
        let s = reconcile_declarative_iam_at_boot(&db, &second).unwrap();
        assert_eq!(s.users_deleted, ["ci-uploader"]);
        assert_eq!(s.groups_deleted, ["Old"]);
        assert!(s.providers_deleted.is_empty());
        assert_eq!(s.groups_updated, ["Engineering"]);
        assert_eq!(s.providers_updated, ["corp"]);
        assert_eq!(s.users_updated, ["dana"]);
        assert_eq!(s.users_created, ["backup-bot"]);
        assert_eq!(s.mapping_rules_replaced, 2);
        assert_eq!(
            (s.users_total, s.groups_total, s.providers_total),
            (2, 1, 1)
        );
        assert_eq!(
            names(db.load_users().unwrap(), |u| u.name.clone()),
            ["backup-bot", "dana"]
        );
        let groups = db.load_groups().unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].description, "engineering");
        assert!(groups[0].permissions.is_empty(), "permissions replaced");
        let rules = db.load_group_mapping_rules().unwrap();
        assert_eq!(
            names(rules, |r| r.match_value.clone()),
            ["dana@example.com", "example.org"]
        );
        // A binding absent from the YAML is left alone.
        assert_eq!(db.list_external_identities().unwrap().len(), 1);

        // An empty rule set clears the table and counts what it cleared.
        let third = snapshot(
            r#"
groups:
  - { name: Engineering, description: engineering }
auth_providers:
  - { name: corp, provider_type: oidc, issuer_url: "https://idp.example.com", client_id: cid2, client_secret: cs }
users:
  - { name: dana, access_key_id: AKDANA, secret_access_key: s1, enabled: false, groups: [Engineering] }
  - { name: backup-bot, access_key_id: AKBOT, secret_access_key: s3 }
"#,
        );
        let s = reconcile_declarative_iam_at_boot(&db, &third).unwrap();
        assert_eq!(s.mapping_rules_replaced, 2);
        assert!(db.load_group_mapping_rules().unwrap().is_empty());
    }
}
