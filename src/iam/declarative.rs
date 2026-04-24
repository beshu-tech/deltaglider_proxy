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
use crate::iam::{normalize_permissions, validate_permissions, Group, IamUser, Permission};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet};

// ───── Wire types ──────────────────────────────────────────────────────

/// One entry in `access.iam_users`. References groups by NAME.
///
/// Secrets (`secret_access_key`) land in the same env-substitution
/// pipeline the rest of the YAML uses (`${env:NAME}` or literal);
/// the canonical exporter redacts them exactly like SigV4 creds.
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

fn default_scopes() -> String {
    "openid email profile".to_string()
}

fn default_email() -> String {
    "email".to_string()
}

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
pub struct CurrentIam {
    pub users: Vec<IamUser>,
    pub groups: Vec<Group>,
    pub auth_providers: Vec<AuthProviderConfig>,
    pub mapping_rules: Vec<GroupMappingRule>,
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
                // Secret redacted — operator wires via env. Same
                // redaction policy the canonical YAML exporter uses.
                secret_access_key: String::new(),
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
            // client_secret redacted (infra secret).
            client_secret: None,
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
    let current = CurrentIam {
        users: db.load_users().map_err(|e| format!("load users: {e}"))?,
        groups: db.load_groups().map_err(|e| format!("load groups: {e}"))?,
        auth_providers: db
            .load_auth_providers()
            .map_err(|e| format!("load auth_providers: {e}"))?,
        mapping_rules: db
            .load_group_mapping_rules()
            .map_err(|e| format!("load mapping_rules: {e}"))?,
    };
    diff_iam(yaml, &current)
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
            Some(dp) if provider_equal(dp, yp) => {}
            Some(dp) => diff.providers_to_update.push((dp.id, yp.clone())),
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
            Some(du) if user_equal(du, yu, &db_group_id_to_name) => {}
            Some(du) => diff.users_to_update.push((du.id, yu.clone())),
            None => diff.users_to_create.push(yu.clone()),
        }
    }
    for du in &db.users {
        if !yaml_users.contains_key(du.name.as_str()) {
            diff.users_to_delete.push((du.id, du.name.clone()));
        }
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
    let current = CurrentIam {
        users: db.load_users().map_err(|e| format!("load users: {e}"))?,
        groups: db.load_groups().map_err(|e| format!("load groups: {e}"))?,
        auth_providers: db
            .load_auth_providers()
            .map_err(|e| format!("load auth_providers: {e}"))?,
        mapping_rules: db
            .load_group_mapping_rules()
            .map_err(|e| format!("load mapping_rules: {e}"))?,
    };
    let diff = diff_iam(yaml, &current)?;
    db.apply_iam_reconcile(&diff, &current)
        .map_err(|e| format!("apply reconcile: {e}"))
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
        CurrentIam {
            users: vec![],
            groups: vec![],
            auth_providers: vec![],
            mapping_rules: vec![],
        }
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
}
