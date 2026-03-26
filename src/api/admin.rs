//! Admin GUI API handlers (separate from S3 SigV4 auth).

use axum::{
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
    Json,
};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

use crate::api::handlers::AppState;
use crate::config::SharedConfig;
use crate::config_db::ConfigDb;
use crate::deltaglider::DynEngine;
use crate::iam::{self, AuthConfig, IamIndex, IamState, IamUser, Permission, SharedIamState};
use crate::session::SessionStore;

/// Type alias for the tracing reload handle.
pub type LogReloadHandle =
    tracing_subscriber::reload::Handle<EnvFilter, tracing_subscriber::Registry>;

/// Shared state for admin API routes.
pub struct AdminState {
    pub password_hash: RwLock<String>,
    pub sessions: Arc<SessionStore>,
    pub config: SharedConfig,
    pub log_reload: LogReloadHandle,
    pub s3_state: Arc<AppState>,
    pub iam_state: SharedIamState,
    /// Encrypted config database for IAM users (None in legacy/open-access mode).
    pub config_db: Option<tokio::sync::Mutex<ConfigDb>>,
}

#[derive(Deserialize)]
pub struct LoginRequest {
    password: String,
}

#[derive(Serialize)]
pub struct LoginResponse {
    ok: bool,
}

#[derive(Serialize)]
pub struct SessionResponse {
    valid: bool,
}

#[derive(Serialize)]
pub struct ConfigResponse {
    listen_addr: String,
    backend_type: String,
    // Backend details
    backend_path: Option<String>,
    backend_endpoint: Option<String>,
    backend_region: Option<String>,
    backend_force_path_style: Option<bool>,
    // Tuning
    max_delta_ratio: f32,
    max_object_size: u64,
    cache_size_mb: usize,
    auth_enabled: bool,
    access_key_id: Option<String>,
    // Log level
    log_level: String,
    // Backend credentials indicator
    backend_has_credentials: bool,
}

#[derive(Deserialize)]
pub struct ConfigUpdateRequest {
    pub max_delta_ratio: Option<f32>,
    pub max_object_size: Option<u64>,
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
    // Restart-required fields
    pub listen_addr: Option<String>,
    pub cache_size_mb: Option<usize>,
    // Log level (hot-reloadable)
    pub log_level: Option<String>,
    // Backend configuration (triggers engine swap)
    pub backend_type: Option<String>,
    pub backend_endpoint: Option<String>,
    pub backend_region: Option<String>,
    pub backend_path: Option<String>,
    pub backend_force_path_style: Option<bool>,
    // Backend S3 credentials (triggers engine swap)
    pub backend_access_key_id: Option<String>,
    pub backend_secret_access_key: Option<String>,
}

#[derive(Serialize)]
pub struct ConfigUpdateResponse {
    success: bool,
    warnings: Vec<String>,
    requires_restart: bool,
}

#[derive(Deserialize)]
pub struct PasswordChangeRequest {
    current_password: String,
    new_password: String,
}

#[derive(Serialize)]
pub struct PasswordChangeResponse {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// POST /api/admin/login — verify password, set session cookie.
pub async fn login(
    State(state): State<Arc<AdminState>>,
    Json(body): Json<LoginRequest>,
) -> impl IntoResponse {
    let hash = state.password_hash.read().clone();
    let valid = match bcrypt::verify(&body.password, &hash) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("bcrypt verify failed (corrupted hash?): {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                HeaderMap::new(),
                Json(LoginResponse { ok: false }),
            )
                .into_response();
        }
    };

    if !valid {
        return (
            StatusCode::UNAUTHORIZED,
            HeaderMap::new(),
            Json(LoginResponse { ok: false }),
        )
            .into_response();
    }

    let token = state.sessions.create_session();

    let cookie = format!(
        "dgp_session={}; HttpOnly; SameSite=Strict; Path=/; Max-Age=86400",
        token
    );

    let mut headers = HeaderMap::new();
    headers.insert(header::SET_COOKIE, cookie.parse().unwrap());

    (StatusCode::OK, headers, Json(LoginResponse { ok: true })).into_response()
}

/// POST /api/admin/logout — clear session.
pub async fn logout(State(state): State<Arc<AdminState>>, headers: HeaderMap) -> impl IntoResponse {
    if let Some(token) = extract_session_token(&headers) {
        state.sessions.remove(&token);
    }

    let cookie = "dgp_session=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0";
    let mut resp_headers = HeaderMap::new();
    resp_headers.insert(header::SET_COOKIE, cookie.parse().unwrap());

    (
        StatusCode::OK,
        resp_headers,
        Json(LoginResponse { ok: true }),
    )
}

