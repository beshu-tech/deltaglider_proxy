//! Sectioned configuration shape for Phase 3 of the progressive-disclosure
//! refactor.
//!
//! This module exists as a *serde boundary only*. It gives the on-disk YAML
//! format a new four-section layout:
//!
//! ```yaml
//! admission: ...
//! access:    ...
//! storage:   ...
//! advanced:  ...
//! ```
//!
//! without changing [`crate::config::Config`]'s in-memory field layout —
//! which has hundreds of call sites across the codebase reading
//! `cfg.max_delta_ratio`, `cfg.backend`, etc.
//!
//! # How it works
//!
//! The public [`Config`](crate::config::Config) deserializer uses a
//! [`#[serde(untagged)]`] enum that tries the sectioned shape first and
//! falls back to the historical flat shape. Both shapes produce the same
//! in-memory `Config`. Serialization emits the sectioned shape for YAML
//! (via [`SectionedConfig::from_flat`] + `to_string`); TOML persistence
//! keeps the flat shape because TOML is deprecation-bound.
//!
//! # Why not `#[serde(flatten)]`?
//!
//! The original plan suggested wrapping each section in a `#[serde(flatten)]`
//! struct, but `flatten` only projects one wire shape onto one in-memory
//! shape — it does not support "accept either flat OR sectioned YAML". The
//! untagged-enum approach is the minimal amount of serde machinery that
//! gets us BOTH shapes on read with a single in-memory target.
//!
//! # What's not here (Phase 3a scope bound)
//!
//! - Shorthand deserializers (`storage: { s3: URL, ... }`) — Phase 3b.
//! - `bucket: { public: true }` admission synthesis — Phase 3b.
//! - `access.iam_mode` + reconciler — Phase 3c.
//! - Group presets expanding to IAM JSON — Phase 3d.
//!
//! The section types here intentionally mirror the current flat field layout
//! one-for-one. They become the insertion point for the above features.

use crate::bucket_policy::BucketPolicyConfig;
use crate::config::{BackendConfig, DefaultsVersion, NamedBackendConfig, TlsConfig};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::net::SocketAddr;

/// Sectioned YAML shape. Converts to/from [`crate::config::Config`] via
/// [`SectionedConfig::from_flat`] / [`SectionedConfig::into_flat`] —
/// never held in memory by the server.
///
/// Top-level `defaults` is kept at the root (not a section) because it's
/// metadata about the whole document, not any one concern.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
pub struct SectionedConfig {
    /// Pinned defaults posture — omitted when the server-current default.
    #[serde(
        default,
        rename = "defaults",
        skip_serializing_if = "DefaultsVersion::is_default"
    )]
    pub defaults_version: DefaultsVersion,

    /// Admission-chain blocks. Phase 3a: always empty — the chain is
    /// synthesized from `storage.buckets[*].public_prefixes`. Phase 3b
    /// populates this with operator-authored blocks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission: Option<AdmissionSection>,

    /// Who can authenticate: SigV4 credentials, OAuth providers (DB-backed
    /// in Phase 3a so this is just the legacy credential pair), IAM users
    /// (Phase 3c).
    #[serde(default, skip_serializing_if = "is_access_default")]
    pub access: AccessSection,

    /// Where data lives: backend(s) + per-bucket overrides.
    #[serde(default, skip_serializing_if = "is_storage_default")]
    pub storage: StorageSection,

    /// Process-level knobs: listen address, caches, TLS, log level, etc.
    #[serde(default, skip_serializing_if = "is_advanced_default")]
    pub advanced: AdvancedSection,
}

/// Phase 3a ships an empty placeholder — the admission chain is derived
/// entirely from bucket public_prefixes. Phase 3b introduces actual block
/// authoring via this type.
///
/// The shape is deliberately unit-struct-like for now (no fields the
/// operator can set). We keep it as a distinct type so Phase 3b can
/// add fields without breaking serde back-compat on configs already
/// written against Phase 3a.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
pub struct AdmissionSection {
    /// Phase 3b: populate with `Vec<AdmissionBlockSpec>`. Today we
    /// deliberately omit any field so operator docs authored now
    /// can't accidentally encode non-portable YAML.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserved: Option<serde_json::Value>,
}

/// Authentication sources and IAM state. Phase 3a only holds the legacy
/// SigV4 credential pair + authentication mode — the IAM DB stays
/// authoritative for users/groups/OIDC providers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
pub struct AccessSection {
    /// Explicit auth-mode selector: `"none"` for open access; absent
    /// means "auto-detect from credentials".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authentication: Option<String>,

