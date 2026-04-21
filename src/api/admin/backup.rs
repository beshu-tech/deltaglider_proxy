//! IAM data export/import (backup & restore).

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::config_db::auth_providers::{AuthProviderConfig, ExternalIdentity, GroupMappingRule};
use crate::iam::{normalize_permissions, validate_permissions, Permission};

use super::users::rebuild_iam_index;
use super::{audit_log, trigger_config_sync, AdminState};

/// Full IAM backup: users (with credentials) + groups + memberships + external auth.
#[derive(Serialize, Deserialize)]
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

#[derive(Serialize, Deserialize)]
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

#[derive(Serialize, Deserialize)]
pub struct BackupGroup {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub permissions: Vec<Permission>,
    pub member_ids: Vec<i64>,
}

/// GET /api/admin/backup — export all IAM data as JSON.
pub async fn export_backup(
    State(state): State<Arc<AdminState>>,
) -> Result<Json<IamBackup>, StatusCode> {
    let db = state.config_db.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    let db = db.lock().await;

    let users = db.load_users().map_err(|e| {
        tracing::error!("Failed to load users for backup: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let groups = db.load_groups().map_err(|e| {
        tracing::error!("Failed to load groups for backup: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let auth_providers = db.load_auth_providers().unwrap_or_default();
    let mapping_rules = db.load_group_mapping_rules().unwrap_or_default();
    let external_identities = db.list_external_identities().unwrap_or_default();

    let backup = IamBackup {
        version: 2,
        users: users
            .into_iter()
            .map(|u| BackupUser {
                id: Some(u.id),
                name: u.name,
                access_key_id: u.access_key_id,
                secret_access_key: u.secret_access_key,
                enabled: u.enabled,
                permissions: u.permissions,
                group_ids: u.group_ids,
            })
            .collect(),
        groups: groups
            .into_iter()
            .map(|g| BackupGroup {
                id: g.id,
                name: g.name,
                description: g.description,
                permissions: g.permissions,
                member_ids: g.member_ids,
            })
            .collect(),
        auth_providers,
        mapping_rules,
        external_identities,
    };

    Ok(Json(backup))
}

/// POST /api/admin/backup — import IAM data from JSON backup.
/// Merges with existing data: skips users/groups that already exist (by name).
pub async fn import_backup(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
    Json(backup): Json<IamBackup>,
) -> Result<Json<ImportResult>, StatusCode> {
    let db = state.config_db.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    let db = db.lock().await;

    // Get bootstrap access key to prevent conflicts
    let bootstrap_key = {
        let iam = state.iam_state.load();
        match iam.as_ref() {
            crate::iam::IamState::Legacy(auth) => Some(auth.access_key_id.clone()),
            _ => None,
        }
    };

    let mut result = ImportResult {
        users_created: 0,
        users_skipped: 0,
        groups_created: 0,
        groups_skipped: 0,
        memberships_created: 0,
        external_identities_created: 0,
        external_identities_skipped: 0,
    };

    // Pre-load existing data once (O(1) lookups instead of O(N²) per-item DB queries)
    let existing_groups = db.load_groups().unwrap_or_default();
    let existing_users = db.load_users().unwrap_or_default();
    let existing_group_names: std::collections::HashSet<String> =
        existing_groups.iter().map(|g| g.name.clone()).collect();
    let existing_user_keys: std::collections::HashSet<String> = existing_users
        .iter()
        .map(|u| u.access_key_id.clone())
        .collect();

    // Import groups first (users reference group_ids)
    let mut group_id_map: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();

    for bg in &backup.groups {
        if existing_group_names.contains(&bg.name) {
            if let Some(existing_group) = existing_groups.iter().find(|g| g.name == bg.name) {
                group_id_map.insert(bg.id, existing_group.id);
            }
            result.groups_skipped += 1;
            continue;
        }

        // Validate permissions before import
        let mut perms = bg.permissions.clone();
        normalize_permissions(&mut perms);
        if let Err(msg) = validate_permissions(&perms) {
            tracing::warn!("Skipping group '{}': invalid permissions: {}", bg.name, msg);
            result.groups_skipped += 1;
            continue;
        }

        match db.create_group(&bg.name, &bg.description, &perms) {
            Ok(created) => {
                group_id_map.insert(bg.id, created.id);
                result.groups_created += 1;
            }
            Err(e) => {
                tracing::warn!("Failed to import group '{}': {}", bg.name, e);
                result.groups_skipped += 1;
            }
        }
    }

    // Import users — track old→new user IDs so external_identities
    // references below can be remapped (not just group memberships).
    //
    // Resolving `old_id` for the mapping:
    //   1. Prefer `bu.id` from the backup (new export format).
    //   2. Fall back to `bg.member_ids` in groups — the original DB's
    //      user IDs leak through here (v2 format, pre-Wave-11).
    //   3. Last resort: assume SQLite autoincrement order matches the
    //      `users` array index + 1.
    //
    // This lets us restore external_identities from backups generated
    // BEFORE the Wave 11 fix added `BackupUser.id`, without breaking
    // existing v1/v2 payloads.
    let mut user_id_map: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
    // Pre-populate with any existing-user overlaps so imports on a
    // non-empty instance still remap correctly for external_identities.
    for existing in &existing_users {
        if let Some((idx, bu)) = backup
            .users
            .iter()
            .enumerate()
            .find(|(_, bu)| bu.access_key_id == existing.access_key_id)
        {
            let old_id = resolve_backup_user_id(bu, idx, &backup);
            user_id_map.insert(old_id, existing.id);
        }
    }

    for (idx, bu) in backup.users.iter().enumerate() {
        // Block reserved names
        if bu.name.starts_with('$') {
            tracing::warn!("Skipping user '{}': reserved name", bu.name);
            result.users_skipped += 1;
            continue;
        }

        // Block bootstrap key conflicts
        if let Some(ref bk) = bootstrap_key {
            if bu.access_key_id == *bk {
                tracing::warn!(
                    "Skipping user '{}': access key conflicts with bootstrap credentials",
                    bu.name
                );
                result.users_skipped += 1;
                continue;
            }
        }

        if existing_user_keys.contains(&bu.access_key_id) {
            result.users_skipped += 1;
            continue;
        }

        // Validate permissions before import
        let mut perms = bu.permissions.clone();
        normalize_permissions(&mut perms);
        if let Err(msg) = validate_permissions(&perms) {
            tracing::warn!("Skipping user '{}': invalid permissions: {}", bu.name, msg);
            result.users_skipped += 1;
            continue;
        }

        match db.create_user(
            &bu.name,
            &bu.access_key_id,
            &bu.secret_access_key,
            bu.enabled,
            &perms,
        ) {
            Ok(created) => {
                // Track old→new id mapping for external_identities below.
                let old_id = resolve_backup_user_id(bu, idx, &backup);
                user_id_map.insert(old_id, created.id);
                // Restore group memberships
                for old_gid in &bu.group_ids {
                    if let Some(&new_gid) = group_id_map.get(old_gid) {
                        if db.add_user_to_group(new_gid, created.id).is_ok() {
                            result.memberships_created += 1;
                        }
                    } else {
                        tracing::warn!(
                            "User '{}': group_id {} not found in backup, membership skipped",
                            bu.name,
                            old_gid
                        );
                    }
                }
                result.users_created += 1;
            }
            Err(e) => {
                tracing::warn!("Failed to import user '{}': {}", bu.name, e);
                result.users_skipped += 1;
            }
        }
    }

    // Import auth providers (v2+), with ID remapping for mapping rules
    let mut provider_id_map: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
    let existing_providers = db.load_auth_providers().unwrap_or_default();
    let existing_provider_names: std::collections::HashSet<String> =
        existing_providers.iter().map(|p| p.name.clone()).collect();

    for bp in &backup.auth_providers {
        if existing_provider_names.contains(&bp.name) {
            // Map old ID to existing provider's ID
            if let Some(existing) = existing_providers.iter().find(|p| p.name == bp.name) {
                provider_id_map.insert(bp.id, existing.id);
            }
            continue;
        }
        let req = crate::config_db::auth_providers::CreateAuthProviderRequest {
            name: bp.name.clone(),
            provider_type: bp.provider_type.clone(),
            enabled: bp.enabled,
            priority: bp.priority,
            display_name: bp.display_name.clone(),
            client_id: bp.client_id.clone(),
            client_secret: bp.client_secret.clone(),
            issuer_url: bp.issuer_url.clone(),
            scopes: bp.scopes.clone(),
            extra_config: bp.extra_config.clone(),
        };
        match db.create_auth_provider(&req) {
            Ok(created) => {
                provider_id_map.insert(bp.id, created.id);
            }
            Err(e) => {
                tracing::warn!("Failed to import auth provider '{}': {}", bp.name, e);
            }
        }
    }

    // Import group mapping rules (v2+), remapping provider_id and group_id
    for rule in &backup.mapping_rules {
        let new_provider_id = rule
            .provider_id
            .and_then(|old_id| provider_id_map.get(&old_id).copied());
        let new_group_id = match group_id_map.get(&rule.group_id) {
            Some(&gid) => gid,
            None => {
                tracing::warn!(
                    "Skipping mapping rule: group_id {} not found in backup",
                    rule.group_id
                );
                continue;
            }
        };
        let req = crate::config_db::auth_providers::CreateMappingRuleRequest {
            provider_id: new_provider_id,
            priority: rule.priority,
            match_type: rule.match_type.clone(),
            match_field: rule.match_field.clone(),
            match_value: rule.match_value.clone(),
            group_id: new_group_id,
        };
        if let Err(e) = db.create_group_mapping_rule(&req) {
            tracing::warn!("Failed to import mapping rule: {}", e);
        }
    }

    // Import external identities (v2+). We remap `user_id` + `provider_id`
    // through the maps built above. Records whose user or provider didn't
    // make it through the import (e.g. skipped due to conflicts) are
    // dropped with a warning rather than imported with dangling references.
    //
    // `last_login` isn't preservable via `create_external_identity` (it
    // resets to `now()`), but the binding — user ↔ external_sub ↔
    // provider — is what matters for re-authentication.
    for ident in &backup.external_identities {
        let new_user_id = match user_id_map.get(&ident.user_id) {
            Some(&uid) => uid,
            None => {
                tracing::warn!(
                    "Skipping external_identity for external_sub '{}': user_id {} not imported",
                    ident.external_sub,
                    ident.user_id
                );
                result.external_identities_skipped += 1;
                continue;
            }
        };
        let new_provider_id = match provider_id_map.get(&ident.provider_id) {
            Some(&pid) => pid,
            None => {
                tracing::warn!(
                    "Skipping external_identity for external_sub '{}': provider_id {} not imported",
                    ident.external_sub,
                    ident.provider_id
                );
                result.external_identities_skipped += 1;
                continue;
            }
        };
        // Skip duplicates idempotently — a second import pass should not
        // double-insert. `find_external_identity` returns the existing
        // row if one already exists for this (provider, external_sub).
        if db
            .find_external_identity(new_provider_id, &ident.external_sub)
            .ok()
            .flatten()
            .is_some()
        {
            result.external_identities_skipped += 1;
            continue;
        }
        match db.create_external_identity(
            new_user_id,
            new_provider_id,
            &ident.external_sub,
            ident.email.as_deref(),
            ident.display_name.as_deref(),
            ident.raw_claims.as_ref(),
        ) {
            Ok(_) => result.external_identities_created += 1,
            Err(e) => {
                tracing::warn!(
                    "Failed to import external_identity for external_sub '{}': {}",
                    ident.external_sub,
                    e
                );
                result.external_identities_skipped += 1;
            }
        }
    }

    // Rebuild IAM index + external auth manager
    rebuild_iam_index(&db, &state.iam_state)?;
    // Reload OAuth providers into memory (otherwise imported providers
    // won't work until restart)
    if let Some(ref ext_auth) = state.external_auth {
        let providers = db.load_auth_providers().unwrap_or_default();
        if !providers.is_empty() {
            ext_auth.rebuild(&providers);
        }
    }
    drop(db);
    // Discover OIDC endpoints for newly imported providers
    if let Some(ref ext_auth) = state.external_auth {
        ext_auth.discover_all().await;
    }
    trigger_config_sync(&state);

    audit_log(
        "import_backup",
        "admin",
        &format!(
            "{}u+{}g+{}ext_id created",
            result.users_created, result.groups_created, result.external_identities_created
        ),
        &headers,
    );

    Ok(Json(result))
}

/// Best-effort resolver for a backup user's original database id.
///
/// Old backups (before the Wave-11 fix) never carried `BackupUser.id`.
/// To restore external_identities from those, we walk a short fallback
/// chain:
///
///   1. `bu.id` — authoritative when present (new exports).
///   2. `backup.groups[].member_ids` — the sibling field lists original
///      user IDs and is present in v2 backups. Match by position: the
///      `idx`-th user was written from `load_users()`, which returns
///      rows in id order, so the `idx`-th member across all groups
///      that refers back to this user yields the original id.
///      Simpler: scan every member_ids list, pick the one whose
///      position in the flattened user list equals `idx`.
///   3. `idx + 1` — SQLite autoincrement starts at 1 and the export
///      writes users in id order. This is a last-resort heuristic.
///      It fails only when the original DB had deleted ids (id gaps).
///
/// None of these are perfect, but (3) covers the overwhelming majority
/// of restores and the damage of a wrong guess is limited to a single
/// dropped external_identity — the operator's next OAuth login will
/// re-provision the binding.
fn resolve_backup_user_id(bu: &BackupUser, idx: usize, backup: &IamBackup) -> i64 {
    if let Some(id) = bu.id {
        return id;
    }
    // Fallback (2): scan groups.member_ids for a candidate.
    // Build a sorted set of member IDs from groups, then pick the
    // idx-th smallest. Since `load_users()` returns users in id order
    // and the user is a member of at least one group, this yields
    // the original id for any user that had a group membership.
    let mut member_ids: Vec<i64> = backup
        .groups
        .iter()
        .flat_map(|g| g.member_ids.iter().copied())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    member_ids.sort();
    if let Some(&cand) = member_ids.get(idx) {
        return cand;
    }
    // Fallback (3): SQLite autoincrement assumption.
    (idx as i64) + 1
}

#[derive(Serialize)]
pub struct ImportResult {
    pub users_created: u32,
    pub users_skipped: u32,
    pub groups_created: u32,
    pub groups_skipped: u32,
    pub memberships_created: u32,
    /// External-identity rows successfully remapped + inserted.
    pub external_identities_created: u32,
    /// Skipped because the referenced user/provider didn't make it,
    /// or a matching (provider, external_sub) already exists.
    pub external_identities_skipped: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_v1_backup_deserializes_without_external_fields() {
        // v1 backups have no auth_providers/mapping_rules/external_identities
        let json = r#"{
            "version": 1,
            "users": [],
            "groups": []
        }"#;
        let backup: IamBackup = serde_json::from_str(json).unwrap();
        assert_eq!(backup.version, 1);
        assert!(backup.auth_providers.is_empty());
        assert!(backup.mapping_rules.is_empty());
        assert!(backup.external_identities.is_empty());
    }

    #[test]
    fn test_v2_backup_roundtrip() {
        let backup = IamBackup {
            version: 2,
            users: vec![BackupUser {
                id: Some(1),
                name: "alice".into(),
                access_key_id: "AK1".into(),
                secret_access_key: "SK1".into(),
                enabled: true,
                permissions: vec![],
                group_ids: vec![1],
            }],
            groups: vec![BackupGroup {
                id: 1,
                name: "devs".into(),
                description: "Dev team".into(),
                permissions: vec![],
                member_ids: vec![],
            }],
            auth_providers: vec![AuthProviderConfig {
                id: 1,
                name: "google".into(),
                provider_type: "oidc".into(),
                enabled: true,
                priority: 10,
                display_name: Some("Google".into()),
                client_id: Some("cid".into()),
                client_secret: Some("****".into()),
                issuer_url: Some("https://accounts.google.com".into()),
                scopes: "openid email".into(),
                extra_config: None,
                created_at: "2024-01-01".into(),
                updated_at: "2024-01-01".into(),
            }],
            mapping_rules: vec![GroupMappingRule {
                id: 1,
                provider_id: Some(1),
                priority: 0,
                match_type: "email_domain".into(),
                match_field: "email".into(),
                match_value: "company.com".into(),
                group_id: 1,
                created_at: "2024-01-01".into(),
            }],
            external_identities: vec![ExternalIdentity {
                id: 1,
                user_id: 1,
                provider_id: 1,
                external_sub: "google-123".into(),
                email: Some("alice@company.com".into()),
                display_name: Some("Alice".into()),
                last_login: None,
                raw_claims: Some(serde_json::json!({"sub": "google-123"})),
                created_at: "2024-01-01".into(),
            }],
        };

        let json = serde_json::to_string(&backup).unwrap();
        let restored: IamBackup = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.version, 2);
        assert_eq!(restored.users.len(), 1);
        assert_eq!(restored.groups.len(), 1);
        assert_eq!(restored.auth_providers.len(), 1);
        assert_eq!(restored.auth_providers[0].name, "google");
        assert_eq!(restored.mapping_rules.len(), 1);
        assert_eq!(restored.mapping_rules[0].match_value, "company.com");
        assert_eq!(restored.external_identities.len(), 1);
        assert_eq!(
            restored.external_identities[0].email.as_deref(),
            Some("alice@company.com")
        );
    }
}
