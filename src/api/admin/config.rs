//! Config handlers: get_config, update_config, change_password, test_s3_connection.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

use crate::deltaglider::DynEngine;
use crate::iam::{AuthConfig, IamState};

use super::{audit_log, trigger_config_sync, validate_password, AdminState};

#[derive(Serialize)]
pub struct ConfigResponse {
    listen_addr: String,
    backend_type: String,
    // Backend details
    backend_path: Option<String>,
    backend_endpoint: Option<String>,
    backend_region: Option<String>,
    backend_force_path_style: Option<bool>,
    // Compression
    max_delta_ratio: f32,
    max_object_size: u64,
    cache_size_mb: usize,
    metadata_cache_mb: usize,
    codec_concurrency: usize,
    codec_timeout_secs: u64,
    // Limits
    request_timeout_secs: u64,
    max_concurrent_requests: usize,
    max_multipart_uploads: usize,
    // Auth
    auth_enabled: bool,
    access_key_id: Option<String>,
    // Security
    clock_skew_seconds: u64,
    replay_window_secs: u64,
    rate_limit_max_attempts: u32,
    rate_limit_window_secs: u64,
    rate_limit_lockout_secs: u64,
    session_ttl_hours: u64,
    trust_proxy_headers: bool,
    secure_cookies: bool,
    debug_headers: bool,
    // Sync
    config_sync_bucket: Option<String>,
    // Per-bucket policies
    bucket_policies: std::collections::HashMap<String, crate::bucket_policy::BucketPolicyConfig>,
    // Log level
    log_level: String,
    // Backend credentials indicator
    backend_has_credentials: bool,
    // Multi-backend
    backends: Vec<BackendInfoResponse>,
    default_backend: Option<String>,
    // Fields that differ from the TOML config file on disk
    tainted_fields: Vec<String>,
}

/// Sanitized backend info (no secrets) for the admin API.
#[derive(Serialize, Clone)]
pub struct BackendInfoResponse {
    pub name: String,
    pub backend_type: String,
    pub path: Option<String>,
    pub endpoint: Option<String>,
    pub region: Option<String>,
    pub force_path_style: Option<bool>,
    pub has_credentials: bool,
}