/// GET /api/admin/session — check if current session is valid.
pub async fn check_session(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let valid = extract_session_token(&headers)
        .map(|t| state.sessions.validate(&t))
        .unwrap_or(false);

    Json(SessionResponse { valid })
}

/// GET /api/admin/config — return sanitized config (no secrets).
pub async fn get_config(State(state): State<Arc<AdminState>>) -> impl IntoResponse {
    let cfg = state.config.read().await;

    let (
        backend_type,
        backend_path,
        backend_endpoint,
        backend_region,
        backend_force_path_style,
        backend_has_credentials,
    ) = match &cfg.backend {
        crate::config::BackendConfig::Filesystem { path } => (
            "filesystem",
            Some(path.display().to_string()),
            None,
            None,
            None,
            false,
        ),
        crate::config::BackendConfig::S3 {
            endpoint,
            region,
            force_path_style,
            access_key_id,
            ..
        } => (
            "s3",
            None,
            endpoint.clone(),
            Some(region.clone()),
            Some(*force_path_style),
            access_key_id.is_some(),
        ),
    };

    // Read the current log filter from the reload handle
    let log_level = state
        .log_reload
        .with_current(|f| f.to_string())
        .unwrap_or_else(|_| cfg.log_level.clone());

    Json(ConfigResponse {
        listen_addr: cfg.listen_addr.to_string(),
        backend_type: backend_type.to_string(),
        backend_path,
        backend_endpoint,
        backend_region,
        backend_force_path_style,
        max_delta_ratio: cfg.max_delta_ratio,
        max_object_size: cfg.max_object_size,
        cache_size_mb: cfg.cache_size_mb,
        auth_enabled: cfg.auth_enabled(),
        access_key_id: cfg.access_key_id.clone(),
        log_level,
        backend_has_credentials,
    })
}

