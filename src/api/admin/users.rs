//! User handlers: list, create, update, delete, rotate keys, canned policies,
//! plus rebuild_iam_index and mask_user helpers.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use std::sync::Arc;

use crate::config_db::ConfigDb;
use crate::iam::{
    self, normalize_permissions, validate_permissions, IamIndex, IamState, IamUser, Permission,
    SharedIamState,
};

use super::{audit_log, trigger_config_sync, AdminState};

#[derive(Deserialize)]
pub struct CreateUserRequest {
    pub name: String,
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
    #[serde(default = "crate::types::default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub permissions: Vec<Permission>,
}

#[derive(Deserialize)]
pub struct UpdateUserRequest {
    pub name: Option<String>,
    pub enabled: Option<bool>,
    pub permissions: Option<Vec<Permission>>,
}

#[derive(Deserialize)]
pub struct RotateKeysRequest {
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
}

/// Mask the secret_access_key for API responses (shown only on create/rotate).
fn mask_user(user: &IamUser) -> IamUser {
    IamUser {
        secret_access_key: "****".to_string(),
        ..user.clone()
    }
}

/// Rebuild the in-memory IamIndex from the database and store it.
/// If no users exist, restores Disabled mode to avoid locking out all access.
/// On first IAM user creation (Legacy -> IAM transition), auto-migrates the
/// legacy TOML credentials as a "legacy-admin" user with full access so
/// existing S3 clients don't break.
pub(super) fn rebuild_iam_index(
    db: &ConfigDb,
    iam_state: &SharedIamState,
) -> Result<(), StatusCode> {
    let mut users = db.load_users().map_err(|e| {
        tracing::error!("Failed to load users from config DB: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    if users.is_empty() {
        tracing::info!("No IAM users in database — disabling auth (open access)");
        iam_state.store(Arc::new(IamState::Disabled));
        return Ok(());
    }

    // Migrate legacy credentials on first IAM user creation so existing
    // S3 clients continue working after the switch to IAM mode.
    let current = iam_state.load();
    if let IamState::Legacy(ref legacy) = **current {
        let already_migrated = users
            .iter()
            .any(|u| u.access_key_id == legacy.access_key_id);
        if !already_migrated {
            let admin_perms = vec![Permission {
                id: 0,
                effect: "Allow".into(),
                actions: vec!["*".into()],
                resources: vec!["*".into()],
                conditions: None,
            }];
            match db.create_user(
                "legacy-admin",
                &legacy.access_key_id,
                &legacy.secret_access_key,
                true,
                &admin_perms,
            ) {
                Ok(migrated) => {
                    tracing::info!(
                        "Migrated legacy credentials to IAM user 'legacy-admin' ({})",
                        migrated.access_key_id
                    );
                    users.push(migrated);
                }
                Err(e) => {
                    tracing::error!("Failed to migrate legacy credentials: {}", e);
                }
            }
        }
    }

    let groups = db.load_groups().map_err(|e| {
        tracing::error!("Failed to load groups from config DB: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let count = users.len();
    let group_count = groups.len();
    let state = IamIndex::build_iam_state(users, groups);
    tracing::debug!(
        "IAM index rebuilt with {} users and {} groups",
        count,
        group_count
    );
    iam_state.store(Arc::new(state));
    Ok(())
}

/// GET /api/admin/policies — return predefined policy templates.
pub async fn get_canned_policies() -> impl IntoResponse {
    Json(iam::canned_policies())
}

/// GET /api/admin/users — list all users (secrets masked).
/// Returns empty list if IAM DB is not initialized (legacy/open mode).
pub async fn list_users(
    State(state): State<Arc<AdminState>>,
) -> Result<Json<Vec<IamUser>>, StatusCode> {
    let db = match state.config_db.as_ref() {
        Some(db) => db,
        None => return Ok(Json(vec![])), // No IAM DB -> empty list (not an error)
    };
    let db = db.lock().await;
    let users = db.load_users().map_err(|e| {
        tracing::error!("Failed to load users: {}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(Json(users.iter().map(mask_user).collect()))
}

/// POST /api/admin/users — create a new user (returns full secret once).
pub async fn create_user(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
    Json(body): Json<CreateUserRequest>,
) -> Result<(StatusCode, Json<IamUser>), StatusCode> {
    let db = state.config_db.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    let db = db.lock().await;

    let access_key_id = body
        .access_key_id
        .unwrap_or_else(iam::generate_access_key_id);
    let secret_access_key = body
        .secret_access_key
        .unwrap_or_else(iam::generate_secret_access_key);

    // Validate access key format: must be non-empty, ASCII, no whitespace
    if access_key_id.is_empty()
        || !access_key_id.is_ascii()
        || access_key_id.contains(char::is_whitespace)
    {
        tracing::warn!("Invalid access key format: {:?}", access_key_id);
        return Err(StatusCode::BAD_REQUEST);
    }

    // Block reserved names
    if body.name.starts_with('$') {
        tracing::warn!("User name cannot start with '$': {:?}", body.name);
        return Err(StatusCode::BAD_REQUEST);
    }

    let mut perms = body.permissions.clone();
    normalize_permissions(&mut perms);
    if let Err(msg) = validate_permissions(&perms) {
        tracing::warn!("Invalid permissions for user '{}': {}", body.name, msg);
        return Err(StatusCode::BAD_REQUEST);
    }

    let user = db
        .create_user(
            &body.name,
            &access_key_id,
            &secret_access_key,
            body.enabled,
            &perms,
        )
        .map_err(|e| {
            tracing::warn!("Failed to create user '{}': {}", body.name, e);
            StatusCode::CONFLICT
        })?;

    rebuild_iam_index(&db, &state.iam_state)?;
    trigger_config_sync(&state);

    tracing::info!("IAM user '{}' created ({})", user.name, user.access_key_id);
    audit_log("create_user", "admin", &user.name, &headers);
    // Return full user including secret (shown only once)
    Ok((StatusCode::CREATED, Json(user)))
}

/// PUT /api/admin/users/:id — update a user.
pub async fn update_user(
    State(state): State<Arc<AdminState>>,
    axum::extract::Path(user_id): axum::extract::Path<i64>,
    headers: HeaderMap,
    Json(body): Json<UpdateUserRequest>,
) -> Result<Json<IamUser>, StatusCode> {
    let db = state.config_db.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    let db = db.lock().await;

    let normalized_perms = body.permissions.as_ref().map(|p| {
        let mut perms = p.clone();
        normalize_permissions(&mut perms);
        perms
    });
    if let Some(ref perms) = normalized_perms {
        if let Err(msg) = validate_permissions(perms) {
            tracing::warn!("Invalid permissions for user {}: {}", user_id, msg);
            return Err(StatusCode::BAD_REQUEST);
        }
    }

    let user = db
        .update_user(
            user_id,
            body.name.as_deref(),
            body.enabled,
            normalized_perms.as_deref(),
        )
        .map_err(|e| {
            tracing::warn!("Failed to update user {}: {}", user_id, e);
            StatusCode::NOT_FOUND
        })?;

    rebuild_iam_index(&db, &state.iam_state)?;
    trigger_config_sync(&state);

    tracing::info!("IAM user '{}' updated", user.name);
    audit_log("update_user", "admin", &user.name, &headers);
    Ok(Json(mask_user(&user)))
}

/// DELETE /api/admin/users/:id — delete a user.
pub async fn delete_user(
    State(state): State<Arc<AdminState>>,
    axum::extract::Path(user_id): axum::extract::Path<i64>,
    headers: HeaderMap,
) -> Result<StatusCode, StatusCode> {
    let db = state.config_db.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    let db = db.lock().await;

    db.delete_user(user_id).map_err(|e| {
        tracing::warn!("Failed to delete user {}: {}", user_id, e);
        StatusCode::NOT_FOUND
    })?;

    // Check if this was the last user before rebuilding
    let remaining = db.load_users().map(|u| u.len()).unwrap_or(0);
    if remaining == 0 {
        tracing::warn!("Last IAM user deleted — switching to open access (no authentication)");
    }

    rebuild_iam_index(&db, &state.iam_state)?;
    trigger_config_sync(&state);

    tracing::info!(
        "IAM user {} deleted ({} users remaining)",
        user_id,
        remaining
    );
    audit_log("delete_user", "admin", &user_id.to_string(), &headers);
    Ok(StatusCode::NO_CONTENT)
}

/// POST /api/admin/users/:id/rotate-keys — set or regenerate access keys.
/// If access_key_id or secret_access_key are provided, uses those values.
/// Otherwise auto-generates new ones.
pub async fn rotate_user_keys(
    State(state): State<Arc<AdminState>>,
    axum::extract::Path(user_id): axum::extract::Path<i64>,
    headers: HeaderMap,
    body: Option<Json<RotateKeysRequest>>,
) -> Result<Json<IamUser>, StatusCode> {
    let db = state.config_db.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    let db = db.lock().await;

    let (new_access_key, new_secret_key) = match body {
        Some(Json(req)) => (
            req.access_key_id
                .unwrap_or_else(iam::generate_access_key_id),
            req.secret_access_key
                .unwrap_or_else(iam::generate_secret_access_key),
        ),
        None => (
            iam::generate_access_key_id(),
            iam::generate_secret_access_key(),
        ),
    };

    let user = db
        .rotate_keys(user_id, &new_access_key, &new_secret_key)
        .map_err(|e| {
            tracing::warn!("Failed to rotate keys for user {}: {}", user_id, e);
            StatusCode::NOT_FOUND
        })?;

    rebuild_iam_index(&db, &state.iam_state)?;
    trigger_config_sync(&state);

    tracing::info!(
        "IAM user '{}' keys rotated (new: {})",
        user.name,
        user.access_key_id
    );
    audit_log("rotate_keys", "admin", &user.name, &headers);
    // Return full user including new secret (shown only once)
    Ok(Json(user))
}
