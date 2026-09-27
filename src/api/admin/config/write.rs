// SPDX-License-Identifier: BUSL-1.1

//! THE config write pipeline. The field-level PATCH (`PUT /config`), the
//! section PUT / validate and the document apply / validate all feed it: a
//! handler only builds the `incoming` config (patch, merge-patch, or parse)
//! and shapes the typed outcome into its own response body.
//!
//! One ordered step list, run by [`run`]:
//!
//! 1. `If-Match` check (apply only) — under the write lock.
//! 2. build `incoming` (the handler's closure).
//! 3. [`prepare`] (pure): bootstrap-hash guard → env-ref provenance →
//!    secret preservation → env overrides re-applied → shorthand
//!    normalisation → `check_all` + warning split → lifecycle and
//!    replication gates → section diff.
//! 4. dry run: stop (the section validate adds the declarative-IAM preview).
//! 5. apply: [`super::apply_config_transition`] → persist → audit.
//!
//! The surfaces differ only where their wire contract differs; every
//! difference is one row of [`Steps`], never a copied step.
//!
//! Every response body goes through [`respond`], which runs the S7 env-value
//! scrub on the typed body ([`ScrubEnv`]) first, so no surface can echo a
//! resolved `${env:NAME}` value.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;

use super::super::AdminState;
use super::{SectionName, TransitionCtx};
use crate::config::Config;

pub(super) type EnvRefs = BTreeMap<String, String>;

/// Which surface feeds the pipeline, with the raw body the steps that must
/// tell an absent key from `null` read.
#[derive(Clone, Copy)]
pub(super) enum Surface<'a> {
    /// `PUT /config`: the running config with the field patch applied.
    Patch,
    /// `PUT|POST /config/section/:name[/validate]`: the merge-patch body.
    Section {
        section: SectionName,
        body: &'a serde_json::Value,
    },
    /// `POST /config/apply|validate`: the YAML document.
    Document { yaml: &'a str },
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    DryRun,
    Apply,
}

/// The per-surface step table: the only place the surfaces differ.
struct Steps {
    /// Refuse a changed `bootstrap_password_hash` (403).
    bootstrap_guard: bool,
    /// Secret preservation + `check_all` + the lifecycle/replication gates.
    /// PATCH starts from the running config and validates its own fields.
    validate: bool,
    /// `normalize_shorthands` (the document parser already runs it).
    normalize: bool,
    /// Carry the running config's env-ref provenance into the document.
    merge_env_refs: bool,
}

impl Surface<'_> {
    fn steps(self, mode: Mode) -> Steps {
        match self {
            Surface::Patch => Steps {
                bootstrap_guard: false,
                validate: false,
                normalize: false,
                merge_env_refs: false,
            },
            Surface::Section { .. } => Steps {
                bootstrap_guard: true,
                validate: true,
                normalize: true,
                merge_env_refs: false,
            },
            // Kept as on the wire before the pipeline existed: the document
            // validate neither refuses a hash change nor merges env refs
            // (only the apply does).
            Surface::Document { .. } => Steps {
                bootstrap_guard: mode == Mode::Apply,
                validate: true,
                normalize: false,
                merge_env_refs: mode == Mode::Apply,
            },
        }
    }

    fn section(self) -> Option<SectionName> {
        match self {
            Surface::Section { section, .. } => Some(section),
            _ => None,
        }
    }
}

/// One config write.
pub(super) struct ConfigWrite<'a> {
    pub surface: Surface<'a>,
    pub mode: Mode,
    /// Request headers (If-Match, audit). `None` only for a dry run.
    pub headers: Option<&'a HeaderMap>,
    /// Extra `${env:NAME}` values the write may resolve (backup restore);
    /// also scrubbed from the response.
    pub extra_env: &'a EnvRefs,
}

/// What the handler's build step returns.
pub(super) struct Built {
    pub incoming: Config,
    /// Warnings of the build itself (PATCH field errors, the document's
    /// parse-time check warnings).
    pub warnings: Vec<String>,
}

/// Every warning the pipeline collects, by step, so each surface picks and
/// orders the ones its contract shows.
#[derive(Default, Debug)]
pub(super) struct Warnings {
    pub build: Vec<String>,
    pub preserve: Vec<String>,
    pub env: Vec<String>,
    /// The check warnings this change introduces.
    pub check_new: Vec<String>,
    /// Standing warnings: the running config's own check warnings plus the
    /// errors on UNCHANGED lifecycle content.
    pub existing: Vec<String>,
    /// Declarative-IAM preview (section dry run only).
    pub preview: Vec<String>,
    pub transition: Vec<String>,
}

