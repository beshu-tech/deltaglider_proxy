// SPDX-License-Identifier: BUSL-1.1

//! IAM type definitions: users, groups, permissions, actions, and authenticated identity.

use iam_rs::IAMPolicy;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use super::permissions;

impl From<&IamUser> for AuthenticatedUser {
    /// A deep copy of the user's identity and policies. Built once per user
    /// at IAM index build ([`super::IamIndex::principal`]); a request takes
    /// the index's shared copy and never builds one.
    fn from(user: &IamUser) -> Self {
        Self::from_parts(
            user.name.clone(),
            user.access_key_id.clone(),
            user.permissions.clone(),
            user.iam_policies.clone(),
        )
    }
}

/// Shared auth configuration extracted from Config at startup.
#[derive(Clone, PartialEq, Eq)]
pub struct AuthConfig {
    pub access_key_id: String,
    pub secret_access_key: String,
}

/// An IAM user with S3 credentials and permissions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IamUser {
    #[serde(default)]
    pub id: i64,
    pub name: String,
    pub access_key_id: String,
    #[serde(skip_serializing_if = "is_masked")]
    pub secret_access_key: String,
    #[serde(default = "crate::types::default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub permissions: Vec<Permission>,
    #[serde(default)]
    pub group_ids: Vec<i64>,
    /// How this user was created: "local" (manually) or "external" (auto-provisioned via OAuth).
    #[serde(default = "default_local")]
    pub auth_source: String,
    /// Precomputed IAM policies from permissions (built at index time, not serialized).
    #[serde(skip)]
    pub iam_policies: Vec<IAMPolicy>,
}

fn default_local() -> String {
    "local".to_string()
}

/// An IAM group with permissions and member user IDs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Group {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub permissions: Vec<Permission>,
    #[serde(default)]
    pub member_ids: Vec<i64>,
    #[serde(default)]
    pub created_at: String,
}

fn is_masked(s: &str) -> bool {
    s == "****"
}

/// Default effect for permissions (Allow).
fn default_allow() -> String {
    "Allow".to_string()
}

/// A permission rule with Allow/Deny effect and optional conditions.
///
/// `PartialEq` is included so declarative-IAM diff logic can compare
/// permissions structurally; `JsonSchema` surfaces this type in the
/// auto-generated schema exposed by the admin API.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Permission {
    #[serde(default)]
    pub id: i64,
    /// "Allow" or "Deny" — Deny rules override Allow rules.
    #[serde(default = "default_allow")]
    pub effect: String,
    /// Action verbs: "read", "write", "delete", "list", "admin", or "*"
    pub actions: Vec<String>,
    /// Resource patterns: "bucket/*", "bucket/prefix*", or "*"
    pub resources: Vec<String>,
    /// Optional AWS IAM Condition block (e.g. `{"StringLike": {"s3:prefix": "builds/*"}}`)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conditions: Option<serde_json::Value>,
}

/// S3 action categories mapped from HTTP methods.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum S3Action {
    Read,   // GET object, HEAD object
    Write,  // PUT object, POST multipart
    Delete, // DELETE object, POST ?delete (batch)
    List,   // GET bucket (ListObjects), GET / (ListBuckets)
    Admin,  // PUT bucket (CreateBucket), DELETE bucket
}

impl S3Action {
    /// String representation for matching against permission action verbs.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Delete => "delete",
            Self::List => "list",
            Self::Admin => "admin",
        }
    }

    /// Map to standard AWS IAM S3 action string.
    pub fn to_iam_action(&self) -> &'static str {
        match self {
            Self::Read => "s3:GetObject",
            Self::Write => "s3:PutObject",
            Self::Delete => "s3:DeleteObject",
            Self::List => "s3:ListBucket",
            Self::Admin => "s3:CreateBucket",
        }
    }
}

/// Name of the principal synthesized for unauthenticated requests that hit a
/// public prefix (see `api/auth.rs`). Compared by [`AuthenticatedUser::is_anonymous`].
pub const ANONYMOUS_USER_NAME: &str = "$anonymous";

