// SPDX-License-Identifier: GPL-3.0-only

//! Document-level (GitOps) config API — export / validate / apply.
//!
//! These handlers accept or return a full canonical YAML document rather
//! than the flattened field-level PATCH shape used by the legacy admin-GUI
//! forms. They exist to serve two personas:
//!
//! - **GitOps operators**: POST a full YAML to `/apply`, the server
//!   validates, merges runtime secrets forward, and atomically swaps the
//!   live config (with rollback on failure).
//! - **GUI users exporting their config**: GET `/export` returns the
//!   canonical YAML form, all secrets stripped, for copy-paste into a
//!   GitOps repo.
//!
//! The sibling `parse_and_validate_yaml`, `preserve_runtime_secrets`, and
//! `preserve_sigv4_pair` helpers live here too — they are private details
//! of this flow and have no callers outside it.

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use super::super::{audit_log, AdminState};
use super::{active_config_path, apply_config_transition, unknown_section_error, SectionName};

//
// These endpoints serve the GitOps persona and the GUI "Copy as YAML" flow.
// They sit alongside the existing field-level `PUT /api/admin/config` (which
// the admin forms use) — nothing is replaced. Secret handling is strict:
// exported YAML never carries SigV4 or backend credentials. Applied YAML has
// its secret fields merged from the current runtime where absent, so the
// GitOps round-trip (export → edit → apply) never accidentally clears creds.

/// Request body for `/config/validate` and `/config/apply`.
///
/// The `yaml` field is the full canonical document. A partial patch is not
/// accepted here — use the field-level `PUT /api/admin/config` for that.
#[derive(Deserialize)]
pub struct ConfigDocumentRequest {
    /// Full canonical YAML document. Secrets may be omitted; they will be
    /// preserved from the running config by `apply`.
    pub yaml: String,
}

#[derive(Serialize)]
pub struct ConfigValidateResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize)]
pub struct ConfigApplyResponse {
    /// The in-memory config was swapped and all hot-reload side effects took
    /// effect (engine rebuild, log filter, IAM state, public-prefix snapshot).
    pub applied: bool,
    /// The applied config was written to disk atomically. When false, the
    /// server will revert to the on-disk config at the next restart — a
    /// state that is sometimes intentional (ephemeral containers) but
    /// usually a problem; clients should surface this clearly.
    pub persisted: bool,
    /// One or more applied fields require a server restart to take full
    /// effect (e.g. `listen_addr`, `cache_size_mb`).
    pub requires_restart: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Path the config was written to. `None` when persist failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub persisted_path: Option<String>,
}

/// Query params shared by `/config/export` and `/config/defaults`.
#[derive(Deserialize, Default)]
pub struct SectionFilterQuery {
    /// Scope the response to one named section: `admission`, `access`,
    /// `storage`, or `advanced`. Absent = whole document.
    #[serde(default)]
    section: Option<String>,
}

