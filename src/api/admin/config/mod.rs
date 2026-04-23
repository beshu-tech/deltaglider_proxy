//! Admin-API config surface, split into submodules along four genuine
//! seams. Each submodule owns its handlers AND the request/response
//! types those handlers produce — the only cross-module coupling is via
//! the shared helpers in this file (`rebuild_engine`,
//! `apply_config_transition`, `rebuild_bucket_derived_snapshots`,
//! `active_config_path`).
//!
//! | Submodule            | Endpoints                                                             | Persona          |
//! |----------------------|-----------------------------------------------------------------------|------------------|
//! | [`field_level`]      | `GET/PUT /api/admin/config`                                            | Admin GUI forms  |
//! | [`document_level`]   | `GET /config/export`, `/defaults`, `POST /config/validate`, `/apply`   | GitOps operators |
//! | [`password`]         | `PUT /api/admin/password`, `POST /api/admin/recover-db`                | Security-critical|
//! | [`trace`]            | `POST /api/admin/config/trace`                                         | Admission debug  |
//!
//! `test_s3_connection` (used by the GUI to probe a candidate backend
//! before saving it) lives alongside the shared helpers here — it's
//! small, stateless, and doesn't obviously belong under any submodule.
//!
//! [`SectionName`] and [`unknown_section_error`] live here too so the
//! three submodules that accept a `section` parameter
//! (`section_level`, `document_level::export_config`,
//! `document_level::config_defaults`) agree on the wire-level name
//! spelling and 404 message.

pub mod document_level;
pub mod field_level;
pub mod password;
pub mod section_level;
pub mod trace;

/// Names of the four sections the admin API understands. Canonical
/// home for the enum + its string-wire spelling — any consumer that
/// accepts a `section` parameter must parse through here, never a
/// local string-match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SectionName {
    Admission,
    Access,
    Storage,
    Advanced,
}

impl SectionName {
    /// Parse a section name off the wire. Returns `None` on unknown
    /// input — caller turns that into a 404 via
    /// [`unknown_section_error`].
    pub(super) fn parse(s: &str) -> Option<Self> {
        match s {
            "admission" => Some(Self::Admission),
            "access" => Some(Self::Access),
            "storage" => Some(Self::Storage),
            "advanced" => Some(Self::Advanced),
            _ => None,
        }
    }

    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Admission => "admission",
            Self::Access => "access",
            Self::Storage => "storage",
            Self::Advanced => "advanced",
        }
    }
}

/// Standard 404 body for section-scoped endpoints. Single source of
/// truth for the error text — four handlers share the same 404
/// trigger, and the message lists the valid section names exactly
/// once.
pub(super) fn unknown_section_error(name: &str) -> String {
    format!(
        "unknown section '{}'; valid names: admission, access, storage, advanced",
        name
    )
}

pub use document_level::{
    apply_config_doc, config_defaults, export_config, validate_config_doc, ConfigApplyResponse,
    ConfigDocumentRequest, ConfigValidateResponse,
};
pub use field_level::{
    get_config, update_config, BackendInfoResponse, ConfigResponse, ConfigUpdateRequest,
    ConfigUpdateResponse,
};
pub use password::{change_password, recover_db, PasswordChangeRequest, PasswordChangeResponse};
// `sync_now` + `SyncNowResponse` are defined inline further down in
// this module (alongside `test_s3_connection`); re-export them here
// for parent modules that pull the whole `config` surface up.
pub use section_level::{get_section, put_section, validate_section, SectionApplyResponse};
pub use trace::{trace_config, trace_config_get, TraceRequest, TraceResolved, TraceResponse};

use axum::extract::State;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

use crate::deltaglider::DynEngine;
use crate::iam::{AuthConfig, IamState};

use super::AdminState;

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

