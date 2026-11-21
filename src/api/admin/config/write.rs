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
//! 6. after the write lock is released: the re-encrypt jobs of the
//!    conversions that the transition opened
//!    ([`super::conversion_start::enqueue`]).
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

use tokio::sync::RwLockWriteGuard;

use super::super::AdminState;
use super::conversion_start::{ConversionPreview, ConversionStarted};
use super::{SectionName, TransitionCtx};
use crate::config::Config;

/// The env values a response is scrubbed of (`name -> value`): not the
/// provenance (`crate::config::EnvRefs`), only what must never echo.
pub(super) type ScrubMap = BTreeMap<String, String>;

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
    /// An internal writer ([`run_internal`]): the running config with an
    /// edit applied, audited as `action` on `target`.
    Internal {
        action: &'static str,
        target: &'a str,
    },
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
}

impl Surface<'_> {
    /// The steps take no [`Mode`] on purpose: a dry run runs exactly the
    /// checks of its apply, so a validate answers what the apply answers.
    fn steps(self) -> Steps {
        match self {
            Surface::Patch => Steps {
                bootstrap_guard: false,
                validate: false,
                normalize: false,
            },
            Surface::Section { .. } => Steps {
                bootstrap_guard: true,
                validate: true,
                normalize: true,
            },
            Surface::Document { .. } => Steps {
                bootstrap_guard: true,
                validate: true,
                normalize: false,
            },
            // Starts from the full running config (nothing redacted, env
            // refs carried), so only the gates of `validate` run.
            Surface::Internal { .. } => Steps {
                bootstrap_guard: false,
                validate: true,
                normalize: false,
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
    pub extra_env: &'a ScrubMap,
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
    /// The structured part of a refusal (the `atomic_writes` object of a
    /// change refused for lack of atomic writes, N2); every surface puts
    /// it in its body as `atomic_writes`.
    pub details: Option<serde_json::Value>,
}

impl Rejection {
    pub(super) fn new(stage: Stage, status: StatusCode, error: impl Into<String>) -> Self {
        Self {
            stage,
            status,
            error: error.into(),
            warnings: Box::default(),
            diff: None,
            details: None,
        }
    }

    /// Add the structured part of the refusal to a JSON `body` object.
    pub(super) fn attach_details(
        details: Option<serde_json::Value>,
        mut body: serde_json::Value,
    ) -> serde_json::Value {
        if let (Some(d), Some(obj)) = (details, body.as_object_mut()) {
            obj.insert("atomic_writes".to_string(), d);
        }
        body
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
        /// One line per changed restart-required field; empty = none.
        restart: Vec<String>,
        diff: Option<serde_json::Value>,
        /// The buckets whose conversion the apply would start.
        conversions: Vec<ConversionPreview>,
    },
    /// Applied in memory; `persist` is the file write.
    Applied {
        warnings: Warnings,
        /// One line per changed restart-required field; empty = none.
        restart: Vec<String>,
        diff: Option<serde_json::Value>,
        persist: Result<String, (String, String)>,
        /// The version after the write (of the section, for a section PUT).
        version: String,
        /// The conversions the write started, with their jobs.
        conversions: Vec<ConversionStarted>,
    },
}

/// A finished write: the outcome plus the env refs its response scrubs.
pub(super) struct WriteResult {
    pub outcome: Outcome,
    pub refs: ScrubMap,
}

/// The output of [`prepare`].
#[derive(Debug)]
pub(super) struct Prepared {
    pub new_cfg: Config,
    pub warnings: Warnings,
    pub restart: Vec<String>,
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
        let outcome = match build(&old).and_then(|b| prepare_with_refs(&old, b, &write, &mut refs))
        {
            Err(r) => Outcome::Rejected(r),
            Ok(mut p) => match super::transition::static_transition_gates(
                &old,
                &p.new_cfg,
                &super::transition::GateInputs::gather(
                    &state.s3_state,
                    &old,
                    &p.new_cfg,
                    write.headers,
                    true,
                    false,
                )
                .await,
            ) {
                // What the apply would refuse, a validate refuses too.
                Err(error) => Outcome::Rejected(Rejection {
                    stage: Stage::Transition,
                    status: StatusCode::UNPROCESSABLE_ENTITY,
                    error,
                    warnings: Box::new(p.warnings),
                    diff: p.diff,
                    details: None,
                }),
                Ok(()) => match dry_run_gates(state, &old, &p.new_cfg).await {
                    Err(e) => Outcome::Rejected(Rejection {
                        stage: Stage::Transition,
                        status: e.status,
                        error: e.message,
                        warnings: Box::new(p.warnings),
                        diff: p.diff,
                        details: e.details,
                    }),
                    Ok(conversions) => {
                        if write.surface.section().is_some() {
                            p.warnings.preview = declarative_preview(state, &old, &p.new_cfg).await;
                        }
                        p.warnings.check_new.extend(conversions.iter().filter_map(
                            super::conversion_start::ConversionPreview::versioning_warning,
                        ));
                        Outcome::Validated {
                            warnings: p.warnings,
                            restart: p.restart,
                            diff: p.diff,
                            conversions,
                        }
                    }
                },
            },
        };
        return WriteResult { outcome, refs };
    }

    // Apply: the write lock is held from the version check to the persist.
    let mut cfg = state.config.write().await;
    let mut discoveries = Vec::new();
    let outcome = apply_locked(state, &mut cfg, &write, &mut refs, &mut discoveries, build).await;
    if let Outcome::Applied {
        persist: Ok(path) | Err((path, _)),
        ..
    } = &outcome
    {
        audit_write(&write, path);
    }
    drop(cfg);
    run_discoveries(discoveries).await;
    let mut outcome = outcome;
    if let Outcome::Applied {
        conversions,
        warnings,
        ..
    } = &mut outcome
    {
        reconcile_job_rules(state).await;
        let no_headers = HeaderMap::new();
        let config = state.config.read().await.clone();
        super::conversion_start::enqueue(
            &state.s3_state,
            &config,
            conversions,
            &mut warnings.transition,
            write.headers.unwrap_or(&no_headers),
        )
        .await;
    }
    WriteResult { outcome, refs }
}

/// The dry run's gates after the static ones: the atomic-writes gate of
/// the apply (N2, a fresh probe: a validate answers what the apply would),
/// then the conversion step: what the apply would start (with the RUNNING
/// engine's listing and an object count for the dialog), and the same
/// active-job refusal.
async fn dry_run_gates(
    state: &Arc<AdminState>,
    old: &Config,
    new: &Config,
) -> Result<Vec<ConversionPreview>, super::transition::TransitionError> {
    crate::coordination::atomicity::gate_encryption_change(
        old,
        new,
        &state.s3_state.backend_capabilities,
    )
    .await?;
    let engine = state.s3_state.engine.load_full();
    let planned =
        super::conversion_start::plan(&engine, old, new, super::conversion_start::COUNT_CAP)
            .await?;
    let buckets: Vec<&str> = planned.iter().map(|p| p.bucket.as_str()).collect();
    super::conversion_start::refuse_active_jobs(&state.s3_state, &buckets, false).await?;
    Ok(planned)
}

/// Drop the runtime rows of the job rules that the applied config no
/// longer names (a rule removed or renamed by this write). It runs after
/// the config write lock is released and reads a copy of the config: a
/// task that holds the jobs DB can wait for the config.
async fn reconcile_job_rules(state: &Arc<AdminState>) {
    let Some(db) = state.jobs_db.as_ref() else {
        return;
    };
    let cfg = state.config.read().await.clone();
    crate::config_apply::reconcile_job_rules(&*db.lock().await, &cfg);
}

/// The audit entry of an applied write.
fn audit_write(write: &ConfigWrite<'_>, path: &str) {
    let no_headers = HeaderMap::new();
    let headers = write.headers.unwrap_or(&no_headers);
    match write.surface {
        Surface::Section { section, .. } if write.headers.is_some() => super::super::audit_log(
            &format!("apply_config_section:{}", section.as_str()),
            "admin",
            path,
            headers,
        ),
        Surface::Document { .. } => super::super::audit_log("apply_config", "admin", path, headers),
        Surface::Internal { action, target } => {
            super::super::audit_log(action, "admin", target, headers)
        }
        _ => {}
    }
}

/// The OIDC discoveries of the provider sets a write published (review A8).
/// Network I/O: the caller runs them after it drops the config write guard,
/// and before it answers, so the ext-auth version is bumped first.
async fn run_discoveries(discoveries: Vec<super::super::external_auth::ProviderDiscovery>) {
    for discovery in discoveries {
        discovery.run().await;
    }
}

/// Steps 1–5 of an apply, under the caller's config write guard: version
/// check, build, prepare, transition, persist. The caller audits, and runs
/// the transition's `discoveries` after it drops the guard.
async fn apply_locked(
    state: &Arc<AdminState>,
    cfg: &mut RwLockWriteGuard<'_, Config>,
    write: &ConfigWrite<'_>,
    refs: &mut ScrubMap,
    discoveries: &mut Vec<super::super::external_auth::ProviderDiscovery>,
    build: impl FnOnce(&Config) -> Result<Built, Rejection>,
) -> Outcome {
    refs.extend(cfg.env_refs.clone());
    let section = write.surface.section();
    let current = super::version::config_version(cfg, section);
    // An internal write carries the headers of a request to another
    // endpoint: its `If-Match` names no config version.
    let internal = matches!(write.surface, Surface::Internal { .. });
    if !internal
        && write
            .headers
            .is_some_and(|h| super::version::if_match_conflicts(h, &current))
    {
        return Outcome::Conflict { current };
    }
    let prepared = match build_over_masked_frozen(cfg, write.surface, build)
        .and_then(|b| prepare_with_refs(cfg, b, write, refs))
    {
        Ok(p) => p,
        Err(r) => return Outcome::Rejected(r),
    };
    let Prepared {
        new_cfg,
        mut warnings,
        diff,
        restart,
    } = prepared;

    let no_headers = HeaderMap::new();
    let headers = write.headers.unwrap_or(&no_headers);
    let ctx = TransitionCtx::Admin {
        state,
        headers,
        restoring: false,
    };
    let report = match super::apply_config_transition(ctx, cfg, new_cfg).await {
        Ok(r) => r,
        Err(e) => {
            return Outcome::Rejected(Rejection {
                stage: Stage::Transition,
                status: e.status,
                error: e.message,
                warnings: Box::new(warnings),
                diff,
                details: e.details,
            });
        }
    };
    refs.extend(cfg.env_refs.clone());
    discoveries.extend(report.discovery);
    warnings.transition = report.warnings;
    let version = super::version::config_version(cfg, section);
    Outcome::Applied {
        warnings,
        // The transition computes the same list from the same two configs.
        restart,
        diff,
        persist: persist(state, cfg),
        version,
        conversions: report.conversions,
    }
}

/// `Err` when `new` differs from `old` over the frozen set (see
/// `env_template::frozen_violation`). `server_writer`: the migrate job's own writer.
pub(crate) fn frozen_check(old: &Config, new: &Config, server_writer: bool) -> Result<(), String> {
    if old.env_refs.frozen.is_empty() {
        return Ok(());
    }
    let tree = |c: &Config| serde_yaml::to_value(c).map_err(|e| format!("internal: {e}"));
    match crate::config::env_template::frozen_violation(
        &old.env_refs,
        &tree(old)?,
        &tree(new)?,
        server_writer,
    ) {
        Some(message) => Err(message),
        None => Ok(()),
    }
}

/// Run `build` on a view with a random mask in every string that a reference
/// supplies (see `config::env_mask`): a field written there is refused whatever
/// it holds, and a mask copied to a path with no declared copy refuses the write.
fn build_over_masked_frozen(
    old: &Config,
    surface: Surface<'_>,
    build: impl FnOnce(&Config) -> Result<Built, Rejection>,
) -> Result<Built, Rejection> {
    use crate::config::env_mask::MaskedView;
    let by_field = matches!(surface, Surface::Patch | Surface::Internal { .. });
    if !by_field {
        return build(old);
    }
    let refuse = |m: String| Rejection::new(Stage::EnvResolve, StatusCode::BAD_REQUEST, m);
    let view = MaskedView::of(old, false).map_err(|e| refuse(e.message()))?;
    let mut built = build(&view.config.clone())?;
    built.incoming = view
        .finish(built.incoming)
        .map_err(|e| refuse(e.message()))?;
    Ok(built)
}

/// THE config file write of a running-config change.
fn persist(state: &Arc<AdminState>, cfg: &Config) -> Result<String, (String, String)> {
    let path = super::active_config_path(state);
    match cfg.persist_to_file(&path) {
        Ok(()) => Ok(path),
        Err(e) => Err((path, e.to_string())),
    }
}

/// Steps 3a–3h: turn `incoming` into the config the transition gets, with
/// no side effect. Pure over `old` (reads only the `DGP_*` env).
#[cfg(test)]
pub(super) fn prepare(
    old: &Config,
    built: Built,
    write: &ConfigWrite<'_>,
) -> Result<Prepared, Rejection> {
    prepare_with_refs(old, built, write, &mut ScrubMap::new())
}

/// [`prepare`] that adds every env value the section body resolved to
/// `refs`, the map the response is scrubbed with: a name resolved through
/// DGP_CONFIG_ENV_ALLOWLIST is in no old provenance, and its value must
/// not echo in a validate result or a rejection either.
pub(super) fn prepare_with_refs(
    old: &Config,
    built: Built,
    write: &ConfigWrite<'_>,
    refs: &mut ScrubMap,
) -> Result<Prepared, Rejection> {
    let steps = write.surface.steps();
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
                details: None,
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

    // Env-ref provenance. A document and a section body were judged and
    // expanded by `env_template` before this step (S7), and their configs
    // carry the running provenance plus what the allowlist added; PATCH and
    // the internal writers carry none and take it from the running config.
    let mut provenance = old.env_refs.clone();
    provenance.merge(std::mem::take(&mut incoming.env_refs));
    incoming.env_refs = provenance;
    // Every surface: the values this body resolved (a document's parse, a
    // section's resolve, a name admitted by DGP_CONFIG_ENV_ALLOWLIST) are
    // scrubbed from every later answer, a rejection included (review B6:
    // only the section surface added them).
    refs.extend(
        incoming
            .env_refs
            .iter()
            .map(|(k, v)| (k.clone(), v.clone())),
    );

    // Secret preservation: the GET/export surfaces redact every secret, so a
    // round-trip must not clear them.
    if steps.validate {
        let probe = match write.surface {
            Surface::Section { section, body } => {
                Some(super::section_level::BackendEncryptionKeyProbe::from_section(section, body))
            }
            Surface::Document { yaml } => Some(super::document_level::document_probe(yaml)),
            // Nothing redacted: the running config carries its secrets.
            Surface::Internal { .. } => None,
            Surface::Patch => unreachable!("PATCH does not preserve"),
        };
        if let Some(probe) = probe {
            match super::preserve_runtime_secrets(&mut incoming, old, &probe) {
                Ok(pw) => w.preserve = pw,
                Err(e) => reject!(Stage::Preserve, StatusCode::BAD_REQUEST, e),
            }
        }
    }

    // Env wins consistently: re-apply the `DGP_*` overrides so an edit to
    // an env-controlled field reaches the file only.
    let document = matches!(write.surface, Surface::Document { .. });
    match super::reapply_env(old, &mut incoming, document) {
        Ok(ew) => w.env = ew,
        Err(e) => reject!(Stage::EnvReapply, StatusCode::INTERNAL_SERVER_ERROR, e),
    }

    // The frozen set (S7): what the config file gave to the environment
    // stays as it is, whatever the request.
    if let Err(e) = frozen_check(old, &incoming, false) {
        reject!(Stage::EnvResolve, StatusCode::BAD_REQUEST, e);
    }
    // A `__dgmigrate_*` route is the migrate job's: no request makes, edits or ends one.
    if let Some(e) = crate::maintenance::migrate::reserved_route_change(old, &incoming) {
        reject!(Stage::Check, StatusCode::BAD_REQUEST, e);
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
        // The changed-only rule gates (lifecycle, replication, declarative
        // IAM): the same step `config lint` runs.
        match incoming.rule_gates(old) {
            Ok(standing) => {
                if !standing.is_empty() {
                    tracing::warn!(
                        "config write: pre-existing invalid rules left unchanged: {}",
                        standing.join("; ")
                    );
                }
                w.existing.extend(standing);
            }
            Err(refusal) => reject!(
                Stage::Gate,
                StatusCode::BAD_REQUEST,
                refusal.errors().join("; ")
            ),
        }
    }

    // What persist would refuse is refused now, before the change is live (an apply and
    // a validate alike): after the transition only a failed file write is left.
    if let Err(e) = incoming.check_persistable() {
        reject!(
            Stage::Check,
            StatusCode::BAD_REQUEST,
            format!("config refused: {e}")
        );
    }

    let diff = write
        .surface
        .section()
        .map(|s| super::section_level::compute_section_diff(s, old, &incoming));
    let restart = super::requires_restart_warnings(old, &incoming);
    Ok(Prepared {
        new_cfg: incoming,
        warnings: w,
        restart,
        diff,
    })
}

/// The result of [`run_internal`].
pub(crate) struct InternalApplied {
    /// Env re-apply, new check warnings, then the transition's.
    pub warnings: Vec<String>,
    /// `Ok(path)`, or `Err((path, error))` when the file write failed (the
    /// config is live in memory either way).
    pub persist: Result<String, (String, String)>,
    /// The conversions the write started, with their jobs.
    pub conversions: Vec<ConversionStarted>,
}

/// Why [`run_internal`] changed nothing.
#[derive(Debug)]
pub(crate) enum InternalRefusal {
    /// The `DGP_*` overrides could not be re-applied.
    EnvReapply(String),
    /// `check_all` or a rule gate refused the edit.
    Invalid { status: StatusCode, error: String },
    /// `apply_config_transition` refused or failed (nothing live changed).
    Transition(String),
}

impl std::fmt::Display for InternalRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EnvReapply(e) => write!(f, "env overrides not re-applied: {e}"),
            Self::Invalid { error, .. } => write!(f, "config refused: {error}"),
            Self::Transition(e) => write!(f, "config transition failed: {e}"),
        }
    }
}