/// `GET /api/admin/config/export[?section=<name>]` — canonical YAML of
/// the current runtime config, with every secret redacted.
///
/// Default (no `section=`) returns the full document — the legacy
/// "Copy as YAML" surface. With `?section=admission|access|storage|
/// advanced`, the response is scoped to just that section (rendered as
/// a top-level `<section>:` YAML document). Lets the UI's per-section
/// Copy-as-YAML button (§3.3 of the revamp plan) hit one endpoint
/// parameterized by section instead of each section hand-assembling
/// its own YAML client-side.
///
/// Unknown section names return 404 (not 400) so deep-linkable URLs
/// produce the same shape as a mis-routed GET.
pub async fn export_config(
    State(state): State<Arc<AdminState>>,
    Query(query): Query<SectionFilterQuery>,
) -> impl IntoResponse {
    let cfg = state.config.read().await;
    let redacted = cfg.redact_all_secrets();
    drop(cfg);

    let Some(section_name) = query.section.as_deref() else {
        // Full document path — unchanged from the pre-Wave-1 behavior.
        return match redacted.to_canonical_yaml() {
            Ok(yaml) => (
                StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, "application/yaml")],
                yaml,
            )
                .into_response(),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to serialize config to YAML: {}", e),
            )
                .into_response(),
        };
    };

    // Section-scoped export. We reuse the SectionedConfig projection
    // the full export does, then pick out just the requested slice.
    // Each section serializes as `<name>:\n  ...` — valid standalone
    // YAML that can be edited and posted back via section PUT.
    let Some(section) = SectionName::parse(section_name) else {
        return (StatusCode::NOT_FOUND, unknown_section_error(section_name)).into_response();
    };
    let sectioned = crate::config_sections::SectionedConfig::from_flat(&redacted);
    let value = match section {
        SectionName::Admission => serde_yaml::to_value(sectioned.admission.unwrap_or_default()),
        SectionName::Access => serde_yaml::to_value(sectioned.access),
        SectionName::Storage => serde_yaml::to_value(sectioned.storage),
        SectionName::Advanced => serde_yaml::to_value(sectioned.advanced),
    };
    let yaml_value = match value {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to serialize section: {}", e),
            )
                .into_response();
        }
    };
    let mut map = serde_yaml::Mapping::new();
    map.insert(
        serde_yaml::Value::String(section.as_str().to_string()),
        yaml_value,
    );
    match serde_yaml::to_string(&serde_yaml::Value::Mapping(map)) {
        Ok(s) => (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/yaml")],
            s,
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to serialize section to YAML: {}", e),
        )
            .into_response(),
    }
}

/// `GET /api/admin/config/defaults[?section=<name>]` — JSON Schema for
/// the Config type, optionally scoped to one section.
///
/// Default (no `section=`) returns the full Config schema (the legacy
/// behaviour). With `?section=admission|access|storage|advanced`, the
/// response is the JSON Schema for just that section's type — exactly
/// what `monaco-yaml` needs when the UI's Monaco editor is bound to
/// one section's scope. Wave 2 of the admin UI plan reads this for
/// per-section YAML linting.
pub async fn config_defaults(Query(query): Query<SectionFilterQuery>) -> impl IntoResponse {
    let schema = match query.section.as_deref() {
        None => serde_json::to_value(schemars::schema_for!(crate::config::Config)),
        Some(name) => match SectionName::parse(name) {
            Some(SectionName::Admission) => serde_json::to_value(schemars::schema_for!(
                crate::config_sections::AdmissionSection
            )),
            Some(SectionName::Access) => {
                serde_json::to_value(schemars::schema_for!(crate::config_sections::AccessSection))
            }
            Some(SectionName::Storage) => serde_json::to_value(schemars::schema_for!(
                crate::config_sections::StorageSection
            )),
            Some(SectionName::Advanced) => serde_json::to_value(schemars::schema_for!(
                crate::config_sections::AdvancedSection
            )),
            None => {
                return (StatusCode::NOT_FOUND, unknown_section_error(name)).into_response();
            }
        },
    };
    match schema {
        Ok(v) => (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/schema+json")],
            Json(v),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("failed to serialize schema: {}", e),
        )
            .into_response(),
    }
}

