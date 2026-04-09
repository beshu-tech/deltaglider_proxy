//! Per-bucket policy configuration and public prefix access control.
//!
//! Each bucket can override global compression settings, route to a
//! specific named backend, and expose key prefixes for unauthenticated
//! read-only access.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Per-bucket policy overrides. All fields are optional — `None` means
/// "use the global default".
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct BucketPolicyConfig {
    /// Enable/disable delta compression for this bucket.
    /// When `false`, all files in this bucket are stored as passthrough
    /// regardless of file type or size.
    #[serde(default)]
    pub compression: Option<bool>,

    /// Override the global `max_delta_ratio` for this bucket.
    /// Delta is kept only if `delta_size / original_size < ratio`.
    #[serde(default)]
    pub max_delta_ratio: Option<f32>,

    /// Route this bucket to a specific named backend.
    /// When `None`, uses the default backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,

    /// Map this virtual bucket name to a different real bucket on the backend.
    /// Example: virtual "archive" → real "prod-archive-2024" on backend "hetzner".
    /// When `None`, the virtual bucket name equals the real bucket name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,

    /// Key prefixes allowing unauthenticated read-only access (GET, HEAD, LIST).
    /// Example: `["builds/", "releases/v2/"]` allows anonymous download of all
    /// objects under those prefixes. Empty vec (default) = no public access.
    /// Use trailing `/` to ensure directory-aligned matching.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub public_prefixes: Vec<String>,
}

/// Resolved bucket policies with global defaults applied.
/// Constructed from `Config` at engine startup.
pub struct BucketPolicyRegistry {
    policies: HashMap<String, BucketPolicyConfig>,
    default_compression: bool,
    default_max_delta_ratio: f32,
}

impl BucketPolicyRegistry {
    /// Create a registry from per-bucket configs and global defaults.
    pub fn new(
        policies: HashMap<String, BucketPolicyConfig>,
        default_max_delta_ratio: f32,
    ) -> Self {
        // Normalize bucket names to lowercase and validate ratio values + public prefixes
        let policies = policies
            .into_iter()
            .map(|(k, mut v)| {
                if let Some(ratio) = v.max_delta_ratio {
                    if !(0.0..=1.0).contains(&ratio) {
                        tracing::warn!(
                            "Bucket '{}' has invalid max_delta_ratio {:.2} (must be 0.0-1.0), ignoring override",
                            k, ratio
                        );
                        v.max_delta_ratio = None;
                    }
                }
                // Validate and normalize public prefixes
                if !v.public_prefixes.is_empty() {
                    let original_count = v.public_prefixes.len();
                    v.public_prefixes = v
                        .public_prefixes
                        .into_iter()
                        .filter_map(|mut p| {
                            // Strip leading slash
                            if p.starts_with('/') {
                                p = p[1..].to_string();
                            }
                            // Reject dangerous patterns
                            if p.contains("..") || p.contains('\0') || p.contains("//") {
                                tracing::warn!(
                                    "Bucket '{}': rejecting invalid public_prefix '{}' (contains .., null, or //)",
                                    k, p
                                );
                                return None;
                            }
                            // Warn about entire-bucket exposure
                            if p.is_empty() {
                                tracing::warn!(
                                    "Bucket '{}': public_prefix is empty string — the ENTIRE bucket is publicly readable!",
                                    k
                                );
                            }
                            // Warn about missing trailing slash (easy misconfiguration)
                            if !p.is_empty() && !p.ends_with('/') {
                                tracing::warn!(
                                    "Bucket '{}': public_prefix '{}' has no trailing '/'. \
                                     This matches '{}anything' — add '{}/' if you meant a directory.",
                                    k, p, p, p
                                );
                            }
                            Some(p)
                        })
                        .collect();
                    v.public_prefixes.sort();
                    v.public_prefixes.dedup();
                    let valid_count = v.public_prefixes.len();
                    if valid_count > 0 {
                        tracing::info!(
                            "Bucket '{}' has {} public prefix(es): {:?}",
                            k,
                            valid_count,
                            v.public_prefixes
                        );
                    }
                    if valid_count < original_count {
                        tracing::warn!(
                            "Bucket '{}': {} public prefix(es) rejected during validation",
                            k,
                            original_count - valid_count
                        );
                    }
                }
                (k.to_ascii_lowercase(), v)
            })
            .collect();
        Self {
            policies,
            default_compression: true,
            default_max_delta_ratio,
        }
    }