/// THE path for a config write that no admin config endpoint makes
/// (bootstrap-credential removal, the backup secrets restore and its
/// rollback): `edit` changes a copy of the running config, then the same
/// env re-apply, `check_all`, rule gates, transition, persist and audit as
/// every config write run. Errors and warnings are env-scrubbed.
pub(crate) async fn run_internal(
    state: &Arc<AdminState>,
    headers: &HeaderMap,
    action: &'static str,
    target: &str,
    edit: impl FnOnce(&mut Config),
) -> Result<InternalApplied, InternalRefusal> {
    let write = Internal {
        headers,
        action,
        target,
        on_persist_error: OnPersistError::Report,
    };
    let held = run_internal_held(
        state,
        write,
        std::future::ready(()),
        |cfg, _| {
            edit(cfg);
            Ok::<(), std::convert::Infallible>(())
        },
        |_| std::future::ready(Ok(())),
    )
    .await;
    held.map_err(|e| match e {
        HeldRefusal::Pipeline(r) => r,
        HeldRefusal::Persist { .. } => unreachable!("OnPersistError::Report keeps the write"),
        HeldRefusal::Edit(never) => match never {},
    })
}

/// Who asks for an internal write, for its audit entry.
pub(crate) struct Internal<'a> {
    pub headers: &'a HeaderMap,
    pub action: &'static str,
    pub target: &'a str,
    pub on_persist_error: OnPersistError,
}

