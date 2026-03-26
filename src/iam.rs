//! IAM: local user management with attribute-based access control (ABAC).
//!
//! Users are stored in an encrypted SQLCipher database (see `config_db.rs`).
//! At runtime, users are indexed in a `HashMap<access_key_id, IamUser>` for
//! O(1) lookup during SigV4 authentication.

use arc_swap::ArcSwap;
use axum::body::Body;
use axum::http::Request;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use rand::Rng;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{debug, warn};

/// Shared auth configuration extracted from Config at startup.
#[derive(Clone)]
pub struct AuthConfig {
    pub access_key_id: String,
    pub secret_access_key: String,
}

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

/// An IAM user with S3 credentials and permissions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IamUser {
    #[serde(default)]
    pub id: i64,
    pub name: String,
    pub access_key_id: String,
    #[serde(skip_serializing_if = "is_masked")]
    pub secret_access_key: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub permissions: Vec<Permission>,
}

fn is_masked(s: &str) -> bool {
    s == "****"
}

fn default_true() -> bool {
    true
}

/// A permission rule (Allow-only for v1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Permission {
    #[serde(default)]
    pub id: i64,
    /// Action verbs: "read", "write", "delete", "list", "admin", or "*"
    pub actions: Vec<String>,
    /// Resource patterns: "bucket/*", "bucket/prefix*", or "*"
    pub resources: Vec<String>,
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
}

/// Resolved identity after SigV4 authentication.
/// Inserted into request extensions by the SigV4 middleware.
#[derive(Debug, Clone)]
pub struct AuthenticatedUser {
    pub name: String,
    pub access_key_id: String,
    pub permissions: Vec<Permission>,
}

impl IamUser {
    /// Returns true if this user has admin-level permissions (actions contain "*" or "admin").
    pub fn is_admin(&self) -> bool {
        self.permissions
            .iter()
            .any(|p| p.actions.iter().any(|a| a == "*" || a == "admin"))
    }
}

/// Fast O(1) user lookup index, rebuilt from the database on load/sync.
pub struct IamIndex {
    users: HashMap<String, IamUser>,
}

impl IamIndex {
    /// Build the index from a list of users (keyed by access_key_id).
    /// Logs warnings for enabled users with no permissions (deny-by-default
    /// means they can authenticate but cannot access any resources).
    pub fn from_users(users: Vec<IamUser>) -> Self {
        let mut map = HashMap::with_capacity(users.len());
        for user in users {
            if user.enabled && user.permissions.is_empty() {
                warn!(
                    "IAM user '{}' ({}) is enabled but has no permissions — all operations will be denied",
                    user.name, user.access_key_id
                );
            }
            map.insert(user.access_key_id.clone(), user);
        }
        Self { users: map }
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
}

// === Permission Evaluation ===

/// Check if a user's permissions allow the given action on the given resource.
/// Allow-only evaluation (v1): any matching Allow rule permits the action.
/// No match = implicit deny.
pub fn evaluate_permissions(
    permissions: &[Permission],
    action: S3Action,
    bucket: &str,
    key: &str,
) -> bool {
    let action_str = action.as_str();

    for perm in permissions {
        // Check if this rule covers the action
        let action_matches = perm.actions.iter().any(|a| a == "*" || a == action_str);
        if !action_matches {
            continue;
        }

        // Check if this rule covers the resource
        let resource = if key.is_empty() {
            bucket.to_string()
        } else {
            format!("{}/{}", bucket, key)
        };

        let resource_matches = perm
            .resources
            .iter()
            .any(|pattern| matches_resource(pattern, &resource));

        if resource_matches {
            return true;
        }
    }

    false // implicit deny
}

/// Match a resource string against a pattern.
/// Patterns: "bucket/*" (prefix + bucket-level), "bucket/exact" (exact), "*" (everything).
/// "bucket/*" also matches the bucket itself (for list operations).
fn matches_resource(pattern: &str, resource: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        // "releases/*" matches "releases/v1.zip" AND "releases" (bucket-level)
        if resource.starts_with(prefix) {
            return true;
        }
        // Also match if pattern is "bucket/*" and resource is "bucket" (no trailing /)
        if let Some(bucket_prefix) = prefix.strip_suffix('/') {
            return resource == bucket_prefix;
        }
        false
    } else {
        resource == pattern
    }
}

// === Authorization Middleware ===

