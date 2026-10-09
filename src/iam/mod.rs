// SPDX-License-Identifier: BUSL-1.1

//! IAM: local user management with attribute-based access control (ABAC).
//!
//! Users are stored in an encrypted SQLCipher database (see `config_db.rs`).
//! At runtime, users are indexed in a `HashMap<access_key_id, IamUser>` for
//! O(1) lookup during SigV4 authentication.
//!
//! # Module structure
//!
//! - `types` — Data types: `IamUser`, `Group`, `Permission`, `S3Action`, `AuthenticatedUser`
//! - `permissions` — Pure permission evaluation logic (no I/O, no framework)
//! - `middleware` — Axum authorization middleware
//! - `keygen` — Cryptographic key generation
//! - `index` — `IamIndex` for O(1) user lookup and `IamState` enum

pub mod declarative;
pub mod external_auth;
pub mod keygen;
pub(crate) mod listing;
pub mod middleware;
pub mod permissions;
pub mod types;

use arc_swap::ArcSwap;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tracing::warn;

// Re-export everything at crate::iam level for backward compatibility
pub use declarative::{
    diff_iam, duplicate_user_name, export_as_declarative, export_as_declarative_inner,
    preview_declarative_iam, preview_declarative_iam_at_boot, reconcile_declarative_iam,
    reconcile_declarative_iam_at_boot, refused_provider_changes, snapshot_from_access,
    validate_declarative_iam, CurrentIam, DeclarativeAuthProvider, DeclarativeExternalIdentity,
    DeclarativeGroup, DeclarativeIam, DeclarativeMappingRule, DeclarativeUser, IamDiff,
    MappingRulesAction, ReconcileStats,
};
pub use keygen::{generate_access_key_id, generate_secret_access_key};
pub use middleware::authorization_middleware;
pub use permissions::{
    normalize_permissions, user_can_see_common_prefix, user_can_see_listed_key,
    validate_permissions,
};
pub use types::*;

/// Monotonic IAM-index version counter.
///
/// Incremented on every successful [`api::admin::users::rebuild_iam_index`]
/// call (which happens after any IAM mutation: user/group CRUD, OAuth
/// provider changes, mapping-rule edits). Exposed via
/// `GET /_/api/admin/iam/version` so integration tests can wait for a
/// deterministic rebuild barrier instead of blind `sleep(1s)` — the
/// latter is both slow AND flake-prone under CI load.
///
/// One counter per process is correct because each TestServer spawns its
/// own proxy process; there is no cross-process IAM state to reconcile.
///
/// Wraps at 2^64 which is ~600 years at 1M rebuilds/sec — safe enough.
static IAM_VERSION: AtomicU64 = AtomicU64::new(0);

/// Increment the IAM version counter and return the new value.
///
/// Called from `rebuild_iam_index` AFTER the new `IamState` is stored,
/// so observers polling the version see the bump only after the state
/// is visible to subsequent authentications.
pub fn bump_iam_version() -> u64 {
    // SeqCst is overkill for correctness here (we only need monotonic
    // observability, Release+Acquire would do), but the counter ticks
    // infrequently (once per IAM mutation, not per request) so the
    // extra synchronisation cost is irrelevant.
    IAM_VERSION.fetch_add(1, Ordering::SeqCst) + 1
}

/// Read the current IAM version counter.
pub fn current_iam_version() -> u64 {
    IAM_VERSION.load(Ordering::SeqCst)
}

/// What S3 does while no IAM user exists. A pure function of the LIVE
/// config ([`crate::config::Config::empty_iam_outcome`]), carried by the
/// IAM state, so a rebuild with no users (the last user deleted, a peer
/// synced an empty DB) follows the current config, never the state the
/// process started in (review B1).
#[derive(Clone, PartialEq, Eq)]
pub enum EmptyIamOutcome {
    /// The bootstrap SigV4 pair signs S3 requests.
    Legacy(AuthConfig),
    /// Open access: explicit `authentication: none`.
    Disabled,
    /// No credential is left: every S3 request is refused.
    DenyAll,
}

