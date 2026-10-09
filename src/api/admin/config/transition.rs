// SPDX-License-Identifier: BUSL-1.1

//! THE config transition: every runtime config change (admin PATCH, section
//! PUT, document apply, backup restore, bootstrap-pair removal, and the
//! background [`crate::config_apply::ConfigMutator`]) goes through
//! [`apply_config_transition`].

use std::sync::Arc;

use axum::http::HeaderMap;
use tokio::sync::RwLockWriteGuard;
use tracing_subscriber::EnvFilter;

use super::super::{AdminState, Bare};
use crate::api::handlers::AppState;
use crate::config::Config;

/// Who runs a transition, and so which runtime handles it has.
pub(crate) enum TransitionCtx<'a> {
    /// The admin API: every step runs.
    Admin {
        state: &'a Arc<AdminState>,
        headers: &'a HeaderMap,
        /// The write puts back a config that was live before (a rollback):
        /// the forward-only gates (the A14 lockout check) do not apply.
        restoring: bool,
    },
    /// A background task (the migrate stage/flip via `ConfigMutator`). It
    /// has the engine, the backend gates and nothing else, so it SKIPS the
    /// steps named by [`BACKGROUND_SKIPPED_STEPS`]; a change that needs one
    /// of them is refused before anything is published.
    Background {
        app: &'a AppState,
        /// Log line of the engine rebuild.
        context: &'a str,
    },
}

/// The steps a [`TransitionCtx::Background`] transition does not run, and the
/// config fields whose change needs each of them.
pub(crate) const BACKGROUND_SKIPPED_STEPS: &[(&str, &str)] = &[
    ("log filter reload", "log_level"),
    (
        "SigV4 / IAM state publish",
        "access_key_id, secret_access_key, authentication",
    ),
    (
        "declarative IAM reconcile",
        "iam_mode, iam_users, iam_groups, auth_providers, group_mapping_rules",
    ),
    (
        "public-prefix snapshot and admission chain rebuild",
        "public_prefixes of a bucket, admission_blocks",
    ),
];

/// Pure: the [`BACKGROUND_SKIPPED_STEPS`] entries that `old → new` needs.
pub(crate) fn background_needs_skipped_steps(old: &Config, new: &Config) -> Vec<&'static str> {
    let public = |c: &Config| {
        c.buckets
            .iter()
            .filter(|(_, p)| !p.public_prefixes.is_empty())
            .map(|(b, p)| (b.clone(), p.public_prefixes.clone()))
            .collect::<Vec<_>>()
    };
    let needs = [
        old.log_level != new.log_level,
        old.access_key_id != new.access_key_id
            || old.secret_access_key != new.secret_access_key
            || old.open_access_requested() != new.open_access_requested(),
        old.iam_mode != new.iam_mode
            || old.iam_users != new.iam_users
            || old.iam_groups != new.iam_groups
            || old.auth_providers != new.auth_providers
            || old.group_mapping_rules != new.group_mapping_rules,
        public(old) != public(new) || old.admission_blocks != new.admission_blocks,
    ];
    BACKGROUND_SKIPPED_STEPS
        .iter()
        .zip(needs)
        .filter(|(_, n)| *n)
        .map(|((step, _), _)| *step)
        .collect()
}

impl TransitionCtx<'_> {
    fn app(&self) -> &AppState {
        match self {
            Self::Admin { state, .. } => &state.s3_state,
            Self::Background { app, .. } => app,
        }
    }
}

