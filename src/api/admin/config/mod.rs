// SPDX-License-Identifier: BUSL-1.1

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
mod transition;
mod version;
mod write;
#[cfg(test)]
use transition::engine_affecting_fields_changed;
use transition::requires_restart_warnings;
pub(crate) use transition::{apply_config_transition, TransitionCtx};
pub use version::install_version_key as install_config_version_key;
pub(crate) use write::{run_internal, InternalRefusal};

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

pub(crate) use document_level::apply_config_inner_with_env;
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

use crate::api::admin::extract::AdminJson;
use axum::extract::State;
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::iam::IamState;

use super::{AdminError, AdminState, Bare, JsonError, Text};

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
    crate::config_apply::rebuild_engine_only(&state.s3_state, cfg, context).await
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

/// S7 (review 4 config-2): an admin write may type a `${env:NAME}` that the
/// boot file resolves into ANY string field (a bucket alias, a rule name);
/// the value then comes back in check warnings, errors and the section
/// diff. Every config write response scrubs through it (see
/// [`write::ScrubEnv`]): each string leaf gets the recorded values replaced
/// by their refs.
pub(super) fn scrub_env_json(
    v: &mut serde_json::Value,
    refs: &std::collections::BTreeMap<String, String>,
) {
    match v {
        serde_json::Value::String(s) => *s = crate::config::scrub_env_values(s, refs),
        serde_json::Value::Array(items) => items.iter_mut().for_each(|x| scrub_env_json(x, refs)),
        serde_json::Value::Object(map) => map.values_mut().for_each(|x| scrub_env_json(x, refs)),
        _ => {}
    }
}

/// Re-apply the `DGP_*` overrides to an edited config (env wins at runtime,
/// see `Config::reapply_env_overrides`) and return the operator warnings:
/// one per env-controlled field the edit changed, one per secret now written
/// as an `${env:NAME}` reference. With `document`, also one per field the
/// document set to exactly its env value (the file keeps its old value).
pub(crate) fn reapply_env(
    running: &crate::config::Config,
    edited: &mut crate::config::Config,
    document: bool,
) -> Result<Vec<String>, String> {
    let report = edited
        .reapply_env_overrides(running, &crate::config::process_env)
        .map_err(|e| format!("environment overrides could not be applied: {e}"))?;
    Ok(env_reapply_warnings(&report, document))
}

/// Pure: the warnings for an [`crate::config::EnvReapply`] report.
pub(crate) fn env_reapply_warnings(
    report: &crate::config::EnvReapply,
    document: bool,
) -> Vec<String> {
    let mut out: Vec<String> = report.edited.iter().map(|f| env_edit_warning(f)).collect();
    for name in &report.refs_added {
        out.push(format!(
            "The value of the secret environment variable {name} is saved to the config file \
             as the reference ${{env:{name}}}, never as the value. Keep {name} set."
        ));
    }
    if document {
        // Secrets are excluded: a redacted secret is filled in from the
        // running (env) value on purpose, which looks the same.
        const SECRET_LEAVES: &[&str] = &[
            "secret_access_key",
            "key",
            "legacy_key",
            "bootstrap_password_hash",
        ];
        for f in &report.echoed_over_file {
            let leaf = f.rsplit('.').next().unwrap_or(f);
            if !SECRET_LEAVES.contains(&leaf) {
                out.push(format!(
                    "The document sets {f} to the value of its environment variable. The config \
                     file keeps its previous value for {f}; the environment value stays in effect."
                ));
            }
        }
    }
    out
}

/// The warning for an edit to a field an environment variable controls.
pub(crate) fn env_edit_warning(field: &str) -> String {
    format!(
        "An environment variable sets {field}. The new value is saved to the config file, \
         but the environment value stays in effect."
    )
}

/// Split the advisory warnings of the RESULTING config into those this change
/// introduces and those the CURRENT config already produces. Pure; unit-tested.
///
/// Returns `(new, existing)`. Matching is by exact text and counts duplicates
/// (a warning that appears twice after and once before is one new, one
/// existing), so a change that adds a second copy of a problem still shows it.
/// Order follows `after`. The apply dialog shows `new` prominently and folds
/// `existing` away, so unrelated standing warnings stop drowning the review.
pub(crate) fn split_new_warnings(
    before: &[String],
    after: Vec<String>,
) -> (Vec<String>, Vec<String>) {
    let mut remaining: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for w in before {
        *remaining.entry(w.as_str()).or_default() += 1;
    }
    let mut new = Vec::new();
    let mut existing = Vec::new();
    for w in after {
        match remaining.get_mut(w.as_str()) {
            Some(n) if *n > 0 => {
                *n -= 1;
                existing.push(w);
            }
            _ => new.push(w),
        }
    }
    (new, existing)
}

// === Credential preservation primitives ===
//
// The section PUT and the document apply redact-round-trip secrets, and the
// write pipeline (`write.rs`) runs ONE preservation step for both
// ([`preserve_runtime_secrets`]). Its contract:
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
// These functions are the single source of truth for that contract; do not
// inline the logic in handlers.

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
        // The key id is not a secret, so exports show it: an unchanged id
        // with no secret is an untouched round-trip, not a rotation.
        (Some(k), None) if old_akid.as_deref() == Some(k.as_str()) => {
            *new_sk = old_sk.clone();
        }
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

/// What `DELETE /api/admin/config/bootstrap-credentials` does.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum BootstrapRemoval {
    /// No pair in the config: nothing to do (idempotent).
    NothingToRemove,
    /// `DGP_ACCESS_KEY_ID` / `DGP_SECRET_ACCESS_KEY` set it: the env var
    /// must go, the config edit would not stick.
    EnvControlled,
    /// No IAM users: removing the pair turns authentication off.
    WouldDisableAuth,
    Remove,
}