impl std::fmt::Debug for EmptyIamOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Legacy(_) => "Legacy(<bootstrap pair>)",
            Self::Disabled => "Disabled",
            Self::DenyAll => "DenyAll",
        })
    }
}

impl EmptyIamOutcome {
    /// THE rule: the pair if one is set, else open access if the operator
    /// asked for it, else refuse everything.
    pub fn from_parts(pair: Option<AuthConfig>, open_access_requested: bool) -> Self {
        match pair {
            Some(pair) => Self::Legacy(pair),
            None if open_access_requested => Self::Disabled,
            None => Self::DenyAll,
        }
    }

    /// The one log text of each outcome.
    pub fn describe(&self) -> &'static str {
        match self {
            Self::Legacy(_) => "the bootstrap SigV4 pair signs S3 requests",
            Self::Disabled => "open access (`authentication: none`)",
            Self::DenyAll => "no credential: every S3 request is refused",
        }
    }
}

/// The credentials S3 has at one moment: the IAM users, and what applies
/// while there are none.
#[derive(Debug, Clone, Copy)]
pub struct AuthSurface<'a> {
    pub users: usize,
    pub when_empty: &'a EmptyIamOutcome,
}

impl AuthSurface<'_> {
    /// Some request can still authenticate (or open access is on).
    pub fn has_credential(&self) -> bool {
        self.users > 0 || *self.when_empty != EmptyIamOutcome::DenyAll
    }
}

/// An admin change refused because it would leave S3 with no credential.
#[derive(Debug, PartialEq, Eq)]
pub struct Lockout;

impl std::fmt::Display for Lockout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "this change leaves the proxy without authentication: no IAM user, no bootstrap \
             SigV4 pair and no `authentication: none` would remain. Create an IAM user or set \
             a bootstrap pair first",
        )
    }
}

/// Review A14, THE rule of every admin action that removes credentials (a
/// user delete, a config change, an IAM restore): it may not take S3 from
/// a credential to none. A deny-all state stays reachable only through a
/// peer sync, which cannot be refused.
pub fn check_lockout(before: AuthSurface<'_>, after: AuthSurface<'_>) -> Result<(), Lockout> {
    if before.has_credential() && !after.has_credential() {
        Err(Lockout)
    } else {
        Ok(())
    }
}

/// Runtime IAM state — supports legacy single-credential mode and multi-user IAM.
pub enum IamState {
    /// Open access. Only explicit `authentication: none` leads here: every
    /// other path to "no credential" is an empty `Iam` index (deny all).
    Disabled,
    /// Legacy single credential pair (backward compatible with old config).
    Legacy(AuthConfig),
    /// Multi-user IAM with per-user credentials and permissions.
    Iam(IamIndex),
}

impl IamState {
    /// IAM mode with no users and no fallback: every signed request is
    /// denied. The state for "authentication is configured, but no
    /// credential is left" (the bootstrap pair removed, the last user gone).
    pub fn deny_all() -> Self {
        Self::from_outcome(EmptyIamOutcome::DenyAll)
    }

    /// The state of an empty user set.
    pub fn from_outcome(outcome: EmptyIamOutcome) -> Self {
        match outcome {
            EmptyIamOutcome::Legacy(pair) => IamState::Legacy(pair),
            EmptyIamOutcome::Disabled => IamState::Disabled,
            EmptyIamOutcome::DenyAll => IamState::Iam(IamIndex::from_users(Vec::new())),
        }
    }

    /// The IAM users in force (0 outside IAM mode).
    pub fn user_count(&self) -> usize {
        match self {
            IamState::Iam(index) => index.len(),
            IamState::Disabled | IamState::Legacy(_) => 0,
        }
    }

    /// The outcome this state applies when its users are gone.
    pub fn when_empty(&self) -> EmptyIamOutcome {
        match self {
            IamState::Disabled => EmptyIamOutcome::Disabled,
            IamState::Legacy(pair) => EmptyIamOutcome::Legacy(pair.clone()),
            IamState::Iam(index) => index.when_empty.clone(),
        }
    }