/// What a successful transition reports.
pub(crate) struct TransitionReport {
    pub warnings: Vec<String>,
    /// OIDC discovery of a provider set the reconcile published. Network
    /// I/O: the caller runs it after it drops the config write guard
    /// (review A8).
    pub discovery: Option<super::super::external_auth::ProviderDiscovery>,
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
pub(super) fn engine_affecting_fields_changed(
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
        // The engine snapshots these at construction (engine/construction.rs), so a
        // change is a silent no-op without a rebuild (reported applied:true but
        // the running engine keeps the old limit / codec parallelism).
        || old.max_passthrough_object_size != new.max_passthrough_object_size
        || old.codec_concurrency != new.codec_concurrency
        || old.range_spool_ttl_secs != new.range_spool_ttl_secs
}

/// Side effects of transitioning the runtime config from `**cfg` to `new_cfg`.
///
/// This is the **single source of truth** for what happens when the
/// config changes. It takes the config WRITE guard, so no caller can run
/// it without the lock, and it stores `new_cfg` into the guard as its last
/// step: a concurrent `state.config.read()` never sees the old config with
/// the new engine (or the reverse).
///
/// ## Contract: `Err` ⇒ no runtime state changed
///
/// The helper runs in two phases (review 4 config-1):
///
/// 1. **Pre-commit** — every fallible step, with no side effect: the
///    background-scope check, config-graph and capability/health gates, the
///    declarative-IAM validation and the engine BUILD (the new engine is
///    held, not stored).
/// 2. **Commit** — [`commit_iam`] first (the lockout re-check and the
///    declarative-IAM reconcile are its only fallible steps; the reconcile is
///    one SQLite transaction, so its `Err` changes nothing; then it publishes
///    the IAM state), then the infallible publishes: log filter,
///    bucket-derived snapshots, the engine store, and the config swap LAST.
///
/// A failure after the reconcile commits (the in-memory index rebuild) is a
/// warning, not an `Err`: the DB and the new config already agree.
pub(crate) async fn apply_config_transition(
    ctx: TransitionCtx<'_>,
    cfg: &mut RwLockWriteGuard<'_, Config>,
    new_cfg: Config,
) -> Result<TransitionReport, String> {
    let old_cfg: &Config = cfg;
    // ── Phase 1: pre-commit (fallible, no side effect) ───────────────────
    if let TransitionCtx::Background { .. } = ctx {
        let skipped = background_needs_skipped_steps(old_cfg, &new_cfg);
        if !skipped.is_empty() {
            return Err(format!(
                "a background config change cannot run: {} (change it through the admin API)",
                skipped.join(", ")
            ));
        }
    }
    transition_gates(&ctx, old_cfg, &new_cfg).await?;
    let admin = match &ctx {
        TransitionCtx::Admin {
            state,
            headers,
            restoring,
        } => Some((*state, *headers, *restoring)),
        TransitionCtx::Background { .. } => None,
    };
    let new_engine = if engine_affecting_fields_changed(old_cfg, &new_cfg) {
        Some(crate::config_apply::build_engine(ctx.app(), &new_cfg).await?)
    } else {
        None
    };

    // ── Phase 2: commit ──────────────────────────────────────────────────
    // The only fallible commit step; it runs before every publish below.
    let (mut warnings, discovery) = match admin {
        Some((state, headers, restoring)) => {
            commit_iam(state, old_cfg, &new_cfg, headers, restoring).await?
        }
        None => (Vec::new(), None),
    };
    // Nothing below returns Err.

    // Log-level hot reload. An invalid filter is a warning: the old filter
    // stays, the config change goes through.
    if let Some((state, _, _)) = admin.filter(|_| old_cfg.log_level != new_cfg.log_level) {
        match crate::audit::with_audit_directive(&new_cfg.log_level).parse::<EnvFilter>() {
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

    // Bucket-derived snapshots — public prefix + admission chain.
    if let Some((state, _, _)) = admin.filter(|_| {
        old_cfg.buckets != new_cfg.buckets || old_cfg.admission_blocks != new_cfg.admission_blocks
    }) {
        super::rebuild_bucket_derived_snapshots(state, &new_cfg.buckets, &new_cfg.admission_blocks);
    }

    // IAM-mode flips are security-meaningful (the declarative "escape hatch"
    // is flip to gui → mutate → flip back): a warn-level line for SIEM.
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

    // Restart-required fields: applied in memory, live only after a restart.
    // `requires_restart_warnings` is the single source (the section dry-run
    // uses it too).
    warnings.extend(requires_restart_warnings(old_cfg, &new_cfg));

    // The engine store and the config swap are the LAST steps: every check
    // above passed, and the write guard is held across both.
    if let Some(engine) = new_engine {
        let context = match &ctx {
            TransitionCtx::Admin { .. } => "Engine rebuilt on config transition",
            TransitionCtx::Background { context, .. } => context,
        };
        crate::config_apply::install_engine(ctx.app(), engine, context);
    }
    **cfg = new_cfg;

    Ok(TransitionReport {
        warnings,
        discovery,
    })
}

/// The gates of [`transition_gates`] that need no I/O. The validate
/// endpoints run them too, so a validate answers what the apply would (the
/// backend probes and the declarative DB validation run on apply only).
pub(super) fn static_transition_gates(
    old_cfg: &crate::config::Config,
    new_cfg: &crate::config::Config,
) -> Result<(), String> {
    // FATAL config-graph errors (bucket → undefined backend, duplicate
    // backend names): un-runnable states that boot refuses too.
    let mut fatal = new_cfg.check_fatal();
    // A webhook URL that every delivery would refuse: refuse the apply that
    // adds it, instead of failing (and retrying) every event later.
    fatal.extend(
        new_cfg
            .event_delivery
            .newly_refused_webhook_urls(&old_cfg.event_delivery),
    );
    if !fatal.is_empty() {
        return Err(format!("config refused: {}", fatal.join("; ")));
    }
    // S8: the boot refuses a sync bucket without DGP_CONFIG_DB_KEY; an apply
    // must not set one up for the next restart either.
    crate::config_db::key::check_sync_bucket_change(
        old_cfg.config_sync_bucket.as_deref(),
        new_cfg.config_sync_bucket.as_deref(),
        crate::config::process_env(crate::config_db::key::CONFIG_DB_KEY_ENV).as_deref(),
    )
    .map_err(|e| format!("config refused: {e}"))
}

/// The pre-commit gates of [`apply_config_transition`] that need no plan
/// output: config-graph errors, the sync-bucket key rule, the backend
/// capability and health probes, and the declarative-IAM validation.
async fn transition_gates(
    ctx: &TransitionCtx<'_>,
    old_cfg: &crate::config::Config,
    new_cfg: &crate::config::Config,
) -> Result<(), String> {
    let app = ctx.app();
    static_transition_gates(old_cfg, new_cfg)?;

    // Backend write-capability gate (guard B, hot-apply half): refuse routing
    // a client-writable bucket onto a non-CAS backend under multi-instance.
    crate::coordination::capability::hot_apply_capability_gate(new_cfg, &app.backend_capabilities)
        .await?;

    // Backend HEALTH gate: a backend whose DEFINITION changes must pass a
    // live connection probe ("Test connection" built into apply).
    crate::coordination::health::hot_apply_health_gate(new_cfg, &app.backend_health).await?;

    // Declarative-IAM validation (H8/H19): the same fallible checks as the
    // reconcile, write-free. A background change never needs it (refused
    // above when the IAM fields change).
    match ctx {
        TransitionCtx::Admin { state, .. } => {
            declarative_iam_precommit_gate(state, old_cfg, new_cfg).await
        }
        TransitionCtx::Background { .. } => Ok(()),
    }
}

/// See [`crate::config::Config::declarative_reconcile_needed`].
pub(super) fn declarative_reconcile_needed(
    old_cfg: &crate::config::Config,
    new_cfg: &crate::config::Config,
) -> bool {
    new_cfg.declarative_reconcile_needed(old_cfg)
}

/// The IAM half of a transition, in ONE DB-locked section: (a) the A14
/// lockout re-check, (b) the declarative reconcile (one SQLite
/// transaction), (c) the publish of the new config's empty-IAM outcome. A
/// rebuild elsewhere (user delete, peer sync) takes the same lock, so
/// neither undoes the other. Fallible only before its first write.
async fn commit_iam(
    state: &Arc<AdminState>,
    old_cfg: &crate::config::Config,
    new_cfg: &crate::config::Config,
    headers: &HeaderMap,
    restoring: bool,
) -> Result<
    (
        Vec<String>,
        Option<super::super::external_auth::ProviderDiscovery>,
    ),
    String,
> {
    let mut warnings = Vec::new();
    let yaml_snapshot = declarative_reconcile_needed(old_cfg, new_cfg).then(|| {
        crate::iam::snapshot_from_access(
            &new_cfg.iam_users,
            &new_cfg.iam_groups,
            &new_cfg.auth_providers,
            &new_cfg.group_mapping_rules,
            &[],
        )
    });
    if let Some(yaml) = &yaml_snapshot {
        // The empty-YAML gate and the DB-presence check ran in the pre-commit
        // gate; they repeat here because the DB lock was released in between.
        if matches!(old_cfg.iam_mode, crate::config_sections::IamMode::Gui)
            && yaml.declares_no_users_or_groups()
        {
            return Err(EMPTY_DECLARATIVE_FLIP.to_string());
        }
        if state.config_db.is_none() {
            return Err(NO_CONFIG_DB_FOR_DECLARATIVE.to_string());
        }
    }
    let outcome = new_cfg.empty_iam_outcome();
    // Nothing IAM-side changes: no lock (a held write, such as the rule
    // delete, keeps the DB lock across its transition) and nothing to
    // refuse (the lockout rule compares two equal surfaces).
    if yaml_snapshot.is_none() && state.iam_state.load().when_empty() == outcome {
        return Ok((warnings, None));
    }
    let db = match &state.config_db {
        Some(db) => Some(db.lock().await),
        None => None,
    };

    // (a) A14: the change may not take S3 from a credential to none.
    if !restoring {
        let current = state.iam_state.load();
        let users_now = match &db {
            Some(db) => db
                .load_users()
                .map_err(|e| format!("could not count the IAM users (no state changed): {e}"))?
                .len(),
            None => current.user_count(),
        };
        let users_after = match (&yaml_snapshot, &db) {
            (Some(yaml), Some(db)) => crate::iam::preview_declarative_iam(db, yaml)
                .map_err(|e| format!("declarative IAM reconcile failed (no state changed): {e}"))?
                .users_after(users_now),
            _ => users_now,
        };
        let before = current.when_empty();
        let surface = |users, when_empty| crate::iam::AuthSurface { users, when_empty };
        crate::iam::check_lockout(surface(users_now, &before), surface(users_after, &outcome))
            .map_err(|e| e.to_string())?;
    }

    // (b) The declarative reconcile: the only fallible write.
    let mut stats = None;
    if let (Some(yaml), Some(db)) = (&yaml_snapshot, &db) {
        stats = Some(
            crate::iam::reconcile_declarative_iam(db, yaml)
                .map_err(|e| format!("declarative IAM reconcile failed (no state changed): {e}"))?,
        );
        // The DB is committed from here: a failed in-memory rebuild is a
        // warning, never an `Err` (the caller would keep the old config over
        // the new DB). The `_declarative` variant skips the legacy-admin
        // auto-migration: YAML is authoritative.
        if let Err(e) = super::super::users::rebuild_iam_index_declarative::<Bare>(
            db,
            &state.iam_state,
            outcome.clone(),
        ) {
            warnings.push(format!(
                "declarative IAM reconciled, but the in-memory IAM index could not be rebuilt \
                 ({:?}): the previous index serves until the next IAM change or restart",
                e.status_code()
            ));
        }
    }

    // (c) What S3 does with no IAM user, from the NEW config (judged on the
    // state after the reconcile). Users stay; only the outcome changes.
    let current = state.iam_state.load();
    if let Some(next) = current.with_when_empty(outcome.clone()) {
        let users_exist = current.has_iam_users();
        state.iam_state.store(Arc::new(next));
        crate::iam::bump_iam_version();
        if users_exist && old_cfg.bootstrap_pair() != new_cfg.bootstrap_pair() {
            warnings.push(
                "IAM mode is active: the bootstrap SigV4 pair is only the fallback for an \
                 empty IAM database. IAM users (including 'legacy-admin', which carries the \
                 old pair) are unchanged; manage them in the Users panel."
                    .to_string(),
            );
        }
        if !users_exist {
            tracing::info!("S3 authentication now: {}", outcome.describe());
        }
    }
    // The live provider set follows the reconcile under the same DB lock:
    // a provider disabled in YAML stops before the lock is released. Its
    // discovery (network I/O) goes back to the caller, which runs it after
    // it drops the config write guard (review A8: under the guard, every
    // LIST and config read waited for it).
    let mut discovery = None;
    if let (Some(stats), Some(db)) = (&stats, &db) {
        if stats.providers_changed() {
            match super::super::external_auth::publish_provider_set(state, db) {
                Ok(d) => discovery = d,
                Err(e) => warnings.push(format!(
                    "declarative IAM reconciled, but the live OAuth providers could not be \
                     rebuilt ({:?}): the previous providers serve until the next provider \
                     change or restart",
                    e.status_code()
                )),
            }
        }
    }
    drop(db);

    let Some(stats) = stats else {
        return Ok((warnings, discovery));
    };

    // Sync + stats warning only when the reconcile changed state: GitOps
    // re-applies stay silent and cause no peer churn.
    if !stats.is_noop() {
        super::super::trigger_config_sync(state);
    }
    for (action, names) in stats.audit_entries() {
        for name in names {
            super::super::audit_log(action, "declarative", name, headers);
        }
    }
    if stats.mapping_rules_replaced > 0 {
        super::super::audit_log(
            "iam_reconcile_mapping_rules_replaced",
            "declarative",
            &format!("{} rules", stats.mapping_rules_replaced),
            headers,
        );
    }
    tracing::info!(
        target: "deltaglider_proxy::config",
        "[declarative-iam] reconciled: {}",
        stats.summary_line()
    );
    if !stats.is_noop() {
        warnings.push(format!(
            "declarative IAM reconciled: {} users total, {} groups total, \
             {} providers total — {}",
            stats.users_total,
            stats.groups_total,
            stats.providers_total,
            stats.summary_line(),
        ));
    }
    Ok((warnings, discovery))
}

const EMPTY_DECLARATIVE_FLIP: &str =
    "Refusing to flip to iam_mode: declarative with no iam_users or iam_groups \
     in YAML — this would wipe the existing users/groups in the encrypted config DB. \
     Add access.iam_users / access.iam_groups to the YAML first, or keep \
     iam_mode: gui to preserve the DB as source of truth.";

const NO_CONFIG_DB_FOR_DECLARATIVE: &str =
    "iam_mode: declarative requires an encrypted config DB; \
     this instance has none initialised (check DGP_BOOTSTRAP_PASSWORD_HASH \
     and that the DB was successfully opened at startup).";

/// Pre-commit gate for the declarative-IAM reconcile (H8/H19). Runs the SAME
/// fallible checks as [`reconcile_declarative_iam`] — empty-YAML gate,
/// config-DB presence, and `diff_iam` validation — WITHOUT writing anything.
/// Both use [`declarative_reconcile_needed`], so they cannot disagree.
async fn declarative_iam_precommit_gate(
    state: &Arc<AdminState>,
    old_cfg: &crate::config::Config,
    new_cfg: &crate::config::Config,
) -> Result<(), String> {
    if !declarative_reconcile_needed(old_cfg, new_cfg) {
        return Ok(());
    }
    let yaml_snapshot = crate::iam::snapshot_from_access(
        &new_cfg.iam_users,
        &new_cfg.iam_groups,
        &new_cfg.auth_providers,
        &new_cfg.group_mapping_rules,
        &[],
    );
    if matches!(old_cfg.iam_mode, crate::config_sections::IamMode::Gui)
        && yaml_snapshot.declares_no_users_or_groups()
    {
        return Err(EMPTY_DECLARATIVE_FLIP.to_string());
    }
    let Some(db_arc) = state.config_db.as_ref() else {
        return Err(NO_CONFIG_DB_FOR_DECLARATIVE.to_string());
    };
    let db = db_arc.lock().await;
    crate::iam::validate_declarative_iam(&db, &yaml_snapshot)
        .map_err(|e| format!("declarative IAM reconcile failed (no state changed): {e}"))
}

/// Return one warning per restart-required field that changed between
/// `old` and `new`. Empty vec = no restart required.
///
/// Single source of truth for the restart-required fieldset (the GUI chips
/// follow it, see `restart_chip_parity_tests`): [`apply_config_transition`] uses this to
/// emit warnings + set its `requires_restart` flag, and the write pipeline's
/// dry run uses the same predicate.
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
    // main.rs sizes the tokio blocking pool before the runtime exists.
    if old.blocking_threads != new.blocking_threads {
        out.push("blocking_threads changed — restart required".to_string());
    }
    // TLS is bound once at startup (tls.rs) and config_sync_bucket launches its
    // poller once at startup — neither is re-read on hot-apply, so a change here
    // is applied to config/disk but has NO runtime effect until restart. Without
    // this warning the operator believes TLS is on / sync is enabled when it is
    // not (silent no-op reported as success).
    if old.tls != new.tls {
        out.push("tls config changed — restart required to (re)bind the listener".to_string());
    }
    if old.config_sync_bucket != new.config_sync_bucket {
        out.push(
            "config_sync_bucket changed — restart required to (re)start the sync poller"
                .to_string(),
        );
    }
    out
}

/// The GUI's "Restart required" chips and [`RESTART_ONLY_YAML_PATHS`] name the
/// same fields: a chip on a hot field tells the operator to restart for
/// nothing, a missing chip hides a change that has no effect yet.
#[cfg(test)]
mod restart_chip_parity_tests {
    use super::requires_restart_warnings;
    use crate::config::{Config, TlsConfig};

    /// The YAML paths that the proxy reads only at startup. Everything else
    /// is hot: engine-affecting fields rebuild the engine, which also
    /// re-sizes the reference cache and the codec permits.
    const RESTART_ONLY_YAML_PATHS: &[&str] = &[
        "advanced.listen_addr",
        "advanced.tls",
        "advanced.blocking_threads",
        "advanced.config_sync_bucket",
    ];

    /// The list is the one `requires_restart_warnings` checks: a change of
    /// each field warns.
    #[test]
    fn the_list_is_what_requires_restart_warnings_checks() {
        let base = Config::default();
        type Change = (&'static str, fn(&mut Config));
        let changes: [Change; 4] = [
            ("advanced.listen_addr", |c| {
                c.listen_addr = "127.0.0.1:1".parse().unwrap()
            }),
            ("advanced.tls", |c| {
                c.tls = Some(TlsConfig {
                    enabled: true,
                    cert_path: None,
                    key_path: None,
                })
            }),
            ("advanced.blocking_threads", |c| {
                c.blocking_threads = Some(3)
            }),
            ("advanced.config_sync_bucket", |c| {
                c.config_sync_bucket = Some("s".into())
            }),
        ];
        assert_eq!(changes.map(|(p, _)| p), RESTART_ONLY_YAML_PATHS);
        for (path, change) in changes {
            let mut c = base.clone();
            change(&mut c);
            assert_eq!(requires_restart_warnings(&base, &c).len(), 1, "{path}");
        }
    }

    fn is_restart_only(path: &str) -> bool {
        RESTART_ONLY_YAML_PATHS
            .iter()
            .any(|p| path == *p || path.starts_with(&format!("{p}.")))
    }

    #[test]
    fn gui_restart_chips_match_the_server_list() {
        let src = include_str!("../../../../demo/s3-browser/ui/src/components/advancedPanels.tsx");
        // Each FormField: its label (with or without a chip) precedes its yamlPath.
        let mut chipped = Vec::new();
        let mut plain = Vec::new();
        for field in src.split("<FormField").skip(1) {
            let Some(i) = field.find("yamlPath=\"") else {
                continue;
            };
            let rest = &field[i + 10..];
            let path = &rest[..rest.find('"').unwrap()];
            if field[..i].contains("<RestartChip") {
                chipped.push(path.to_string());
            } else {
                plain.push(path.to_string());
            }
        }
        assert!(!chipped.is_empty(), "the parser found no chip");
        for p in &chipped {
            assert!(
                is_restart_only(p),
                "{p} has a Restart required chip but applies live"
            );
        }
        for p in &plain {
            assert!(!is_restart_only(p), "{p} needs a restart but has no chip");
        }
        for p in RESTART_ONLY_YAML_PATHS {
            assert!(
                chipped
                    .iter()
                    .any(|c| is_restart_only(c) && c.starts_with(p)),
                "{p} has no chip in the GUI"
            );
        }
    }
}

/// Review 4 config-1: `apply_config_transition` returns `Err` only before its
/// first runtime publish, and the engine store and config swap are its last
/// steps.
#[cfg(test)]
mod transition_order_tests {
    use super::background_needs_skipped_steps;
    use crate::config::Config;

    /// A background change may reroute buckets (the migrate stage/flip) but
    /// never touch what only the admin side can publish.
    #[test]
    fn background_scope_names_each_skipped_step() {
        let old = Config::default();
        let mut routed = old.clone();
        routed.buckets.insert(
            "b".into(),
            crate::bucket_policy::BucketPolicyConfig {
                backend: Some("other".into()),
                ..Default::default()
            },
        );
        assert!(background_needs_skipped_steps(&old, &routed).is_empty());

        let mut public = routed.clone();
        public.buckets.get_mut("b").unwrap().public_prefixes = vec!["pub/".into()];
        assert_eq!(
            background_needs_skipped_steps(&old, &public),
            ["public-prefix snapshot and admission chain rebuild"]
        );
        let mut all = public.clone();
        all.log_level = "warn".into();
        all.access_key_id = Some("K".into());
        all.iam_mode = crate::config_sections::IamMode::Declarative;
        assert_eq!(background_needs_skipped_steps(&old, &all).len(), 4);
        let mut open = old.clone();
        open.authentication = Some("none".into());
        assert_eq!(
            background_needs_skipped_steps(&old, &open),
            ["SigV4 / IAM state publish"],
            "`authentication` decides the empty-IAM outcome"
        );
    }

    /// The body of `apply_config_transition` (comments stripped).
    fn transition_body() -> String {
        fn_body(concat!("pub(crate) async fn ", "apply_config_transition("))
    }

    /// The body of the fn whose signature starts with `head`.
    fn fn_body(head: &str) -> String {
        let src = include_str!("transition.rs");
        let start = src.find(head).unwrap();
        let end = start + src[start..].find("\n}\n").unwrap();
        src[start..end]
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn no_err_after_the_first_publish_and_the_engine_store_is_last() {
        let body = transition_body();
        let publishes = [
            ".store(",
            "log_reload.reload(",
            "rebuild_bucket_derived_snapshots(",
            "install_engine(",
            "**cfg = new_cfg;",
        ];
        let first_publish = publishes
            .iter()
            .filter_map(|p| body.find(p))
            .min()
            .expect("publishes found");
        let tail = &body[first_publish..];
        assert!(
            !tail.contains(")?") && !tail.contains("return Err"),
            "a fallible step follows a publish in apply_config_transition:\n{tail}"
        );
        // Every fallible step (gates, engine build, the IAM commit) sits
        // before the first publish.
        for step in ["transition_gates(", "build_engine(", "commit_iam("] {
            let at = body.find(step).unwrap_or_else(|| panic!("{step} missing"));
            assert!(at < first_publish, "{step} runs after a publish");
        }
        let engine_store = body.find("install_engine(").unwrap();
        let swap = body.find("**cfg = new_cfg;").unwrap();
        assert!(
            engine_store < swap,
            "the config swap follows the engine store"
        );
        for p in publishes {
            if let Some(at) = body.rfind(p) {
                assert!(at <= swap, "{p} runs after the config swap");
            }
        }
    }

    /// Review A8: the transition runs under the config write guard, so it
    /// makes no network request. OIDC discovery goes back to the caller in
    /// `TransitionReport::discovery`.
    #[test]
    fn the_transition_runs_no_oidc_discovery() {
        let src = include_str!("transition.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        for call in ["discover_all", "rebuild_external_auth(", ".run().await"] {
            assert!(!prod.contains(call), "transition.rs calls {call}");
        }
        assert!(prod.contains("publish_provider_set("));
    }

    /// `commit_iam` is fallible only before its first write: after the
    /// reconcile committed, nothing returns `Err`.
    #[test]
    fn commit_iam_fails_only_before_its_first_write() {
        let body = fn_body(concat!("async fn ", "commit_iam("));
        let lockout = body.find("check_lockout(").expect("the lockout re-check");
        let reconcile = body
            .find("reconcile_declarative_iam(")
            .expect("the reconcile");
        let publish = body.find(".store(").expect("the IAM publish");
        assert!(lockout < reconcile && reconcile < publish);
        let after_write = &body[body.find("rebuild_iam_index_declarative").unwrap()..];
        assert!(
            !after_write.contains(")?") && !after_write.contains("return Err"),
            "commit_iam can fail after the reconcile committed:\n{after_write}"
        );
    }
}
