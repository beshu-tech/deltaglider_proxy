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
#[serde(deny_unknown_fields)]
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

/// Admission chain authoring surface. Phase 3b.2.a populates this with
/// the operator-facing wire format for admission blocks; the evaluator
/// still relies on synthesised public-prefix blocks for the live
/// request path (Phase 3b.2.b wires operator-authored blocks through).
///
/// An empty [`AdmissionSection`] (no `blocks:` field) round-trips as a
/// default — the admission chain remains exclusively synthesised from
/// bucket public_prefixes, as in Phase 2.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(deny_unknown_fields)]
pub struct AdmissionSection {
    /// Operator-authored admission blocks. Evaluated in order before
    /// the synthesised public-prefix blocks; first match wins (RRR
    /// semantics).
    ///
    /// In Phase 3b.2.a these blocks deserialize cleanly and round-trip
    /// through `/export` / `/apply`, but are **not** yet dispatched by
    /// the evaluator. A warning is logged at chain-build time so
    /// operators know the gap. Phase 3b.2.b removes the stub and wires
    /// them through for real.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocks: Vec<crate::admission::AdmissionBlockSpec>,
}

/// Authentication sources and IAM state. Phase 3a only holds the legacy
/// SigV4 credential pair + authentication mode — the IAM DB stays
/// authoritative for users/groups/OIDC providers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(deny_unknown_fields)]
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
///
/// # Shorthand forms (Phase 3b.1)
///
/// The section accepts two compact forms in addition to the full-length
/// `backend:` sub-map. Operators who run a single backend can write:
///
/// ```yaml
/// storage:
///   s3: https://example.com       # endpoint URL; triggers S3 backend
///   region: eu-central-1          # optional (default us-east-1)
///   access_key_id: AKIA...        # optional
///   secret_access_key: ...        # optional
///   buckets: { ... }
/// ```
///
/// or
///
/// ```yaml
/// storage:
///   filesystem: /var/dgp          # path; triggers filesystem backend
///   buckets: { ... }
/// ```
///
/// The shorthand fields expand into [`BackendConfig`] at load time via
/// [`StorageSection::normalize`]. Only one of `backend:` / `s3:` /
/// `filesystem:` may be set; mixing them is rejected as operator error.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(deny_unknown_fields)]
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

    // ── Shorthand fields ─────────────────────────────────────────────
    //
    // These never appear in the canonical export — they are operator-
    // authoring conveniences only. [`normalize`] empties them after
    // expanding into [`backend`].
    /// Shorthand: S3 endpoint URL. Expanding this sets `backend` to a
    /// `BackendConfig::S3` with this endpoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub s3: Option<String>,

    /// Shorthand: filesystem path. Expanding this sets `backend` to a
    /// `BackendConfig::Filesystem` with this path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filesystem: Option<std::path::PathBuf>,

    /// Optional companion to `s3:` — AWS region (default `us-east-1`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,

    /// Optional companion to `s3:` — access key id. Absent = use the
    /// environment / IAM instance profile per the AWS SDK's chain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_key_id: Option<String>,

    /// Optional companion to `s3:` — secret access key. Must be set
    /// together with `access_key_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_access_key: Option<String>,

    /// Optional companion to `s3:` — force path-style addressing.
    /// Default `true` (MinIO-compatible). Set `false` for AWS-native
    /// virtual-hosted-style.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub force_path_style: Option<bool>,
}