/// Parse a YAML config document and collect validation warnings.
///
/// Returns `(Config, warnings)` on success, an error string on parse
/// failure. [`Config::check`] is the single source of truth for validation
/// — it mutates fields that can't be satisfied (e.g. clears an unresolved
/// `default_backend`) and returns the corresponding human-readable
/// warnings.
///
/// An empty / whitespace-only body is rejected explicitly: `serde_yaml`
/// deserializes `""` into `Config::default()`, which on apply would reset
/// every field to its default. That's almost certainly operator error (a
/// CI template variable didn't expand, a pipeline piped the wrong file),
/// and its consequences are destructive. Fail loudly instead.
///
/// The log-filter string is parsed here too (not at swap time) so a
/// malformed filter cannot enter the runtime config; the admin handler
/// surfaces the parse error to the caller and leaves state unchanged.
fn parse_and_validate_yaml(yaml: &str) -> Result<(crate::config::Config, Vec<String>), String> {
    if yaml.trim().is_empty() {
        return Err(
            "empty YAML body: apply requires a full canonical config document. Refusing to reset every field to its default."
                .to_string(),
        );
    }
    // Go through the dual-shape deserializer so GitOps operators can POST
    // either the legacy flat shape or the Phase 3 sectioned shape
    // (admission/access/storage/advanced). Export round-trips re-emit
    // sectioned — if we used plain `serde_yaml::from_str::<Config>` here
    // the roundtrip would break.
    let mut cfg = crate::config::Config::from_yaml_str(yaml)
        .map_err(|e| format!("YAML parse error: {}", e))?;
    // Validate the log filter up front so it can't silently enter runtime
    // state and then fail at the next process restart. An invalid filter is
    // a non-recoverable structural error for this doc, not a warning.
    if cfg
        .log_level
        .parse::<tracing_subscriber::EnvFilter>()
        .is_err()
    {
        return Err(format!(
            "invalid log_level filter '{}': expected a tracing-subscriber EnvFilter (e.g. 'info', 'deltaglider_proxy=debug')",
            cfg.log_level
        ));
    }
    let warnings = cfg.check();
    Ok((cfg, warnings))
}

/// `POST /api/admin/config/validate` — dry-run.
///
/// Parses the YAML body, runs validation, and reports warnings or errors.
/// No runtime state is mutated. Used by CI (`dgpctl config lint` in Phase 4)
/// and by the admin GUI's pre-apply confirmation modal.
pub async fn validate_config_doc(Json(body): Json<ConfigDocumentRequest>) -> impl IntoResponse {
    match parse_and_validate_yaml(&body.yaml) {
        Ok((_, warnings)) => (
            StatusCode::OK,
            Json(ConfigValidateResponse {
                ok: true,
                warnings,
                error: None,
            }),
        ),
        Err(err) => (
            StatusCode::BAD_REQUEST,
            Json(ConfigValidateResponse {
                ok: false,
                warnings: vec![],
                error: Some(err),
            }),
        ),
    }
}

/// Merge runtime secrets into an incoming (redacted) Config and report any
/// credential transitions that would silently drop live creds.
///
/// When the exported YAML is POSTed back via `apply`, its secret fields are
/// all None (export redacts them). We don't want apply to silently clear
/// credentials — the expected GitOps flow is "edit the non-secret fields,
/// leave secrets to the runtime". So for every secret that's None in the
/// incoming doc, we copy the current runtime value across.
///
/// Operators who actually want to rotate a secret via YAML set it to a
/// literal value (for infra-secret rotation from a secret manager that
/// substitutes into the YAML pre-apply).
///
/// Returns a list of warnings covering every case where the merge cannot
/// carry creds forward safely — backend renames, backend-type swaps, and
/// asymmetric credential pairs. The caller surfaces these in the apply
/// response so operators are never caught by a silent auth-loss.
fn preserve_runtime_secrets(
    incoming: &mut crate::config::Config,
    current: &crate::config::Config,
) -> Vec<String> {
    let mut warnings = Vec::new();

    // Top-level infra secrets. The bootstrap hash is the only
    // top-level infra secret left after the per-backend encryption
    // refactor — per-backend keys live on `backend_encryption` (the
    // singleton) and on each `backends[i].encryption`, and are
    // preserved by the per-backend secret-preservation path.
    if incoming.bootstrap_password_hash.is_none() {
        incoming.bootstrap_password_hash = current.bootstrap_password_hash.clone();
    }

    // Top-level proxy SigV4 creds — both-or-neither, asymmetric warns.
    super::preserve_sigv4_pair(
        &mut incoming.access_key_id,
        &mut incoming.secret_access_key,
        &current.access_key_id,
        &current.secret_access_key,
        "proxy-level",
        &mut warnings,
    );

    // Primary + named backend creds, with type-flip + removed-backend
    // warnings. Helpers live in `super` so the section-PUT path uses
    // identical logic; see the doc-block at the helpers' definition.
    super::preserve_primary_backend_creds(incoming, current, &mut warnings);
    super::preserve_named_backends_creds(incoming, current, &mut warnings);

    // Webhook header values are masked to REDACTED_SENTINEL on export
    // (`redact_all_secrets`). A full-document export → edit → apply
    // round-trip (GUI "Export/Import YAML" or any GitOps flow) would
    // otherwise persist the literal sentinel as the bearer token. Mirror
    // the section-PUT path so the document path preserves untouched
    // secrets identically.
    super::preserve_event_delivery_secrets(&mut incoming.event_delivery, &current.event_delivery);

    warnings
}