    /// This state with another empty-set outcome (the config changed the
    /// pair or `authentication`). `None` when nothing changes. A state with
    /// users keeps them; an empty one becomes the outcome itself.
    pub fn with_when_empty(&self, outcome: EmptyIamOutcome) -> Option<IamState> {
        if self.when_empty() == outcome {
            return None;
        }
        Some(match self {
            IamState::Iam(index) if !index.is_empty() => IamState::Iam(IamIndex {
                users: index.users.clone(),
                groups: index.groups.clone(),
                when_empty: outcome,
            }),
            _ => Self::from_outcome(outcome),
        })
    }

    /// IAM mode with at least one user. A `deny_all` index is IAM mode
    /// without users: it must not pass for "IAM users exist".
    pub fn has_iam_users(&self) -> bool {
        matches!(self, IamState::Iam(index) if !index.is_empty())
    }

    /// Whether the S3 API accepts this key pair now: the bootstrap pair in
    /// `Legacy` mode; in `Iam` mode only an enabled user's pair (the
    /// bootstrap pair too when a user carries it, as `legacy-admin` does).
    /// Open mode checks no credential, so no pair is "accepted".
    pub fn accepts_credentials(&self, access_key_id: &str, secret_access_key: &str) -> bool {
        use crate::security::secret_eq;
        match self {
            IamState::Disabled => false,
            IamState::Legacy(auth) => {
                secret_eq(access_key_id.as_bytes(), auth.access_key_id.as_bytes())
                    && secret_eq(
                        secret_access_key.as_bytes(),
                        auth.secret_access_key.as_bytes(),
                    )
            }
            IamState::Iam(index) => index.get(access_key_id).is_some_and(|u| {
                u.enabled && secret_eq(secret_access_key.as_bytes(), u.secret_access_key.as_bytes())
            }),
        }
    }
}

/// Thread-safe, hot-swappable IAM state.
pub type SharedIamState = Arc<ArcSwap<IamState>>;

/// Fast O(1) user lookup index, rebuilt from the database on load/sync.
pub struct IamIndex {
    users: HashMap<String, IamUser>,
    groups: Vec<Group>,
    /// What S3 does once the users are gone; set from the live config.
    when_empty: EmptyIamOutcome,
}

impl IamIndex {
    /// Build the index from a list of users (keyed by access_key_id).
    pub fn from_users(users: Vec<IamUser>) -> Self {
        Self::from_users_and_groups(users, Vec::new())
    }

    /// Build the index from users and groups, merging group permissions into each user's
    /// effective permission set. The user's `permissions` field in the index will contain
    /// both direct and group-inherited permissions.
    pub fn from_users_and_groups(users: Vec<IamUser>, groups: Vec<Group>) -> Self {
        let group_perms: HashMap<i64, &[Permission]> = groups
            .iter()
            .map(|g| (g.id, g.permissions.as_slice()))
            .collect();

        let mut map = HashMap::with_capacity(users.len());
        for mut user in users {
            for gid in &user.group_ids {
                if let Some(perms) = group_perms.get(gid) {
                    user.permissions.extend(perms.iter().cloned());
                }
            }

            user.permissions = match permissions::expand_permission_templates(
                &user.permissions,
                &user.name,
                &user.access_key_id,
            ) {
                Ok(perms) => perms,
                Err(e) => {
                    warn!(
                        "IAM user '{}' ({}) has invalid permission templates: {} — denying all permissions",
                        user.name, user.access_key_id, e
                    );
                    Vec::new()
                }
            };

            // Precompute IAM policies from permissions for iam-rs evaluation
            user.iam_policies = user
                .permissions
                .iter()
                .map(permissions::permission_to_iam_policy)
                .collect();

            if user.enabled && user.permissions.is_empty() {
                warn!(
                    "IAM user '{}' ({}) is enabled but has no permissions — all operations will be denied",
                    user.name, user.access_key_id
                );
            }
            map.insert(user.access_key_id.clone(), user);
        }
        Self {
            users: map,
            groups,
            when_empty: EmptyIamOutcome::DenyAll,
        }
    }

