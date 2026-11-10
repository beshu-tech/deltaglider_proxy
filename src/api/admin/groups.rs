// SPDX-License-Identifier: BUSL-1.1

//! Group handlers: list, create, update, delete, add/remove members.

use crate::api::admin::extract::AdminJson;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;
use std::sync::Arc;

use crate::iam::{normalize_permissions, validate_permissions, Group, Permission};

use super::users::{check_group_name, group_name_error, next_iam_copy_name, rebuild_iam_index};
use super::{audit_log, trigger_config_sync, AdminError, AdminState};

#[derive(Deserialize)]
pub struct CreateGroupRequest {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub permissions: Vec<Permission>,
    /// Optional list of user IDs to add as members during creation.
    #[serde(default)]
    pub member_ids: Vec<i64>,
}

#[derive(Deserialize)]
pub struct UpdateGroupRequest {
    pub name: Option<String>,
    pub description: Option<String>,
    pub permissions: Option<Vec<Permission>>,
    /// Users to add to the group, and users to remove from it. A change,
    /// never the whole list: other writers (another admin, a peer node
    /// through the config sync, every OIDC login) change members too, and a
    /// whole list would undo their changes. They apply in the same
    /// transaction as the rest of the edit: one edit is one request, and a
    /// refusal of any part (a user that does not exist, a name another group
    /// has) applies none. A user in both lists is a 400.
    #[serde(default)]
    pub add_member_ids: Vec<i64>,
    #[serde(default)]
    pub remove_member_ids: Vec<i64>,
}

/// Pure: the members to add and to remove for an edit, judged against the
/// group's live members (`current`): a user that is a member already is not
/// added, a user that is not a member is not removed. Each list is sorted
/// and without duplicates. `Err` names a user that is in both lists.
pub(crate) fn member_changes(
    current: &[i64],
    add: &[i64],
    remove: &[i64],
) -> Result<(Vec<i64>, Vec<i64>), i64> {
    use std::collections::BTreeSet;
    let current: BTreeSet<i64> = current.iter().copied().collect();
    let add: BTreeSet<i64> = add.iter().copied().collect();
    let remove: BTreeSet<i64> = remove.iter().copied().collect();
    if let Some(both) = add.intersection(&remove).next() {
        return Err(*both);
    }
    Ok((
        add.difference(&current).copied().collect(),
        remove.intersection(&current).copied().collect(),
    ))
}

#[derive(Deserialize)]
pub struct CloneGroupRequest {
    pub name: Option<String>,
    #[serde(default)]
    pub copy_members: bool,
}

#[derive(Deserialize)]
pub struct AddGroupMemberRequest {
    pub user_id: i64,
}

/// GET /api/admin/groups — list all groups.
pub async fn list_groups(
    State(state): State<Arc<AdminState>>,
) -> Result<Json<Vec<Group>>, AdminError> {
    let db = match state.config_db.as_ref() {
        Some(db) => db,
        None => return Ok(Json(vec![])),
    };
    let db = db.lock().await;
    let groups = db.load_groups().inspect_err(|e| {
        tracing::error!("Failed to load groups: {}", e);
    })?;
    Ok(Json(groups))
}

