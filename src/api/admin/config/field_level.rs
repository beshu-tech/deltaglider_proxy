//! Legacy field-level admin-GUI config API — `GET` and `PUT /api/admin/config`.
//!
//! The admin GUI forms post partial JSON (only the fields being edited).
//! This submodule hosts:
//!
//! - [`get_config`] — returns a flat, sanitized snapshot of the runtime
//!   config (no secrets), with a `tainted_fields` list showing which
//!   in-memory values differ from the on-disk config file.
//! - [`update_config`] — applies a partial PATCH to the runtime config.
//!   Builds a prospective `new_cfg` from the patch, then hands off to the
//!   shared [`super::apply_config_transition`] helper for every side
//!   effect (engine rebuild, log reload, IAM swap, snapshot rebuild,
//!   restart detection, persist). The same helper backs the document-
//!   level `apply_config_doc` path, so hot-reload behavior cannot drift
//!   between PATCH and APPLY surfaces.
//!
//! Types here (`ConfigResponse`, `ConfigUpdateRequest`/`Response`,
//! `BackendInfoResponse`) are the wire shape the legacy GUI depends on.
//! Changes here are visible to the `/_/api/admin/config` consumers.

use axum::extract::State;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use super::super::AdminState;
use super::{active_config_path, apply_config_transition};

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
    // Per-bucket policies (BTreeMap for deterministic JSON ordering, matching
    // the canonical YAML export).
    bucket_policies: std::collections::BTreeMap<String, crate::bucket_policy::BucketPolicyConfig>,
    // Log level
    log_level: String,
    // Backend credentials indicator
    backend_has_credentials: bool,
    // Multi-backend
    backends: Vec<BackendInfoResponse>,
    default_backend: Option<String>,
    // Operator-authored admission blocks (Phase 3b.2). The UI uses
    // this for the new Admission tab; empty vec when no blocks are
    // authored.
    admission_blocks: Vec<crate::admission::AdmissionBlockSpec>,
    // IAM source-of-truth mode (Phase 3c.1). `"gui"` or `"declarative"`.
    // The UI uses this to render a banner when declarative mode is
    // active (IAM edits blocked) and to drive a toggle.
    iam_mode: crate::config_sections::IamMode,
    // Fields that differ from the TOML config file on disk
    tainted_fields: Vec<String>,
    // Encryption-at-rest status. Exposes a boolean ONLY — the key
    // itself is an infra secret and never leaves the server. Drives
    // the encryption panel's status indicator and the per-bucket
    // "encrypted at rest" badge in BucketsPanel.
    encryption_enabled: bool,
}

/// Per-backend encryption status summary. Exposed in
/// `BackendInfoResponse.encryption` so admin UI can render the
/// encryption badge per backend without peeking at secrets. Every
/// field here is non-secret:
///
///   * `mode` — string tag (`none`/`aes256-gcm-proxy`/`sse-kms`/`sse-s3`).
///   * `has_key` — does the backend carry key material? (For
///     aes256-gcm-proxy: true when `key` is set. For native modes:
///     true when AWS handles encryption — always true for sse-kms
///     since `kms_key_id` is required; always true for sse-s3.)
///   * `key_id` — non-secret id. For aes256-gcm-proxy this is the
///     operator's explicit id or the derived SHA-256-based one.
///   * `kms_key_id` — KMS ARN or alias. Non-secret; operators need
///     to see WHICH KMS key a backend uses.
///   * `shim_active` — decrypt-only shim flag. True when
///     `legacy_key` is configured (the backend can still decrypt
///     objects stamped with the pre-transition key).
#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct BackendEncryptionSummary {
    pub mode: String,
    pub has_key: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kms_key_id: Option<String>,
    pub shim_active: bool,
}

