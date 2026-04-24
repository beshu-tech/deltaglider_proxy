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
//! # Features layered on the section types
//!
//! - Phase 3b.1: shorthand deserializers (`storage: { s3: URL, ... }` /
//!   `{ filesystem: PATH, ... }`) + per-bucket `public: true` that
//!   compiles to `public_prefixes: [""]`. See
//!   [`StorageSection::normalize`] / [`crate::bucket_policy::BucketPolicyConfig::normalize`].
//! - Phase 3b.2.a/b: operator-authored admission blocks
//!   ([`AdmissionSection::blocks`]) with deny/reject/allow-anonymous
//!   actions. The evaluator dispatches these live; see
//!   [`crate::admission`].
//! - Phase 3c.1/3c.2: `access.iam_mode: gui | declarative` toggle
//!   gating admin-API IAM mutation routes; see [`IamMode`] and
//!   [`crate::api::admin::auth::require_not_declarative`].
//!
//! Still pending:
//! - Phase 3c.3: the reconciler (sync-diff DB ↔ YAML on apply when
//!   mode is declarative).
//! - Phase 3d: group presets expanding to IAM policy documents.

use crate::bucket_policy::BucketPolicyConfig;
use crate::config::{
    default_cache_size_mb, default_listen_addr, default_log_level, default_max_delta_ratio,
    default_max_object_size, default_metadata_cache_mb, BackendConfig, DefaultsVersion,
    NamedBackendConfig, TlsConfig,
};
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

    /// Admission-chain blocks. Operator-authored rules (deny / reject /
    /// allow-anonymous) that fire BEFORE the synthesized public-prefix
    /// blocks derived from `storage.buckets[*].public_prefixes`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission: Option<AdmissionSection>,

    /// Who can authenticate. Carries the legacy SigV4 credential pair,
    /// the authentication-mode selector, and `iam_mode: gui | declarative`
    /// (Phase 3c). OAuth providers and IAM users stay in the encrypted
    /// DB until the Phase 3c.3 reconciler makes YAML authoritative.
    #[serde(default, skip_serializing_if = "is_access_default")]
    pub access: AccessSection,

    /// Where data lives: backend(s) + per-bucket overrides.
    #[serde(default, skip_serializing_if = "is_storage_default")]
    pub storage: StorageSection,

    /// Process-level knobs: listen address, caches, TLS, log level, etc.
    #[serde(default, skip_serializing_if = "is_advanced_default")]
    pub advanced: AdvancedSection,
}

/// Admission chain authoring surface. Holds the operator-facing wire
/// format for admission blocks; the evaluator dispatches these live
/// (Phase 3b.2.b) AHEAD of the synthesised public-prefix blocks
/// derived from `storage.buckets[*].public_prefixes`.
///
/// An empty [`AdmissionSection`] (no `blocks:` field) round-trips as a
/// default — the admission chain is then exclusively synthesised from
/// bucket public_prefixes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(deny_unknown_fields)]
pub struct AdmissionSection {
    /// Operator-authored admission blocks. Evaluated in order before
    /// the synthesised public-prefix blocks; first match wins (RRR
    /// semantics). Supported actions: `allow-anonymous`, `deny`,
    /// `reject { type: reject, status, message }`, `continue`.
    /// See [`crate::admission::AdmissionBlockSpec`] for the full
    /// match-predicate schema (method / source_ip_list / bucket /
    /// path_glob / authenticated / config_flag).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocks: Vec<crate::admission::AdmissionBlockSpec>,
}

/// Authentication sources and IAM state.
///
/// Phase 3c.1 introduces [`AccessSection::iam_mode`]:
///
/// - `Gui` (default) — the encrypted IAM DB is the source of truth
///   for users, groups, OAuth providers, and mapping rules. Runtime
///   IAM changes go through the admin GUI (or admin API); the YAML
///   `access:` section holds ONLY the legacy SigV4 credential pair
///   and the authentication-mode selector.
/// - `Declarative` — the YAML document is the source of truth. The
///   reconciler (Phase 3c.3) sync-diffs the DB to YAML on every
///   `apply`; admin-API mutations on users/groups/providers return
///   403 (Phase 3c.2).
///
/// The enum is deliberately serde-case-insensitive: `gui`, `Gui`,
/// and `GUI` all parse. Default is `Gui` so existing deployments
/// silently get the status-quo behavior; operators explicitly opt
/// into `declarative` by setting the field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(deny_unknown_fields)]
pub struct AccessSection {
    /// GUI vs declarative IAM mode selector. See [`IamMode`].
    #[serde(default, skip_serializing_if = "IamMode::is_default")]
    pub iam_mode: IamMode,