impl From<&crate::config::NamedBackendConfig> for BackendInfoResponse {
    fn from(named: &crate::config::NamedBackendConfig) -> Self {
        match &named.backend {
            crate::config::BackendConfig::Filesystem { path } => Self {
                name: named.name.clone(),
                backend_type: "filesystem".into(),
                path: Some(path.display().to_string()),
                endpoint: None,
                region: None,
                force_path_style: None,
                has_credentials: false,
            },
            crate::config::BackendConfig::S3 {
                endpoint,
                region,
                force_path_style,
                access_key_id,
                ..
            } => Self {
                name: named.name.clone(),
                backend_type: "s3".into(),
                path: None,
                endpoint: endpoint.clone(),
                region: Some(region.clone()),
                force_path_style: Some(*force_path_style),
                has_credentials: access_key_id.is_some(),
            },
        }
    }
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
    // Per-bucket compression policies
    pub bucket_policies:
        Option<std::collections::HashMap<String, crate::bucket_policy::BucketPolicyConfig>>,
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

/// Compare the runtime config against the TOML file on disk.
/// Returns a list of field names where the runtime value differs from disk.
fn compute_tainted_fields(runtime: &crate::config::Config) -> Vec<String> {
    let disk = match crate::config::Config::resolve_config_path() {
        Some(path) => match crate::config::Config::from_file(&path) {
            Ok(cfg) => cfg,
            Err(_) => return vec![], // Can't read file — nothing to compare
        },
        None => return vec![], // No config file on disk
    };

    let mut tainted = Vec::new();

    // Compression settings
    if (runtime.max_delta_ratio - disk.max_delta_ratio).abs() > f32::EPSILON {
        tainted.push("max_delta_ratio".to_string());
    }
    if runtime.max_object_size != disk.max_object_size {
        tainted.push("max_object_size".to_string());
    }
    if runtime.cache_size_mb != disk.cache_size_mb {
        tainted.push("cache_size_mb".to_string());
    }
    if runtime.metadata_cache_mb != disk.metadata_cache_mb {
        tainted.push("metadata_cache_mb".to_string());
    }

    // Backend type
    let runtime_type = match &runtime.backend {
        crate::config::BackendConfig::Filesystem { .. } => "filesystem",
        crate::config::BackendConfig::S3 { .. } => "s3",
    };
    let disk_type = match &disk.backend {
        crate::config::BackendConfig::Filesystem { .. } => "filesystem",
        crate::config::BackendConfig::S3 { .. } => "s3",
    };
    if runtime_type != disk_type {
        tainted.push("backend_type".to_string());
    }

    // Backend details (only compare within same type)
    match (&runtime.backend, &disk.backend) {
        (
            crate::config::BackendConfig::Filesystem { path: rp },
            crate::config::BackendConfig::Filesystem { path: dp },
        ) => {
            if rp != dp {
                tainted.push("backend_path".to_string());
            }
        }
        (
            crate::config::BackendConfig::S3 {
                endpoint: re,
                region: rr,
                force_path_style: rf,
                ..
            },
            crate::config::BackendConfig::S3 {
                endpoint: de,
                region: dr,
                force_path_style: df,
                ..
            },
        ) => {
            if re != de {
                tainted.push("backend_endpoint".to_string());
            }
            if rr != dr {
                tainted.push("backend_region".to_string());
            }
            if rf != df {
                tainted.push("backend_force_path_style".to_string());
            }
        }
        _ => {} // Different backend types already flagged above
    }

    // Auth
    if runtime.access_key_id != disk.access_key_id {
        tainted.push("access_key_id".to_string());
    }

    // Log level
    if runtime.log_level != disk.log_level {
        tainted.push("log_level".to_string());
    }

    // Config sync
    if runtime.config_sync_bucket != disk.config_sync_bucket {
        tainted.push("config_sync_bucket".to_string());
    }

    // Listen address
    if runtime.listen_addr != disk.listen_addr {
        tainted.push("listen_addr".to_string());
    }

    // Bucket policies
    if runtime.buckets != disk.buckets {
        tainted.push("bucket_policies".to_string());
    }

    // Multi-backend
    if runtime.backends.len() != disk.backends.len()
        || runtime
            .backends
            .iter()
            .zip(disk.backends.iter())
            .any(|(r, d)| r.name != d.name)
    {
        tainted.push("backends".to_string());
    }
    if runtime.default_backend != disk.default_backend {
        tainted.push("default_backend".to_string());
    }

    tainted
}

/// Rebuild the engine from current config, storing the new engine on success.
/// Returns `Ok(())` on success, or an error message string on failure.
pub(super) async fn rebuild_engine(
    state: &Arc<AdminState>,
    cfg: &crate::config::Config,
    context: &str,
) -> Result<(), String> {
    match DynEngine::new(cfg, Some(state.s3_state.metrics.clone())).await {
        Ok(new_engine) => {
            state.s3_state.engine.store(Arc::new(new_engine));
            tracing::info!("{}", context);
            Ok(())
        }
        Err(e) => Err(format!("{}", e)),
    }
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

    // Read startup-time settings from env vars (these aren't in Config)
    let env_u64 = |name: &str, default: u64| -> u64 {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    let env_usize = |name: &str, default: usize| -> usize {
        std::env::var(name)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    let env_bool = |name: &str, default: bool| -> bool {
        std::env::var(name)
            .ok()
            .map(|v| v == "true" || v == "1")
            .unwrap_or(default)
    };
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);

    let tainted_fields = compute_tainted_fields(&cfg);

    let backends_info: Vec<BackendInfoResponse> =
        cfg.backends.iter().map(BackendInfoResponse::from).collect();

    Json(ConfigResponse {
        listen_addr: cfg.listen_addr.to_string(),
        backend_type: backend_type.to_string(),
        backend_path,
        backend_endpoint,
        backend_region,
        backend_force_path_style,
        // Compression
        max_delta_ratio: cfg.max_delta_ratio,
        max_object_size: cfg.max_object_size,
        cache_size_mb: cfg.cache_size_mb,
        metadata_cache_mb: cfg.metadata_cache_mb,
        codec_concurrency: cfg.codec_concurrency.unwrap_or_else(|| (cpus * 4).max(16)),
        codec_timeout_secs: env_u64("DGP_CODEC_TIMEOUT_SECS", 60),
        // Limits
        request_timeout_secs: env_u64("DGP_REQUEST_TIMEOUT_SECS", 300),
        max_concurrent_requests: env_usize("DGP_MAX_CONCURRENT_REQUESTS", 1024),
        max_multipart_uploads: env_usize("DGP_MAX_MULTIPART_UPLOADS", 1000),
        // Auth
        auth_enabled: cfg.auth_enabled(),
        access_key_id: cfg.access_key_id.clone(),
        // Security
        clock_skew_seconds: env_u64("DGP_CLOCK_SKEW_SECONDS", 300),
        replay_window_secs: env_u64("DGP_REPLAY_WINDOW_SECS", 2),
        rate_limit_max_attempts: env_u64("DGP_RATE_LIMIT_MAX_ATTEMPTS", 100) as u32,
        rate_limit_window_secs: env_u64("DGP_RATE_LIMIT_WINDOW_SECS", 300),
        rate_limit_lockout_secs: env_u64("DGP_RATE_LIMIT_LOCKOUT_SECS", 600),
        session_ttl_hours: env_u64("DGP_SESSION_TTL_HOURS", 4),
        trust_proxy_headers: env_bool("DGP_TRUST_PROXY_HEADERS", true),
        secure_cookies: env_bool("DGP_SECURE_COOKIES", true),
        debug_headers: env_bool("DGP_DEBUG_HEADERS", false),
        // Sync
        config_sync_bucket: cfg.config_sync_bucket.clone(),
        bucket_policies: cfg.buckets.clone(),
        // Logging
        log_level,
        backend_has_credentials,
        backends: backends_info,
        default_backend: cfg.default_backend.clone(),
        tainted_fields,
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
            let old_backend = cfg.backend.clone();
            if let Err(e) =
                rebuild_engine(&state, &cfg, "Backend engine rebuilt successfully").await
            {
                cfg.backend = old_backend;
                warnings.push(format!(
                    "Failed to create engine with new backend config (config rolled back): {}",
                    e
                ));
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

    // Update per-bucket policies (hot-reloadable — triggers engine rebuild)
    if let Some(ref bucket_policies) = body.bucket_policies {
        // Normalize bucket names to lowercase before storing (S3 bucket names are lowercase)
        let normalized: std::collections::HashMap<String, _> = bucket_policies
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
            .collect();
        let old_buckets = cfg.buckets.clone();
        cfg.buckets = normalized;
        if let Err(e) =
            rebuild_engine(&state, &cfg, "Bucket policies updated, engine rebuilt").await
        {
            cfg.buckets = old_buckets;
            warnings.push(format!("Failed to apply bucket policies: {}", e));
        }
        // Rebuild public prefix snapshot (hot-swap, lock-free)
        let new_snapshot = crate::bucket_policy::PublicPrefixSnapshot::from_config(&cfg.buckets);
        state
            .public_prefix_snapshot
            .store(std::sync::Arc::new(new_snapshot));
    }

    // Persist to TOML file
    if let Err(e) = cfg.persist_to_file(crate::config::DEFAULT_CONFIG_FILENAME) {
        warnings.push(format!("Failed to persist config: {}", e));
    }

    Json(ConfigUpdateResponse {
        success: true,
        warnings,
        requires_restart,
    })
}

/// PUT /api/admin/password — change bootstrap password.
pub async fn change_password(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
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
                        "Password hash is corrupted. Delete .deltaglider_bootstrap_hash and restart."
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

    // Validate new password quality
    if let Err(msg) = validate_password(&body.new_password) {
        return (
            StatusCode::BAD_REQUEST,
            Json(PasswordChangeResponse {
                ok: false,
                error: Some(msg.to_string()),
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

    // Re-encrypt the IAM config database with the new password hash FIRST.
    // If this fails, we must NOT update the in-memory hash or persist — the DB
    // would become out of sync and the next restart would fail to open it.
    if let Some(ref db_mutex) = state.config_db {
        let db = db_mutex.lock().await;
        if let Err(e) = db.rekey(&new_hash) {
            tracing::error!(
                "Failed to re-encrypt config DB after password change: {}",
                e
            );
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(PasswordChangeResponse {
                    ok: false,
                    error: Some(format!("Failed to re-encrypt config database: {}", e)),
                }),
            )
                .into_response();
        }
        tracing::info!("Config DB re-encrypted with new bootstrap password hash");
        // Upload re-encrypted DB to S3
        trigger_config_sync(&state);
    }

    // Update in-memory only after DB rekey succeeded
    *state.password_hash.write() = new_hash.clone();

    // Persist to state file
    let state_file = std::path::Path::new(".deltaglider_bootstrap_hash");
    if let Err(e) = crate::config::write_bootstrap_hash_file(state_file, &new_hash) {
        tracing::warn!("Failed to persist new admin hash: {}", e);
    }

    // Also update config
    {
        let mut cfg = state.config.write().await;
        cfg.bootstrap_password_hash = Some(new_hash);
    }

    audit_log("change_password", "bootstrap", "", &headers);

    (
        StatusCode::OK,
        Json(PasswordChangeResponse {
            ok: true,
            error: None,
        }),
    )
        .into_response()
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

// ============================================================================
// Config DB recovery
// ============================================================================

#[derive(Deserialize)]
pub struct RecoverDbRequest {
    candidate_password: String,
}

#[derive(Serialize)]
pub struct RecoverDbResponse {
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    correct_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    correct_hash_base64: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// POST /api/admin/recover-db — try a candidate password against the locked config DB.
///
/// Only available when `config_db_mismatch` is true. Returns the correct bcrypt
/// hash (and base64 version) if the candidate password successfully decrypts the DB.
pub async fn recover_db(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
    Json(body): Json<RecoverDbRequest>,
) -> impl IntoResponse {
    if !state.config_db_mismatch {
        return (
            StatusCode::NOT_FOUND,
            Json(RecoverDbResponse {
                success: false,
                correct_hash: None,
                correct_hash_base64: None,
                error: Some("No config DB mismatch detected".into()),
            }),
        );
    }

    // Rate limit recovery attempts
    let client_ip = crate::rate_limiter::extract_client_ip(&headers);
    if let Some(ip) = &client_ip {
        if state.rate_limiter.is_limited(ip) {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                Json(RecoverDbResponse {
                    success: false,
                    correct_hash: None,
                    correct_hash_base64: None,
                    error: Some("Too many attempts — try again later".into()),
                }),
            );
        }
    }

    // The SQLCipher DB is encrypted with the bcrypt HASH string (not the plaintext
    // password). Accept the hash in either raw ($2b$12$...) or base64 form.
    let candidate = body.candidate_password.trim().to_string();
    let candidate_hash = if candidate.starts_with("$2") {
        // Raw bcrypt hash
        candidate.clone()
    } else {
        // Try base64 decode → UTF-8 → bcrypt hash prefix check
        let decoded =
            base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &candidate)
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .filter(|s| s.starts_with("$2"));

        match decoded {
            Some(hash) => hash,
            None => {
                if let Some(ip) = &client_ip {
                    state.rate_limiter.record_failure(ip);
                }
                return (
                    StatusCode::BAD_REQUEST,
                    Json(RecoverDbResponse {
                        success: false,
                        correct_hash: None,
                        correct_hash_base64: None,
                        error: Some(
                            "Input is not a bcrypt hash. Provide the hash ($2b$12$...) or its base64 encoding."
                                .into(),
                        ),
                    }),
                );
            }
        }
    };

    // Try local .db.bak first
    let bak_path = crate::config_db::config_db_path().with_extension("db.bak");
    let try_path = if bak_path.exists() {
        Some(bak_path)
    } else {
        // Try S3 fallback if config_sync is enabled
        if let Some(ref sync) = state.config_sync {
            match sync.download_raw().await {
                Ok(data) => {
                    let tmp_path = crate::config_db::config_db_path().with_extension("db.recovery");
                    if let Err(e) = std::fs::write(&tmp_path, &data) {
                        tracing::warn!("Failed to write recovery temp file: {}", e);
                        None
                    } else {
                        Some(tmp_path)
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to download config DB from S3 for recovery: {}", e);
                    None
                }
            }
        } else {
            None
        }
    };

    let Some(db_path) = try_path else {
        return (
            StatusCode::NOT_FOUND,
            Json(RecoverDbResponse {
                success: false,
                correct_hash: None,
                correct_hash_base64: None,
                error: Some(
                    "No config database found to recover (no .bak file and no S3 copy)".into(),
                ),
            }),
        );
    };

    // Try to open with the candidate hash
    let is_recovery_temp = db_path
        .extension()
        .map(|e| e == "recovery")
        .unwrap_or(false);
    let result = crate::config_db::ConfigDb::open_or_create(&db_path, &candidate_hash);

    // Always clean up the recovery temp file (from S3 download), regardless of outcome
    if is_recovery_temp {
        let _ = std::fs::remove_file(&db_path);
    }

    match result {
        Ok(_db) => {
            if let Some(ip) = &client_ip {
                state.rate_limiter.record_success(ip);
            }

            let hash_base64 = base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                candidate_hash.as_bytes(),
            );

            audit_log("recover_db_success", "admin", "", &headers);

            (
                StatusCode::OK,
                Json(RecoverDbResponse {
                    success: true,
                    correct_hash: Some(candidate_hash),
                    correct_hash_base64: Some(hash_base64),
                    error: None,
                }),
            )
        }
        Err(_) => {
            if let Some(ip) = &client_ip {
                state.rate_limiter.record_failure(ip);
            }

            (
                StatusCode::UNAUTHORIZED,
                Json(RecoverDbResponse {
                    success: false,
                    correct_hash: None,
                    correct_hash_base64: None,
                    error: Some("Password does not match the encrypted database".into()),
                }),
            )
        }
    }
}