    /// Legacy proxy SigV4 credentials (the "bootstrap admin" key).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_key_id: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_access_key: Option<String>,
}

/// Backends + per-bucket overrides.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
pub struct StorageSection {
    /// Default (legacy) single backend. Compatible with existing
    /// one-backend deployments.
    #[serde(default, skip_serializing_if = "is_backend_default")]
    pub backend: BackendConfig,

    /// Named backends for multi-backend routing. When non-empty, the
    /// legacy `backend` field is ignored at runtime.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub backends: Vec<NamedBackendConfig>,

    /// Name of the default backend when `backends` is populated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_backend: Option<String>,

    /// Per-bucket compression / quota / public-prefix overrides.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub buckets: BTreeMap<String, BucketPolicyConfig>,
}

/// Process-level tunables: listener, TLS, log level, caches, and the
/// infrastructure-only secrets (`bootstrap_password_hash`,
/// `encryption_key`, `config_sync_bucket`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
pub struct AdvancedSection {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen_addr: Option<SocketAddr>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_delta_ratio: Option<f32>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_object_size: Option<u64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_size_mb: Option<usize>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata_cache_mb: Option<usize>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec_concurrency: Option<usize>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocking_threads: Option<usize>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_level: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_sync_bucket: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<TlsConfig>,

    /// Bcrypt hash of the bootstrap password. An infra secret: stripped
    /// by the same redactor that powers `to_canonical_yaml`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bootstrap_password_hash: Option<String>,

    /// Hex-encoded 256-bit AES key for encryption at rest. Infra secret.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encryption_key: Option<String>,
}

// ══ skip_serializing_if helpers — any non-default value surfaces. ══════

fn is_access_default(s: &AccessSection) -> bool {
    s == &AccessSection::default()
}

fn is_storage_default(s: &StorageSection) -> bool {
    s == &StorageSection::default()
}

fn is_advanced_default(s: &AdvancedSection) -> bool {
    s == &AdvancedSection::default()
}

fn is_backend_default(b: &BackendConfig) -> bool {
    b == &BackendConfig::default()
}

impl SectionedConfig {
    /// Build a `SectionedConfig` from a flat [`Config`].
    ///
    /// This is the canonical exporter — called by `to_canonical_yaml`.
    /// We deliberately keep default-valued `Option<T>` fields as `None`
    /// so the serialized YAML omits them (cleaner GitOps diffs).
    pub fn from_flat(flat: &crate::config::Config) -> Self {
        Self {
            defaults_version: flat.defaults_version,
            admission: None, // Phase 3a: not yet operator-editable.
            access: AccessSection {
                authentication: flat.authentication.clone(),
                access_key_id: flat.access_key_id.clone(),
                secret_access_key: flat.secret_access_key.clone(),
            },
            storage: StorageSection {
                backend: flat.backend.clone(),
                backends: flat.backends.clone(),
                default_backend: flat.default_backend.clone(),
                buckets: flat.buckets.clone(),
            },
            advanced: AdvancedSection {
                // Emit only non-default values to keep the exported YAML
                // minimal. Round-trip correctness is the invariant — the
                // defaults round back through `Config::default()`.
                listen_addr: some_if_nondefault(flat.listen_addr, default_listen()),
                max_delta_ratio: some_if_nondefault(flat.max_delta_ratio, default_ratio()),
                max_object_size: some_if_nondefault(flat.max_object_size, default_max_object()),
                cache_size_mb: some_if_nondefault(flat.cache_size_mb, default_cache_mb()),
                metadata_cache_mb: some_if_nondefault(
                    flat.metadata_cache_mb,
                    default_metadata_cache_mb(),
                ),
                codec_concurrency: flat.codec_concurrency,
                blocking_threads: flat.blocking_threads,
                log_level: some_if_nondefault_str(&flat.log_level, default_log()),
                config_sync_bucket: flat.config_sync_bucket.clone(),
                tls: flat.tls.clone(),
                bootstrap_password_hash: flat.bootstrap_password_hash.clone(),
                encryption_key: flat.encryption_key.clone(),
            },
        }
    }

