// SPDX-License-Identifier: BUSL-1.1

//! THE config transition: every runtime config change (admin PATCH, section
//! PUT, document apply, backup restore, bootstrap-pair removal, and the
//! background [`crate::config_apply::ConfigMutator`]) goes through
//! [`apply_config_transition`].

use std::sync::Arc;

use axum::http::HeaderMap;
use tokio::sync::RwLockWriteGuard;
use tracing_subscriber::EnvFilter;

use super::super::AdminState;
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
        /// The transition puts back the config that a failed persist
        /// replaced: the encryption witness restores what the failed
        /// change published instead of publishing.
        restoring: bool,
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
/// Every `Config` field is classified below, with no `..`: a new field does
/// not compile until it is put in a class (docs/plan/config-model.md). The
/// engine snapshots its fields at construction (engine/construction.rs), so
/// an engine field missing from the first list is a silent no-op apply
/// (reported applied:true, the running engine keeps the old value).
pub(super) fn engine_affecting_fields_changed(
    old: &crate::config::Config,
    new: &crate::config::Config,
) -> bool {
    macro_rules! classify {
        (engine: [$($e:ident),* $(,)?], other: [$($o:ident),* $(,)?] $(,)?) => {{
            let crate::config::Config { $($e: _,)* $($o: _,)* } = old;
            false $(|| old.$e != new.$e)*
        }};
    }
    classify! {
        engine: [
            backend,
            backend_encryption,
            backends,
            default_backend,
            buckets,
            max_object_size,
            cache_size_mb,
            max_delta_ratio,
            metadata_cache_mb,
            max_passthrough_object_size,
            codec_concurrency,
            range_spool_ttl_secs,
        ],
        other: [
            defaults_version,
            // Restart class (`requires_restart_warnings`). The engine reads
            // config_sync_bucket too, but a change needs a restart anyway.
            listen_addr,
            blocking_threads,
            tls,
            config_sync_bucket,
            // Read per request or by their own subsystem.
            filtered_list_max_engine_pages,
            authentication,
            access_key_id,
            secret_access_key,
            bootstrap_password_hash,
            log_level,
            config_sync_object_key,
            replication,
            lifecycle,
            event_delivery,
            jobs,
            admission_blocks,
            iam_mode,
            iam_users,
            iam_groups,
            auth_providers,
            group_mapping_rules,
            // Runtime-only. `tuning` is fixed for the process (env-only).
            env_refs,
            env_shadow,
            tuning,
        ],
    }
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
    // A bucket whose write encryption changes: existing objects keep their
    // form; the response names the re-encrypt job (issue #116).
    warnings.extend(new_cfg.bucket_encryption_notes(old_cfg));
    // One audit entry per subject whose write encryption changes (issue
    // #116 P3): here, so every write path (document, section, field,
    // backends, backup, migrate flip) emits it once. Key ids only.
    audit_encryption_changes(&ctx, &new_cfg.encryption_changes(old_cfg));

    // The engine store and the config swap are the LAST steps: every check
    // above passed, and the write guard is held across both.
    let engine_rebuilt = new_engine.is_some();
    if let Some(engine) = new_engine {
        let context = match &ctx {
            TransitionCtx::Admin { .. } => "Engine rebuilt on config transition",
            TransitionCtx::Background { context, .. } => context,
        };
        crate::config_apply::install_engine(ctx.app(), engine, context);
    }
    // Multi-instance (issue #116 P3): publish this node's encryption of
    // every bucket that the change re-encrypted, and of every bucket that
    // the request confirmed as a downgrade (also with no config change: the
    // supported way to finish a downgrade that the nodes got by restart),
    // so the other instances refuse writes until they agree. The gate of
    // this node follows the new config before any request. A failed
    // publish is a warning: the change is live, the witness retries the
    // publish at each check, and until one succeeds this node refuses the
    // changed buckets. A ROLLBACK (a failed create, a failed persist)
    // publishes nothing: it puts back what the failed change published (a
    // rollback to plaintext would otherwise publish a downgrade that no
    // request confirmed).
    let confirmed_downgrades = match &ctx {
        TransitionCtx::Admin {
            headers,
            restoring: false,
            ..
        } => crate::config::confirmed_subjects(
            Some(headers),
            crate::config::CONFIRM_ENCRYPTION_DOWNGRADE_HEADER,
        ),
        _ => Vec::new(),
    };
    let rollback = match &ctx {
        TransitionCtx::Admin { restoring, .. } | TransitionCtx::Background { restoring, .. } => {
            *restoring
        }
    };
    if let Some(witness) = &ctx.app().encryption_witness {
        let published = if rollback {
            Some(witness.restore_last_publish(&new_cfg).await)
        } else if engine_rebuilt || !confirmed_downgrades.is_empty() {
            Some(
                witness
                    .publish_changes(old_cfg, &new_cfg, &confirmed_downgrades)
                    .await,
            )
        } else {
            None
        };
        if let Some(Err(e)) = published {
            warnings.push(format!(
                "the encryption witness in the sync bucket was not updated ({e}). The change \
                 is live; the witness tries again at each check (every \
                 DGP_ENCRYPTION_WITNESS_POLL_SECS), and until it succeeds this instance \
                 refuses writes to the buckets whose encryption the change moved away from \
                 the shared one"
            ));
        }
    }
    // Event delivery or replication may have started or stopped reading the
    // outbox: the append path skips its write while nothing reads it.
    if let Some(db) = ctx.app().jobs_db.as_ref() {
        crate::event_outbox::set_config_readers(db, &new_cfg);
    }
    **cfg = new_cfg;

    Ok(TransitionReport {
        warnings,
        discovery,
    })
}

