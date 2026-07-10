// SPDX-License-Identifier: GPL-3.0-only

//! Phase 3c.3 — Declarative-mode IAM reconciler.
//!
//! In `iam_mode: declarative` the YAML `access.*` fields are the
//! source of truth for users, groups, auth providers, and mapping
//! rules. On every `apply_config_transition` the reconciler:
//!
//!   1. Builds a [`DeclarativeIam`] snapshot from the new YAML.
//!   2. Builds a [`CurrentIam`] snapshot from the encrypted config DB.
//!   3. Computes a pure [`IamDiff`] via [`diff_iam`] — validation
//!      errors surface here before any DB writes.
//!   4. Applies the diff inside a single SQLite transaction via
//!      `ConfigDb::apply_iam_reconcile` — atomic; partial failures roll back.
//!
//! External identities (runtime OAuth byproducts) are intentionally
//! NOT reconciled — they stay DB-only. They ARE cascade-deleted when
//! a YAML-authoritative delete removes the user or provider they
//! reference; that's expected behaviour.
//!
//! ## External (OAuth-provisioned) users and the DELETE decision
//!
//! A pre-existing DB user absent from the YAML `iam_users` is normally
//! DELETED — the YAML is the source of truth. There is ONE exception:
//! a user whose `auth_source == "external"` was auto-provisioned by the
//! OAuth flow and is never authored in YAML. Deleting such a user on
//! every reconcile would churn: the next login re-creates them with a
//! brand-new access key.
//!
//! So an external user absent from YAML is deleted-for-reconcile ONLY
//! when its state is fully RECONSTRUCTABLE from the OAuth flow — i.e.
//! BOTH (a) it has no direct `permissions`, AND (b) its group set is
//! EXACTLY the set a matching `group_mapping_rule` would auto-assign it
//! (the "external baseline"). Such a user can be transparently rebuilt
//! on the next login, so dropping it is safe and idempotent.
//!
//! If the external user carries direct permissions OR extra group
//! memberships beyond the mapping-rule grant, that state was added
//! MANUALLY (admin GUI / API) and is NOT reconstructable from the OAuth
//! flow — the reconciler PRESERVES it (skips the delete). `auth_source
//! == "local"` users keep the plain "delete if absent from YAML"
//! behaviour. The OAuth baseline is precomputed by the reconciler
//! orchestrator (it needs `external_identities` + mapping rules) and
//! carried into the pure `diff_iam` via `CurrentIam::external_baseline_groups`,
//! so the diff stays I/O-free.
//!
//! ## Naming: name, not id
//!
//! YAML references entities by NAME (users.groups = ["admins"],
//! mapping_rules.group = "readers"). IDs are ephemeral DB
//! autoincrement values and must not leak into declarative YAML.
//! The diff resolves names → IDs at apply time using the
//! post-create-and-update group/provider maps, so references in
//! the same YAML apply consistently even when the referent is a
//! newly-created group.
//!
//! ## Idempotency
//!
//! Running `reconcile_declarative_iam` twice on the same YAML
//! produces a no-op: the second [`diff_iam`] returns an empty
//! [`IamDiff`]. Tests pin this contract.

use crate::config_db::auth_providers::{AuthProviderConfig, GroupMappingRule};
use crate::config_db::ConfigDb;
use crate::iam::external_auth::mapping::{evaluate_mappings, filter_rules_for_email_verification};
use crate::iam::external_auth::types::ExternalIdentityInfo;
use crate::iam::{normalize_permissions, validate_permissions, Group, IamUser, Permission};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};

// ───── Wire types ──────────────────────────────────────────────────────

/// One entry in `access.iam_users`. References groups by NAME.
///
/// Secrets (`secret_access_key`) are deserialised literally; operators
/// must materialise secret values before apply. The canonical exporter
/// redacts them exactly like SigV4 creds.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DeclarativeUser {
    pub name: String,
    pub access_key_id: String,
    #[serde(default)]
    pub secret_access_key: String,
    #[serde(default = "crate::types::default_true")]
    pub enabled: bool,
    /// Group memberships by group NAME. Each must appear in
    /// `access.iam_groups`; references to unknown groups are
    /// rejected by validation (no DB writes happen).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<String>,
    /// Direct permissions on top of group-inherited ones. The
    /// authz evaluator unions both.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub permissions: Vec<Permission>,
}

/// One entry in `access.iam_groups`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DeclarativeGroup {
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub permissions: Vec<Permission>,
}

/// One entry in `access.auth_providers`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DeclarativeAuthProvider {
    pub name: String,
    pub provider_type: String,
    #[serde(default = "crate::types::default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub priority: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// Infra secret; stripped by redactors.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub issuer_url: Option<String>,
    #[serde(default = "default_scopes")]
    pub scopes: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra_config: Option<serde_json::Value>,
}

/// One entry in `access.group_mapping_rules`. Provider and group
/// referenced by NAME. `provider: None` (absent) means "applies to
/// all providers", matching the DB's `provider_id: NULL` semantic.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DeclarativeMappingRule {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default)]
    pub priority: i64,
    pub match_type: String,
    #[serde(default = "default_email")]
    pub match_field: String,
    pub match_value: String,
    pub group: String,
}

// Provider/mapping serde defaults are shared with the config-DB shapes
// (`config_db::auth_providers`) so the two representations can't drift.
use crate::config_db::auth_providers::{default_email, default_scopes};

/// The whole declarative IAM snapshot, projected out of `AccessSection`.
/// Built inside `apply_config_transition` right before the reconciler
/// runs; carried by value (it's small).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DeclarativeIam {
    pub users: Vec<DeclarativeUser>,
    pub groups: Vec<DeclarativeGroup>,
    pub auth_providers: Vec<DeclarativeAuthProvider>,
    pub mapping_rules: Vec<DeclarativeMappingRule>,
}

impl DeclarativeIam {
    pub fn is_empty(&self) -> bool {
        self.users.is_empty()
            && self.groups.is_empty()
            && self.auth_providers.is_empty()
            && self.mapping_rules.is_empty()
    }
}

/// Snapshot of the current DB IAM state at diff time. Built by the
/// reconciler orchestrator right before `diff_iam`. Keeps the diff
/// function a pure `fn(yaml, current) -> Result<IamDiff, String>`.
#[derive(Default)]
pub struct CurrentIam {
    pub users: Vec<IamUser>,
    pub groups: Vec<Group>,
    pub auth_providers: Vec<AuthProviderConfig>,
    pub mapping_rules: Vec<GroupMappingRule>,
    /// Precomputed OAuth baseline for `auth_source == "external"` users:
    /// `user_id -> the group IDs a real login would auto-assign that
    /// user`. Computed by replaying the EXACT same mapping logic the
    /// OAuth callback uses — provider-FILTERED [`evaluate_mappings`]
    /// against an [`ExternalIdentityInfo`] reconstructed from each
    /// stored identity's `raw_claims` — unioned across the user's
    /// identities. (It must NOT use the unfiltered preview path, which
    /// over-grants groups from rules scoped to providers the user never
    /// logged in through; an over-broad baseline can mark a manually
    /// granted membership "reconstructable" and silently delete it.)
    ///
    /// Built by the reconciler orchestrator (it requires DB access to
    /// `external_identities`); the pure [`diff_iam`] reads it to decide
    /// whether an absent external user is reconstructable-from-OAuth and
    /// therefore safe to delete. Missing entry (non-external user, or an
    /// external user with no identity) is treated as an empty baseline.
    pub external_baseline_groups: HashMap<i64, Vec<i64>>,
}

// ───── Diff output ─────────────────────────────────────────────────────

/// The concrete operations the reconciler will execute against the
/// DB. Pure-function output of [`diff_iam`]; consumed by
/// `ConfigDb::reconcile`.
///
/// Lists are kept in apply-order (groups before users-that-reference-
/// them; providers before mapping-rules). Delete lists carry
/// `(db_id, name)` so the reconciler can audit-log with names without
/// a second lookup.
#[derive(Debug, Default, PartialEq)]
pub struct IamDiff {
    pub groups_to_create: Vec<DeclarativeGroup>,
    pub groups_to_update: Vec<(i64, DeclarativeGroup)>,
    pub groups_to_delete: Vec<(i64, String)>,

    pub providers_to_create: Vec<DeclarativeAuthProvider>,
    pub providers_to_update: Vec<(i64, DeclarativeAuthProvider)>,
    pub providers_to_delete: Vec<(i64, String)>,

    /// Users with their referenced group NAMES. The name→id
    /// resolution happens inside the reconcile transaction after
    /// groups have been committed.
    pub users_to_create: Vec<DeclarativeUser>,
    pub users_to_update: Vec<(i64, DeclarativeUser)>,
    pub users_to_delete: Vec<(i64, String)>,

    /// The subset of `users_to_delete` that are OAuth-provisioned (`external`)
    /// users being culled because they're fully reconstructable from the OAuth
    /// flow (login rebuilds them). These deletes are BENIGN + by-design, so the
    /// unattended-startup destructive-change guard must NOT count them — only
    /// `local` (authored-state) deletes are dangerous to apply on a restart.
    pub external_user_deletes: Vec<i64>,

    /// Mapping rules are wipe-and-rebuild (no stable per-row identity;
    /// any field change is a delete+insert). The enum captures the
    /// three non-overlapping states so the reconcile can't conflate
    /// "idempotent no-op" with "YAML is empty, wipe the DB" — the
    /// previous `Vec<…> + ambiguous helper` shape did exactly that
    /// (correctness x-ray C1: any re-apply of a non-empty rule set
    /// silently dropped the table).
    pub mapping_rules: MappingRulesAction,
}

/// What the reconciler should do with `group_mapping_rules`. Built
/// by [`diff_iam`] from the equality compare (`mapping_rules_equal`)
/// between YAML and DB. One of three states:
///
/// * `Keep` — YAML matches DB exactly, no-op. Never touches the table.
/// * `ClearAll` — YAML has zero rules, DB has some. Delete-all, no
///   re-insert. (Setting `yaml: []` in declarative mode means "remove
///   all mapping rules.")
/// * `ReplaceWith(rules)` — YAML has rules different from DB. Wipe
///   and re-insert from `rules`.
#[derive(Debug, PartialEq, Default)]
pub enum MappingRulesAction {
    #[default]
    Keep,
    ClearAll,
    ReplaceWith(Vec<DeclarativeMappingRule>),
}