/// PUT /api/admin/config — update configuration.
/// Hot-reloadable fields take effect immediately.
/// Restart-required fields are saved but return a warning.
pub async fn update_config(
    State(state): State<Arc<AdminState>>,
    Json(body): Json<ConfigUpdateRequest>,
) -> impl IntoResponse {
    let mut cfg = state.config.write().await;
    let mut warnings = Vec::new();
    let mut requires_restart = false;

    // Hot-reloadable fields
    if let Some(ratio) = body.max_delta_ratio {
        cfg.max_delta_ratio = ratio;
    }
    if let Some(size) = body.max_object_size {
        cfg.max_object_size = size;
    }
    if let Some(ref key) = body.access_key_id {
        cfg.access_key_id = if key.is_empty() {
            None
        } else {
            Some(key.clone())
        };
    }
    if let Some(ref secret) = body.secret_access_key {
        cfg.secret_access_key = if secret.is_empty() {
            None
        } else {
            Some(secret.clone())
        };
    }

    // Hot-reloadable: log level
    if let Some(ref level_str) = body.log_level {
        match level_str.parse::<EnvFilter>() {
            Ok(new_filter) => {
                if let Err(e) = state.log_reload.reload(new_filter) {
                    warnings.push(format!("Failed to reload log filter: {}", e));
                } else {
                    cfg.log_level = level_str.clone();
                    tracing::info!("Log level changed to: {}", level_str);
                }
            }
            Err(e) => {
                warnings.push(format!("Invalid log filter '{}': {}", level_str, e));
            }
        }
    }

    // Hot-reloadable: backend configuration (triggers engine swap)
    let current_backend_type = match &cfg.backend {
        crate::config::BackendConfig::Filesystem { .. } => "filesystem",
        crate::config::BackendConfig::S3 { .. } => "s3",
    };
    let requested_type = body.backend_type.as_deref().unwrap_or(current_backend_type);
    let type_changed = requested_type != current_backend_type;

    // Check if any backend field changed
    let be_key_changed = body
        .backend_access_key_id
        .as_ref()
        .is_some_and(|k| !k.is_empty());
    let be_secret_changed = body
        .backend_secret_access_key
        .as_ref()
        .is_some_and(|s| !s.is_empty());
    let backend_fields_changed = type_changed
        || body.backend_endpoint.is_some()
        || body.backend_region.is_some()
        || body.backend_path.is_some()
        || body.backend_force_path_style.is_some()
        || be_key_changed
        || be_secret_changed;

    if backend_fields_changed {
        let mut need_engine_swap = false;

        if type_changed {
            // Construct a new BackendConfig variant
            match requested_type {
                "filesystem" => {
                    let path = body
                        .backend_path
                        .clone()
                        .unwrap_or_else(|| "./data".to_string());
                    cfg.backend = crate::config::BackendConfig::Filesystem {
                        path: std::path::PathBuf::from(path),
                    };
                    need_engine_swap = true;
                    warnings.push(
                        "Backend type changed. Data in the previous backend is not migrated."
                            .to_string(),
                    );
                }
                "s3" => {
                    cfg.backend = crate::config::BackendConfig::S3 {
                        endpoint: body.backend_endpoint.clone(),
                        region: body
                            .backend_region
                            .clone()
                            .unwrap_or_else(|| "us-east-1".to_string()),
                        force_path_style: body.backend_force_path_style.unwrap_or(true),
                        access_key_id: body.backend_access_key_id.clone().filter(|k| !k.is_empty()),
                        secret_access_key: body
                            .backend_secret_access_key
                            .clone()
                            .filter(|s| !s.is_empty()),
                    };
                    need_engine_swap = true;
                    warnings.push(
                        "Backend type changed. Data in the previous backend is not migrated."
                            .to_string(),
                    );
                }
                other => {
                    warnings.push(format!(
                        "Unknown backend type: '{}'. Must be 'filesystem' or 's3'.",
                        other
                    ));
                }
            }
        } else {
            // Same type — update fields in-place
            match &mut cfg.backend {
                crate::config::BackendConfig::Filesystem { ref mut path } => {
                    if let Some(ref p) = body.backend_path {
                        *path = std::path::PathBuf::from(p);
                        need_engine_swap = true;
                    }
                }
                crate::config::BackendConfig::S3 {
                    ref mut endpoint,
                    ref mut region,
                    ref mut force_path_style,
                    ref mut access_key_id,
                    ref mut secret_access_key,
                } => {
                    if let Some(ref ep) = body.backend_endpoint {
                        *endpoint = if ep.is_empty() {
                            None
                        } else {
                            Some(ep.clone())
                        };
                        need_engine_swap = true;
                    }
                    if let Some(ref r) = body.backend_region {
                        *region = r.clone();
                        need_engine_swap = true;
                    }
                    if let Some(fps) = body.backend_force_path_style {
                        *force_path_style = fps;
                        need_engine_swap = true;
                    }
                    if let Some(ref key) = body.backend_access_key_id {
                        if !key.is_empty() {
                            *access_key_id = Some(key.clone());
                            need_engine_swap = true;
                        }
                    }
                    if let Some(ref secret) = body.backend_secret_access_key {
                        if !secret.is_empty() {
                            *secret_access_key = Some(secret.clone());
                            need_engine_swap = true;
                        }
                    }
                }
            }
        }

        if need_engine_swap {
            // Save old backend config so we can rollback if engine creation fails.
            let old_backend = cfg.backend.clone();
            match DynEngine::new(&cfg, Some(state.s3_state.metrics.clone())).await {
                Ok(new_engine) => {
                    state.s3_state.engine.store(Arc::new(new_engine));
                    tracing::info!("Backend engine rebuilt successfully");
                }
                Err(e) => {
                    // Rollback: restore old backend config so config and engine stay consistent.
                    cfg.backend = old_backend;
                    warnings.push(format!(
                        "Failed to create engine with new backend config (config rolled back): {}",
                        e
                    ));
                }
            }
        }
    }

    // Restart-required fields
    if let Some(ref addr) = body.listen_addr {
        if let Ok(parsed) = addr.parse() {
            if cfg.listen_addr != parsed {
                cfg.listen_addr = parsed;
                requires_restart = true;
                warnings.push(format!(
                    "listen_addr changed to {} — restart required",
                    addr
                ));
            }
        } else {
            warnings.push(format!("Invalid listen_addr: {}", addr));
        }
    }
    if let Some(cache) = body.cache_size_mb {
        if cfg.cache_size_mb != cache {
            cfg.cache_size_mb = cache;
            requires_restart = true;
            warnings.push(format!(
                "cache_size_mb changed to {} — restart required",
                cache
            ));
        }
    }

    // Hot-reload auth credentials into the live SigV4 middleware
    // Hot-reload auth credentials — but only when NOT in IAM mode.
    // In IAM mode, the IamIndex is the source of truth; overwriting it
    // with Legacy mode would silently destroy all IAM user authentication.
    if body.access_key_id.is_some() || body.secret_access_key.is_some() {
        let current = state.iam_state.load();
        if matches!(&**current, IamState::Iam(_)) {
            tracing::warn!(
                "Ignoring legacy credential update — IAM mode is active. \
                 Use the Users panel to manage credentials."
            );
            warnings.push(
                "Legacy credentials updated in config but NOT applied — \
                 IAM mode is active. Manage users via the Users panel."
                    .to_string(),
            );
        } else {
            let new_state = if let (Some(ref key_id), Some(ref secret)) =
                (&cfg.access_key_id, &cfg.secret_access_key)
            {
                IamState::Legacy(AuthConfig {
                    access_key_id: key_id.clone(),
                    secret_access_key: secret.clone(),
                })
            } else {
                IamState::Disabled
            };
            state.iam_state.store(Arc::new(new_state));
            tracing::info!(
                "Auth credentials hot-reloaded (auth enabled: {})",
                cfg.auth_enabled()
            );
        }
    }

    // Persist to TOML file
    if let Err(e) = cfg.persist_to_file("deltaglider_proxy.toml") {
        warnings.push(format!("Failed to persist config: {}", e));
    }

    Json(ConfigUpdateResponse {
        success: true,
        warnings,
        requires_restart,
    })
}