/// The step a write stopped at.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Stage {
    Build,
    Bootstrap,
    EnvResolve,
    Preserve,
    EnvReapply,
    Normalize,
    Check,
    Gate,
    Transition,
}

#[derive(Debug)]
pub(super) struct Rejection {
    pub stage: Stage,
    pub status: StatusCode,
    pub error: String,
    /// Boxed: the warnings are large and a `Rejection` rides in `Result`s.
    pub warnings: Box<Warnings>,
    pub diff: Option<serde_json::Value>,
}

impl Rejection {
    pub(super) fn new(stage: Stage, status: StatusCode, error: impl Into<String>) -> Self {
        Self {
            stage,
            status,
            error: error.into(),
            warnings: Box::default(),
            diff: None,
        }
    }
}

pub(super) enum Outcome {
    /// `If-Match` named another version.
    Conflict {
        current: String,
    },
    Rejected(Rejection),
    /// Dry run passed.
    Validated {
        warnings: Warnings,
        requires_restart: bool,
        diff: Option<serde_json::Value>,
    },
    /// Applied in memory; `persist` is the file write.
    Applied {
        warnings: Warnings,
        requires_restart: bool,
        diff: Option<serde_json::Value>,
        persist: Result<String, (String, String)>,
        /// The version after the write (of the section, for a section PUT).
        version: String,
    },
}

/// A finished write: the outcome plus the env refs its response scrubs.
pub(super) struct WriteResult {
    pub outcome: Outcome,
    pub refs: EnvRefs,
}

/// The output of [`prepare`].
#[derive(Debug)]
pub(super) struct Prepared {
    pub new_cfg: Config,
    pub warnings: Warnings,
    pub requires_restart: bool,
    pub diff: Option<serde_json::Value>,
}

/// Run a config write. `build` makes `incoming` from the running config.
pub(super) async fn run(
    state: &Arc<AdminState>,
    write: ConfigWrite<'_>,
    build: impl FnOnce(&Config) -> Result<Built, Rejection>,
) -> WriteResult {
    let mut refs = write.extra_env.clone();
    if write.mode == Mode::DryRun {
        let old = state.config.read().await.clone();
        refs.extend(old.env_refs.clone());
        let outcome = match build(&old).and_then(|b| prepare(&old, b, &write)) {
            Err(r) => Outcome::Rejected(r),
            Ok(mut p) => {
                if write.surface.section().is_some() {
                    p.warnings.preview = declarative_preview(state, &old, &p.new_cfg).await;
                }
                Outcome::Validated {
                    warnings: p.warnings,
                    requires_restart: p.requires_restart,
                    diff: p.diff,
                }
            }
        };
        return WriteResult { outcome, refs };
    }

    // Apply: the write lock is held from the version check to the persist.
    let mut cfg = state.config.write().await;
    refs.extend(cfg.env_refs.clone());
    let section = write.surface.section();
    let current = super::version::config_version(&cfg, section);
    if write
        .headers
        .is_some_and(|h| super::version::if_match_conflicts(h, &current))
    {
        let outcome = Outcome::Conflict { current };
        return WriteResult { outcome, refs };
    }
    let prepared = match build(&cfg).and_then(|b| prepare(&cfg, b, &write)) {
        Ok(p) => p,
        Err(r) => {
            let outcome = Outcome::Rejected(r);
            return WriteResult { outcome, refs };
        }
    };
    let Prepared {
        new_cfg,
        mut warnings,
        diff,
        requires_restart: _,
    } = prepared;

    let no_headers = HeaderMap::new();
    let headers = write.headers.unwrap_or(&no_headers);
    let ctx = TransitionCtx::Admin { state, headers };
    let report = match super::apply_config_transition(ctx, &mut cfg, new_cfg).await {
        Ok(r) => r,
        Err(e) => {
            let outcome = Outcome::Rejected(Rejection {
                stage: Stage::Transition,
                status: StatusCode::UNPROCESSABLE_ENTITY,
                error: e,
                warnings: Box::new(warnings),
                diff,
            });
            return WriteResult { outcome, refs };
        }
    };
    refs.extend(cfg.env_refs.clone());
    warnings.transition = report.warnings;
    let version = super::version::config_version(&cfg, section);
    let path = super::active_config_path(state);
    let persist = match cfg.persist_to_file(&path) {
        Ok(()) => Ok(path.clone()),
        Err(e) => Err((path.clone(), e.to_string())),
    };
    match write.surface {
        Surface::Section { section, .. } if write.headers.is_some() => super::super::audit_log(
            &format!("apply_config_section:{}", section.as_str()),
            "admin",
            &path,
            headers,
        ),
        Surface::Document { .. } => {
            super::super::audit_log("apply_config", "admin", &path, headers)
        }
        _ => {}
    }
    let outcome = Outcome::Applied {
        warnings,
        requires_restart: report.requires_restart,
        diff,
        persist,
        version,
    };
    WriteResult { outcome, refs }
}