impl MappingRulesAction {
    /// True when the action performs NO table writes (state equal).
    pub fn is_noop(&self) -> bool {
        matches!(self, Self::Keep)
    }
}

impl IamDiff {
    pub fn is_empty(&self) -> bool {
        self.groups_to_create.is_empty()
            && self.groups_to_update.is_empty()
            && self.groups_to_delete.is_empty()
            && self.providers_to_create.is_empty()
            && self.providers_to_update.is_empty()
            && self.providers_to_delete.is_empty()
            && self.users_to_create.is_empty()
            && self.users_to_update.is_empty()
            && self.users_to_delete.is_empty()
            && self.mapping_rules.is_noop()
    }

    /// Count of `local` (authored-state) user deletes — `users_to_delete` minus
    /// the reconstructable-external culls in `external_user_deletes`. The
    /// unattended-startup guard refuses on THIS count, not the raw total: an
    /// OAuth-provisioned external user (never in YAML) being culled is benign
    /// and by-design (login rebuilds it), so it must not crash-loop a restart.
    pub fn local_user_delete_count(&self) -> usize {
        self.users_to_delete
            .iter()
            .filter(|(id, _)| !self.external_user_deletes.contains(id))
            .count()
    }

    /// Human-readable one-liner summarising what this diff would do
    /// if applied. Format mirrors `ReconcileStats::summary_line()` so
    /// the validate-dry-run preview and the live apply-response
    /// summary render in the same shape. Called by the section
    /// `/validate` endpoint to preview a declarative-IAM apply
    /// without touching the DB.
    ///
    /// Callers can check [`Self::is_empty`] first to skip rendering
    /// the line entirely on no-op diffs.
    pub fn summary_line(&self) -> String {
        let mapping_rules = match &self.mapping_rules {
            MappingRulesAction::Keep => "keep".to_string(),
            MappingRulesAction::ClearAll => "clear".to_string(),
            MappingRulesAction::ReplaceWith(rules) => format!("replace({})", rules.len()),
        };
        format!(
            "users(+{}/~{}/-{}) groups(+{}/~{}/-{}) providers(+{}/~{}/-{}) mapping_rules={}",
            self.users_to_create.len(),
            self.users_to_update.len(),
            self.users_to_delete.len(),
            self.groups_to_create.len(),
            self.groups_to_update.len(),
            self.groups_to_delete.len(),
            self.providers_to_create.len(),
            self.providers_to_update.len(),
            self.providers_to_delete.len(),
            mapping_rules,
        )
    }
}

/// Project the current DB IAM state into a [`DeclarativeIam`]
/// ready to drop into `access.*` fields of a YAML config. Secrets
/// are redacted — operator materialises them from env vars at
/// deploy time.
///
/// The resulting snapshot is exactly what `diff_iam(&snapshot, &db)`
/// would see as "already-matching" — i.e., pasting this output into
/// a declarative-mode YAML and applying is a strict no-op (per
/// `ReconcileStats::is_noop()`). This is the roundtripability
/// contract the "Export from DB" button on the Access panel
/// depends on.
///
/// Usage (from an admin endpoint):
/// ```ignore
/// let db = config_db.lock().await;
/// let snapshot = export_as_declarative(&db)?;
/// let yaml = serde_yaml::to_string(&snapshot)?;
/// ```
pub fn export_as_declarative(db: &ConfigDb) -> Result<DeclarativeIam, String> {
    export_as_declarative_inner(db, false)
}

/// Like [`export_as_declarative`] but optionally emits the REAL secrets
/// (`secret_access_key` / `client_secret`) instead of redacting them.
///
/// `include_secrets = true` is used by the dedicated "Export full IAM (YAML)"
/// affordance, which produces a lossless round-trippable file: re-importing it
/// restores every credential verbatim. The trade-off is that the resulting YAML
/// contains LIVE credentials, so the export endpoint guards it behind an admin
/// session and the UI warns the operator to treat the file as a secret.
///
/// `include_secrets = false` is the redaction policy the canonical YAML exporter
/// uses (secrets blanked; operator wires them from env at deploy time).
pub fn export_as_declarative_inner(
    db: &ConfigDb,
    include_secrets: bool,
) -> Result<DeclarativeIam, String> {
    let db_users = db.load_users().map_err(|e| format!("load users: {e}"))?;
    let db_groups = db.load_groups().map_err(|e| format!("load groups: {e}"))?;
    let db_providers = db
        .load_auth_providers()
        .map_err(|e| format!("load auth_providers: {e}"))?;
    let db_rules = db
        .load_group_mapping_rules()
        .map_err(|e| format!("load mapping_rules: {e}"))?;

    // Build group_id→name + provider_id→name lookups for rule export.
    let group_id_to_name: HashMap<i64, String> =
        db_groups.iter().map(|g| (g.id, g.name.clone())).collect();
    let provider_id_to_name: HashMap<i64, String> = db_providers
        .iter()
        .map(|p| (p.id, p.name.clone()))
        .collect();

    let users = db_users
        .iter()
        .map(|u| {
            // Resolve group memberships from id → name. Skip unknown
            // ids (shouldn't happen in a consistent DB but defensive).
            let groups = u
                .group_ids
                .iter()
                .filter_map(|gid| group_id_to_name.get(gid).cloned())
                .collect();
            DeclarativeUser {
                name: u.name.clone(),
                access_key_id: u.access_key_id.clone(),
                // Redacted by default (operator wires via env). The full-IAM
                // export passes include_secrets=true for a lossless round trip.
                secret_access_key: if include_secrets {
                    u.secret_access_key.clone()
                } else {
                    String::new()
                },
                enabled: u.enabled,
                groups,
                permissions: u.permissions.clone(),
            }
        })
        .collect();

    let groups = db_groups
        .iter()
        .map(|g| DeclarativeGroup {
            name: g.name.clone(),
            description: g.description.clone(),
            permissions: g.permissions.clone(),
        })
        .collect();

    let auth_providers = db_providers
        .iter()
        .map(|p| DeclarativeAuthProvider {
            name: p.name.clone(),
            provider_type: p.provider_type.clone(),
            enabled: p.enabled,
            priority: p.priority,
            display_name: p.display_name.clone(),
            client_id: p.client_id.clone(),
            // Redacted by default; full-IAM export includes the real secret.
            client_secret: if include_secrets {
                p.client_secret.clone()
            } else {
                None
            },
            issuer_url: p.issuer_url.clone(),
            scopes: p.scopes.clone(),
            extra_config: p.extra_config.clone(),
        })
        .collect();

    let mapping_rules = db_rules
        .iter()
        .map(|r| DeclarativeMappingRule {
            // Resolve provider_id → name (None when the rule applies
            // to all providers, i.e. DB provider_id IS NULL).
            provider: r
                .provider_id
                .and_then(|pid| provider_id_to_name.get(&pid).cloned()),
            priority: r.priority,
            match_type: r.match_type.clone(),
            match_field: r.match_field.clone(),
            match_value: r.match_value.clone(),
            group: group_id_to_name
                .get(&r.group_id)
                .cloned()
                .unwrap_or_else(|| "<unknown-group>".to_string()),
        })
        .collect();

    Ok(DeclarativeIam {
        users,
        groups,
        auth_providers,
        mapping_rules,
    })
}

/// Pure preview: would-be reconcile diff against the current DB
/// without applying anything. Used by the section `/validate`
/// dry-run path so the Apply dialog can show the operator what the
/// reconciler will do BEFORE they commit.
///
/// Returns `Ok(IamDiff)` on success (diff may or may not be empty;
/// call `diff.is_empty()` to check) or `Err(String)` on validation
/// failure. The error is the same shape the live apply would emit.
///
/// This is the observable-contract side of the validate/apply pair:
/// both surfaces funnel through `diff_iam`, so the dry-run can't
/// lie about what the live apply will actually do.
pub fn preview_declarative_iam(db: &ConfigDb, yaml: &DeclarativeIam) -> Result<IamDiff, String> {
    let current = load_current_iam(db)?;
    diff_iam(yaml, &current)
}

/// Load the full `CurrentIam` snapshot from the DB, including the
/// precomputed OAuth baseline for external users. Shared by the preview
/// (`preview_declarative_iam`) and live-apply (`reconcile_declarative_iam`)
/// paths so the two can never disagree on what the diff sees.
fn load_current_iam(db: &ConfigDb) -> Result<CurrentIam, String> {
    let mapping_rules = db
        .load_group_mapping_rules()
        .map_err(|e| format!("load mapping_rules: {e}"))?;
    let external_baseline_groups = compute_external_baseline_groups(db, &mapping_rules)?;
    Ok(CurrentIam {
        users: db.load_users().map_err(|e| format!("load users: {e}"))?,
        groups: db.load_groups().map_err(|e| format!("load groups: {e}"))?,
        auth_providers: db
            .load_auth_providers()
            .map_err(|e| format!("load auth_providers: {e}"))?,
        mapping_rules,
        external_baseline_groups,
    })
}

