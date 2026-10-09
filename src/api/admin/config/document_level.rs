// SPDX-License-Identifier: BUSL-1.1

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
//! The validate and apply handlers feed the shared write pipeline
//! ([`super::write`]); this file owns the document parse
//! (`parse_and_validate_yaml`) and the response shapes.

use crate::api::admin::extract::{AdminJson, AdminQuery};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use super::super::{audit_log, AdminError, AdminState, Bare};
use super::write::{
    self, Built, ConfigWrite, EnvRefs, Mode, Outcome, Rejection, ScrubEnv, Stage, Surface,
    Warnings, WriteResult,
};
use super::{unknown_section_error, SectionName};
use axum::response::Response;

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
    /// Warnings the RUNNING config already produces (standing problems this
    /// document did not introduce). `warnings` carries only the new ones.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub existing_warnings: Vec<String>,
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
    /// Warnings the RUNNING config already produces (standing problems this
    /// document did not introduce). `warnings` carries only the new ones.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub existing_warnings: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// On a `409` (stale `If-Match`): the document's current version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_version: Option<String>,
    /// Path the config was written to. `None` when persist failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub persisted_path: Option<String>,
    /// Set when the apply worked in memory but the file write failed
    /// (`<path>: <error>`); the GUI explains that the change is lost at restart.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub persist_error: Option<String>,
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
    AdminQuery(query): AdminQuery<SectionFilterQuery>,
) -> Result<Response, AdminError> {
    let cfg = state.config.read().await;
    let redacted = cfg.redact_all_secrets();
    // The version an apply sends back in `If-Match`: of the whole document,
    // or of the one exported section.
    let version =
        super::version::config_version(&cfg, query.section.as_deref().and_then(SectionName::parse));
    drop(cfg);
    let etag = (axum::http::header::ETAG, super::version::etag(&version));

    let Some(section_name) = query.section.as_deref() else {
        // Full document path — unchanged from the pre-Wave-1 behavior.
        let yaml = redacted.to_canonical_yaml().map_err(|e| {
            AdminError::internal(format!("failed to serialize config to YAML: {}", e))
        })?;
        return Ok(yaml_response(yaml, etag));
    };

    // Section-scoped export. We reuse the SectionedConfig projection
    // the full export does, then pick out just the requested slice.
    // Each section serializes as `<name>:\n  ...` — valid standalone
    // YAML that can be edited and posted back via section PUT.
    let section = SectionName::parse(section_name)
        .ok_or_else(|| AdminError::not_found(unknown_section_error(section_name)))?;
    let sectioned = crate::config_sections::SectionedConfig::from_flat(&redacted);
    let value = match section {
        SectionName::Admission => serde_yaml::to_value(sectioned.admission.unwrap_or_default()),
        SectionName::Access => serde_yaml::to_value(sectioned.access),
        SectionName::Storage => serde_yaml::to_value(sectioned.storage),
        SectionName::Advanced => serde_yaml::to_value(sectioned.advanced),
    };
    let yaml_value =
        value.map_err(|e| AdminError::internal(format!("failed to serialize section: {}", e)))?;
    let mut map = serde_yaml::Mapping::new();
    map.insert(
        serde_yaml::Value::String(section.as_str().to_string()),
        yaml_value,
    );
    let s = serde_yaml::to_string(&serde_yaml::Value::Mapping(map))
        .map_err(|e| AdminError::internal(format!("failed to serialize section to YAML: {}", e)))?;
    Ok(yaml_response(s, etag))
}

/// A 200 `application/yaml` body with the given `ETag`.
fn yaml_response(
    yaml: String,
    etag: (axum::http::HeaderName, axum::http::HeaderValue),
) -> Response {
    (
        StatusCode::OK,
        [
            (
                axum::http::header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("application/yaml"),
            ),
            etag,
        ],
        yaml,
    )
        .into_response()
}

