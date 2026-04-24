//! Admin API for managing named backends (multi-backend routing).

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::config::{BackendConfig, NamedBackendConfig};

use super::AdminState;

#[derive(Serialize)]
pub struct BackendListResponse {
    pub backends: Vec<super::config::BackendInfoResponse>,
    pub default_backend: Option<String>,
}

#[derive(Deserialize)]
pub struct CreateBackendRequest {
    pub name: String,
    #[serde(rename = "type")]
    pub backend_type: String,
    pub path: Option<String>,
    pub endpoint: Option<String>,
    pub region: Option<String>,
    pub force_path_style: Option<bool>,
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
    /// Set this backend as the default.
    pub set_default: Option<bool>,
}

#[derive(Serialize)]
pub struct BackendMutationResponse {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub requires_restart: bool,
}

fn build_backend_config(req: &CreateBackendRequest) -> Result<BackendConfig, String> {
    match req.backend_type.as_str() {
        "filesystem" => {
            let path = req.path.as_deref().unwrap_or("./data").to_string();
            Ok(BackendConfig::Filesystem {
                path: std::path::PathBuf::from(path),
            })
        }
        "s3" => {
            // Validate credentials upfront (S3Backend::new will reject them later,
            // but the error is confusing; better to fail early with a clear message)
            if req.access_key_id.as_ref().is_none_or(|s| s.is_empty())
                || req.secret_access_key.as_ref().is_none_or(|s| s.is_empty())
            {
                return Err("S3 backend requires both access_key_id and secret_access_key".into());
            }
            Ok(BackendConfig::S3 {
                endpoint: req.endpoint.clone(),
                region: req
                    .region
                    .clone()
                    .unwrap_or_else(|| "us-east-1".to_string()),
                force_path_style: req.force_path_style.unwrap_or(true),
                access_key_id: req.access_key_id.clone(),
                secret_access_key: req.secret_access_key.clone(),
            })
        }
        other => Err(format!(
            "Unknown backend type: '{other}'. Must be 'filesystem' or 's3'."
        )),
    }
}

/// GET /api/admin/backends — list all named backends.
pub async fn list_backends(State(state): State<Arc<AdminState>>) -> impl IntoResponse {
    let cfg = state.config.read().await;
    let backends = cfg
        .backends
        .iter()
        .map(super::config::BackendInfoResponse::from)
        .collect();

    Json(BackendListResponse {
        backends,
        default_backend: cfg.default_backend.clone(),
    })
}

/// POST /api/admin/backends — add a new named backend.
pub async fn create_backend(
    State(state): State<Arc<AdminState>>,
    Json(body): Json<CreateBackendRequest>,
) -> impl IntoResponse {
    let name = body.name.trim().to_string();
    if name.is_empty() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            Json(BackendMutationResponse {
                success: false,
                error: Some("Backend name cannot be empty".into()),
                requires_restart: false,
            }),
        );
    }

    let backend_config = match build_backend_config(&body) {
        Ok(bc) => bc,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(BackendMutationResponse {
                    success: false,
                    error: Some(e),
                    requires_restart: false,
                }),
            );
        }
    };

    let mut cfg = state.config.write().await;

    // Check for duplicate name
    if cfg.backends.iter().any(|b| b.name == name) {
        return (
            axum::http::StatusCode::CONFLICT,
            Json(BackendMutationResponse {
                success: false,
                error: Some(format!("Backend '{}' already exists", name)),
                requires_restart: false,
            }),
        );
    }

    let old_backends = cfg.backends.clone();
    let old_default = cfg.default_backend.clone();

    cfg.backends.push(NamedBackendConfig {
        name: name.clone(),
        backend: backend_config,
        // STEP-1: per-backend encryption config. `CreateBackendRequest`
        // will gain an optional `encryption` field in Step 6 (per the
        // plan); until then new backends default to plaintext (mode:
        // none) — operators configure encryption after creation via
        // the Backends panel or a section-level PATCH.
        encryption: crate::config::BackendEncryptionConfig::default(),
    });

    if body.set_default == Some(true) || cfg.default_backend.is_none() {
        cfg.default_backend = Some(name.clone());
    }

    if let Err(e) = super::config::rebuild_engine(
        &state,
        &cfg,
        &format!("Backend '{}' added, engine rebuilt", name),
    )
    .await
    {
        cfg.backends = old_backends;
        cfg.default_backend = old_default;
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(BackendMutationResponse {
                success: false,
                error: Some(format!("Failed to rebuild engine: {}", e)),
                requires_restart: false,
            }),
        );
    }

    // Persist to the active config file resolved at startup from `--config`
    // or the search-path walk. Hardcoding `DEFAULT_CONFIG_FILENAME` here
    // used to silently redirect admin-API writes to a stale location when
    // the operator had launched with `--config /etc/dgp/config.yaml`,
    // producing a latent "my backend disappears on restart" bug.
    //
    // Note: we do NOT call `trigger_config_sync` here. That helper uploads
    // the SQLCipher IAM database to S3 — a backend mutation changes the
    // TOML/YAML config file, not the IAM DB, so the sync would be a no-op
    // network round-trip. Handlers that DO mutate the IAM DB (users,
    // groups, external_auth, password) are the correct callers.
    let persist_path = super::config::active_config_path(&state);
    if let Err(e) = cfg.persist_to_file(&persist_path) {
        tracing::warn!("Failed to persist config to {}: {}", persist_path, e);
    }

    (
        axum::http::StatusCode::CREATED,
        Json(BackendMutationResponse {
            success: true,
            error: None,
            requires_restart: false,
        }),
    )
}