/// Rebuild every hot-swappable structure derived from bucket-level config.
/// Today that's the public-prefix snapshot *and* the admission chain; both
/// are derived from the same input data (`config.buckets` + admission
/// blocks) and must stay in sync across config changes. Call this from
/// every handler that mutates `state.config.buckets` or
/// `state.config.admission_blocks` — the helper exists to prevent one
/// site from drifting behind the other as new derived snapshots are added.
pub(super) fn rebuild_bucket_derived_snapshots(
    state: &Arc<AdminState>,
    buckets: &std::collections::BTreeMap<String, crate::bucket_policy::BucketPolicyConfig>,
    operator_blocks: &[crate::admission::AdmissionBlockSpec],
) {
    let new_prefix_snapshot = crate::bucket_policy::PublicPrefixSnapshot::from_config(buckets);
    state
        .public_prefix_snapshot
        .store(std::sync::Arc::new(new_prefix_snapshot));

    // Compile operator-authored blocks into runtime form and merge
    // with synthesised public-prefix blocks. `from_config_parts`
    // handles ordering (operator blocks first) and logs per-block
    // warnings for unknown config_flag predicates. Phase 3b.2.b
    // upgraded this from a silent-no-op to live dispatch.
    let new_chain = crate::admission::AdmissionChain::from_config_parts(buckets, operator_blocks);
    state.admission_chain.store(std::sync::Arc::new(new_chain));
}

/// Decision on whether two `Config` snapshots require the storage engine to
/// be rebuilt. An engine rebuild constructs the backend clients, warms the
/// reference cache, and is the most expensive side effect we can apply —
/// so this check enumerates the fields the engine actually reads and lets
/// the caller skip the work when nothing relevant changed.
///
/// Any field listed here must be tested for equality. Anything outside
/// (listen_addr, log_level, SigV4 creds, bootstrap hash, tls config,
/// config_sync_bucket, defaults_version) is engine-orthogonal.
fn engine_affecting_fields_changed(
    old: &crate::config::Config,
    new: &crate::config::Config,
) -> bool {
    old.backend != new.backend
        || old.backend_encryption != new.backend_encryption
        || old.backends != new.backends
        || old.default_backend != new.default_backend
        || old.buckets != new.buckets
        || old.max_object_size != new.max_object_size
        || old.cache_size_mb != new.cache_size_mb
        || old.max_delta_ratio != new.max_delta_ratio
        || old.metadata_cache_mb != new.metadata_cache_mb
}