/// `POST /api/admin/config/apply` — atomic full-document apply.
///
/// Workflow:
/// 1. Parse + validate the incoming YAML.
/// 2. Merge runtime secrets forward (redacted round-trip preservation).
/// 3. Defense-in-depth: reject apply attempts that would change the
///    bootstrap password hash (legitimate path is `PUT /password`).
/// 4. Under the write lock, hand off to [`apply_config_transition`] which
///    owns every downstream side effect (engine rebuild, log reload,
///    IAM swap, snapshot rebuilds, restart detection).
/// 5. Persist. Persist failure => HTTP 500 with the in-memory state left
///    intact so operators can retry without a data-loss window.
pub async fn apply_config_doc(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
    Json(body): Json<ConfigDocumentRequest>,
) -> impl IntoResponse {
    // 1. Parse + validate the incoming document (no lock held — pure work).
    let (mut incoming, parse_warnings) = match parse_and_validate_yaml(&body.yaml) {
        Ok(v) => v,
        Err(err) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ConfigApplyResponse {
                    applied: false,
                    persisted: false,
                    requires_restart: false,
                    warnings: vec![],
                    error: Some(err),
                    persisted_path: None,
                }),
            );
        }
    };

    // 2. Acquire the write lock and hold it for the remainder of the apply.
    //    Serializes admin mutations so a concurrent PATCH via `update_config`
    //    cannot race our read-for-compare and our write-to-swap.
    let mut cfg = state.config.write().await;

    // 3. Merge runtime secrets into the incoming doc. `preserve_runtime_secrets`
    //    emits its own warnings for credential transitions that would
    //    silently clear state — surface them to the caller.
    let preserve_warnings = preserve_runtime_secrets(&mut incoming, &cfg);

    // 4. Defense in depth: refuse to swap the bootstrap password hash
    //    through `apply`. The legitimate path is PUT /api/admin/password,
    //    which verifies the current password and re-encrypts the config
    //    database atomically. Accepting an arbitrary hash here would let
    //    an admin-session holder lock future admins out of the GUI (by
    //    setting a hash whose plaintext they don't share) or seed a hash
    //    whose plaintext they control. Export redaction means round-trips
    //    naturally produce `None` here (which `preserve_runtime_secrets`
    //    fills back in); anything else indicates a manual edit.
    if incoming.bootstrap_password_hash != cfg.bootstrap_password_hash {
        return (
            StatusCode::FORBIDDEN,
            Json(ConfigApplyResponse {
                applied: false,
                persisted: false,
                requires_restart: false,
                warnings: vec![],
                error: Some(
                    "bootstrap_password_hash cannot be changed via /config/apply; use PUT /api/admin/password (verifies the current password and re-encrypts the config DB atomically)".to_string(),
                ),
                persisted_path: None,
            }),
        );
    }

    // 5. Run the transition side effects. The helper owns engine rebuild
    //    (with bail-before-swap on failure), log reload, IAM state swap,
    //    snapshot rebuilds, and restart detection. Behavior intentionally
    //    mirrors the field-level PATCH path in `update_config` — both
    //    paths compose their responses from the same single source of
    //    transition truth.
    let old_cfg = cfg.clone();
    let (transition_warnings, requires_restart) =
        match apply_config_transition(&state, &old_cfg, &incoming).await {
            Ok(r) => r,
            Err(e) => {
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(ConfigApplyResponse {
                        applied: false,
                        persisted: false,
                        requires_restart: false,
                        warnings: parse_warnings
                            .into_iter()
                            .chain(preserve_warnings)
                            .collect(),
                        error: Some(format!(
                            "Failed to build engine from applied config (no state changed): {}",
                            e
                        )),
                        persisted_path: None,
                    }),
                );
            }
        };

    // 6. Atomic in-memory swap (still inside the write lock).
    *cfg = incoming;

    // 7. Persist to the active config file, preserving its on-disk extension.
    //    `persist_to_file` is atomic (write-to-tempfile + rename) so the
    //    file is either the old content or the new content — never a
    //    partial write. The write itself can still fail (permission
    //    denied, disk full, missing directory); we surface that as
    //    `persisted: false` + HTTP 500 so GitOps pipelines don't mistake
    //    a persist failure for a clean apply.
    let persist_path = active_config_path(&state);
    let (persisted, persisted_path, status, persist_warning) =
        match cfg.persist_to_file(&persist_path) {
            Ok(()) => (true, Some(persist_path.clone()), StatusCode::OK, None),
            Err(e) => (
                false,
                None,
                StatusCode::INTERNAL_SERVER_ERROR,
                Some(format!(
                    "Applied in memory but FAILED to persist to {}: {}. Server will revert to the on-disk config on next restart — fix the underlying IO problem and re-apply.",
                    persist_path, e
                )),
            ),
        };

    audit_log("apply_config", "admin", &persist_path, &headers);

    let warnings: Vec<String> = parse_warnings
        .into_iter()
        .chain(preserve_warnings)
        .chain(transition_warnings)
        .chain(persist_warning)
        .collect();

    (
        status,
        Json(ConfigApplyResponse {
            applied: true,
            persisted,
            requires_restart,
            warnings,
            error: None,
            persisted_path,
        }),
    )
}