/// POST /api/admin/groups — create a new group.
pub async fn create_group(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
    AdminJson(body): AdminJson<CreateGroupRequest>,
) -> Result<(StatusCode, Json<Group>), AdminError> {
    let db = state
        .config_db
        .as_ref()
        .ok_or_else(AdminError::no_config_db)?;
    let db = db.lock().await;

    check_group_name(&body.name)?;
    let mut perms = body.permissions.clone();
    normalize_permissions(&mut perms);
    if let Err(msg) = validate_permissions(&perms) {
        tracing::warn!("Invalid permissions for group '{}': {}", body.name, msg);
        // The text names the caller's own rule, so the operator can fix it.
        return Err(AdminError::invalid(format!("invalid permissions: {msg}")));
    }

    let group = db
        .create_group(&body.name, &body.description, &perms)
        .inspect_err(|e| {
            tracing::warn!("Failed to create group '{}': {}", body.name, e);
        })?;

    // Add members if provided in the creation request.
    // INSERT OR IGNORE silently skips non-existent user IDs — the reloaded
    // group response will only contain successfully added members.
    let mut failed_ids = Vec::new();
    for user_id in &body.member_ids {
        if let Err(e) = db.add_user_to_group(group.id, *user_id) {
            tracing::warn!(
                "Failed to add user {} to group '{}': {}",
                user_id,
                group.name,
                e
            );
            failed_ids.push(*user_id);
        }
    }
    if !failed_ids.is_empty() {
        tracing::warn!(
            "Group '{}': {} of {} member_ids could not be added: {:?}",
            group.name,
            failed_ids.len(),
            body.member_ids.len(),
            failed_ids
        );
    }

    // Reload group to include member_ids in the response
    let group = if !body.member_ids.is_empty() {
        db.get_group_by_id(group.id).inspect_err(|e| {
            tracing::error!("Failed to reload group after adding members: {}", e);
        })?
    } else {
        group
    };

    rebuild_iam_index(&db, &state.iam_state)?;
    trigger_config_sync(&state);

    tracing::info!("IAM group '{}' created (id={})", group.name, group.id);
    audit_log("create_group", "admin", &group.name, &headers);
    Ok((StatusCode::CREATED, Json(group)))
}

/// POST /api/admin/groups/:id/clone — duplicate a group.
pub async fn clone_group(
    State(state): State<Arc<AdminState>>,
    axum::extract::Path(group_id): axum::extract::Path<i64>,
    headers: HeaderMap,
    body: Option<Json<CloneGroupRequest>>,
) -> Result<(StatusCode, Json<Group>), AdminError> {
    let db = state
        .config_db
        .as_ref()
        .ok_or_else(AdminError::no_config_db)?;
    let db = db.lock().await;
    let body = body.map(|Json(body)| body);
    let source = db.get_group_by_id(group_id).inspect_err(|e| {
        tracing::warn!("Failed to load source group {} for clone: {}", group_id, e);
    })?;

    let name = body
        .as_ref()
        .and_then(|b| b.name.as_ref())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            // Only the existing names matter for collision-avoidance, so use the
            // lightweight name-only query rather than load_groups()'s per-group
            // permission/member fan-out.
            let names = db.load_group_names().unwrap_or_default().into_iter();
            next_iam_copy_name("group", &source.name, names)
        });
    check_group_name(&name)?;
    let copy_members = body.map(|b| b.copy_members).unwrap_or(false);

    let group = db
        .clone_group(group_id, &name, copy_members)
        .inspect_err(|e| {
            tracing::warn!("Failed to clone group {} as '{}': {}", group_id, name, e);
        })?;

    rebuild_iam_index(&db, &state.iam_state)?;
    trigger_config_sync(&state);

    tracing::info!("IAM group '{}' cloned to '{}'", source.name, group.name);
    audit_log(
        "clone_group",
        "admin",
        &format!("{} -> {}", source.name, group.name),
        &headers,
    );
    Ok((StatusCode::CREATED, Json(group)))
}