/// Resolved identity after SigV4 authentication.
///
/// Shared, never copied: the IAM index builds one per user, the bootstrap
/// principal is built once per access key, and a request carries an
/// `Arc` of it in its extensions ([`insert_request_principal`] /
/// [`request_principal`]). It is deliberately not `Clone`: a deep copy per
/// request cost 2-10 µs, and without `Clone` the type cannot be put into
/// request extensions by value (`Extensions::insert` needs `Clone`).
#[derive(Debug)]
pub struct AuthenticatedUser {
    pub name: String,
    pub access_key_id: String,
    /// Read through [`Self::permissions`]. Private: the compiled policy is
    /// built from these at construction and must not drift from them.
    permissions: Vec<Permission>,
    /// Precomputed IAM policies for iam-rs evaluation (includes conditions
    /// support). Read through [`Self::iam_policies`].
    iam_policies: Vec<IAMPolicy>,
    /// `iam_policies`, compiled once ([`super::compiled::CompiledPolicy`]).
    compiled: super::compiled::CompiledPolicy,
}

/// `$`-prefixed user names are reserved for synthetic principals
/// (`$anonymous`, `$bootstrap`). Every path that names a stored user refuses
/// them, or (OAuth auto-provisioning, where the IdP picks the name) strips
/// them with [`strip_reserved_principal_prefix`].
pub fn is_reserved_principal_name(name: &str) -> bool {
    name.starts_with('$')
}

/// A blank user or group name names nothing: the GUI cannot select it, and
/// `${iam:username}` would expand to "". The admin API refuses it on create
/// and rename. (Declarative YAML and backup restore still accept one, so an
/// existing deployment keeps booting.)
pub fn is_blank_principal_name(name: &str) -> bool {
    name.trim().is_empty()
}

/// `name` with every reserved prefix removed, so that
/// `is_reserved_principal_name` is false for the result.
pub fn strip_reserved_principal_prefix(name: &str) -> &str {
    let mut name = name;
    while is_reserved_principal_name(name) {
        name = &name[1..];
    }
    name
}

/// Principal name of the bootstrap (single-credential, legacy-mode) user.
pub const BOOTSTRAP_USER_NAME: &str = "$bootstrap";

/// The principal the SigV4 middleware resolved for this request, if any.
/// THE reader of the slot: a reader that asked for another type
/// (`AuthenticatedUser` by value) would find nothing, and "no principal"
/// means "not checked" to the authorization middleware.
pub fn request_principal(ext: &axum::http::Extensions) -> Option<&Arc<AuthenticatedUser>> {
    ext.get::<Arc<AuthenticatedUser>>()
}

/// Put the resolved principal into the request's extensions (one `Arc`
/// clone, no copy). THE writer of the slot that [`request_principal`] reads.
pub fn insert_request_principal(ext: &mut axum::http::Extensions, user: Arc<AuthenticatedUser>) {
    ext.insert(user);
}

impl AuthenticatedUser {
    /// A principal whose IAM policies are built from its permissions.
    pub fn new(
        name: impl Into<String>,
        access_key_id: impl Into<String>,
        permissions: Vec<Permission>,
    ) -> Self {
        let iam_policies = permissions
            .iter()
            .map(permissions::permission_to_iam_policy)
            .collect();
        Self::from_parts(name.into(), access_key_id.into(), permissions, iam_policies)
    }

    /// A principal from permissions and their already-built IAM policies.
    pub fn from_parts(
        name: String,
        access_key_id: String,
        permissions: Vec<Permission>,
        iam_policies: Vec<IAMPolicy>,
    ) -> Self {
        let compiled = super::compiled::CompiledPolicy::new(&iam_policies);
        Self {
            name,
            access_key_id,
            permissions,
            iam_policies,
            compiled,
        }
    }

    /// The effective permissions (direct, group-inherited, templates expanded).
    pub fn permissions(&self) -> &[Permission] {
        &self.permissions
    }

    /// The IAM policies built from [`Self::permissions`].
    pub fn iam_policies(&self) -> &[IAMPolicy] {
        &self.iam_policies
    }

    /// The bootstrap principal: full access to everything. THE one definition,
    /// shared by SigV4 auth and browser form-POST, so the two cannot drift
    /// into different privileges.
    pub fn bootstrap(access_key_id: &str) -> Self {
        Self::new(
            BOOTSTRAP_USER_NAME,
            access_key_id,
            vec![Permission {
                id: 0,
                effect: "Allow".to_string(),
                actions: vec!["*".to_string()],
                resources: vec!["*".to_string()],
                conditions: None,
            }],
        )
    }

