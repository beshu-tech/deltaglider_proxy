// SPDX-License-Identifier: GPL-3.0-only

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
    apply_config_doc, apply_declarative_iam, config_defaults, export_config,
    export_declarative_iam, validate_config_doc, validate_declarative_iam, ConfigApplyResponse,
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
    // RESTRICTIVE-FIRST ORDERING (do not reorder — see audit A3). These two
    // ArcSwaps are published sequentially (nanoseconds apart), so an in-flight
    // request that reads one at an early middleware stage and the other later
    // can briefly observe a torn mix. We publish the PUBLIC-PREFIX snapshot
    // (the narrower-access half) BEFORE the admission chain so that a torn read
    // can only ever grant LESS access, never more: an anonymous request is
    // scoped to whatever prefix snapshot it reads, so seeing the new (e.g.
    // just-privatized, narrower) prefixes + the old admission chain fails
    // closed. Reversing this would open a (still tiny, but real) window where a
    // torn read grants access being removed. Keep prefix-snapshot store first.
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

    // 4c. Phase 3c.3 — Declarative IAM reconciler.
    //
    //     Runs iff the TARGET mode is Declarative AND the IAM fields
    //     actually differ from old_cfg (correctness-xray M2: previously
    //     the reconcile fired on every PATCH touching anything in the
    //     access section — log_level fiddling would trigger the full
    //     diff + sync cycle for no reason).
    //
    //     Covers three transition cases:
    //       - Gui      → Declarative : empty-YAML gate (below), reconcile otherwise.
    //       - Declarative → Declarative : reconcile when IAM fields changed.
    //       - Declarative → Gui / Gui → Gui : no-op (DB owned by GUI).
    //
    //     Validation + diff happens BEFORE any DB write (via diff_iam);
    //     a single SQLite transaction covers every create/update/delete
    //     (via ConfigDb::apply_iam_reconcile). Partial failures roll the
    //     whole reconcile back.
    if matches!(
        new_cfg.iam_mode,
        crate::config_sections::IamMode::Declarative
    ) {
        let yaml_snapshot = crate::iam::snapshot_from_access(
            &new_cfg.iam_users,
            &new_cfg.iam_groups,
            &new_cfg.auth_providers,
            &new_cfg.group_mapping_rules,
        );

        // Short-circuit when the target mode is unchanged AND the IAM
        // fields are identical to the old snapshot. This matters
        // because apply_config_transition runs on any PATCH — most
        // of which (log_level, cache_size_mb, …) don't touch IAM.
        // Without this guard we'd pay reconcile overhead + a config-
        // sync upload on every unrelated admin click.
        let old_iam_unchanged = matches!(
            old_cfg.iam_mode,
            crate::config_sections::IamMode::Declarative
        ) && old_cfg.iam_users == new_cfg.iam_users
            && old_cfg.iam_groups == new_cfg.iam_groups
            && old_cfg.auth_providers == new_cfg.auth_providers
            && old_cfg.group_mapping_rules == new_cfg.group_mapping_rules;

        if !old_iam_unchanged {
            // Empty-gate: only on the gui→declarative flip. A flip to
            // declarative with empty YAML would delete every DB user in
            // one go; make the operator opt in by specifying IAM in YAML.
            if matches!(old_cfg.iam_mode, crate::config_sections::IamMode::Gui)
                && yaml_snapshot.is_empty()
            {
                return Err(
                    "Refusing to flip to iam_mode: declarative with empty IAM in YAML — \
                     this would wipe the existing users/groups in the encrypted config DB. \
                     Add access.iam_users / access.iam_groups to the YAML first, or keep \
                     iam_mode: gui to preserve the DB as source of truth."
                        .to_string(),
                );
            }

            // The reconciler requires a config DB. On instances without
            // one (bootstrap-disabled deployments), declarative mode is
            // meaningless — surface the error at apply time rather than
            // silently succeed.
            let Some(db_arc) = state.config_db.as_ref() else {
                return Err("iam_mode: declarative requires an encrypted config DB; \
                     this instance has none initialised (check DGP_BOOTSTRAP_PASSWORD_HASH \
                     and that the DB was successfully opened at startup)."
                    .to_string());
            };
            let db = db_arc.lock().await;
            let stats = crate::iam::reconcile_declarative_iam(&db, &yaml_snapshot)
                .map_err(|e| format!("declarative IAM reconcile failed (no state changed): {e}"))?;

            // Rebuild the in-memory IAM index from the now-committed DB.
            // `rebuild_iam_index_declarative` bumps `IAM_VERSION` so
            // integration-test barriers fire correctly. The `_declarative`
            // variant skips the legacy-admin auto-migration branch: in
            // declarative mode YAML is authoritative, so auto-creating
            // a row YAML didn't declare would be a silent side-effect
            // that breaks idempotency.
            super::users::rebuild_iam_index_declarative(&db, &state.iam_state)
                .map_err(|e| format!("rebuild_iam_index after reconcile: {e:?}"))?;
            drop(db);

            // Config sync upload only when the reconcile actually changed
            // state (correctness-xray L1). Idempotent re-applies produce
            // stats.is_noop() == true; skipping avoids spurious ETag bumps
            // + peer-replica churn on GitOps reconcile loops.
            if !stats.is_noop() {
                super::trigger_config_sync(state);
            }

            // Emit one audit entry per mutation via the `audit_entries()`
            // helper (hygiene #1: replaces a 10-block copy-paste loop).
            let empty = axum::http::HeaderMap::new();
            for (action, names) in stats.audit_entries() {
                for name in names {
                    super::audit_log(action, "declarative", name, &empty);
                }
            }
            if stats.mapping_rules_replaced > 0 {
                super::audit_log(
                    "iam_reconcile_mapping_rules_replaced",
                    "declarative",
                    &format!("{} rules", stats.mapping_rules_replaced),
                    &empty,
                );
            }

            tracing::info!(
                target: "deltaglider_proxy::config",
                "[declarative-iam] reconciled: {}",
                stats.summary_line()
            );

            // Surface stats as an apply-response warning when anything
            // actually changed; idempotent apply stays silent
            // (operators rely on "no warning line" = "true no-op").
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
        }
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

// === Credential preservation primitives ===
//
// The admin API has TWO write paths that update `Config`: the full-document
// `POST /config/apply` (handled in `document_level.rs`) and the per-section
// `PUT /config/section/:name` (handled in `section_level.rs`). Both must
// implement the same contract for runtime-credential preservation:
//
//   1. Redacted GET → edit non-secret fields → PUT must NOT silently
//      clear credentials that were redacted out of the GET response.
//      → preserve the runtime value when the incoming half is None.
//
//   2. Asymmetric SigV4 pairs are NEVER cross-wired: if the operator set
//      exactly one half of `(access_key_id, secret_access_key)`, we
//      refuse to fill the other from runtime. Filling would produce a
//      superficially-authenticated state that silently fails at signature
//      verification. We emit a warning instead, so the operator can
//      supply the missing half.
//
//   3. Backend type-flips (S3 ↔ Filesystem) drop credentials and emit a
//      warning so the operator notices.
//
//   4. Named backends that disappear from the new config (rename or
//      removal) drop their credentials silently — we warn so a GitOps
//      round-trip doesn't lose state without surfacing it.
//
// These functions are the single source of truth for that contract.
// Both write paths MUST call them; do not inline the logic in handlers.

/// Preserve a SigV4-style credential pair from `old` into `new` when both
/// halves are absent in the incoming doc. If the operator set exactly one
/// half we refuse to fill the other and emit a warning.
///
/// `label` is the human-readable owner of the pair, used in the warning
/// text. Examples: `"proxy-level"`, `"primary backend"`, `"backend 'foo'"`.
pub(super) fn preserve_sigv4_pair(
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

/// Preserve unredacted `event_delivery.webhook_headers` values across a section
/// round-trip. The GET masks each header value to
/// [`crate::config::REDACTED_SENTINEL`] (keeping the key), so an unedited
/// round-trip would otherwise overwrite the real bearer token with the mask.
///
/// Per-key, three cases mirror the SigV4 "None means unchanged" contract:
/// - value still equals the sentinel → the operator did not retype it → restore
///   the old value (or drop the key if `old` has no such header — a masked value
///   for a key that never existed is meaningless, treat as unset).
/// - value differs from the sentinel → the operator typed a real value → keep it.
/// - key absent from `new` → the operator removed it (the merge-patch already
///   applied the delete) → nothing to do here.
pub(super) fn preserve_event_delivery_secrets(
    new: &mut crate::config_sections::EventDeliveryConfig,
    old: &crate::config_sections::EventDeliveryConfig,
) {
    let sentinel = crate::config::REDACTED_SENTINEL;
    let mut drop_keys: Vec<String> = Vec::new();
    for (key, value) in new.webhook_headers.iter_mut() {
        if value == sentinel {
            match old.webhook_headers.get(key) {
                Some(prev) => *value = prev.clone(),
                None => drop_keys.push(key.clone()),
            }
        }
    }
    for key in drop_keys {
        new.webhook_headers.remove(&key);
    }
}

/// Preserve credentials on the PRIMARY backend across a config swap.
///
/// Cases handled:
/// * S3 → S3: same-mode preservation via `preserve_sigv4_pair`.
/// * S3 → Filesystem: type-flip drops creds; warns if old had any.
/// * Filesystem → S3: warns if the operator supplied no creds in the
///   new doc (relying on env / instance creds).
/// * Filesystem → Filesystem: no creds to preserve.
pub(super) fn preserve_primary_backend_creds(
    incoming: &mut crate::config::Config,
    current: &crate::config::Config,
    warnings: &mut Vec<String>,
) {
    use crate::config::BackendConfig;
    match (&mut incoming.backend, &current.backend) {
        (
            BackendConfig::S3 {
                access_key_id: new_akid,
                secret_access_key: new_sk,
                ..
            },
            BackendConfig::S3 {
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
                warnings,
            );
        }
        (
            BackendConfig::Filesystem { .. },
            BackendConfig::S3 {
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
            BackendConfig::S3 {
                access_key_id: new_akid,
                secret_access_key: new_sk,
                ..
            },
            BackendConfig::Filesystem { .. },
        ) if new_akid.is_none() && new_sk.is_none() => {
            warnings.push(
                "primary backend switched from filesystem to S3 but incoming YAML has no credentials — the new backend will rely on instance / env credentials only".to_string(),
            );
        }
        _ => {}
    }
}

/// Preserve credentials on NAMED backends across a config swap.
///
/// Matches old → new entries by `name`. Type-flips drop creds with a
/// warning. Vanished backends (renamed or removed) also warn so a GitOps
/// round-trip doesn't lose state without surfacing it.
pub(super) fn preserve_named_backends_creds(
    incoming: &mut crate::config::Config,
    current: &crate::config::Config,
    warnings: &mut Vec<String>,
) {
    use crate::config::BackendConfig;
    let old_by_name: std::collections::HashMap<&str, &BackendConfig> = current
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
                BackendConfig::S3 {
                    access_key_id: new_akid,
                    secret_access_key: new_sk,
                    ..
                },
                Some(BackendConfig::S3 {
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
                    warnings,
                );
            }
            (BackendConfig::S3 { .. }, Some(BackendConfig::Filesystem { .. }))
            | (BackendConfig::Filesystem { .. }, Some(BackendConfig::S3 { .. })) => {
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
            BackendConfig::S3 {
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
            ..
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
        allow_local: false,
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

#[cfg(test)]
mod preserve_tests {
    use super::*;
    use crate::config::REDACTED_SENTINEL;
    use crate::config_sections::EventDeliveryConfig;
    use std::collections::BTreeMap;

    fn ed_with(headers: &[(&str, &str)]) -> EventDeliveryConfig {
        EventDeliveryConfig {
            webhook_headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..EventDeliveryConfig::default()
        }
    }

    #[test]
    fn preserve_restores_untouched_sentinel_value() {
        // Operator left "Authorization" masked → restore the old token.
        let old = ed_with(&[("Authorization", "Bearer real-token")]);
        let mut new = ed_with(&[("Authorization", REDACTED_SENTINEL)]);
        preserve_event_delivery_secrets(&mut new, &old);
        assert_eq!(
            new.webhook_headers.get("Authorization").map(String::as_str),
            Some("Bearer real-token")
        );
    }

    #[test]
    fn preserve_keeps_retyped_value() {
        // Operator typed a new token → keep it, don't restore the old one.
        let old = ed_with(&[("Authorization", "Bearer old")]);
        let mut new = ed_with(&[("Authorization", "Bearer NEW")]);
        preserve_event_delivery_secrets(&mut new, &old);
        assert_eq!(
            new.webhook_headers.get("Authorization").map(String::as_str),
            Some("Bearer NEW")
        );
    }

    #[test]
    fn preserve_drops_sentinel_for_unknown_key() {
        // A masked value for a key the old config never had is meaningless → drop.
        let old = ed_with(&[]);
        let mut new = ed_with(&[("X-New", REDACTED_SENTINEL)]);
        preserve_event_delivery_secrets(&mut new, &old);
        assert!(!new.webhook_headers.contains_key("X-New"));
    }

    #[test]
    fn preserve_leaves_removed_key_removed() {
        // Operator removed "Authorization" (absent from new) → stays removed;
        // a different untouched header is restored.
        let old = ed_with(&[("Authorization", "Bearer real"), ("X-Env", "prod")]);
        let mut new = ed_with(&[("X-Env", REDACTED_SENTINEL)]);
        preserve_event_delivery_secrets(&mut new, &old);
        assert!(!new.webhook_headers.contains_key("Authorization"));
        assert_eq!(
            new.webhook_headers.get("X-Env").map(String::as_str),
            Some("prod")
        );
    }

    #[test]
    fn redact_masks_header_values_keeps_keys() {
        // The GET-side redaction (config.rs) masks values but keeps keys.
        let cfg = crate::config::Config {
            event_delivery: ed_with(&[("Authorization", "Bearer secret"), ("X-Env", "prod")]),
            ..crate::config::Config::default()
        };
        let redacted = cfg.redact_all_secrets();
        let h: &BTreeMap<String, String> = &redacted.event_delivery.webhook_headers;
        assert_eq!(
            h.get("Authorization").map(String::as_str),
            Some(REDACTED_SENTINEL)
        );
        assert_eq!(h.get("X-Env").map(String::as_str), Some(REDACTED_SENTINEL));
        assert_eq!(h.len(), 2, "keys must survive redaction");
    }
}