/// `GET /api/admin/config/declarative-iam-export` — project the
/// current encrypted IAM DB (users, groups, auth providers, mapping
/// rules) into a YAML fragment ready to paste into the `access:`
/// section of a declarative-mode config.
///
/// Always returns `application/yaml` with a top-level `access:` key
/// containing `iam_users`, `iam_groups`, `auth_providers`, and
/// `group_mapping_rules` populated from the DB. Includes
/// `iam_mode: declarative` so the emitted fragment is self-describing.
///
/// **Secrets redacted**: user `secret_access_key` emits as `""`,
/// provider `client_secret` emits as `null`. Operator wires both
/// via env vars / secret manager before applying.
///
/// **Roundtripability contract**: applying the unredacted form of
/// this output on a live instance that sourced it is an idempotent
/// no-op (ReconcileStats::is_noop() ⇒ true), because the diff sees
/// same-name entries with matching fields.
///
/// Returns 404 when no config DB is initialised (bootstrap-disabled
/// deployments have nothing to export).
#[derive(serde::Deserialize, Default)]
pub struct ExportIamQuery {
    /// When true, emit real `secret_access_key` / `client_secret` values so the
    /// export round-trips losslessly on re-import. Default false (redacted).
    #[serde(default)]
    pub include_secrets: bool,
}