    /// Explicit auth-mode selector: `"none"` for open access; absent
    /// means "auto-detect from credentials".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authentication: Option<String>,

    /// Legacy proxy SigV4 credentials (the "bootstrap admin" key).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_key_id: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_access_key: Option<String>,

    // ── Phase 3c.3: declarative-mode IAM state ──
    //
    // These four fields are consulted BY THE RECONCILER only when
    // `iam_mode == Declarative`. In `Gui` mode they are tolerated
    // (validation still runs) but never applied — the DB remains
    // source of truth.
    /// IAM users as declared in YAML. References groups by NAME.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub iam_users: Vec<crate::iam::DeclarativeUser>,
    /// IAM groups as declared in YAML.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub iam_groups: Vec<crate::iam::DeclarativeGroup>,
    /// External auth providers (OIDC/OAuth) as declared in YAML.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub auth_providers: Vec<crate::iam::DeclarativeAuthProvider>,
    /// OAuth group-mapping rules as declared in YAML. References
    /// providers + groups by NAME.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub group_mapping_rules: Vec<crate::iam::DeclarativeMappingRule>,
}

/// Source-of-truth selector for IAM state. See
/// [`AccessSection::iam_mode`] for semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "kebab-case")]
pub enum IamMode {
    /// DB is the source of truth. Runtime IAM CRUD through the GUI /
    /// admin API mutates the DB; YAML `access.users`/`groups`/`providers`
    /// are applied as seeds only when the DB is empty at startup.
    #[default]
    Gui,
    /// YAML is the source of truth. The reconciler rebuilds the DB to
    /// match the YAML on every `apply`; admin-API mutations on
    /// users/groups/providers return 403.
    Declarative,
}

impl IamMode {
    /// `skip_serializing_if` helper: omit the field when it equals the
    /// server-current default. Keeps the exported YAML minimal so
    /// default deployments don't grow an `iam_mode: gui` line.
    pub(crate) fn is_default(&self) -> bool {
        matches!(self, IamMode::Gui)
    }
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

    /// Per-backend encryption config for the singleton `backend` above.
    /// Ignored when `backends` (the list) is non-empty — in that case
    /// each named entry's own `encryption` field applies.
    #[serde(default, skip_serializing_if = "crate::config::is_default_encryption")]
    pub backend_encryption: crate::config::BackendEncryptionConfig,

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

    /// Lazy bucket replication configuration. A single periodic worker
    /// walks each due rule and copies objects source → destination
    /// through the engine (so encryption + delta compression stay
    /// transparent to the operator). See
    /// `docs/product/reference/replication.md` and the YAML schema at
    /// `deltaglider_proxy.example.yaml`.
    #[serde(default, skip_serializing_if = "is_default_replication")]
    pub replication: ReplicationConfig,
}

/// Skip emitting `replication:` from canonical YAML exports when it's
/// the default (disabled-by-default semantics: empty rules list with
/// default scheduler controls).
pub(crate) fn is_default_replication(r: &ReplicationConfig) -> bool {
    r == &ReplicationConfig::default()
}

/// Global replication controls + the rules list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ReplicationConfig {
    /// Master kill-switch for the replication worker. Defaults to `true`;
    /// when false, the worker never ticks even if rules exist.
    #[serde(default = "default_replication_enabled")]
    pub enabled: bool,

    /// How often the scheduler wakes up to check for due rules. Parsed
    /// via humantime. Defaults to `30s`. Minimum enforced at config
    /// load time (`5s`) to prevent scheduler-thrash.
    #[serde(default = "default_tick_interval")]
    pub tick_interval: String,

    /// Per-rule failure ring bound. When a rule accumulates more than
    /// this number of per-object failures, the oldest are dropped.
    #[serde(default = "default_max_failures")]
    pub max_failures_retained: u32,

    /// Replication rules. Each rule describes a source → destination
    /// copy with its own interval and filters. Empty by default.
    #[serde(default)]
    pub rules: Vec<ReplicationRule>,
}

impl Default for ReplicationConfig {
    fn default() -> Self {
        Self {
            enabled: default_replication_enabled(),
            tick_interval: default_tick_interval(),
            max_failures_retained: default_max_failures(),
            rules: Vec::new(),
        }
    }
}