/// Map an HTTP method + path to an S3 action.
fn classify_action(method: &axum::http::Method, path: &str) -> S3Action {
    let is_bucket_level = path.trim_matches('/').split('/').count() <= 1;

    match *method {
        axum::http::Method::GET | axum::http::Method::HEAD => {
            if is_bucket_level {
                S3Action::List
            } else {
                S3Action::Read
            }
        }
        axum::http::Method::PUT => {
            if is_bucket_level {
                S3Action::Admin
            } else {
                S3Action::Write
            }
        }
        axum::http::Method::DELETE => {
            if is_bucket_level {
                S3Action::Admin
            } else {
                S3Action::Delete
            }
        }
        axum::http::Method::POST => {
            // POST is used for multipart uploads, batch delete, etc.
            // Check query string for ?delete (batch delete)
            S3Action::Write
        }
        _ => S3Action::Read,
    }
}

/// Extract bucket and key from the URI path (path-style: /{bucket}/{key...}).
fn parse_bucket_key(path: &str) -> (&str, &str) {
    let trimmed = path.trim_start_matches('/');
    match trimmed.split_once('/') {
        Some((bucket, key)) => (bucket, key),
        None => (trimmed, ""),
    }
}

/// Axum middleware that checks IAM permissions after SigV4 authentication.
///
/// If an `AuthenticatedUser` is present in request extensions (inserted by
/// the SigV4 middleware in IAM mode), evaluates their permissions against
/// the requested action and resource. Denies with 403 if not permitted.
///
/// In legacy mode or open access, no `AuthenticatedUser` is present and
/// the request passes through unchecked.
pub async fn authorization_middleware(
    request: Request<Body>,
    next: Next,
) -> Result<Response, Response> {
    // Only enforce if an AuthenticatedUser was inserted by SigV4 middleware
    let user = match request.extensions().get::<AuthenticatedUser>() {
        Some(u) => u.clone(),
        None => return Ok(next.run(request).await),
    };

    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let query = request.uri().query().unwrap_or("");

    // Determine the S3 action
    let mut action = classify_action(&method, &path);

    // POST /{bucket}?delete is a batch DELETE, not a write.
    // Must check for exact "delete" query parameter, not substring
    // (otherwise ?delimiter= would also match).
    if method == axum::http::Method::POST
        && query
            .split('&')
            .any(|p| p == "delete" || p.starts_with("delete="))
    {
        action = S3Action::Delete;
    }

    let (bucket, key) = parse_bucket_key(&path);

    if !evaluate_permissions(&user.permissions, action, bucket, key) {
        debug!(
            "IAM denied: user='{}' action={:?} bucket='{}' key='{}'",
            user.name, action, bucket, key
        );
        return Err(crate::api::S3Error::AccessDenied.into_response());
    }

    debug!(
        "IAM allowed: user='{}' action={:?} bucket='{}' key='{}'",
        user.name, action, bucket, key
    );

    Ok(next.run(request).await)
}

// === Key Generation ===

/// Generate an AWS-like access key ID (20 chars: "AK" + 18 uppercase alphanumeric).
pub fn generate_access_key_id() -> String {
    let mut rng = rand::thread_rng();
    let chars: Vec<char> = (0..18)
        .map(|_| {
            let idx = rng.gen_range(0..36);
            if idx < 10 {
                (b'0' + idx) as char
            } else {
                (b'A' + idx - 10) as char
            }
        })
        .collect();
    format!("AK{}", chars.iter().collect::<String>())
}

