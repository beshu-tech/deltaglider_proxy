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
    // preserved by the per-backend secret-preservation path in
    // Step 6 (TODO).
    if incoming.bootstrap_password_hash.is_none() {
        incoming.bootstrap_password_hash = current.bootstrap_password_hash.clone();
    }

    // Top-level SigV4 creds. Both-or-neither semantics: if the operator sets
    // only one half of the pair, preserving the other half from runtime
    // would cross-wire a new key id onto the old secret (or vice versa),
    // producing broken but auth-enabled state. Warn loudly instead.
    preserve_sigv4_pair(
        &mut incoming.access_key_id,
        &mut incoming.secret_access_key,
        &current.access_key_id,
        &current.secret_access_key,
        "proxy-level",
        &mut warnings,
    );

    // Primary backend creds: only when BOTH old and new are S3 can we carry
    // creds forward. Any other transition discards them — and we warn so
    // the operator can re-supply them in the incoming doc.
    match (&mut incoming.backend, &current.backend) {
        (
            crate::config::BackendConfig::S3 {
                access_key_id: new_akid,
                secret_access_key: new_sk,
                ..
            },
            crate::config::BackendConfig::S3 {
                access_key_id: old_akid,
                secret_access_key: old_sk,
                ..
            },
        ) => {
            preserve_sigv4_pair(
                new_akid,
                new_sk,
                old_akid,
                old_sk,
                "primary backend",
                &mut warnings,
            );
        }
        (
            crate::config::BackendConfig::Filesystem { .. },
            crate::config::BackendConfig::S3 {
                access_key_id: old_akid,
                secret_access_key: old_sk,
                ..
            },
        ) if old_akid.is_some() || old_sk.is_some() => {
            warnings.push(
                "primary backend switched from S3 to filesystem — previous S3 credentials are dropped".to_string(),
            );
        }
        (
            crate::config::BackendConfig::S3 {
                access_key_id: new_akid,
                secret_access_key: new_sk,
                ..
            },
            crate::config::BackendConfig::Filesystem { .. },
        ) if new_akid.is_none() && new_sk.is_none() => {
            warnings.push(
                "primary backend switched from filesystem to S3 but incoming YAML has no credentials — the new backend will rely on instance / env credentials only".to_string(),
            );
        }
        _ => {}
    }

    // Named backends: match by name + type. Any backend present in the old
    // config that disappears from the new (rename or removal) drops its
    // creds; we warn. A backend whose name matches but whose type switched
    // (S3 ↔ filesystem) also drops creds; we warn.
    // Collect owned names up front so the later mutation of
    // `incoming.backends` doesn't clash with the borrow used to build the
    // new-name set used further down.
    let old_by_name: std::collections::HashMap<&str, &crate::config::BackendConfig> = current
        .backends
        .iter()
        .map(|n| (n.name.as_str(), &n.backend))
        .collect();
    let new_names: std::collections::HashSet<String> =
        incoming.backends.iter().map(|n| n.name.clone()).collect();

    for new_named in &mut incoming.backends {
        let old_backend = old_by_name.get(new_named.name.as_str());
        match (&mut new_named.backend, old_backend) {
            (
                crate::config::BackendConfig::S3 {
                    access_key_id: new_akid,
                    secret_access_key: new_sk,
                    ..
                },
                Some(crate::config::BackendConfig::S3 {
                    access_key_id: old_akid,
                    secret_access_key: old_sk,
                    ..
                }),
            ) => {
                preserve_sigv4_pair(
                    new_akid,
                    new_sk,
                    old_akid,
                    old_sk,
                    &format!("backend '{}'", new_named.name),
                    &mut warnings,
                );
            }
            (
                crate::config::BackendConfig::S3 { .. },
                Some(crate::config::BackendConfig::Filesystem { .. }),
            )
            | (
                crate::config::BackendConfig::Filesystem { .. },
                Some(crate::config::BackendConfig::S3 { .. }),
            ) => {
                warnings.push(format!(
                    "backend '{}' changed type — previous credentials are dropped",
                    new_named.name
                ));
            }
            _ => {}
        }
    }

    // Warn about backends that existed before and vanished (operator renamed
    // or removed them); their creds cannot be preserved even if the new
    // config has similarly-named replacements.
    for old_named in &current.backends {
        let had_creds = matches!(
            &old_named.backend,
            crate::config::BackendConfig::S3 {
                access_key_id: Some(_),
                secret_access_key: Some(_),
                ..
            },
        );
        if had_creds && !new_names.contains(&old_named.name) {
            warnings.push(format!(
                "backend '{}' removed (or renamed) — its credentials are gone from runtime",
                old_named.name
            ));
        }
    }

    warnings
}

/// Preserve a SigV4-style credential pair from `old` into `new` where both
/// halves are absent in the incoming doc. If the operator set exactly one
/// half we refuse to fill the other — cross-wiring a new key id with the
/// old secret (or vice versa) produces a superficially-authenticated state
/// that silently fails at signature verification — and emit a warning so
/// they can supply the missing half.
fn preserve_sigv4_pair(
    new_akid: &mut Option<String>,
    new_sk: &mut Option<String>,
    old_akid: &Option<String>,
    old_sk: &Option<String>,
    label: &str,
    warnings: &mut Vec<String>,
) {
    match (&*new_akid, &*new_sk) {
        (None, None) => {
            *new_akid = old_akid.clone();
            *new_sk = old_sk.clone();
        }
        (Some(_), Some(_)) => {}
        (Some(_), None) => {
            warnings.push(format!(
                "{} credentials are asymmetric in the applied YAML (access_key_id set, secret_access_key missing) — not cross-wiring the runtime secret; authentication will fail until both are supplied",
                label
            ));
        }
        (None, Some(_)) => {
            warnings.push(format!(
                "{} credentials are asymmetric in the applied YAML (secret_access_key set, access_key_id missing) — not cross-wiring the runtime key id; authentication will fail until both are supplied",
                label
            ));
        }
    }
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
pub async fn export_declarative_iam(State(state): State<Arc<AdminState>>) -> impl IntoResponse {
    let Some(db_arc) = state.config_db.as_ref() else {
        return (
            StatusCode::NOT_FOUND,
            "config DB not initialised — nothing to export".to_string(),
        )
            .into_response();
    };
    let db = db_arc.lock().await;
    let snapshot = match crate::iam::export_as_declarative(&db) {
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