impl BackendEncryptionSummary {
    /// Build a summary from a config variant + backend name.
    /// Non-secret-only extraction — `key`/`legacy_key` are NOT
    /// surfaced here. The backend name is needed to derive the
    /// `key_id` when the operator left it implicit (same derivation
    /// the engine wrapper uses: `SHA-256(name || 0x00 || key)`).
    pub fn from_config(backend_name: &str, enc: &crate::config::BackendEncryptionConfig) -> Self {
        use crate::config::BackendEncryptionConfig as E;
        match enc {
            E::None => Self {
                mode: "none".into(),
                has_key: false,
                key_id: None,
                kms_key_id: None,
                shim_active: false,
            },
            E::Aes256GcmProxy {
                key,
                key_id,
                legacy_key,
                ..
            } => {
                // Surface the same id the engine stamps on written
                // objects. Explicit wins; otherwise derive from the
                // name + key (same logic as the engine's
                // `derive_key_id`).
                let resolved_kid: Option<String> = match key_id {
                    Some(explicit) => Some(explicit.clone()),
                    None => key
                        .as_deref()
                        .and_then(|hex| derive_key_id_for_summary(backend_name, hex)),
                };
                Self {
                    mode: "aes256-gcm-proxy".into(),
                    has_key: key.is_some(),
                    key_id: resolved_kid,
                    kms_key_id: None,
                    shim_active: legacy_key.is_some(),
                }
            }
            E::SseKms {
                kms_key_id,
                legacy_key,
                ..
            } => Self {
                mode: "sse-kms".into(),
                has_key: true,
                key_id: None,
                kms_key_id: Some(kms_key_id.clone()),
                shim_active: legacy_key.is_some(),
            },
            E::SseS3 { legacy_key, .. } => Self {
                mode: "sse-s3".into(),
                has_key: true,
                key_id: None,
                kms_key_id: None,
                shim_active: legacy_key.is_some(),
            },
        }
    }
}

/// Mirror of the engine's `derive_key_id` for the summary path.
/// Decodes hex → 32 bytes, then SHA-256(name ‖ 0x00 ‖ key) truncated
/// to 16 hex chars. Returns None on malformed hex (the engine would
/// reject that at startup; the summary just elides the id).
fn derive_key_id_for_summary(backend_name: &str, hex_key: &str) -> Option<String> {
    use sha2::Digest;
    let bytes = hex::decode(hex_key).ok()?;
    if bytes.len() != 32 {
        return None;
    }
    let mut hasher = sha2::Sha256::new();
    hasher.update(backend_name.as_bytes());
    hasher.update(b"\0");
    hasher.update(&bytes);
    Some(hex::encode(&hasher.finalize()[..8]))
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
    /// Per-backend encryption status. Step 6.
    pub encryption: BackendEncryptionSummary,
}