/// What an internal write does when the config file write fails.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum OnPersistError {
    /// Keep the change live and report the error in
    /// [`InternalApplied::persist`] (the config endpoints do the same).
    Report,
    /// Put the old config back, so memory and file never differ (a rule
    /// that only memory lost would come back at the next restart).
    RollBack,
}

/// Why [`run_internal_held`] changed nothing.
#[derive(Debug)]
pub(crate) enum HeldRefusal<E> {
    /// `edit` or `commit` refused; for `commit`, the old config is back.
    Edit(E),
    Pipeline(InternalRefusal),
    /// The file write failed and [`OnPersistError::RollBack`] put the old
    /// config back.
    Persist {
        path: String,
        error: String,
    },
}

/// [`run_internal`] for a writer with more to do under the config write
/// lock. `hold` runs after the lock is taken (the lock order: config
/// OUTER, so `hold` may take the config-DB lock); `edit` may refuse, and
/// nothing changes; `commit` runs after the persist, and a refusal puts the
/// old config back (live and in the file). The audit entry is written only
/// when everything succeeded.
// A `hold` that keeps the config DB lock (the rule delete) must wrap an
// edit that leaves the IAM fields, the bootstrap pair and `authentication`
// unchanged: `commit_iam` takes that lock for an IAM change, and a tokio
// Mutex is not reentrant (the step-2 deadlock of the bugscan review).
pub(crate) async fn run_internal_held<H, E, C>(
    state: &Arc<AdminState>,
    write: Internal<'_>,
    hold: impl std::future::Future<Output = H>,
    edit: impl FnOnce(&mut Config, &mut H) -> Result<(), E>,
    commit: impl FnOnce(H) -> C,
) -> Result<InternalApplied, HeldRefusal<E>>
where
    C: std::future::Future<Output = Result<(), E>>,
{
    let config_write = ConfigWrite {
        surface: Surface::Internal {
            action: write.action,
            target: write.target,
        },
        mode: Mode::Apply,
        headers: Some(write.headers),
        extra_env: &ScrubMap::new(),
    };
    let mut refs = ScrubMap::new();
    let mut discoveries = Vec::new();
    let mut cfg = state.config.write().await;
    let result = 'locked: {
        let mut held = hold.await;
        let old = cfg.clone();
        let mut refused = None;
        let outcome = apply_locked(
            state,
            &mut cfg,
            &config_write,
            &mut refs,
            &mut discoveries,
            |running| {
                let mut incoming = running.clone();
                match edit(&mut incoming, &mut held) {
                    Ok(()) => Ok(Built {
                        incoming,
                        warnings: Vec::new(),
                    }),
                    Err(e) => {
                        refused = Some(e);
                        Err(Rejection::new(Stage::Build, StatusCode::BAD_REQUEST, ""))
                    }
                }
            },
        )
        .await;
        if let Some(e) = refused {
            break 'locked Err(HeldRefusal::Edit(e));
        }
        let scrub = |s: String| crate::config::scrub_env_values(&s, &refs);
        let (warnings, persist, conversions) = match outcome {
            Outcome::Applied {
                warnings,
                persist,
                conversions,
                ..
            } => (warnings, persist, conversions),
            Outcome::Rejected(r) => {
                break 'locked Err(HeldRefusal::Pipeline(match r.stage {
                    Stage::EnvReapply => InternalRefusal::EnvReapply(scrub(r.error)),
                    // A 409 (an active job) keeps its status.
                    Stage::Transition if r.status == StatusCode::CONFLICT => {
                        InternalRefusal::Invalid {
                            status: r.status,
                            error: scrub(r.error),
                        }
                    }
                    Stage::Transition => InternalRefusal::Transition(scrub(r.error)),
                    _ => InternalRefusal::Invalid {
                        status: r.status,
                        error: scrub(r.error),
                    },
                }));
            }
            Outcome::Conflict { .. } | Outcome::Validated { .. } => {
                unreachable!("an internal write is an apply without If-Match")
            }
        };
        // The engine that opened this write's conversions: a roll back
        // removes the markers it wrote.
        let opened_by = state.s3_state.engine.load_full();
        if let (Err((path, error)), OnPersistError::RollBack) = (&persist, write.on_persist_error) {
            roll_back(state, &mut cfg, write.headers, old, false, &mut discoveries).await;
            super::conversion_start::compensate(&opened_by, &conversions, write.headers).await;
            break 'locked Err(HeldRefusal::Persist {
                path: path.clone(),
                error: scrub(error.clone()),
            });
        }
        if let Err(e) = commit(held).await {
            roll_back(
                state,
                &mut cfg,
                write.headers,
                old,
                persist.is_ok(),
                &mut discoveries,
            )
            .await;
            super::conversion_start::compensate(&opened_by, &conversions, write.headers).await;
            break 'locked Err(HeldRefusal::Edit(e));
        }
        let (Ok(path) | Err((path, _))) = &persist;
        audit_write(&config_write, path);
        let mut all = warnings.env;
        all.extend(warnings.check_new);
        all.extend(warnings.transition);
        Ok(InternalApplied {
            warnings: all.into_iter().map(scrub).collect(),
            persist: persist.map_err(|(p, e)| (p, scrub(e))),
            conversions,
        })
    };
    // OIDC discovery is network I/O: after the guard (review A8).
    drop(cfg);
    run_discoveries(discoveries).await;
    let mut result = result;
    if let Ok(applied) = &mut result {
        let config = state.config.read().await.clone();
        super::conversion_start::enqueue(
            &state.s3_state,
            &config,
            &mut applied.conversions,
            &mut applied.warnings,
            write.headers,
        )
        .await;
    }
    result
}