/// PUT /api/admin/groups/:id — update a group.
pub async fn update_group(
    State(state): State<Arc<AdminState>>,
    axum::extract::Path(group_id): axum::extract::Path<i64>,
    headers: HeaderMap,
    AdminJson(body): AdminJson<UpdateGroupRequest>,
) -> Result<Json<Group>, AdminError> {
    let db = state
        .config_db
        .as_ref()
        .ok_or_else(AdminError::no_config_db)?;
    let db = db.lock().await;

    // Refuse a RENAME to a name outside the rule. A row that already carries
    // one (created before names were checked, or by declarative YAML) stays
    // editable: the GUI always sends the current name back.
    if let Some(name) = body.name.as_deref() {
        if group_name_error(name).is_some() {
            let current = db.get_group_by_id(group_id)?;
            if current.name != name {
                check_group_name(name)?;
            }
        }
    }
    let normalized_perms = body.permissions.as_ref().map(|p| {
        let mut perms = p.clone();
        normalize_permissions(&mut perms);
        perms
    });
    if let Some(ref perms) = normalized_perms {
        if let Err(msg) = validate_permissions(perms) {
            tracing::warn!("Invalid permissions for group {}: {}", group_id, msg);
            // The text names the caller's own rule, so the operator can fix it.
            return Err(AdminError::invalid(format!("invalid permissions: {msg}")));
        }
    }

    // The membership part of the edit, decided BEFORE the first write.
    let changes = if body.add_member_ids.is_empty() && body.remove_member_ids.is_empty() {
        None
    } else {
        Some(
            member_changes(
                &db.get_group_members(group_id)?,
                &body.add_member_ids,
                &body.remove_member_ids,
            )
            .map_err(|user| {
                AdminError::invalid(format!(
                    "user {user} is in both add_member_ids and remove_member_ids"
                ))
            })?,
        )
    };

    let group = db
        .update_group_with_members(
            group_id,
            body.name.as_deref(),
            body.description.as_deref(),
            normalized_perms.as_deref(),
            changes
                .as_ref()
                .map(|(add, remove)| (add.as_slice(), remove.as_slice())),
        )
        .inspect_err(|e| {
            tracing::warn!("Failed to update group {}: {}", group_id, e);
            // A rename to a name another group has → 409; a member that does
            // not exist → 404; nothing was applied.
        })?;

    // The audit lines of the member changes (the same two the per-member
    // endpoints write), built once under the lock: ONE user load, ONE group
    // lookup. They are written after the rebuild and the sync trigger, like
    // every other IAM write: a failed rebuild answers 500 and audits nothing.
    let member_audit = changes
        .as_ref()
        .map(|(add, remove)| member_audit_lines(&db, group_id, add, remove))
        .unwrap_or_default();

    rebuild_iam_index(&db, &state.iam_state)?;
    trigger_config_sync(&state);
    drop(db);

    for (action, target) in &member_audit {
        audit_log(action, "admin", target, &headers);
    }
    tracing::info!("IAM group '{}' updated", group.name);
    audit_log("update_group", "admin", &group.name, &headers);
    Ok(Json(group))
}

/// DELETE /api/admin/groups/:id — delete a group.
pub async fn delete_group(
    State(state): State<Arc<AdminState>>,
    axum::extract::Path(group_id): axum::extract::Path<i64>,
    headers: HeaderMap,
) -> Result<StatusCode, AdminError> {
    let db = state
        .config_db
        .as_ref()
        .ok_or_else(AdminError::no_config_db)?;
    let db = db.lock().await;
    let target = group_target(&db, group_id);

    db.delete_group(group_id).inspect_err(|e| {
        tracing::warn!("Failed to delete group {}: {}", group_id, e);
    })?;

    rebuild_iam_index(&db, &state.iam_state)?;
    trigger_config_sync(&state);

    tracing::info!("IAM group {} deleted", group_id);
    audit_log("delete_group", "admin", &target, &headers);
    Ok(StatusCode::NO_CONTENT)
}

/// POST /api/admin/groups/:id/members — add a user to a group.
pub async fn add_group_member(
    State(state): State<Arc<AdminState>>,
    axum::extract::Path(group_id): axum::extract::Path<i64>,
    headers: HeaderMap,
    AdminJson(body): AdminJson<AddGroupMemberRequest>,
) -> Result<StatusCode, AdminError> {
    let db = state
        .config_db
        .as_ref()
        .ok_or_else(AdminError::no_config_db)?;
    let db = db.lock().await;

    db.add_user_to_group(group_id, body.user_id)
        .inspect_err(|e| {
            tracing::warn!(
                "Failed to add user {} to group {}: {}",
                body.user_id,
                group_id,
                e
            );
        })?;
    let target = format!(
        "user {} to group {}",
        user_target(&db, body.user_id),
        group_target(&db, group_id)
    );

    rebuild_iam_index(&db, &state.iam_state)?;
    trigger_config_sync(&state);

    tracing::info!("User {} added to group {}", body.user_id, group_id);
    audit_log("add_member", "admin", &target, &headers);
    Ok(StatusCode::OK)
}