/// The audit entries of a transition's encryption changes.
fn audit_encryption_changes(ctx: &TransitionCtx<'_>, changes: &[crate::config::EncryptionChange]) {
    let none = HeaderMap::new();
    let (user, headers) = match ctx {
        TransitionCtx::Admin { headers, .. } => ("admin", *headers),
        TransitionCtx::Background { .. } => ("background", &none),
    };
    for c in changes {
        crate::audit::audit_log(
            c.kind.audit_action(),
            user,
            &c.audit_target(),
            headers,
            c.subject.bucket().unwrap_or(""),
            "",
        );
    }
}

/// The request header that confirms the removal of a bucket's legacy
/// key ids (comma-separated), after a legacy-key usage scan (issue #116
/// review A). `config apply --confirm-key-removal` sends it.
pub(crate) const CONFIRM_KEY_REMOVAL_HEADER: &str = "x-dgp-confirm-key-removal";

/// What the request confirms, and the buckets that hold no object: the
/// inputs of [`static_transition_gates`].
#[derive(Debug, Default)]
pub(super) struct GateInputs {
    /// The key ids that the request confirms for removal.
    pub key_removals: Vec<String>,
    /// The confirmed downgrade subjects; `None` = not gated.
    pub downgrades: Option<Vec<String>>,
    /// The confirmed plaintext-refusal subjects; `None` = not gated.
    pub refusals: Option<Vec<String>>,
    /// The configured buckets (old ∪ new) that no backend lists.
    pub absent: Vec<String>,
}

impl GateInputs {
    /// THE builder. `headers` = the request's (`None` for a background
    /// change). `forward` = a forward admin write: only it judges the
    /// downgrade and plaintext-refusal gates (a rollback puts back a config
    /// that was live; a background change was confirmed when it started).
    pub(super) async fn gather(
        app: &AppState,
        old_cfg: &Config,
        new_cfg: &Config,
        headers: Option<&HeaderMap>,
        forward: bool,
    ) -> Self {
        use crate::config::{
            confirmed_subjects, CONFIRM_ENCRYPTION_DOWNGRADE_HEADER,
            CONFIRM_PLAINTEXT_REFUSAL_HEADER,
        };
        let judged = |header: &str| forward.then(|| confirmed_subjects(headers, header));
        let mut absent = absent_buckets(app, old_cfg).await;
        absent.extend(absent_buckets(app, new_cfg).await);
        Self {
            key_removals: confirmed_subjects(headers, CONFIRM_KEY_REMOVAL_HEADER),
            downgrades: judged(CONFIRM_ENCRYPTION_DOWNGRADE_HEADER),
            refusals: judged(CONFIRM_PLAINTEXT_REFUSAL_HEADER),
            absent,
        }
    }
}