    /// Whether delta compression is enabled for this bucket.
    pub fn compression_enabled(&self, bucket: &str) -> bool {
        self.policies
            .get(bucket)
            .and_then(|p| p.compression)
            .unwrap_or(self.default_compression)
    }

    /// The max delta ratio for this bucket (per-bucket override or global).
    /// When a bucket has compression explicitly enabled but no threshold set,
    /// and the global ratio is 0 (compression disabled globally), use 0.75
    /// as a sensible default — otherwise the per-bucket compression flag
    /// would have no effect.
    pub fn max_delta_ratio(&self, bucket: &str) -> f32 {
        if let Some(policy) = self.policies.get(bucket) {
            if let Some(ratio) = policy.max_delta_ratio {
                return ratio;
            }
            // Per-bucket compression explicitly ON, but no threshold set
            // and global is 0 (disabled) → use sensible default
            if policy.compression == Some(true) && self.default_max_delta_ratio == 0.0 {
                return 0.75;
            }
        }
        self.default_max_delta_ratio
    }

    /// Resolve routing for a bucket: returns (backend_name, real_bucket_name).
    /// `None` backend means use the default backend.
    pub fn resolve_backend<'a>(&'a self, bucket: &'a str) -> (Option<&'a str>, &'a str) {
        match self.policies.get(bucket) {
            Some(policy) => {
                let backend = policy.backend.as_deref();
                let real_bucket = policy.alias.as_deref().unwrap_or(bucket);
                (backend, real_bucket)
            }
            None => (None, bucket),
        }
    }

    /// All configured bucket policies (for admin API).
    pub fn policies(&self) -> &HashMap<String, BucketPolicyConfig> {
        &self.policies
    }

    /// Build a routing table from bucket policies (for RoutingBackend).
    /// Returns map of virtual_bucket → (backend_name, real_bucket_name_or_none).
    pub fn routing_table(&self) -> HashMap<String, (String, Option<String>)> {
        self.policies
            .iter()
            .filter_map(|(bucket, policy)| {
                policy.backend.as_ref().map(|backend_name| {
                    (bucket.clone(), (backend_name.clone(), policy.alias.clone()))
                })
            })
            .collect()
    }
}

/// Check if a key falls under a prefix.
/// Empty prefix matches everything (entire bucket public).
/// Otherwise, key must start with prefix.
fn key_matches_prefix(key: &str, prefix: &str) -> bool {
    prefix.is_empty() || key.starts_with(prefix)
}

// ── Public Prefix Snapshot (lock-free, hot-swappable) ──

/// Pre-built snapshot of public prefix config for the SigV4 auth middleware.
/// Stored in `Arc<ArcSwap<PublicPrefixSnapshot>>` and swapped atomically
/// on config hot-reload. Reading is lock-free (no mutex on the hot path).
#[derive(Clone, Debug, Default)]
pub struct PublicPrefixSnapshot {
    /// bucket_name (lowercase) → sorted vec of public prefixes
    entries: HashMap<String, Vec<String>>,
}