/// `GET /api/admin/config/defaults[?section=<name>]` — JSON Schema for
/// the Config type, optionally scoped to one section.
///
/// Default (no `section=`) returns the schema of the whole canonical
/// (sectioned) document. With `?section=admission|access|storage|advanced`, the
/// response is the JSON Schema for just that section's type — exactly
/// what `monaco-yaml` needs when the UI's Monaco editor is bound to
/// one section's scope. Wave 2 of the admin UI plan reads this for
/// per-section YAML linting.
pub async fn config_defaults(
    AdminQuery(query): AdminQuery<SectionFilterQuery>,
) -> Result<Response, AdminError> {
    let v = match query.section.as_deref() {
        None => crate::cli::config::canonical_schema(),
        Some(name) => SectionName::parse(name)
            .and_then(|s| crate::cli::config::section_schema(s.as_str()))
            .ok_or_else(|| AdminError::not_found(unknown_section_error(name)))?,
    };
    Ok((
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "application/schema+json")],
        Json(v),
    )
        .into_response())
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
fn parse_and_validate_yaml(
    yaml: &str,
    known_env: &std::collections::BTreeMap<String, String>,
) -> Result<(crate::config::Config, Vec<String>), String> {
    if yaml.trim().is_empty() {
        return Err(EMPTY_DOCUMENT.to_string());
    }
    // Expand `${env:NAME}` refs, but ONLY names the running config already
    // resolved from its file (S7: see `expand_env_admin`), and record the
    // provenance so persist/export re-emit the refs instead of the values.
    // The `config apply` CLI expands client-side against the OPERATOR env
    // and escapes `$`, so this pass leaves its text unchanged.
    let (yaml, env_refs) = crate::config::expand_env_admin(yaml, known_env).map_err(|e| {
        format!(
            "env expansion error: {e}. The admin API resolves only `${{env:NAME}}` refs \
             that the config file loaded at boot already uses, or names listed in \
             {}; reference a new variable in that file, allowlist it, or expand it \
             client-side (`config apply`).",
            crate::config::CONFIG_ENV_ALLOWLIST_VAR
        )
    })?;
    let scrub = |m: String| crate::config::scrub_env_values(&m, &env_refs);
    use crate::config::DocumentRefusal as R;
    let (mut cfg, warnings) =
        crate::config::Config::validate_document(&yaml).map_err(|refusal| {
            scrub(match refusal {
                R::Empty => EMPTY_DOCUMENT.to_string(),
                R::Parse(e) => format!("YAML parse error: {}", e),
                R::LogFilter(filter) => format!(
                    "invalid log_level filter '{filter}': expected a tracing-subscriber \
                     EnvFilter (e.g. 'info', 'deltaglider_proxy=debug')"
                ),
                R::Fatal(fatal) => format!("config refused: {}", fatal.join("; ")),
            })
        })?;
    cfg.env_refs = env_refs.clone();
    cfg.record_env_ref_paths();
    // The rule gates need the RUNNING config (a new defect vs. a standing
    // one): the write pipeline runs them.
    Ok((cfg, warnings.into_iter().map(scrub).collect()))
}

const EMPTY_DOCUMENT: &str = "empty YAML body: apply requires a full canonical config document. \
     Refusing to reset every field to its default.";

/// `POST /api/admin/config/validate` — dry-run.
///
/// Parses the YAML body, runs the write pipeline's validation steps, and
/// reports warnings or errors. No runtime state is mutated. Used by CI and
/// by the admin GUI's pre-apply confirmation modal.
pub async fn validate_config_doc(
    State(state): State<Arc<AdminState>>,
    AdminJson(body): AdminJson<ConfigDocumentRequest>,
) -> impl IntoResponse {
    let known = (*state.config.read().await.env_refs).clone();
    let no_env = EnvRefs::new();
    let write = ConfigWrite {
        surface: Surface::Document { yaml: &body.yaml },
        mode: Mode::DryRun,
        headers: None,
        extra_env: &no_env,
    };
    shape_validate(run_document(&state, &body.yaml, known, write).await)
}

/// Parse a document and run it through the write pipeline: the one body of
/// `/config/validate` and `/config/apply`, so both carry the same
/// parse-time warnings.
async fn run_document(
    state: &Arc<AdminState>,
    yaml: &str,
    known: EnvRefs,
    write: ConfigWrite<'_>,
) -> WriteResult {
    match parse_and_validate_yaml(yaml, &known) {
        Err(err) => WriteResult {
            outcome: Outcome::Rejected(Rejection::new(Stage::Build, StatusCode::BAD_REQUEST, err)),
            refs: known,
        },
        Ok((incoming, warnings)) => {
            write::run(state, write, |_| Ok(Built { incoming, warnings })).await
        }
    }
}

