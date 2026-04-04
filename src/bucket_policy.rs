//! Per-bucket policy configuration.
//!
//! Each bucket can override global compression settings and route to a
//! specific named backend. Unconfigured buckets inherit global defaults
//! and use the default backend.

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
        // Normalize bucket names to lowercase and validate ratio values
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
    pub fn max_delta_ratio(&self, bucket: &str) -> f32 {
        self.policies
            .get(bucket)
            .and_then(|p| p.max_delta_ratio)
            .unwrap_or(self.default_max_delta_ratio)
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
                    (
                        bucket.clone(),
                        (backend_name.clone(), policy.alias.clone()),
                    )
                })
            })
            .collect()
    }
}

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
