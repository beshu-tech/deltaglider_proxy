//! Per-bucket policy configuration.
//!
//! Each bucket can override global compression settings. Unconfigured buckets
//! inherit the global defaults. Designed to be extended with `backend` and
//! `alias` fields for multi-backend routing (Phase 2).

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

    /// All configured bucket policies (for admin API).
    pub fn policies(&self) -> &HashMap<String, BucketPolicyConfig> {
        &self.policies
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
                max_delta_ratio: None,
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
                compression: None,
                max_delta_ratio: Some(0.95),
            },
        );
        let registry = BucketPolicyRegistry::new(policies, 0.75);
        assert!((registry.max_delta_ratio("aggressive") - 0.95).abs() < f32::EPSILON);
        assert!((registry.max_delta_ratio("default-bucket") - 0.75).abs() < f32::EPSILON);
    }
}