/// The warnings a refused document write reports, for validate and apply.
fn rejection_warnings(stage: Stage, w: Warnings) -> Vec<String> {
    match stage {
        Stage::Transition => [w.check_new, w.preserve].concat(),
        // The document's parse-time warnings ride along.
        Stage::Preserve | Stage::Check | Stage::Gate => w.build,
        _ => Vec::new(),
    }
}

/// The `/config/validate` body of a write outcome.
fn shape_validate(result: WriteResult) -> Response {
    let WriteResult { outcome, refs } = result;
    let refused = |status, error, warnings| {
        let body = ConfigValidateResponse {
            ok: false,
            warnings,
            existing_warnings: Vec::new(),
            error: Some(error),
        };
        write::respond(status, body, &refs, None)
    };
    match outcome {
        Outcome::Rejected(r) => {
            refused(r.status, r.error, rejection_warnings(r.stage, *r.warnings))
        }
        Outcome::Validated { warnings: w, .. } => {
            let body = ConfigValidateResponse {
                ok: true,
                warnings: [w.check_new, w.preserve, w.env].concat(),
                existing_warnings: w.existing,
                error: None,
            };
            write::respond(StatusCode::OK, body, &refs, None)
        }
        // A dry run neither checks a version nor applies.
        Outcome::Conflict { .. } | Outcome::Applied { .. } => refused(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal: a dry run applied".to_string(),
            Vec::new(),
        ),
    }
}

/// The encryption-key presence probe of a document's `storage:` block: the
/// preservation step must tell an absent `key:` (keep) from `key: null`
/// (disable), which the parsed config cannot.
pub(super) fn document_probe(yaml: &str) -> super::section_level::BackendEncryptionKeyProbe {
    let storage = serde_yaml::from_str::<serde_json::Value>(yaml)
        .ok()
        .and_then(|v| v.get("storage").cloned())
        .unwrap_or(serde_json::Value::Null);
    super::section_level::BackendEncryptionKeyProbe::from_section(SectionName::Storage, &storage)
}

/// `POST /api/admin/config/apply` — atomic full-document apply through the
/// write pipeline (see `super::write`): parse and validate, merge runtime
/// secrets forward, refuse a bootstrap-hash change (the legitimate path is
/// `PUT /password`), transition, persist. A persist failure answers 500 with
/// the in-memory state applied, so an operator can retry.
pub async fn apply_config_doc(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
    AdminJson(body): AdminJson<ConfigDocumentRequest>,
) -> impl IntoResponse {
    // Thin HTTP wrapper. The pipeline lives in `apply_config_inner` so a sibling
    // mutation path (backup restore) can call it directly and read the TYPED
    // result instead of re-parsing this handler's own HTTP response body.
    let (status, resp) = apply_config_inner(&state, &headers, body).await;
    // The document's version after the apply (or the current one on a 409),
    // for the client's next `If-Match`.
    let version = super::version::config_version(&*state.config.read().await, None);
    (
        status,
        [(axum::http::header::ETAG, super::version::etag(&version))],
        Json(resp),
    )
}

/// The full config apply as a typed call (no HTTP extractors). Returns the
/// status and the scrubbed [`ConfigApplyResponse`] for every arm.
/// `apply_config_doc` is the thin route handler; `backup.rs` calls this.
pub(crate) async fn apply_config_inner(
    state: &Arc<AdminState>,
    headers: &HeaderMap,
    body: ConfigDocumentRequest,
) -> (StatusCode, ConfigApplyResponse) {
    apply_config_inner_with_env(state, headers, body, &Default::default()).await
}

