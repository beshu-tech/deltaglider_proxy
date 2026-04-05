//! Admin API for managing named backends (multi-backend routing).

use axum::extract::{Path, State};
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::config::{BackendConfig, NamedBackendConfig};
use crate::deltaglider::DynEngine;

use super::{trigger_config_sync, AdminState};

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
            let path = req
                .path
                .as_deref()
                .unwrap_or("./data")
                .to_string();
            Ok(BackendConfig::Filesystem {
                path: std::path::PathBuf::from(path),
            })
        }
        "s3" => Ok(BackendConfig::S3 {
            endpoint: req.endpoint.clone(),
            region: req.region.clone().unwrap_or_else(|| "us-east-1".to_string()),
            force_path_style: req.force_path_style.unwrap_or(true),
            access_key_id: req.access_key_id.clone(),
            secret_access_key: req.secret_access_key.clone(),
        }),
        other => Err(format!("Unknown backend type: '{other}'. Must be 'filesystem' or 's3'.")),
    }
}

/// GET /api/admin/backends — list all named backends.
pub async fn list_backends(State(state): State<Arc<AdminState>>) -> impl IntoResponse {
    let cfg = state.config.read().await;
    let backends = cfg
        .backends
        .iter()
        .map(|named| {
            let (bt, path, endpoint, region, fps, has_creds) = match &named.backend {
                BackendConfig::Filesystem { path } => {
                    ("filesystem", Some(path.display().to_string()), None, None, None, false)
                }
                BackendConfig::S3 {
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
            super::config::BackendInfoResponse {
                name: named.name.clone(),
                backend_type: bt.to_string(),
                path,
                endpoint,
                region,
                force_path_style: fps,
                has_credentials: has_creds,
            }
        })
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
    });

    if body.set_default == Some(true) || cfg.default_backend.is_none() {
        cfg.default_backend = Some(name.clone());
    }

    // Rebuild engine
    match DynEngine::new(&cfg, Some(state.s3_state.metrics.clone())).await {
        Ok(new_engine) => {
            state.s3_state.engine.store(Arc::new(new_engine));
            tracing::info!("Backend '{}' added, engine rebuilt", name);
        }
        Err(e) => {
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
    }

    if let Err(e) = cfg.persist_to_file("deltaglider_proxy.toml") {
        tracing::warn!("Failed to persist config: {}", e);
    }
    trigger_config_sync(&state);

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
                error: Some("Cannot delete the default backend. Assign a new default first.".into()),
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

    // Rebuild engine
    match DynEngine::new(&cfg, Some(state.s3_state.metrics.clone())).await {
        Ok(new_engine) => {
            state.s3_state.engine.store(Arc::new(new_engine));
            tracing::info!("Backend '{}' removed, engine rebuilt", name);
        }
        Err(e) => {
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
    }

    if let Err(e) = cfg.persist_to_file("deltaglider_proxy.toml") {
        tracing::warn!("Failed to persist config: {}", e);
    }
    trigger_config_sync(&state);

    (
        axum::http::StatusCode::OK,
        Json(BackendMutationResponse {
            success: true,
            error: None,
            requires_restart: false,
        }),
    )
}
