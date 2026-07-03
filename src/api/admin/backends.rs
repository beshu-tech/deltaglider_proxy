// SPDX-License-Identifier: GPL-3.0-only

//! Admin API for managing named backends (multi-backend routing).

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::config::{BackendConfig, NamedBackendConfig};

use super::{audit_log, AdminState};

#[derive(Serialize)]
pub struct BackendListResponse {
    pub backends: Vec<super::config::BackendInfoResponse>,
    pub default_backend: Option<String>,
}

#[derive(Serialize)]
pub struct BucketBackendOriginResponse {
    pub name: String,
    pub creation_date: String,
    pub backend_name: Option<String>,
    pub backend_type: Option<String>,
    pub backend_endpoint: Option<String>,
    pub backend_region: Option<String>,
    pub backend_path: Option<String>,
    pub real_bucket: Option<String>,
}

#[derive(Serialize)]
pub struct BucketOriginListResponse {
    pub buckets: Vec<BucketBackendOriginResponse>,
}

#[derive(Deserialize)]
pub struct CreateBucketOnBackendRequest {
    pub name: String,
    pub backend_name: String,
}

#[derive(Serialize)]
pub struct CreateBucketOnBackendResponse {
    pub success: bool,
    pub bucket: String,
    pub backend_name: String,
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
                allow_local: false,
            })
        }
        other => Err(format!(
            "Unknown backend type: '{other}'. Must be 'filesystem' or 's3'."
        )),
    }
}

/// GET /api/admin/backends — list all named backends.
///
/// When `cfg.backends` is empty but `cfg.backend` holds a configured
/// singleton, we synthesise a `"default"` entry (`is_synthesized:
/// true`) so the admin UI's Backends panel reflects the operator's
/// working backend regardless of whether their YAML uses the legacy
/// singleton `backend:` shape or the named-list `backends:` shape.
///
/// Without this synthesis the panel shows "no named backends" while
/// the proxy is happily serving from the configured singleton — an
/// inconsistency operators have reported as a phantom "where did my
/// backend go?" moment. The same synthesis also lives in
/// `GET /config`'s `backends[]` projection; both endpoints now agree.
pub async fn list_backends(State(state): State<Arc<AdminState>>) -> impl IntoResponse {
    let cfg = state.config.read().await;
    let mut backends: Vec<super::config::BackendInfoResponse> = if cfg.backends.is_empty() {
        vec![super::config::BackendInfoResponse::synthesized_default(
            &cfg,
        )]
    } else {
        cfg.backends
            .iter()
            .map(super::config::BackendInfoResponse::from)
            .collect()
    };
    // Stamp the startup capability-gate verdicts (guard B) so the panel can
    // render a per-backend conditional-write banner. Absent = not assessed.
    let verdicts = state.s3_state.backend_capabilities.snapshot();
    for b in &mut backends {
        b.capability = verdicts.get(&b.name).cloned();
    }

    Json(BackendListResponse {
        backends,
        default_backend: cfg.default_backend.clone(),
    })
}

/// GET /api/admin/buckets — list buckets with resolved backend origin.
///
/// The S3-compatible ListBuckets XML stays conservative; this JSON endpoint is
/// for the admin UI, which needs provider badges and tooltips. The browser still
/// merges this onto the SigV4-filtered ListBuckets result so bucket visibility
/// semantics do not change for non-admin IAM users.
pub async fn list_bucket_origins(
    State(state): State<Arc<AdminState>>,
) -> Result<Json<BucketOriginListResponse>, (StatusCode, String)> {
    let cfg = state.config.read().await;
    let backend_infos: Vec<super::config::BackendInfoResponse> = if cfg.backends.is_empty() {
        vec![super::config::BackendInfoResponse::synthesized_default(
            &cfg,
        )]
    } else {
        cfg.backends
            .iter()
            .map(super::config::BackendInfoResponse::from)
            .collect()
    };
    let default_backend = cfg
        .default_backend
        .clone()
        .or_else(|| backend_infos.first().map(|b| b.name.clone()));
    drop(cfg);

    let backend_by_name: std::collections::HashMap<_, _> = backend_infos
        .iter()
        .map(|backend| (backend.name.as_str(), backend))
        .collect();
    let bucket_list = state
        .s3_state
        .engine
        .load()
        .list_bucket_origins()
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to list bucket origins: {e}"),
            )
        })?;

    let buckets = bucket_list
        .into_iter()
        .map(|bucket| {
            let backend_name = bucket
                .backend_name
                .clone()
                .or_else(|| default_backend.clone());
            let backend = backend_name
                .as_deref()
                .and_then(|name| backend_by_name.get(name).copied());
            BucketBackendOriginResponse {
                name: bucket.name,
                creation_date: bucket.creation_date.to_rfc3339(),
                backend_name,
                backend_type: backend.map(|b| b.backend_type.clone()),
                backend_endpoint: backend.and_then(|b| b.endpoint.clone()),
                backend_region: backend.and_then(|b| b.region.clone()),
                backend_path: backend.and_then(|b| b.path.clone()),
                real_bucket: bucket.real_bucket,
            }
        })
        .collect();

    Ok(Json(BucketOriginListResponse { buckets }))
}