impl From<&crate::config::NamedBackendConfig> for BackendInfoResponse {
    fn from(named: &crate::config::NamedBackendConfig) -> Self {
        let encryption = BackendEncryptionSummary::from_config(&named.name, &named.encryption);
        match &named.backend {
            crate::config::BackendConfig::Filesystem { path } => Self {
                name: named.name.clone(),
                backend_type: "filesystem".into(),
                path: Some(path.display().to_string()),
                endpoint: None,
                region: None,
                force_path_style: None,
                has_credentials: false,
                encryption,
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
                encryption,
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
    // Per-bucket compression policies (BTreeMap mirrors the storage type on
    // `Config::buckets`, keeping canonical JSON responses deterministic).
    pub bucket_policies:
        Option<std::collections::BTreeMap<String, crate::bucket_policy::BucketPolicyConfig>>,
    // Operator-authored admission blocks (Phase 3b.2). Replaces the
    // full list on PATCH when present. `AdmissionSpec::validate` runs
    // before assignment — invalid blocks surface as warnings and the
    // PATCH rejects (the legacy "always 200 with warnings" contract
    // still covers validation errors via the `warnings` field).
    pub admission_blocks: Option<Vec<crate::admission::AdmissionBlockSpec>>,
    // IAM source-of-truth selector (Phase 3c.1). Accepts `"gui"` or
    // `"declarative"`; the middleware that gates IAM mutation routes
    // picks up the new value on the next request.
    pub iam_mode: Option<crate::config_sections::IamMode>,
}

#[derive(Serialize)]
pub struct ConfigUpdateResponse {
    success: bool,
    warnings: Vec<String>,
    requires_restart: bool,
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
        ) if rp != dp => {
            tainted.push("backend_path".to_string());
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
    use crate::config::{env_bool, env_parse_with_default};
    let env_u64 = |name: &str, default: u64| -> u64 { env_parse_with_default(name, default) };
    let env_usize = |name: &str, default: usize| -> usize { env_parse_with_default(name, default) };
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);

    let tainted_fields = compute_tainted_fields(&cfg);

    // Assemble the per-backend response list. When the operator is on
    // the legacy singleton path (no `backends:` in YAML), synthesise
    // a "default" entry that reflects `cfg.backend` + `cfg.backend_encryption`
    // so the UI's per-backend rendering works uniformly across both
    // configurations. Step 7 consumes this.
    let backends_info: Vec<BackendInfoResponse> = if cfg.backends.is_empty() {
        // Singleton path. Build a synthesised NamedBackendConfig so
        // the `From<&NamedBackendConfig>` impl does the heavy lifting;
        // this keeps the non-secret projection logic in ONE place.
        vec![BackendInfoResponse::from(
            &crate::config::NamedBackendConfig {
                name: "default".into(),
                backend: cfg.backend.clone(),
                encryption: cfg.backend_encryption.clone(),
            },
        )]
    } else {
        cfg.backends.iter().map(BackendInfoResponse::from).collect()
    };

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
        trust_proxy_headers: env_bool("DGP_TRUST_PROXY_HEADERS", false),
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
        // Admission chain (Phase 3b.2) — forwarded verbatim so the UI
        // can render an editor. Collapse the `[""]` sentinel on the
        // bucket-policy side is handled elsewhere; admission blocks
        // are operator-authored and round-trip without transformation.
        admission_blocks: cfg.admission_blocks.clone(),
        // IAM source-of-truth selector (Phase 3c.1). Included so the
        // UI can drive the `iam_mode: declarative` banner + toggle.
        iam_mode: cfg.iam_mode,
        tainted_fields,
        // Expose presence of the key as a boolean ONLY. Never leaks
        // the key material itself — that stays in the infra-secret
        // bucket and is redacted from every public surface.
        //
        // STEP-1 SHIM: "any backend has a non-None encryption mode"
        // stands in for the former single-key boolean. Step 6 replaces
        // this with per-backend summaries and Step 8 deletes the flag
        // entirely.
        encryption_enabled: matches!(
            cfg.backend_encryption,
            crate::config::BackendEncryptionConfig::Aes256GcmProxy { .. }
                | crate::config::BackendEncryptionConfig::SseKms { .. }
                | crate::config::BackendEncryptionConfig::SseS3 { .. }
        ) || cfg
            .backends
            .iter()
            .any(|b| !matches!(b.encryption, crate::config::BackendEncryptionConfig::None)),
    })
}