/// Generate an AWS-like secret access key (40 chars, base64-alphabet).
pub fn generate_secret_access_key() -> String {
    let mut rng = rand::thread_rng();
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    (0..40)
        .map(|_| ALPHABET[rng.gen_range(0..ALPHABET.len())] as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_evaluate_permissions_allow_read() {
        let perms = vec![Permission {
            id: 0,
            actions: vec!["read".into()],
            resources: vec!["releases/*".into()],
        }];

        assert!(evaluate_permissions(
            &perms,
            S3Action::Read,
            "releases",
            "v1.zip"
        ));
        assert!(!evaluate_permissions(
            &perms,
            S3Action::Write,
            "releases",
            "v1.zip"
        ));
        assert!(!evaluate_permissions(
            &perms,
            S3Action::Read,
            "other-bucket",
            "file.txt"
        ));
    }

    #[test]
    fn test_evaluate_permissions_wildcard_action() {
        let perms = vec![Permission {
            id: 0,
            actions: vec!["*".into()],
            resources: vec!["*".into()],
        }];

        assert!(evaluate_permissions(&perms, S3Action::Read, "any", "key"));
        assert!(evaluate_permissions(&perms, S3Action::Delete, "any", "key"));
        assert!(evaluate_permissions(&perms, S3Action::Admin, "any", ""));
    }

    #[test]
    fn test_evaluate_permissions_no_permissions_denies() {
        let perms: Vec<Permission> = vec![];
        assert!(!evaluate_permissions(
            &perms,
            S3Action::Read,
            "bucket",
            "key"
        ));
    }

    #[test]
    fn test_evaluate_permissions_multiple_rules() {
        let perms = vec![
            Permission {
                id: 0,
                actions: vec!["read".into(), "list".into()],
                resources: vec!["releases/*".into()],
            },
            Permission {
                id: 0,
                actions: vec!["write".into()],
                resources: vec!["uploads/*".into()],
            },
        ];

        assert!(evaluate_permissions(
            &perms,
            S3Action::Read,
            "releases",
            "v1.zip"
        ));
        assert!(evaluate_permissions(&perms, S3Action::List, "releases", ""));
        assert!(evaluate_permissions(
            &perms,
            S3Action::Write,
            "uploads",
            "file.bin"
        ));
        assert!(!evaluate_permissions(
            &perms,
            S3Action::Write,
            "releases",
            "v1.zip"
        ));
        assert!(!evaluate_permissions(
            &perms,
            S3Action::Delete,
            "releases",
            "v1.zip"
        ));
    }

    #[test]
    fn test_evaluate_permissions_exact_resource() {
        let perms = vec![Permission {
            id: 0,
            actions: vec!["read".into()],
            resources: vec!["specific-bucket/exact-key.txt".into()],
        }];

        assert!(evaluate_permissions(
            &perms,
            S3Action::Read,
            "specific-bucket",
            "exact-key.txt"
        ));
        assert!(!evaluate_permissions(
            &perms,
            S3Action::Read,
            "specific-bucket",
            "other-key.txt"
        ));
    }

    #[test]
    fn test_evaluate_permissions_bucket_level() {
        let perms = vec![Permission {
            id: 0,
            actions: vec!["list".into()],
            resources: vec!["my-bucket".into()],
        }];

        // Bucket-level operation (empty key)
        assert!(evaluate_permissions(
            &perms,
            S3Action::List,
            "my-bucket",
            ""
        ));
        // Key-level operation in same bucket — doesn't match exact "my-bucket"
        assert!(!evaluate_permissions(
            &perms,
            S3Action::List,
            "my-bucket",
            "prefix/"
        ));
    }

    #[test]
    fn test_evaluate_permissions_bucket_wildcard() {
        let perms = vec![Permission {
            id: 0,
            actions: vec!["list".into(), "read".into()],
            resources: vec!["my-bucket/*".into()],
        }];

        // "my-bucket/*" should match keys inside the bucket
        assert!(evaluate_permissions(
            &perms,
            S3Action::Read,
            "my-bucket",
            "file.txt"
        ));
        // AND bucket-level (listing the bucket itself)
        assert!(evaluate_permissions(
            &perms,
            S3Action::List,
            "my-bucket",
            ""
        ));
    }

    #[test]
    fn test_matches_resource_patterns() {
        assert!(matches_resource("*", "anything/at/all"));
        assert!(matches_resource("releases/*", "releases/v1.zip"));
        assert!(matches_resource("releases/*", "releases/sub/dir/file"));
        assert!(!matches_resource("releases/*", "other/file"));
        assert!(matches_resource("exact", "exact"));
        assert!(!matches_resource("exact", "not-exact"));
    }

    #[test]
    fn test_generate_access_key_id_format() {
        let key = generate_access_key_id();
        assert_eq!(key.len(), 20);
        assert!(key.starts_with("AK"));
        assert!(key[2..]
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()));
    }

    #[test]
    fn test_generate_secret_access_key_format() {
        let key = generate_secret_access_key();
        assert_eq!(key.len(), 40);
        assert!(key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/'));
    }

    #[test]
    fn test_generate_keys_are_unique() {
        let k1 = generate_access_key_id();
        let k2 = generate_access_key_id();
        assert_ne!(k1, k2);

        let s1 = generate_secret_access_key();
        let s2 = generate_secret_access_key();
        assert_ne!(s1, s2);
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
            },
            IamUser {
                id: 2,
                name: "viewer".into(),
                access_key_id: "AKVIEW01".into(),
                secret_access_key: "secret2".into(),
                enabled: false,
                created_at: String::new(),
                permissions: vec![],
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
}