/// Put back a config that was live before a failed restore: a transition with the
/// forward-only gates off, then the file write. It holds real values, so no mask view.
pub(crate) async fn put_back_config(
    state: &Arc<AdminState>,
    headers: &HeaderMap,
    snapshot: Config,
) -> Result<(), String> {
    let mut discoveries = Vec::new();
    let mut cfg = state.config.write().await;
    let ctx = TransitionCtx::Admin {
        state,
        headers,
        restoring: true,
    };
    let result = match super::apply_config_transition(ctx, &mut cfg, snapshot).await {
        Ok(report) => {
            discoveries.extend(report.discovery);
            persist(state, &cfg)
                .map(|_| super::super::audit_log("restore_rollback", "admin", "config", headers))
                .map_err(|(path, e)| {
                    let e = crate::config::scrub_env_values(&e, &cfg.env_refs);
                    format!("config file {path} not restored: {e}")
                })
        }
        Err(e) => Err(format!(
            "config not restored: {}",
            crate::config::scrub_env_values(&e.message, &cfg.env_refs)
        )),
    };
    drop(cfg);
    run_discoveries(discoveries).await;
    result
}

/// Put `old` back after a failed internal write, and into the file when
/// the write reached it.
async fn roll_back(
    state: &Arc<AdminState>,
    cfg: &mut RwLockWriteGuard<'_, Config>,
    headers: &HeaderMap,
    old: Config,
    re_persist: bool,
    discoveries: &mut Vec<super::super::external_auth::ProviderDiscovery>,
) {
    let ctx = TransitionCtx::Admin {
        state,
        headers,
        restoring: true,
    };
    match super::apply_config_transition(ctx, cfg, old).await {
        Ok(report) => discoveries.extend(report.discovery),
        Err(e) => {
            tracing::error!("config write rollback failed, the new config stays live: {e}");
            return;
        }
    }
    if re_persist {
        if let Err((path, e)) = persist(state, cfg) {
            tracing::error!("config write rollback: persist to {path} failed: {e}");
        }
    }
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
    if matches!(old.iam_mode, crate::config_sections::IamMode::Gui)
        && yaml_snapshot.declares_no_users_or_groups()
    {
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
    fn scrub_env(&mut self, refs: &ScrubMap);
}

pub(super) fn scrub_strings<'a>(items: impl IntoIterator<Item = &'a mut String>, refs: &ScrubMap) {
    for s in items {
        *s = crate::config::scrub_env_values(s, refs);
    }
}