    /// Look up a user by access_key_id. O(1).
    pub fn get(&self, access_key_id: &str) -> Option<&IamUser> {
        self.users.get(access_key_id)
    }

    /// Look up a user by database id. O(n); for per-request checks on the
    /// low-traffic admin surface, not for the S3 hot path.
    pub fn get_by_id(&self, id: i64) -> Option<&IamUser> {
        self.users.values().find(|u| u.id == id)
    }

    /// Number of users in the index.
    pub fn len(&self) -> usize {
        self.users.len()
    }

    pub fn is_empty(&self) -> bool {
        self.users.is_empty()
    }

    /// Get the groups stored in the index.
    pub fn groups(&self) -> &[Group] {
        &self.groups
    }

    /// Build IAM state from users and groups. Users exist: `Iam(index)`,
    /// carrying `when_empty`. No users (the last user deleted, a peer synced
    /// an empty DB): the state of `when_empty` itself. A rebuild passes the
    /// current state's [`IamState::when_empty`]; a config change passes the
    /// new config's [`crate::config::Config::empty_iam_outcome`].
    pub fn build_iam_state(
        users: Vec<IamUser>,
        groups: Vec<Group>,
        when_empty: EmptyIamOutcome,
    ) -> IamState {
        if users.is_empty() {
            return IamState::from_outcome(when_empty);
        }
        let mut index = Self::from_users_and_groups(users, groups);
        index.when_empty = when_empty;
        IamState::Iam(index)
    }
}

/// Return predefined policy templates for the admin UI.
///
/// A strict ladder, least privilege first. `write` never includes `delete`
/// and only `*` grants `admin` (bucket create/delete + the admin API). The
/// old "Read/Write (No Delete)" was `Allow *` + `Deny delete` — i.e. an
/// administrator — while reading like a narrower Read/Write.
pub fn canned_policies() -> Vec<CannedPolicy> {
    fn allow(actions: &[&str]) -> Vec<Permission> {
        vec![Permission {
            id: 0,
            effect: "Allow".into(),
            actions: actions.iter().map(|a| (*a).into()).collect(),
            resources: vec!["*".into()],
            conditions: None,
        }]
    }
    vec![
        CannedPolicy {
            name: "Read Only",
            description: "List and download objects in every bucket",
            permissions: allow(&["read", "list"]),
        },
        CannedPolicy {
            name: "Read/Write (no delete)",
            description: "List, download and upload; cannot delete objects",
            permissions: allow(&["read", "write", "list"]),
        },
        CannedPolicy {
            name: "Read/Write/Delete",
            description: "List, download, upload and delete objects; no bucket or admin operations",
            permissions: allow(&["read", "write", "delete", "list"]),
        },
        CannedPolicy {
            name: "Full Access (admin)",
            description: "Every operation, including bucket management and the admin API",
            permissions: allow(&["*"]),
        },
    ]
}

