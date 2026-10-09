// SPDX-License-Identifier: BUSL-1.1

//! Admin API for managing named backends (multi-backend routing).

use crate::api::admin::extract::AdminJson;
use crate::storage::StorageBackend;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::config::{BackendConfig, Config, NamedBackendConfig};

use super::{audit_log, AdminError, AdminState};

#[derive(Serialize)]
pub struct BackendListResponse {
    pub backends: Vec<super::config::BackendInfoResponse>,
    pub default_backend: Option<String>,
}

#[derive(Serialize)]
pub struct BucketBackendOriginResponse {
    pub name: String,
    pub creation_date: Option<String>,
    pub backend_name: Option<String>,
    pub backend_type: Option<String>,
    pub backend_endpoint: Option<String>,
    pub backend_region: Option<String>,
    pub backend_path: Option<String>,
    pub real_bucket: Option<String>,
    /// Present + non-null when this bucket's backend could not be listed
    /// (503 / throttle / connection). Carries the VERBATIM backend error so the
    /// UI can show why it's dark. The bucket is still listed (config-declared),
    /// flagged unavailable — never silently dropped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<String>,
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
            // A relative path resolves against the proxy's working directory,
            // which differs between systemd, Docker and a shell.
            let path = std::path::PathBuf::from(req.path.as_deref().unwrap_or("").trim());
            if !path.is_absolute() {
                return Err(
                    "Filesystem backend requires an absolute path, e.g. /var/lib/deltaglider/data"
                        .into(),
                );
            }
            Ok(BackendConfig::Filesystem { path })
        }
        "s3" => {
            // Validate credentials upfront (S3Backend::new will reject them later,
            // but the error is confusing; better to fail early with a clear message)
            if req.access_key_id.as_ref().is_none_or(|s| s.is_empty())
                || req.secret_access_key.as_ref().is_none_or(|s| s.is_empty())
            {
                return Err("S3 backend requires both access_key_id and secret_access_key".into());
            }
            // The SSRF policy the engine applies at rebuild, checked here so
            // a refused endpoint is the caller's error (400), not a 500.
            if let Some(ep) = req.endpoint.as_deref() {
                crate::storage::check_s3_endpoint(ep, false)?;
            }
            Ok(BackendConfig::S3 {
                session_token: None,
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
    // Stamp connectivity/auth health (boot probe + re-probe loop) — drives
    // the panel's health column. Absent = never probed (probe off).
    let health = state.s3_state.backend_health.snapshot();
    for b in &mut backends {
        b.capability = verdicts.get(&b.name).cloned();
        b.health = health.get(&b.name).cloned();
    }

    Json(BackendListResponse {
        backends,
        default_backend: cfg.default_backend.clone(),
    })
}

/// POST /api/admin/backends/:name/probe — "Test connection": run a live
/// connectivity/credentials probe against one backend RIGHT NOW, update the
/// health cache, and return the verdict. `:name` accepts a named backend or
/// the synthesized `"default"`.
pub async fn probe_backend(
    State(state): State<Arc<AdminState>>,
    axum::extract::Path(name): axum::extract::Path<String>,
    headers: HeaderMap,
) -> Result<Json<crate::coordination::health::HealthEntry>, AdminError> {
    let target = {
        let cfg = state.config.read().await;
        crate::coordination::health::probe_targets(&cfg)
            .into_iter()
            .find(|(n, _, _)| *n == name)
    };
    let Some((name, backend, fallback)) = target else {
        return Err(AdminError::not_found(format!("no backend named '{name}'")));
    };
    let verdict =
        crate::coordination::health::probe_backend_health(&backend, fallback.as_deref()).await;
    state.s3_state.backend_health.set(&name, &backend, verdict);
    let entry = state
        .s3_state
        .backend_health
        .snapshot()
        .remove(&name)
        .expect("entry just set");
    audit_log("backend_probe", "admin", &name, &headers);
    Ok(Json(entry))
}

/// GET /api/admin/buckets — list buckets with resolved backend origin.
///
/// The S3-compatible ListBuckets XML stays conservative; this JSON endpoint is
/// for the admin UI, which needs provider badges and tooltips. The browser still
/// merges this onto the SigV4-filtered ListBuckets result so bucket visibility
/// semantics do not change for non-admin IAM users.
pub async fn list_bucket_origins(
    State(state): State<Arc<AdminState>>,
) -> Result<Json<BucketOriginListResponse>, AdminError> {
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
    let default_backend = Some(cfg.default_backend_name());
    drop(cfg);

    let backend_by_name: std::collections::HashMap<_, _> = backend_infos
        .iter()
        .map(|backend| (backend.name.as_str(), backend))
        .collect();
    let engine = state.s3_state.engine.load();
    let bucket_list = engine
        .list_bucket_origins()
        .await
        .map_err(|e| AdminError::internal(format!("failed to list bucket origins: {e}")))?;

    // The coordination bucket is not a client bucket.
    let registry = engine.bucket_policy_registry();
    let buckets = bucket_list
        .into_iter()
        .filter(|bucket| !registry.is_reserved(&bucket.name))
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
                creation_date: bucket.creation_date.map(|d| d.to_rfc3339()),
                backend_name,
                backend_type: backend.map(|b| b.backend_type.clone()),
                backend_endpoint: backend.and_then(|b| b.endpoint.clone()),
                backend_region: backend.and_then(|b| b.region.clone()),
                backend_path: backend.and_then(|b| b.path.clone()),
                real_bucket: bucket.real_bucket,
                unavailable: bucket.unavailable,
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
    AdminJson(body): AdminJson<CreateBucketOnBackendRequest>,
) -> Result<Json<CreateBucketOnBackendResponse>, AdminError> {
    let bucket = body.name.trim().to_string();
    if bucket.is_empty() {
        return Err(AdminError::invalid("Bucket name cannot be empty"));
    }
    if let Some(reason) = state
        .s3_state
        .engine
        .load()
        .bucket_policy_registry()
        .reserved_bucket_reason(&bucket)
    {
        return Err(AdminError::forbidden(reason));
    }
    let backend_name = body.backend_name.trim().to_string();
    if backend_name.is_empty() {
        return Err(AdminError::invalid("backend_name cannot be empty"));
    }

    // Buckets are keyed by virtual bucket name, normalized lowercase.
    let bucket_key = bucket.to_ascii_lowercase();

    // An existing bucket is not created again: routing it to another backend
    // would hide every object it holds from clients.
    let existing = state
        .s3_state
        .engine
        .load()
        .list_bucket_origins()
        .await
        .map_err(|e| AdminError::internal(format!("failed to list buckets: {e}")))?;
    if existing
        .iter()
        .any(|b| b.name.eq_ignore_ascii_case(&bucket_key))
    {
        return Err(AdminError::conflict(format!(
            "bucket '{bucket_key}' already exists; to move it to backend '{backend_name}', \
             run a migrate job"
        )));
    }

    // Same write-capability gate as a config apply: a client-writable bucket
    // on a non-CAS backend under multi-instance makes the next boot exit(1).
    // Checked first for its 409; the transition runs the gate again under
    // the lock. No-op single-instance.
    let mut probe = state.config.read().await.clone();
    route_bucket(&mut probe, &bucket_key, &backend_name)?;
    if let Err(e) = crate::coordination::capability::hot_apply_capability_gate(
        &probe,
        &state.s3_state.backend_capabilities,
    )
    .await
    {
        return Err(AdminError::conflict(e));
    }
    drop(probe);

    use super::config::{HeldRefusal, Internal, InternalRefusal, OnPersistError};
    let applied = super::config::run_internal_held(
        &state,
        Internal {
            headers: &headers,
            action: "admin_create_bucket",
            target: &format!("{bucket_key}@{backend_name}"),
            on_persist_error: OnPersistError::Report,
        },
        std::future::ready(()),
        |cfg, _| route_bucket(cfg, &bucket_key, &backend_name),
        // Create using the SAME key the route is stored under (`bucket_key`,
        // lowercased). The route is keyed lowercase, so creating with the
        // original case would miss the explicit route in resolve_existing()
        // and fall through to the DEFAULT backend — silently creating the
        // bucket on the wrong backend. A failed create takes the route back.
        |_| async {
            state
                .s3_state
                .engine
                .load()
                .create_bucket(&bucket_key)
                .await
                .map_err(|e| AdminError::invalid(e.to_string()))
        },
    )
    .await;
    match applied {
        Ok(a) => {
            if let Err((path, e)) = a.persist {
                tracing::warn!("Failed to persist config to {}: {}", path, e);
            }
        }
        Err(HeldRefusal::Edit(e)) => return Err(e),
        Err(HeldRefusal::Pipeline(InternalRefusal::Transition(e))) => {
            return Err(AdminError::internal(format!(
                "Failed to rebuild engine: {e}"
            )))
        }
        Err(HeldRefusal::Pipeline(InternalRefusal::EnvReapply(e))) => {
            return Err(AdminError::internal(e))
        }
        Err(HeldRefusal::Pipeline(InternalRefusal::Invalid { status, error })) => {
            return Err(AdminError::status(status, error))
        }
        Err(HeldRefusal::Persist { .. }) => unreachable!("OnPersistError::Report"),
    }

    Ok(Json(CreateBucketOnBackendResponse {
        success: true,
        // Report the actual (normalized, lowercased) bucket name that was
        // created and routed — not the original-case input.
        bucket: bucket_key,
        backend_name,
    }))
}

/// Route `bucket_key` to `backend_name` in `cfg` (the edit of
/// [`create_bucket_on_backend`]).
fn route_bucket(cfg: &mut Config, bucket_key: &str, backend_name: &str) -> Result<(), AdminError> {
    let backend_exists = if cfg.backends.is_empty() {
        backend_name == "default"
    } else {
        cfg.backends.iter().any(|b| b.name == backend_name)
    };
    if !backend_exists {
        return Err(AdminError::invalid(format!(
            "Unknown backend '{}'",
            backend_name
        )));
    }
    let named = !cfg.backends.is_empty();
    let policy = cfg.buckets.entry(bucket_key.to_string()).or_default();
    policy.backend = named.then(|| backend_name.to_string());
    Ok(())
}

/// A backend mutation answer.
fn mutation(
    status: StatusCode,
    error: Option<String>,
) -> (StatusCode, Json<BackendMutationResponse>) {
    (
        status,
        Json(BackendMutationResponse {
            success: error.is_none(),
            error,
            requires_restart: false,
        }),
    )
}

/// Run a backend edit through the config write pipeline and answer it.
///
/// We do NOT call `trigger_config_sync` here. That helper uploads the
/// SQLCipher IAM database to S3 — a backend mutation changes the YAML
/// config file, not the IAM DB, so the sync would be a no-op network
/// round-trip.
async fn apply_backend_edit(
    state: &Arc<AdminState>,
    headers: &HeaderMap,
    action: &'static str,
    name: &str,
    ok: StatusCode,
    edit: impl FnOnce(&mut Config) -> Result<(), (StatusCode, String)>,
) -> (StatusCode, Json<BackendMutationResponse>) {
    use super::config::{HeldRefusal, Internal, InternalRefusal, OnPersistError};
    let applied = super::config::run_internal_held(
        state,
        Internal {
            headers,
            action,
            target: name,
            on_persist_error: OnPersistError::Report,
        },
        std::future::ready(()),
        |cfg, _| edit(cfg),
        |_| std::future::ready(Ok(())),
    )
    .await;
    match applied {
        Ok(a) => {
            if let Err((path, e)) = a.persist {
                tracing::warn!("Failed to persist config to {}: {}", path, e);
            }
            mutation(ok, None)
        }
        Err(HeldRefusal::Edit((status, error))) => mutation(status, Some(error)),
        Err(HeldRefusal::Pipeline(InternalRefusal::Transition(e))) => mutation(
            StatusCode::INTERNAL_SERVER_ERROR,
            Some(format!("Failed to rebuild engine: {}", e)),
        ),
        Err(HeldRefusal::Pipeline(InternalRefusal::EnvReapply(e))) => {
            mutation(StatusCode::INTERNAL_SERVER_ERROR, Some(e))
        }
        Err(HeldRefusal::Pipeline(InternalRefusal::Invalid { status, error })) => {
            mutation(status, Some(error))
        }
        Err(HeldRefusal::Persist { .. }) => unreachable!("OnPersistError::Report"),
    }
}

/// POST /api/admin/backends — add a new named backend.
pub async fn create_backend(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
    AdminJson(body): AdminJson<CreateBackendRequest>,
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

    // Health pre-commit gate (parity with apply/section-PUT — without it the
    // GUI's "Add backend" was the one door where an unpopulated secret or
    // dead endpoint still went live silently, the exact incident class this
    // gate exists for). Runs BEFORE the config write lock so a slow probe
    // never stalls S3 traffic. Success seeds the health cache so the badge
    // is green immediately. `DGP_BOOT_BACKEND_PROBE=off` skips, same as
    // everywhere else.
    if crate::coordination::health::boot_probe_mode()
        != crate::coordination::health::BootProbeMode::Off
    {
        let verdict =
            crate::coordination::health::probe_backend_health(&backend_config, None).await;
        if !verdict.is_healthy() {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(BackendMutationResponse {
                    success: false,
                    error: Some(format!(
                        "backend '{name}' failed its connection probe — {}. Fix the \
                         endpoint/credentials and retry (nothing was changed)",
                        verdict.cause()
                    )),
                    requires_restart: false,
                }),
            );
        }
        state
            .s3_state
            .backend_health
            .set(&name, &backend_config, verdict);
    }

    let set_default = body.set_default == Some(true);
    apply_backend_edit(
        &state,
        &headers,
        "backend_create",
        &name,
        StatusCode::CREATED,
        |cfg| {
            if cfg.backends.iter().any(|b| b.name == name) {
                return Err((
                    StatusCode::CONFLICT,
                    format!("Backend '{}' already exists", name),
                ));
            }
            cfg.backends.push(NamedBackendConfig {
                name: name.clone(),
                backend: backend_config,
                // New backends default to plaintext (mode: none) —
                // operators configure encryption after creation via the
                // Backends panel or a section-level PATCH.
                encryption: crate::config::BackendEncryptionConfig::default(),
            });
            if set_default || cfg.default_backend.is_none() {
                cfg.default_backend = Some(name.clone());
            }
            Ok(())
        },
    )
    .await
}

/// DELETE /api/admin/backends/:name — remove a named backend.
pub async fn delete_backend(
    State(state): State<Arc<AdminState>>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let refuse = |status, error: String| Err((status, error));
    apply_backend_edit(
        &state,
        &headers,
        "backend_delete",
        &name,
        StatusCode::OK,
        |cfg| {
            // Guard: the synthesised "default" entry surfaced by list_backends
            // when `cfg.backends` is empty is NOT a real named backend — it's
            // a virtual projection of `cfg.backend`. A DELETE on it would
            // otherwise fall into the generic "not found" branch below with
            // a misleading error; surface the specific shape issue instead.
            if name == "default" && cfg.backends.iter().all(|b| b.name != name) {
                return refuse(
                    StatusCode::CONFLICT,
                    "Cannot delete the synthesised 'default' backend — it represents the \
                     legacy singleton `cfg.backend`. To move off the singleton, add a named \
                     backend alongside it, then clear the singleton via section PUT on `storage`."
                        .into(),
                );
            }
            if !cfg.backends.iter().any(|b| b.name == name) {
                return refuse(
                    StatusCode::NOT_FOUND,
                    format!("Backend '{}' not found", name),
                );
            }
            if cfg.default_backend.as_deref() == Some(&name) {
                return refuse(
                    StatusCode::CONFLICT,
                    "Cannot delete the default backend. Assign a new default first.".into(),
                );
            }
            let routed: Vec<String> = cfg
                .buckets
                .iter()
                .filter(|(_, p)| p.backend.as_deref() == Some(&name))
                .map(|(bucket, _)| bucket.clone())
                .collect();
            if !routed.is_empty() {
                return refuse(
                    StatusCode::CONFLICT,
                    format!(
                        "Cannot delete '{}': buckets [{}] route to it. Re-route them first.",
                        name,
                        routed.join(", ")
                    ),
                );
            }
            cfg.backends.retain(|b| b.name != name);
            Ok(())
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fs_request(path: Option<&str>) -> CreateBackendRequest {
        CreateBackendRequest {
            name: "local-disk".into(),
            backend_type: "filesystem".into(),
            path: path.map(str::to_string),
            endpoint: None,
            region: None,
            force_path_style: None,
            access_key_id: None,
            secret_access_key: None,
            set_default: None,
        }
    }

    #[test]
    fn filesystem_backend_requires_an_absolute_path() {
        for bad in [None, Some(""), Some("./data"), Some("data/archive")] {
            assert!(build_backend_config(&fs_request(bad)).is_err(), "{bad:?}");
        }
        let ok = build_backend_config(&fs_request(Some(" /srv/dg "))).unwrap();
        assert!(
            matches!(ok, BackendConfig::Filesystem { ref path } if path == std::path::Path::new("/srv/dg"))
        );
    }

    /// The list shows an S3 backend's key id (an identifier) and never its
    /// secret.
    #[test]
    fn the_backend_list_shows_the_key_id_not_the_secret() {
        let named = crate::config::NamedBackendConfig {
            name: "hetzner-fsn1".into(),
            backend: BackendConfig::S3 {
                session_token: None,
                endpoint: Some("https://fsn1.example".into()),
                region: "eu-central-1".into(),
                force_path_style: true,
                access_key_id: Some("AKHETZNER".into()),
                secret_access_key: Some("hetzner-secret".into()),
                allow_local: false,
            },
            encryption: Default::default(),
        };
        let json = serde_json::to_string(&super::super::config::BackendInfoResponse::from(&named))
            .unwrap();
        assert!(json.contains(r#""access_key_id":"AKHETZNER""#), "{json}");
        assert!(!json.contains("hetzner-secret"), "{json}");
    }
}

/// Objects the legacy-key scan HEADs when the request names no `limit`.
const LEGACY_SCAN_DEFAULT_LIMIT: u64 = 10_000;
/// Upper bound on `limit`: one request must stay inside the request timeout.
const LEGACY_SCAN_MAX_LIMIT: u64 = 1_000_000;
/// HEADs in flight at once during the scan.
const LEGACY_SCAN_CONCURRENCY: usize = 16;
/// Keys listed per page, and object keys reported as examples.
const LEGACY_SCAN_PAGE: u32 = 1000;
const LEGACY_SCAN_EXAMPLES: usize = 10;

#[derive(Deserialize)]
pub struct LegacyKeyUsageQuery {
    pub limit: Option<u64>,
}

/// `GET /backends/:name/legacy-key-usage` — how many objects and delta
/// references of this backend still carry the legacy key id. The count is
/// EXACT (every object and reference is HEADed) until `limit` objects are
/// scanned; then the scan stops and `complete` is false.
#[derive(Serialize, Debug, Default, PartialEq)]
pub struct LegacyKeyUsage {
    pub backend: String,
    /// The id the legacy key stamps (`None`: no legacy key configured).
    pub legacy_key_id: Option<String>,
    /// Buckets that route to this backend (all scanned when `complete`).
    pub buckets: Vec<String>,
    pub objects_scanned: u64,
    pub objects_under_legacy_key: u64,
    pub references_scanned: u64,
    pub references_under_legacy_key: u64,
    /// Up to 10 `bucket/key` names under the legacy key.
    pub examples: Vec<String>,
    /// Buckets or objects the scan could not read (capped at 10).
    pub errors: Vec<String>,
    pub complete: bool,
    pub limit: u64,
    pub safe_to_clear: bool,
}

impl LegacyKeyUsage {
    /// Pure: clearing the legacy key cannot make an object unreadable only
    /// when the scan saw everything, read everything, and found nothing
    /// under the legacy key id.
    pub fn safe_to_clear(&self) -> bool {
        self.legacy_key_id.is_some()
            && self.complete
            && self.errors.is_empty()
            && self.objects_under_legacy_key == 0
            && self.references_under_legacy_key == 0
    }

    fn error(&mut self, e: String) {
        if self.errors.len() < LEGACY_SCAN_EXAMPLES {
            self.errors.push(e);
        }
    }
}

pub async fn legacy_key_usage(
    State(state): State<Arc<AdminState>>,
    Path(name): Path<String>,
    crate::api::admin::extract::AdminQuery(q): crate::api::admin::extract::AdminQuery<
        LegacyKeyUsageQuery,
    >,
) -> Result<Json<LegacyKeyUsage>, AdminError> {
    use futures::StreamExt;

    let limit = q
        .limit
        .unwrap_or(LEGACY_SCAN_DEFAULT_LIMIT)
        .clamp(1, LEGACY_SCAN_MAX_LIMIT);
    let engine = state.s3_state.engine.load().clone();
    let origins = engine
        .list_bucket_origins()
        .await
        .map_err(|e| AdminError::internal(format!("failed to list buckets: {e}")))?;
    let mut usage = LegacyKeyUsage {
        backend: name.clone(),
        limit,
        ..Default::default()
    };
    {
        let cfg = state.config.read().await;
        let enc = cfg
            .backend_encryption_by_name(&name)
            .ok_or(AdminError::not_found(format!("no backend named '{name}'")))?;
        usage.legacy_key_id = crate::deltaglider::effective_legacy_key_id(&name, enc);
        let registry = engine.bucket_policy_registry();
        for b in &origins {
            if registry.is_reserved(&b.name) {
                continue;
            }
            if cfg
                .effective_backend_for_bucket(&b.name)
                .is_some_and(|(n, _)| n == name)
            {
                if let Some(e) = &b.unavailable {
                    usage.error(format!("{}: {e}", b.name));
                }
                usage.buckets.push(b.name.clone());
            }
        }
    }
    let Some(kid) = usage.legacy_key_id.clone() else {
        usage.complete = true;
        return Ok(Json(usage));
    };

    'buckets: for bucket in usage.buckets.clone() {
        // Delta references first: one legacy reference breaks every delta
        // in its deltaspace, so they count even when the object budget ends.
        match engine.storage().list_deltaspaces(&bucket).await {
            Ok(prefixes) => {
                for prefix in prefixes {
                    let storage = engine.storage();
                    match storage.has_reference(&bucket, &prefix).await {
                        Ok(false) => continue,
                        Ok(true) => {}
                        Err(e) => {
                            usage.error(format!("{bucket}/{prefix}/.dg/reference.bin: {e}"));
                            continue;
                        }
                    }
                    usage.references_scanned += 1;
                    match storage.get_reference_metadata(&bucket, &prefix).await {
                        Ok(m) => {
                            if crate::maintenance::stamped_with_key_id(&m.user_metadata, &kid) {
                                usage.references_under_legacy_key += 1;
                            }
                        }
                        Err(e) => usage.error(format!("{bucket}/{prefix}/.dg/reference.bin: {e}")),
                    }
                }
            }
            Err(e) => usage.error(format!("{bucket}: cannot list delta references: {e}")),
        }

        let mut token: Option<String> = None;
        loop {
            let page = match engine
                .list_objects(&bucket, "", None, LEGACY_SCAN_PAGE, token.as_deref(), false)
                .await
            {
                Ok(p) => p,
                Err(e) => {
                    usage.error(format!("{bucket}: cannot list objects: {e}"));
                    continue 'buckets;
                }
            };
            let budget = (limit - usage.objects_scanned) as usize;
            let keys: Vec<String> = page
                .objects
                .into_iter()
                .map(|(k, _)| k)
                .filter(|k| !k.ends_with('/'))
                .collect();
            let over_budget = keys.len() > budget;
            let heads: Vec<_> = futures::stream::iter(keys.into_iter().take(budget))
                .map(|key| {
                    let engine = engine.clone();
                    let bucket = bucket.clone();
                    async move {
                        let r = engine.head(&bucket, &key).await;
                        (key, r)
                    }
                })
                .buffer_unordered(LEGACY_SCAN_CONCURRENCY)
                .collect()
                .await;
            for (key, r) in heads {
                usage.objects_scanned += 1;
                match r {
                    Ok(m) if crate::maintenance::stamped_with_key_id(&m.user_metadata, &kid) => {
                        usage.objects_under_legacy_key += 1;
                        if usage.examples.len() < LEGACY_SCAN_EXAMPLES {
                            usage.examples.push(format!("{bucket}/{key}"));
                        }
                    }
                    Ok(_) => {}
                    Err(e) => usage.error(format!("{bucket}/{key}: {e}")),
                }
            }
            if over_budget || (page.is_truncated && usage.objects_scanned >= limit) {
                // Budget spent with objects left: the count is a lower bound.
                usage.complete = false;
                usage.safe_to_clear = false;
                return Ok(Json(usage));
            }
            match (page.is_truncated, page.next_continuation_token) {
                (true, Some(t)) => token = Some(t),
                _ => break,
            }
        }
    }
    usage.complete = true;
    usage.safe_to_clear = usage.safe_to_clear();
    Ok(Json(usage))
}

#[cfg(test)]
mod legacy_usage_tests {
    use super::LegacyKeyUsage;

    #[test]
    fn safe_to_clear_needs_a_complete_clean_scan() {
        let clean = LegacyKeyUsage {
            legacy_key_id: Some("kid".into()),
            complete: true,
            ..Default::default()
        };
        assert!(clean.safe_to_clear());
        let cases = [
            LegacyKeyUsage {
                legacy_key_id: None,
                ..clean_copy(&clean)
            },
            LegacyKeyUsage {
                complete: false,
                ..clean_copy(&clean)
            },
            LegacyKeyUsage {
                errors: vec!["releases: 503".into()],
                ..clean_copy(&clean)
            },
            LegacyKeyUsage {
                objects_under_legacy_key: 1,
                ..clean_copy(&clean)
            },
            LegacyKeyUsage {
                references_under_legacy_key: 1,
                ..clean_copy(&clean)
            },
        ];
        for c in cases {
            assert!(!c.safe_to_clear(), "{c:?}");
        }
    }

    fn clean_copy(u: &LegacyKeyUsage) -> LegacyKeyUsage {
        LegacyKeyUsage {
            legacy_key_id: u.legacy_key_id.clone(),
            complete: u.complete,
            ..Default::default()
        }
    }
}