fn default_replication_enabled() -> bool {
    true
}

fn default_tick_interval() -> String {
    "30s".to_string()
}

fn default_max_failures() -> u32 {
    100
}

/// A single replication rule: copy objects from `source` to `destination`
/// on `interval`. The cross-encryption/cross-backend/cross-compression
/// transparency comes from routing the copy through
/// `engine.retrieve` → `engine.store`, not through raw storage layer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ReplicationRule {
    /// Rule name. Unique within the config, ASCII `[A-Za-z0-9_.-]{1,64}`.
    /// Used as the key in the `replication_state` DB table.
    pub name: String,

    /// Per-rule toggle. `false` keeps the rule declared but inert — the
    /// scheduler skips it. Operator-facing pause/resume (via the admin
    /// API) is stored in the DB separately and takes precedence.
    #[serde(default = "default_rule_enabled")]
    pub enabled: bool,

    pub source: ReplicationEndpoint,
    pub destination: ReplicationEndpoint,

    /// How often this rule runs. Humantime-parsed (`"15m"`, `"1h"`, ...).
    /// Minimum enforced at config load time (`30s`).
    pub interval: String,

    /// Objects per scheduler yield. The worker copies this many objects
    /// then yields to the tokio scheduler so other due rules can
    /// interleave on the same worker task. Defaults to 100.
    #[serde(default = "default_batch_size")]
    pub batch_size: u32,

    /// When `true`, keys present on destination but NOT on source are
    /// deleted after the forward-copy pass. Opt-in by design — default
    /// `false` — because "did my delete replicate" is a footgun.
    #[serde(default)]
    pub replicate_deletes: bool,

    /// Policy for handling objects that exist on both sides.
    #[serde(default)]
    pub conflict: ConflictPolicy,

    /// Optional globset: if non-empty, ONLY keys matching at least one
    /// of these patterns are replicated. Applied in addition to
    /// `exclude_globs`.
    #[serde(default)]
    pub include_globs: Vec<String>,

    /// Optional globset: keys matching any of these patterns are
    /// skipped. Default excludes DG internals (`.dg/*`).
    #[serde(default = "default_exclude_globs")]
    pub exclude_globs: Vec<String>,
}

fn default_rule_enabled() -> bool {
    true
}

fn default_batch_size() -> u32 {
    100
}