/// A predefined policy template for quick user setup.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CannedPolicy {
    pub name: &'static str,
    pub description: &'static str,
    pub permissions: Vec<Permission>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// auth-4: the pair the S3 API accepts, per mode. In IAM mode the
    /// bootstrap pair counts only when a user carries it.
    #[test]
    fn accepts_credentials_truth_table() {
        let boot = AuthConfig {
            access_key_id: "AKBOOT".into(),
            secret_access_key: "SKBOOT".into(),
        };
        let user = |ak: &str, sk: &str, enabled: bool| IamUser {
            id: 1,
            name: ak.to_lowercase(),
            access_key_id: ak.into(),
            secret_access_key: sk.into(),
            enabled,
            created_at: String::new(),
            permissions: vec![],
            group_ids: vec![],
            auth_source: "local".into(),
            iam_policies: vec![],
        };
        let legacy = IamState::Legacy(boot.clone());
        assert!(legacy.accepts_credentials("AKBOOT", "SKBOOT"));
        assert!(!legacy.accepts_credentials("AKBOOT", "wrong"));
        assert!(!legacy.accepts_credentials("AKOTHER", "SKBOOT"));
        let iam = IamIndex::build_iam_state(
            vec![user("AKCI", "SKCI", true), user("AKOFF", "SKOFF", false)],
            vec![],
            EmptyIamOutcome::Legacy(boot.clone()),
        );
        assert!(iam.accepts_credentials("AKCI", "SKCI"));
        assert!(!iam.accepts_credentials("AKCI", "wrong"));
        assert!(!iam.accepts_credentials("AKOFF", "SKOFF"), "disabled");
        assert!(
            !iam.accepts_credentials("AKBOOT", "SKBOOT"),
            "bootstrap pair"
        );
        let carried = IamState::Iam(IamIndex::from_users(vec![user("AKBOOT", "SKBOOT", true)]));
        assert!(
            carried.accepts_credentials("AKBOOT", "SKBOOT"),
            "legacy-admin"
        );
        assert!(!IamState::Disabled.accepts_credentials("AKBOOT", "SKBOOT"));
    }

    /// Only the preset that says "admin" may grant `*`; every other preset
    /// is Allow-only and delete appears only where the name says so.
    #[test]
    fn canned_policies_do_not_hide_admin() {
        for p in canned_policies() {
            let grants_star = p
                .permissions
                .iter()
                .any(|r| r.effect == "Allow" && r.actions.iter().any(|a| a == "*"));
            assert_eq!(
                grants_star,
                p.name.contains("admin"),
                "{}: `*` only in the admin preset",
                p.name
            );
            assert!(
                p.permissions.iter().all(|r| r.effect == "Allow"),
                "{}: no Allow-*/Deny tricks",
                p.name
            );
            let deletes = p
                .permissions
                .iter()
                .any(|r| r.actions.iter().any(|a| a == "delete" || a == "*"));
            assert_eq!(
                deletes,
                !p.name.contains("no delete") && p.name != "Read Only",
                "{}: delete matches the name",
                p.name
            );
        }
    }

    #[test]
    fn test_iam_index_lookup() {
        let users = vec![
            IamUser {
                id: 1,
                name: "admin".into(),
                access_key_id: "AKADMIN1".into(),
                secret_access_key: "secret1".into(),
                enabled: true,
                created_at: String::new(),
                permissions: vec![],
                group_ids: vec![],
                auth_source: "local".into(),
                iam_policies: vec![],
            },
            IamUser {
                id: 2,
                name: "viewer".into(),
                access_key_id: "AKVIEW01".into(),
                secret_access_key: "secret2".into(),
                enabled: false,
                created_at: String::new(),
                permissions: vec![],
                group_ids: vec![],
                auth_source: "local".into(),
                iam_policies: vec![],
            },
        ];

        let index = IamIndex::from_users(users);
        assert_eq!(index.len(), 2);

        let admin = index.get("AKADMIN1").unwrap();
        assert_eq!(admin.name, "admin");
        assert!(admin.enabled);

        let viewer = index.get("AKVIEW01").unwrap();
        assert!(!viewer.enabled);

        assert!(index.get("AKNOTHERE").is_none());
    }

    #[test]
    fn test_group_permissions_merged_with_user() {
        let users = vec![IamUser {
            id: 1,
            name: "dev".into(),
            access_key_id: "AK1".into(),
            secret_access_key: "s".into(),
            enabled: true,
            created_at: String::new(),
            permissions: vec![Permission {
                id: 0,
                effect: "Allow".into(),
                actions: vec!["read".into()],
                resources: vec!["*".into()],
                conditions: None,
            }],
            group_ids: vec![10],
            auth_source: "local".into(),
            iam_policies: vec![],
        }];
        let groups = vec![Group {
            id: 10,
            name: "writers".into(),
            description: String::new(),
            permissions: vec![Permission {
                id: 0,
                effect: "Allow".into(),
                actions: vec!["write".into()],
                resources: vec!["*".into()],
                conditions: None,
            }],
            member_ids: vec![1],
            created_at: String::new(),
        }];
        let index = IamIndex::from_users_and_groups(users, groups);
        let user = index.get("AK1").unwrap();
        let auth = AuthenticatedUser {
            name: user.name.clone(),
            access_key_id: user.access_key_id.clone(),
            iam_policies: user.iam_policies.clone(),
            permissions: user.permissions.clone(),
        };
        assert!(auth.can(S3Action::Read, "bucket", "key"));
        assert!(auth.can(S3Action::Write, "bucket", "key"));
        assert!(!auth.can(S3Action::Delete, "bucket", "key"));
    }

    #[test]
    fn test_group_deny_overrides_user_allow() {
        let users = vec![IamUser {
            id: 1,
            name: "dev".into(),
            access_key_id: "AK1".into(),
            secret_access_key: "s".into(),
            enabled: true,
            created_at: String::new(),
            permissions: vec![Permission {
                id: 0,
                effect: "Allow".into(),
                actions: vec!["*".into()],
                resources: vec!["*".into()],
                conditions: None,
            }],
            group_ids: vec![10],
            auth_source: "local".into(),
            iam_policies: vec![],
        }];
        let groups = vec![Group {
            id: 10,
            name: "no-delete".into(),
            description: String::new(),
            permissions: vec![Permission {
                id: 0,
                effect: "Deny".into(),
                actions: vec!["delete".into()],
                resources: vec!["releases/*".into()],
                conditions: None,
            }],
            member_ids: vec![1],
            created_at: String::new(),
        }];
        let index = IamIndex::from_users_and_groups(users, groups);
        let user = index.get("AK1").unwrap();
        let auth = AuthenticatedUser {
            name: user.name.clone(),
            access_key_id: user.access_key_id.clone(),
            iam_policies: user.iam_policies.clone(),
            permissions: user.permissions.clone(),
        };
        assert!(auth.can(S3Action::Read, "releases", "v1.zip"));
        assert!(!auth.can(S3Action::Delete, "releases", "v1.zip"));
        assert!(auth.can(S3Action::Delete, "uploads", "file.bin"));
    }

    #[test]
    fn test_user_in_multiple_groups() {
        let users = vec![IamUser {
            id: 1,
            name: "dev".into(),
            access_key_id: "AK1".into(),
            secret_access_key: "s".into(),
            enabled: true,
            created_at: String::new(),
            permissions: vec![],
            group_ids: vec![10, 20],
            auth_source: "local".into(),
            iam_policies: vec![],
        }];
        let groups = vec![
            Group {
                id: 10,
                name: "readers".into(),
                description: String::new(),
                permissions: vec![Permission {
                    id: 0,
                    effect: "Allow".into(),
                    actions: vec!["read".into(), "list".into()],
                    resources: vec!["*".into()],
                    conditions: None,
                }],
                member_ids: vec![1],
                created_at: String::new(),
            },
            Group {
                id: 20,
                name: "writers".into(),
                description: String::new(),
                permissions: vec![Permission {
                    id: 0,
                    effect: "Allow".into(),
                    actions: vec!["write".into()],
                    resources: vec!["uploads/*".into()],
                    conditions: None,
                }],
                member_ids: vec![1],
                created_at: String::new(),
            },
        ];
        let index = IamIndex::from_users_and_groups(users, groups);
        let user = index.get("AK1").unwrap();
        let auth = AuthenticatedUser {
            name: user.name.clone(),
            access_key_id: user.access_key_id.clone(),
            iam_policies: user.iam_policies.clone(),
            permissions: user.permissions.clone(),
        };
        assert!(auth.can(S3Action::Read, "bucket", "key"));
        assert!(auth.can(S3Action::List, "bucket", ""));
        assert!(auth.can(S3Action::Write, "uploads", "file.bin"));
        assert!(!auth.can(S3Action::Write, "releases", "v1.zip"));
        assert!(!auth.can(S3Action::Delete, "bucket", "key"));
    }

    #[test]
    fn test_group_permission_templates_expand_per_member_user() {
        let users = vec![
            IamUser {
                id: 1,
                name: "alice".into(),
                access_key_id: "AKALICE".into(),
                secret_access_key: "s".into(),
                enabled: true,
                created_at: String::new(),
                permissions: vec![],
                group_ids: vec![10],
                auth_source: "local".into(),
                iam_policies: vec![],
            },
            IamUser {
                id: 2,
                name: "bob".into(),
                access_key_id: "AKBOB".into(),
                secret_access_key: "s".into(),
                enabled: true,
                created_at: String::new(),
                permissions: vec![],
                group_ids: vec![10],
                auth_source: "local".into(),
                iam_policies: vec![],
            },
        ];
        let groups = vec![Group {
            id: 10,
            name: "home-readers".into(),
            description: String::new(),
            permissions: vec![Permission {
                id: 0,
                effect: "Allow".into(),
                actions: vec!["read".into()],
                resources: vec!["prod/home/${iam:username}/*".into()],
                conditions: None,
            }],
            member_ids: vec![1, 2],
            created_at: String::new(),
        }];
        let index = IamIndex::from_users_and_groups(users, groups);
        let alice = index.get("AKALICE").unwrap();
        let bob = index.get("AKBOB").unwrap();

        assert_eq!(alice.permissions[0].resources, vec!["prod/home/alice/*"]);
        assert_eq!(bob.permissions[0].resources, vec!["prod/home/bob/*"]);
    }

    fn boot() -> AuthConfig {
        AuthConfig {
            access_key_id: "AKBOOT".into(),
            secret_access_key: "s".into(),
        }
    }

    fn one_user() -> Vec<IamUser> {
        vec![IamUser {
            id: 1,
            name: "test".into(),
            access_key_id: "AK1".into(),
            secret_access_key: "s".into(),
            enabled: true,
            created_at: String::new(),
            permissions: vec![],
            group_ids: vec![],
            auth_source: "local".into(),
            iam_policies: vec![],
        }]
    }

    use EmptyIamOutcome::{DenyAll, Disabled, Legacy};

    /// The rule: the pair, else `authentication: none`, else deny all.
    #[test]
    fn empty_iam_outcome_truth_table() {
        assert_eq!(
            EmptyIamOutcome::from_parts(Some(boot()), true),
            Legacy(boot())
        );
        assert_eq!(
            EmptyIamOutcome::from_parts(Some(boot()), false),
            Legacy(boot())
        );
        assert_eq!(EmptyIamOutcome::from_parts(None, true), Disabled);
        assert_eq!(EmptyIamOutcome::from_parts(None, false), DenyAll);
    }

    /// An empty user set is the outcome itself; users carry it unchanged.
    #[test]
    fn when_empty_round_trips() {
        for outcome in [Legacy(boot()), Disabled, DenyAll] {
            let empty = IamIndex::build_iam_state(vec![], vec![], outcome.clone());
            assert_eq!(empty.when_empty(), outcome);
            let full = IamIndex::build_iam_state(one_user(), vec![], outcome.clone());
            assert!(matches!(full, IamState::Iam(_)));
            assert_eq!(full.when_empty(), outcome);
        }
    }

    /// A rebuild (user delete, peer sync) inherits the carried outcome.
    #[test]
    fn rebuild_keeps_the_carried_outcome() {
        let iam = IamIndex::build_iam_state(one_user(), vec![], Legacy(boot()));
        let empty = IamIndex::build_iam_state(vec![], vec![], iam.when_empty());
        assert!(matches!(empty, IamState::Legacy(a) if a.access_key_id == "AKBOOT"));
    }

    /// Review B1: started open, a user created, `authentication: none`
    /// removed, then the last user deleted: deny all, never open.
    #[test]
    fn hardening_then_last_delete_stays_closed() {
        let iam = IamIndex::build_iam_state(one_user(), vec![], Disabled);
        let hardened = iam.with_when_empty(DenyAll).expect("the outcome changed");
        assert!(hardened.has_iam_users(), "users stay");
        let empty = IamIndex::build_iam_state(vec![], vec![], hardened.when_empty());
        assert!(matches!(&empty, IamState::Iam(i) if i.is_empty()));
    }

    /// Review B1: in deny-all, `authentication: none` applies at once; a
    /// new pair too. An unchanged outcome publishes nothing.
    #[test]
    fn an_empty_state_takes_a_new_outcome_at_once() {
        let deny = IamState::deny_all();
        assert!(matches!(
            deny.with_when_empty(Disabled),
            Some(IamState::Disabled)
        ));
        assert!(matches!(
            deny.with_when_empty(Legacy(boot())),
            Some(IamState::Legacy(a)) if a.access_key_id == "AKBOOT"
        ));
        assert!(deny.with_when_empty(DenyAll).is_none());
        assert!(matches!(
            IamState::Legacy(boot()).with_when_empty(DenyAll),
            Some(IamState::Iam(i)) if i.is_empty()
        ));
    }

    /// A14: only a change from some credential to none is refused.
    #[test]
    fn check_lockout_table() {
        let pair = Legacy(boot());
        let surface = |users, when_empty| AuthSurface { users, when_empty };
        let refused = |b: AuthSurface<'_>, a: AuthSurface<'_>| check_lockout(b, a).is_err();
        // The last user deleted, no pair, no `none`.
        assert!(refused(surface(1, &DenyAll), surface(0, &DenyAll)));
        // The last user deleted with a pair or `none` left.
        assert!(!refused(surface(1, &pair), surface(0, &pair)));
        assert!(!refused(surface(1, &Disabled), surface(0, &Disabled)));
        // The pair removed: fine with users, refused without.
        assert!(!refused(surface(2, &pair), surface(2, &DenyAll)));
        assert!(refused(surface(0, &pair), surface(0, &DenyAll)));
        // `none` removed without users.
        assert!(refused(surface(0, &Disabled), surface(0, &DenyAll)));
        // Already deny-all (a peer sync emptied the DB): any change passes.
        assert!(!refused(surface(0, &DenyAll), surface(0, &DenyAll)));
        assert!(!refused(surface(0, &DenyAll), surface(0, &Disabled)));
        // D1: `none` set while users exist, the last user deleted: open.
        assert!(!refused(surface(1, &Disabled), surface(0, &Disabled)));
    }

    /// The boot and the lockout rule agree: a config the boot refuses
    /// (FATAL) is exactly a state with no credential, so an admin change
    /// can never reach a state the next boot refuses.
    #[test]
    fn the_boot_rule_and_the_lockout_rule_agree() {
        use crate::config::{AuthConfigOutcome, Config};
        let pairs: [(Option<&str>, Option<&str>); 4] = [
            (None, None),
            (Some("AK"), Some("SK")),
            (Some("AK"), None),
            (Some("AK"), Some("  ")),
        ];
        for (ak, sk) in pairs {
            for auth in [None, Some("none"), Some(" NONE "), Some("bogus")] {
                for users in [0usize, 1] {
                    let cfg = Config {
                        access_key_id: ak.map(str::to_string),
                        secret_access_key: sk.map(str::to_string),
                        authentication: auth.map(str::to_string),
                        ..Config::default()
                    };
                    let fatal = matches!(
                        cfg.classify_auth_config(users > 0),
                        AuthConfigOutcome::Missing | AuthConfigOutcome::UnrecognizedMode
                    );
                    let outcome = cfg.empty_iam_outcome();
                    let has = AuthSurface {
                        users,
                        when_empty: &outcome,
                    }
                    .has_credential();
                    assert_eq!(
                        fatal, !has,
                        "pair ({ak:?}, {sk:?}), authentication {auth:?}, users {users}"
                    );
                }
            }
        }
    }

    /// The outcome's Debug never prints the pair's secret.
    #[test]
    fn the_outcome_debug_hides_the_secret() {
        assert!(!format!("{:?}", Legacy(boot())).contains("AKBOOT"));
    }
}