/// PUT /api/admin/password — change admin password.
pub async fn change_password(
    State(state): State<Arc<AdminState>>,
    Json(body): Json<PasswordChangeRequest>,
) -> impl IntoResponse {
    let current_hash = state.password_hash.read().clone();
    let valid = match bcrypt::verify(&body.current_password, &current_hash) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("bcrypt verify failed (corrupted hash?): {}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(PasswordChangeResponse {
                    ok: false,
                    error: Some(
                        "Password hash is corrupted. Delete .deltaglider_admin_hash and restart."
                            .to_string(),
                    ),
                }),
            )
                .into_response();
        }
    };

    if !valid {
        return (
            StatusCode::FORBIDDEN,
            Json(PasswordChangeResponse {
                ok: false,
                error: Some("Current password is incorrect".to_string()),
            }),
        )
            .into_response();
    }

    let new_hash = match bcrypt::hash(&body.new_password, bcrypt::DEFAULT_COST) {
        Ok(h) => h,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(PasswordChangeResponse {
                    ok: false,
                    error: Some(format!("Hashing failed: {}", e)),
                }),
            )
                .into_response();
        }
    };

    // Update in-memory
    *state.password_hash.write() = new_hash.clone();

    // Re-encrypt the IAM config database with the new password hash.
    // Without this, the next restart would fail to open the DB (wrong key).
    if let Some(ref db_mutex) = state.config_db {
        let db = db_mutex.lock().await;
        if let Err(e) = db.rekey(&new_hash) {
            tracing::error!(
                "Failed to re-encrypt config DB after password change: {}",
                e
            );
        } else {
            tracing::info!("Config DB re-encrypted with new admin password hash");
        }
    }

    // Persist to state file
    let state_file = std::path::Path::new(".deltaglider_admin_hash");
    if let Err(e) = std::fs::write(state_file, &new_hash) {
        tracing::warn!("Failed to persist new admin hash: {}", e);
    }

    // Also update config
    {
        let mut cfg = state.config.write().await;
        cfg.admin_password_hash = Some(new_hash);
    }

    (
        StatusCode::OK,
        Json(PasswordChangeResponse {
            ok: true,
            error: None,
        }),
    )
        .into_response()
}

// === Test S3 Connection ===

#[derive(Deserialize)]
pub struct TestS3Request {
    pub endpoint: Option<String>,
    pub region: Option<String>,
    pub force_path_style: Option<bool>,
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
}

#[derive(Serialize)]
pub struct TestS3Response {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub buckets: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<String>,
}