    /// [`Self::bootstrap`], built once per bootstrap access key and shared:
    /// a request takes an `Arc` clone. A changed key (config apply) builds
    /// the next one.
    pub fn bootstrap_shared(access_key_id: &str) -> Arc<Self> {
        static CURRENT: arc_swap::ArcSwapOption<AuthenticatedUser> =
            arc_swap::ArcSwapOption::const_empty();
        if let Some(user) = CURRENT.load_full() {
            if user.access_key_id == access_key_id {
                return user;
            }
        }
        let user = Arc::new(Self::bootstrap(access_key_id));
        CURRENT.store(Some(user.clone()));
        user
    }

    /// The synthesized public-prefix reader (`$anonymous`). Anonymous callers
    /// get the bytes they are allowed to read, never deployment provenance.
    ///
    /// Structural, not by name alone: the synthetic principal is the only one
    /// with no access key. A stored IAM user always has one, so a user who is
    /// NAMED `$anonymous` (a reserved name that slipped in) is not anonymous
    /// and never skips signature checks.
    pub fn is_anonymous(&self) -> bool {
        self.name == ANONYMOUS_USER_NAME && self.access_key_id.is_empty()
    }

    /// Context-free check, for TESTS only. With no request context iam-rs
    /// skips every conditioned statement, so an `aws:SourceIp` Deny never
    /// fires; on an authz path that is a bypass (S14, class 2). Production
    /// code must call [`Self::can_with_context`]; `cfg(test)` makes the
    /// compiler refuse any other use.
    #[cfg(test)]
    pub fn can(&self, action: S3Action, bucket: &str, key: &str) -> bool {
        if !self.iam_policies.is_empty() {
            permissions::evaluate_compiled(&self.compiled, action, bucket, key, &Default::default())
        } else {
            permissions::evaluate(&self.permissions, action, bucket, key)
        }
    }

    /// THE authorization check: action on bucket/key under the request's
    /// policy context (`aws:SourceIp`, `s3:prefix`, ...).
    pub fn can_with_context(
        &self,
        action: S3Action,
        bucket: &str,
        key: &str,
        context: &iam_rs::Context,
    ) -> bool {
        if !self.iam_policies.is_empty() {
            permissions::evaluate_compiled(&self.compiled, action, bucket, key, context)
        } else {
            // Legacy path — no conditions support, ignore context
            permissions::evaluate(&self.permissions, action, bucket, key)
        }
    }

    /// Check if an explicit Deny rule matches (including conditions).
    /// Used to distinguish "no matching Allow" from "explicitly denied" for LIST fallback logic.
    pub fn is_explicitly_denied(
        &self,
        action: S3Action,
        bucket: &str,
        key: &str,
        context: &iam_rs::Context,
    ) -> bool {
        if !self.iam_policies.is_empty() {
            permissions::is_explicitly_denied_compiled(&self.compiled, action, bucket, key, context)
        } else {
            // Legacy: check if any Deny rule matches
            permissions::has_matching_deny(&self.permissions, action, bucket, key)
        }
    }

    /// Check if this user should see the given bucket in ListBuckets.
    /// A user with "my-bucket/prefix/*" should see "my-bucket" in the list.
    /// Ignores Deny rules for visibility (deny only blocks actions, not bucket discovery).
    pub fn can_see_bucket(&self, bucket: &str) -> bool {
        permissions::has_any_on_bucket(&self.permissions, bucket)
    }

    /// Returns true if any of this user's permissions have conditions attached.
    pub fn has_any_conditions(&self) -> bool {
        self.permissions.iter().any(|p| p.conditions.is_some())
    }

    /// Returns true if this user has full admin permissions for a request
    /// from `client_ip` (conditions on the admin rule must hold).
    pub fn is_admin(&self, client_ip: Option<std::net::IpAddr>) -> bool {
        permissions::is_admin(&self.permissions, client_ip)
    }
}