impl PublicPrefixSnapshot {
    /// Build from bucket policy config (called at startup and on hot-reload).
    /// Applies the same validation as `BucketPolicyRegistry::new()` — rejects
    /// dangerous prefixes (`..`, null bytes, `//`) and strips leading `/`.
    pub fn from_config(buckets: &HashMap<String, BucketPolicyConfig>) -> Self {
        let entries = buckets
            .iter()
            .filter(|(_, v)| !v.public_prefixes.is_empty())
            .map(|(k, v)| {
                let validated: Vec<String> = v
                    .public_prefixes
                    .iter()
                    .filter_map(|p| {
                        let p = p.strip_prefix('/').unwrap_or(p);
                        if p.contains("..") || p.contains('\0') || p.contains("//") {
                            None
                        } else {
                            Some(p.to_string())
                        }
                    })
                    .collect();
                (k.to_ascii_lowercase(), validated)
            })
            .filter(|(_, v)| !v.is_empty())
            .collect();
        Self { entries }
    }

    /// Check if an object key is publicly readable (GET/HEAD).
    pub fn is_public_read(&self, bucket: &str, key: &str) -> bool {
        self.entries
            .get(bucket)
            .map(|prefixes| prefixes.iter().any(|p| key_matches_prefix(key, p)))
            .unwrap_or(false)
    }

    /// Check if a LIST request prefix overlaps with any public prefix.
    pub fn list_overlaps_public(&self, bucket: &str, requested_prefix: &str) -> bool {
        self.entries
            .get(bucket)
            .map(|prefixes| {
                prefixes.iter().any(|pp| {
                    key_matches_prefix(requested_prefix, pp) || pp.starts_with(requested_prefix)
                })
            })
            .unwrap_or(false)
    }