/// [`apply_config_inner`] that may also resolve the `${env:NAME}` refs in
/// `extra_env` (`name → value`): a backup restore passes the refs it made
/// for values this host's env already supplies (see `hydrate_restore_doc`).
pub(crate) async fn apply_config_inner_with_env(
    state: &Arc<AdminState>,
    headers: &HeaderMap,
    body: ConfigDocumentRequest,
    extra_env: &std::collections::BTreeMap<String, String>,
) -> (StatusCode, ConfigApplyResponse) {
    let mut known = (*state.config.read().await.env_refs).clone();
    known.extend(extra_env.iter().map(|(k, v)| (k.clone(), v.clone())));
    // Parse before the lock (pure work): a bad document answers 400 even
    // with a stale `If-Match`.
    let write = ConfigWrite {
        surface: Surface::Document { yaml: &body.yaml },
        mode: Mode::Apply,
        headers: Some(headers),
        extra_env,
    };
    let WriteResult { outcome, refs } = run_document(state, &body.yaml, known, write).await;
    let (status, mut resp) = shape_apply(outcome);
    resp.scrub_env(&refs);
    (status, resp)
}

/// The `/config/apply` body of a write outcome (not yet scrubbed).
fn shape_apply(outcome: Outcome) -> (StatusCode, ConfigApplyResponse) {
    let refused = |error: String, warnings: Vec<String>| ConfigApplyResponse {
        applied: false,
        persisted: false,
        requires_restart: false,
        warnings,
        existing_warnings: Vec::new(),
        error: Some(error),
        current_version: None,
        persisted_path: None,
        persist_error: None,
    };
    match outcome {
        Outcome::Conflict { current } => {
            let mut resp = refused(
                "config_conflict: the config changed after you loaded it (another tab, \
                 another admin, or a GitOps apply). Export it again and re-apply your edits."
                    .to_string(),
                Vec::new(),
            );
            resp.current_version = Some(current);
            (StatusCode::CONFLICT, resp)
        }
        Outcome::Rejected(r) => {
            let error = match r.stage {
                Stage::Transition => {
                    format!("Config transition refused (no state changed): {}", r.error)
                }
                _ => r.error,
            };
            let warnings = rejection_warnings(r.stage, *r.warnings);
            (r.status, refused(error, warnings))
        }
        Outcome::Applied {
            warnings: w,
            restart,
            persist,
            ..
        } => {
            // `persist_to_file` is atomic (tempfile + rename); the write can
            // still fail (permissions, disk full): `persisted: false` + 500,
            // so a GitOps pipeline never mistakes it for a clean apply.
            let (persisted_path, status, persist_error, persist_warning) = match persist {
                Ok(path) => (Some(path), StatusCode::OK, None, None),
                Err((path, e)) => (
                    None,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Some(format!("{path}: {e}")),
                    Some(format!(
                        "Applied in memory but FAILED to persist to {path}: {e}. Server will \
                         revert to the on-disk config on next restart — fix the underlying IO \
                         problem and re-apply."
                    )),
                ),
            };
            let resp = ConfigApplyResponse {
                applied: true,
                persisted: persisted_path.is_some(),
                requires_restart: !restart.is_empty(),
                warnings: [w.check_new, w.preserve, w.env, w.transition]
                    .concat()
                    .into_iter()
                    .chain(persist_warning)
                    .collect(),
                existing_warnings: w.existing,
                error: None,
                current_version: None,
                persisted_path,
                persist_error,
            };
            (status, resp)
        }
        Outcome::Validated { .. } => (
            StatusCode::INTERNAL_SERVER_ERROR,
            refused(
                "internal: an apply ran as a dry run".to_string(),
                Vec::new(),
            ),
        ),
    }
}

impl ScrubEnv for ConfigApplyResponse {
    fn scrub_env(&mut self, refs: &EnvRefs) {
        write::scrub_strings(
            self.warnings
                .iter_mut()
                .chain(&mut self.existing_warnings)
                .chain(&mut self.error),
            refs,
        );
    }
}