/// Compute `user_id -> auto-assigned group IDs` for every external
/// identity, replaying the EXACT mapping logic a real OAuth login uses:
/// provider-FILTERED [`evaluate_mappings`] against an
/// [`ExternalIdentityInfo`] reconstructed from the identity's stored
/// `raw_claims` (faithfully mirroring the callback path in
/// `api::admin::external_auth`). A user with multiple identities gets
/// the UNION of the per-identity group sets, deduped. The result is the
/// OAuth "baseline" the delete decision compares against in [`diff_iam`].
///
/// Using the provider-scoped path (not the unfiltered `preview_email_*`
/// helper) is load-bearing: an unfiltered preview would include groups
/// granted only by rules scoped to OTHER providers, over-broadening the
/// baseline so a manually-added membership could match it and the user
/// would be deleted — losing the manual grant on next login.
fn compute_external_baseline_groups(
    db: &ConfigDb,
    mapping_rules: &[GroupMappingRule],
) -> Result<HashMap<i64, Vec<i64>>, String> {
    let identities = db
        .list_external_identities()
        .map_err(|e| format!("load external_identities: {e}"))?;
    // Only ENABLED providers can authenticate a login (ExternalAuthManager::
    // rebuild skips disabled ones, src/iam/external_auth/mod.rs). An identity
    // whose provider is disabled (or deleted) can therefore never be rebuilt by
    // a real login, so its mapping-rule groups must NOT enter the baseline —
    // otherwise an unreconstructable external user looks "reconstructable" and
    // gets deleted, permanently losing state login can't restore. Skipping them
    // keeps the baseline a strict under-approximation of what login grants
    // (fails safe toward PRESERVE).
    let enabled_provider_ids: std::collections::HashSet<i64> = db
        .load_auth_providers()
        .map_err(|e| format!("load auth_providers: {e}"))?
        .into_iter()
        .filter(|p| p.enabled)
        .map(|p| p.id)
        .collect();
    let mut baseline: HashMap<i64, Vec<i64>> = HashMap::new();
    for ident in &identities {
        if !enabled_provider_ids.contains(&ident.provider_id) {
            continue;
        }
        // Reconstruct the identity info exactly as the OAuth callback +
        // the `migrate`/recompute path do (src/api/admin/external_auth.rs):
        // raw_claims when present, else an empty object; email/name from
        // the stored columns; groups left empty (claim-derived groups
        // live inside raw_claims and are read by claim-based rules).
        let identity_info = ExternalIdentityInfo {
            subject: ident.external_sub.clone(),
            email: ident.email.clone(),
            email_verified: ident.email_verified,
            name: ident.display_name.clone(),
            groups: vec![],
            raw_claims: ident
                .raw_claims
                .clone()
                .unwrap_or_else(|| serde_json::json!({})),
        };
        // SECURITY: gate email-based rules on the stored email_verified, same
        // as the OAuth callback — never grant email-based groups to an
        // unverified identity on a declarative recompute.
        let gated_rules = filter_rules_for_email_verification(mapping_rules, &identity_info);
        let groups = evaluate_mappings(&gated_rules, &identity_info, ident.provider_id);
        let entry = baseline.entry(ident.user_id).or_default();
        for gid in groups {
            if !entry.contains(&gid) {
                entry.push(gid);
            }
        }
    }
    Ok(baseline)
}

// ───── Validation ──────────────────────────────────────────────────────

/// Run every validator; on the first error, return with no writes
/// planned. Called unconditionally at the top of [`diff_iam`] so
/// "validation before side-effects" holds as a pure predicate.
fn validate(yaml: &DeclarativeIam, db: &CurrentIam) -> Result<(), String> {
    // Uniqueness within YAML
    require_unique_names(yaml.users.iter().map(|u| &u.name), "iam_users")?;
    require_unique_names(yaml.groups.iter().map(|g| &g.name), "iam_groups")?;
    require_unique_names(
        yaml.auth_providers.iter().map(|p| &p.name),
        "auth_providers",
    )?;
    require_unique_names(
        yaml.users.iter().map(|u| &u.access_key_id),
        "iam_users.access_key_id",
    )?;

    // Reserved-name blocks ($-prefixed are reserved for synthetic
    // principals like $anonymous and $bootstrap).
    for u in &yaml.users {
        if u.name.starts_with('$') {
            return Err(format!(
                "user '{}': names starting with `$` are reserved",
                u.name
            ));
        }
    }

    // Every user.groups[*] must resolve in yaml.groups.
    let yaml_group_names: HashSet<&str> = yaml.groups.iter().map(|g| g.name.as_str()).collect();
    for u in &yaml.users {
        for gn in &u.groups {
            if !yaml_group_names.contains(gn.as_str()) {
                return Err(format!(
                    "user '{}': references unknown group '{}'",
                    u.name, gn
                ));
            }
        }
    }

    // Every mapping_rules.group + mapping_rules.provider must resolve.
    let yaml_provider_names: HashSet<&str> = yaml
        .auth_providers
        .iter()
        .map(|p| p.name.as_str())
        .collect();
    for r in &yaml.mapping_rules {
        if !yaml_group_names.contains(r.group.as_str()) {
            return Err(format!(
                "mapping rule with match_value='{}': group '{}' not defined in iam_groups",
                r.match_value, r.group
            ));
        }
        if let Some(p) = &r.provider {
            if !yaml_provider_names.contains(p.as_str()) {
                return Err(format!(
                    "mapping rule with match_value='{}': provider '{}' not defined in auth_providers",
                    r.match_value, p
                ));
            }
        }
    }

    // Permissions shape validation — per-entity so the error message
    // says which entity was bad.
    for u in &yaml.users {
        let mut perms = u.permissions.clone();
        normalize_permissions(&mut perms);
        validate_permissions(&perms).map_err(|e| format!("user '{}': {e}", u.name))?;
    }
    for g in &yaml.groups {
        let mut perms = g.permissions.clone();
        normalize_permissions(&mut perms);
        validate_permissions(&perms).map_err(|e| format!("group '{}': {e}", g.name))?;
    }

    // Empty-secret-on-CREATE guard: a redacted export blanks secrets, and for an
    // EXISTING user/provider the reconciler preserves the DB value
    // (desired_existing_*). But a user/provider being CREATED (name not in the
    // DB) with an empty secret has no value to preserve — it would onboard a user
    // who can never authenticate / an OIDC provider that can't complete the flow.
    // Reject at validation time so the operator supplies the real secret.
    let db_user_names: HashSet<&str> = db.users.iter().map(|u| u.name.as_str()).collect();
    for u in &yaml.users {
        if !db_user_names.contains(u.name.as_str()) && u.secret_access_key.is_empty() {
            return Err(format!(
                "user '{}' is new but has an empty secret_access_key — a redacted export cannot \
                 onboard a fresh user (they could never authenticate). Supply the real secret.",
                u.name
            ));
        }
    }
    let db_provider_names: HashSet<&str> =
        db.auth_providers.iter().map(|p| p.name.as_str()).collect();
    for p in &yaml.auth_providers {
        let secret_missing = p.client_secret.as_deref().unwrap_or("").is_empty();
        if !db_provider_names.contains(p.name.as_str()) && secret_missing {
            return Err(format!(
                "auth_provider '{}' is new but has an empty client_secret — a redacted export \
                 cannot onboard a fresh OIDC provider. Supply the real client_secret.",
                p.name
            ));
        }
    }

    // Cross-check: YAML user access_key_id must not collide with
    // ANY existing DB user (except the DB user with the same name,
    // which is the update-in-place case). Catches two scenarios:
    //
    //   1. YAML user "new-alice" has an access_key that matches a
    //      different DB user "old-alice" who's slated for DELETE
    //      (diff-by-name pairs them by name → "old-alice" delete +
    //      "new-alice" create — mid-transaction UNIQUE violation).
    //
    //   2. M1: YAML swaps access_keys between two surviving DB
    //      users (both UPDATE in place). First UPDATE would
    //      violate UNIQUE(access_key_id) because the second user
    //      still holds the target key.
    //
    // Doing this post-validation lets operators get a clean error
    // with both conflicting names rather than "apply reconcile:
    // SQLite error: UNIQUE constraint failed: users.access_key_id".
    for yu in &yaml.users {
        for db_user in &db.users {
            if yu.name == db_user.name {
                // Same user by name → UPDATE in place. The access_key
                // can change freely (the old value is gone the moment
                // the UPDATE hits). No collision possible against THIS
                // db_user; loop to check against OTHER db_users.
                continue;
            }
            if yu.access_key_id == db_user.access_key_id {
                return Err(format!(
                    "user '{}' in YAML collides on access_key_id with existing DB user '{}'. \
                     SQLite UNIQUE(access_key_id) would reject the reconcile mid-transaction; \
                     fix by renaming one of the users or assigning distinct access keys.",
                    yu.name, db_user.name
                ));
            }
        }
    }

    Ok(())
}

/// Helper: ensure every item in the iterator is unique, else error
/// naming the field and the duplicate.
fn require_unique_names<'a, I, S>(iter: I, field: &str) -> Result<(), String>
where
    I: IntoIterator<Item = &'a S>,
    S: AsRef<str> + 'a,
{
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for item in iter {
        let s = item.as_ref();
        if !seen.insert(s) {
            return Err(format!("duplicate entry in {field}: '{s}'"));
        }
    }
    Ok(())
}

// ───── Pure diff ───────────────────────────────────────────────────────