/// DELETE /api/admin/backends/:name — remove a named backend.
pub async fn delete_backend(
    State(state): State<Arc<AdminState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let mut cfg = state.config.write().await;

    // Check if backend exists
    if !cfg.backends.iter().any(|b| b.name == name) {
        return (
            axum::http::StatusCode::NOT_FOUND,
            Json(BackendMutationResponse {
                success: false,
                error: Some(format!("Backend '{}' not found", name)),
                requires_restart: false,
            }),
        );
    }

    // Check if it's the default backend
    if cfg.default_backend.as_deref() == Some(&name) {
        return (
            axum::http::StatusCode::CONFLICT,
            Json(BackendMutationResponse {
                success: false,
                error: Some(
                    "Cannot delete the default backend. Assign a new default first.".into(),
                ),
                requires_restart: false,
            }),
        );
    }

    // Check if any bucket policies route to this backend
    let routed: Vec<String> = cfg
        .buckets
        .iter()
        .filter(|(_, p)| p.backend.as_deref() == Some(&name))
        .map(|(bucket, _)| bucket.clone())
        .collect();
    if !routed.is_empty() {
        return (
            axum::http::StatusCode::CONFLICT,
            Json(BackendMutationResponse {
                success: false,
                error: Some(format!(
                    "Cannot delete '{}': buckets [{}] route to it. Re-route them first.",
                    name,
                    routed.join(", ")
                )),
                requires_restart: false,
            }),
        );
    }

    let old_backends = cfg.backends.clone();
    cfg.backends.retain(|b| b.name != name);

    if let Err(e) = super::config::rebuild_engine(
        &state,
        &cfg,
        &format!("Backend '{}' removed, engine rebuilt", name),
    )
    .await
    {
        cfg.backends = old_backends;
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(BackendMutationResponse {
                success: false,
                error: Some(format!("Failed to rebuild engine: {}", e)),
                requires_restart: false,
            }),
        );
    }

    // Persist to the active config file resolved at startup from `--config`
    // or the search-path walk. Hardcoding `DEFAULT_CONFIG_FILENAME` here
    // used to silently redirect admin-API writes to a stale location when
    // the operator had launched with `--config /etc/dgp/config.yaml`,
    // producing a latent "my backend disappears on restart" bug.
    //
    // Note: we do NOT call `trigger_config_sync` here. That helper uploads
    // the SQLCipher IAM database to S3 — a backend mutation changes the
    // TOML/YAML config file, not the IAM DB, so the sync would be a no-op
    // network round-trip. Handlers that DO mutate the IAM DB (users,
    // groups, external_auth, password) are the correct callers.
    let persist_path = super::config::active_config_path(&state);
    if let Err(e) = cfg.persist_to_file(&persist_path) {
        tracing::warn!("Failed to persist config to {}: {}", persist_path, e);
    }

    (
        axum::http::StatusCode::OK,
        Json(BackendMutationResponse {
            success: true,
            error: None,
            requires_restart: false,
        }),
    )
}