impl ScrubEnv for ConfigValidateResponse {
    fn scrub_env(&mut self, refs: &EnvRefs) {
        write::scrub_strings(
            self.warnings
                .iter_mut()
                .chain(&mut self.existing_warnings)
                .chain(&mut self.error),
            refs,
        );
    }
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
    AdminQuery(q): AdminQuery<ExportIamQuery>,
    headers: HeaderMap,
) -> Result<Response, AdminError> {
    let db_arc = state
        .config_db
        .as_ref()
        .ok_or_else(AdminError::no_config_db)?;
    let db = db_arc.lock().await;
    // `include_secrets=true` produces a lossless, round-trippable full-IAM file
    // (the "Export full IAM (YAML)" affordance). The file then contains LIVE
    // credentials — the UI warns the operator and the route is admin-gated.
    let snapshot = crate::iam::export_as_declarative_inner(&db, q.include_secrets)
        .map_err(|e| AdminError::internal(format!("export_as_declarative: {}", e)))?;
    // #71 review: the reconciler keys users by NAME, so a same-name pair cannot
    // be represented in this YAML. User names are unique since schema v25, so
    // this is a safety check: refuse rather than hand back a file that cannot
    // be re-imported.
    if let Some(name) = crate::iam::duplicate_user_name(&snapshot) {
        return Err(AdminError::conflict(format!(
            "full-IAM export cannot represent two users named '{name}': the reconciler \
                 keys users by name, so this database's same-name pair would not round-trip. \
                 Use the admin backup (POST /_/api/admin/backup) for a lossless artifact, or \
                 rename one of the users."
        )));
    }
    drop(db);
    if q.include_secrets {
        audit_log("export_iam_with_secrets", "admin", "", &headers);
    }

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
    // #71: OAuth bindings ride along so the lossless export is genuinely
    // lossless — without them, a DB wipe re-provisions duplicate users on
    // the next OAuth login. (Empty for redacted exports; skip_serializing
    // keeps hand-authored YAML clean either way.)
    access_map.insert(
        serde_yaml::Value::String("external_identities".into()),
        serde_yaml::to_value(&snapshot.external_identities).unwrap_or(serde_yaml::Value::Null),
    );

    let mut root = serde_yaml::Mapping::new();
    root.insert(
        serde_yaml::Value::String("access".into()),
        serde_yaml::Value::Mapping(access_map),
    );

    let yaml = serde_yaml::to_string(&serde_yaml::Value::Mapping(root))
        .map_err(|e| AdminError::internal(format!("serialize declarative IAM to YAML: {}", e)))?;
    Ok((
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "application/yaml")],
        yaml,
    )
        .into_response())
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
    /// OAuth login bindings upserted (#71).
    pub external_identities_applied: usize,
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
        &access.external_identities,
    ))
}

/// `POST /_/api/admin/config/declarative-iam-validate` — dry-run a full-IAM
/// YAML import: parse + diff against the live DB, return the change summary
/// WITHOUT touching state. Powers the Apply-dialog preview.
pub async fn validate_declarative_iam(
    State(state): State<Arc<AdminState>>,
    AdminJson(body): AdminJson<ConfigDocumentRequest>,
) -> Result<Json<IamImportSummary>, AdminError> {
    let db_arc = state
        .config_db
        .as_ref()
        .ok_or_else(AdminError::no_config_db)?;
    let snapshot = parse_iam_yaml(&body.yaml).map_err(AdminError::invalid)?;
    let db = db_arc.lock().await;
    let diff = crate::iam::preview_declarative_iam(&db, &snapshot)
        .map_err(|e| AdminError::invalid(format!("validation failed: {e}")))?;
    Ok(Json(summarise_diff(&diff)))
}