/// POST /api/admin/buckets — create a bucket pinned to a named backend.
///
/// This is intentionally admin-only and separate from the public S3
/// `PUT /{bucket}` API. S3 has no portable "backend hint" concept; the admin
/// UI needs an explicit control when multiple backends exist.
pub async fn create_bucket_on_backend(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
    Json(body): Json<CreateBucketOnBackendRequest>,
) -> Result<Json<CreateBucketOnBackendResponse>, (StatusCode, String)> {
    let bucket = body.name.trim().to_string();
    if bucket.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Bucket name cannot be empty".into(),
        ));
    }
    let backend_name = body.backend_name.trim().to_string();
    if backend_name.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            "backend_name cannot be empty".into(),
        ));
    }

    let mut cfg = state.config.write().await;

    let backend_exists = if cfg.backends.is_empty() {
        backend_name == "default"
    } else {
        cfg.backends.iter().any(|b| b.name == backend_name)
    };
    if !backend_exists {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("Unknown backend '{}'", backend_name),
        ));
    }

    // Buckets are keyed by virtual bucket name, normalized lowercase.
    let bucket_key = bucket.to_ascii_lowercase();
    let old_policy = cfg.buckets.get(&bucket_key).cloned();
    let mut policy = old_policy.clone().unwrap_or_default();
    policy.backend = if cfg.backends.is_empty() {
        None
    } else {
        Some(backend_name.clone())
    };
    cfg.buckets.insert(bucket_key.clone(), policy);

    if let Err(e) = super::config::rebuild_engine(
        &state,
        &cfg,
        &format!(
            "Bucket '{}' routed to backend '{}', engine rebuilt",
            bucket, backend_name
        ),
    )
    .await
    {
        if let Some(previous) = old_policy {
            cfg.buckets.insert(bucket_key, previous);
        } else {
            cfg.buckets.remove(&bucket_key);
        }
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to rebuild engine: {e}"),
        ));
    }

    // Create using the SAME key the route is stored under (`bucket_key`,
    // lowercased). The route is keyed lowercase, so creating with the original
    // case would miss the explicit route in resolve_existing() and fall through
    // to the DEFAULT backend — silently creating the bucket on the wrong backend
    // (and, for an uppercase name + S3 default, failing with InvalidBucketName).
    if let Err(e) = state
        .s3_state
        .engine
        .load()
        .create_bucket(&bucket_key)
        .await
    {
        // Roll back routing if create failed (e.g. already exists / backend error).
        if let Some(previous) = old_policy {
            cfg.buckets.insert(bucket_key.clone(), previous);
        } else {
            cfg.buckets.remove(&bucket_key);
        }
        let _ = super::config::rebuild_engine(
            &state,
            &cfg,
            &format!(
                "Bucket create failed for '{}', reverted backend routing",
                bucket
            ),
        )
        .await;
        return Err((StatusCode::BAD_REQUEST, e.to_string()));
    }

    let persist_path = super::config::active_config_path(&state);
    if let Err(e) = cfg.persist_to_file(&persist_path) {
        tracing::warn!("Failed to persist config to {}: {}", persist_path, e);
    }
    drop(cfg);

    audit_log(
        "admin_create_bucket",
        "admin",
        &format!("{bucket_key}@{backend_name}"),
        &headers,
    );

    Ok(Json(CreateBucketOnBackendResponse {
        success: true,
        // Report the actual (normalized, lowercased) bucket name that was
        // created and routed — not the original-case input.
        bucket: bucket_key,
        backend_name,
    }))
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
    // or the search-path walk. Hardcoding a CWD-relative default here
    // used to silently redirect admin-API writes to a stale location when
    // the operator had launched with `--config /etc/dgp/config.yaml`,
    // producing a latent "my backend disappears on restart" bug.
    //
    // Note: we do NOT call `trigger_config_sync` here. That helper uploads
    // the SQLCipher IAM database to S3 — a backend mutation changes the
    // YAML config file, not the IAM DB, so the sync would be a no-op
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

    // Guard: the synthesised "default" entry surfaced by list_backends
    // when `cfg.backends` is empty is NOT a real named backend — it's
    // a virtual projection of `cfg.backend`. A DELETE on it would
    // otherwise fall into the generic "not found" branch below with
    // a misleading error; surface the specific shape issue instead.
    if name == "default" && cfg.backends.iter().all(|b| b.name != name) {
        return (
            axum::http::StatusCode::CONFLICT,
            Json(BackendMutationResponse {
                success: false,
                error: Some(
                    "Cannot delete the synthesised 'default' backend — it represents the legacy \
                     singleton `cfg.backend`. To move off the singleton, add a named backend \
                     alongside it, then clear the singleton via section PUT on `storage`."
                        .into(),
                ),
                requires_restart: false,
            }),
        );
    }

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
    // or the search-path walk. Hardcoding a CWD-relative default here
    // used to silently redirect admin-API writes to a stale location when
    // the operator had launched with `--config /etc/dgp/config.yaml`,
    // producing a latent "my backend disappears on restart" bug.
    //
    // Note: we do NOT call `trigger_config_sync` here. That helper uploads
    // the SQLCipher IAM database to S3 — a backend mutation changes the
    // YAML config file, not the IAM DB, so the sync would be a no-op
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