fn default_exclude_globs() -> Vec<String> {
    vec![".dg/*".to_string()]
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ReplicationEndpoint {
    pub bucket: String,
    #[serde(default)]
    pub prefix: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ConflictPolicy {
    /// Copy only if source is strictly newer than destination (default).
    /// Ties fall through to source-wins (can't distinguish clocks across
    /// storage tiers).
    #[default]
    NewerWins,
    /// Always copy, overwriting destination.
    SourceWins,
    /// Never copy when destination exists. Useful for "seed once"
    /// replication rules.
    SkipIfDestExists,
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
/// infrastructure-only secret (`bootstrap_password_hash`,
/// `config_sync_bucket`).
///
/// Per-backend encryption lives under `storage.backends[*].encryption`
/// (and `storage.backend_encryption` for the singleton path), NOT here.
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
                iam_mode: flat.iam_mode,
                authentication: flat.authentication.clone(),
                access_key_id: flat.access_key_id.clone(),
                secret_access_key: flat.secret_access_key.clone(),
                iam_users: flat.iam_users.clone(),
                iam_groups: flat.iam_groups.clone(),
                auth_providers: flat.auth_providers.clone(),
                group_mapping_rules: flat.group_mapping_rules.clone(),
            },
            storage: StorageSection {
                backend: flat.backend.clone(),
                backend_encryption: flat.backend_encryption.clone(),
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
                replication: flat.replication.clone(),
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
                listen_addr: some_if_nondefault(flat.listen_addr, default_listen_addr()),
                max_delta_ratio: some_if_nondefault(
                    flat.max_delta_ratio,
                    default_max_delta_ratio(),
                ),
                max_object_size: some_if_nondefault(
                    flat.max_object_size,
                    default_max_object_size(),
                ),
                cache_size_mb: some_if_nondefault(flat.cache_size_mb, default_cache_size_mb()),
                metadata_cache_mb: some_if_nondefault(
                    flat.metadata_cache_mb,
                    default_metadata_cache_mb(),
                ),
                codec_concurrency: flat.codec_concurrency,
                blocking_threads: flat.blocking_threads,
                log_level: some_if_nondefault_str(&flat.log_level, default_log_level()),
                config_sync_bucket: flat.config_sync_bucket.clone(),
                tls: flat.tls.clone(),
                bootstrap_password_hash: flat.bootstrap_password_hash.clone(),
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
            iam_mode: self.access.iam_mode,
            iam_users: self.access.iam_users,
            iam_groups: self.access.iam_groups,
            auth_providers: self.access.auth_providers,
            group_mapping_rules: self.access.group_mapping_rules,
            bootstrap_password_hash: self.advanced.bootstrap_password_hash,
            codec_concurrency: self.advanced.codec_concurrency,
            blocking_threads: self.advanced.blocking_threads,
            log_level: self.advanced.log_level.unwrap_or(defaults.log_level),
            config_sync_bucket: self.advanced.config_sync_bucket,
            tls: self.advanced.tls,
            buckets: self.storage.buckets,
            backend_encryption: self.storage.backend_encryption,
            backends: self.storage.backends,
            default_backend: self.storage.default_backend,
            replication: self.storage.replication,
            admission_blocks: self.admission.map(|s| s.blocks).unwrap_or_default(),
            // iam_mode already populated above from self.access.iam_mode.
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

// ────────────────────────────────────────────────────────────────────────
// Replication config validation — pure, unit-testable, no I/O.
// ────────────────────────────────────────────────────────────────────────

/// Rule-name regex: `[A-Za-z0-9_.-]{1,64}`. ASCII-only, no whitespace,
/// no slashes. Matches the SQLite column used as primary key in
/// `replication_state`.
fn is_valid_replication_rule_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// Minimum legal `interval` per rule. Anything tighter would thrash
/// the scheduler and the destination backend's rate limits.
const MIN_RULE_INTERVAL_SECS: u64 = 30;

/// Minimum legal global `tick_interval`. The scheduler wakes up every
/// tick; too-frequent ticks burn CPU for no benefit.
const MIN_TICK_INTERVAL_SECS: u64 = 5;

/// Maximum batch size per rule. Above this and the worker's yield
/// becomes coarse enough that other due rules get starved.
const MAX_BATCH_SIZE: u32 = 10_000;

/// Validate the replication config and return human-readable warnings
/// (empty = all-good). NOT hard errors — the existing `Config::check`
/// contract is warnings-only; the worker refuses to act on rules that
/// this validator flagged.
pub fn validate_replication(cfg: &ReplicationConfig) -> Vec<String> {
    let mut warnings = Vec::new();

    // tick_interval parses as humantime.
    match humantime::parse_duration(&cfg.tick_interval) {
        Ok(d) => {
            let secs: u64 = d.as_secs();
            if secs < MIN_TICK_INTERVAL_SECS {
                warnings.push(format!(
                    "replication.tick_interval={} is below the minimum {}s",
                    cfg.tick_interval, MIN_TICK_INTERVAL_SECS
                ));
            }
        }
        Err(e) => {
            warnings.push(format!(
                "replication.tick_interval={} is not a valid humantime duration: {}",
                cfg.tick_interval, e
            ));
        }
    }

    // Per-rule checks.
    let mut seen_names = std::collections::HashSet::new();
    for rule in &cfg.rules {
        if !is_valid_replication_rule_name(&rule.name) {
            warnings.push(format!(
                "replication rule name '{}' is invalid (must match [A-Za-z0-9_.-]{{1,64}})",
                rule.name
            ));
            continue;
        }
        if !seen_names.insert(rule.name.clone()) {
            warnings.push(format!(
                "replication rule name '{}' is duplicated — the first entry wins",
                rule.name
            ));
            continue;
        }

        // Interval parses + meets minimum.
        match humantime::parse_duration(&rule.interval) {
            Ok(d) => {
                let secs: u64 = d.as_secs();
                if secs < MIN_RULE_INTERVAL_SECS {
                    warnings.push(format!(
                        "replication rule '{}' interval={} below minimum {}s",
                        rule.name, rule.interval, MIN_RULE_INTERVAL_SECS
                    ));
                }
            }
            Err(e) => {
                warnings.push(format!(
                    "replication rule '{}' interval={} invalid: {}",
                    rule.name, rule.interval, e
                ));
            }
        }

        if rule.batch_size == 0 || rule.batch_size > MAX_BATCH_SIZE {
            warnings.push(format!(
                "replication rule '{}' batch_size={} outside [1, {}]",
                rule.name, rule.batch_size, MAX_BATCH_SIZE
            ));
        }

        // Self-loop: source and destination reference the same
        // bucket+prefix. Degenerate — the rule would copy objects
        // back to themselves, pointlessly re-incrementing counters.
        if rule.source.bucket == rule.destination.bucket
            && rule.source.prefix == rule.destination.prefix
        {
            warnings.push(format!(
                "replication rule '{}' is a self-loop \
                 (source == destination); it will be skipped at runtime",
                rule.name
            ));
        }

        // Globs compile.
        for glob in rule.include_globs.iter().chain(rule.exclude_globs.iter()) {
            if globset::Glob::new(glob).is_err() {
                warnings.push(format!(
                    "replication rule '{}' glob pattern {:?} is invalid",
                    rule.name, glob
                ));
            }
        }
    }

    // Cycle detection across rules. A 2-hop cycle (A→B, B→A with
    // overlapping prefixes) would cause pathological write-amplification
    // because each tick finds objects to copy back.
    warnings.extend(detect_replication_cycles(&cfg.rules));

    warnings
}

/// Pure cycle detection: report all rules that form a cycle with another.
/// A cycle exists when there's a chain of rules whose source+prefix
/// graph returns to the starting bucket+prefix.
///
/// Exposed `pub(crate)` so unit tests can pin the behaviour.
pub(crate) fn detect_replication_cycles(rules: &[ReplicationRule]) -> Vec<String> {
    use std::collections::{HashMap, HashSet};

    /// `(bucket, prefix)` identifies a node in the replication graph.
    type Node = (String, String);

    // Edge list: source node -> list of (dest node, rule_name).
    let mut edges: HashMap<Node, Vec<(Node, String)>> = HashMap::new();
    for rule in rules {
        let src: Node = (rule.source.bucket.clone(), rule.source.prefix.clone());
        let dst: Node = (
            rule.destination.bucket.clone(),
            rule.destination.prefix.clone(),
        );
        edges.entry(src).or_default().push((dst, rule.name.clone()));
    }

    let mut warnings = Vec::new();
    let mut seen_cycles: HashSet<String> = HashSet::new();

    // For each node, run a bounded DFS looking for a return.
    for start in edges.keys() {
        let mut stack: Vec<(Node, Vec<String>)> = vec![(start.clone(), Vec::new())];
        let mut visited: HashSet<Node> = HashSet::new();
        while let Some((node, path)) = stack.pop() {
            if !visited.insert(node.clone()) {
                continue;
            }
            if let Some(targets) = edges.get(&node) {
                for (next, rule_name) in targets {
                    let mut next_path = path.clone();
                    next_path.push(rule_name.clone());
                    if next == start {
                        // Cycle found! Deduplicate by the sorted rule-names
                        // set so the warnings list stays clean.
                        let mut sorted = next_path.clone();
                        sorted.sort();
                        let key = sorted.join("|");
                        if seen_cycles.insert(key) {
                            warnings.push(format!(
                                "replication rules form a cycle: {}",
                                next_path.join(" -> ")
                            ));
                        }
                        continue;
                    }
                    stack.push((next.clone(), next_path));
                }
            }
        }
    }

    warnings
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

    // ────────────────────────────────────────────────────────────────
    // Replication validation + cycle detection tests
    // ────────────────────────────────────────────────────────────────

    fn rule(name: &str, src: (&str, &str), dst: (&str, &str), interval: &str) -> ReplicationRule {
        ReplicationRule {
            name: name.to_string(),
            enabled: true,
            source: ReplicationEndpoint {
                bucket: src.0.to_string(),
                prefix: src.1.to_string(),
            },
            destination: ReplicationEndpoint {
                bucket: dst.0.to_string(),
                prefix: dst.1.to_string(),
            },
            interval: interval.to_string(),
            batch_size: 100,
            replicate_deletes: false,
            conflict: ConflictPolicy::NewerWins,
            include_globs: Vec::new(),
            exclude_globs: vec![".dg/*".to_string()],
        }
    }

    #[test]
    fn replication_validation_accepts_default() {
        let warnings = validate_replication(&ReplicationConfig::default());
        assert!(warnings.is_empty(), "default should be valid: {:?}", warnings);
    }

    #[test]
    fn replication_validation_rejects_bad_rule_name() {
        let cfg = ReplicationConfig {
            rules: vec![rule("has spaces!", ("a", ""), ("b", ""), "1h")],
            ..Default::default()
        };
        let warnings = validate_replication(&cfg);
        assert!(
            warnings.iter().any(|w| w.contains("invalid")
                && w.contains("has spaces!")),
            "{:?}",
            warnings
        );
    }

    #[test]
    fn replication_validation_flags_duplicate_rule_name() {
        let cfg = ReplicationConfig {
            rules: vec![
                rule("r1", ("a", ""), ("b", ""), "1h"),
                rule("r1", ("c", ""), ("d", ""), "1h"),
            ],
            ..Default::default()
        };
        let warnings = validate_replication(&cfg);
        assert!(warnings.iter().any(|w| w.contains("duplicated")));
    }

    #[test]
    fn replication_validation_rejects_too_short_interval() {
        let cfg = ReplicationConfig {
            rules: vec![rule("r1", ("a", ""), ("b", ""), "5s")],
            ..Default::default()
        };
        let warnings = validate_replication(&cfg);
        assert!(warnings.iter().any(|w| w.contains("below minimum")));
    }

    #[test]
    fn replication_validation_rejects_bad_interval() {
        let cfg = ReplicationConfig {
            rules: vec![rule("r1", ("a", ""), ("b", ""), "not-a-duration")],
            ..Default::default()
        };
        let warnings = validate_replication(&cfg);
        assert!(warnings.iter().any(|w| w.contains("invalid")));
    }

    #[test]
    fn replication_validation_flags_self_loop() {
        let cfg = ReplicationConfig {
            rules: vec![rule("loop", ("a", "pfx"), ("a", "pfx"), "1h")],
            ..Default::default()
        };
        let warnings = validate_replication(&cfg);
        assert!(warnings.iter().any(|w| w.contains("self-loop")));
    }

    #[test]
    fn replication_validation_detects_two_hop_cycle() {
        let cfg = ReplicationConfig {
            rules: vec![
                rule("a-to-b", ("bkt_a", ""), ("bkt_b", ""), "1h"),
                rule("b-to-a", ("bkt_b", ""), ("bkt_a", ""), "1h"),
            ],
            ..Default::default()
        };
        let warnings = validate_replication(&cfg);
        let cycle_warns: Vec<_> = warnings
            .iter()
            .filter(|w| w.contains("cycle"))
            .collect();
        assert!(!cycle_warns.is_empty(), "expected cycle warning: {:?}", warnings);
    }

    #[test]
    fn replication_validation_non_cycle_fan_out_is_ok() {
        let cfg = ReplicationConfig {
            rules: vec![
                rule("a-to-b", ("a", ""), ("b", ""), "1h"),
                rule("a-to-c", ("a", ""), ("c", ""), "1h"),
                rule("a-to-d", ("a", ""), ("d", ""), "1h"),
            ],
            ..Default::default()
        };
        let warnings = validate_replication(&cfg);
        assert!(
            warnings.iter().all(|w| !w.contains("cycle")),
            "fan-out is not a cycle: {:?}",
            warnings
        );
    }

    #[test]
    fn replication_validation_rejects_bad_glob() {
        let mut cfg = ReplicationConfig {
            rules: vec![rule("r", ("a", ""), ("b", ""), "1h")],
            ..Default::default()
        };
        cfg.rules[0].include_globs = vec!["[badbracket".to_string()];
        let warnings = validate_replication(&cfg);
        assert!(warnings.iter().any(|w| w.contains("glob")));
    }

    #[test]
    fn replication_validation_flags_batch_size_zero() {
        let mut cfg = ReplicationConfig {
            rules: vec![rule("r", ("a", ""), ("b", ""), "1h")],
            ..Default::default()
        };
        cfg.rules[0].batch_size = 0;
        let warnings = validate_replication(&cfg);
        assert!(warnings.iter().any(|w| w.contains("batch_size")));
    }

    #[test]
    fn replication_roundtrip_serde() {
        let cfg = ReplicationConfig {
            enabled: true,
            tick_interval: "30s".to_string(),
            max_failures_retained: 100,
            rules: vec![rule("r", ("a", ""), ("b", ""), "1h")],
        };
        let yaml = serde_yaml::to_string(&cfg).unwrap();
        let back: ReplicationConfig = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(cfg, back);
    }
}