/// Steps 3a–3h: turn `incoming` into the config the transition gets, with
/// no side effect. Pure over `old` (reads only the `DGP_*` env).
pub(super) fn prepare(
    old: &Config,
    built: Built,
    write: &ConfigWrite<'_>,
) -> Result<Prepared, Rejection> {
    let steps = write.surface.steps(write.mode);
    let Built {
        mut incoming,
        warnings: build_warnings,
    } = built;
    let mut w = Warnings {
        build: build_warnings,
        ..Warnings::default()
    };
    macro_rules! reject {
        ($stage:expr, $status:expr, $err:expr) => {
            return Err(Rejection {
                stage: $stage,
                status: $status,
                error: $err.into(),
                warnings: Box::new(w),
                diff: None,
            })
        };
    }

    // Bootstrap hash. The document export redacts it, so an absent hash in
    // a document means "keep". The legitimate change path is
    // `PUT /api/admin/password`, which verifies the current password: an
    // arbitrary hash here would let an admin-session holder lock future
    // admins out of the GUI.
    if let Surface::Document { .. } = write.surface {
        if incoming.bootstrap_password_hash.is_none() {
            incoming.bootstrap_password_hash = old.bootstrap_password_hash.clone();
        }
    }
    if steps.bootstrap_guard && incoming.bootstrap_password_hash != old.bootstrap_password_hash {
        let via = match write.surface {
            Surface::Section { .. } => "/config/section",
            _ => "/config/apply",
        };
        reject!(
            Stage::Bootstrap,
            StatusCode::FORBIDDEN,
            format!(
                "bootstrap_password_hash cannot be changed via {via}; use PUT \
                 /api/admin/password (verifies the current password)"
            )
        );
    }

    // Env-ref provenance.
    match write.surface {
        // `into_flat` starts from a default: carry the refs through, then
        // resolve the full-scalar `${env:NAME}` strings of the body (section
        // GETs emit refs for ref-sourced secrets, so a GUI round-trip echoes
        // them back). Only recorded names resolve, never the server env (S7).
        Surface::Section { .. } => {
            incoming.env_refs = old.env_refs.clone();
            if let Err(e) = incoming.resolve_env_ref_scalars() {
                reject!(
                    Stage::EnvResolve,
                    StatusCode::BAD_REQUEST,
                    format!(
                        "env reference in section body did not resolve: {e}. Only names the \
                         boot config uses, or names listed in {}, resolve.",
                        crate::config::CONFIG_ENV_ALLOWLIST_VAR
                    )
                );
            }
        }
        // A document applied through the CLI arrives pre-expanded, so the
        // parse recorded no refs for names the BOOT file resolved: without
        // this merge those secrets would persist materialised. Newly
        // recorded names win over stale ones.
        Surface::Document { .. } if steps.merge_env_refs => {
            for (name, value) in &old.env_refs {
                incoming
                    .env_refs
                    .entry(name.clone())
                    .or_insert_with(|| value.clone());
            }
        }
        _ => {}
    }

    // Secret preservation: the GET/export surfaces redact every secret, so a
    // round-trip must not clear them.
    if steps.validate {
        let probe = match write.surface {
            Surface::Section { section, body } => {
                super::section_level::BackendEncryptionKeyProbe::from_section(section, body)
            }
            Surface::Document { yaml } => super::document_level::document_probe(yaml),
            Surface::Patch => unreachable!("PATCH does not preserve"),
        };
        match super::preserve_runtime_secrets(&mut incoming, old, &probe) {
            Ok(pw) => w.preserve = pw,
            Err(e) => reject!(Stage::Preserve, StatusCode::BAD_REQUEST, e),
        }
    }

    // Env wins consistently: re-apply the `DGP_*` overrides so an edit to
    // an env-controlled field reaches the file only.
    let document = matches!(write.surface, Surface::Document { .. });
    match super::reapply_env(old, &mut incoming, document) {
        Ok(ew) => w.env = ew,
        Err(e) => reject!(Stage::EnvReapply, StatusCode::INTERNAL_SERVER_ERROR, e),
    }

    // Y1: expand the shorthands (`public: true`, storage `s3:`) before the
    // bucket-derived snapshots walk the buckets.
    if steps.normalize {
        if let Err(e) = incoming.normalize_shorthands() {
            reject!(Stage::Normalize, StatusCode::BAD_REQUEST, e.to_string());
        }
    }

    if steps.validate {
        // The same fatal gate and warnings as boot. The running config's
        // own warnings are "existing"; only the rest are this change's
        // (issue #92). A running config that fails its own gate has no
        // baseline: all new.
        match incoming.check_all() {
            Ok(after) => {
                let before = old.clone().check_all().unwrap_or_default();
                let (new, existing) = super::split_new_warnings(&before, after);
                w.check_new = new;
                w.existing = existing;
            }
            Err(fatal) => reject!(
                Stage::Check,
                StatusCode::BAD_REQUEST,
                format!("config refused: {}", fatal.join("; "))
            ),
        }
        // Changed-only gates: an error on UNCHANGED lifecycle content is a
        // standing warning, so a pre-existing bad rule cannot block an
        // unrelated edit.
        match crate::lifecycle::planner::lifecycle_gate(&old.lifecycle, &incoming.lifecycle) {
            Ok(standing) => {
                if !standing.is_empty() {
                    tracing::warn!(
                        "config write: pre-existing invalid lifecycle config left unchanged: {}",
                        standing.join("; ")
                    );
                }
                w.existing.extend(standing);
            }
            Err(errs) => reject!(Stage::Gate, StatusCode::BAD_REQUEST, errs.join("; ")),
        }
        // Duplicate replication rule names (#13): state, cursor and lease
        // are keyed by name.
        if let Err(errs) =
            crate::config_sections::replication_gate(&old.replication, &incoming.replication)
        {
            reject!(Stage::Gate, StatusCode::BAD_REQUEST, errs.join("; "));
        }
    }

    let diff = write
        .surface
        .section()
        .map(|s| super::section_level::compute_section_diff(s, old, &incoming));
    let requires_restart = !super::requires_restart_warnings(old, &incoming).is_empty();
    Ok(Prepared {
        new_cfg: incoming,
        warnings: w,
        requires_restart,
        diff,
    })
}