impl StorageSection {
    /// Expand shorthand forms (`s3:` / `filesystem:`) into a full
    /// [`BackendConfig`]. Leaves the canonical `backend:` untouched when
    /// no shorthand is present.
    ///
    /// Errors when multiple shorthand+backend combinations are
    /// ambiguous — the operator should set exactly one. Also validates
    /// shorthand inputs at the cheap points (non-empty URL, syntactic
    /// `http[s]://` prefix) so typos surface as load errors rather than
    /// opaque AWS SDK failures much later. The long-form
    /// `backend: { type: S3, endpoint }` has no such validation today
    /// for back-compat; Phase 6 can tighten it symmetrically.
    pub fn normalize(&mut self) -> Result<(), String> {
        let has_s3 = self.s3.is_some();
        let has_fs = self.filesystem.is_some();
        let has_backend = !is_backend_default(&self.backend);

        match (has_s3, has_fs, has_backend) {
            (false, false, _) => {
                // No shorthand — nothing to expand. `backend` may or may
                // not be default; either is fine.
            }
            (true, false, false) => {
                // S3 shorthand, no explicit backend. Validate endpoint
                // then expand.
                validate_s3_endpoint(self.s3.as_deref().expect("has_s3 asserted above"))?;
                self.backend = BackendConfig::S3 {
                    endpoint: self.s3.take(),
                    region: self
                        .region
                        .take()
                        .unwrap_or_else(|| "us-east-1".to_string()),
                    force_path_style: self.force_path_style.take().unwrap_or(true),
                    access_key_id: self.access_key_id.take(),
                    secret_access_key: self.secret_access_key.take(),
                };
            }
            (false, true, false) => {
                // Filesystem shorthand, no explicit backend. Validate
                // path then expand.
                validate_filesystem_path(self.filesystem.as_ref().expect("has_fs asserted above"))?;
                self.backend = BackendConfig::Filesystem {
                    path: self.filesystem.take().expect("has_fs asserted above"),
                };
            }
            (true, true, _) => {
                return Err(
                    "storage: `s3:` and `filesystem:` cannot both be set — a single backend \
                     shorthand must pick one"
                        .to_string(),
                );
            }
            (_, _, true) => {
                return Err(format!(
                    "storage: shorthand ({}) cannot be combined with an explicit `backend:` \
                     — pick one form",
                    if has_s3 { "`s3:`" } else { "`filesystem:`" }
                ));
            }
        }

        // Stray companion fields without the anchor are operator error —
        // `region:` alone, for instance, has nothing to attach to.
        if self.region.is_some()
            || self.access_key_id.is_some()
            || self.secret_access_key.is_some()
            || self.force_path_style.is_some()
        {
            return Err(
                "storage: S3 companion fields (region / access_key_id / secret_access_key / \
                 force_path_style) can only be set together with `s3:`"
                    .to_string(),
            );
        }
        Ok(())
    }
}

/// Reject obviously-wrong S3 endpoint URLs at load time. The full
/// URL shape is validated later by the AWS SDK — here we only cheaply
/// rule out the mistakes a human most often makes:
///
/// - empty string (template interpolation left a hole),
/// - missing scheme (copy-paste of "minio:9000" without the scheme),
/// - pathological length (>= 4096 chars — no legitimate endpoint hits this).
fn validate_s3_endpoint(url: &str) -> Result<(), String> {
    if url.is_empty() {
        return Err("storage: `s3:` endpoint is empty — this usually means an \
                    environment variable substitution left a hole. Set a concrete URL."
            .to_string());
    }
    if url.len() > 4096 {
        return Err(format!(
            "storage: `s3:` endpoint is {}-chars long; refusing (legitimate endpoints are <1 KB)",
            url.len()
        ));
    }
    let lower = url.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return Err(format!(
            "storage: `s3:` endpoint `{}` must start with http:// or https:// — AWS SDK \
             rejects scheme-less URLs later in the stack anyway; failing loudly here instead",
            url
        ));
    }
    Ok(())
}