/// POST /api/admin/test-s3 — test S3 connectivity with provided (or saved) credentials.
pub async fn test_s3_connection(
    State(state): State<Arc<AdminState>>,
    Json(body): Json<TestS3Request>,
) -> impl IntoResponse {
    let cfg = state.config.read().await;

    // Merge form values with saved config (form overrides, blanks fall back to saved)
    let (saved_endpoint, saved_region, saved_fps, saved_key, saved_secret) = match &cfg.backend {
        crate::config::BackendConfig::S3 {
            endpoint,
            region,
            force_path_style,
            access_key_id,
            secret_access_key,
        } => (
            endpoint.clone(),
            Some(region.clone()),
            Some(*force_path_style),
            access_key_id.clone(),
            secret_access_key.clone(),
        ),
        _ => (None, None, None, None, None),
    };

    let merged_endpoint = body.endpoint.clone().or(saved_endpoint);
    let merged_region = body
        .region
        .clone()
        .or(saved_region)
        .unwrap_or_else(|| "us-east-1".to_string());
    let merged_fps = body.force_path_style.or(saved_fps).unwrap_or(true);
    let merged_key = body
        .access_key_id
        .clone()
        .filter(|k| !k.is_empty())
        .or(saved_key);
    let merged_secret = body
        .secret_access_key
        .clone()
        .filter(|s| !s.is_empty())
        .or(saved_secret);

    // Drop the config lock before doing I/O
    drop(cfg);

    let test_config = crate::config::BackendConfig::S3 {
        endpoint: merged_endpoint,
        region: merged_region,
        force_path_style: merged_fps,
        access_key_id: merged_key,
        secret_access_key: merged_secret,
    };

    // Build a temporary client
    let client = match crate::storage::S3Backend::build_client(&test_config).await {
        Ok(c) => c,
        Err(e) => {
            return Json(TestS3Response {
                success: false,
                buckets: None,
                error: Some(e.to_string()),
                error_kind: Some("credentials".to_string()),
            });
        }
    };

    // Try list_buckets with a 10-second timeout
    match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        client.list_buckets().send(),
    )
    .await
    {
        Ok(Ok(response)) => {
            let names: Vec<String> = response
                .buckets()
                .iter()
                .filter_map(|b| b.name().map(|n| n.to_string()))
                .collect();
            Json(TestS3Response {
                success: true,
                buckets: Some(names),
                error: None,
                error_kind: None,
            })
        }
        Ok(Err(e)) => {
            let err_str = format!("{}", e);
            let kind = if err_str.contains("credentials")
                || err_str.contains("InvalidAccessKeyId")
                || err_str.contains("SignatureDoesNotMatch")
                || err_str.contains("403")
            {
                "credentials"
            } else if err_str.contains("connect")
                || err_str.contains("Connection refused")
                || err_str.contains("dns")
                || err_str.contains("resolve")
            {
                "connection"
            } else {
                "unknown"
            };
            Json(TestS3Response {
                success: false,
                buckets: None,
                error: Some(err_str),
                error_kind: Some(kind.to_string()),
            })
        }
        Err(_) => Json(TestS3Response {
            success: false,
            buckets: None,
            error: Some("Connection timed out after 10 seconds".to_string()),
            error_kind: Some("timeout".to_string()),
        }),
    }
}

/// Middleware: validate session for protected admin routes.
/// Returns 401 if the session cookie is missing or invalid.
pub async fn require_session(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> impl IntoResponse {
    let valid = extract_session_token(&headers)
        .map(|t| state.sessions.validate(&t))
        .unwrap_or(false);

    if !valid {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"error": "unauthorized"})),
        )
            .into_response();
    }

    next.run(request).await.into_response()
}

/// Extract the `dgp_session` token from the Cookie header.
fn extract_session_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .find_map(|part| {
            let part = part.trim();
            part.strip_prefix("dgp_session=")
                .map(|value| value.to_string())
        })
}

// === IAM User Management Handlers ===

#[derive(Deserialize)]
pub struct CreateUserRequest {
    pub name: String,
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub permissions: Vec<Permission>,
}

fn default_true() -> bool {
    true
}