/// Side effects of transitioning the runtime config from `old` to `new`.
///
/// This is the **single source of truth** for what happens when the admin
/// config changes — previously duplicated (and drifting) between the
/// field-level PATCH path (`update_config`) and the document-level APPLY
/// path (`apply_config_doc`). Both now build a prospective `new_cfg` and
/// hand off to this helper under their write lock.
///
/// The helper:
/// 1. Rebuilds the storage engine when any engine-affecting field changed
///    (see [`engine_affecting_fields_changed`]) — rollback is the caller's
///    responsibility; on failure the helper returns `Err` without touching
///    any other state.
/// 2. Hot-reloads the log filter when `log_level` changed and parses.
/// 3. Swaps the IAM state when the legacy SigV4 credentials changed —
///    unless the deployment is in full IAM-mode, in which case the YAML/
///    patch values are ignored (the DB is the source of truth) and a
///    warning is emitted so operators can see their legacy edits had no
///    effect.
/// 4. Rebuilds all bucket-derived snapshots (public-prefix + admission
///    chain) when `buckets` changed.
/// 5. Emits `requires_restart` + warnings for fields that cannot be
///    hot-applied (`listen_addr`, `cache_size_mb`).
///
/// Returns `(warnings, requires_restart)`. Warnings include both the
/// "restart required" notices and any hot-reload failures (invalid log
/// filter strings, IAM-mode-blocked credential edits). Callers can
/// extend the returned vec with handler-specific warnings before
/// composing their response.
///
/// ## Lock invariant — do not weaken
///
/// Every caller MUST hold the `state.config` write lock for the full
/// duration of the transition *and* the subsequent `*cfg = new` swap.
/// The helper stores the rebuilt engine into `state.s3_state.engine`
/// before returning; releasing the config write lock between engine-swap
/// and config-swap would expose a window where a concurrent
/// `state.config.read()` sees the *old* config with the *new* engine
/// serving requests. Any refactor that moves the await points must
/// preserve "both happen under one write lock" as the atomicity barrier.
pub(crate) async fn apply_config_transition(
    state: &Arc<AdminState>,
    old_cfg: &crate::config::Config,
    new_cfg: &crate::config::Config,
) -> Result<(Vec<String>, bool), String> {
    let mut warnings = Vec::new();
    let mut requires_restart = false;

    // 1. Engine rebuild — only on fields the engine reads. Bail early on
    //    failure so callers can roll back their in-memory mutation
    //    without having side-effected anything downstream.
    if engine_affecting_fields_changed(old_cfg, new_cfg) {
        rebuild_engine(state, new_cfg, "Engine rebuilt on config transition").await?;
    }

    // 2. Log-level hot reload. Invalid filter is a warning, not an error —
    //    this matches the existing behavior where an invalid log level
    //    keeps the old filter but doesn't block the config change.
    if old_cfg.log_level != new_cfg.log_level {
        match new_cfg.log_level.parse::<EnvFilter>() {
            Ok(new_filter) => {
                if let Err(e) = state.log_reload.reload(new_filter) {
                    warnings.push(format!("Failed to reload log filter: {}", e));
                } else {
                    tracing::info!("Log level changed to: {}", new_cfg.log_level);
                }
            }
            Err(e) => {
                warnings.push(format!("Invalid log filter '{}': {}", new_cfg.log_level, e));
            }
        }
    }

    // 3. Legacy SigV4 credentials — hot-swap the IamState, but only when
    //    the deployment is NOT in full IAM mode. In IAM mode the IamIndex
    //    is authoritative; overwriting it here would silently destroy
    //    every per-user credential (which is how the old field-level
    //    patch path discovered this rule, the hard way).
    if old_cfg.access_key_id != new_cfg.access_key_id
        || old_cfg.secret_access_key != new_cfg.secret_access_key
    {
        let current_iam = state.iam_state.load();
        if matches!(&**current_iam, IamState::Iam(_)) {
            warnings.push(
                "Legacy credentials changed but IAM mode is active — edit ignored. Manage users via the Users panel."
                    .to_string(),
            );
        } else {
            let new_state = if let (Some(ref k), Some(ref s)) =
                (&new_cfg.access_key_id, &new_cfg.secret_access_key)
            {
                IamState::Legacy(AuthConfig {
                    access_key_id: k.clone(),
                    secret_access_key: s.clone(),
                })
            } else {
                IamState::Disabled
            };
            state.iam_state.store(Arc::new(new_state));
            tracing::info!(
                "Auth credentials hot-reloaded (auth enabled: {})",
                new_cfg.auth_enabled()
            );
        }
    }

    // 4. Bucket-derived snapshots — public prefix + admission chain.
    //    Rebuild iff the bucket policy set OR the operator-authored
    //    admission blocks changed; changing other fields doesn't affect
    //    these.
    if old_cfg.buckets != new_cfg.buckets || old_cfg.admission_blocks != new_cfg.admission_blocks {
        rebuild_bucket_derived_snapshots(state, &new_cfg.buckets, &new_cfg.admission_blocks);
    }

    // 4b. IAM-mode transitions. Declarative ↔ gui flips are security-
    //     meaningful — the declarative-mode "escape hatch" is a pair of
    //     `/apply` calls (flip to gui, mutate, flip back) and auditors
    //     need to see it distinctly from other config changes. Emits a
    //     warn-level log line with the direction so SIEM / log-review
    //     workflows can alert on it.
    if old_cfg.iam_mode != new_cfg.iam_mode {
        tracing::warn!(
            target: "deltaglider_proxy::config",
            from = ?old_cfg.iam_mode,
            to = ?new_cfg.iam_mode,
            "[config] access.iam_mode changed: {:?} → {:?}. In declarative mode the admin-\
             API IAM mutation routes return 403; a flip to `gui` restores them. Review the \
             subsequent apply_config audit log entries to see what mutations followed.",
            old_cfg.iam_mode,
            new_cfg.iam_mode
        );
    }

    // 5. Restart-required fields. The values are applied to the config
    //    in memory (the caller has already swapped them), but the server
    //    must restart for them to take effect at the HTTP layer. The
    //    set of restart-required fields is the SINGLE source of truth
    //    here in `requires_restart_warnings` — the section-level
    //    dry-run (`/config/section/:name/validate`) delegates to it
    //    too so the two code paths can't drift.
    for w in requires_restart_warnings(old_cfg, new_cfg) {
        requires_restart = true;
        warnings.push(w);
    }

    Ok((warnings, requires_restart))
}