pub async fn export_declarative_iam(
    State(state): State<Arc<AdminState>>,
    axum::extract::Query(q): axum::extract::Query<ExportIamQuery>,
) -> impl IntoResponse {
    let Some(db_arc) = state.config_db.as_ref() else {
        return (
            StatusCode::NOT_FOUND,
            "config DB not initialised — nothing to export".to_string(),
        )
            .into_response();
    };
    let db = db_arc.lock().await;
    // `include_secrets=true` produces a lossless, round-trippable full-IAM file
    // (the "Export full IAM (YAML)" affordance). The file then contains LIVE
    // credentials — the UI warns the operator and the route is admin-gated.
    let snapshot = match crate::iam::export_as_declarative_inner(&db, q.include_secrets) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("export_as_declarative: {}", e),
            )
                .into_response();
        }
    };
    drop(db);

    // Emit a minimal YAML with `access.iam_mode: declarative` + the
    // 4 IAM slices. Matches the shape `declarative-iam.md` documents.
    let mut access_map = serde_yaml::Mapping::new();
    access_map.insert(
        serde_yaml::Value::String("iam_mode".into()),
        serde_yaml::Value::String("declarative".into()),
    );
    access_map.insert(
        serde_yaml::Value::String("iam_users".into()),
        serde_yaml::to_value(&snapshot.users).unwrap_or(serde_yaml::Value::Null),
    );
    access_map.insert(
        serde_yaml::Value::String("iam_groups".into()),
        serde_yaml::to_value(&snapshot.groups).unwrap_or(serde_yaml::Value::Null),
    );
    access_map.insert(
        serde_yaml::Value::String("auth_providers".into()),
        serde_yaml::to_value(&snapshot.auth_providers).unwrap_or(serde_yaml::Value::Null),
    );
    access_map.insert(
        serde_yaml::Value::String("group_mapping_rules".into()),
        serde_yaml::to_value(&snapshot.mapping_rules).unwrap_or(serde_yaml::Value::Null),
    );

    let mut root = serde_yaml::Mapping::new();
    root.insert(
        serde_yaml::Value::String("access".into()),
        serde_yaml::Value::Mapping(access_map),
    );

    match serde_yaml::to_string(&serde_yaml::Value::Mapping(root)) {
        Ok(yaml) => (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/yaml")],
            yaml,
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("serialize declarative IAM to YAML: {}", e),
        )
            .into_response(),
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Full-IAM YAML import (validate dry-run + apply).
//
// The counterpart to `export_declarative_iam`. Takes the same `access:` shaped
// YAML and reconciles it into the IAM DB via the existing declarative engine
// (`preview_declarative_iam` for the dry-run diff, `reconcile_declarative_iam`
// for the atomic apply). UNLIKE the declarative-mode config-apply path, this
// works regardless of `iam_mode` (the reconciler is mode-agnostic) — it's a GUI
// convenience for full IAM round-trips. The route is admin-GUI-session gated.
// ─────────────────────────────────────────────────────────────────────────

/// `{ created, updated, deleted, mapping_rules_replaced }` summary returned by
/// both the dry-run (`/validate`) and the apply.
#[derive(Serialize, Default)]
pub struct IamImportSummary {
    pub users_created: usize,
    pub users_updated: usize,
    pub users_deleted: usize,
    pub groups_created: usize,
    pub groups_updated: usize,
    pub groups_deleted: usize,
    pub providers_created: usize,
    pub providers_updated: usize,
    pub providers_deleted: usize,
    pub mapping_rules_replaced: usize,
    /// True when applying this YAML would change nothing.
    pub no_changes: bool,
}

/// Parse the incoming `access:`-shaped YAML into a `DeclarativeIam` snapshot.
/// Returns a 400-friendly error string on malformed YAML.
fn parse_iam_yaml(yaml: &str) -> Result<crate::iam::DeclarativeIam, String> {
    let sectioned: crate::config_sections::SectionedConfig =
        serde_yaml::from_str(yaml).map_err(|e| format!("invalid YAML: {e}"))?;
    let access = sectioned.access;
    Ok(crate::iam::snapshot_from_access(
        &access.iam_users,
        &access.iam_groups,
        &access.auth_providers,
        &access.group_mapping_rules,
    ))
}

/// `POST /_/api/admin/config/declarative-iam-validate` — dry-run a full-IAM
/// YAML import: parse + diff against the live DB, return the change summary
/// WITHOUT touching state. Powers the Apply-dialog preview.
pub async fn validate_declarative_iam(
    State(state): State<Arc<AdminState>>,
    Json(body): Json<ConfigDocumentRequest>,
) -> impl IntoResponse {
    let Some(db_arc) = state.config_db.as_ref() else {
        return (
            StatusCode::NOT_FOUND,
            "config DB not initialised — IAM import unavailable".to_string(),
        )
            .into_response();
    };
    let snapshot = match parse_iam_yaml(&body.yaml) {
        Ok(s) => s,
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };
    let db = db_arc.lock().await;
    match crate::iam::preview_declarative_iam(&db, &snapshot) {
        Ok(diff) => Json(summarise_diff(&diff)).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, format!("validation failed: {e}")).into_response(),
    }
}