/// PUT /api/admin/config — update configuration via field-level patch.
///
/// The GUI forms post partial JSON (only the fields being edited). This
/// handler merges the patch into a prospective `new_cfg`, then hands the
/// old/new pair off to [`apply_config_transition`] — the same helper the
/// document-level `apply_config_doc` path uses — so hot-reload side
/// effects can never drift between the two surfaces.
///
/// Patch-specific logic that stays in this handler:
/// - Backend-field translation (the PATCH schema has flattened
///   `backend_type`/`backend_endpoint`/… instead of a nested struct).
/// - Parse-error warnings for a bad `listen_addr` string (the helper only
///   sees already-parsed `SocketAddr` values).
/// - Rollback of `cfg.backend` on engine-rebuild failure — the PATCH
///   contract returns `success: true` with a warning rather than a 5xx,
///   matching the admin-GUI's legacy expectations.
pub async fn update_config(
    State(state): State<Arc<AdminState>>,
    Json(body): Json<ConfigUpdateRequest>,
) -> impl IntoResponse {
    let mut cfg = state.config.write().await;
    let mut warnings = Vec::new();
    let old_cfg = cfg.clone();

    // ── Apply the patch ──────────────────────────────────────────────────
    // Hot-reloadable scalars.
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
    if let Some(ref level_str) = body.log_level {
        // Validate BEFORE mutating. Writing an unparseable filter into
        // `cfg.log_level` would persist it to disk; on the next restart
        // `EnvFilter::parse` fails and the server falls back to a default,
        // leaving a poisoned config file that silently disagrees with the
        // runtime. `parse_and_validate_yaml` does the same pre-validation
        // for the document-level apply path; mirror it here so PATCH and
        // APPLY agree.
        match level_str.parse::<tracing_subscriber::EnvFilter>() {
            Ok(_) => cfg.log_level = level_str.clone(),
            Err(e) => warnings.push(format!(
                "Invalid log_level '{}': {} — keeping current value",
                level_str, e
            )),
        }
    }

    // Backend-config patch — translate the flattened PATCH fields into a
    // `BackendConfig` mutation on `cfg.backend`.
    if let Err(msg) = apply_backend_patch(&mut cfg.backend, &body, &mut warnings) {
        warnings.push(msg);
    }

    // Restart-required scalar that needs parse-error handling.
    if let Some(ref addr) = body.listen_addr {
        match addr.parse() {
            Ok(parsed) => cfg.listen_addr = parsed,
            Err(_) => warnings.push(format!("Invalid listen_addr: {}", addr)),
        }
    }
    if let Some(cache) = body.cache_size_mb {
        cfg.cache_size_mb = cache;
    }

    // Bucket policies — normalize names to lowercase before storing,
    // then expand any per-bucket shorthands (`public: true` →
    // `public_prefixes: [""]`) so the runtime `PublicPrefixSnapshot`
    // sees the form it expects. Without this call, a PATCH setting
    // only `public: true` lands as `public_prefixes: []` and is
    // silently non-functional — the bucket looks public in the admin
    // UI but anonymous reads 403.
    if let Some(ref bucket_policies) = body.bucket_policies {
        let mut new_buckets: std::collections::BTreeMap<
            String,
            crate::bucket_policy::BucketPolicyConfig,
        > = bucket_policies
            .iter()
            .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
            .collect();
        for (name, policy) in new_buckets.iter_mut() {
            if let Err(e) = policy.normalize() {
                warnings.push(format!("bucket `{}`: {}", name, e));
            }
        }
        cfg.buckets = new_buckets;
    }

    // Operator-authored admission blocks. PATCH replaces the full
    // list — identical semantics to `bucket_policies` above. Validation
    // runs through `AdmissionSpec::validate` so duplicate names, bad
    // reject statuses, unsafe `source_ip_list` sizes, and bad globs
    // are caught here and surfaced as warnings (same 200-with-warnings
    // contract the rest of this handler uses).
    if let Some(ref blocks) = body.admission_blocks {
        let spec = crate::admission::AdmissionSpec {
            blocks: blocks.clone(),
        };
        match spec.validate() {
            Ok(()) => cfg.admission_blocks = blocks.clone(),
            Err(e) => {
                warnings.push(format!("admission_blocks: {}", e));
                // Don't touch runtime state on validation failure —
                // the operator's next GET must still show the old
                // (valid) chain.
            }
        }
    }

    // IAM source-of-truth selector. Changing this triggers the
    // `require_not_declarative` middleware to flip its gate on every
    // subsequent IAM mutation request — `apply_config_transition`
    // also emits a warn-level audit log line on the transition.
    if let Some(new_mode) = body.iam_mode {
        cfg.iam_mode = new_mode;
    }

    // ── Run transition side effects ──────────────────────────────────────
    // Any failure here (currently: engine rebuild) means the patch can't
    // be honored. Roll back the in-memory mutation and surface a warning,
    // preserving the legacy PATCH contract ("success: true, warnings: [...]")
    // instead of returning a 5xx like apply_config_doc does.
    match apply_config_transition(&state, &old_cfg, &cfg).await {
        Ok((transition_warnings, requires_restart)) => {
            warnings.extend(transition_warnings);

            // Persist AFTER side effects succeed. Persist failure is a
            // warning, not a rollback — the runtime state is correct;
            // only the on-disk file is stale.
            let persist_path = active_config_path(&state);
            if let Err(e) = cfg.persist_to_file(&persist_path) {
                warnings.push(format!(
                    "Failed to persist config to {}: {}",
                    persist_path, e
                ));
            }

            Json(ConfigUpdateResponse {
                success: true,
                warnings,
                requires_restart,
            })
        }
        Err(engine_err) => {
            // Roll back the in-memory mutation so the next read sees the
            // pre-patch state. The engine was left untouched by the helper
            // (failure happens before it's stored), so we only need to
            // restore `*cfg`.
            *cfg = old_cfg;
            warnings.push(format!(
                "Failed to apply config patch: {}. Pre-patch config restored.",
                engine_err
            ));
            Json(ConfigUpdateResponse {
                success: true,
                warnings,
                requires_restart: false,
            })
        }
    }
}