/// Return one warning per restart-required field that changed between
/// `old` and `new`. Empty vec = no restart required.
///
/// Single source of truth for the restart-required fieldset:
/// [`apply_config_transition`] uses this to emit warnings + set its
/// `requires_restart` flag, and
/// [`super::section_level::restart_required_between`] uses the same
/// predicate for its stateless dry-run. Adding a fifth restart-
/// required field means editing exactly this function.
pub(super) fn requires_restart_warnings(
    old: &crate::config::Config,
    new: &crate::config::Config,
) -> Vec<String> {
    let mut out = Vec::new();
    if old.listen_addr != new.listen_addr {
        out.push(format!(
            "listen_addr changed to {} — restart required",
            new.listen_addr
        ));
    }
    if old.cache_size_mb != new.cache_size_mb {
        out.push(format!(
            "cache_size_mb changed to {} — restart required",
            new.cache_size_mb
        ));
    }
    out
}

/// Resolve the path the admin API should persist config changes to.
///
/// Resolution order:
/// 1. The startup-time config path frozen in `AdminState::config_file_path`
///    (set from `--config` at server launch, falling back to the file found
///    on the default search path at that time). This is authoritative —
///    runtime changes to env vars or the filesystem must not redirect
///    persistence to a different file.
/// 2. `DEFAULT_YAML_CONFIG_FILENAME` in CWD when the server was started
///    without any config file at all. New deployments persist as YAML by
///    default.
pub(crate) fn active_config_path(state: &AdminState) -> String {
    state
        .config_file_path
        .clone()
        .unwrap_or_else(|| crate::config::DEFAULT_YAML_CONFIG_FILENAME.to_string())
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

// ────────────────────────────────────────────────────────────────────
// POST /api/admin/config/sync-now
// ────────────────────────────────────────────────────────────────────
//
// Operator-triggered config DB S3 sync.
//
// The background task runs every 5 minutes (see startup.rs::
// spawn_config_sync_poll). This endpoint lets an operator force an
// immediate check-and-pull without waiting for the next tick, which
// is useful when:
//
//   - A recent out-of-band mutation (e.g. from a different replica or
//     a restore) needs to propagate faster than 5 min.
//   - Integration tests need a deterministic barrier (same spirit as
//     the iam/version counter — poll instead of sleep).
//
// Only pulls (downloads newer state), never pushes. The push side is
// triggered automatically by every IAM mutation via
// `trigger_config_sync`.

#[derive(Serialize)]
pub struct SyncNowResponse {
    /// True if this call actually downloaded + applied a newer copy.
    /// False means local copy is current (ETag unchanged) or sync is
    /// disabled on this instance.
    downloaded: bool,
    /// Human-readable status. Always present; drives the GUI toast.
    status: String,
}

/// POST /api/admin/config/sync-now — force an immediate pull from the
/// config-sync S3 bucket and reopen the IAM database if newer.
///
/// Returns 404 when `config_sync_bucket` is not configured (this
/// instance isn't part of a sync group). 200 on any outcome when
/// sync IS configured — the response body says whether a download
/// actually happened.
pub async fn sync_now(
    State(state): State<Arc<AdminState>>,
) -> Result<Json<SyncNowResponse>, axum::http::StatusCode> {
    let sync = state
        .config_sync
        .as_ref()
        .ok_or(axum::http::StatusCode::NOT_FOUND)?;

    match sync.poll_and_sync().await {
        Ok(true) => {
            // New state downloaded — reopen the DB and rebuild IAM, exactly
            // as the periodic poll does. Kept consistent by funnelling
            // through the same helper (`reopen_and_rebuild_iam` in the
            // config_db_sync module).
            //
            // Clone the password hash OUT of the RwLock before crossing
            // `.await`: parking_lot guards are not Send, and holding one
            // across an await would block the task's Send contract even
            // though logically we only need the string value.
            let password_hash = state.password_hash.read().clone();
            crate::config_db_sync::reopen_and_rebuild_iam(
                &state.config_db,
                &password_hash,
                &state.iam_state,
                &state.external_auth,
                "sync-now endpoint",
            )
            .await;
            Ok(Json(SyncNowResponse {
                downloaded: true,
                status: "Downloaded newer config DB and reloaded IAM".to_string(),
            }))
        }
        Ok(false) => Ok(Json(SyncNowResponse {
            downloaded: false,
            status: "Local copy is current (ETag unchanged)".to_string(),
        })),
        Err(e) => {
            tracing::warn!("sync-now failed: {e}");
            Err(axum::http::StatusCode::BAD_GATEWAY)
        }
    }
}