/// `POST /_/api/admin/config/declarative-iam-apply` — apply a full-IAM YAML
/// import: parse, reconcile atomically, rebuild the index, sync, audit.
pub async fn apply_declarative_iam(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
    Json(body): Json<ConfigDocumentRequest>,
) -> impl IntoResponse {
    let Some(db_arc) = state.config_db.as_ref() else {
        return (
            StatusCode::NOT_FOUND,
            "config DB not initialised — IAM import unavailable".to_string(),
        )
            .into_response();
    };
    let snapshot = match parse_iam_yaml(&body.yaml) {
        Ok(s) => s,
        Err(e) => return (StatusCode::BAD_REQUEST, e).into_response(),
    };

    let db = db_arc.lock().await;
    let stats = match crate::iam::reconcile_declarative_iam(&db, &snapshot) {
        Ok(s) => s,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                format!("IAM import failed (no state changed): {e}"),
            )
                .into_response();
        }
    };
    // Rebuild the in-memory index from the now-committed DB. Use the
    // `_declarative` variant (bumps IAM_VERSION for test barriers AND skips the
    // legacy-admin auto-migration): a full-IAM YAML import is authoritative for
    // the entire IAM set, so we must not silently auto-author a `legacy-admin`
    // row the imported document didn't declare — same contract as the
    // declarative config-apply path (config/mod.rs).
    if let Err(e) = super::super::users::rebuild_iam_index_declarative(&db, &state.iam_state) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("rebuild_iam_index after IAM import: {e:?}"),
        )
            .into_response();
    }
    drop(db);

    if !stats.is_noop() {
        super::super::trigger_config_sync(&state);
    }
    for (action, names) in stats.audit_entries() {
        for name in names {
            audit_log(action, "iam-yaml-import", name, &headers);
        }
    }
    tracing::info!("[iam-yaml-import] {}", stats.summary_line());

    Json(summarise_stats(&stats)).into_response()
}

fn summarise_diff(diff: &crate::iam::IamDiff) -> IamImportSummary {
    let s = IamImportSummary {
        users_created: diff.users_to_create.len(),
        users_updated: diff.users_to_update.len(),
        users_deleted: diff.users_to_delete.len(),
        groups_created: diff.groups_to_create.len(),
        groups_updated: diff.groups_to_update.len(),
        groups_deleted: diff.groups_to_delete.len(),
        providers_created: diff.providers_to_create.len(),
        providers_updated: diff.providers_to_update.len(),
        providers_deleted: diff.providers_to_delete.len(),
        mapping_rules_replaced: match &diff.mapping_rules {
            crate::iam::MappingRulesAction::ReplaceWith(v) => v.len(),
            crate::iam::MappingRulesAction::ClearAll => 0,
            crate::iam::MappingRulesAction::Keep => 0,
        },
        no_changes: false,
    };
    let no_changes = s.users_created == 0
        && s.users_updated == 0
        && s.users_deleted == 0
        && s.groups_created == 0
        && s.groups_updated == 0
        && s.groups_deleted == 0
        && s.providers_created == 0
        && s.providers_updated == 0
        && s.providers_deleted == 0
        && s.mapping_rules_replaced == 0;
    IamImportSummary { no_changes, ..s }
}

fn summarise_stats(stats: &crate::iam::ReconcileStats) -> IamImportSummary {
    IamImportSummary {
        users_created: stats.users_created.len(),
        users_updated: stats.users_updated.len(),
        users_deleted: stats.users_deleted.len(),
        groups_created: stats.groups_created.len(),
        groups_updated: stats.groups_updated.len(),
        groups_deleted: stats.groups_deleted.len(),
        providers_created: stats.providers_created.len(),
        providers_updated: stats.providers_updated.len(),
        providers_deleted: stats.providers_deleted.len(),
        mapping_rules_replaced: stats.mapping_rules_replaced,
        no_changes: stats.is_noop(),
    }
}

#[cfg(test)]
mod iam_import_tests {
    use super::*;
    use crate::iam::{DeclarativeUser, IamDiff, MappingRulesAction, ReconcileStats};

    // The import handlers do their I/O around two pure functions:
    // `parse_iam_yaml` (YAML → snapshot) and the summary builders
    // (diff/stats → count response). Both are unit-tested here without a
    // TestServer — the request-pipeline seam is exercised by the existing
    // declarative-reconcile integration coverage.