/// Post-authorization signal to the ListObjects handler indicating
/// whether the caller's permission covered the entire bucket/prefix
/// (= `Unrestricted`) or only a subset (= `Filtered`).
///
/// Inserted into request extensions by the authorization middleware for
/// LIST requests. The handler uses it to decide whether to filter the
/// engine's returned keys by per-key permission.
///
/// Why this lives here, not in the handler: the middleware is where
/// full policy context is already resolved (`s3:prefix`, deny chain,
/// `can_see_bucket` fallback). Recomputing that in the handler would
/// duplicate logic and risk drift. The handler just reads the signal.
///
/// Background: pre-C1-security-fix, a user with a prefix-scoped
/// permission like `bucket/alice/*` was allowed to call
/// `GET /bucket?prefix=` (empty) via the `can_see_bucket` fallback,
/// and the handler returned every key in the bucket — including keys
/// outside alice/. `ListScope::Filtered` closes that bypass by forcing
/// the handler to filter the response through `user_can_see_listed_key`
/// (per-key Read|List under the request context).
#[derive(Debug, Clone)]
pub enum ListScope {
    /// The caller's policy authorises every key under the requested
    /// prefix. No per-key filtering needed.
    Unrestricted,
    /// The caller was admitted via `can_see_bucket` fallback (or has
    /// prefix-scoped permissions that don't cover the requested prefix
    /// in full). The handler MUST filter returned keys by
    /// `user_can_see_listed_key` with `context`.
    Filtered {
        /// The authenticated user, captured at authorization time so
        /// the filter uses the exact same policy set.
        user: Arc<AuthenticatedUser>,
        /// The request's policy context (`aws:SourceIp`, ...) WITHOUT
        /// `s3:prefix`, for the per-key checks. Without it an IP-conditioned
        /// Deny never fires in the filter.
        context: Box<iam_rs::Context>,
    },
}

impl IamUser {
    /// Returns true if this user has full admin permissions:
    /// actions must contain "*" or "admin", AND resources must contain "*".
    /// A user with actions=["*"] on a specific bucket is NOT considered admin.
    /// Conditions on the admin rule must hold for a request from `client_ip`.
    pub fn is_admin(&self, client_ip: Option<std::net::IpAddr>) -> bool {
        permissions::is_admin(&self.permissions, client_ip)
    }
}

#[cfg(test)]
mod principal_tests {
    use super::*;

    /// Only the synthetic principal (no access key) is anonymous. A stored
    /// user who is merely NAMED `$anonymous` keeps full signature checks.
    #[test]
    fn anonymous_is_structural_not_a_name() {
        let named = |name: &str, ak: &str| {
            AuthenticatedUser::from_parts(name.into(), ak.into(), vec![], vec![])
        };
        assert!(named(ANONYMOUS_USER_NAME, "").is_anonymous());
        assert!(!named(ANONYMOUS_USER_NAME, "AKREAL").is_anonymous());
        assert!(!named("alice", "").is_anonymous());
        assert!(is_blank_principal_name(""));
        assert!(is_blank_principal_name(" \t"));
        assert!(!is_blank_principal_name("dana"));
        assert!(is_reserved_principal_name("$anonymous"));
        assert!(is_reserved_principal_name("$x"));
        assert!(!is_reserved_principal_name("dana$"));
        assert_eq!(strip_reserved_principal_prefix("$$anonymous"), "anonymous");
        assert_eq!(strip_reserved_principal_prefix("dana"), "dana");
        assert_eq!(strip_reserved_principal_prefix("$"), "");
    }

    /// The bootstrap principal is full access. SigV4 and form-POST both build
    /// it here, so this pins the privilege both paths get.
    #[test]
    fn bootstrap_has_full_access() {
        let user = AuthenticatedUser::bootstrap("AKBOOT");
        assert_eq!(user.name, BOOTSTRAP_USER_NAME);
        assert_eq!(user.access_key_id, "AKBOOT");
        assert!(user.is_admin(None));
        for action in [
            S3Action::Read,
            S3Action::Write,
            S3Action::Delete,
            S3Action::List,
        ] {
            assert!(user.can(action, "releases", "any/key.zip"), "{action:?}");
        }
    }

    #[test]
    fn from_iam_user_copies_identity_and_policies() {
        let mut iam: IamUser = serde_json::from_value(serde_json::json!({
            "name": "ci-uploader",
            "access_key_id": "AKCI",
            "secret_access_key": "secret",
            "permissions": [{ "actions": ["read"], "resources": ["releases/*"] }],
        }))
        .unwrap();
        iam.iam_policies = iam
            .permissions
            .iter()
            .map(permissions::permission_to_iam_policy)
            .collect();
        let user = AuthenticatedUser::from(&iam);
        assert_eq!(user.name, "ci-uploader");
        assert_eq!(user.access_key_id, "AKCI");
        assert!(user.can(S3Action::Read, "releases", "v1.zip"));
        assert!(!user.can(S3Action::Write, "releases", "v1.zip"));
    }
}