    /// Get the public prefixes for a bucket.
    pub fn public_prefixes_for_bucket(&self, bucket: &str) -> &[String] {
        self.entries
            .get(bucket)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// True if any public prefix is configured.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Type alias for the shared, hot-swappable public prefix snapshot.
pub type SharedPublicPrefixSnapshot = std::sync::Arc<arc_swap::ArcSwap<PublicPrefixSnapshot>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_defaults_when_no_override() {
        let registry = BucketPolicyRegistry::new(HashMap::new(), 0.75);
        assert!(registry.compression_enabled("any-bucket"));
        assert!((registry.max_delta_ratio("any-bucket") - 0.75).abs() < f32::EPSILON);
    }

    #[test]
    fn test_compression_disabled() {
        let mut policies = HashMap::new();
        policies.insert(
            "no-compress".into(),
            BucketPolicyConfig {
                compression: Some(false),
                ..Default::default()
            },
        );
        let registry = BucketPolicyRegistry::new(policies, 0.75);
        assert!(!registry.compression_enabled("no-compress"));
        assert!(registry.compression_enabled("other-bucket"));
    }

    #[test]
    fn test_per_bucket_ratio() {
        let mut policies = HashMap::new();
        policies.insert(
            "aggressive".into(),
            BucketPolicyConfig {
                max_delta_ratio: Some(0.95),
                ..Default::default()
            },
        );
        let registry = BucketPolicyRegistry::new(policies, 0.75);
        assert!((registry.max_delta_ratio("aggressive") - 0.95).abs() < f32::EPSILON);
        assert!((registry.max_delta_ratio("default-bucket") - 0.75).abs() < f32::EPSILON);
    }

    #[test]
    fn test_resolve_backend_default() {
        let registry = BucketPolicyRegistry::new(HashMap::new(), 0.75);
        let (backend, real) = registry.resolve_backend("any-bucket");
        assert_eq!(backend, None);
        assert_eq!(real, "any-bucket");
    }

    #[test]
    fn test_resolve_backend_explicit() {
        let mut policies = HashMap::new();
        policies.insert(
            "archive".into(),
            BucketPolicyConfig {
                backend: Some("hetzner".into()),
                alias: Some("prod-archive".into()),
                ..Default::default()
            },
        );
        let registry = BucketPolicyRegistry::new(policies, 0.75);
        let (backend, real) = registry.resolve_backend("archive");
        assert_eq!(backend, Some("hetzner"));
        assert_eq!(real, "prod-archive");
    }

    #[test]
    fn test_resolve_backend_no_alias() {
        let mut policies = HashMap::new();
        policies.insert(
            "dev-data".into(),
            BucketPolicyConfig {
                backend: Some("local".into()),
                ..Default::default()
            },
        );
        let registry = BucketPolicyRegistry::new(policies, 0.75);
        let (backend, real) = registry.resolve_backend("dev-data");
        assert_eq!(backend, Some("local"));
        assert_eq!(real, "dev-data");
    }

    // ── Public prefix tests (via PublicPrefixSnapshot — the production path) ──

    fn policy_with_public(prefixes: Vec<&str>) -> BucketPolicyConfig {
        BucketPolicyConfig {
            public_prefixes: prefixes.into_iter().map(String::from).collect(),
            ..Default::default()
        }
    }

    /// Build a snapshot from raw policies, running them through BucketPolicyRegistry
    /// validation first (leading-slash strip, dotdot rejection, bucket lowercasing)
    /// to match the production flow.
    fn snapshot_from(policies: HashMap<String, BucketPolicyConfig>) -> PublicPrefixSnapshot {
        let reg = BucketPolicyRegistry::new(policies, 0.75);
        PublicPrefixSnapshot::from_config(reg.policies())
    }

    #[test]
    fn test_public_prefix_basic() {
        let mut policies = HashMap::new();
        policies.insert("releases".into(), policy_with_public(vec!["builds/"]));
        let snap = snapshot_from(policies);
        assert!(snap.is_public_read("releases", "builds/v1.zip"));
        assert!(snap.is_public_read("releases", "builds/subdir/file.txt"));
    }

    #[test]
    fn test_public_prefix_no_match() {
        let mut policies = HashMap::new();
        policies.insert("releases".into(), policy_with_public(vec!["builds/"]));
        let snap = snapshot_from(policies);
        assert!(!snap.is_public_read("releases", "secret/data.zip"));
        assert!(!snap.is_public_read("releases", "other.txt"));
    }

    #[test]
    fn test_public_prefix_boundary() {
        // "pub/" must NOT match "public/"
        let mut policies = HashMap::new();
        policies.insert("test".into(), policy_with_public(vec!["pub/"]));
        let snap = snapshot_from(policies);
        assert!(snap.is_public_read("test", "pub/file.txt"));
        assert!(!snap.is_public_read("test", "public/file.txt"));
        assert!(!snap.is_public_read("test", "pubdata/file.txt"));
    }

    #[test]
    fn test_public_prefix_exact_key() {
        let mut policies = HashMap::new();
        policies.insert("test".into(), policy_with_public(vec!["builds/"]));
        let snap = snapshot_from(policies);
        assert!(snap.is_public_read("test", "builds/"));
    }

    #[test]
    fn test_public_prefix_empty_no_public() {
        let snap = snapshot_from(HashMap::new());
        assert!(!snap.is_public_read("any", "any/key"));
        assert!(snap.is_empty());
    }

    #[test]
    fn test_public_prefix_entire_bucket() {
        let mut policies = HashMap::new();
        policies.insert("open".into(), policy_with_public(vec![""]));
        let snap = snapshot_from(policies);
        assert!(snap.is_public_read("open", "anything"));
        assert!(snap.is_public_read("open", "deep/nested/path.zip"));
        assert!(!snap.is_empty());
    }

    #[test]
    fn test_public_prefix_multiple() {
        let mut policies = HashMap::new();
        policies.insert("test".into(), policy_with_public(vec!["builds/", "docs/"]));
        let snap = snapshot_from(policies);
        assert!(snap.is_public_read("test", "builds/v1.zip"));
        assert!(snap.is_public_read("test", "docs/readme.md"));
        assert!(!snap.is_public_read("test", "secret/key"));
    }

    #[test]
    fn test_public_prefix_validation_dotdot() {
        let mut policies = HashMap::new();
        policies.insert("test".into(), policy_with_public(vec!["../etc/", "ok/"]));
        // Registry validation strips "../etc/", keeps "ok/"
        let snap = snapshot_from(policies);
        assert!(!snap.is_public_read("test", "../etc/passwd"));
        assert!(snap.is_public_read("test", "ok/file"));
    }

    #[test]
    fn test_public_prefix_normalization_leading_slash() {
        let mut policies = HashMap::new();
        policies.insert("test".into(), policy_with_public(vec!["/builds/"]));
        // Registry strips leading "/" → matches "builds/"
        let snap = snapshot_from(policies);
        assert!(snap.is_public_read("test", "builds/v1.zip"));
    }

    #[test]
    fn test_public_prefix_case_sensitivity() {
        let mut policies = HashMap::new();
        policies.insert("MyBucket".into(), policy_with_public(vec!["Builds/"]));
        // Registry lowercases bucket name, prefix preserved as-is
        let snap = snapshot_from(policies);
        assert!(snap.is_public_read("mybucket", "Builds/v1.zip"));
        assert!(!snap.is_public_read("mybucket", "builds/v1.zip")); // case-sensitive prefix
    }

    #[test]
    fn test_list_overlaps_narrower() {
        let mut policies = HashMap::new();
        policies.insert("test".into(), policy_with_public(vec!["builds/"]));
        let snap = snapshot_from(policies);
        assert!(snap.list_overlaps_public("test", "builds/v2/"));
    }

    #[test]
    fn test_list_overlaps_broader() {
        let mut policies = HashMap::new();
        policies.insert("test".into(), policy_with_public(vec!["builds/"]));
        let snap = snapshot_from(policies);
        assert!(snap.list_overlaps_public("test", ""));
    }

    #[test]
    fn test_list_overlaps_disjoint() {
        let mut policies = HashMap::new();
        policies.insert("test".into(), policy_with_public(vec!["builds/"]));
        let snap = snapshot_from(policies);
        assert!(!snap.list_overlaps_public("test", "secret/"));
    }

    #[test]
    fn test_list_overlaps_exact() {
        let mut policies = HashMap::new();
        policies.insert("test".into(), policy_with_public(vec!["builds/"]));
        let snap = snapshot_from(policies);
        assert!(snap.list_overlaps_public("test", "builds/"));
    }

    #[test]
    fn test_public_prefix_snapshot_multi() {
        let mut policies = HashMap::new();
        policies.insert(
            "releases".into(),
            BucketPolicyConfig {
                public_prefixes: vec!["builds/".into(), "docs/".into()],
                ..Default::default()
            },
        );
        let snap = PublicPrefixSnapshot::from_config(&policies);
        assert!(!snap.is_empty());
        assert!(snap.is_public_read("releases", "builds/v1.zip"));
        assert!(snap.is_public_read("releases", "docs/readme.md"));
        assert!(!snap.is_public_read("releases", "secret/data"));
        assert!(!snap.is_public_read("other-bucket", "builds/v1.zip"));
        assert_eq!(snap.public_prefixes_for_bucket("releases").len(), 2);
        assert_eq!(snap.public_prefixes_for_bucket("other").len(), 0);
    }

    #[test]
    fn test_public_prefix_snapshot_empty() {
        let snap = PublicPrefixSnapshot::from_config(&HashMap::new());
        assert!(snap.is_empty());
        assert!(!snap.is_public_read("any", "any"));
    }

    #[test]
    fn test_routing_table() {
        let mut policies = HashMap::new();
        policies.insert(
            "archive".into(),
            BucketPolicyConfig {
                backend: Some("hetzner".into()),
                alias: Some("prod-archive".into()),
                ..Default::default()
            },
        );
        policies.insert(
            "plain".into(),
            BucketPolicyConfig {
                compression: Some(false),
                ..Default::default()
            },
        );
        let registry = BucketPolicyRegistry::new(policies, 0.75);
        let table = registry.routing_table();
        assert_eq!(table.len(), 1); // Only "archive" has a backend
        assert_eq!(
            table.get("archive"),
            Some(&("hetzner".to_string(), Some("prod-archive".to_string())))
        );
    }
}
