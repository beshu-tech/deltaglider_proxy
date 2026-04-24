//! IAM type definitions: users, groups, permissions, actions, and authenticated identity.

use iam_rs::IAMPolicy;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::permissions;

/// Shared auth configuration extracted from Config at startup.
#[derive(Clone)]
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

/// Resolved identity after SigV4 authentication.
/// Inserted into request extensions by the SigV4 middleware.
#[derive(Debug, Clone)]
pub struct AuthenticatedUser {
    pub name: String,
    pub access_key_id: String,
    pub permissions: Vec<Permission>,
    /// Precomputed IAM policies for iam-rs evaluation (includes conditions support).
    pub iam_policies: Vec<IAMPolicy>,
}

impl AuthenticatedUser {
    /// Check if this user is allowed to perform the given action on the given resource.
    /// Uses iam-rs for evaluation when policies are available (supports conditions),
    /// falls back to legacy evaluation otherwise.
    pub fn can(&self, action: S3Action, bucket: &str, key: &str) -> bool {
        if !self.iam_policies.is_empty() {
            permissions::evaluate_iam(&self.iam_policies, action, bucket, key, &Default::default())
        } else {
            permissions::evaluate(&self.permissions, action, bucket, key)
        }
    }

    /// Check with request context (s3:prefix, aws:SourceIp, etc.).
    /// Used by the authorization middleware to pass conditions from the HTTP request.
    pub fn can_with_context(
        &self,
        action: S3Action,
        bucket: &str,
        key: &str,
        context: &iam_rs::Context,
    ) -> bool {
        if !self.iam_policies.is_empty() {
            permissions::evaluate_iam(&self.iam_policies, action, bucket, key, context)
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
            permissions::is_explicitly_denied_iam(&self.iam_policies, action, bucket, key, context)
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

    /// Returns true if this user has full admin permissions.
    pub fn is_admin(&self) -> bool {
        permissions::is_admin(&self.permissions)
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
/// the handler to filter the response through `user.can(Read|List, bucket, key)`.
#[derive(Debug, Clone)]
pub enum ListScope {
    /// The caller's policy authorises every key under the requested
    /// prefix. No per-key filtering needed.
    Unrestricted,
    /// The caller was admitted via `can_see_bucket` fallback (or has
    /// prefix-scoped permissions that don't cover the requested prefix
    /// in full). The handler MUST filter returned keys by
    /// `user.can(Read|List, bucket, key)`.
    Filtered {
        /// The authenticated user, captured at authorization time so
        /// the filter uses the exact same policy set.
        user: Box<AuthenticatedUser>,
    },
}

impl IamUser {
    /// Returns true if this user has full admin permissions:
    /// actions must contain "*" or "admin", AND resources must contain "*".
    /// A user with actions=["*"] on a specific bucket is NOT considered admin.
    pub fn is_admin(&self) -> bool {
        permissions::is_admin(&self.permissions)
    }
}