/// Pure decision for the remove-bootstrap-credentials endpoint.
pub(crate) fn bootstrap_removal_decision(
    has_pair: bool,
    env_controlled: bool,
    iam_active: bool,
) -> BootstrapRemoval {
    if !has_pair {
        BootstrapRemoval::NothingToRemove
    } else if env_controlled {
        BootstrapRemoval::EnvControlled
    } else if !iam_active {
        BootstrapRemoval::WouldDisableAuth
    } else {
        BootstrapRemoval::Remove
    }
}

/// `DELETE /api/admin/config/bootstrap-credentials` — remove the bootstrap
/// SigV4 pair from the config: the explicit action the Credentials page
/// offers (clearing both fields was ambiguous). Refused (409) while no IAM
/// users exist, and when env vars set the pair.
pub async fn remove_bootstrap_credentials(
    State(state): State<Arc<AdminState>>,
    headers: axum::http::HeaderMap,
) -> axum::response::Response {
    // The refusals answer `{"error": ..}`; an env re-apply failure a text 500.
    let conflict = |msg: &str| AdminError::<JsonError>::conflict(msg).into_response();
    let iam_active = matches!(&**state.iam_state.load(), IamState::Iam(_));
    let (env_controlled, has_pair) = {
        let cfg = state.config.read().await;
        (
            cfg.tuning.bootstrap_pair_from_env,
            cfg.access_key_id.is_some() || cfg.secret_access_key.is_some(),
        )
    };
    match bootstrap_removal_decision(has_pair, env_controlled, iam_active) {
        BootstrapRemoval::NothingToRemove => {
            return Json(serde_json::json!({ "removed": false, "warnings": [] })).into_response()
        }
        BootstrapRemoval::EnvControlled => {
            return conflict(
                "the bootstrap SigV4 pair comes from DGP_ACCESS_KEY_ID / DGP_SECRET_ACCESS_KEY: \
                 unset those environment variables and restart",
            )
        }
        BootstrapRemoval::WouldDisableAuth => {
            return conflict(
                "no IAM users exist: removing the bootstrap SigV4 pair would leave the proxy \
                 without authentication. Create an IAM admin user first",
            )
        }
        BootstrapRemoval::Remove => {}
    }
    let mut removed_key = None;
    let applied = run_internal(
        &state,
        &headers,
        "remove_bootstrap_credentials",
        "access_key_id",
        |cfg| {
            removed_key = cfg.access_key_id.take();
            cfg.secret_access_key = None;
        },
    )
    .await;
    let mut warnings = match applied {
        Ok(a) => {
            let mut w = a.warnings;
            if let Err((path, e)) = a.persist {
                w.push(format!("Failed to persist config to {path}: {e}"));
            }
            w
        }
        Err(InternalRefusal::EnvReapply(e)) => {
            return AdminError::<Text>::internal(e).into_response()
        }
        Err(InternalRefusal::Transition(e)) => return conflict(&e),
        Err(InternalRefusal::Invalid { status, error }) => {
            return AdminError::<JsonError>::status(status, error).into_response()
        }
    };
    // The same key may live on as an IAM user (the first IAM user carries
    // the pair over as 'legacy-admin'): say so, it still signs requests.
    let still_iam_user = removed_key
        .as_deref()
        .and_then(|k| match &**state.iam_state.load() {
            IamState::Iam(index) => index.get(k).map(|u| u.name.clone()),
            _ => None,
        });
    if let Some(name) = still_iam_user {
        warnings.push(format!(
            "the removed access key id is also IAM user '{name}', which still signs S3 \
             requests: delete or disable that user in the Users panel to revoke the key"
        ));
    }
    Json(serde_json::json!({ "removed": true, "warnings": warnings })).into_response()
}

