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
use crate::deltaglider::DynEngine;
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
    let valid = bcrypt::verify(&body.password, &hash).unwrap_or(false);

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
            match DynEngine::new(&cfg).await {
                Ok(new_engine) => {
                    state.s3_state.engine.store(Arc::new(new_engine));
                    tracing::info!("Backend engine rebuilt successfully");
                }
                Err(e) => {
                    warnings.push(format!(
                        "Failed to create engine with new backend config (keeping old engine): {}",
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
    let valid = bcrypt::verify(&body.current_password, &current_hash).unwrap_or(false);

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