/// Reject obviously-wrong filesystem paths. We do NOT require the path
/// to exist at load time — startup may precede the mount — but we
/// reject empty paths (template interpolation hole) and block relative
/// `..` escapes that usually indicate a template-variable mixup.
fn validate_filesystem_path(path: &std::path::Path) -> Result<(), String> {
    if path.as_os_str().is_empty() {
        return Err(
            "storage: `filesystem:` path is empty — this usually means an \
                    environment variable substitution left a hole"
                .to_string(),
        );
    }
    // Block `..` anywhere in the path. An operator with a legitimate
    // symlink-escape use-case can pre-resolve in their deployment
    // tooling; here we default-closed.
    for component in path.components() {
        if component.as_os_str() == ".." {
            return Err(format!(
                "storage: `filesystem:` path `{}` contains a `..` component — refusing as a \
                 probable template-variable mixup; use an absolute path without `..`",
                path.display()
            ));
        }
    }
    Ok(())
}

/// Process-level tunables: listener, TLS, log level, caches, and the
/// infrastructure-only secrets (`bootstrap_password_hash`,
/// `encryption_key`, `config_sync_bucket`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(deny_unknown_fields)]
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
    ///
    /// # Shorthand exporting policy
    ///
    /// - **Bucket-level `public: true`** IS collapsed from
    ///   `public_prefixes: [""]` when unambiguous. The shorthand is an
    ///   exact 1:1 with the expanded form (exactly one prefix, exactly
    ///   the empty string), so round-tripping is lossless and the GUI's
    ///   "Public read" toggle maps directly to the YAML.
    /// - **Storage-level `s3:` / `filesystem:`** are NOT collapsed.
    ///   Reason: the expanded `backend: { type: S3, endpoint, region,
    ///   force_path_style, ... }` form carries fields (`force_path_style`,
    ///   named backends, etc.) that the shorthand can't express without
    ///   ambiguity, and collapsing selectively would make the exporter
    ///   non-deterministic. Operators who want the compact form should
    ///   keep it in their GitOps source-of-truth file; the server's
    ///   persisted artifact is explicit by contract.
    ///
    /// This asymmetry is intentional. Don't "fix" it by adding a
    /// `collapse_backend_to_shorthand()` without a hard contract that
    /// the collapse is lossless for ALL current and future backend
    /// fields.
    pub fn from_flat(flat: &crate::config::Config) -> Self {
        Self {
            defaults_version: flat.defaults_version,
            // Only emit `admission:` when the operator actually
            // authored blocks — keeps default-config exports empty.
            admission: if flat.admission_blocks.is_empty() {
                None
            } else {
                Some(AdmissionSection {
                    blocks: flat.admission_blocks.clone(),
                })
            },
            access: AccessSection {
                authentication: flat.authentication.clone(),
                access_key_id: flat.access_key_id.clone(),
                secret_access_key: flat.secret_access_key.clone(),
            },
            storage: StorageSection {
                backend: flat.backend.clone(),
                backends: flat.backends.clone(),
                default_backend: flat.default_backend.clone(),
                // Prefer the compact shorthand form (`public: true`) when
                // the canonical expansion is unambiguous. Keeps GitOps
                // diffs short and maps 1:1 to the GUI's bucket-settings
                // "Public read" toggle.
                buckets: flat
                    .buckets
                    .iter()
                    .map(|(name, policy)| (name.clone(), policy.collapse_to_shorthand()))
                    .collect(),
                // Shorthand fields never appear in the canonical export —
                // the expanded `backend:` carries the information instead.
                // Future `collapse_backend_to_shorthand()` could emit
                // these, but today we keep the exporter predictable.
                s3: None,
                filesystem: None,
                region: None,
                access_key_id: None,
                secret_access_key: None,
                force_path_style: None,
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
    ///
    /// Shorthand storage forms (`s3:`, `filesystem:`) are expanded in
    /// place before the flat Config is assembled. Bucket-level
    /// shorthands (`public: true`) are expanded later by
    /// `Config::normalize_shorthands`.
    pub fn into_flat(mut self) -> Result<crate::config::Config, String> {
        self.storage.normalize()?;
        // Validate operator-authored admission blocks semantically
        // (duplicate names, invalid Reject status, conflicting
        // source_ip forms). Structural errors already surfaced via
        // serde; this runs the cross-field checks.
        if let Some(section) = self.admission.as_ref() {
            let spec = crate::admission::AdmissionSpec {
                blocks: section.blocks.clone(),
            };
            spec.validate()?;
        }
        Ok(self.into_flat_unchecked())
    }

    /// Internal: flat projection without any shorthand expansion. Used
    /// by tests and by the exporter's round-trip verification where
    /// shorthands have already been resolved.
    fn into_flat_unchecked(self) -> crate::config::Config {
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
            cache_size_mb: self
                .advanced
                .cache_size_mb
                .unwrap_or(defaults.cache_size_mb),
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
            admission_blocks: self.admission.map(|s| s.blocks).unwrap_or_default(),
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
        let back = sectioned.into_flat().unwrap();
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
        let back = sectioned.into_flat().unwrap();
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

    // ── Phase 3b.1: storage shorthand ─────────────────────────────────

    #[test]
    fn storage_s3_shorthand_expands_to_s3_backend() {
        let mut storage = StorageSection {
            s3: Some("https://minio.example.com".into()),
            region: Some("eu-central-1".into()),
            access_key_id: Some("AKIA".into()),
            secret_access_key: Some("secret".into()),
            ..Default::default()
        };
        storage.normalize().unwrap();
        match &storage.backend {
            BackendConfig::S3 {
                endpoint,
                region,
                access_key_id,
                secret_access_key,
                force_path_style,
            } => {
                assert_eq!(endpoint.as_deref(), Some("https://minio.example.com"));
                assert_eq!(region, "eu-central-1");
                assert_eq!(access_key_id.as_deref(), Some("AKIA"));
                assert_eq!(secret_access_key.as_deref(), Some("secret"));
                assert!(*force_path_style, "force_path_style default is true");
            }
            other => panic!("expected S3 backend, got {other:?}"),
        }
        // Shorthand fields must be drained after expansion.
        assert!(storage.s3.is_none());
        assert!(storage.region.is_none());
        assert!(storage.access_key_id.is_none());
        assert!(storage.secret_access_key.is_none());
    }

    #[test]
    fn storage_s3_shorthand_uses_us_east_1_when_region_absent() {
        let mut storage = StorageSection {
            s3: Some("https://example.com".into()),
            ..Default::default()
        };
        storage.normalize().unwrap();
        match &storage.backend {
            BackendConfig::S3 { region, .. } => assert_eq!(region, "us-east-1"),
            other => panic!("expected S3 backend, got {other:?}"),
        }
    }

    #[test]
    fn storage_filesystem_shorthand_expands_to_fs_backend() {
        let mut storage = StorageSection {
            filesystem: Some("/var/dgp".into()),
            ..Default::default()
        };
        storage.normalize().unwrap();
        match &storage.backend {
            BackendConfig::Filesystem { path } => {
                assert_eq!(path.to_str(), Some("/var/dgp"));
            }
            other => panic!("expected Filesystem backend, got {other:?}"),
        }
        assert!(storage.filesystem.is_none());
    }

    #[test]
    fn storage_shorthand_mixed_s3_and_filesystem_is_error() {
        let mut storage = StorageSection {
            s3: Some("https://example.com".into()),
            filesystem: Some("/var/dgp".into()),
            ..Default::default()
        };
        let err = storage
            .normalize()
            .expect_err("s3: and filesystem: together must be rejected");
        assert!(
            err.contains("s3") && err.contains("filesystem"),
            "error must name both fields, got: {err}"
        );
    }

    #[test]
    fn storage_shorthand_combined_with_explicit_backend_is_error() {
        let mut storage = StorageSection {
            s3: Some("https://example.com".into()),
            backend: BackendConfig::Filesystem {
                path: "/explicit".into(),
            },
            ..Default::default()
        };
        let err = storage
            .normalize()
            .expect_err("s3: together with explicit backend: must be rejected");
        assert!(
            err.contains("shorthand") && err.contains("backend"),
            "error must explain the conflict, got: {err}"
        );
    }

    #[test]
    fn storage_shorthand_companion_without_anchor_is_error() {
        // `region:` alone has nothing to attach to — it's not valid on
        // the canonical `backend: { type: S3, ... }` form either. Make
        // sure the operator sees a clear error.
        let mut storage = StorageSection {
            region: Some("eu-central-1".into()),
            ..Default::default()
        };
        let err = storage
            .normalize()
            .expect_err("region without s3: must be rejected");
        assert!(
            err.contains("region") || err.contains("companion"),
            "error must name the orphaned field, got: {err}"
        );
    }

    #[test]
    fn storage_no_shorthand_is_noop() {
        // A storage section with only the canonical `backend:` must not
        // be modified by normalize. This is the hot path for legacy
        // configs.
        let original = StorageSection {
            backend: BackendConfig::Filesystem {
                path: "/data".into(),
            },
            ..Default::default()
        };
        let mut storage = original.clone();
        storage.normalize().unwrap();
        assert_eq!(storage, original);
    }

    // ── Phase 3b.1 hardening: input validation ────────────────────────

    #[test]
    fn storage_s3_empty_endpoint_rejected() {
        let mut storage = StorageSection {
            s3: Some(String::new()),
            ..Default::default()
        };
        let err = storage
            .normalize()
            .expect_err("empty s3 endpoint must be rejected");
        assert!(
            err.contains("empty"),
            "error must name the problem, got: {err}"
        );
    }

    #[test]
    fn storage_s3_missing_scheme_rejected() {
        let mut storage = StorageSection {
            s3: Some("minio:9000".into()),
            ..Default::default()
        };
        let err = storage
            .normalize()
            .expect_err("scheme-less s3 endpoint must be rejected");
        assert!(
            err.contains("http") && err.contains("minio:9000"),
            "error must guide the operator, got: {err}"
        );
    }

    #[test]
    fn storage_s3_scheme_case_insensitive() {
        // HTTP:// and HTTPS:// should be accepted — the AWS SDK is case-
        // insensitive on schemes.
        let mut storage = StorageSection {
            s3: Some("HTTPS://example.com".into()),
            ..Default::default()
        };
        storage.normalize().unwrap();
    }

    #[test]
    fn storage_s3_pathological_length_rejected() {
        let huge = "http://".to_string() + &"a".repeat(5000);
        let mut storage = StorageSection {
            s3: Some(huge),
            ..Default::default()
        };
        let err = storage
            .normalize()
            .expect_err("pathologically long URL must be rejected");
        assert!(
            err.contains("chars"),
            "error must mention length, got: {err}"
        );
    }

    #[test]
    fn storage_filesystem_empty_path_rejected() {
        let mut storage = StorageSection {
            filesystem: Some(std::path::PathBuf::new()),
            ..Default::default()
        };
        let err = storage
            .normalize()
            .expect_err("empty filesystem path must be rejected");
        assert!(
            err.contains("empty"),
            "error must name the problem, got: {err}"
        );
    }

    #[test]
    fn storage_filesystem_parent_escape_rejected() {
        let mut storage = StorageSection {
            filesystem: Some("/var/lib/dgp/../../etc".into()),
            ..Default::default()
        };
        let err = storage
            .normalize()
            .expect_err("path with `..` components must be rejected");
        assert!(
            err.contains(".."),
            "error must name the problem, got: {err}"
        );
    }

    #[test]
    fn storage_filesystem_relative_parent_also_rejected() {
        let mut storage = StorageSection {
            filesystem: Some("../oops".into()),
            ..Default::default()
        };
        let err = storage
            .normalize()
            .expect_err("relative path with `..` must be rejected");
        assert!(err.contains(".."));
    }
}
