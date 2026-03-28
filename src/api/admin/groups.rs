//! Group handlers: list, create, update, delete, add/remove members.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;
use std::sync::Arc;

use crate::iam::{normalize_permissions, validate_permissions, Group, Permission};

use super::users::rebuild_iam_index;
use super::{audit_log, trigger_config_sync, AdminState};

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
}

#[derive(Deserialize)]
pub struct AddGroupMemberRequest {
    pub user_id: i64,
}

/// GET /api/admin/groups — list all groups.
pub async fn list_groups(
    State(state): State<Arc<AdminState>>,
) -> Result<Json<Vec<Group>>, StatusCode> {
    let db = match state.config_db.as_ref() {
        Some(db) => db,
        None => return Ok(Json(vec![])),
    };
    let db = db.lock().await;
    let groups = db.load_groups().map_err(|e| {
        tracing::error!("Failed to load groups: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(groups))
}

/// POST /api/admin/groups — create a new group.
pub async fn create_group(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
    Json(body): Json<CreateGroupRequest>,
) -> Result<(StatusCode, Json<Group>), StatusCode> {
    let db = state.config_db.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    let db = db.lock().await;

    let mut perms = body.permissions.clone();
    normalize_permissions(&mut perms);
    if let Err(msg) = validate_permissions(&perms) {
        tracing::warn!("Invalid permissions for group '{}': {}", body.name, msg);
        return Err(StatusCode::BAD_REQUEST);
    }

    let group = db
        .create_group(&body.name, &body.description, &perms)
        .map_err(|e| {
            tracing::warn!("Failed to create group '{}': {}", body.name, e);
            StatusCode::CONFLICT
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
        db.get_group_by_id(group.id).map_err(|e| {
            tracing::error!("Failed to reload group after adding members: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR
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

/// PUT /api/admin/groups/:id — update a group.
pub async fn update_group(
    State(state): State<Arc<AdminState>>,
    axum::extract::Path(group_id): axum::extract::Path<i64>,
    headers: HeaderMap,
    Json(body): Json<UpdateGroupRequest>,
) -> Result<Json<Group>, StatusCode> {
    let db = state.config_db.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    let db = db.lock().await;

    let normalized_perms = body.permissions.as_ref().map(|p| {
        let mut perms = p.clone();
        normalize_permissions(&mut perms);
        perms
    });
    if let Some(ref perms) = normalized_perms {
        if let Err(msg) = validate_permissions(perms) {
            tracing::warn!("Invalid permissions for group {}: {}", group_id, msg);
            return Err(StatusCode::BAD_REQUEST);
        }
    }

    let group = db
        .update_group(
            group_id,
            body.name.as_deref(),
            body.description.as_deref(),
            normalized_perms.as_deref(),
        )
        .map_err(|e| {
            tracing::warn!("Failed to update group {}: {}", group_id, e);
            StatusCode::NOT_FOUND
        })?;

    rebuild_iam_index(&db, &state.iam_state)?;
    trigger_config_sync(&state);

    tracing::info!("IAM group '{}' updated", group.name);
    audit_log("update_group", "admin", &group.name, &headers);
    Ok(Json(group))
}

/// DELETE /api/admin/groups/:id — delete a group.
pub async fn delete_group(
    State(state): State<Arc<AdminState>>,
    axum::extract::Path(group_id): axum::extract::Path<i64>,
    headers: HeaderMap,
) -> Result<StatusCode, StatusCode> {
    let db = state.config_db.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    let db = db.lock().await;

    db.delete_group(group_id).map_err(|e| {
        tracing::warn!("Failed to delete group {}: {}", group_id, e);
        StatusCode::NOT_FOUND
    })?;

    rebuild_iam_index(&db, &state.iam_state)?;
    trigger_config_sync(&state);

    tracing::info!("IAM group {} deleted", group_id);
    audit_log("delete_group", "admin", &group_id.to_string(), &headers);
    Ok(StatusCode::NO_CONTENT)
}

/// POST /api/admin/groups/:id/members — add a user to a group.
pub async fn add_group_member(
    State(state): State<Arc<AdminState>>,
    axum::extract::Path(group_id): axum::extract::Path<i64>,
    headers: HeaderMap,
    Json(body): Json<AddGroupMemberRequest>,
) -> Result<StatusCode, StatusCode> {
    let db = state.config_db.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    let db = db.lock().await;

    db.add_user_to_group(group_id, body.user_id).map_err(|e| {
        tracing::warn!(
            "Failed to add user {} to group {}: {}",
            body.user_id,
            group_id,
            e
        );
        StatusCode::BAD_REQUEST
    })?;

    rebuild_iam_index(&db, &state.iam_state)?;
    trigger_config_sync(&state);

    tracing::info!("User {} added to group {}", body.user_id, group_id);
    audit_log(
        "add_member",
        "admin",
        &format!("group:{}+user:{}", group_id, body.user_id),
        &headers,
    );
    Ok(StatusCode::OK)
}

/// DELETE /api/admin/groups/:id/members/:user_id — remove a user from a group.
pub async fn remove_group_member(
    State(state): State<Arc<AdminState>>,
    axum::extract::Path((group_id, user_id)): axum::extract::Path<(i64, i64)>,
    headers: HeaderMap,
) -> Result<StatusCode, StatusCode> {
    let db = state.config_db.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    let db = db.lock().await;

    db.remove_user_from_group(group_id, user_id).map_err(|e| {
        tracing::warn!(
            "Failed to remove user {} from group {}: {}",
            user_id,
            group_id,
            e
        );
        StatusCode::BAD_REQUEST
    })?;

    rebuild_iam_index(&db, &state.iam_state)?;
    trigger_config_sync(&state);

    tracing::info!("User {} removed from group {}", user_id, group_id);
    audit_log(
        "remove_member",
        "admin",
        &format!("group:{}+user:{}", group_id, user_id),
        &headers,
    );
    Ok(StatusCode::NO_CONTENT)
}