/// DELETE /api/admin/groups/:id/members/:user_id — remove a user from a group.
pub async fn remove_group_member(
    State(state): State<Arc<AdminState>>,
    axum::extract::Path((group_id, user_id)): axum::extract::Path<(i64, i64)>,
    headers: HeaderMap,
) -> Result<StatusCode, AdminError> {
    let db = state
        .config_db
        .as_ref()
        .ok_or_else(AdminError::no_config_db)?;
    let db = db.lock().await;

    db.remove_user_from_group(group_id, user_id)
        .inspect_err(|e| {
            tracing::warn!(
                "Failed to remove user {} from group {}: {}",
                user_id,
                group_id,
                e
            );
        })?;
    let target = format!(
        "user {} from group {}",
        user_target(&db, user_id),
        group_target(&db, group_id)
    );

    rebuild_iam_index(&db, &state.iam_state)?;
    trigger_config_sync(&state);

    tracing::info!("User {} removed from group {}", user_id, group_id);
    audit_log("remove_member", "admin", &target, &headers);
    Ok(StatusCode::NO_CONTENT)
}

/// Audit label of a group: `name (id N)`.
pub(crate) fn group_target(db: &crate::config_db::ConfigDb, id: i64) -> String {
    super::named_target(db.get_group_by_id(id).ok().map(|g| g.name).as_deref(), id)
}

/// Audit label of a user: `name (id N)`.
fn user_target(db: &crate::config_db::ConfigDb, id: i64) -> String {
    let name = db
        .load_users()
        .ok()
        .and_then(|us| us.into_iter().find(|u| u.id == id))
        .map(|u| u.name);
    super::named_target(name.as_deref(), id)
}

/// The `(action, target)` audit lines of an edit's member changes, with one
/// user load and one group lookup for all of them.
fn member_audit_lines(
    db: &crate::config_db::ConfigDb,
    group_id: i64,
    add: &[i64],
    remove: &[i64],
) -> Vec<(&'static str, String)> {
    let names: std::collections::HashMap<i64, String> = db
        .load_users()
        .map(|users| users.into_iter().map(|u| (u.id, u.name)).collect())
        .unwrap_or_default();
    let group = group_target(db, group_id);
    let mut lines = Vec::with_capacity(add.len() + remove.len());
    for (action, ids, direction) in [("add_member", add, "to"), ("remove_member", remove, "from")] {
        for &id in ids {
            let user = super::named_target(names.get(&id).map(String::as_str), id);
            lines.push((action, format!("user {user} {direction} group {group}")));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::member_changes;

    #[test]
    fn member_changes_judge_the_edit_against_the_live_members() {
        // 1 is a member and not added again; 9 is no member and not removed.
        assert_eq!(
            member_changes(&[1, 2], &[1, 4], &[2, 9]),
            Ok((vec![4], vec![2]))
        );
        assert_eq!(
            member_changes(&[], &[5, 5, 3], &[]),
            Ok((vec![3, 5], vec![]))
        );
        assert_eq!(
            member_changes(&[1, 2], &[], &[2, 1]),
            Ok((vec![], vec![1, 2]))
        );
        // Members that nobody names stay as they are.
        assert_eq!(member_changes(&[1, 2, 3], &[], &[]), Ok((vec![], vec![])));
        // A user in both lists is refused.
        assert_eq!(member_changes(&[1], &[2], &[2]), Err(2));
    }
}