    #[test]
    fn parse_iam_yaml_reads_access_iam_slices() {
        // Mirrors exactly what `export_declarative_iam` emits.
        let yaml = "\
access:
  iam_mode: declarative
  iam_users:
    - name: alice
      access_key_id: AKIAALICE
      secret_access_key: s3cr3t
  iam_groups: []
  auth_providers: []
  group_mapping_rules: []
";
        let snap = parse_iam_yaml(yaml).expect("valid IAM YAML parses");
        assert_eq!(snap.users.len(), 1);
        assert_eq!(snap.users[0].name, "alice");
        assert_eq!(snap.users[0].access_key_id, "AKIAALICE");
        // Secret survives the parse (lossless round-trip contract).
        assert_eq!(snap.users[0].secret_access_key, "s3cr3t");
        assert!(snap.groups.is_empty());
        assert!(snap.auth_providers.is_empty());
        assert!(snap.mapping_rules.is_empty());
    }

    #[test]
    fn parse_iam_yaml_rejects_malformed() {
        assert!(parse_iam_yaml("access: [this is not a mapping").is_err());
    }

    #[test]
    fn parse_iam_yaml_empty_access_is_a_full_wipe_snapshot() {
        // An `access: {}` document means "no users/groups/etc." — the
        // reconcile downstream interprets that as delete-all. We only
        // assert the snapshot is empty here; the wipe semantics live in
        // the reconcile tests.
        let snap = parse_iam_yaml("access: {}").expect("empty access parses");
        assert!(snap.users.is_empty());
        assert!(snap.groups.is_empty());
    }

    #[test]
    fn summarise_diff_counts_each_category() {
        let mut diff = IamDiff::default();
        diff.users_to_create.push(DeclarativeUser {
            name: "a".into(),
            access_key_id: "AKIA".into(),
            secret_access_key: String::new(),
            enabled: true,
            groups: vec![],
            permissions: vec![],
        });
        diff.users_to_update.push((
            1,
            DeclarativeUser {
                name: "b".into(),
                access_key_id: "AKIB".into(),
                secret_access_key: String::new(),
                enabled: true,
                groups: vec![],
                permissions: vec![],
            },
        ));
        diff.users_to_delete.push((2, "c".into()));
        diff.mapping_rules = MappingRulesAction::ReplaceWith(vec![]);

        let s = summarise_diff(&diff);
        assert_eq!(s.users_created, 1);
        assert_eq!(s.users_updated, 1);
        assert_eq!(s.users_deleted, 1);
        assert_eq!(s.groups_created, 0);
        // ReplaceWith([]) is a real action (clears the table) but reports 0 rows.
        assert_eq!(s.mapping_rules_replaced, 0);
        // Users changed, so this is NOT a no-op.
        assert!(!s.no_changes);
    }

    #[test]
    fn summarise_diff_empty_is_no_changes() {
        let s = summarise_diff(&IamDiff::default());
        assert!(s.no_changes);
        assert_eq!(s.users_created, 0);
        assert_eq!(s.mapping_rules_replaced, 0);
    }

    #[test]
    fn summarise_diff_keep_rules_reports_zero() {
        let diff = IamDiff {
            mapping_rules: MappingRulesAction::Keep,
            ..IamDiff::default()
        };
        assert_eq!(summarise_diff(&diff).mapping_rules_replaced, 0);
        assert!(summarise_diff(&diff).no_changes);
    }

    #[test]
    fn summarise_stats_mirrors_reconcile_counts() {
        let stats = ReconcileStats {
            users_created: vec!["a".into(), "b".into()],
            groups_deleted: vec!["g".into()],
            mapping_rules_replaced: 3,
            ..ReconcileStats::default()
        };
        let s = summarise_stats(&stats);
        assert_eq!(s.users_created, 2);
        assert_eq!(s.groups_deleted, 1);
        assert_eq!(s.mapping_rules_replaced, 3);
        assert!(!s.no_changes);
    }
}