/// Translate the flattened backend-field patch (`backend_type`,
/// `backend_endpoint`, `backend_region`, …) into a mutation on
/// `cfg.backend`. Factored out of `update_config` so the handler body
/// stays focused on the patch/transition composition.
///
/// Returns `Err(msg)` only when the operator specified an unknown
/// backend type; the caller surfaces that as a warning. All other
/// branches (type unchanged but fields updated, type changed to a known
/// variant) return `Ok(())` and mutate in place.
fn apply_backend_patch(
    backend: &mut crate::config::BackendConfig,
    body: &ConfigUpdateRequest,
    warnings: &mut Vec<String>,
) -> Result<(), String> {
    // Reject empty path up-front. An empty `PathBuf` silently becomes CWD at
    // runtime, which is almost never the operator's intent — most likely an
    // uncleared admin-GUI form field.
    if let Some(ref p) = body.backend_path {
        if p.is_empty() {
            return Err(
                "backend_path must not be empty — supply a concrete filesystem path".into(),
            );
        }
    }

    let current_type = match backend {
        crate::config::BackendConfig::Filesystem { .. } => "filesystem",
        crate::config::BackendConfig::S3 { .. } => "s3",
    };
    // Case-insensitive: accept "S3" / "FileSystem" / "s3" equivalently. The
    // canonical on-the-wire value (and the one `ConfigResponse` echoes back)
    // stays lowercase, so a client that re-POSTs what it read won't round-trip
    // into an error.
    let requested_type_owned = body.backend_type.as_deref().map(str::to_ascii_lowercase);
    let requested_type = requested_type_owned.as_deref().unwrap_or(current_type);

    if requested_type != current_type {
        // Type change — construct the fresh variant.
        match requested_type {
            "filesystem" => {
                let path = body
                    .backend_path
                    .clone()
                    .unwrap_or_else(|| "./data".to_string());
                *backend = crate::config::BackendConfig::Filesystem {
                    path: std::path::PathBuf::from(path),
                };
                warnings.push(
                    "Backend type changed. Data in the previous backend is not migrated."
                        .to_string(),
                );
                return Ok(());
            }
            "s3" => {
                *backend = crate::config::BackendConfig::S3 {
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
                warnings.push(
                    "Backend type changed. Data in the previous backend is not migrated."
                        .to_string(),
                );
                return Ok(());
            }
            other => {
                return Err(format!(
                    "Unknown backend type: '{}'. Must be 'filesystem' or 's3'.",
                    other
                ));
            }
        }
    }

    // Same type — update fields in-place.
    match backend {
        crate::config::BackendConfig::Filesystem { path } => {
            if let Some(ref p) = body.backend_path {
                *path = std::path::PathBuf::from(p);
            }
        }
        crate::config::BackendConfig::S3 {
            endpoint,
            region,
            force_path_style,
            access_key_id,
            secret_access_key,
        } => {
            if let Some(ref ep) = body.backend_endpoint {
                *endpoint = if ep.is_empty() {
                    None
                } else {
                    Some(ep.clone())
                };
            }
            if let Some(ref r) = body.backend_region {
                *region = r.clone();
            }
            if let Some(fps) = body.backend_force_path_style {
                *force_path_style = fps;
            }
            if let Some(ref key) = body.backend_access_key_id {
                if !key.is_empty() {
                    *access_key_id = Some(key.clone());
                }
            }
            if let Some(ref secret) = body.backend_secret_access_key {
                if !secret.is_empty() {
                    *secret_access_key = Some(secret.clone());
                }
            }
        }
    }
    Ok(())
}