    /// Collapse a `SectionedConfig` back into a flat [`Config`]. The
    /// inverse of [`SectionedConfig::from_flat`].
    ///
    /// Missing scalars fall back to their `Config::default()` values —
    /// which is the whole point of the `Option<T>` wrapping in
    /// `AdvancedSection`: authors only set the fields they care about.
    pub fn into_flat(self) -> crate::config::Config {
        let defaults = crate::config::Config::default();
        crate::config::Config {
            defaults_version: self.defaults_version,
            listen_addr: self.advanced.listen_addr.unwrap_or(defaults.listen_addr),
            backend: self.storage.backend,
            max_delta_ratio: self
                .advanced
                .max_delta_ratio
                .unwrap_or(defaults.max_delta_ratio),
            max_object_size: self
                .advanced
                .max_object_size
                .unwrap_or(defaults.max_object_size),
            cache_size_mb: self.advanced.cache_size_mb.unwrap_or(defaults.cache_size_mb),
            metadata_cache_mb: self
                .advanced
                .metadata_cache_mb
                .unwrap_or(defaults.metadata_cache_mb),
            authentication: self.access.authentication,
            access_key_id: self.access.access_key_id,
            secret_access_key: self.access.secret_access_key,
            bootstrap_password_hash: self.advanced.bootstrap_password_hash,
            codec_concurrency: self.advanced.codec_concurrency,
            blocking_threads: self.advanced.blocking_threads,
            log_level: self.advanced.log_level.unwrap_or(defaults.log_level),
            config_sync_bucket: self.advanced.config_sync_bucket,
            encryption_key: self.advanced.encryption_key,
            tls: self.advanced.tls,
            buckets: self.storage.buckets,
            backends: self.storage.backends,
            default_backend: self.storage.default_backend,
        }
    }
}

/// Helper: return `Some(value)` unless it equals the default, in which
/// case `None` (which `skip_serializing_if` then omits).
fn some_if_nondefault<T: PartialEq>(value: T, default: T) -> Option<T> {
    if value == default {
        None
    } else {
        Some(value)
    }
}

fn some_if_nondefault_str(value: &str, default: String) -> Option<String> {
    if value == default {
        None
    } else {
        Some(value.to_string())
    }
}

// These duplicate the private `fn default_*` helpers in config.rs. Not DRY,
// but the alternative is making those `pub(crate)` and importing them —
// which inverts the dependency direction (sections should know about
// Config, not the other way around). Four-line duplicates are acceptable.
fn default_listen() -> SocketAddr {
    "0.0.0.0:9000".parse().expect("hardcoded literal")
}
fn default_ratio() -> f32 {
    0.75
}
fn default_max_object() -> u64 {
    100 * 1024 * 1024
}
fn default_cache_mb() -> usize {
    100
}
fn default_metadata_cache_mb() -> usize {
    50
}
fn default_log() -> String {
    "deltaglider_proxy=debug,tower_http=debug".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn round_trips_default_config() {
        let flat = Config::default();
        let sectioned = SectionedConfig::from_flat(&flat);
        let back = sectioned.into_flat();
        assert_eq!(flat, back, "default Config must round-trip losslessly");
    }

    #[test]
    fn round_trips_populated_config() {
        let flat = Config {
            max_delta_ratio: 0.42,
            cache_size_mb: 512,
            access_key_id: Some("AKIA".into()),
            secret_access_key: Some("secret".into()),
            log_level: "info".into(),
            ..Config::default()
        };
        let sectioned = SectionedConfig::from_flat(&flat);
        let back = sectioned.into_flat();
        assert_eq!(flat, back);
    }

    #[test]
    fn omits_default_scalars_from_advanced() {
        let flat = Config::default();
        let sectioned = SectionedConfig::from_flat(&flat);
        // All AdvancedSection Option<T>s should be None for a default Config,
        // so the emitted YAML has no `advanced:` section at all.
        assert_eq!(sectioned.advanced, AdvancedSection::default());
        // AccessSection also defaults empty when Config has no creds.
        assert_eq!(sectioned.access, AccessSection::default());
        // StorageSection's backend is the default Filesystem, so it's
        // omitted from serialisation. Non-default scalars (buckets,
        // backends) are also empty collections — hence StorageSection
        // equals its Default.
        assert_eq!(sectioned.storage, StorageSection::default());
    }

    #[test]
    fn yaml_sectioned_shape_emits_four_sections_only_when_non_default() {
        let flat = Config {
            max_delta_ratio: 0.25,
            ..Config::default()
        };
        let sectioned = SectionedConfig::from_flat(&flat);
        let yaml = serde_yaml::to_string(&sectioned).unwrap();
        assert!(
            yaml.contains("advanced:"),
            "overridden max_delta_ratio should surface an advanced section, got: {yaml}"
        );
        assert!(
            !yaml.contains("access:"),
            "default access should be omitted, got: {yaml}"
        );
        assert!(
            !yaml.contains("storage:"),
            "default storage should be omitted, got: {yaml}"
        );
    }
}
