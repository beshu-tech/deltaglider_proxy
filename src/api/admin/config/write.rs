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

use tokio::sync::RwLockWriteGuard;

use super::super::AdminState;
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
    /// Carry the running config's env-ref provenance into the document.
    merge_env_refs: bool,
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
                merge_env_refs: false,
            },
            Surface::Section { .. } => Steps {
                bootstrap_guard: true,
                validate: true,
                normalize: true,
                merge_env_refs: false,
            },
            Surface::Document { .. } => Steps {
                bootstrap_guard: true,
                validate: true,
                normalize: false,
                merge_env_refs: true,
            },
            // Starts from the full running config (nothing redacted, env
            // refs carried), so only the gates of `validate` run.
            Surface::Internal { .. } => Steps {
                bootstrap_guard: false,
                validate: true,
                normalize: false,
                merge_env_refs: false,
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
        /// One line per changed restart-required field; empty = none.
        restart: Vec<String>,
        diff: Option<serde_json::Value>,
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
            Ok(mut p) => match super::transition::static_transition_gates(&old, &p.new_cfg) {
                // What the apply would refuse, a validate refuses too.
                Err(error) => Outcome::Rejected(Rejection {
                    stage: Stage::Transition,
                    status: StatusCode::UNPROCESSABLE_ENTITY,
                    error,
                    warnings: Box::new(p.warnings),
                    diff: p.diff,
                }),
                Ok(()) => {
                    if write.surface.section().is_some() {
                        p.warnings.preview = declarative_preview(state, &old, &p.new_cfg).await;
                    }
                    Outcome::Validated {
                        warnings: p.warnings,
                        restart: p.restart,
                        diff: p.diff,
                    }
                }
            },
        };
        return WriteResult { outcome, refs };
    }

    // Apply: the write lock is held from the version check to the persist.
    let mut cfg = state.config.write().await;
    let outcome = apply_locked(state, &mut cfg, &write, &mut refs, build).await;
    if let Outcome::Applied {
        persist: Ok(path) | Err((path, _)),
        ..
    } = &outcome
    {
        audit_write(&write, path);
    }
    WriteResult { outcome, refs }
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

/// Steps 1–5 of an apply, under the caller's config write guard: version
/// check, build, prepare, transition, persist. The caller audits.
async fn apply_locked(
    state: &Arc<AdminState>,
    cfg: &mut RwLockWriteGuard<'_, Config>,
    write: &ConfigWrite<'_>,
    refs: &mut ScrubMap,
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
    let prepared = match build(cfg).and_then(|b| prepare_with_refs(cfg, b, write, refs)) {
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
        // A backup restore's rollback puts back the config that was live.
        restoring: matches!(
            write.surface,
            Surface::Internal {
                action: RESTORE_ROLLBACK_ACTION,
                ..
            }
        ),
    };
    let report = match super::apply_config_transition(ctx, cfg, new_cfg).await {
        Ok(r) => r,
        Err(e) => {
            return Outcome::Rejected(Rejection {
                stage: Stage::Transition,
                status: StatusCode::UNPROCESSABLE_ENTITY,
                error: e,
                warnings: Box::new(warnings),
                diff,
            });
        }
    };
    refs.extend(cfg.env_refs.clone());
    warnings.transition = report.warnings;
    let version = super::version::config_version(cfg, section);
    Outcome::Applied {
        warnings,
        // The transition computes the same list from the same two configs.
        restart,
        diff,
        persist: persist(state, cfg),
        version,
    }
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
            let resolved = incoming.resolve_env_ref_scalars();
            refs.extend(incoming.env_refs.clone());
            if let Err(e) = resolved {
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
                if !incoming.env_refs.contains_key(name) {
                    incoming.env_refs.insert(name.clone(), value.clone());
                    if let Some(path) = old.env_refs.paths.get(name) {
                        incoming.env_refs.paths.insert(name.clone(), path.clone());
                    }
                    if let Some(default) = old.env_refs.defaults.get(name) {
                        incoming
                            .env_refs
                            .defaults
                            .insert(name.clone(), default.clone());
                    }
                }
            }
        }
        _ => {}
    }
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
    let mut cfg = state.config.write().await;
    let mut held = hold.await;
    let old = cfg.clone();
    let mut refused = None;
    let outcome = apply_locked(state, &mut cfg, &config_write, &mut refs, |running| {
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
    })
    .await;
    if let Some(e) = refused {
        return Err(HeldRefusal::Edit(e));
    }
    let scrub = |s: String| crate::config::scrub_env_values(&s, &refs);
    let (warnings, persist) = match outcome {
        Outcome::Applied {
            warnings, persist, ..
        } => (warnings, persist),
        Outcome::Rejected(r) => {
            return Err(HeldRefusal::Pipeline(match r.stage {
                Stage::EnvReapply => InternalRefusal::EnvReapply(scrub(r.error)),
                Stage::Transition => InternalRefusal::Transition(scrub(r.error)),
                _ => InternalRefusal::Invalid {
                    status: r.status,
                    error: scrub(r.error),
                },
            }))
        }
        Outcome::Conflict { .. } | Outcome::Validated { .. } => {
            unreachable!("an internal write is an apply without If-Match")
        }
    };
    if let (Err((path, error)), OnPersistError::RollBack) = (&persist, write.on_persist_error) {
        roll_back(state, &mut cfg, write.headers, old, false).await;
        return Err(HeldRefusal::Persist {
            path: path.clone(),
            error: scrub(error.clone()),
        });
    }
    if let Err(e) = commit(held).await {
        roll_back(state, &mut cfg, write.headers, old, persist.is_ok()).await;
        return Err(HeldRefusal::Edit(e));
    }
    let (Ok(path) | Err((path, _))) = &persist;
    audit_write(&config_write, path);
    let mut all = warnings.env;
    all.extend(warnings.check_new);
    all.extend(warnings.transition);
    Ok(InternalApplied {
        warnings: all.into_iter().map(scrub).collect(),
        persist: persist.map_err(|(p, e)| (p, scrub(e))),
    })
}

/// The internal-write action of a backup restore's rollback: it puts back
/// a config that was live, so the forward-only gates do not apply.
pub(crate) const RESTORE_ROLLBACK_ACTION: &str = "restore_rollback";

/// Put `old` back after a failed internal write, and into the file when
/// the write reached it.
async fn roll_back(
    state: &Arc<AdminState>,
    cfg: &mut RwLockWriteGuard<'_, Config>,
    headers: &HeaderMap,
    old: Config,
    re_persist: bool,
) {
    let ctx = TransitionCtx::Admin {
        state,
        headers,
        restoring: true,
    };
    if let Err(e) = super::apply_config_transition(ctx, cfg, old).await {
        tracing::error!("config write rollback failed, the new config stays live: {e}");
        return;
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
        let offenders: Vec<String> = crate::source_scan::prod_sources("src")
            .into_iter()
            .filter(|(rel, _)| !ALLOWED.contains(&rel.as_str()))
            .filter(|(_, text)| crate::source_scan::prod_text(text).contains(".persist_to_file("))
            .map(|(rel, _)| rel)
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
        for p in crate::source_scan::rust_files("src/api/admin") {
            let path = p.to_string_lossy().replace('\\', "/");
            if path.ends_with("config/write.rs") || path.ends_with("config/transition.rs") {
                continue;
            }
            let src = std::fs::read_to_string(&p).unwrap();
            for (i, line) in src.lines().enumerate() {
                if line.contains("apply_config_transition(") && !line.trim_start().starts_with("//")
                {
                    bad.push(format!("{path}:{}: {}", i + 1, line.trim()));
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
}