/// Compute the reconciliation diff. Pure; no I/O; returns an error
/// on YAML validation failure. The error string is suitable for
/// surfacing in the apply response verbatim.
pub fn diff_iam(yaml: &DeclarativeIam, db: &CurrentIam) -> Result<IamDiff, String> {
    validate(yaml, db)?;

    let mut diff = IamDiff::default();

    // ── Groups ──
    let yaml_groups: HashMap<&str, &DeclarativeGroup> =
        yaml.groups.iter().map(|g| (g.name.as_str(), g)).collect();
    for yg in &yaml.groups {
        match db.groups.iter().find(|dg| dg.name == yg.name) {
            Some(dg) if group_equal(dg, yg) => {}
            Some(dg) => diff.groups_to_update.push((dg.id, yg.clone())),
            None => diff.groups_to_create.push(yg.clone()),
        }
    }
    for dg in &db.groups {
        if !yaml_groups.contains_key(dg.name.as_str()) {
            diff.groups_to_delete.push((dg.id, dg.name.clone()));
        }
    }

    // ── Providers ──
    let yaml_providers: HashMap<&str, &DeclarativeAuthProvider> = yaml
        .auth_providers
        .iter()
        .map(|p| (p.name.as_str(), p))
        .collect();
    for yp in &yaml.auth_providers {
        match db.auth_providers.iter().find(|dp| dp.name == yp.name) {
            Some(dp) => {
                // A redacted export blanks client_secret to None. Preserve the
                // DB secret so an export→apply round-trip doesn't wipe it
                // (which would break the OIDC provider). Same class as the
                // user secret_access_key preservation. (X-ray H9.)
                let desired = desired_existing_provider(dp, yp);
                if !provider_equal(dp, &desired) {
                    diff.providers_to_update.push((dp.id, desired));
                }
            }
            None => diff.providers_to_create.push(yp.clone()),
        }
    }
    for dp in &db.auth_providers {
        if !yaml_providers.contains_key(dp.name.as_str()) {
            diff.providers_to_delete.push((dp.id, dp.name.clone()));
        }
    }

    // ── Users ──
    //
    // Equality compares access_key_id, enabled, permissions (by
    // content), and group-membership (resolving DB group IDs to
    // names via the db.groups snapshot). If any mismatch → update.
    let db_group_id_to_name: HashMap<i64, &str> =
        db.groups.iter().map(|g| (g.id, g.name.as_str())).collect();
    let yaml_users: HashMap<&str, &DeclarativeUser> =
        yaml.users.iter().map(|u| (u.name.as_str(), u)).collect();
    for yu in &yaml.users {
        match db.users.iter().find(|du| du.name == yu.name) {
            Some(du) => {
                let desired = desired_existing_user(du, yu);
                if !user_equal(du, &desired, &db_group_id_to_name) {
                    diff.users_to_update.push((du.id, desired));
                }
            }
            None => diff.users_to_create.push(yu.clone()),
        }
    }
    for du in &db.users {
        if yaml_users.contains_key(du.name.as_str()) {
            continue;
        }
        // External (OAuth-provisioned) users are never authored in YAML,
        // so "absent from YAML" alone must NOT delete them. Delete only
        // when the user is fully reconstructable from the OAuth flow
        // (no direct perms + groups == the mapping-rule baseline);
        // otherwise the state was added manually and must survive.
        // `local` users keep the plain delete-if-absent behaviour.
        if du.auth_source == "external" {
            let baseline = db
                .external_baseline_groups
                .get(&du.id)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            if !external_user_is_reconstructable(du, baseline) {
                continue; // preserve manually-granted state
            }
            // Reconstructable external cull: record it so the startup guard can
            // exclude it (login rebuilds these; deleting them is by-design).
            diff.external_user_deletes.push(du.id);
        }
        diff.users_to_delete.push((du.id, du.name.clone()));
    }

    // ── Mapping rules: wipe-and-rebuild, modelled as a tri-state ──
    //
    // Three non-overlapping cases:
    //   1. YAML matches DB exactly → Keep (no table writes).
    //   2. YAML empty, DB non-empty → ClearAll (delete, no re-insert).
    //   3. YAML differs from DB (including YAML non-empty, DB empty)
    //      → ReplaceWith (delete + re-insert from YAML).
    //
    // The enum form avoids the correctness-x-ray C1 bug where a
    // `Vec` + "fire clear if the vec is empty but DB has rules"
    // helper wiped mapping rules on every idempotent re-apply of a
    // non-empty rule set (helper couldn't distinguish "YAML matches
    // non-empty DB, stay put" from "YAML is empty, wipe DB").
    diff.mapping_rules = if mapping_rules_equal(&yaml.mapping_rules, &db.mapping_rules, db) {
        MappingRulesAction::Keep
    } else if yaml.mapping_rules.is_empty() {
        MappingRulesAction::ClearAll
    } else {
        MappingRulesAction::ReplaceWith(yaml.mapping_rules.clone())
    };

    Ok(diff)
}

// ───── Equality predicates ─────────────────────────────────────────────

fn desired_existing_user(db: &IamUser, yaml: &DeclarativeUser) -> DeclarativeUser {
    if !yaml.secret_access_key.is_empty() {
        return yaml.clone();
    }
    DeclarativeUser {
        secret_access_key: db.secret_access_key.clone(),
        ..yaml.clone()
    }
}

/// When the YAML provider's `client_secret` is absent (a redacted export),
/// preserve the DB's existing secret — else an export→apply round-trip wipes
/// it and breaks the OIDC provider. A non-empty incoming secret is a genuine
/// rotation and passes through. (X-ray H9, mirror of `desired_existing_user`.)
fn desired_existing_provider(
    db: &AuthProviderConfig,
    yaml: &DeclarativeAuthProvider,
) -> DeclarativeAuthProvider {
    if yaml
        .client_secret
        .as_deref()
        .map(|s| !s.is_empty())
        .unwrap_or(false)
    {
        return yaml.clone();
    }
    DeclarativeAuthProvider {
        client_secret: db.client_secret.clone(),
        ..yaml.clone()
    }
}

fn group_equal(db: &Group, yaml: &DeclarativeGroup) -> bool {
    db.name == yaml.name
        && db.description == yaml.description
        && permissions_equal(&db.permissions, &yaml.permissions)
}

fn provider_equal(db: &AuthProviderConfig, yaml: &DeclarativeAuthProvider) -> bool {
    db.name == yaml.name
        && db.provider_type == yaml.provider_type
        && db.enabled == yaml.enabled
        && db.priority == yaml.priority
        && db.display_name == yaml.display_name
        && db.client_id == yaml.client_id
        && db.client_secret == yaml.client_secret
        && db.issuer_url == yaml.issuer_url
        && db.scopes == yaml.scopes
        && db.extra_config == yaml.extra_config
}

fn user_equal(
    db: &IamUser,
    yaml: &DeclarativeUser,
    db_group_id_to_name: &HashMap<i64, &str>,
) -> bool {
    // Access key + secret are both compared — a rotated secret in
    // YAML is a real change. Secret equality matters because the
    // redact-on-export convention means a round-tripped YAML has
    // `secret: ""`; the reconcile's "is this a change?" decision
    // must fire on that.
    if db.name != yaml.name
        || db.access_key_id != yaml.access_key_id
        || db.secret_access_key != yaml.secret_access_key
        || db.enabled != yaml.enabled
        || !permissions_equal(&db.permissions, &yaml.permissions)
    {
        return false;
    }
    // Resolve DB user's group_ids through the group table to names,
    // sort both sides, compare. Unknown IDs (shouldn't happen in a
    // consistent DB but tests can construct them) are filtered out
    // — treated as "not a group" rather than crashing.
    let mut db_group_names: Vec<&str> = db
        .group_ids
        .iter()
        .filter_map(|id| db_group_id_to_name.get(id).copied())
        .collect();
    let mut yaml_group_names: Vec<&str> = yaml.groups.iter().map(String::as_str).collect();
    db_group_names.sort_unstable();
    yaml_group_names.sort_unstable();
    db_group_names == yaml_group_names
}

/// Pure predicate: is this `auth_source == "external"` user's state
/// fully RECONSTRUCTABLE from the OAuth flow, and therefore safe to
/// delete during a declarative reconcile when it's absent from YAML?
///
/// True iff BOTH:
///   (a) the user has NO direct `permissions`, AND
///   (b) its actual group set equals `baseline_group_ids` — the set a
///       matching `group_mapping_rule` would auto-assign it on login.
///
/// Both sides are compared as SETS (order-independent, dedup-safe), so a
/// user whose groups are exactly the mapping-rule grant (in any order,
/// with or without accidental duplicates) is reconstructable. Any direct
/// permission, or any group beyond the baseline, means the state was
/// added manually and can't be rebuilt from OAuth → preserve (return
/// false). Caller only invokes this for external users; local users are
/// handled by the plain delete-if-absent path.
fn external_user_is_reconstructable(user: &IamUser, baseline_group_ids: &[i64]) -> bool {
    // A disabled external user is an admin soft-ban: re-provisioning on next
    // login would resurrect them as ENABLED, silently undoing the ban. The
    // `enabled` flag is not reconstructable from the OAuth flow → preserve.
    if !user.enabled {
        return false;
    }
    if !user.permissions.is_empty() {
        return false;
    }
    let actual: HashSet<i64> = user.group_ids.iter().copied().collect();
    let baseline: HashSet<i64> = baseline_group_ids.iter().copied().collect();
    actual == baseline
}

fn permissions_equal(a: &[Permission], b: &[Permission]) -> bool {
    // Compare by JSON canonicalisation — tolerates field-order
    // differences in `conditions` (serde_json::Value equality
    // handles that natively) and ignores the `id` field (ephemeral).
    //
    // H1 regression: normalize BOTH sides before comparing.
    // `replace_{user,group}_permissions` normalizes on insert (so the
    // DB holds canonical `"Allow"`/`"Deny"`), but YAML that uses the
    // AWS-IAM-style lowercase `"allow"` stays uncanonicalized on the
    // diff side. Without pre-normalization the diff would report
    // "changed" every apply → perpetual audit churn + non-idempotent
    // reconcile even when operator intent is stable.
    let norm = |ps: &[Permission]| -> Vec<serde_json::Value> {
        let mut canon = ps.to_vec();
        normalize_permissions(&mut canon);
        let mut out: Vec<serde_json::Value> = canon
            .iter()
            .map(|p| {
                // Strip `id` so DB-loaded permissions with autogen
                // ids compare equal to YAML-authored permissions
                // (which never carry an id).
                let mut v = serde_json::to_value(p).unwrap_or(serde_json::Value::Null);
                if let Some(obj) = v.as_object_mut() {
                    obj.remove("id");
                }
                v
            })
            .collect();
        // Normalise order (permissions are unordered semantically).
        out.sort_by_key(|v| v.to_string());
        out
    };
    norm(a) == norm(b)
}