/// The section dry run's declarative-IAM preview: the would-be reconcile
/// (validation only, zero DB writes), the same `diff_iam` the live apply
/// runs, so the preview cannot lie.
async fn declarative_preview(state: &Arc<AdminState>, old: &Config, new: &Config) -> Vec<String> {
    if !super::transition::declarative_reconcile_needed(old, new) {
        return Vec::new();
    }
    let yaml_snapshot = crate::iam::snapshot_from_access(
        &new.iam_users,
        &new.iam_groups,
        &new.auth_providers,
        &new.group_mapping_rules,
        &[],
    );
    if matches!(old.iam_mode, crate::config_sections::IamMode::Gui) && yaml_snapshot.is_empty() {
        return vec![
            "declarative IAM preview: flip to declarative mode with empty iam_users / \
             iam_groups would be REFUSED by the live apply (would wipe the DB). \
             Add IAM content to the YAML first."
                .to_string(),
        ];
    }
    let Some(db_arc) = state.config_db.as_ref() else {
        return Vec::new();
    };
    let db = db_arc.lock().await;
    vec![
        match crate::iam::preview_declarative_iam(&db, &yaml_snapshot) {
            Ok(d) if d.is_empty() => {
                "declarative IAM preview: no IAM changes (idempotent apply)".to_string()
            }
            Ok(d) => format!("declarative IAM preview: {}", d.summary_line()),
            Err(e) => format!(
                "declarative IAM preview REJECTED at validation (live apply would return this \
             error verbatim): {e}"
            ),
        },
    ]
}

// ── Response side ─────────────────────────────────────────────────────────

/// S7 (review 4 config-2): a response body scrubs the resolved values of
/// the recorded env refs out of every string it carries.
pub(super) trait ScrubEnv {
    fn scrub_env(&mut self, refs: &EnvRefs);
}

pub(super) fn scrub_strings<'a>(items: impl IntoIterator<Item = &'a mut String>, refs: &EnvRefs) {
    for s in items {
        *s = crate::config::scrub_env_values(s, refs);
    }
}

/// THE way a config write response leaves: scrub, then serialise.
pub(super) fn respond<T: serde::Serialize + ScrubEnv>(
    status: StatusCode,
    mut body: T,
    refs: &EnvRefs,
    etag: Option<HeaderValue>,
) -> Response {
    body.scrub_env(refs);
    let mut resp = (status, Json(body)).into_response();
    if let Some(tag) = etag {
        resp.headers_mut().insert(axum::http::header::ETAG, tag);
    }
    resp
}