/// Merge the runtime secrets into an incoming (redacted) config: the ONE
/// preservation step of every section and document write (the write
/// pipeline calls it). For every secret the GET/export surfaces redact, a
/// value absent from the body keeps the runtime value; a literal value
/// rotates it.
///
/// Returns the warnings for every case the merge cannot carry creds forward
/// safely (backend renames, backend-type swaps, asymmetric pairs), so an
/// operator is never caught by a silent auth loss. `Err` only for an
/// invalid encryption-key edit.
pub(super) fn preserve_runtime_secrets(
    incoming: &mut crate::config::Config,
    current: &crate::config::Config,
    probe: &section_level::BackendEncryptionKeyProbe,
) -> Result<Vec<String>, String> {
    let mut warnings = Vec::new();
    // Per-backend AES keys: three-state per field (absent = keep, null =
    // clear, string = rotate). The probe read the RAW body, because after
    // the merge absent and null are both `None`. A rebuilt engine with
    // `key: None` would write plaintext and strand every encrypted object.
    section_level::preserve_all_backend_encryption(incoming, current, probe)?;
    preserve_sigv4_pair(
        &mut incoming.access_key_id,
        &mut incoming.secret_access_key,
        &current.access_key_id,
        &current.secret_access_key,
        "proxy-level",
        &mut warnings,
    );
    preserve_primary_backend_creds(incoming, current, &mut warnings);
    preserve_named_backends_creds(incoming, current, &mut warnings);
    // Webhook header values are masked to REDACTED_SENTINEL on GET/export.
    preserve_event_delivery_secrets(&mut incoming.event_delivery, &current.event_delivery);
    Ok(warnings)
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
    // Slack bot token: an untouched (sentinel) value preserves the old token; a
    // sentinel with no old token to restore is meaningless → clear it.
    if new.slack_bot_token.as_deref() == Some(sentinel) {
        new.slack_bot_token = old.slack_bot_token.clone();
    }
    // Slack incoming-webhook URLs are masked to the sentinel on export (the
    // hooks.slack.com path token is the credential); restore an untouched one.
    // A sentinel with no old value to restore is meaningless → leave it (config
    // validation will reject a literal sentinel as not a valid URL).
    if new.webhook_url.as_deref() == Some(sentinel) {
        new.webhook_url = old.webhook_url.clone();
    }
    // webhook_urls are masked element-wise on export. A masked entry carries no
    // identity, so index-based restore is only SOUND when the list wasn't
    // reordered or resized — otherwise index i in the new list can align to the
    // WRONG old URL and silently restore a different secret. Restore by index
    // ONLY when the lengths match (a pure in-place edit); if they differ, leave
    // the sentinel in place so config validation rejects it (the operator must
    // supply the real URL for the entry they added/moved) rather than us guessing.
    if new.webhook_urls.len() == old.webhook_urls.len() {
        for (i, url) in new.webhook_urls.iter_mut().enumerate() {
            if url == sentinel {
                if let Some(prev) = old.webhook_urls.get(i) {
                    *url = prev.clone();
                }
            }
        }
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
    AdminJson(body): AdminJson<TestS3Request>,
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
        session_token: None,
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
            let err_str = crate::config_db_sync::describe_sdk_error(&e);
            let kind = probe_error_kind(&e);
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

/// Classify a failed test-connection `ListBuckets` for the GUI:
/// "credentials", "connection", "timeout", or "unknown".
fn probe_error_kind<E>(e: &aws_sdk_s3::error::SdkError<E>) -> &'static str
where
    E: aws_sdk_s3::error::ProvideErrorMetadata + std::error::Error + 'static,
{
    use aws_sdk_s3::error::{ProvideErrorMetadata, SdkError};
    match e {
        SdkError::TimeoutError(_) => "timeout",
        SdkError::DispatchFailure(d) if d.is_timeout() => "timeout",
        SdkError::DispatchFailure(d) if d.is_io() => "connection",
        SdkError::ServiceError(svc) => {
            let status = svc.raw().status().as_u16();
            let credential_code = matches!(
                e.code(),
                Some(
                    "InvalidAccessKeyId"
                        | "SignatureDoesNotMatch"
                        | "AccessDenied"
                        | "ExpiredToken"
                        | "InvalidToken"
                )
            );
            if credential_code || status == 401 || status == 403 {
                "credentials"
            } else {
                "unknown"
            }
        }
        // Construction failures carry their cause (e.g. no credentials
        // provider) only in the source chain.
        _ => {
            let chain = aws_sdk_s3::error::DisplayErrorContext(e)
                .to_string()
                .to_ascii_lowercase();
            if chain.contains("credential") {
                "credentials"
            } else if chain.contains("dns") || chain.contains("connect") {
                "connection"
            } else {
                "unknown"
            }
        }
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
/// instance isn't part of a sync group). With sync configured: 200 when
/// the local copy is current afterwards, 409 when the synced copy was not
/// merged (refused as a rollback, newer schema, merge error: the body says
/// why), 502 when the bucket cannot be read.
pub async fn sync_now(
    State(state): State<Arc<AdminState>>,
) -> Result<(axum::http::StatusCode, Json<SyncNowResponse>), AdminError<Bare>> {
    let sync = state.config_sync.as_ref().ok_or_else(no_sync_bucket)?;

    // Same helper as the periodic poll: download, three-way merge, rebuild.
    let outcome = crate::config_db_sync::pull_and_merge(
        sync,
        &state.config_db,
        sync.db_key(),
        &state.iam_state,
        &state.external_auth,
        Some(&state.sessions),
        "sync-now endpoint",
    )
    .await;
    if let Err(e) = &outcome {
        tracing::warn!("sync-now failed: {e}");
    }
    let (code, body) = sync_now_reply(outcome, sync.status().pull_error);
    Ok((code, Json(body)))
}

/// Pure: the sync-now answer from the pull result and the pull error that
/// the sync recorded. A copy that was downloaded but not merged is a 409,
/// never a 200: the instances diverge until an operator acts.
fn sync_now_reply(
    outcome: Result<Option<bool>, String>,
    pull_error: Option<String>,
) -> (axum::http::StatusCode, SyncNowResponse) {
    use axum::http::StatusCode;
    match (outcome, pull_error) {
        (Err(e), _) => (
            StatusCode::BAD_GATEWAY,
            SyncNowResponse {
                downloaded: false,
                status: format!("Cannot read the sync bucket: {e}"),
            },
        ),
        (Ok(Some(true)), _) => (
            StatusCode::OK,
            SyncNowResponse {
                downloaded: true,
                status: "Downloaded newer config DB and reloaded IAM".to_string(),
            },
        ),
        (Ok(_), Some(e)) => (
            StatusCode::CONFLICT,
            SyncNowResponse {
                downloaded: false,
                status: format!(
                    "The synced config DB was not merged: {e}. GET \
                     /_/api/admin/config/sync shows the sync state"
                ),
            },
        ),
        (Ok(_), None) => (
            StatusCode::OK,
            SyncNowResponse {
                downloaded: false,
                status: "Local copy is current (ETag unchanged)".to_string(),
            },
        ),
    }
}

/// The bare 404 of the sync endpoints on an instance without a sync bucket.
fn no_sync_bucket() -> AdminError<Bare> {
    AdminError::not_found("no config sync bucket configured")
}

#[derive(Serialize)]
pub struct SyncStatusResponse {
    #[serde(flatten)]
    status: crate::config_db_sync::SyncStatus,
    /// The local DB's `sync_generation` (the last copy merged or uploaded).
    sync_generation: Option<i64>,
}

/// GET /api/admin/config/sync — the config-sync state of THIS instance:
/// last good pull and push, the last errors, a parked upload, the merge
/// base. 404 when no sync bucket is configured.
pub async fn sync_status(
    State(state): State<Arc<AdminState>>,
) -> Result<Json<SyncStatusResponse>, AdminError<Bare>> {
    let sync = state.config_sync.as_ref().ok_or_else(no_sync_bucket)?;
    let sync_generation = match &state.config_db {
        Some(db) => db.lock().await.sync_generation().ok(),
        None => None,
    };
    Ok(Json(SyncStatusResponse {
        status: sync.status(),
        sync_generation,
    }))
}

#[cfg(test)]
mod sync_now_reply_tests {
    use super::sync_now_reply;
    use axum::http::StatusCode;

    #[test]
    fn a_refused_or_failed_merge_is_a_conflict() {
        let (code, body) = sync_now_reply(Ok(Some(false)), Some("rollback".into()));
        assert_eq!(code, StatusCode::CONFLICT);
        assert!(!body.downloaded && body.status.contains("rollback"));
        // A refusal inside the download (newer schema) returns Ok(None).
        let (code, _) = sync_now_reply(Ok(None), Some("schema v30".into()));
        assert_eq!(code, StatusCode::CONFLICT);
    }

    #[test]
    fn current_or_merged_is_ok_and_a_read_error_is_bad_gateway() {
        assert_eq!(sync_now_reply(Ok(None), None).0, StatusCode::OK);
        let (code, body) = sync_now_reply(Ok(Some(true)), None);
        assert_eq!(code, StatusCode::OK);
        assert!(body.downloaded);
        assert_eq!(
            sync_now_reply(Err("boom".into()), None).0,
            StatusCode::BAD_GATEWAY
        );
    }
}

#[cfg(test)]
mod split_warnings_tests {
    use super::split_new_warnings;

    fn v(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn standing_warnings_are_existing_and_only_the_change_is_new() {
        // Issue #92: the rate-limit warning showed up on every unrelated apply.
        let before = v(&["rate-limit shares one bucket", "event URL is http"]);
        let after = v(&[
            "rate-limit shares one bucket",
            "bucket quota is 0",
            "event URL is http",
        ]);
        let (new, existing) = split_new_warnings(&before, after);
        assert_eq!(new, v(&["bucket quota is 0"]));
        assert_eq!(
            existing,
            v(&["rate-limit shares one bucket", "event URL is http"])
        );
    }

    #[test]
    fn duplicates_are_counted_and_fixed_warnings_vanish() {
        let (new, existing) = split_new_warnings(&v(&["a", "gone"]), v(&["a", "a"]));
        assert_eq!(new, v(&["a"]));
        assert_eq!(existing, v(&["a"]));
    }

    #[test]
    fn no_baseline_means_everything_is_new() {
        let (new, existing) = split_new_warnings(&[], v(&["x"]));
        assert_eq!(new, v(&["x"]));
        assert!(existing.is_empty());
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
    fn engine_affecting_includes_passthrough_size_and_codec_concurrency() {
        // These are engine-snapshotted at construction; a change must force a
        // rebuild or the new value is a silent no-op (X-ray M4).
        let base = crate::config::Config::default();

        let mut bigger = base.clone();
        bigger.max_passthrough_object_size = base.max_passthrough_object_size + 1;
        assert!(
            engine_affecting_fields_changed(&base, &bigger),
            "max_passthrough_object_size change must trigger an engine rebuild"
        );

        let mut codec = base.clone();
        codec.codec_concurrency = Some(base.codec_concurrency.unwrap_or(4) + 1);
        assert!(
            engine_affecting_fields_changed(&base, &codec),
            "codec_concurrency change must trigger an engine rebuild"
        );

        // Sanity: an unrelated field (log_level) must NOT trigger a rebuild.
        let mut logonly = base.clone();
        logonly.log_level = "debug".to_string();
        assert!(!engine_affecting_fields_changed(&base, &logonly));
    }

    #[test]
    fn restart_warnings_cover_tls_and_config_sync_bucket() {
        // Both are bound once at startup; a change must WARN restart-required or
        // the operator believes TLS/sync is on when it is not (X-ray M-adjacent).
        let base = crate::config::Config::default();

        let mut tls = base.clone();
        tls.tls = Some(crate::config::TlsConfig {
            enabled: true,
            cert_path: None,
            key_path: None,
        });
        assert!(
            requires_restart_warnings(&base, &tls)
                .iter()
                .any(|w| w.contains("tls")),
            "a TLS change must emit a restart-required warning"
        );

        let mut sync = base.clone();
        sync.config_sync_bucket = Some("dgp-sync".to_string());
        assert!(
            requires_restart_warnings(&base, &sync)
                .iter()
                .any(|w| w.contains("config_sync_bucket")),
            "a config_sync_bucket change must emit a restart-required warning"
        );

        // No change → no warnings.
        assert!(requires_restart_warnings(&base, &base).is_empty());
    }

    #[test]
    fn webhook_urls_index_restore_only_when_lengths_match() {
        // Same length → in-place restore by index (the safe round-trip case).
        let old = EventDeliveryConfig {
            webhook_urls: vec!["https://a/secret1".into(), "https://b/secret2".into()],
            ..EventDeliveryConfig::default()
        };
        let mut new = EventDeliveryConfig {
            webhook_urls: vec![REDACTED_SENTINEL.into(), "https://b/secret2".into()],
            ..EventDeliveryConfig::default()
        };
        preserve_event_delivery_secrets(&mut new, &old);
        assert_eq!(new.webhook_urls[0], "https://a/secret1");

        // Different length (operator deleted an entry) → NO index restore, or a
        // masked entry could align to the wrong old URL. The sentinel is left in
        // place (config validation then rejects it) rather than silently
        // restoring the wrong secret.
        let mut shortened = EventDeliveryConfig {
            webhook_urls: vec![REDACTED_SENTINEL.into()],
            ..EventDeliveryConfig::default()
        };
        preserve_event_delivery_secrets(&mut shortened, &old);
        assert_eq!(
            shortened.webhook_urls[0], REDACTED_SENTINEL,
            "a length change must NOT index-restore (would swap secrets)"
        );
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
    fn preserve_slack_bot_token() {
        // untouched sentinel → restore old token
        let old = EventDeliveryConfig {
            slack_bot_token: Some("xoxb-real-token".to_string()),
            ..Default::default()
        };
        let mut new = EventDeliveryConfig {
            slack_bot_token: Some(REDACTED_SENTINEL.to_string()),
            ..Default::default()
        };
        preserve_event_delivery_secrets(&mut new, &old);
        assert_eq!(new.slack_bot_token.as_deref(), Some("xoxb-real-token"));

        // retyped token → keep it
        let mut new2 = EventDeliveryConfig {
            slack_bot_token: Some("xoxb-NEW".to_string()),
            ..Default::default()
        };
        preserve_event_delivery_secrets(&mut new2, &old);
        assert_eq!(new2.slack_bot_token.as_deref(), Some("xoxb-NEW"));

        // sentinel with no old token → cleared
        let mut new3 = EventDeliveryConfig {
            slack_bot_token: Some(REDACTED_SENTINEL.to_string()),
            ..Default::default()
        };
        preserve_event_delivery_secrets(&mut new3, &EventDeliveryConfig::default());
        assert_eq!(new3.slack_bot_token, None);
    }

    #[test]
    fn redact_masks_slack_bot_token() {
        let cfg = crate::config::Config {
            event_delivery: EventDeliveryConfig {
                slack_bot_token: Some("xoxb-secret".to_string()),
                ..Default::default()
            },
            ..crate::config::Config::default()
        };
        let redacted = cfg.redact_all_secrets();
        assert_eq!(
            redacted.event_delivery.slack_bot_token.as_deref(),
            Some(REDACTED_SENTINEL)
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

    // ── M2: Slack incoming-webhook URL is a secret ────────────────────────

    #[test]
    fn redact_masks_slack_incoming_webhook_url() {
        use crate::config_sections::EventDeliveryFormat;
        let cfg = crate::config::Config {
            event_delivery: EventDeliveryConfig {
                enabled: true,
                format: EventDeliveryFormat::Slack,
                slack_bot_token: None, // incoming-webhook mode
                webhook_url: Some("https://hooks.slack.com/services/T/B/SECRET".to_string()),
                webhook_urls: vec!["https://hooks.slack.com/services/T/B/SECRET2".to_string()],
                ..Default::default()
            },
            ..crate::config::Config::default()
        };
        let r = cfg.redact_all_secrets();
        assert_eq!(
            r.event_delivery.webhook_url.as_deref(),
            Some(REDACTED_SENTINEL),
            "slack incoming-webhook URL must be masked"
        );
        assert_eq!(
            r.event_delivery.webhook_urls,
            vec![REDACTED_SENTINEL.to_string()]
        );
    }

    #[test]
    fn redact_keeps_raw_webhook_url_visible() {
        use crate::config_sections::EventDeliveryFormat;
        // Raw format: URL stays visible (creds live in headers, masked separately).
        let cfg = crate::config::Config {
            event_delivery: EventDeliveryConfig {
                enabled: true,
                format: EventDeliveryFormat::Raw,
                webhook_url: Some("https://example.com/hook".to_string()),
                ..Default::default()
            },
            ..crate::config::Config::default()
        };
        let r = cfg.redact_all_secrets();
        assert_eq!(
            r.event_delivery.webhook_url.as_deref(),
            Some("https://example.com/hook")
        );
    }

    #[test]
    fn preserve_restores_untouched_slack_webhook_url() {
        use crate::config_sections::EventDeliveryFormat;
        let old = EventDeliveryConfig {
            format: EventDeliveryFormat::Slack,
            webhook_url: Some("https://hooks.slack.com/services/REAL".to_string()),
            webhook_urls: vec!["https://hooks.slack.com/services/REAL2".to_string()],
            ..Default::default()
        };
        // Operator left both masked (untouched round-trip).
        let mut new = EventDeliveryConfig {
            format: EventDeliveryFormat::Slack,
            webhook_url: Some(REDACTED_SENTINEL.to_string()),
            webhook_urls: vec![REDACTED_SENTINEL.to_string()],
            ..Default::default()
        };
        preserve_event_delivery_secrets(&mut new, &old);
        assert_eq!(
            new.webhook_url.as_deref(),
            Some("https://hooks.slack.com/services/REAL")
        );
        assert_eq!(
            new.webhook_urls,
            vec!["https://hooks.slack.com/services/REAL2".to_string()]
        );

        // A retyped URL is kept, not restored.
        let mut new2 = EventDeliveryConfig {
            format: EventDeliveryFormat::Slack,
            webhook_url: Some("https://hooks.slack.com/services/NEW".to_string()),
            ..Default::default()
        };
        preserve_event_delivery_secrets(&mut new2, &old);
        assert_eq!(
            new2.webhook_url.as_deref(),
            Some("https://hooks.slack.com/services/NEW")
        );
    }
}

#[cfg(test)]
mod probe_error_tests {
    use super::*;
    use aws_sdk_s3::error::SdkError;
    use aws_sdk_s3::operation::list_buckets::ListBucketsError;
    use aws_smithy_runtime_api::http::{Response, StatusCode};
    use aws_smithy_types::body::SdkBody;

    fn service_error(status: u16, code: &str) -> SdkError<ListBucketsError> {
        let inner = ListBucketsError::generic(
            aws_smithy_types::error::ErrorMetadata::builder()
                .code(code)
                .build(),
        );
        let resp = Response::new(StatusCode::try_from(status).unwrap(), SdkBody::empty());
        SdkError::service_error(inner, resp)
    }

    /// `SdkError`'s Display is only "service error", so the kind must come
    /// from the status and code.
    #[test]
    fn wrong_credentials_classify_as_credentials() {
        assert_eq!(
            probe_error_kind(&service_error(403, "InvalidAccessKeyId")),
            "credentials"
        );
        assert_eq!(
            probe_error_kind(&service_error(403, "SignatureDoesNotMatch")),
            "credentials"
        );
        assert_eq!(
            probe_error_kind(&service_error(500, "InternalError")),
            "unknown"
        );
    }

    #[test]
    fn dispatch_failure_classifies_as_connection() {
        let err: SdkError<ListBucketsError> =
            SdkError::dispatch_failure(aws_smithy_runtime_api::client::result::ConnectorError::io(
                std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "Connection refused")
                    .into(),
            ));
        assert_eq!(probe_error_kind(&err), "connection");
    }
}

/// Class guard for issue #92 H1/H2/M1: every admin edit path, run against a
/// config whose every `DGP_*` input carries a distinct sentinel, must persist
/// a file that contains none of the sentinels.
#[cfg(test)]
pub(crate) mod env_leak_probe {
    use crate::config::{BackendConfig, BackendEncryptionConfig, Config};
    use std::cell::RefCell;
    use std::collections::{BTreeMap, BTreeSet};

    const FILE: &str = r#"
access:
  access_key_id: FILEAKID
  secret_access_key: file-secret-0001
storage:
  backend:
    type: filesystem
    path: /srv/file-data
  backend_encryption:
    mode: aes256-gcm-proxy
    key: "1111111111111111111111111111111111111111111111111111111111111111"
  backends:
    - name: eu-archive
      type: s3
      endpoint: http://file-endpoint:9000
      region: eu-file-1
      access_key_id: FILEBEAKID
      secret_access_key: file-be-secret
      encryption:
        mode: aes256-gcm-proxy
        key: "2222222222222222222222222222222222222222222222222222222222222222"
    - name: kms-one
      type: filesystem
      path: /srv/kms
      encryption:
        mode: sse-kms
        kms_key_id: arn:file
advanced:
  cache_size_mb: 100
"#;

    /// Variables whose value cannot be a sentinel (booleans).
    const BOOLEANS: &[&str] = &[
        "DGP_TLS_ENABLED",
        "DGP_S3_PATH_STYLE",
        "DGP_BACKEND_ALLOW_LOCAL",
    ];

    fn sentinel_for(name: &str, i: usize) -> String {
        match name {
            "DGP_TLS_ENABLED" | "DGP_S3_PATH_STYLE" => "true".into(),
            "DGP_BACKEND_ALLOW_LOCAL" => "false".into(),
            "DGP_LISTEN_ADDR" => "127.0.0.9:7701".into(),
            "DGP_MAX_DELTA_RATIO" => "0.3713".into(),
            n if n.ends_with("ENCRYPTION_KEY") => format!("{:064x}", 0xabcdef00_u64 + i as u64),
            n if n.ends_with("_MB")
                || n.ends_with("_SIZE")
                || n.ends_with("CONCURRENCY")
                || n.ends_with("THREADS") =>
            {
                format!("{}", 770_000 + i)
            }
            n => format!("SENTINEL-{n}-q7"),
        }
    }

    /// `(lookup map, sentinels to look for)`: every variable the override
    /// code reads for the probe config, each with a distinct value.
    pub fn sentinel_env() -> (BTreeMap<String, String>, Vec<(String, String)>) {
        let names = RefCell::new(BTreeSet::new());
        let record = |n: &str| {
            names.borrow_mut().insert(n.to_string());
            Some("1".to_string())
        };
        Config::from_yaml_str(FILE)
            .unwrap()
            .apply_env_overrides_with(&record);
        let env: BTreeMap<String, String> = names
            .into_inner()
            .into_iter()
            .enumerate()
            .map(|(i, n)| {
                let v = sentinel_for(&n, i);
                (n, v)
            })
            .collect();
        let check = env
            .iter()
            .filter(|(n, _)| !BOOLEANS.contains(&n.as_str()))
            // DGP_ADMIN_PASSWORD_HASH is shadowed by DGP_BOOTSTRAP_PASSWORD_HASH.
            .filter(|(n, _)| n.as_str() != "DGP_ADMIN_PASSWORD_HASH")
            .map(|(n, v)| (n.clone(), v.clone()))
            .collect();
        (env, check)
    }

    pub fn running(env: &BTreeMap<String, String>) -> Config {
        let mut cfg = Config::from_yaml_str(FILE).unwrap();
        cfg.apply_env_overrides_at_load(&|n: &str| env.get(n).cloned());
        cfg
    }

    /// Re-apply env to `edited`, persist it, and assert no sentinel reached
    /// the file. The persist runs WITHOUT the leak guard first (the pipeline
    /// itself must be clean), then with it (it must not refuse a clean file).
    pub fn assert_no_leak(
        label: &str,
        running: &Config,
        mut edited: Config,
        env: &BTreeMap<String, String>,
        check: &[(String, String)],
    ) {
        let lookup = |n: &str| env.get(n).cloned();
        edited.reapply_env_overrides(running, &lookup).unwrap();
        let file = edited
            .to_canonical_yaml_for_persist_with(&|_| None)
            .unwrap();
        for (name, value) in check {
            assert!(
                !file.contains(value.as_str()),
                "{label}: the value of {name} ({value}) reached the file:\n{file}"
            );
        }
        edited
            .to_canonical_yaml_for_persist_with(&lookup)
            .unwrap_or_else(|e| panic!("{label}: guard refused a clean file: {e}"));
        // And the env still wins at runtime.
        assert_eq!(
            edited.secret_access_key.as_deref(),
            env.get("DGP_SECRET_ACCESS_KEY").map(String::as_str),
            "{label}"
        );
    }

    /// The export shows the key id and hides the secret. An unedited
    /// round-trip (same key id, no secret) keeps the secret; a new key id
    /// without its secret stays asymmetric.
    #[test]
    fn a_visible_unchanged_key_id_keeps_its_secret() {
        let old_k = Some("AKBOOT".to_string());
        let old_s = Some("boot-secret".to_string());
        let (mut k, mut sec, mut w) = (old_k.clone(), None, Vec::new());
        super::preserve_sigv4_pair(&mut k, &mut sec, &old_k, &old_s, "proxy-level", &mut w);
        assert_eq!(sec, old_s);
        assert!(w.is_empty(), "{w:?}");
        let (mut k, mut sec, mut w) = (Some("AKNEW".to_string()), None, Vec::new());
        super::preserve_sigv4_pair(&mut k, &mut sec, &old_k, &old_s, "proxy-level", &mut w);
        assert_eq!(sec, None);
        assert_eq!(w.len(), 1, "{w:?}");
    }

    /// A backend key id shows in the redacted GET; an unedited round-trip
    /// of that GET (key id, no secret) keeps every backend secret.
    #[test]
    fn a_redacted_backend_round_trip_keeps_backend_secrets() {
        let mut run = Config {
            backend: BackendConfig::S3 {
                session_token: None,
                endpoint: Some("https://s3.example".into()),
                region: "eu-central-1".into(),
                force_path_style: true,
                access_key_id: Some("AKPRIMARY".into()),
                secret_access_key: Some("primary-secret".into()),
                allow_local: false,
            },
            ..Config::default()
        };
        run.backends.push(crate::config::NamedBackendConfig {
            name: "hetzner-fsn1".into(),
            backend: BackendConfig::S3 {
                session_token: None,
                endpoint: Some("https://fsn1.example".into()),
                region: "eu-central-1".into(),
                force_path_style: true,
                access_key_id: Some("AKNAMED".into()),
                secret_access_key: Some("named-secret".into()),
                allow_local: false,
            },
            encryption: BackendEncryptionConfig::default(),
        });
        let redacted = run.redact_all_secrets();
        let creds = |b: &BackendConfig| match b {
            BackendConfig::S3 {
                access_key_id,
                secret_access_key,
                ..
            } => (access_key_id.clone(), secret_access_key.clone()),
            _ => (None, None),
        };
        assert_eq!(creds(&redacted.backend), (Some("AKPRIMARY".into()), None));
        assert_eq!(
            creds(&redacted.backends[0].backend),
            (Some("AKNAMED".into()), None)
        );
        let mut e = crate::config_sections::SectionedConfig::from_flat(&redacted)
            .into_flat()
            .unwrap();
        let mut w = Vec::new();
        super::preserve_primary_backend_creds(&mut e, &run, &mut w);
        super::preserve_named_backends_creds(&mut e, &run, &mut w);
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(creds(&e.backend), creds(&run.backend));
        assert_eq!(
            creds(&e.backends[0].backend),
            creds(&run.backends[0].backend)
        );
    }

    #[test]
    fn bootstrap_removal_truth_table() {
        use super::BootstrapRemoval::*;
        let d = super::bootstrap_removal_decision;
        assert_eq!(d(false, false, false), NothingToRemove);
        assert_eq!(d(false, true, true), NothingToRemove);
        assert_eq!(d(true, true, true), EnvControlled);
        assert_eq!(d(true, false, false), WouldDisableAuth);
        assert_eq!(d(true, false, true), Remove);
    }

    fn preserve_all(new: &mut Config, old: &Config) {
        let mut w = Vec::new();
        super::preserve_sigv4_pair(
            &mut new.access_key_id,
            &mut new.secret_access_key,
            &old.access_key_id,
            &old.secret_access_key,
            "proxy-level",
            &mut w,
        );
        super::preserve_primary_backend_creds(new, old, &mut w);
        super::preserve_named_backends_creds(new, old, &mut w);
        let probe = super::section_level::BackendKeyPresence::default();
        super::section_level::preserve_backend_encryption_secrets(
            "default",
            &mut new.backend_encryption,
            &old.backend_encryption,
            probe,
        )
        .unwrap();
        for n in &mut new.backends {
            if let Some(o) = old.backends.iter().find(|o| o.name == n.name) {
                super::section_level::preserve_backend_encryption_secrets(
                    &n.name.clone(),
                    &mut n.encryption,
                    &o.encryption,
                    probe,
                )
                .unwrap();
            }
        }
    }

    fn set_region(cfg: &mut Config) {
        if let BackendConfig::S3 { region, .. } = &mut cfg.backend {
            *region = "eu-edit-1".into();
        }
    }

    #[test]
    fn no_edit_path_writes_an_env_value_into_the_file() {
        let (env, check) = sentinel_env();
        assert!(check.len() >= 20, "too few sentinels: {check:?}");
        let run = running(&env);

        // Section PUT whose body echoes the RUNNING values, one member edited.
        let mut e = crate::config_sections::SectionedConfig::from_flat(&run)
            .into_flat()
            .unwrap();
        set_region(&mut e);
        preserve_all(&mut e, &run);
        assert_no_leak(
            "section PUT (runtime echo, region edit)",
            &run,
            e,
            &env,
            &check,
        );

        // Section PUT from the GUI's redacted GET (the file view).
        let mut e = crate::config_sections::SectionedConfig::from_flat(&run.redact_all_secrets())
            .into_flat()
            .unwrap();
        e.cache_size_mb = 55;
        e.max_delta_ratio = 0.5;
        preserve_all(&mut e, &run);
        assert_no_leak("section PUT (redacted GET)", &run, e, &env, &check);

        // Field PATCH of one backend member.
        let mut e = run.clone();
        let req: super::ConfigUpdateRequest =
            serde_json::from_value(serde_json::json!({ "backend_region": "eu-edit-1" })).unwrap();
        let mut w = Vec::new();
        super::field_level::apply_backend_patch(&mut e.backend, &req, &mut w).unwrap();
        assert_no_leak("field PATCH region", &run, e, &env, &check);

        // Document apply of the export (redacted secrets refilled from the
        // running config).
        let yaml = run.to_canonical_yaml_with(&|_| None).unwrap();
        let mut e = Config::from_yaml_str(&yaml).unwrap();
        super::preserve_runtime_secrets(
            &mut e,
            &run,
            &super::document_level::document_probe(&yaml),
        )
        .unwrap();
        assert_no_leak("document apply", &run, e, &env, &check);

        // Encryption mode flip away from proxy AES (singleton + named).
        let mut e = run.clone();
        e.backend_encryption = BackendEncryptionConfig::default();
        for n in &mut e.backends {
            if n.name == "eu-archive" {
                n.encryption = BackendEncryptionConfig::default();
            }
        }
        preserve_all(&mut e, &run);
        assert_no_leak("encryption mode flip", &run, e, &env, &check);
    }

    #[test]
    fn a_promoted_env_key_is_written_as_a_reference() {
        let (env, _) = sentinel_env();
        let run = running(&env);
        let mut e = run.clone();
        e.backend_encryption = BackendEncryptionConfig::default();
        preserve_all(&mut e, &run);
        let report = e
            .reapply_env_overrides(&run, &|n: &str| env.get(n).cloned())
            .unwrap();
        assert_eq!(report.refs_added, vec!["DGP_ENCRYPTION_KEY".to_string()]);
        let file = e.to_canonical_yaml_for_persist_with(&|_| None).unwrap();
        assert!(
            file.contains("legacy_key: ${env:DGP_ENCRYPTION_KEY}"),
            "{file}"
        );
        // Reloading the file against the same environment gives the key back.
        let lookup = |n: &str| env.get(n).cloned();
        let text = crate::config::expand_env_with(&file, lookup).unwrap();
        let back = Config::from_yaml_str(&text).unwrap();
        assert_eq!(
            back.backend_encryption.legacy_key(),
            env.get("DGP_ENCRYPTION_KEY").map(String::as_str)
        );
    }

    #[test]
    fn persist_and_export_refuse_a_secret_env_value() {
        let (env, _) = sentinel_env();
        let lookup = |n: &str| env.get(n).cloned();
        let mut run = running(&env);
        // Simulate a future path that forgets the file view: a secret env
        // value copied into a field no env variable controls.
        run.event_delivery.slack_bot_token = env.get("DGP_SECRET_ACCESS_KEY").cloned();
        let err = run.to_canonical_yaml_for_persist_with(&lookup).unwrap_err();
        assert!(err.to_string().contains("DGP_SECRET_ACCESS_KEY"), "{err}");
        // A value the FILE itself holds is not a leak, even when an env
        // variable has the same value: here the named backend's secret
        // equals DGP_BE_AWS_SECRET_ACCESS_KEY (one MinIO, two definitions).
        let same = [
            ("DGP_S3_ENDPOINT", "http://env-minio:9000"),
            ("DGP_BE_AWS_SECRET_ACCESS_KEY", "file-be-secret"),
        ];
        let same = |n: &str| {
            same.iter()
                .find(|(k, _)| *k == n)
                .map(|(_, v)| v.to_string())
        };
        let mut cfg = Config::from_yaml_str(FILE).unwrap();
        cfg.apply_env_overrides_at_load(&same);
        cfg.to_canonical_yaml_for_persist_with(&same).unwrap();
        let mut edit = cfg.clone();
        edit.cache_size_mb = 7;
        edit.reapply_env_overrides(&cfg, &same).unwrap();
        edit.to_canonical_yaml_for_persist_with(&same).unwrap();
    }
}