fn mapping_rules_equal(
    yaml_rules: &[DeclarativeMappingRule],
    db_rules: &[GroupMappingRule],
    db: &CurrentIam,
) -> bool {
    if yaml_rules.len() != db_rules.len() {
        return false;
    }
    // Project both to comparable tuples (provider NAME, group NAME, fields).
    let db_group_id_to_name: HashMap<i64, &str> =
        db.groups.iter().map(|g| (g.id, g.name.as_str())).collect();
    let db_prov_id_to_name: HashMap<i64, &str> = db
        .auth_providers
        .iter()
        .map(|p| (p.id, p.name.as_str()))
        .collect();
    // Comparable tuple: (provider_name?, priority, match_type,
    // match_field, match_value, group_name). Alias keeps clippy's
    // type-complexity warning happy without losing the tuple shape's
    // documentary value.
    type RuleTuple<'a> = (Option<&'a str>, i64, &'a str, &'a str, &'a str, &'a str);
    let mut yaml_tuples: Vec<RuleTuple<'_>> = yaml_rules
        .iter()
        .map(|r| {
            (
                r.provider.as_deref(),
                r.priority,
                r.match_type.as_str(),
                r.match_field.as_str(),
                r.match_value.as_str(),
                r.group.as_str(),
            )
        })
        .collect();
    let mut db_tuples: Vec<RuleTuple<'_>> = db_rules
        .iter()
        .map(|r| {
            let provider_name = r
                .provider_id
                .and_then(|id| db_prov_id_to_name.get(&id).copied());
            let group_name = db_group_id_to_name
                .get(&r.group_id)
                .copied()
                .unwrap_or("<unknown>");
            (
                provider_name,
                r.priority,
                r.match_type.as_str(),
                r.match_field.as_str(),
                r.match_value.as_str(),
                group_name,
            )
        })
        .collect();
    yaml_tuples.sort();
    db_tuples.sort();
    yaml_tuples == db_tuples
}

// ───── Orchestrator ────────────────────────────────────────────────────

/// Stats returned by the reconciler. Consumed by the caller
/// (`apply_config_transition`) to emit audit entries and a structured
/// log line.
#[derive(Debug, Default, Clone)]
pub struct ReconcileStats {
    pub users_total: usize,
    pub groups_total: usize,
    pub providers_total: usize,

    pub users_created: Vec<String>,
    pub users_updated: Vec<String>,
    pub users_deleted: Vec<String>,
    pub groups_created: Vec<String>,
    pub groups_updated: Vec<String>,
    pub groups_deleted: Vec<String>,
    pub providers_created: Vec<String>,
    pub providers_updated: Vec<String>,
    pub providers_deleted: Vec<String>,
    pub mapping_rules_replaced: usize,
}

impl ReconcileStats {
    /// Per-entity audit entries the caller should emit. Each tuple is
    /// `(audit_action, &names)` — the caller walks the list and calls
    /// `audit_log(action, "declarative", name, …)`. Centralising the
    /// tag→name-list mapping here means `apply_config_transition`
    /// doesn't have a 10-block copy-paste and a future stat field
    /// (e.g. `external_identities_culled`) only needs a new entry
    /// here.
    pub fn audit_entries(&self) -> Vec<(&'static str, &Vec<String>)> {
        vec![
            ("iam_reconcile_group_create", &self.groups_created),
            ("iam_reconcile_group_update", &self.groups_updated),
            ("iam_reconcile_group_delete", &self.groups_deleted),
            ("iam_reconcile_provider_create", &self.providers_created),
            ("iam_reconcile_provider_update", &self.providers_updated),
            ("iam_reconcile_provider_delete", &self.providers_deleted),
            ("iam_reconcile_user_create", &self.users_created),
            ("iam_reconcile_user_update", &self.users_updated),
            ("iam_reconcile_user_delete", &self.users_deleted),
        ]
    }

    /// Total count of name-level changes. Excludes `mapping_rules_replaced`
    /// because mapping rules are a replace-semantic count, not a per-name list.
    pub fn total_named_changes(&self) -> usize {
        self.users_created.len()
            + self.users_updated.len()
            + self.users_deleted.len()
            + self.groups_created.len()
            + self.groups_updated.len()
            + self.groups_deleted.len()
            + self.providers_created.len()
            + self.providers_updated.len()
            + self.providers_deleted.len()
    }

    /// True when the reconcile made zero changes (pure idempotent apply).
    pub fn is_noop(&self) -> bool {
        self.total_named_changes() == 0 && self.mapping_rules_replaced == 0
    }

    /// Human-readable one-liner for the log / apply-response warning.
    /// Centralises the format so the tracing::info! and the warning
    /// string can't drift.
    pub fn summary_line(&self) -> String {
        format!(
            "users(+{}/~{}/-{}) groups(+{}/~{}/-{}) providers(+{}/~{}/-{}) mapping_rules={}",
            self.users_created.len(),
            self.users_updated.len(),
            self.users_deleted.len(),
            self.groups_created.len(),
            self.groups_updated.len(),
            self.groups_deleted.len(),
            self.providers_created.len(),
            self.providers_updated.len(),
            self.providers_deleted.len(),
            self.mapping_rules_replaced,
        )
    }
}

/// Build the `DeclarativeIam` snapshot from `AccessSection` fields.
/// Extracted so `apply_config_transition` can build it without
/// borrowing the full `Config` twice.
pub fn snapshot_from_access(
    users: &[DeclarativeUser],
    groups: &[DeclarativeGroup],
    auth_providers: &[DeclarativeAuthProvider],
    mapping_rules: &[DeclarativeMappingRule],
) -> DeclarativeIam {
    DeclarativeIam {
        users: users.to_vec(),
        groups: groups.to_vec(),
        auth_providers: auth_providers.to_vec(),
        mapping_rules: mapping_rules.to_vec(),
    }
}

/// Orchestrator: take a DB handle + YAML snapshot, compute the diff,
/// apply it atomically. Returns stats for logging/audit.
///
/// Returns `Err(String)` on validation failure (no DB writes) or
/// on DB error during apply (everything rolled back by the
/// transaction; no partial state observable).
pub fn reconcile_declarative_iam(
    db: &ConfigDb,
    yaml: &DeclarativeIam,
) -> Result<ReconcileStats, String> {
    let current = load_current_iam(db)?;
    let diff = diff_iam(yaml, &current)?;
    db.apply_iam_reconcile(&diff, &current)
        .map_err(|e| format!("apply reconcile: {e}"))
}

/// Validate a declarative-IAM YAML snapshot against the current DB WITHOUT
/// writing anything — runs the same load + diff (which validates unique names,
/// group refs, permissions, and access-key collisions) as
/// `reconcile_declarative_iam` but discards the diff. Used as a pre-commit gate
/// so a config-apply that will fail its IAM reconcile is rejected BEFORE any
/// runtime side effect (engine rebuild, snapshot publish) is committed (H8/H19).
pub fn validate_declarative_iam(db: &ConfigDb, yaml: &DeclarativeIam) -> Result<(), String> {
    let current = load_current_iam(db)?;
    diff_iam(yaml, &current).map(|_| ())
}