/// THE way a config write response leaves: scrub, then serialise.
pub(super) fn respond<T: serde::Serialize + ScrubEnv>(
    status: StatusCode,
    mut body: T,
    refs: &ScrubMap,
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
    fn scrub_env(&mut self, refs: &ScrubMap) {
        super::scrub_env_json(self, refs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// N8: every running-config write persists through this pipeline, so a
    /// new writer cannot skip its env re-apply, gates, transition and audit
    /// by calling `persist_to_file` itself. The other callers write no
    /// running config: the background `ConfigMutator` (its own mutate →
    /// rebuild → persist pipeline) and the `--init` wizard (a new file).
    #[test]
    fn config_persists_only_through_the_write_pipeline() {
        const ALLOWED: [&str; 3] = [
            "src/api/admin/config/write.rs",
            "src/config_apply.rs",
            "src/init.rs",
        ];
        // A call under any spelling: `cfg.persist_to_file(p)`, the UFCS
        // `Config::persist_to_file(&cfg, p)`, a split chain. The old scan
        // looked for the text `.persist_to_file(` in the file.
        let offenders: Vec<String> = crate::source_scan::ast::src()
            .iter()
            .filter(|s| !ALLOWED.contains(&s.rel.as_str()))
            .filter(|s| s.prod.calls_named("persist_to_file").next().is_some())
            .map(|s| s.rel.clone())
            .collect();
        assert!(
            offenders.is_empty(),
            "config persisted outside the write pipeline (use run_internal): {offenders:?}"
        );
    }

    fn write(surface: Surface<'_>, mode: Mode) -> ConfigWrite<'_> {
        static EMPTY: std::sync::OnceLock<ScrubMap> = std::sync::OnceLock::new();
        ConfigWrite {
            surface,
            mode,
            headers: None,
            extra_env: EMPTY.get_or_init(Default::default),
        }
    }

    /// Review B6 guard: whatever the surface, the env values the incoming
    /// config resolved are in the scrub map after `prepare`, on success and
    /// on a rejection. The match has no `_` arm: a new surface does not
    /// compile until it is listed here.
    #[test]
    fn every_surface_scrubs_its_incoming_refs() {
        let body = serde_json::json!({});
        let surfaces = [
            Surface::Patch,
            Surface::Section {
                section: super::SectionName::Storage,
                body: &body,
            },
            Surface::Document { yaml: "" },
            Surface::Internal {
                action: "t",
                target: "t",
            },
        ];
        for surface in surfaces {
            match surface {
                Surface::Patch
                | Surface::Section { .. }
                | Surface::Document { .. }
                | Surface::Internal { .. } => {}
            }
            let mut old = running();
            // The section surface starts from the running provenance.
            old.env_refs
                .insert("B6_NAME".into(), "b6-secret-value-0001".into());
            let mut incoming = old.clone();
            incoming
                .env_refs
                .insert("B6_NAME".into(), "b6-secret-value-0001".into());
            for mode in [Mode::DryRun, Mode::Apply] {
                let mut refs = ScrubMap::new();
                let _ = prepare_with_refs(
                    &old,
                    built(incoming.clone()),
                    &write(surface, mode),
                    &mut refs,
                );
                assert_eq!(
                    refs.get("B6_NAME").map(String::as_str),
                    Some("b6-secret-value-0001"),
                    "a write (apply: {}) scrubs nothing it resolved",
                    matches!(mode, Mode::Apply)
                );
            }
        }
    }

    /// Review E11: a webhook URL list edited around a mask ([mask, C] for
    /// [A, B]) cannot be restored. The write must refuse it at the preserve
    /// step and say why, not fail later on the URL policy with advice to
    /// set `allow_local`.
    #[test]
    fn a_still_masked_url_is_refused_at_preserve() {
        let mut old = running();
        old.event_delivery.webhook_urls = vec![
            "https://a.example/secret1".into(),
            "https://b.example/secret2".into(),
        ];
        let mut incoming = old.clone();
        incoming.event_delivery.webhook_urls = vec![
            crate::config::REDACTED_SENTINEL.into(),
            "https://c.example/new".into(),
        ];
        let w = write(Surface::Document { yaml: "" }, Mode::DryRun);
        let rejection = match prepare(&old, built(incoming), &w) {
            Ok(_) => panic!("an unrestorable mask went through"),
            Err(r) => r,
        };
        assert!(
            matches!(rejection.stage, Stage::Preserve),
            "{}",
            rejection.error
        );
        assert_eq!(rejection.status, StatusCode::BAD_REQUEST);
        assert!(
            rejection.error.contains("webhook_urls[0]"),
            "{}",
            rejection.error
        );
        assert!(
            !rejection.error.contains("allow_local"),
            "{}",
            rejection.error
        );
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
        // Validate = apply: the document validate refuses it the same way.
        let dv = write(Surface::Document { yaml }, Mode::DryRun);
        let r = prepare(&old, built(changed), &dv).unwrap_err();
        assert_eq!(
            (r.stage, r.status),
            (Stage::Bootstrap, StatusCode::FORBIDDEN)
        );
    }

    /// A pre-expanded document (the `config apply` CLI) records no refs: the
    /// validate carries the running refs forward like the apply, so it
    /// reports no "saved as the reference" warnings the apply would not.
    #[test]
    fn document_validate_merges_the_running_env_refs() {
        let mut old = running();
        old.env_refs.insert("T_SECRET".into(), "v4lue".into());
        let dv = write(Surface::Document { yaml: "" }, Mode::DryRun);
        let p = prepare(&old, built(Config::default()), &dv).unwrap();
        assert_eq!(p.new_cfg.env_refs, old.env_refs);
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
        new.blocking_threads = Some(64);
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
        assert_eq!(p.restart, ["blocking_threads changed — restart required"]);
        let diff = p.diff.unwrap();
        assert!(diff["advanced"]["blocking_threads"].is_object(), "{diff}");
    }

    /// R10: an internal write (bootstrap removal, backup secrets restore)
    /// runs the boot gate like every other config write, skips secret
    /// preservation (nothing is redacted) and the hash guard.
    #[test]
    fn internal_writes_run_the_fatal_gate() {
        let old = running();
        let w = write(
            Surface::Internal {
                action: "t",
                target: "t",
            },
            Mode::Apply,
        );
        let mut bad = old.clone();
        bad.buckets.insert(
            "releases".into(),
            crate::bucket_policy::BucketPolicyConfig {
                backend: Some("nope".into()),
                ..Default::default()
            },
        );
        let r = prepare(&old, built(bad), &w).unwrap_err();
        assert_eq!((r.stage, r.status), (Stage::Check, StatusCode::BAD_REQUEST));
        let mut ok = old.clone();
        ok.access_key_id = None;
        ok.bootstrap_password_hash = Some("$2b$12$other".into());
        assert!(prepare(&old, built(ok), &w).is_ok());
    }

    /// R10: every admin config write goes through [`run`]: no other admin
    /// file calls the transition by hand (skipping `check_all`, the gates
    /// and the audit).
    #[test]
    fn only_the_pipeline_calls_the_transition() {
        let mut bad = Vec::new();
        for src in crate::source_scan::ast::src() {
            if !src.rel.starts_with("src/api/admin/")
                || src.rel.ends_with("config/write.rs")
                || src.rel.ends_with("config/transition.rs")
            {
                continue;
            }
            // Every use as a value (a call, an alias, a function pointer); a
            // `pub use` re-export is no call. The old scan wanted
            // `apply_config_transition(` on a line of its own.
            for p in src.prod.paths.iter().chain(&src.test.paths) {
                if p.expr
                    && p.segs
                        .last()
                        .is_some_and(|s| s == "apply_config_transition")
                {
                    bad.push(format!("{}:{}", src.rel, p.line));
                }
            }
        }
        assert!(
            bad.is_empty(),
            "use write::run / run_internal:\n{}",
            bad.join("\n")
        );
    }

    #[tokio::test]
    async fn respond_scrubs_every_string_of_the_body() {
        let refs: ScrubMap = [("S".to_string(), "sekret-value-1234".to_string())].into();
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

    // ── The mask view of an internal writer ──

    const SK: &str = "bootstrap-secret-0001";
    const SSK: &str = "s3-singleton-secret-0002";
    const EK: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

    fn singleton() -> Config {
        let text = "access:\n  access_key_id: AKB\n  secret_access_key: ${env:SK}\n\
            storage:\n  backend:\n    type: s3\n    endpoint: http://127.0.0.1:1\n    region: us-east-1\n    access_key_id: AKS\n    secret_access_key: ${env:SSK}\n  backend_encryption:\n    mode: aes256-gcm-proxy\n    key: ${env:EK}\n    key_id: k\n";
        let env = |n: &str| match n {
            "SK" => Some(SK.to_string()),
            "SSK" => Some(SSK.to_string()),
            "EK" => Some(EK.to_string()),
            _ => None,
        };
        Config::from_template_with(text, &env).unwrap()
    }

    fn internal() -> Surface<'static> {
        Surface::Internal {
            action: "test",
            target: "test",
        }
    }

    fn made(c: Config) -> Result<Built, Rejection> {
        Ok(Built {
            incoming: c,
            warnings: Vec::new(),
        })
    }

    /// A mask that lands anywhere but on its own frozen path is refused with
    /// the path, whether it stands alone, inside a longer text or in a key.
    #[test]
    fn a_mask_that_lands_outside_its_frozen_path_is_refused() {
        let cfg = singleton();
        for (label, edit) in [
            (
                "/public_url",
                (|c: &mut Config, mask: &str| c.public_url = Some(mask.to_string()))
                    as fn(&mut Config, &str),
            ),
            ("/public_url", |c, mask| {
                c.public_url = Some(format!("http://h.example/{mask}/x"))
            }),
            ("/buckets/", |c, mask| {
                c.buckets.insert(mask.to_string(), Default::default());
            }),
        ] {
            let err = build_over_masked_frozen(&cfg, internal(), |masked| {
                let mut c = masked.clone();
                let mask = masked.secret_access_key.clone().unwrap();
                edit(&mut c, &mask);
                made(c)
            })
            .err()
            .unwrap_or_else(|| panic!("{label}: not refused"));
            assert!(err.error.contains(label), "{label}: {}", err.error);
            assert!(!err.error.contains(SK), "{}", err.error);
        }
    }

    /// The singleton's references go with its copy: the new backend `default`
    /// holds the real values, and no mask is left.
    #[test]
    fn the_singleton_copy_gets_the_real_values_and_the_templates() {
        let cfg = singleton();
        let made = build_over_masked_frozen(&cfg, internal(), |masked| {
            let mut c = masked.clone();
            c.add_named_backend(
                crate::config::NamedBackendConfig {
                    name: "second".into(),
                    backend: crate::config::BackendConfig::Filesystem {
                        path: "/data/second".into(),
                    },
                    encryption: Default::default(),
                },
                false,
            )
            .unwrap();
            made(c)
        })
        .unwrap()
        .incoming;
        let default = &made.backends[0];
        assert_eq!(default.name, "default");
        match &default.backend {
            crate::config::BackendConfig::S3 {
                secret_access_key, ..
            } => assert_eq!(secret_access_key.as_deref(), Some(SSK)),
            other => panic!("{other:?}"),
        }
        assert_eq!(default.encryption.primary_key(), Some(EK));
        for path in [
            "/backends/[default]/secret_access_key",
            "/backends/[default]/encryption/key",
        ] {
            assert!(made.env_refs.template_at(path).is_some(), "{path}");
        }
        assert!(!serde_yaml::to_string(&made).unwrap().contains("frozen-"));
        // Persist writes the references at the copy, never the values.
        let text = made.to_canonical_yaml_for_persist_with(&|_| None).unwrap();
        assert!(!text.contains(SSK) && !text.contains(EK), "{text}");
        assert_eq!(text.matches("${env:SSK}").count(), 2, "{text}");
    }

    const AL: &str = "ALLOWED-SECRET-0003";

    /// A running config whose S3 secret comes from an allowlisted reference
    /// that an admin request added: a holder that is not frozen.
    fn allowlisted() -> Config {
        let text = format!(
            "access:\n  access_key_id: AKB\n  secret_access_key: plain-bootstrap-0001\nstorage:\n  backend:\n    type: s3\n    endpoint: http://127.0.0.1:1\n    region: us-east-1\n    access_key_id: AKS\n    secret_access_key: {AL}\n"
        );
        let mut cfg = Config::from_yaml_str(&text).unwrap();
        cfg.env_refs.insert("AL".into(), AL.into());
        cfg.env_refs.templates.insert(
            "/backend/secret_access_key".into(),
            crate::config::env_template::Template::parse("${env:AL}").unwrap(),
        );
        cfg
    }

    fn backend_secret(c: &Config) -> String {
        match &c.backend {
            crate::config::BackendConfig::S3 {
                secret_access_key, ..
            } => secret_access_key.clone().unwrap(),
            other => panic!("{other:?}"),
        }
    }

    /// A fake NEW copy site: a writer that clones a field that comes from a
    /// reference to a path with no declared derive step. Frozen or not, the
    /// write is refused before any transition, with the path.
    #[test]
    fn an_undeclared_copy_of_any_reference_holder_is_refused() {
        type Edit = fn(&mut Config, &str);
        let to_url: Edit = |c, m| c.public_url = Some(m.to_string());
        let inside: Edit = |c, m| c.public_url = Some(format!("http://h.example/{m}/x"));
        let as_key: Edit = |c, m| {
            c.buckets.insert(m.to_string(), Default::default());
        };
        for (cfg, from) in [(singleton(), true), (allowlisted(), false)] {
            for (label, edit) in [
                ("/public_url", to_url),
                ("/public_url", inside),
                ("/buckets/", as_key),
            ] {
                let err = build_over_masked_frozen(&cfg, internal(), |masked| {
                    let mut c = masked.clone();
                    let mask = if from {
                        masked.secret_access_key.clone().unwrap()
                    } else {
                        backend_secret(masked)
                    };
                    edit(&mut c, &mask);
                    made(c)
                })
                .err()
                .unwrap_or_else(|| panic!("frozen={from} {label}: not refused"));
                assert_eq!(err.status, StatusCode::BAD_REQUEST);
                assert!(err.error.contains(label), "{label}: {}", err.error);
                assert!(
                    err.error.contains("comes from the environment"),
                    "{}",
                    err.error
                );
                assert!(
                    !err.error.contains(AL) && !err.error.contains(SK),
                    "{}",
                    err.error
                );
            }
        }
    }

    /// The same copy with its derive step declared is kept, with the real
    /// value and the template; a writer that changes a holder that is not
    /// frozen is free to.
    #[test]
    fn a_declared_copy_of_a_non_frozen_holder_is_kept() {
        let cfg = allowlisted();
        let kept = build_over_masked_frozen(&cfg, internal(), |masked| {
            let mut c = masked.clone();
            c.add_named_backend(
                crate::config::NamedBackendConfig {
                    name: "second".into(),
                    backend: crate::config::BackendConfig::Filesystem {
                        path: "/data/second".into(),
                    },
                    encryption: Default::default(),
                },
                false,
            )
            .unwrap();
            made(c)
        })
        .unwrap()
        .incoming;
        assert_eq!(backend_secret_of_named(&kept, "default"), AL);
        assert!(kept
            .env_refs
            .template_at("/backends/[default]/secret_access_key")
            .is_some());
        let freed = build_over_masked_frozen(&cfg, internal(), |masked| {
            let mut c = masked.clone();
            if let crate::config::BackendConfig::S3 {
                secret_access_key, ..
            } = &mut c.backend
            {
                *secret_access_key = Some("a-new-secret-0004".into());
            }
            made(c)
        })
        .unwrap()
        .incoming;
        assert_eq!(backend_secret(&freed), "a-new-secret-0004");
    }

    fn backend_secret_of_named(c: &Config, name: &str) -> String {
        match &c.backends.iter().find(|b| b.name == name).unwrap().backend {
            crate::config::BackendConfig::S3 {
                secret_access_key, ..
            } => secret_access_key.clone().unwrap(),
            other => panic!("{other:?}"),
        }
    }

    /// A `__dgmigrate_*` route belongs to the migrate job: no admin surface
    /// creates, changes or removes one, and a write that leaves it as it is
    /// (a save during a job) is no change.
    #[test]
    fn no_admin_write_creates_changes_or_removes_a_migrate_route() {
        let route = format!("{}a_0", crate::maintenance::migrate::TRANSIENT_PREFIX);
        let with_route = |compression: bool| {
            let mut c = running();
            c.buckets.insert(
                route.clone(),
                crate::bucket_policy::BucketPolicyConfig {
                    compression: Some(compression),
                    ..Default::default()
                },
            );
            c
        };
        let body = serde_json::json!({});
        for surface in [
            Surface::Patch,
            Surface::Section {
                section: super::SectionName::Storage,
                body: &body,
            },
            Surface::Document { yaml: "" },
            internal(),
        ] {
            let refused = |old: &Config, new: Config| {
                prepare(old, built(new), &write(surface, Mode::DryRun)).err()
            };
            let created = refused(&running(), with_route(true)).expect("create");
            assert_eq!(created.status, StatusCode::BAD_REQUEST);
            assert!(created.error.contains(&route), "{}", created.error);
            assert!(
                refused(&with_route(true), with_route(false)).is_some(),
                "change"
            );
            assert!(refused(&with_route(true), running()).is_some(), "remove");
            assert!(
                refused(&with_route(true), with_route(true)).is_none(),
                "a save that leaves the route as it is"
            );
        }
    }

    /// What persist refuses is refused before the change is live: the pipeline asks
    /// `check_persistable` in `prepare_with_refs` (apply and validate), and the
    /// background writer asks it before its transition.
    #[test]
    fn persist_refusals_come_before_the_transition() {
        let write = crate::source_scan::prod_text(include_str!("write.rs"));
        let prepare = write.find("pub(super) fn prepare_with_refs(").unwrap();
        let check = write.find("incoming.check_persistable()").unwrap();
        assert!(prepare < check, "the check is outside prepare");
        let prepared = write.find(".and_then(|b| prepare_with_refs(cfg, b, write, refs))");
        let applied = write.find("let report = match super::apply_config_transition(");
        assert!(
            prepared < applied,
            "an apply prepares before its transition"
        );
        let apply = crate::source_scan::prod_text(include_str!("../../../config_apply.rs"));
        let check = apply.find(".check_persistable()").unwrap();
        let transition = apply
            .find("apply_config_transition(ctx(), &mut cfg, new_cfg)")
            .unwrap();
        assert!(check < transition, "the background writer persists first");
    }

    /// An edit sees a mask in every frozen string, and the mask of a key
    /// parses as a key: nothing it does with a mask can reach a credential.
    #[test]
    fn the_edit_sees_masks_and_only_masks() {
        let cfg = singleton();
        let mut seen = false;
        build_over_masked_frozen(&cfg, internal(), |masked| {
            seen = true;
            assert_ne!(masked.secret_access_key.as_deref(), Some(SK));
            let key = masked.backend_encryption.primary_key().unwrap();
            assert_ne!(key, EK);
            assert!(crate::storage::EncryptionKey::from_hex(key).is_ok());
            made(masked.clone())
        })
        .unwrap();
        assert!(seen);
    }

    /// The closures that edit a masked view are plain `FnOnce`: they cannot
    /// wait on I/O, so no probe, health gate or request runs on a mask. The
    /// gates of the backend endpoints run on the request body or on the
    /// running config, before the lock.
    #[test]
    fn nothing_acts_on_a_masked_view() {
        let write = crate::source_scan::prod_text(include_str!("write.rs"));
        for sig in [
            "build: impl FnOnce(&Config) -> Result<Built, Rejection>,",
            "edit: impl FnOnce(&mut Config, &mut H) -> Result<(), E>,",
            "edit: impl FnOnce(&mut Config),",
        ] {
            assert!(write.contains(sig), "a writer closure changed: {sig}");
        }
    }
}
