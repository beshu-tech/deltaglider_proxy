//! IAM data export/import (backup & restore).

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::iam::Permission;

use super::users::rebuild_iam_index;
use super::{audit_log, trigger_config_sync, AdminState};

/// Full IAM backup: users (with credentials) + groups + memberships.
#[derive(Serialize, Deserialize)]
pub struct IamBackup {
    pub version: u32,
    pub users: Vec<BackupUser>,
    pub groups: Vec<BackupGroup>,
}

#[derive(Serialize, Deserialize)]
pub struct BackupUser {
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

    let backup = IamBackup {
        version: 1,
        users: users
            .into_iter()
            .map(|u| BackupUser {
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

    let mut result = ImportResult {
        users_created: 0,
        users_skipped: 0,
        groups_created: 0,
        groups_skipped: 0,
        memberships_created: 0,
    };

    // Import groups first (users reference group_ids)
    let mut group_id_map: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();

    for bg in &backup.groups {
        // Check if group already exists by name
        let existing = db.load_groups().unwrap_or_default();
        if existing.iter().any(|g| g.name == bg.name) {
            // Map old ID to existing ID
            if let Some(existing_group) = existing.iter().find(|g| g.name == bg.name) {
                group_id_map.insert(bg.id, existing_group.id);
            }
            result.groups_skipped += 1;
            continue;
        }

        match db.create_group(&bg.name, &bg.description, &bg.permissions) {
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

    // Import users
    for bu in &backup.users {
        // Check if user already exists by access_key_id
        let existing = db.load_users().unwrap_or_default();
        if existing.iter().any(|u| u.access_key_id == bu.access_key_id) {
            result.users_skipped += 1;
            continue;
        }

        match db.create_user(
            &bu.name,
            &bu.access_key_id,
            &bu.secret_access_key,
            bu.enabled,
            &bu.permissions,
        ) {
            Ok(created) => {
                // Restore group memberships
                for old_gid in &bu.group_ids {
                    if let Some(&new_gid) = group_id_map.get(old_gid) {
                        if db.add_user_to_group(new_gid, created.id).is_ok() {
                            result.memberships_created += 1;
                        }
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

    // Rebuild IAM index
    rebuild_iam_index(&db, &state.iam_state)?;
    trigger_config_sync(&state);

    audit_log(
        "import_backup",
        "admin",
        &format!(
            "{}u+{}g created",
            result.users_created, result.groups_created
        ),
        &headers,
    );

    Ok(Json(result))
}

#[derive(Serialize)]
pub struct ImportResult {
    pub users_created: u32,
    pub users_skipped: u32,
    pub groups_created: u32,
    pub groups_skipped: u32,
    pub memberships_created: u32,
}
