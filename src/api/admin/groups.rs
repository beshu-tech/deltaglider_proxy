//! Group handlers: list, create, update, delete, add/remove members.

use axum::{extract::State, http::StatusCode, Json};
use serde::Deserialize;
use std::sync::Arc;

use crate::iam::{Group, Permission};

use super::users::rebuild_iam_index;
use super::AdminState;

#[derive(Deserialize)]
pub struct CreateGroupRequest {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub permissions: Vec<Permission>,
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
    Json(body): Json<CreateGroupRequest>,
) -> Result<(StatusCode, Json<Group>), StatusCode> {
    let db = state.config_db.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    let db = db.lock().await;

    let group = db
        .create_group(&body.name, &body.description, &body.permissions)
        .map_err(|e| {
            tracing::warn!("Failed to create group '{}': {}", body.name, e);
            StatusCode::CONFLICT
        })?;

    rebuild_iam_index(&db, &state.iam_state)?;

    tracing::info!("IAM group '{}' created (id={})", group.name, group.id);
    Ok((StatusCode::CREATED, Json(group)))
}

/// PUT /api/admin/groups/:id — update a group.
pub async fn update_group(
    State(state): State<Arc<AdminState>>,
    axum::extract::Path(group_id): axum::extract::Path<i64>,
    Json(body): Json<UpdateGroupRequest>,
) -> Result<Json<Group>, StatusCode> {
    let db = state.config_db.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    let db = db.lock().await;

    let group = db
        .update_group(
            group_id,
            body.name.as_deref(),
            body.description.as_deref(),
            body.permissions.as_deref(),
        )
        .map_err(|e| {
            tracing::warn!("Failed to update group {}: {}", group_id, e);
            StatusCode::NOT_FOUND
        })?;

    rebuild_iam_index(&db, &state.iam_state)?;

    tracing::info!("IAM group '{}' updated", group.name);
    Ok(Json(group))
}

/// DELETE /api/admin/groups/:id — delete a group.
pub async fn delete_group(
    State(state): State<Arc<AdminState>>,
    axum::extract::Path(group_id): axum::extract::Path<i64>,
) -> Result<StatusCode, StatusCode> {
    let db = state.config_db.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    let db = db.lock().await;

    db.delete_group(group_id).map_err(|e| {
        tracing::warn!("Failed to delete group {}: {}", group_id, e);
        StatusCode::NOT_FOUND
    })?;

    rebuild_iam_index(&db, &state.iam_state)?;

    tracing::info!("IAM group {} deleted", group_id);
    Ok(StatusCode::NO_CONTENT)
}

/// POST /api/admin/groups/:id/members — add a user to a group.
pub async fn add_group_member(
    State(state): State<Arc<AdminState>>,
    axum::extract::Path(group_id): axum::extract::Path<i64>,
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

    tracing::info!("User {} added to group {}", body.user_id, group_id);
    Ok(StatusCode::OK)
}

/// DELETE /api/admin/groups/:id/members/:user_id — remove a user from a group.
pub async fn remove_group_member(
    State(state): State<Arc<AdminState>>,
    axum::extract::Path((group_id, user_id)): axum::extract::Path<(i64, i64)>,
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

    tracing::info!("User {} removed from group {}", user_id, group_id);
    Ok(StatusCode::NO_CONTENT)
}