// ───── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn perm(actions: &[&str], resources: &[&str]) -> Permission {
        Permission {
            id: 0,
            effect: "Allow".into(),
            actions: actions.iter().map(|s| s.to_string()).collect(),
            resources: resources.iter().map(|s| s.to_string()).collect(),
            conditions: None,
        }
    }
    fn yu(name: &str, ak: &str) -> DeclarativeUser {
        DeclarativeUser {
            name: name.into(),
            access_key_id: ak.into(),
            secret_access_key: "sk".into(),
            enabled: true,
            groups: vec![],
            permissions: vec![],
        }
    }
    fn yg(name: &str) -> DeclarativeGroup {
        DeclarativeGroup {
            name: name.into(),
            description: String::new(),
            permissions: vec![],
        }
    }
    fn db_user(id: i64, name: &str, ak: &str) -> IamUser {
        IamUser {
            id,
            name: name.into(),
            access_key_id: ak.into(),
            secret_access_key: "sk".into(),
            enabled: true,
            created_at: String::new(),
            permissions: vec![],
            group_ids: vec![],
            auth_source: "local".into(),
            iam_policies: vec![],
        }
    }
    fn db_group(id: i64, name: &str) -> Group {
        Group {
            id,
            name: name.into(),
            description: String::new(),
            permissions: vec![],
            member_ids: vec![],
            created_at: String::new(),
        }
    }

    fn empty_db() -> CurrentIam {
        CurrentIam::default()
    }

    #[test]
    fn diff_empty_yaml_empty_db_is_empty() {
        let diff = diff_iam(&DeclarativeIam::default(), &empty_db()).unwrap();
        assert!(diff.is_empty());
    }

    #[test]
    fn diff_creates_new_users_and_groups() {
        let yaml = DeclarativeIam {
            users: vec![yu("alice", "AKIA1")],
            groups: vec![yg("admins")],
            ..Default::default()
        };
        let diff = diff_iam(&yaml, &empty_db()).unwrap();
        assert_eq!(diff.groups_to_create.len(), 1);
        assert_eq!(diff.groups_to_create[0].name, "admins");
        assert_eq!(diff.users_to_create.len(), 1);
        assert_eq!(diff.users_to_create[0].name, "alice");
    }

    #[test]
    fn diff_rejects_new_user_with_empty_secret() {
        // A redacted export applied to a FRESH instance: the new user has a
        // blanked secret. There's no DB row to preserve it from, so onboarding
        // them would create a user who can never authenticate. Must be rejected.
        let mut u = yu("alice", "AKIA1");
        u.secret_access_key = String::new();
        let yaml = DeclarativeIam {
            users: vec![u],
            ..Default::default()
        };
        let err = diff_iam(&yaml, &empty_db()).unwrap_err();
        assert!(
            err.contains("empty secret_access_key"),
            "must reject a new user with an empty secret, got: {err}"
        );

        // But an EXISTING user (name in DB) with a blank secret is fine — the
        // reconciler preserves the DB secret.
        let mut u2 = yu("alice", "AKIA1");
        u2.secret_access_key = String::new();
        let yaml2 = DeclarativeIam {
            users: vec![u2],
            ..Default::default()
        };
        let current = CurrentIam {
            users: vec![db_user(1, "alice", "AKIA1")],
            ..empty_db()
        };
        assert!(
            diff_iam(&yaml2, &current).is_ok(),
            "an existing user with a blanked secret must be allowed (preserved)"
        );
    }

    #[test]
    fn diff_updates_user_with_changed_access_key() {
        let yaml = DeclarativeIam {
            users: vec![yu("alice", "AKIA_NEW")],
            ..Default::default()
        };
        let current = CurrentIam {
            users: vec![db_user(42, "alice", "AKIA_OLD")],
            ..empty_db()
        };
        let diff = diff_iam(&yaml, &current).unwrap();
        assert!(
            diff.users_to_create.is_empty(),
            "must NOT create — name matched"
        );
        assert!(
            diff.users_to_delete.is_empty(),
            "must NOT delete — name matched"
        );
        assert_eq!(diff.users_to_update.len(), 1);
        assert_eq!(
            diff.users_to_update[0].0, 42,
            "db_id preserved across rotation"
        );
        assert_eq!(diff.users_to_update[0].1.access_key_id, "AKIA_NEW");
    }

    #[test]
    fn diff_deletes_db_user_missing_from_yaml() {
        let yaml = DeclarativeIam::default();
        let current = CurrentIam {
            users: vec![db_user(7, "bob", "AKIA_B")],
            ..empty_db()
        };
        let diff = diff_iam(&yaml, &current).unwrap();
        assert_eq!(diff.users_to_delete, vec![(7, "bob".to_string())]);
    }

    #[test]
    fn diff_is_idempotent_after_apply() {
        // Equivalent YAML vs DB: diff should be empty. Matches
        // "running reconcile twice is a no-op" invariant.
        let yaml = DeclarativeIam {
            users: vec![DeclarativeUser {
                name: "alice".into(),
                access_key_id: "AKIA1".into(),
                secret_access_key: "sk".into(),
                enabled: true,
                groups: vec!["admins".into()],
                permissions: vec![perm(&["*"], &["*"])],
            }],
            groups: vec![DeclarativeGroup {
                name: "admins".into(),
                description: String::new(),
                permissions: vec![],
            }],
            ..Default::default()
        };
        let current = CurrentIam {
            users: vec![IamUser {
                group_ids: vec![11],
                permissions: vec![perm(&["*"], &["*"])],
                ..db_user(1, "alice", "AKIA1")
            }],
            groups: vec![db_group(11, "admins")],
            ..empty_db()
        };
        let diff = diff_iam(&yaml, &current).unwrap();
        assert!(
            diff.is_empty(),
            "YAML equivalent to DB must produce empty diff, got {diff:?}"
        );
    }

    #[test]
    fn diff_redacted_existing_user_secret_is_idempotent() {
        let yaml = DeclarativeIam {
            users: vec![DeclarativeUser {
                secret_access_key: String::new(),
                ..yu("alice", "AKIA1")
            }],
            ..Default::default()
        };
        let current = CurrentIam {
            users: vec![IamUser {
                secret_access_key: "existing-secret".into(),
                ..db_user(1, "alice", "AKIA1")
            }],
            ..empty_db()
        };

        let diff = diff_iam(&yaml, &current).unwrap();

        assert!(
            diff.users_to_update.is_empty(),
            "redacted exported secret must not rotate existing user credentials: {diff:?}"
        );
    }

    /// X-ray H9: a redacted export→apply must NOT wipe an OIDC provider's
    /// client_secret. The YAML carries client_secret: None (redacted); the DB
    /// has the real secret — the diff must preserve it (no spurious update).
    #[test]
    fn diff_redacted_existing_provider_secret_is_idempotent() {
        let yaml = DeclarativeIam {
            auth_providers: vec![DeclarativeAuthProvider {
                name: "goog".into(),
                provider_type: "oidc".into(),
                enabled: true,
                priority: 0,
                display_name: None,
                client_id: Some("client-123".into()),
                client_secret: None, // redacted
                issuer_url: Some("https://accounts.google.com".into()),
                scopes: default_scopes(),
                extra_config: None,
            }],
            ..Default::default()
        };
        let current = CurrentIam {
            auth_providers: vec![AuthProviderConfig {
                id: 1,
                name: "goog".into(),
                provider_type: "oidc".into(),
                enabled: true,
                priority: 0,
                display_name: None,
                client_id: Some("client-123".into()),
                client_secret: Some("real-oidc-secret".into()),
                issuer_url: Some("https://accounts.google.com".into()),
                scopes: default_scopes(),
                extra_config: None,
                created_at: String::new(),
                updated_at: String::new(),
            }],
            ..empty_db()
        };

        let diff = diff_iam(&yaml, &current).unwrap();
        assert!(
            diff.providers_to_update.is_empty(),
            "redacted provider secret must not trigger an update that wipes it: {diff:?}"
        );
    }

    #[test]
    fn diff_rejects_user_referencing_unknown_group() {
        let yaml = DeclarativeIam {
            users: vec![DeclarativeUser {
                groups: vec!["ghosts".into()],
                ..yu("alice", "AKIA1")
            }],
            groups: vec![yg("admins")],
            ..Default::default()
        };
        let err = diff_iam(&yaml, &empty_db()).unwrap_err();
        assert!(
            err.contains("ghosts"),
            "error must name the missing group, got: {err}"
        );
    }

    #[test]
    fn diff_rejects_duplicate_user_names() {
        let yaml = DeclarativeIam {
            users: vec![yu("alice", "K1"), yu("alice", "K2")],
            ..Default::default()
        };
        let err = diff_iam(&yaml, &empty_db()).unwrap_err();
        assert!(err.contains("duplicate"));
        assert!(err.contains("iam_users"));
    }

    #[test]
    fn diff_rejects_duplicate_access_keys() {
        let yaml = DeclarativeIam {
            users: vec![yu("alice", "K1"), yu("bob", "K1")],
            ..Default::default()
        };
        let err = diff_iam(&yaml, &empty_db()).unwrap_err();
        assert!(err.contains("K1") || err.contains("access_key_id"));
    }

    #[test]
    fn diff_rejects_reserved_dollar_prefix_names() {
        let yaml = DeclarativeIam {
            users: vec![yu("$anonymous", "K1")],
            ..Default::default()
        };
        let err = diff_iam(&yaml, &empty_db()).unwrap_err();
        assert!(err.contains("reserved"));
    }

    #[test]
    fn diff_rejects_mapping_rule_with_unknown_provider() {
        let yaml = DeclarativeIam {
            groups: vec![yg("admins")],
            mapping_rules: vec![DeclarativeMappingRule {
                provider: Some("missing".into()),
                priority: 10,
                match_type: "email_domain".into(),
                match_field: "email".into(),
                match_value: "example.com".into(),
                group: "admins".into(),
            }],
            ..Default::default()
        };
        let err = diff_iam(&yaml, &empty_db()).unwrap_err();
        assert!(err.contains("missing"));
    }

    #[test]
    fn diff_rejects_mapping_rule_with_unknown_group() {
        let yaml = DeclarativeIam {
            auth_providers: vec![DeclarativeAuthProvider {
                name: "goog".into(),
                provider_type: "oidc".into(),
                enabled: true,
                priority: 0,
                display_name: None,
                client_id: None,
                client_secret: None,
                issuer_url: None,
                scopes: default_scopes(),
                extra_config: None,
            }],
            mapping_rules: vec![DeclarativeMappingRule {
                provider: Some("goog".into()),
                priority: 10,
                match_type: "email_domain".into(),
                match_field: "email".into(),
                match_value: "example.com".into(),
                group: "ghosts".into(),
            }],
            ..Default::default()
        };
        let err = diff_iam(&yaml, &empty_db()).unwrap_err();
        assert!(err.contains("ghosts"));
    }

    #[test]
    fn diff_rejects_access_key_collision_on_deleted_db_user() {
        // YAML has "new-alice" with access_key "KREUSED"; DB has a
        // different user "old-alice" with the same access_key that
        // YAML would delete. The diff-by-name would leave this to
        // SQL UNIQUE to blow up mid-transaction — we catch it at
        // validation instead.
        let yaml = DeclarativeIam {
            users: vec![yu("new-alice", "KREUSED")],
            ..Default::default()
        };
        let current = CurrentIam {
            users: vec![db_user(1, "old-alice", "KREUSED")],
            ..empty_db()
        };
        let err = diff_iam(&yaml, &current).unwrap_err();
        assert!(err.contains("access_key_id") || err.contains("conflict"));
    }

    #[test]
    fn diff_groups_update_detects_changed_permissions() {
        let yaml = DeclarativeIam {
            groups: vec![DeclarativeGroup {
                name: "admins".into(),
                description: String::new(),
                permissions: vec![perm(&["*"], &["*"])],
            }],
            ..Default::default()
        };
        let current = CurrentIam {
            groups: vec![Group {
                permissions: vec![perm(&["read"], &["*"])],
                ..db_group(11, "admins")
            }],
            ..empty_db()
        };
        let diff = diff_iam(&yaml, &current).unwrap();
        assert_eq!(diff.groups_to_update.len(), 1);
        assert_eq!(diff.groups_to_update[0].0, 11);
    }

    #[test]
    fn diff_user_membership_change_surfaces_as_update() {
        // YAML puts alice in groups=["admins"]; DB has alice with
        // group_ids=[22] (=readers). Must be a user-update.
        let yaml = DeclarativeIam {
            users: vec![DeclarativeUser {
                groups: vec!["admins".into()],
                ..yu("alice", "K1")
            }],
            groups: vec![yg("admins"), yg("readers")],
            ..Default::default()
        };
        let current = CurrentIam {
            users: vec![IamUser {
                group_ids: vec![22],
                ..db_user(1, "alice", "K1")
            }],
            groups: vec![db_group(11, "admins"), db_group(22, "readers")],
            ..empty_db()
        };
        let diff = diff_iam(&yaml, &current).unwrap();
        assert_eq!(diff.users_to_update.len(), 1);
        assert_eq!(diff.users_to_update[0].0, 1);
    }

    // ───── C1 regression: mapping_rules tri-state ─────────────────────

    fn db_rule(id: i64, provider_id: Option<i64>, group_id: i64, value: &str) -> GroupMappingRule {
        GroupMappingRule {
            id,
            provider_id,
            priority: 10,
            match_type: "email_domain".into(),
            match_field: "email".into(),
            match_value: value.into(),
            group_id,
            created_at: String::new(),
        }
    }

    fn yaml_rule(provider: Option<&str>, group: &str, value: &str) -> DeclarativeMappingRule {
        DeclarativeMappingRule {
            provider: provider.map(str::to_string),
            priority: 10,
            match_type: "email_domain".into(),
            match_field: "email".into(),
            match_value: value.into(),
            group: group.into(),
        }
    }

    #[test]
    fn mapping_rules_idempotent_reapply_keeps_diff_noop() {
        // C1 regression: re-applying identical YAML with non-empty
        // mapping rules must NOT trigger a wipe. Before the enum fix,
        // `mapping_rules_to_replace` stayed empty on equality (correct)
        // and a helper then concluded "wipe DB because vec is empty
        // AND DB has rules" (WRONG — wiped every idempotent re-apply).
        let yaml = DeclarativeIam {
            groups: vec![yg("admins")],
            mapping_rules: vec![yaml_rule(None, "admins", "corp.example")],
            ..Default::default()
        };
        let current = CurrentIam {
            groups: vec![db_group(7, "admins")],
            mapping_rules: vec![db_rule(1, None, 7, "corp.example")],
            ..empty_db()
        };
        let diff = diff_iam(&yaml, &current).unwrap();
        assert_eq!(
            diff.mapping_rules,
            MappingRulesAction::Keep,
            "idempotent reapply must NOT clear or replace mapping rules"
        );
        assert!(diff.is_empty(), "idempotent diff must report is_empty()");
    }

    #[test]
    fn mapping_rules_yaml_empty_db_has_some_clears_all() {
        let yaml = DeclarativeIam {
            groups: vec![yg("admins")],
            mapping_rules: vec![], // explicit empty
            ..Default::default()
        };
        let current = CurrentIam {
            groups: vec![db_group(7, "admins")],
            mapping_rules: vec![db_rule(1, None, 7, "old.example")],
            ..empty_db()
        };
        let diff = diff_iam(&yaml, &current).unwrap();
        assert_eq!(
            diff.mapping_rules,
            MappingRulesAction::ClearAll,
            "YAML empty + DB non-empty must explicitly ClearAll"
        );
    }

    #[test]
    fn mapping_rules_yaml_differs_from_db_replaces() {
        let yaml = DeclarativeIam {
            groups: vec![yg("admins")],
            mapping_rules: vec![yaml_rule(None, "admins", "new.example")],
            ..Default::default()
        };
        let current = CurrentIam {
            groups: vec![db_group(7, "admins")],
            mapping_rules: vec![db_rule(1, None, 7, "old.example")],
            ..empty_db()
        };
        let diff = diff_iam(&yaml, &current).unwrap();
        match &diff.mapping_rules {
            MappingRulesAction::ReplaceWith(rules) => {
                assert_eq!(rules.len(), 1);
                assert_eq!(rules[0].match_value, "new.example");
            }
            other => panic!("expected ReplaceWith, got {other:?}"),
        }
    }

    #[test]
    fn mapping_rules_both_empty_is_keep_not_clear() {
        // Sanity: YAML empty AND DB empty must be Keep (idempotent),
        // not ClearAll (which would still DELETE over an empty table).
        let yaml = DeclarativeIam {
            mapping_rules: vec![],
            ..Default::default()
        };
        let current = empty_db();
        let diff = diff_iam(&yaml, &current).unwrap();
        assert_eq!(diff.mapping_rules, MappingRulesAction::Keep);
    }

    // ───── H1 regression: case-insensitive perm effect ─────────────────

    #[test]
    fn diff_user_permissions_lowercase_effect_is_idempotent() {
        // H1 regression: YAML authored with `effect: "allow"` must
        // not mark the user as changed on every apply. The DB
        // normalizes on insert, so `permissions_equal` has to
        // normalize both sides before comparing.
        let lowercase_perm = Permission {
            id: 0,
            effect: "allow".into(),
            actions: vec!["read".into()],
            resources: vec!["*".into()],
            conditions: None,
        };
        let canonical_perm = Permission {
            effect: "Allow".into(),
            ..lowercase_perm.clone()
        };
        let yaml = DeclarativeIam {
            users: vec![DeclarativeUser {
                permissions: vec![lowercase_perm],
                ..yu("alice", "K1")
            }],
            ..Default::default()
        };
        let current = CurrentIam {
            users: vec![IamUser {
                permissions: vec![canonical_perm],
                ..db_user(1, "alice", "K1")
            }],
            ..empty_db()
        };
        let diff = diff_iam(&yaml, &current).unwrap();
        assert!(
            diff.users_to_update.is_empty(),
            "`allow` vs `Allow` is not a semantic change; diff must be empty. \
             Got users_to_update = {:?}",
            diff.users_to_update
        );
    }

    // ───── M1 regression: access-key swap between surviving users ─────

    #[test]
    fn diff_rejects_access_key_swap_between_surviving_users() {
        // M1: YAML swaps access_keys between two users who both exist
        // in DB. Under diff-by-name, both are UPDATE candidates; the
        // first UPDATE would then violate UNIQUE(access_key_id) with
        // the other user's still-held key. Validation must catch it
        // BEFORE the transaction so the operator gets a clear error.
        let yaml = DeclarativeIam {
            users: vec![
                yu("alice", "K_BOB"), // alice grabs bob's key
                yu("bob", "K_ALICE"), // bob grabs alice's key
            ],
            ..Default::default()
        };
        let current = CurrentIam {
            users: vec![db_user(1, "alice", "K_ALICE"), db_user(2, "bob", "K_BOB")],
            ..empty_db()
        };
        let err = diff_iam(&yaml, &current).unwrap_err();
        assert!(
            err.contains("collides") || err.contains("conflict") || err.contains("UNIQUE"),
            "swap must be caught at validation, got: {err}"
        );
        assert!(
            err.contains("alice") && err.contains("bob"),
            "error must cite both conflicting user names, got: {err}"
        );
    }

    // ───── External-user delete-preservation (this fix) ───────────────

    fn db_external_user(id: i64, name: &str, ak: &str) -> IamUser {
        IamUser {
            auth_source: "external".into(),
            ..db_user(id, name, ak)
        }
    }

    // --- pure helper: external_user_is_reconstructable ---

    #[test]
    fn reconstructable_external_no_perms_groups_eq_baseline_is_true() {
        // (i) external, no perms, groups == baseline → deletable.
        let user = IamUser {
            group_ids: vec![11, 22],
            ..db_external_user(1, "oauth-alice", "K1")
        };
        assert!(external_user_is_reconstructable(&user, &[11, 22]));
    }

    #[test]
    fn reconstructable_external_extra_manual_group_is_false() {
        // (ii) external, no perms, groups ⊋ baseline → preserve.
        let user = IamUser {
            group_ids: vec![11, 22, 33], // 33 added manually
            ..db_external_user(1, "oauth-alice", "K1")
        };
        assert!(!external_user_is_reconstructable(&user, &[11, 22]));
    }

    #[test]
    fn reconstructable_external_with_direct_perms_is_false() {
        // (iii) external WITH direct permissions, groups == baseline → preserve.
        let user = IamUser {
            group_ids: vec![11],
            permissions: vec![perm(&["read"], &["bucket/*"])],
            ..db_external_user(1, "oauth-alice", "K1")
        };
        assert!(!external_user_is_reconstructable(&user, &[11]));
    }

    #[test]
    fn reconstructable_external_empty_groups_empty_baseline_is_true() {
        // (iv) external, empty groups + empty baseline, no perms → deletable.
        let user = db_external_user(1, "oauth-alice", "K1");
        assert!(external_user_is_reconstructable(&user, &[]));
    }

    #[test]
    fn reconstructable_disabled_external_user_is_false() {
        // M5: a DISABLED external user (admin soft-ban) is NOT reconstructable
        // even when groups==baseline + no perms — re-provisioning would
        // resurrect them as enabled, undoing the ban.
        let user = IamUser {
            enabled: false,
            group_ids: vec![11, 22],
            ..db_external_user(1, "oauth-banned", "K1")
        };
        assert!(
            !external_user_is_reconstructable(&user, &[11, 22]),
            "disabled external user must be preserved (not churned/re-enabled)"
        );
    }

    #[test]
    fn reconstructable_set_compare_is_order_and_dedup_safe() {
        // (v) set comparison: order-independent + dedup-safe.
        let user = IamUser {
            group_ids: vec![22, 11, 11], // unordered + duplicate
            ..db_external_user(1, "oauth-alice", "K1")
        };
        assert!(external_user_is_reconstructable(&user, &[11, 22, 22]));
    }

    // --- diff_iam-level: external delete decision ---

    #[test]
    fn diff_deletes_reconstructable_external_user_absent_from_yaml() {
        // External user absent from YAML, only the mapping-rule group,
        // no direct perms → reconstructable → appears in users_to_delete.
        let yaml = DeclarativeIam::default();
        let current = CurrentIam {
            users: vec![IamUser {
                group_ids: vec![11],
                ..db_external_user(5, "oauth-bob", "K_OB")
            }],
            groups: vec![db_group(11, "readers")],
            external_baseline_groups: HashMap::from([(5, vec![11])]),
            ..empty_db()
        };
        let diff = diff_iam(&yaml, &current).unwrap();
        assert_eq!(diff.users_to_delete, vec![(5, "oauth-bob".to_string())]);
        // The cull is recorded as an EXTERNAL delete, so the unattended-startup
        // guard treats it as benign: local_user_delete_count() must be 0 (else a
        // declarative+OAuth deploy crash-loops on restart once anyone logs in).
        assert_eq!(diff.external_user_deletes, vec![5]);
        assert_eq!(
            diff.local_user_delete_count(),
            0,
            "a reconstructable-external cull must NOT count as a destructive local delete"
        );
    }

    #[test]
    fn local_user_delete_count_counts_only_non_external() {
        // Mixed: a local delete (id 7) + an external cull (id 5). Only the local
        // one is "destructive" for the startup refuse-to-start decision.
        let diff = IamDiff {
            users_to_delete: vec![(7, "alice".into()), (5, "oauth-bob".into())],
            external_user_deletes: vec![5],
            ..IamDiff::default()
        };
        assert_eq!(diff.local_user_delete_count(), 1);
    }

    #[test]
    fn diff_preserves_external_user_with_extra_group() {
        // Same external user but with an extra manually-added group
        // beyond the mapping-rule baseline → NOT in users_to_delete.
        let yaml = DeclarativeIam::default();
        let current = CurrentIam {
            users: vec![IamUser {
                group_ids: vec![11, 99], // 99 is manual
                ..db_external_user(5, "oauth-bob", "K_OB")
            }],
            groups: vec![db_group(11, "readers"), db_group(99, "secret")],
            external_baseline_groups: HashMap::from([(5, vec![11])]),
            ..empty_db()
        };
        let diff = diff_iam(&yaml, &current).unwrap();
        assert!(
            diff.users_to_delete.is_empty(),
            "external user with extra group must be preserved, got {:?}",
            diff.users_to_delete
        );
    }

    #[test]
    fn diff_preserves_external_user_with_direct_perms() {
        // External user with direct perms, groups == baseline → preserve.
        let yaml = DeclarativeIam::default();
        let current = CurrentIam {
            users: vec![IamUser {
                group_ids: vec![11],
                permissions: vec![perm(&["write"], &["bucket/*"])],
                ..db_external_user(5, "oauth-bob", "K_OB")
            }],
            groups: vec![db_group(11, "readers")],
            external_baseline_groups: HashMap::from([(5, vec![11])]),
            ..empty_db()
        };
        let diff = diff_iam(&yaml, &current).unwrap();
        assert!(
            diff.users_to_delete.is_empty(),
            "external user with direct perms must be preserved, got {:?}",
            diff.users_to_delete
        );
    }

    #[test]
    fn diff_still_deletes_local_user_absent_from_yaml() {
        // Regression guard: a LOCAL user absent from YAML is still
        // deleted regardless of the external-preservation logic.
        let yaml = DeclarativeIam::default();
        let current = CurrentIam {
            users: vec![IamUser {
                group_ids: vec![11, 99],
                permissions: vec![perm(&["admin"], &["*"])],
                ..db_user(7, "local-carol", "K_LC")
            }],
            groups: vec![db_group(11, "readers"), db_group(99, "secret")],
            ..empty_db()
        };
        let diff = diff_iam(&yaml, &current).unwrap();
        assert_eq!(diff.users_to_delete, vec![(7, "local-carol".to_string())]);
    }

    #[test]
    fn diff_preserves_external_user_with_missing_baseline_entry() {
        // External user with groups but NO baseline entry (e.g. no
        // email on their identity) → baseline treated as empty → groups
        // ⊋ empty → preserved.
        let yaml = DeclarativeIam::default();
        let current = CurrentIam {
            users: vec![IamUser {
                group_ids: vec![11],
                ..db_external_user(5, "oauth-bob", "K_OB")
            }],
            groups: vec![db_group(11, "readers")],
            // no entry for user 5
            ..empty_db()
        };
        let diff = diff_iam(&yaml, &current).unwrap();
        assert!(
            diff.users_to_delete.is_empty(),
            "external user with no baseline + non-empty groups must be preserved"
        );
    }

    // ───── Provider-scoped baseline (data-loss regression) ────────────

    #[test]
    fn baseline_is_provider_filtered_not_unfiltered_preview() {
        // Drive the REAL computation (compute_external_baseline_groups)
        // against a multi-provider, provider-SCOPED rule set. This is
        // the gap the adversarial review found: the baseline must mirror
        // what an actual login grants (provider-filtered evaluate_mappings),
        // NOT the unfiltered preview that would over-grant cross-provider
        // groups and silently delete a manually-added membership.
        use crate::config_db::auth_providers::{
            CreateAuthProviderRequest, CreateMappingRuleRequest,
        };
        use crate::config_db::ConfigDb;

        let db = ConfigDb::in_memory("test-pass").unwrap();

        // Two providers.
        let prov_a = db
            .create_auth_provider(&CreateAuthProviderRequest {
                name: "okta".into(),
                provider_type: "oidc".into(),
                enabled: true,
                priority: 0,
                display_name: None,
                client_id: None,
                client_secret: None,
                issuer_url: None,
                scopes: default_scopes(),
                extra_config: None,
            })
            .unwrap();
        let prov_b = db
            .create_auth_provider(&CreateAuthProviderRequest {
                name: "google".into(),
                provider_type: "oidc".into(),
                enabled: true,
                priority: 0,
                display_name: None,
                client_id: None,
                client_secret: None,
                issuer_url: None,
                scopes: default_scopes(),
                extra_config: None,
            })
            .unwrap();

        // Two groups: 'eng' (granted by provider A's rule), 'staff'
        // (granted by provider B's rule).
        let grp_eng = db.create_group("eng", "", &[]).unwrap();
        let grp_staff = db.create_group("staff", "", &[]).unwrap();

        // Rule A scoped to provider A → eng. Rule B scoped to provider B
        // → staff. BOTH would match the email by domain; only the
        // provider filter keeps them apart.
        db.create_group_mapping_rule(&CreateMappingRuleRequest {
            provider_id: Some(prov_a.id),
            priority: 10,
            match_type: "email_domain".into(),
            match_field: "email".into(),
            match_value: "corp.example".into(),
            group_id: grp_eng.id,
        })
        .unwrap();
        db.create_group_mapping_rule(&CreateMappingRuleRequest {
            provider_id: Some(prov_b.id),
            priority: 10,
            match_type: "email_domain".into(),
            match_field: "email".into(),
            match_value: "corp.example".into(),
            group_id: grp_staff.id,
        })
        .unwrap();

        // External user with an identity ONLY on provider B.
        let user = db
            .create_external_user("oauth-bob", "AKEXTBOB0001", "secret-bob-12")
            .unwrap();
        db.create_external_identity(
            user.id,
            prov_b.id,
            "bob-sub-200",
            Some("bob@corp.example"),
            Some("Bob"),
            None,
            true,
        )
        .unwrap();

        // Manual extra membership in 'eng' (the cross-provider group) —
        // the kind of grant an admin adds via POST /groups/:id/members.
        db.add_user_to_group(grp_eng.id, user.id).unwrap();

        let rules = db.load_group_mapping_rules().unwrap();
        let baseline = compute_external_baseline_groups(&db, &rules).unwrap();

        let got: std::collections::BTreeSet<i64> = baseline
            .get(&user.id)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect();
        let expected: std::collections::BTreeSet<i64> = [grp_staff.id].into_iter().collect();
        assert_eq!(
            got, expected,
            "baseline must be the provider-B-scoped grant {{staff}} only — NOT \
             {{eng,staff}} from the unfiltered preview path"
        );

        // End-to-end through the helper: actual groups = {eng, staff},
        // baseline = {staff} → NOT reconstructable → must be PRESERVED.
        let user_loaded = db.get_user_by_id(user.id).unwrap();
        let baseline_groups = baseline.get(&user.id).cloned().unwrap_or_default();
        assert!(
            !external_user_is_reconstructable(&user_loaded, &baseline_groups),
            "user with a manual cross-provider group beyond the real-login \
             baseline must be preserved (not deleted)"
        );
    }

    #[test]
    fn baseline_excludes_disabled_provider_rules() {
        // M3 (data-loss regression): a DISABLED provider can never authenticate
        // a login (ExternalAuthManager::rebuild skips it), so its mapping-rule
        // groups must NOT enter the baseline. If they did, an external user
        // whose ONLY membership came from that disabled provider would look
        // "reconstructable" and be DELETED — but a real login could never
        // rebuild them, so the deletion is permanent state loss.
        use crate::config_db::auth_providers::{
            CreateAuthProviderRequest, CreateMappingRuleRequest,
        };
        use crate::config_db::ConfigDb;

        let db = ConfigDb::in_memory("test-pass").unwrap();

        // One DISABLED provider whose rule would grant 'eng'.
        let prov = db
            .create_auth_provider(&CreateAuthProviderRequest {
                name: "legacy-okta".into(),
                provider_type: "oidc".into(),
                enabled: false, // ← disabled: login through it is impossible
                priority: 0,
                display_name: None,
                client_id: None,
                client_secret: None,
                issuer_url: None,
                scopes: default_scopes(),
                extra_config: None,
            })
            .unwrap();
        let grp_eng = db.create_group("eng", "", &[]).unwrap();
        db.create_group_mapping_rule(&CreateMappingRuleRequest {
            provider_id: Some(prov.id),
            priority: 10,
            match_type: "email_domain".into(),
            match_field: "email".into(),
            match_value: "corp.example".into(),
            group_id: grp_eng.id,
        })
        .unwrap();

        // External user provisioned (historically) through the now-disabled
        // provider, member of 'eng'.
        let user = db
            .create_external_user("oauth-carol", "AKEXTCAR0001", "secret-carol1")
            .unwrap();
        db.create_external_identity(
            user.id,
            prov.id,
            "carol-sub-1",
            Some("carol@corp.example"),
            Some("Carol"),
            None,
            true,
        )
        .unwrap();
        db.add_user_to_group(grp_eng.id, user.id).unwrap();

        let rules = db.load_group_mapping_rules().unwrap();
        let baseline = compute_external_baseline_groups(&db, &rules).unwrap();

        // The disabled provider's rule must NOT contribute → empty baseline.
        let got = baseline.get(&user.id).cloned().unwrap_or_default();
        assert!(
            got.is_empty(),
            "disabled provider's rule must not enter the baseline, got {got:?}"
        );

        // actual = {eng}, baseline = {} → NOT reconstructable → PRESERVED.
        let user_loaded = db.get_user_by_id(user.id).unwrap();
        assert!(
            !external_user_is_reconstructable(&user_loaded, &got),
            "external user reachable only via a disabled provider must be preserved"
        );
    }
}