/// `POST /_/api/admin/config/declarative-iam-apply` — apply a full-IAM YAML
/// import: parse, reconcile atomically, rebuild the index, sync, audit.
pub async fn apply_declarative_iam(
    State(state): State<Arc<AdminState>>,
    headers: HeaderMap,
    AdminJson(body): AdminJson<ConfigDocumentRequest>,
) -> Result<Json<IamImportSummary>, AdminError> {
    let db_arc = state
        .config_db
        .as_ref()
        .ok_or_else(AdminError::no_config_db)?;
    let snapshot = parse_iam_yaml(&body.yaml).map_err(AdminError::invalid)?;

    let db = db_arc.lock().await;
    let stats = crate::iam::reconcile_declarative_iam(&db, &snapshot)
        .map_err(|e| AdminError::invalid(format!("IAM import failed (no state changed): {e}")))?;
    // Rebuild the in-memory index from the now-committed DB. Use the
    // `_declarative` variant (bumps IAM_VERSION for test barriers AND skips the
    // legacy-admin auto-migration): a full-IAM YAML import is authoritative for
    // the entire IAM set, so we must not silently auto-author a `legacy-admin`
    // row the imported document didn't declare — same contract as the
    // declarative config-apply path (config/mod.rs).
    if let Err(e) = super::super::users::rebuild_iam_index_declarative::<Bare>(
        &db,
        &state.iam_state,
        state.iam_state.load().when_empty(),
    ) {
        // The message names the status only, as it always did.
        return Err(AdminError::internal(format!(
            "rebuild_iam_index after IAM import: {:?}",
            e.status_code()
        )));
    }
    drop(db);
    if stats.providers_changed() {
        if let Err(e) = super::super::external_auth::rebuild_external_auth(&state).await {
            return Err(AdminError::internal(format!(
                "rebuild_external_auth after IAM import: {:?}",
                e.status_code()
            )));
        }
    }

    if !stats.is_noop() {
        super::super::trigger_config_sync(&state);
    }
    for (action, names) in stats.audit_entries() {
        for name in names {
            audit_log(action, "iam-yaml-import", name, &headers);
        }
    }
    tracing::info!("[iam-yaml-import] {}", stats.summary_line());

    Ok(Json(summarise_stats(&stats)))
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
            crate::iam::MappingRulesAction::ClearAll(n) => *n,
            crate::iam::MappingRulesAction::Keep => 0,
        },
        external_identities_applied: diff.external_identities.len(),
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
        && s.mapping_rules_replaced == 0
        && s.external_identities_applied == 0
        && diff.mapping_rules.is_noop();
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
        external_identities_applied: stats.external_identities_applied,
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
    fn summarise_diff_reports_a_mapping_rule_wipe() {
        let diff = IamDiff {
            mapping_rules: crate::iam::MappingRulesAction::ClearAll(6),
            ..IamDiff::default()
        };
        let s = summarise_diff(&diff);
        assert!(
            !s.no_changes,
            "a wipe of every mapping rule reads as no change"
        );
        assert_eq!(
            s.mapping_rules_replaced, 6,
            "the preview hides the deleted rules"
        );
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
            auth_source: None,
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
                auth_source: None,
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
    fn summarise_diff_bindings_only_is_not_no_changes() {
        // A full-IAM import that only restores OAuth bindings (users/groups
        // match, rules Keep) writes rows — it must not report no_changes.
        use crate::iam::DeclarativeExternalIdentity as EI;
        let diff = IamDiff {
            external_identities: vec![EI {
                user: "dana".into(),
                provider: "okta".into(),
                subject: "sub".into(),
                email: None,
                display_name: None,
                email_verified: None,
                raw_claims: None,
            }],
            ..IamDiff::default()
        };
        let s = summarise_diff(&diff);
        assert!(!s.no_changes, "bindings-only import is a real change");
        assert_eq!(s.external_identities_applied, 1);
    }

    #[test]
    fn summarise_diff_empty_bindings_is_no_changes() {
        let diff = IamDiff::default();
        let s = summarise_diff(&diff);
        assert!(s.no_changes);
        assert_eq!(s.external_identities_applied, 0);
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

    /// Doc-apply CRITICAL: a redacted export→apply (backend key masked to
    /// None, no `key:` in the YAML) must PRESERVE the running key, not wipe it
    /// (which would make historical objects unreadable + write plaintext).
    #[test]
    fn doc_apply_preserves_redacted_backend_encryption_key() {
        use crate::config::BackendEncryptionConfig as E;
        let real_key = "a".repeat(64);
        let current = crate::config::Config {
            backend_encryption: E::Aes256GcmProxy {
                key: Some(real_key.clone()),
                key_id: Some("k1".into()),
                legacy_key: None,
                legacy_key_id: None,
            },
            ..Default::default()
        };
        // Incoming: same mode, key redacted to None; YAML has no `key:` field.
        let mut incoming = crate::config::Config {
            backend_encryption: E::Aes256GcmProxy {
                key: None,
                key_id: Some("k1".into()),
                legacy_key: None,
                legacy_key_id: None,
            },
            ..Default::default()
        };
        let yaml = "storage:\n  backend_encryption:\n    mode: aes256-gcm-proxy\n    key_id: k1\n";
        super::super::preserve_runtime_secrets(&mut incoming, &current, &document_probe(yaml))
            .unwrap();
        match incoming.backend_encryption {
            E::Aes256GcmProxy { key, .. } => assert_eq!(
                key,
                Some(real_key),
                "redacted key must be restored from the running config"
            ),
            other => panic!("mode changed unexpectedly: {other:?}"),
        }
    }

    /// The escape hatch still works: explicit `key: null` in the YAML DISABLES
    /// preservation (operator intent to clear), not a redacted-absent.
    #[test]
    fn doc_apply_explicit_null_key_is_not_preserved() {
        use crate::config::BackendEncryptionConfig as E;
        let current = crate::config::Config {
            backend_encryption: E::Aes256GcmProxy {
                key: Some("b".repeat(64)),
                key_id: None,
                legacy_key: None,
                legacy_key_id: None,
            },
            ..Default::default()
        };
        let mut incoming = crate::config::Config {
            backend_encryption: E::Aes256GcmProxy {
                key: None,
                key_id: None,
                legacy_key: None,
                legacy_key_id: None,
            },
            ..Default::default()
        };
        let yaml = "storage:\n  backend_encryption:\n    mode: aes256-gcm-proxy\n    key: null\n";
        super::super::preserve_runtime_secrets(&mut incoming, &current, &document_probe(yaml))
            .unwrap();
        match incoming.backend_encryption {
            E::Aes256GcmProxy { key, .. } => {
                assert_eq!(key, None, "explicit null must NOT be preserved")
            }
            other => panic!("unexpected: {other:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `/config/validate` must refuse what boot and apply refuse.
    #[test]
    fn validate_rejects_route_to_undefined_backend() {
        let err = parse_and_validate_yaml(
            "storage:\n  buckets:\n    releases: { backend: hetzner-fsn1 }\n",
            &Default::default(),
        )
        .expect_err("fatal config must not validate");
        assert!(err.contains("undefined backend 'hetzner-fsn1'"), "{err}");
    }

    /// `config lint` and `/config/validate` run the same validation step:
    /// one YAML corpus through both gives the same verdict (the validate
    /// side against a default running config, as lint has none).
    #[test]
    fn lint_and_validate_give_the_same_verdict() {
        let corpus: &[(&str, &str, bool)] = &[
            ("minimal", "storage:\n  filesystem: /var/dgp\n", true),
            ("empty", "", false),
            ("whitespace", "  \n\n", false),
            ("parse error", "advanced: [unclosed\n", false),
            ("unknown field", "advanced:\n  no_such_field: 1\n", false),
            ("bad log filter", "advanced:\n  log_level: \"not==valid\"\n", false),
            (
                "route to undefined backend",
                "storage:\n  buckets:\n    releases: { backend: hetzner-fsn1 }\n",
                false,
            ),
            (
                "duplicate admission block",
                "admission:\n  blocks:\n    - name: dup\n      match: {}\n      action: deny\n    \
                 - name: dup\n      match: {}\n      action: deny\n",
                false,
            ),
            (
                "public and prefixes",
                "storage:\n  buckets:\n    releases:\n      public: true\n      \
                 public_prefixes: [\"x/\"]\n",
                false,
            ),
            (
                "lifecycle delete without expire_after",
                "storage:\n  lifecycle:\n    enabled: true\n    rules:\n      - name: r\n        \
                 bucket: releases\n        prefix: \"\"\n",
                false,
            ),
            (
                "duplicate replication rule",
                "storage:\n  replication:\n    rules:\n      - name: r\n        source:\n          \
                 bucket: releases\n        destination:\n          bucket: downloads\n        \
                 interval: 1h\n      - name: r\n        source:\n          bucket: releases\n        \
                 destination:\n          bucket: db-archive\n        interval: 1h\n",
                false,
            ),
            ("warning only", "advanced:\n  max_delta_ratio: 0.99\n", true),
        ];
        let dir = tempfile::tempdir().unwrap();
        let running = crate::config::Config::default();
        let no_env = EnvRefs::new();
        for (name, yaml, ok) in corpus {
            let path = dir.path().join("cfg.yaml");
            std::fs::write(&path, yaml).unwrap();
            let lint_ok =
                crate::cli::config::lint(path.to_str().unwrap()) == crate::cli::config::EXIT_OK;
            let write = ConfigWrite {
                surface: Surface::Document { yaml },
                mode: Mode::DryRun,
                headers: None,
                extra_env: &no_env,
            };
            let validate_ok = parse_and_validate_yaml(yaml, &no_env)
                .map_err(|e| Rejection::new(Stage::Build, StatusCode::BAD_REQUEST, e))
                .and_then(|(incoming, warnings)| {
                    write::prepare(&running, Built { incoming, warnings }, &write)
                })
                .is_ok();
            assert_eq!(lint_ok, validate_ok, "{name}: lint and validate disagree");
            assert_eq!(validate_ok, *ok, "{name}: unexpected verdict");
        }
    }
}

#[cfg(test)]
mod env_leak_tests {
    use super::*;

    /// S7: the admin document path must not read arbitrary server env vars.
    /// `HOME` is set in every test environment and is not referenced by any
    /// loaded config, so its value must never appear in the result.
    #[test]
    fn admin_doc_does_not_expand_unreferenced_env() {
        let home = std::env::var("HOME").expect("HOME is set");
        let res = parse_and_validate_yaml("log_level: \"${env:HOME}\"\n", &Default::default());
        match res {
            Ok((cfg, w)) => {
                assert_ne!(cfg.log_level, home);
                assert!(!w.iter().any(|w| w.contains(&home)));
            }
            Err(e) => assert!(!e.contains(&home), "leaked HOME: {e}"),
        }
    }

    /// A name the loaded file already uses still resolves (the IaC
    /// round-trip), and its value is scrubbed from error text.
    #[test]
    fn admin_doc_resolves_known_refs_and_scrubs_errors() {
        let known: std::collections::BTreeMap<String, String> =
            [("LOGF".to_string(), "/not/a/filter".to_string())].into();
        let err = parse_and_validate_yaml("log_level: \"${env:LOGF}\"\n", &known).unwrap_err();
        assert!(!err.contains("/not/a/filter"), "{err}");
        assert!(err.contains("${env:LOGF}"), "{err}");
        let known: std::collections::BTreeMap<String, String> =
            [("LOGF".to_string(), "debug".to_string())].into();
        let (cfg, _) = parse_and_validate_yaml("log_level: \"${env:LOGF}\"\n", &known).unwrap();
        assert_eq!(cfg.log_level, "debug");
        assert_eq!(cfg.env_refs.get("LOGF").map(String::as_str), Some("debug"));
    }
}

#[cfg(test)]
mod review2_tests {
    use super::*;

    /// Review-2 (S7): an export keeps `${env:X}` refs. Importing it on a
    /// fresh/DR instance whose boot file does not use X fails, even when the
    /// operator exports X there (the documented IaC contract).
    ///
    /// Lead decision: S7 stays; the operator opts a name in through
    /// DGP_CONFIG_ENV_ALLOWLIST. Without it the ref fails, and the error
    /// names the opt-in.
    #[test]
    fn review2_foreign_export_ref_resolves_when_target_sets_the_var() {
        std::env::set_var("REVIEW2_S7_DR_LEVEL", "debug");
        let doc = "log_level: \"${env:REVIEW2_S7_DR_LEVEL}\"\n";
        let refused = parse_and_validate_yaml(doc, &Default::default());
        std::env::set_var(crate::config::CONFIG_ENV_ALLOWLIST_VAR, "REVIEW2_S7_*");
        let r = parse_and_validate_yaml(doc, &Default::default());
        std::env::remove_var(crate::config::CONFIG_ENV_ALLOWLIST_VAR);
        std::env::remove_var("REVIEW2_S7_DR_LEVEL");
        let err = refused.expect_err("S7: not allowlisted, not resolved");
        assert!(err.contains("DGP_CONFIG_ENV_ALLOWLIST"), "{err}");
        assert!(r.is_ok(), "{:?}", r.err());
    }
}