impl ScrubEnv for serde_json::Value {
    fn scrub_env(&mut self, refs: &EnvRefs) {
        super::scrub_env_json(self, refs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(surface: Surface<'_>, mode: Mode) -> ConfigWrite<'_> {
        static EMPTY: std::sync::OnceLock<EnvRefs> = std::sync::OnceLock::new();
        ConfigWrite {
            surface,
            mode,
            headers: None,
            extra_env: EMPTY.get_or_init(Default::default),
        }
    }

    fn built(incoming: Config) -> Built {
        Built {
            incoming,
            warnings: Vec::new(),
        }
    }

    fn running() -> Config {
        Config {
            bootstrap_password_hash: Some("$2b$12$running".into()),
            ..Config::default()
        }
    }

    #[test]
    fn document_apply_keeps_an_absent_hash_and_refuses_a_changed_one() {
        let old = running();
        let yaml = "";
        let w = write(Surface::Document { yaml }, Mode::Apply);
        let absent = Config::default();
        let p = prepare(&old, built(absent), &w).unwrap();
        assert_eq!(
            p.new_cfg.bootstrap_password_hash,
            old.bootstrap_password_hash
        );

        let changed = Config {
            bootstrap_password_hash: Some("$2b$12$other".into()),
            ..Config::default()
        };
        let r = prepare(&old, built(changed.clone()), &w).unwrap_err();
        assert_eq!(
            (r.stage, r.status),
            (Stage::Bootstrap, StatusCode::FORBIDDEN)
        );
        assert!(r.error.contains("/config/apply"), "{}", r.error);
        // The document validate does not guard the hash.
        let dv = write(Surface::Document { yaml }, Mode::DryRun);
        assert!(prepare(&old, built(changed), &dv).is_ok());
    }

    #[test]
    fn section_refuses_a_hash_change_and_names_its_route() {
        let old = running();
        let body = serde_json::json!({});
        let section = SectionName::Advanced;
        let w = write(
            Surface::Section {
                section,
                body: &body,
            },
            Mode::DryRun,
        );
        let r = prepare(&old, built(Config::default()), &w).unwrap_err();
        assert_eq!(r.stage, Stage::Bootstrap);
        assert!(r.error.contains("/config/section"), "{}", r.error);
    }

    #[test]
    fn patch_skips_validation_and_section_runs_the_gates() {
        let old = running();
        // A fatal lifecycle rule (delete without expire_after).
        let mut bad = old.clone();
        bad.lifecycle = serde_yaml::from_str(
            "enabled: true\nrules:\n  - name: r\n    bucket: b\n    prefix: ''\n",
        )
        .unwrap();
        let p = prepare(
            &old,
            built(bad.clone()),
            &write(Surface::Patch, Mode::Apply),
        );
        assert!(p.is_ok(), "PATCH validates only its own fields");
        let body = serde_json::json!({});
        let section = SectionName::Storage;
        let w = write(
            Surface::Section {
                section,
                body: &body,
            },
            Mode::Apply,
        );
        let r = prepare(&old, built(bad), &w).unwrap_err();
        assert_eq!((r.stage, r.status), (Stage::Gate, StatusCode::BAD_REQUEST));
    }

    #[test]
    fn section_diff_and_restart_flag_come_from_prepare() {
        let old = running();
        let mut new = old.clone();
        new.cache_size_mb += 1;
        let body = serde_json::json!({});
        let section = SectionName::Advanced;
        let w = write(
            Surface::Section {
                section,
                body: &body,
            },
            Mode::DryRun,
        );
        let p = prepare(&old, built(new), &w).unwrap();
        assert!(p.requires_restart);
        let diff = p.diff.unwrap();
        assert!(diff["advanced"]["cache_size_mb"].is_object(), "{diff}");
    }

    #[tokio::test]
    async fn respond_scrubs_every_string_of_the_body() {
        let refs: EnvRefs = [("S".to_string(), "sekret-value-1234".to_string())].into();
        let body = serde_json::json!({
            "error": "bad sekret-value-1234",
            "warnings": ["w sekret-value-1234"],
            "diff": { "a": { "after": "sekret-value-1234" } },
        });
        let resp = respond(StatusCode::OK, body, &refs, None);
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(!text.contains("sekret-value-1234"), "{text}");
        assert!(text.contains("${env:S}"), "{text}");
    }
}