#[derive(Deserialize)]
pub struct UpdateUserRequest {
    pub name: Option<String>,
    pub enabled: Option<bool>,
    pub permissions: Option<Vec<Permission>>,
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
/// On first IAM user creation (Legacy → IAM transition), auto-migrates the
/// legacy TOML credentials as a "legacy-admin" user with full access so
/// existing S3 clients don't break.
fn rebuild_iam_index(db: &ConfigDb, iam_state: &SharedIamState) -> Result<(), StatusCode> {
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
                actions: vec!["*".into()],
                resources: vec!["*".into()],
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

    let count = users.len();
    let index = IamIndex::from_users(users);
    iam_state.store(Arc::new(IamState::Iam(index)));
    tracing::debug!("IAM index rebuilt with {} users", count);
    Ok(())
}

/// GET /api/admin/users — list all users (secrets masked).
/// Returns empty list if IAM DB is not initialized (legacy/open mode).
pub async fn list_users(
    State(state): State<Arc<AdminState>>,
) -> Result<Json<Vec<IamUser>>, StatusCode> {
    let db = match state.config_db.as_ref() {
        Some(db) => db,
        None => return Ok(Json(vec![])), // No IAM DB → empty list (not an error)
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

    let user = db
        .create_user(
            &body.name,
            &access_key_id,
            &secret_access_key,
            body.enabled,
            &body.permissions,
        )
        .map_err(|e| {
            tracing::warn!("Failed to create user '{}': {}", body.name, e);
            StatusCode::CONFLICT
        })?;

    rebuild_iam_index(&db, &state.iam_state)?;

    tracing::info!("IAM user '{}' created ({})", user.name, user.access_key_id);
    // Return full user including secret (shown only once)
    Ok((StatusCode::CREATED, Json(user)))
}

/// PUT /api/admin/users/:id — update a user.
pub async fn update_user(
    State(state): State<Arc<AdminState>>,
    axum::extract::Path(user_id): axum::extract::Path<i64>,
    Json(body): Json<UpdateUserRequest>,
) -> Result<Json<IamUser>, StatusCode> {
    let db = state.config_db.as_ref().ok_or(StatusCode::NOT_FOUND)?;
    let db = db.lock().await;

    let user = db
        .update_user(
            user_id,
            body.name.as_deref(),
            body.enabled,
            body.permissions.as_deref(),
        )
        .map_err(|e| {
            tracing::warn!("Failed to update user {}: {}", user_id, e);
            StatusCode::NOT_FOUND
        })?;

    rebuild_iam_index(&db, &state.iam_state)?;

    tracing::info!("IAM user '{}' updated", user.name);
    Ok(Json(mask_user(&user)))
}

/// DELETE /api/admin/users/:id — delete a user.
pub async fn delete_user(
    State(state): State<Arc<AdminState>>,
    axum::extract::Path(user_id): axum::extract::Path<i64>,
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

    tracing::info!(
        "IAM user {} deleted ({} users remaining)",
        user_id,
        remaining
    );
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct RotateKeysRequest {
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
}

/// POST /api/admin/users/:id/rotate-keys — set or regenerate access keys.
/// If access_key_id or secret_access_key are provided, uses those values.
/// Otherwise auto-generates new ones.
pub async fn rotate_user_keys(
    State(state): State<Arc<AdminState>>,
    axum::extract::Path(user_id): axum::extract::Path<i64>,
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

    tracing::info!(
        "IAM user '{}' keys rotated (new: {})",
        user.name,
        user.access_key_id
    );
    // Return full user including new secret (shown only once)
    Ok(Json(user))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: SharedAuthConfig must reflect credential updates immediately.
    /// This guards against reverting to a static Extension<Option<Arc<AuthConfig>>>.
    #[test]
    fn shared_auth_config_reflects_updates() {
        let shared: SharedIamState = Arc::new(arc_swap::ArcSwap::from_pointee(IamState::Disabled));

        // Initially no auth
        assert!(matches!(&**shared.load(), IamState::Disabled));

        // Simulate admin API updating credentials
        shared.store(Arc::new(IamState::Legacy(AuthConfig {
            access_key_id: "new-key".to_string(),
            secret_access_key: "new-secret".to_string(),
        })));

        // Middleware must see the update
        let loaded = shared.load();
        match &**loaded {
            IamState::Legacy(auth) => {
                assert_eq!(auth.access_key_id, "new-key");
                assert_eq!(auth.secret_access_key, "new-secret");
            }
            _ => panic!("Expected IamState::Legacy"),
        }

        // Simulate disabling auth (clearing both credentials)
        shared.store(Arc::new(IamState::Disabled));
        assert!(matches!(&**shared.load(), IamState::Disabled));
    }

    #[test]
    fn extract_session_token_from_cookie() {
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, "dgp_session=abc123".parse().unwrap());
        assert_eq!(extract_session_token(&headers).unwrap(), "abc123");

        // Multiple cookies
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            "foo=bar; dgp_session=xyz789; baz=qux".parse().unwrap(),
        );
        assert_eq!(extract_session_token(&headers).unwrap(), "xyz789");

        // No session cookie
        let mut headers = HeaderMap::new();
        headers.insert(header::COOKIE, "foo=bar".parse().unwrap());
        assert!(extract_session_token(&headers).is_none());

        // No cookie header at all
        assert!(extract_session_token(&HeaderMap::new()).is_none());
    }
}
