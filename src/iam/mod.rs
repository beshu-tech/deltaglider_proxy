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

pub mod external_auth;
pub mod keygen;
pub mod middleware;
pub mod permissions;
pub mod types;

use arc_swap::ArcSwap;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::warn;

// Re-export everything at crate::iam level for backward compatibility
pub use keygen::{generate_access_key_id, generate_secret_access_key};
pub use middleware::authorization_middleware;
pub use permissions::{normalize_permissions, validate_permissions};
pub use types::*;

/// Runtime IAM state — supports legacy single-credential mode and multi-user IAM.
pub enum IamState {
    /// No auth configured — open access.
    Disabled,
    /// Legacy single credential pair (backward compatible with old config).
    Legacy(AuthConfig),
    /// Multi-user IAM with per-user credentials and permissions.
    Iam(IamIndex),
}

/// Thread-safe, hot-swappable IAM state.
pub type SharedIamState = Arc<ArcSwap<IamState>>;

/// Fast O(1) user lookup index, rebuilt from the database on load/sync.
pub struct IamIndex {
    users: HashMap<String, IamUser>,
    groups: Vec<Group>,
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
        Self { users: map, groups }
    }

    /// Look up a user by access_key_id. O(1).
    pub fn get(&self, access_key_id: &str) -> Option<&IamUser> {
        self.users.get(access_key_id)
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

    /// Build IAM state from users and groups.
    /// Returns `Iam(index)` if users exist, `Disabled` otherwise.
    pub fn build_iam_state(users: Vec<IamUser>, groups: Vec<Group>) -> IamState {
        if users.is_empty() {
            return IamState::Disabled;
        }
        IamState::Iam(Self::from_users_and_groups(users, groups))
    }
}

/// Return predefined policy templates for the admin UI.
pub fn canned_policies() -> Vec<CannedPolicy> {
    vec![
        CannedPolicy {
            name: "Full Access",
            description: "All S3 operations on all resources",
            permissions: vec![Permission {
                id: 0,
                effect: "Allow".into(),
                actions: vec!["*".into()],
                resources: vec!["*".into()],
                conditions: None,
            }],
        },
        CannedPolicy {
            name: "Read Only",
            description: "Read and list all resources",
            permissions: vec![Permission {
                id: 0,
                effect: "Allow".into(),
                actions: vec!["read".into(), "list".into()],
                resources: vec!["*".into()],
                conditions: None,
            }],
        },
        CannedPolicy {
            name: "Read/Write",
            description: "Read, write, and list all resources",
            permissions: vec![Permission {
                id: 0,
                effect: "Allow".into(),
                actions: vec!["read".into(), "write".into(), "list".into()],
                resources: vec!["*".into()],
                conditions: None,
            }],
        },
        CannedPolicy {
            name: "Read/Write (No Delete)",
            description: "Full access except delete operations are denied",
            permissions: vec![
                Permission {
                    id: 0,
                    effect: "Allow".into(),
                    actions: vec!["*".into()],
                    resources: vec!["*".into()],
                    conditions: None,
                },
                Permission {
                    id: 0,
                    effect: "Deny".into(),
                    actions: vec!["delete".into()],
                    resources: vec!["*".into()],
                    conditions: None,
                },
            ],
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
    fn test_build_iam_state_empty_users() {
        let state = IamIndex::build_iam_state(vec![], vec![]);
        assert!(matches!(state, IamState::Disabled));
    }

    #[test]
    fn test_build_iam_state_with_users() {
        let users = vec![IamUser {
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
        }];
        let state = IamIndex::build_iam_state(users, vec![]);
        assert!(matches!(state, IamState::Iam(_)));
    }
}