/// The gates of [`transition_gates`] that need no I/O. The validate
/// endpoints run them too, so a validate answers what the apply would (the
/// backend probes and the declarative DB validation run on apply only).
pub(super) fn static_transition_gates(
    old_cfg: &crate::config::Config,
    new_cfg: &crate::config::Config,
    inputs: &GateInputs,
) -> Result<(), String> {
    let absent_buckets = &inputs.absent;
    // FATAL config-graph errors (bucket → undefined backend, duplicate
    // backend names): un-runnable states that boot refuses too.
    let mut fatal = new_cfg.check_fatal();
    // A backend set to encrypt with no key refuses every write (issue
    // #116): refuse the apply that makes one, instead of the writes later.
    fatal.extend(new_cfg.newly_keyless_proxy_backends(old_cfg));
    // A bucket's active key never leaves the keyring in one step (issue
    // #116): its objects would become unreadable.
    fatal.extend(new_cfg.dropped_bucket_keys(old_cfg, &inputs.key_removals, absent_buckets));
    // A bucket that would store plaintext where it encrypted needs an
    // explicit confirmation (issue #116 P3).
    fatal.extend(new_cfg.unconfirmed_encryption_downgrades(old_cfg, inputs.downgrades.as_deref()));
    // A subject that starts to encrypt refuses its plaintext history
    // unless it serves it (issue #120): explicit confirmation, or `serve`.
    fatal.extend(new_cfg.unconfirmed_plaintext_refusals(
        old_cfg,
        inputs.refusals.as_deref(),
        absent_buckets,
    ));
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

/// The configured buckets with their own encryption block that the storage
/// listing does not show: they hold no object (see
/// `Config::dropped_bucket_keys`). A failed listing, or a backend that did
/// not answer (an `unavailable` placeholder lists the bucket), counts every
/// bucket as present, so the key gate stays on.
pub(super) async fn absent_buckets(app: &AppState, cfg: &crate::config::Config) -> Vec<String> {
    let with_block: Vec<&String> = cfg
        .buckets
        .iter()
        .filter(|(_, p)| p.encryption.is_some())
        .map(|(n, _)| n)
        .collect();
    if with_block.is_empty() {
        return Vec::new();
    }
    let Ok(listed) = app.engine.load().list_bucket_origins().await else {
        return Vec::new();
    };
    with_block
        .into_iter()
        .filter(|n| !listed.iter().any(|b| b.name.eq_ignore_ascii_case(n)))
        .cloned()
        .collect()
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
    // A background change (migrate) confirms nothing; the gates judge
    // forward admin writes only.
    let (headers, forward) = match ctx {
        TransitionCtx::Admin {
            headers, restoring, ..
        } => (Some(*headers), !*restoring),
        TransitionCtx::Background { .. } => (None, false),
    };
    let inputs = GateInputs::gather(app, old_cfg, new_cfg, headers, forward).await;
    static_transition_gates(old_cfg, new_cfg, &inputs)?;

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
        if let Err(e) = super::super::users::rebuild_iam_index_declarative(
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

#[cfg(test)]
mod keyless_proxy_gate_tests {
    use super::{static_transition_gates, GateInputs};
    use crate::config::{BackendEncryptionConfig, Config};

    /// A forward admin write that confirms `refusals` (`None` = not gated),
    /// with `absent` buckets and no other confirmation.
    fn inputs(absent: &[&str], refusals: Option<&[&str]>) -> GateInputs {
        let owned = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        GateInputs {
            key_removals: Vec::new(),
            downgrades: Some(Vec::new()),
            refusals: refusals.map(owned),
            absent: owned(absent),
        }
    }

    fn proxy(key: Option<&str>) -> BackendEncryptionConfig {
        BackendEncryptionConfig::Aes256GcmProxy {
            key: key.map(str::to_string),
            key_id: None,
            legacy_key: None,
            legacy_key_id: None,
            plaintext_reads: Default::default(),
        }
    }

    /// Issue #116 P0.1: an apply that leaves a backend in proxy mode with
    /// no key is refused (its writes would all fail); a keyed one passes,
    /// and a backend that was keyless before does not block other edits.
    #[test]
    fn an_apply_that_makes_a_keyless_proxy_backend_is_refused() {
        let old = Config::default();
        let mut keyless = old.clone();
        keyless.backend_encryption = proxy(None);
        let err = static_transition_gates(&old, &keyless, &inputs(&[], None))
            .expect_err("a keyless proxy backend must be refused");
        assert!(
            err.contains("backend 'default'") && err.contains("DGP_ENCRYPTION_KEY"),
            "the refusal names the backend and the env var: {err}"
        );

        let mut keyed = old.clone();
        keyed.backend_encryption = proxy(Some(&"ab".repeat(32)));
        static_transition_gates(&old, &keyed, &inputs(&[], None))
            .expect("a keyed proxy backend applies");

        let mut unrelated = keyless.clone();
        unrelated.log_level = "warn".into();
        static_transition_gates(&keyless, &unrelated, &inputs(&[], None))
            .expect("a backend that was keyless before does not block another edit");
    }

    /// #120: a subject that starts to encrypt with `plaintext_reads: refuse`
    /// needs a confirmation, unless it serves its plaintext history or the
    /// bucket does not exist yet.
    #[test]
    fn a_bucket_that_starts_to_encrypt_must_serve_or_confirm_its_history() {
        let yaml =
            |enc: &str| format!("storage:\n  filesystem: /tmp/x\n  buckets:\n    releases:\n{enc}");
        let old = Config::from_yaml_str(&yaml("      compression: true\n")).unwrap();
        let block = |extra: &str| {
            format!(
                "      encryption:\n        mode: aes256-gcm-proxy\n        key: \"{}\"\n        \
                 key_id: releases-2026\n{extra}",
                "ab".repeat(32)
            )
        };
        let refuse = Config::from_yaml_str(&yaml(&block(""))).unwrap();
        let err = static_transition_gates(&old, &refuse, &inputs(&[], Some(&[])))
            .expect_err("the history would be refused");
        assert!(
            err.contains("bucket 'releases'") && err.contains("X-DGP-Confirm-Plaintext-Refusal"),
            "{err}"
        );
        static_transition_gates(&old, &refuse, &inputs(&[], Some(&["releases"])))
            .expect("confirmed");
        static_transition_gates(&old, &refuse, &inputs(&["releases"], Some(&[])))
            .expect("a bucket that does not exist has no history");
        let serve =
            Config::from_yaml_str(&yaml(&block("        plaintext_reads: serve\n"))).unwrap();
        static_transition_gates(&old, &serve, &inputs(&[], Some(&[])))
            .expect("the history is served");
    }

    /// #120 review: a bucket that drops its `mode: none` block and so
    /// starts to INHERIT an encrypting backend is gated by its own name
    /// (the backend did not change, so no backend subject covers it). A
    /// bucket that follows a backend which starts to encrypt is covered by
    /// the backend's subject.
    #[test]
    fn a_bucket_that_starts_to_inherit_encryption_must_serve_or_confirm_its_history() {
        let yaml = |backend_enc: &str, bucket_enc: &str| {
            Config::from_yaml_str(&format!(
                "storage:\n  backends:\n    - name: local-disk\n      type: filesystem\n      \
                 path: /tmp/x\n{backend_enc}  default_backend: local-disk\n  buckets:\n    \
                 downloads: {{ backend: local-disk{bucket_enc} }}\n"
            ))
            .unwrap()
        };
        let aes = |extra: &str| {
            format!(
                "      encryption: {{ mode: aes256-gcm-proxy, key: \"{}\", key_id: local-2026{extra} }}\n",
                "ab".repeat(32)
            )
        };
        let own_none = ", encryption: { mode: none }";
        let old = yaml(&aes(""), own_none);
        let new = yaml(&aes(""), "");
        let err = static_transition_gates(&old, &new, &inputs(&[], Some(&[])))
            .expect_err("the inheriting bucket's history would be refused");
        assert!(
            err.contains("bucket 'downloads'")
                && err.contains("X-DGP-Confirm-Plaintext-Refusal: downloads"),
            "{err}"
        );
        static_transition_gates(&old, &new, &inputs(&[], Some(&["downloads"])))
            .expect("confirmed by the bucket's name");
        static_transition_gates(&old, &new, &inputs(&["downloads"], Some(&[])))
            .expect("a bucket that does not exist has no history");
        let (old_s, new_s) = (
            yaml(&aes(", plaintext_reads: serve"), own_none),
            yaml(&aes(", plaintext_reads: serve"), ""),
        );
        static_transition_gates(&old_s, &new_s, &inputs(&[], Some(&[])))
            .expect("the backend serves the history");

        // The backend starts to encrypt: its subject covers the bucket.
        let (plain, enc) = (yaml("", ""), yaml(&aes(""), ""));
        let err = static_transition_gates(&plain, &enc, &inputs(&[], Some(&[])))
            .expect_err("the backend's history would be refused");
        assert!(
            err.contains("backend:local-disk") && !err.contains("bucket 'downloads'"),
            "{err}"
        );
        static_transition_gates(&plain, &enc, &inputs(&[], Some(&["backend:local-disk"])))
            .expect("the backend's confirmation covers its inheriting bucket");
    }

    /// The documented shape keeps working: the YAML names the mode, the
    /// `DGP_BACKEND_<NAME>_ENCRYPTION_KEY` env var supplies the key.
    #[test]
    fn a_key_from_the_env_var_is_not_keyless() {
        let yaml = "storage:\n  backends:\n  - name: hetzner-fsn1\n    type: filesystem\n    \
                    path: /tmp/x\n    encryption:\n      mode: aes256-gcm-proxy\n";
        let mut cfg = Config::from_yaml_str(yaml).unwrap();
        assert_eq!(cfg.keyless_proxy_backends(), ["hetzner-fsn1"]);
        let key = "cd".repeat(32);
        cfg.apply_env_overrides_at_load(&|n: &str| {
            (n == "DGP_BACKEND_HETZNER_FSN1_ENCRYPTION_KEY").then(|| key.clone())
        });
        assert!(cfg.keyless_proxy_backends().is_empty());
    }

    /// Only the backends the engine wraps count: the singleton block is
    /// unused once named backends exist.
    #[test]
    fn only_wrapped_backends_count_as_keyless() {
        let mut cfg = Config {
            backend_encryption: proxy(None),
            ..Default::default()
        };
        cfg.backends.push(crate::config::NamedBackendConfig {
            name: "local-disk".into(),
            backend: cfg.backend.clone(),
            encryption: proxy(None),
        });
        assert_eq!(cfg.keyless_proxy_backends(), ["local-disk"]);
    }
}
