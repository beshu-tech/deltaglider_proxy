// SPDX-License-Identifier: BUSL-1.1

//! Configuration for DeltaGlider Proxy S3 server

pub mod advisories;
mod check;
pub use check::{DocumentRefusal, RuleGateRefusal};
mod env;
pub mod env_overrides;
pub mod env_shadow;
mod expansion;
mod lenient;

pub use env::*;
pub use expansion::*;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

mod registry;
pub use registry::*;
pub mod test_seams;
pub mod tuning;
pub use tuning::RuntimeTuning;

/// Default YAML config filename.
pub const DEFAULT_YAML_CONFIG_FILENAME: &str = "deltaglider_proxy.yaml";

/// Error message for any attempt to load or persist a `.toml` config.
/// TOML support was removed in v1.4.1; the message names the one-time
/// conversion path so operators are never left guessing.
pub const TOML_REMOVED_MSG: &str = "TOML configs are no longer supported (removed in v1.4.1). \
     Convert with `deltaglider_proxy config migrate` on v1.4.0, then point the server at the \
     YAML file.";

/// True when the path carries a `.toml` extension (case-insensitive).
/// Used to reject removed-format configs loudly instead of silently
/// ignoring them — see [`TOML_REMOVED_MSG`].
pub fn path_is_toml(path: &str) -> bool {
    std::path::Path::new(path)
        .extension()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.eq_ignore_ascii_case("toml"))
}

/// Placeholder substituted for secret VALUES that must keep their KEY visible in
/// a redacted export (so the GUI can show *which* secrets exist while masking the
/// value). Used for `event_delivery.webhook_headers` values. The section-PUT
/// preserve path treats an incoming value equal to this sentinel as "unchanged,
/// keep the runtime secret" — see `preserve_event_delivery_secrets`. SigV4 and
/// backend creds use `None`-omission instead (no key to preserve).
pub const REDACTED_SENTINEL: &str = "__redacted__";

/// Ordered list of default config file locations. The `.toml` entries
/// are TRIPWIRES, not loadable formats: a leftover TOML config found on
/// disk (with no YAML earlier in the order) makes startup fail loudly
/// with [`TOML_REMOVED_MSG`] instead of being silently ignored.
pub const DEFAULT_CONFIG_SEARCH_PATHS: &[&str] = &[
    DEFAULT_YAML_CONFIG_FILENAME,
    "deltaglider_proxy.yml",
    "deltaglider_proxy.toml",
    "/etc/deltaglider_proxy/config.yaml",
    "/etc/deltaglider_proxy/config.yml",
    "/etc/deltaglider_proxy/config.toml",
];

/// Thread-safe shared config for hot-reload from admin GUI.
pub type SharedConfig = Arc<tokio::sync::RwLock<Config>>;

/// Pinned default-posture version. Absent in a config file means "use whatever
/// the running server considers current"; setting it explicitly opts the
/// deployment out of silent default changes across upgrades.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum DefaultsVersion {
    #[default]
    V1,
}

impl DefaultsVersion {
    pub(crate) fn is_default(&self) -> bool {
        matches!(self, DefaultsVersion::V1)
    }
}

/// Server configuration
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Config {
    /// Pin the defaults posture to a specific version. Omitted in the file =
    /// inherit whatever the running server considers current. Set to `v1`
    /// to pin explicitly and receive a warning if the server ships new
    /// defaults in a future release.
    #[serde(
        default,
        rename = "defaults",
        skip_serializing_if = "DefaultsVersion::is_default"
    )]
    pub defaults_version: DefaultsVersion,

    /// Address to listen on
    #[serde(default = "default_listen_addr")]
    pub listen_addr: SocketAddr,

    /// Storage backend configuration (legacy singleton path).
    ///
    /// When `backends` (the multi-backend list) is non-empty this field
    /// is ignored — the engine uses the list. When the list is empty,
    /// this singleton is wrapped and registered as the sole backend
    /// under the synthetic name `"default"`.
    #[serde(default)]
    pub backend: BackendConfig,

    /// Per-backend encryption-at-rest config for the legacy singleton
    /// `backend` field above. Ignored when `backends` (the list) is
    /// non-empty — in that case each entry's own `encryption` field
    /// applies.
    ///
    /// Lives at the flat root (not nested under `backend`) because
    /// `BackendConfig` is a tagged enum (`type: s3|filesystem`) and
    /// `#[serde(flatten)]` doesn't compose cleanly with enum tags; so
    /// encryption rides as a sibling instead. The sectioned YAML shape
    /// surfaces it under `storage.backend_encryption`.
    #[serde(default, skip_serializing_if = "is_default_encryption")]
    pub backend_encryption: BackendEncryptionConfig,

    /// Maximum delta ratio (store as delta only if ratio < this value)
    #[serde(default = "default_max_delta_ratio")]
    pub max_delta_ratio: f32,

    /// Maximum object size in bytes (xdelta3 memory constraint)
    #[serde(default = "default_max_object_size")]
    pub max_object_size: u64,

    /// Maximum passthrough object size for the streaming multipart paths
    /// (Phase B). Decoupled from `max_object_size` (which gates the
    /// buffered delta/reference RAM ceiling) because streaming copies are
    /// O(part_size) in memory and can handle far larger objects. Default
    /// 64 GiB.
    #[serde(default = "default_max_passthrough_object_size")]
    pub max_passthrough_object_size: u64,

    /// Seconds a verified reconstruction of a large delta object stays
    /// cached, so the other range reads of the object skip the decode
    /// (storage-11). `0` turns the cache off. Default 60.
    #[serde(default = "default_range_spool_ttl_secs")]
    pub range_spool_ttl_secs: u64,

    /// Reference cache size in MB
    #[serde(default = "default_cache_size_mb")]
    pub cache_size_mb: usize,

    /// Metadata cache size in MB (object metadata, eliminates HEAD requests).
    /// Default: 50 MB (~125K entries). Set to 0 to disable.
    #[serde(default = "default_metadata_cache_mb")]
    pub metadata_cache_mb: usize,

    /// Engine pages one filtered LIST (a policy the proxy cannot narrow to
    /// prefixes) may scan before it answers "use a narrower prefix".
    #[serde(default = "default_filtered_list_max_engine_pages")]
    pub filtered_list_max_engine_pages: usize,

    /// Explicit authentication mode selector.
    ///
    /// Accepted values:
    ///   - `"none"` — Open access: no identity. Must be explicit. A signed
    ///     request is still verified, with the access key as the secret.
    ///
    /// When absent, the proxy infers the mode from credentials:
    ///   - Credentials present → bootstrap or IAM mode (auto-detected)
    ///   - Credentials absent → **FATAL error** (proxy refuses to start)
    ///
    /// Future values: `"oidc"`, `"ldap"`, `"saml"`, or combinations.
    #[serde(default)]
    pub authentication: Option<String>,

    /// Proxy access key ID for SigV4 authentication.
    /// When both access_key_id and secret_access_key are set, all requests
    /// must be SigV4-signed with these credentials.
    #[serde(default)]
    pub access_key_id: Option<String>,

    /// Proxy secret access key for SigV4 authentication.
    /// Must be set together with access_key_id.
    #[serde(default)]
    pub secret_access_key: Option<String>,

    /// Bcrypt hash of the bootstrap password.
    /// Seeds DB encryption, admin GUI access, and session signing.
    /// Set via DGP_BOOTSTRAP_PASSWORD_HASH (or legacy DGP_ADMIN_PASSWORD_HASH).
    #[serde(default, alias = "admin_password_hash")]
    pub bootstrap_password_hash: Option<String>,

    /// Maximum concurrent delta encode/decode operations.
    /// Defaults to the number of available CPU cores.
    #[serde(default)]
    pub codec_concurrency: Option<usize>,

    /// Maximum blocking threads for the tokio runtime.
    /// Defaults to tokio's built-in default (512).
    #[serde(default)]
    pub blocking_threads: Option<usize>,

    /// Log level filter string.
    /// Set via config file, DGP_LOG_LEVEL env var, or admin GUI. Overridden by RUST_LOG.
    /// Default: [`DEFAULT_LOG_LEVEL`] (info).
    #[serde(default = "default_log_level")]
    pub log_level: String,

    /// S3 bucket for config DB sync (multi-instance IAM).
    /// When set, the encrypted config DB is synced to/from this S3 bucket.
    #[serde(default)]
    pub config_sync_bucket: Option<String>,

    /// S3 object key for the synced config DB (default `.deltaglider/config.db`).
    /// `DGP_CONFIG_SYNC_KEY` overrides this when set.
    #[serde(default)]
    pub config_sync_object_key: Option<String>,

    /// TLS configuration (optional).
    /// When enabled, both the S3 port and the demo UI port serve HTTPS.
    #[serde(default)]
    pub tls: Option<TlsConfig>,

    /// Per-bucket policy overrides.
    /// Each entry overrides global compression settings for a specific bucket.
    /// Unconfigured buckets inherit the global defaults.
    ///
    /// `BTreeMap` (not `HashMap`) is deliberate: canonical YAML export must
    /// be byte-stable across runs and across processes so that GitOps
    /// diffing, CI round-trip checks, and copy-as-YAML exports are
    /// reproducible. `HashMap` iteration order depends on per-process
    /// seed state, which would flake any artifact-compare pipeline.
    #[serde(default)]
    pub buckets: std::collections::BTreeMap<String, crate::bucket_policy::BucketPolicyConfig>,

    /// Named backends for multi-backend routing.
    /// When non-empty, the legacy `backend` field is ignored.
    #[serde(default)]
    pub backends: Vec<NamedBackendConfig>,

    /// Name of the default backend (used for buckets without explicit routing).
    /// Must reference a name in `backends`. Defaults to the first entry.
    #[serde(default)]
    pub default_backend: Option<String>,

    /// Lazy bucket replication configuration (v1: scheduled one-way copy
    /// via the engine). See [`crate::config_sections::ReplicationConfig`]
    /// for the full shape; rules are validated in `Config::check`.
    #[serde(default)]
    pub replication: crate::config_sections::ReplicationConfig,

    /// Delete-only lifecycle expiration rules. Disabled by default and
    /// engine-routed so DeltaGlider internals stay hidden from deletion
    /// planning. See [`crate::config_sections::LifecycleConfig`].
    #[serde(default)]
    pub lifecycle: crate::config_sections::LifecycleConfig,

    /// Durable event outbox delivery. Disabled by default; when enabled a
    /// background dispatcher POSTs event rows to a configured webhook.
    #[serde(default)]
    pub event_delivery: crate::config_sections::EventDeliveryConfig,

    /// One lease setting for the job leases (`advanced.jobs`). See
    /// [`crate::config_sections::JobsConfig`].
    #[serde(default)]
    pub jobs: crate::config_sections::JobsConfig,

    /// Operator-authored admission blocks.
    ///
    /// Parsed from `admission.blocks:` in the sectioned YAML OR from
    /// `admission_blocks:` at the root of a flat-shape YAML (both
    /// shapes round-trip through `from_yaml_str` → `to_canonical_yaml`).
    ///
    /// The admission chain builder ([`crate::admission::AdmissionChain::from_config_parts`])
    /// compiles these into runtime [`crate::admission::Match::Predicates`]
    /// blocks that fire BEFORE the synthesised public-prefix blocks
    /// derived from `buckets:`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub admission_blocks: Vec<crate::admission::AdmissionBlockSpec>,

    /// IAM source-of-truth selector (Phase 3c.1).
    ///
    /// Parsed from `access.iam_mode:` in the sectioned YAML. The flat
    /// shape also accepts `iam_mode:` at the root for round-trip
    /// symmetry. `Gui` default means existing deployments keep
    /// DB-authoritative semantics; operators explicitly opt into
    /// `declarative` to make YAML authoritative.
    #[serde(
        default,
        skip_serializing_if = "crate::config_sections::IamMode::is_default"
    )]
    pub iam_mode: crate::config_sections::IamMode,

    // ── Phase 3c.3: declarative-mode IAM fields ──
    //
    // Consumed by the reconciler inside `apply_config_transition`
    // when `iam_mode == Declarative`. In `Gui` mode these are
    // tolerated but not applied (the DB is source of truth).
    /// IAM users as declared in YAML (Phase 3c.3).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub iam_users: Vec<crate::iam::DeclarativeUser>,
    /// IAM groups as declared in YAML (Phase 3c.3).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub iam_groups: Vec<crate::iam::DeclarativeGroup>,
    /// External auth providers declared in YAML (Phase 3c.3).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub auth_providers: Vec<crate::iam::DeclarativeAuthProvider>,
    /// OAuth group-mapping rules declared in YAML (Phase 3c.3).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub group_mapping_rules: Vec<crate::iam::DeclarativeMappingRule>,

    /// `${env:NAME}` provenance: `name → value` for every env ref the loaded
    /// document resolved from the environment. NEVER serialized — it exists
    /// so persist/export can re-emit `${env:NAME}` instead of the
    /// materialized secret, making the IaC round-trip lossless:
    /// provision a secret-free template → tweak via the GUI (persisted with
    /// refs intact) → export → put the export back into IaC.
    ///
    /// Populated by [`Self::from_yaml_file`] and
    /// by the admin document-apply path (which expands against the SERVER
    /// environment and merges the previous config's provenance). Refs whose
    /// `:-default` was used are not recorded (see
    /// [`expand_env_vars_recording`]).
    #[serde(skip)]
    #[schemars(skip)]
    pub env_refs: std::collections::BTreeMap<String, String>,

    /// What the config FILE had in every slot a `DGP_*` variable overrode
    /// (see [`env_shadow`]). NEVER serialized. Persist and export restore
    /// these values ([`Self::file_view`]) so an env value — a secret above
    /// all — never reaches the YAML file.
    #[serde(skip)]
    #[schemars(skip)]
    pub env_shadow: env_shadow::EnvShadow,

    /// Env-only settings the request path reads (see [`tuning`]). NEVER
    /// serialized: `Self::apply_env_overrides_tracked` fills it from the
    /// environment, at boot and on every admin apply.
    #[serde(skip)]
    #[schemars(skip)]
    pub tuning: RuntimeTuning,
}

/// Per-backend encryption-at-rest configuration.
///
/// Replaces the former global `advanced.encryption_key` single-key model.
/// Each named backend declares its OWN mode; the engine wraps each
/// backend independently (see `src/deltaglider/engine/construction.rs` backend
/// registry construction). Four modes:
///
/// - `None` — objects stored plaintext.
/// - `Aes256GcmProxy` — proxy-side AES-256-GCM (the wrapper encrypts
///   before bytes hit the backend). `key` is the 256-bit hex; `key_id`
///   is stamped on each written object so reads can detect "this object
///   was encrypted with a different key" and emit a specific error
///   instead of an opaque AEAD failure. `legacy_key`/`legacy_key_id`
///   support the decrypt-only shim during mode transitions.
/// - `SseKms` — delegate to S3 native SSE-KMS. Bytes are encrypted by
///   AWS before landing on disk; proxy doesn't wrap.
/// - `SseS3` — delegate to S3 native AES256 (no KMS involvement).
///
/// Name mixing in the default key_id derivation (see
/// `derive_key_id` in engine/construction.rs) is load-bearing: two
/// backends with identical `key` bytes but different names produce
/// DIFFERENT ids, so objects are NOT accidentally portable between
/// them. Operators who want portability set an explicit identical
/// `key_id` on both.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "mode", rename_all = "kebab-case")]
pub enum BackendEncryptionConfig {
    /// Objects stored plaintext on this backend.
    ///
    /// `legacy_key`/`legacy_key_id` keep decrypting historical objects
    /// after an operator disables encryption on a backend that used to
    /// be `aes256-gcm-proxy`. Without this shim, any object written
    /// under the old mode becomes unreadable the moment `mode: none`
    /// takes effect — the wrapper's read path would see `dg-encrypted`
    /// metadata and no key to decrypt with, returning 500. Documented
    /// as recipe (D) in `reference/encryption-at-rest.md`.
    None {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        legacy_key: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        legacy_key_id: Option<String>,
    },

    /// Proxy-side AES-256-GCM. `key` is hex-encoded 256 bits.
    /// `key_id` is stamped on each object's `dg-encryption-key-id`
    /// metadata; derived automatically from `SHA-256(backend_name || key)`
    /// when absent. `legacy_key`/`legacy_key_id` provide a decrypt-only
    /// shim during mode transitions (see engine/construction.rs resolver).
    Aes256GcmProxy {
        /// 64-char hex key. Infra secret; stripped by redactors.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
        /// Optional stable id. Max 64 chars, `[A-Za-z0-9_.-]` only
        /// (S3 user-metadata header-safe).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key_id: Option<String>,
        /// Decrypt-only shim: preserve the ability to READ objects
        /// that were written with a previous key (after a rotation, or
        /// a transition to a native mode). A config apply that changes
        /// `key` fills it with the retired key when the body leaves it
        /// out (`preserve_backend_encryption_secrets`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        legacy_key: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        legacy_key_id: Option<String>,
    },

    /// S3 native SSE-KMS. Proxy does not wrap; AWS encrypts on write.
    /// Stamps `dg-encrypted-native: sse-kms` in user metadata so the
    /// read-side sanity check (xattr-strip defense) knows this object
    /// was not proxy-encrypted.
    SseKms {
        /// KMS key ARN or alias. Required.
        kms_key_id: String,
        /// Enable S3 bucket keys (reduces KMS cost on bursty traffic).
        #[serde(
            default = "crate::types::default_true",
            deserialize_with = "lenient::bool_or_string"
        )]
        #[schemars(schema_with = "lenient::bool_or_string_schema")]
        bucket_key_enabled: bool,
        /// Decrypt-only shim: keep reading objects written with the
        /// old proxy-mode key after migrating to SSE-KMS.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        legacy_key: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        legacy_key_id: Option<String>,
    },

    /// S3 native SSE-S3 (AES-256, AWS-managed keys). No KMS.
    SseS3 {
        /// Decrypt-only shim — see `SseKms::legacy_key`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        legacy_key: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        legacy_key_id: Option<String>,
    },
}

/// `skip_serializing_if` helper: backends with encryption `None` (and
/// no legacy shim) omit the field entirely in canonical YAML. A
/// `None { legacy_key: Some(...) }` DOES serialise so the shim stays
/// visible to operators and survives a round-trip through the
/// canonical exporter.
pub(crate) fn is_default_encryption(e: &BackendEncryptionConfig) -> bool {
    matches!(
        e,
        BackendEncryptionConfig::None {
            legacy_key: None,
            legacy_key_id: None,
        }
    )
}

impl Default for BackendEncryptionConfig {
    fn default() -> Self {
        Self::None {
            legacy_key: None,
            legacy_key_id: None,
        }
    }
}

impl BackendEncryptionConfig {
    /// Short machine-readable tag for the mode, used by admin API
    /// summaries and diff renderers. Matches the YAML `mode:` tag.
    pub fn mode_tag(&self) -> &'static str {
        match self {
            Self::None { .. } => "none",
            Self::Aes256GcmProxy { .. } => "aes256-gcm-proxy",
            Self::SseKms { .. } => "sse-kms",
            Self::SseS3 { .. } => "sse-s3",
        }
    }

    /// Strip every secret key material (used by redactors). Preserves
    /// mode + non-secret identifiers (`key_id`, `kms_key_id`) so the
    /// redacted surface still conveys "this backend is encrypted with
    /// mode X" to operators reading the exported YAML.
    pub fn redact_secrets(&mut self) {
        // `${env:NAME}` references survive redaction — a reference is not a
        // secret, and stripping it would break the env-ref round-trip.
        fn clear_unless_ref(slot: &mut Option<String>) {
            if !slot.as_deref().is_some_and(is_env_ref) {
                *slot = None;
            }
        }
        match self {
            Self::None { legacy_key, .. } => {
                clear_unless_ref(legacy_key);
            }
            Self::Aes256GcmProxy {
                key, legacy_key, ..
            } => {
                clear_unless_ref(key);
                clear_unless_ref(legacy_key);
            }
            Self::SseKms { legacy_key, .. } => {
                // kms_key_id is an ARN — NOT secret. Operators need to
                // see it to know WHICH KMS key (so it's left in the `..`).
                clear_unless_ref(legacy_key);
            }
            Self::SseS3 { legacy_key, .. } => {
                clear_unless_ref(legacy_key);
            }
        }
    }

    /// Accessor for the primary `key` hex (only present on
    /// `Aes256GcmProxy`). Returns `None` on every other variant —
    /// that's the variants where "primary key" is semantically
    /// meaningless (None) or delegated to AWS (SseKms/SseS3).
    pub fn primary_key(&self) -> Option<&str> {
        match self {
            Self::Aes256GcmProxy { key, .. } => key.as_deref(),
            Self::None { .. } | Self::SseKms { .. } | Self::SseS3 { .. } => None,
        }
    }
    /// Accessor for the shim `legacy_key` hex. Present on every
    /// variant (the shim decrypts historical objects after any mode
    /// change, including "encryption disabled" — recipe (D) in the
    /// reference docs).
    pub fn legacy_key(&self) -> Option<&str> {
        match self {
            Self::None { legacy_key, .. }
            | Self::Aes256GcmProxy { legacy_key, .. }
            | Self::SseKms { legacy_key, .. }
            | Self::SseS3 { legacy_key, .. } => legacy_key.as_deref(),
        }
    }
    /// Accessor for the shim `legacy_key_id`. Matches `legacy_key` on
    /// variant coverage — the pair is always present or absent
    /// together.
    pub fn legacy_key_id(&self) -> Option<&str> {
        match self {
            Self::None { legacy_key_id, .. }
            | Self::Aes256GcmProxy { legacy_key_id, .. }
            | Self::SseKms { legacy_key_id, .. }
            | Self::SseS3 { legacy_key_id, .. } => legacy_key_id.as_deref(),
        }
    }
    /// Mutable accessor for the `legacy_key` slot — used by
    /// `preserve_backend_encryption_secrets` to re-populate the old
    /// key when the operator's edit omitted it. Available on every
    /// variant since `legacy_key` is a valid shim carrier on all of
    /// them (including `None`, per recipe (D)).
    pub fn legacy_key_mut(&mut self) -> Option<&mut Option<String>> {
        match self {
            Self::None { legacy_key, .. }
            | Self::Aes256GcmProxy { legacy_key, .. }
            | Self::SseKms { legacy_key, .. }
            | Self::SseS3 { legacy_key, .. } => Some(legacy_key),
        }
    }
}

/// Check whether a `key_id` string matches the documented charset
/// `[A-Za-z0-9_.-]{1,64}`. Pure function, unit-tested below.
///
/// Stored into S3 as `x-amz-meta-dg-encryption-key-id` — header values
/// must be printable ASCII and ideally header-safe. Restrict to the
/// intersection of "printable" and "survives all tools" (DNS label
/// shape basically).
pub(crate) fn is_valid_key_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// Normalise a backend name into an env-var-safe suffix: uppercase,
/// `-`/`.` → `_`. So `"eu-archive"` → `"EU_ARCHIVE"` → env var
/// `DGP_BACKEND_EU_ARCHIVE_ENCRYPTION_KEY`.
///
/// The synthetic name `"default"` (used by the singleton backend
/// path) gets the un-prefixed env-var names `DGP_ENCRYPTION_KEY` /
/// `DGP_SSE_KMS_KEY_ID` so single-backend deployments keep the short
/// names operators are used to.
///
/// Shared with `engine::env_name_for_backend` (the
/// `DGP_BACKEND_<NAME>_ENCRYPTION_KEY` formatter) so the char-mapping
/// rules live in one place. Drift risk: an operator's env var no
/// longer matches the named backend if the two functions diverge.
pub(crate) fn env_suffix_for_backend_name(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '-' | '.' => '_',
            c => c.to_ascii_uppercase(),
        })
        .collect()
}

/// Apply env-var overrides to a single backend's encryption config.
/// Only touches the SECRET fields (`key`, `kms_key_id`) — the mode
/// itself stays authoritative in YAML. Called once per backend
/// (including the synthetic "default" for the singleton path).
/// Apply the per-backend encryption env var to `enc`. Returns the name of
/// the field it set (`key` or `kms_key_id`), or `None` when nothing applied.
pub(crate) fn apply_backend_encryption_env(
    backend_name: &str,
    enc: &mut BackendEncryptionConfig,
    env: EnvLookup,
) -> Option<&'static str> {
    let (key_env, kms_env) = backend_encryption_env_names(backend_name);
    let set = |name: &str| env(name).filter(|v| !v.is_empty());
    match enc {
        BackendEncryptionConfig::Aes256GcmProxy { key, .. } => {
            let v = set(&key_env)?;
            *key = Some(v);
            Some("key")
        }
        BackendEncryptionConfig::SseKms { kms_key_id, .. } => {
            let v = set(&kms_env)?;
            *kms_key_id = v;
            Some("kms_key_id")
        }
        // None and SseS3 carry no secrets beyond the legacy shim;
        // env vars for primary-key material are silently ignored
        // (the shim key stays in YAML/config and is not env-overridden).
        BackendEncryptionConfig::None { .. } | BackendEncryptionConfig::SseS3 { .. } => None,
    }
}

/// `(encryption key var, SSE-KMS key id var)` for a backend. The singleton
/// ("default") uses the unadorned names.
pub(crate) fn backend_encryption_env_names(backend_name: &str) -> (String, String) {
    if backend_name == "default" {
        (
            "DGP_ENCRYPTION_KEY".to_string(),
            "DGP_SSE_KMS_KEY_ID".to_string(),
        )
    } else {
        let suf = env_suffix_for_backend_name(backend_name);
        (
            format!("DGP_BACKEND_{}_ENCRYPTION_KEY", suf),
            format!("DGP_BACKEND_{}_SSE_KMS_KEY_ID", suf),
        )
    }
}

/// A named storage backend with its connection configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NamedBackendConfig {
    /// Human-readable name (e.g., "local", "hetzner", "aws")
    pub name: String,
    /// The actual backend configuration
    #[serde(flatten)]
    pub backend: BackendConfig,
    /// Per-backend encryption-at-rest configuration. Defaults to
    /// `mode: none` — plaintext. Omitted from canonical YAML when
    /// default.
    #[serde(default, skip_serializing_if = "is_default_encryption")]
    pub encryption: BackendEncryptionConfig,
}

/// TLS configuration (optional)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TlsConfig {
    /// Enable TLS
    #[serde(default)]
    pub enabled: bool,
    /// Path to PEM certificate file (optional — auto-generates self-signed if omitted)
    #[serde(default)]
    pub cert_path: Option<String>,
    /// Path to PEM private key file (required if cert_path is set)
    #[serde(default)]
    pub key_path: Option<String>,
}

/// Storage backend configuration
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum BackendConfig {
    /// Filesystem backend for local storage/development
    Filesystem {
        /// Directory for data storage
        path: PathBuf,
    },

    /// S3 backend for production use
    S3 {
        /// S3 endpoint URL (for MinIO, LocalStack, or custom S3-compatible services)
        /// If not specified, uses AWS default endpoint
        #[serde(default)]
        endpoint: Option<String>,

        /// AWS region
        #[serde(default = "default_region")]
        region: String,

        /// Use path-style URLs (required for MinIO, LocalStack)
        #[serde(
            default = "default_force_path_style",
            deserialize_with = "lenient::bool_or_string"
        )]
        #[schemars(schema_with = "lenient::bool_or_string_schema")]
        force_path_style: bool,

        /// AWS access key ID (optional, can use env/instance credentials)
        #[serde(default)]
        access_key_id: Option<String>,

        /// AWS secret access key (optional, can use env/instance credentials)
        #[serde(default)]
        secret_access_key: Option<String>,

        /// Permit `http://` and private-IP / localhost endpoints when set.
        /// Off by default: the SSRF guard at `src/storage/s3/client.rs` rejects
        /// such endpoints to prevent admin-API abuse pivoting through the
        /// S3 backend (e.g. swapping the endpoint to AWS IMDS).
        /// Set to true for MinIO/dev/CI; production must keep false.
        /// Backward-compat: when false, the legacy `DGP_BACKEND_ALLOW_LOCAL`
        /// env var still grants permission (so existing deployments work
        /// unchanged); explicitly setting `allow_local: true` in the config
        /// is the preferred path going forward.
        #[serde(
            default,
            skip_serializing_if = "is_false",
            deserialize_with = "lenient::bool_or_string"
        )]
        #[schemars(schema_with = "lenient::bool_or_string_schema")]
        allow_local: bool,

        /// Session token of temporary (STS) credentials. Runtime only:
        /// the CLI sets it from `AWS_SESSION_TOKEN` / the profile. Never
        /// read from or written to config, so no format change.
        #[serde(skip)]
        #[schemars(skip)]
        session_token: Option<String>,
    },
}

#[inline]
fn is_false(b: &bool) -> bool {
    !*b
}

// Default value functions for serde.
//
// `pub(crate)` so `config_sections.rs` can reuse them — the sectioned
// shape needs the same defaults to decide whether to emit an explicit
// value or omit the field (serde's `skip_serializing_if`). Keeping a
// single source of truth prevents drift between the flat and sectioned
// shapes, which the `round_trips_default_config` test would otherwise
// have to catch after the fact.
pub(crate) fn default_listen_addr() -> SocketAddr {
    "0.0.0.0:9000".parse().unwrap()
}

pub(crate) fn default_max_delta_ratio() -> f32 {
    0.75
}

pub(crate) fn default_max_object_size() -> u64 {
    100 * 1024 * 1024 // 100MB
}

pub(crate) fn default_max_passthrough_object_size() -> u64 {
    64 * 1024 * 1024 * 1024 // 64 GiB
}

pub(crate) fn default_range_spool_ttl_secs() -> u64 {
    60
}

pub(crate) fn default_cache_size_mb() -> usize {
    100
}

pub(crate) fn default_metadata_cache_mb() -> usize {
    50
}

pub(crate) fn default_filtered_list_max_engine_pages() -> usize {
    50
}

fn default_region() -> String {
    "us-east-1".to_string()
}

fn default_force_path_style() -> bool {
    true
}

/// The log filter when neither `RUST_LOG`, `DGP_LOG_LEVEL` nor the file
/// sets one. Startup and `--init` use it too.
pub const DEFAULT_LOG_LEVEL: &str = "deltaglider_proxy=info,tower_http=info";

pub(crate) fn default_log_level() -> String {
    DEFAULT_LOG_LEVEL.to_string()
}

impl Default for BackendConfig {
    fn default() -> Self {
        BackendConfig::Filesystem {
            path: PathBuf::from("./data"),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            defaults_version: DefaultsVersion::default(),
            listen_addr: default_listen_addr(),
            backend: BackendConfig::default(),
            max_delta_ratio: default_max_delta_ratio(),
            max_object_size: default_max_object_size(),
            max_passthrough_object_size: default_max_passthrough_object_size(),
            range_spool_ttl_secs: default_range_spool_ttl_secs(),
            cache_size_mb: default_cache_size_mb(),
            metadata_cache_mb: default_metadata_cache_mb(),
            filtered_list_max_engine_pages: default_filtered_list_max_engine_pages(),
            authentication: None,
            access_key_id: None,
            secret_access_key: None,
            bootstrap_password_hash: None,
            codec_concurrency: None,
            blocking_threads: None,
            log_level: default_log_level(),
            config_sync_bucket: None,
            config_sync_object_key: None,
            tls: None,
            buckets: std::collections::BTreeMap::new(),
            backends: Vec::new(),
            default_backend: None,
            backend_encryption: BackendEncryptionConfig::default(),
            replication: crate::config_sections::ReplicationConfig::default(),
            lifecycle: crate::config_sections::LifecycleConfig::default(),
            event_delivery: crate::config_sections::EventDeliveryConfig::default(),
            jobs: crate::config_sections::JobsConfig::default(),
            admission_blocks: Vec::new(),
            iam_mode: crate::config_sections::IamMode::default(),
            iam_users: Vec::new(),
            iam_groups: Vec::new(),
            auth_providers: Vec::new(),
            group_mapping_rules: Vec::new(),
            env_refs: std::collections::BTreeMap::new(),
            env_shadow: env_shadow::EnvShadow::default(),
            tuning: RuntimeTuning::default(),
        }
    }
}

/// Outcome of [`Config::classify_auth_config`]. The caller maps each
/// variant to the appropriate logging + (for the fatal cases) process exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthConfigOutcome {
    /// SigV4 credentials are configured → auth is ON. `redundant_none` is
    /// true if `authentication = "none"` was also set (ignored — worth a note).
    CredentialsEnabled { redundant_none: bool },
    /// No bootstrap pair, but IAM users exist (declarative `iam_users`, or
    /// users in the config DB) → auth is ON in IAM mode. `redundant_none`:
    /// `authentication: none` was also set (overridden by the IAM users).
    IamUsers { redundant_none: bool },
    /// No credentials, `authentication = "none"` → explicit open access.
    OpenAccess,
    /// No credentials, an unrecognised `authentication` value → FATAL.
    UnrecognizedMode,
    /// No credentials AND no `authentication` field → FATAL (refuse to start).
    Missing,
}

impl AuthConfigOutcome {
    /// The lines the startup prints after "FATAL: …" for a fatal outcome:
    /// the YAML the operator can add. `None` for the outcomes that start.
    pub fn fatal_help(&self) -> Option<&'static [&'static str]> {
        match self {
            Self::Missing => Some(&[
                "  The proxy refuses to start without explicit authentication configuration.",
                "  This prevents accidental exposure of S3 data.",
                "",
                "  Options:",
                "    1. Set S3 credentials (recommended):",
                "       access:",
                "         access_key_id: \"...\"",
                "         secret_access_key: \"...\"",
                "",
                "    2. Declare IAM users (access.iam_users with iam_mode: declarative),",
                "       or create IAM users in the admin GUI first: once users exist in the",
                "       config DB, the bootstrap pair is optional.",
                "",
                "    3. Explicitly allow open access (development only):",
                "       access:",
                "         authentication: none",
                "",
                "  Environment variables:",
                "    DGP_ACCESS_KEY_ID + DGP_SECRET_ACCESS_KEY, or DGP_AUTHENTICATION=none",
            ]),
            Self::UnrecognizedMode => Some(&[
                "  Accepted values:",
                "    access:",
                "      authentication: none   # open access (development only)",
                "    (omit the field to auto-detect from credentials)",
                "",
                "  Or set S3 credentials instead:",
                "    access:",
                "      access_key_id: \"...\"",
                "      secret_access_key: \"...\"",
            ]),
            Self::CredentialsEnabled { .. } | Self::IamUsers { .. } | Self::OpenAccess => None,
        }
    }
}

/// Classification of a parsed YAML document's top-level shape.
///
/// The [`ConfigShape::Mixed`] variant is a hard error: silently picking
/// one shape over the other would drop half of what the operator wrote.
/// We surface the conflicting keys in the error message so they can tell
/// at a glance which half was accidental.
enum ConfigShape {
    Sectioned,
    Flat,
    /// The doc mixes root-level flat keys (`listen_addr:`, `backend:`,
    /// …) and at least one Phase 3+ section header (`admission:`,
    /// `access:`, `storage:`, `advanced:`). Operator error — reject.
    Mixed {
        flat_keys: Vec<String>,
        section_keys: Vec<String>,
    },
}

/// Classify a parsed YAML document as sectioned (Phase 3+ canonical shape)
/// vs. flat (legacy) vs. mixed (reject).
///
/// The classifier treats `defaults:` as shape-neutral — it is a
/// document-level version pin permitted at the root in both shapes.
/// A doc with *only* `defaults:` (or nothing at all) is classified as
/// flat by default, which is correct because the flat deserializer is
/// the superset.
fn classify_shape(doc: &serde_yaml::Value) -> ConfigShape {
    let Some(map) = doc.as_mapping() else {
        // Sequences, scalars, and null end up here. The flat deserializer
        // will produce a precise error with the path, so just route that
        // way.
        return ConfigShape::Flat;
    };
    const SECTION_KEYS: &[&str] = &["admission", "access", "storage", "advanced"];
    // Any flat-shape-only key at the root disqualifies "sectioned".
    // Must include every public on-disk key of `Config`; additions need
    // to show up here too or mixed-shape detection silently misses them.
    const FLAT_ONLY_KEYS: &[&str] = &[
        "listen_addr",
        "backend",
        "backends",
        "default_backend",
        "max_delta_ratio",
        "max_object_size",
        "cache_size_mb",
        "metadata_cache_mb",
        "filtered_list_max_engine_pages",
        "authentication",
        "access_key_id",
        "secret_access_key",
        "bootstrap_password_hash",
        "admin_password_hash", // legacy alias for bootstrap_password_hash
        "codec_concurrency",
        "blocking_threads",
        "log_level",
        "config_sync_bucket",
        "config_sync_object_key",
        "backend_encryption",
        "tls",
        "buckets",
        "lifecycle",
        // Phase 3b.2.a: operator-authored admission blocks at the flat
        // root. Both sectioned (`admission:`) and flat (`admission_blocks:`)
        // forms exist for round-trip preservation; mixing them is
        // operator error and the classifier catches it.
        "admission_blocks",
        // Phase 3c.1: IAM source-of-truth selector at the flat root.
        "iam_mode",
    ];

    let mut flat_keys = Vec::new();
    let mut section_keys = Vec::new();
    for key in map.keys().filter_map(|k| k.as_str()) {
        if SECTION_KEYS.contains(&key) {
            section_keys.push(key.to_string());
        } else if FLAT_ONLY_KEYS.contains(&key) {
            flat_keys.push(key.to_string());
        }
        // Unknown keys (e.g. `storge:`) are ignored here — they'll be
        // caught by `deny_unknown_fields` on whichever shape we
        // eventually dispatch to.
    }

    match (flat_keys.is_empty(), section_keys.is_empty()) {
        (true, false) => ConfigShape::Sectioned,
        (false, true) | (true, true) => ConfigShape::Flat,
        (false, false) => ConfigShape::Mixed {
            flat_keys,
            section_keys,
        },
    }
}

/// THE bucket-policy normalize rule of every write path (YAML load, section
/// PUT, document apply via [`Config::normalize_shorthands`], and the PATCH):
/// expand shorthands, and refuse a policy that cannot be normalized (for
/// example `public: true` beside non-empty `public_prefixes`).
pub(crate) fn normalize_bucket_policies(
    buckets: &mut std::collections::BTreeMap<String, crate::bucket_policy::BucketPolicyConfig>,
) -> Result<(), String> {
    for (name, policy) in buckets.iter_mut() {
        policy
            .normalize()
            .map_err(|e| format!("bucket `{name}`: {e}"))?;
    }
    Ok(())
}

/// Root keys of a FLAT-shape document that `Config` does not know. The flat
/// shape keeps serde's lenient default (review 4 config-11: a
/// `deny_unknown_fields` there would stop an existing file from booting), so
/// the loader and `config lint` name them instead. Empty for any other shape.
pub fn unknown_flat_root_keys(doc: &serde_yaml::Value) -> Vec<String> {
    if !matches!(classify_shape(doc), ConfigShape::Flat) {
        return Vec::new();
    }
    let Some(map) = doc.as_mapping() else {
        return Vec::new();
    };
    let schema = schemars::schema_for!(Config);
    let known = schema.schema.object.as_ref().map(|o| &o.properties);
    map.keys()
        .filter_map(|k| k.as_str())
        .filter(|k| *k != "admin_password_hash") // serde alias
        .filter(|k| !known.is_some_and(|p| p.contains_key(*k)))
        .map(str::to_string)
        .collect()
}

impl Config {
    /// Load configuration from a file. YAML is the only supported format;
    /// a `.toml` path fails loudly with [`TOML_REMOVED_MSG`] (operators
    /// must learn the format is gone, not have their file silently
    /// ignored). Any other extension is parsed as YAML.
    pub fn from_file(path: &str) -> Result<Self, ConfigError> {
        if path_is_toml(path) {
            return Err(ConfigError::Parse(format!("{path}: {TOML_REMOVED_MSG}")));
        }
        Self::from_yaml_file(path)
    }

    /// Load configuration from a YAML file explicitly.
    ///
    /// Accepts two on-disk shapes transparently:
    ///   * **Sectioned** (Phase 3+ canonical) — top-level `admission:` /
    ///     `access:` / `storage:` / `advanced:` keys. Parsed via
    ///     [`crate::config_sections::SectionedConfig`] then collapsed into the
    ///     flat in-memory `Config`.
    ///   * **Flat** (legacy) — fields like `listen_addr:`, `backend:`,
    ///     `buckets:` directly at the document root. Still works verbatim.
    ///
    /// Shape detection is explicit (key-presence check, not a silent untagged-
    /// enum fallthrough) so that when a sectioned document has a typo inside
    /// e.g. `storage:`, the error message names the section — not a cryptic
    /// "unknown variant" coming from the flat-shape attempt.
    pub fn from_yaml_file(path: &str) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path).map_err(|e| ConfigError::Io(e.to_string()))?;
        // Expand ${VAR} / ${VAR:-default} against the environment BEFORE parsing
        // (the in-process replacement for an external envsubst step). Loading a
        // file from disk is unambiguously "render against my environment"; the
        // admin `config apply` path parses a doc body via `from_yaml_str` and is
        // deliberately NOT expanded (it would resolve against the server's env).
        let (content, env_refs) = expand_env_vars_recording(&content)?;
        let mut cfg = Self::from_yaml_str(&content)?;
        cfg.env_refs = env_refs;
        Ok(cfg)
    }

    /// Parse a YAML string into a `Config`. See [`Self::from_yaml_file`]
    /// for the dual-shape contract.
    pub fn from_yaml_str(content: &str) -> Result<Self, ConfigError> {
        // Accept empty documents as "use defaults entirely" — matters for
        // wizard-generated files and for round-trips where the canonical
        // exporter elides every section.
        let trimmed = content.trim();
        if trimmed.is_empty() {
            return Ok(Config::default());
        }

        // First pass: peek at the top-level keys to classify shape. This is
        // O(document size) but only runs on config load (startup / explicit
        // apply), never per-request.
        let doc: serde_yaml::Value =
            serde_yaml::from_str(content).map_err(|e| ConfigError::Parse(e.to_string()))?;

        let mut cfg = match classify_shape(&doc) {
            ConfigShape::Sectioned => {
                let sectioned: crate::config_sections::SectionedConfig =
                    lenient::from_value(doc).map_err(|e| ConfigError::Parse(e.to_string()))?;
                sectioned.into_flat().map_err(ConfigError::Parse)?
            }
            ConfigShape::Flat => {
                let unknown = unknown_flat_root_keys(&doc);
                if !unknown.is_empty() {
                    tracing::warn!(
                        "config: unknown root key(s) ignored: {} (a typo keeps the default)",
                        unknown.join(", ")
                    );
                }
                lenient::from_value(doc).map_err(|e| ConfigError::Parse(e.to_string()))?
            }
            ConfigShape::Mixed {
                flat_keys,
                section_keys,
            } => {
                return Err(ConfigError::Parse(format!(
                    "config YAML mixes legacy flat keys ({}) with Phase 3+ section headers ({}); \
                     pick one shape. The canonical export (the admin API `/config/export`) \
                     always emits the sectioned shape.",
                    flat_keys.join(", "),
                    section_keys.join(", "),
                )));
            }
        };
        cfg.normalize_shorthands()?;
        Ok(cfg)
    }

    /// Expand all shorthand forms in the loaded config into their
    /// canonical representations and run semantic validation that
    /// can't live in the serde layer. Called exactly once per load,
    /// after deserialization, before the config is handed to any
    /// consumer.
    ///
    /// Today this covers:
    /// - per-bucket `public: true` → `public_prefixes: [""]`
    ///   (the `BucketPolicyConfig::normalize` call).
    /// - operator-authored admission-block validation (duplicate
    ///   names, bad Reject status, conflicting `source_ip` forms,
    ///   disallowed name charset).
    ///
    /// Returns an error when a shorthand conflicts with its canonical
    /// form (e.g. `public: true` AND non-empty `public_prefixes:`)
    /// or when a semantic check fails. Future shorthands (group
    /// presets) plug in here.
    ///
    /// `pub(crate)` because the section-level admin API
    /// (`src/api/admin/config/section_level.rs::apply_section`)
    /// needs to re-run shorthand normalisation after merge-patching
    /// a section and collapsing it back through
    /// [`crate::config_sections::SectionedConfig::into_flat`]. The
    /// YAML loader runs this automatically; the section PUT has to
    /// call it explicitly.
    pub(crate) fn normalize_shorthands(&mut self) -> Result<(), ConfigError> {
        normalize_bucket_policies(&mut self.buckets).map_err(ConfigError::Parse)?;
        // Validate admission blocks on EVERY load path — the sectioned
        // loader also calls `AdmissionSpec::validate` inside
        // `into_flat`, but that bypasses flat-shape YAML.
        // Running validation here (after both paths converge) closes
        // the gap.
        if !self.admission_blocks.is_empty() {
            let spec = crate::admission::AdmissionSpec {
                blocks: self.admission_blocks.clone(),
            };
            spec.validate()
                .map_err(|e| ConfigError::Parse(format!("admission: {}", e)))?;
        }
        Ok(())
    }

    /// Load configuration from environment variables
    pub fn from_env() -> Self {
        let mut config = Self::default();
        config.apply_env_overrides();
        config
    }

    /// Apply environment variable overrides on top of existing config.
    /// Environment variables always take precedence over file-based config.
    ///
    /// Records what the file had in every overridden slot
    /// ([`Self::env_shadow`]), so persist and export write the file's value
    /// back instead of the env value.
    fn apply_env_overrides(&mut self) {
        self.apply_env_overrides_at_load(&process_env);
    }

    /// [`Self::apply_env_overrides_tracked`] for a config just loaded from
    /// its file: also records which secret env values the file itself holds
    /// (see [`env_shadow::EnvShadow`]), so the leak guard never refuses to
    /// write back what the operator put in the file.
    pub(crate) fn apply_env_overrides_at_load(&mut self, env: EnvLookup) {
        let file_tree = serde_yaml::to_value(&*self).ok();
        self.apply_env_overrides_tracked(env);
        if let Some(tree) = file_tree {
            self.env_shadow.1 = env_overrides::secret_env_values(self, env)
                .into_iter()
                .filter(|(_, v)| tree_has_string(&tree, v))
                .map(|(_, v)| v)
                .collect();
        }
    }

    /// [`Self::apply_env_overrides`] over an injected lookup: applies the
    /// overrides and REPLACES [`Self::env_shadow`] with the file values of
    /// the slots that were overridden. `self` must be the file view.
    pub(crate) fn apply_env_overrides_tracked(&mut self, env: EnvLookup) {
        // Env-only settings: no field to override, no shadow to record.
        self.tuning = RuntimeTuning::from_env(env);
        let before = serde_yaml::to_value(&*self).ok();
        let applied = self.apply_env_overrides_with(env);
        self.env_shadow = match before {
            Some(tree) => env_shadow::capture(&tree, &applied),
            None => {
                // Unreachable in practice (Config always serializes). Without
                // the file view, persist could only write env values — so
                // record every applied slot as absent: the file then omits
                // them rather than leak an env secret.
                tracing::error!("config did not serialize; env-controlled fields will be omitted from the persisted file");
                env_shadow::EnvShadow(
                    applied.into_iter().map(|s| (s, None)).collect(),
                    Default::default(),
                )
            }
        };
    }

    /// Re-apply the env overrides to an edited config that is about to
    /// replace `running` (env wins consistently, not only at boot).
    ///
    /// The edit is first turned into its file view, member by member (see
    /// [`env_shadow::unapply_echoes`]): a value that still equals the running
    /// env value is an echo and takes the file's value back; a value the edit
    /// changed keeps the edit. Then the env overrides are applied on top and
    /// the shadow is recaptured.
    ///
    /// A secret env value that the edit copied to a field no env variable
    /// controls (a mode flip promotes the env encryption key to `legacy_key`)
    /// is recorded as an `${env:NAME}` reference, so the file names the
    /// variable instead of holding the key.
    pub(crate) fn reapply_env_overrides(
        &mut self,
        running: &Config,
        env: EnvLookup,
    ) -> Result<EnvReapply, ConfigError> {
        let parse = |e: serde_yaml::Error| ConfigError::Parse(e.to_string());
        let mut env_refs = std::mem::take(&mut self.env_refs);
        let original = serde_yaml::to_value(&*self).map_err(parse)?;
        let running_tree = serde_yaml::to_value(running).map_err(parse)?;
        let mut tree = original.clone();
        let found = env_shadow::unapply_echoes(&mut tree, &running_tree, &running.env_shadow);
        let mut report = EnvReapply {
            edited: found
                .edited
                .iter()
                .map(|p| env_shadow::display(p))
                .collect(),
            echoed_over_file: found
                .echoed_over_file
                .iter()
                .map(|p| env_shadow::display(p))
                .collect(),
            refs_added: Vec::new(),
        };
        let mut file_view: Config = match serde_yaml::from_value(tree) {
            Ok(cfg) => cfg,
            Err(e) => {
                // The member-wise merge produced a shape that does not parse
                // (an edit inside a block whose type the env sets). Fall back
                // to the file's whole block: the edit is dropped, never the
                // other way round.
                tracing::warn!(
                    "env re-apply: member-wise merge failed ({e}); keeping the file's blocks"
                );
                let mut tree = original;
                env_shadow::restore(&mut tree, &running.env_shadow);
                report.edited.clear();
                serde_yaml::from_value(tree).map_err(parse)?
            }
        };

        // Secret env values copied outside their own slots become refs.
        let file_tree = serde_yaml::to_value(&file_view).map_err(parse)?;
        let already_in_file = running.env_shadow.file_strings();
        let mut secrets = env_overrides::secret_env_values(running, env);
        secrets.extend(env_overrides::secret_env_values(&file_view, env));
        for (name, value) in secrets {
            if !already_in_file.contains(&value)
                && tree_has_string(&file_tree, &value)
                && env_refs.insert(name.clone(), value).is_none()
            {
                report.refs_added.push(name);
            }
        }
        report.refs_added.sort();
        report.refs_added.dedup();

        file_view.env_refs = env_refs;
        file_view.apply_env_overrides_tracked(env);
        // What the ORIGINAL file held stays allowed; the edit cannot add to it.
        file_view.env_shadow.1 = running.env_shadow.1.clone();
        *self = file_view;
        Ok(report)
    }

    /// Names of the SECRET env variables whose value appears as a plain
    /// scalar in `tree` (a serialized file or export), other than values the
    /// config file itself already held. Persist and export refuse to write
    /// such a tree: defence in depth behind [`Self::file_view`].
    pub(crate) fn env_secret_leaks(&self, tree: &serde_yaml::Value, env: EnvLookup) -> Vec<String> {
        let allowed = self.env_shadow.file_strings();
        let mut leaks: Vec<String> = env_overrides::secret_env_values(self, env)
            .into_iter()
            .filter(|(_, v)| v.len() >= 4 && !allowed.contains(v) && tree_has_string(tree, v))
            .map(|(name, _)| name)
            .collect();
        leaks.dedup();
        leaks
    }

    fn refuse_env_secret_leaks(
        &self,
        tree: &serde_yaml::Value,
        env: EnvLookup,
    ) -> Result<(), ConfigError> {
        let leaks = self.env_secret_leaks(tree, env);
        if leaks.is_empty() {
            return Ok(());
        }
        Err(ConfigError::Parse(format!(
            "refusing to write the config: it would contain the value of the secret \
             environment variable(s) {}. Set these values only in the environment.",
            leaks.join(", ")
        )))
    }

    /// The config as the FILE describes it: every env-overridden slot holds
    /// the file's value again (see [`Self::env_shadow`]). The result carries
    /// no shadow, so applying it twice is a no-op. Persist and every export
    /// start from here — an env value, secret or not, never reaches a file.
    pub fn file_view(&self) -> Result<Config, ConfigError> {
        if self.env_shadow.is_empty() {
            return Ok(self.clone());
        }
        let mut tree = serde_yaml::to_value(self).map_err(|e| ConfigError::Parse(e.to_string()))?;
        env_shadow::restore(&mut tree, &self.env_shadow);
        let mut cfg: Config =
            serde_yaml::from_value(tree).map_err(|e| ConfigError::Parse(e.to_string()))?;
        cfg.env_refs = self.env_refs.clone();
        Ok(cfg)
    }

    /// Last-resort fallback when [`Self::file_view`] fails: remove every
    /// env-controlled slot, so a secret from the environment still never
    /// leaves the process.
    fn clear_env_slots(&mut self) {
        let Ok(mut tree) = serde_yaml::to_value(&*self) else {
            return;
        };
        for path in self.env_shadow.0.keys() {
            env_shadow::put(&mut tree, path, None);
        }
        if let Ok(mut cfg) = serde_yaml::from_value::<Config>(tree) {
            cfg.env_refs = std::mem::take(&mut self.env_refs);
            *self = cfg;
        } else {
            self.access_key_id = None;
            self.secret_access_key = None;
        }
    }

    /// The env overrides themselves. Returns the slot of every field it
    /// changed (see [`env_shadow`]). Pure apart from the warnings it prints.
    pub(crate) fn apply_env_overrides_with(&mut self, env: EnvLookup) -> Vec<env_shadow::EnvSlot> {
        use env_shadow::slot;
        let mut applied = Vec::new();

        if let Some(addr) = env("DGP_LISTEN_ADDR") {
            match addr.parse() {
                Ok(parsed) => {
                    self.listen_addr = parsed;
                    applied.push(slot(&["listen_addr"]));
                }
                Err(e) => eprintln!("Warning: ignoring invalid DGP_LISTEN_ADDR=\"{addr}\": {e}"),
            }
        }

        // The legacy singleton backend: an S3 activator replaces the WHOLE
        // block (unset members take their defaults), DGP_DATA_DIR replaces
        // it with a filesystem backend.
        if env("DGP_S3_ENDPOINT").is_some() || env("DGP_S3_REGION").is_some() {
            self.backend = BackendConfig::S3 {
                session_token: None,
                endpoint: env("DGP_S3_ENDPOINT"),
                region: env("DGP_S3_REGION").unwrap_or_else(|| "us-east-1".to_string()),
                force_path_style: lookup_bool(env, "DGP_S3_PATH_STYLE", true),
                access_key_id: env("DGP_BE_AWS_ACCESS_KEY_ID"),
                secret_access_key: env("DGP_BE_AWS_SECRET_ACCESS_KEY"),
                allow_local: lookup_bool(env, "DGP_BACKEND_ALLOW_LOCAL", false),
            };
            applied.push(slot(&["backend"]));
        } else if let Some(dir) = env("DGP_DATA_DIR") {
            self.backend = BackendConfig::Filesystem {
                path: PathBuf::from(dir),
            };
            applied.push(slot(&["backend"]));
        }

        macro_rules! parsed {
            ($var:literal, $ty:ty, $field:ident, $wrap:expr) => {
                if let Some(v) = lookup_parse::<$ty>(env, $var) {
                    self.$field = $wrap(v);
                    applied.push(slot(&[stringify!($field)]));
                }
            };
        }
        macro_rules! text {
            ($var:literal, $field:ident, $wrap:expr) => {
                if let Some(v) = env($var) {
                    self.$field = $wrap(v);
                    applied.push(slot(&[stringify!($field)]));
                }
            };
        }
        parsed!("DGP_MAX_DELTA_RATIO", f32, max_delta_ratio, |v| v);
        parsed!("DGP_MAX_OBJECT_SIZE", u64, max_object_size, |v| v);
        parsed!(
            "DGP_MAX_PASSTHROUGH_OBJECT_SIZE",
            u64,
            max_passthrough_object_size,
            |v| v
        );
        parsed!("DGP_RANGE_SPOOL_TTL_SECS", u64, range_spool_ttl_secs, |v| v);
        parsed!("DGP_CACHE_MB", usize, cache_size_mb, |v| v);
        parsed!("DGP_METADATA_CACHE_MB", usize, metadata_cache_mb, |v| v);
        parsed!(
            "DGP_FILTERED_LIST_MAX_ENGINE_PAGES",
            usize,
            filtered_list_max_engine_pages,
            |v| v
        );
        parsed!("DGP_CODEC_CONCURRENCY", usize, codec_concurrency, Some);
        parsed!("DGP_BLOCKING_THREADS", usize, blocking_threads, Some);

        // Authentication mode + proxy SigV4 credentials.
        text!("DGP_AUTHENTICATION", authentication, Some);
        text!("DGP_ACCESS_KEY_ID", access_key_id, Some);
        text!("DGP_SECRET_ACCESS_KEY", secret_access_key, Some);

        // Admin GUI password hash
        if let Some(v) =
            env("DGP_BOOTSTRAP_PASSWORD_HASH").or_else(|| env("DGP_ADMIN_PASSWORD_HASH"))
        {
            self.bootstrap_password_hash = Some(v);
            applied.push(slot(&["bootstrap_password_hash"]));
        }

        // Log level (runtime operational). RUST_LOG sets the startup filter
        // above DGP_LOG_LEVEL (startup.rs::init_tracing), so it is the
        // effective level: the GUI shows it and an apply cannot replace it.
        if let Some(v) = env(RUST_LOG).or_else(|| env("DGP_LOG_LEVEL")) {
            self.log_level = v;
            applied.push(slot(&["log_level"]));
        }

        // Config DB S3 sync
        text!("DGP_CONFIG_SYNC_BUCKET", config_sync_bucket, Some);
        if let Some(key) = env("DGP_CONFIG_SYNC_KEY") {
            if !key.trim().is_empty() {
                self.config_sync_object_key = Some(key);
                applied.push(slot(&["config_sync_object_key"]));
            }
        }

        // Per-backend encryption overrides.
        //
        // Names are normalised: uppercase, `-`/`.` → `_`. So a backend
        // named "eu-archive" reads from `DGP_BACKEND_EU_ARCHIVE_ENCRYPTION_KEY`.
        //
        // For each backend, we apply overrides based on the CURRENT
        // encryption mode in YAML:
        //   * Aes256GcmProxy mode → `..._ENCRYPTION_KEY` sets the key.
        //   * SseKms mode → `..._SSE_KMS_KEY_ID` sets the kms_key_id.
        //   * Other modes → no override (None/SseS3 don't carry secrets).
        //
        // The env var changes only the secret/id fields, never the
        // mode itself — that stays authoritative in YAML. Keeps env-
        // based secret injection orthogonal to structural config.
        for named in self.backends.iter_mut() {
            if let Some(field) =
                apply_backend_encryption_env(&named.name, &mut named.encryption, env)
            {
                applied.push(vec![
                    env_shadow::SlotSeg::Key("backends".into()),
                    env_shadow::SlotSeg::Named(named.name.clone()),
                    env_shadow::SlotSeg::Key("encryption".into()),
                    env_shadow::SlotSeg::Key(field.into()),
                ]);
            }
        }
        // Singleton backend path: uses the unadorned `DGP_ENCRYPTION_KEY`
        // / `DGP_SSE_KMS_KEY_ID` so single-backend deployments keep a
        // short env-var name. The synthetic backend name is "default".
        if let Some(field) =
            apply_backend_encryption_env("default", &mut self.backend_encryption, env)
        {
            applied.push(slot(&["backend_encryption", field]));
        }

        // TLS: a truthy flag replaces the WHOLE block (unset paths → None,
        // which means a generated self-signed certificate).
        if lookup_bool(env, "DGP_TLS_ENABLED", false) {
            self.tls = Some(TlsConfig {
                enabled: true,
                cert_path: env("DGP_TLS_CERT"),
                key_path: env("DGP_TLS_KEY"),
            });
            applied.push(slot(&["tls"]));
        }
        applied
    }

    /// Resolve the path to the active config file on disk.
    /// Returns `None` if no config file is found.
    ///
    /// Resolution order:
    /// 1. `DGP_CONFIG` env var, if set — returned **unconditionally** (not
    ///    contingent on the file existing at resolve time). Operators who
    ///    explicitly set this var have declared intent; the caller decides
    ///    what to do when the target is absent (typical: fall back to
    ///    defaults at startup, error out on persist). Silently falling
    ///    through would redirect the admin-API persist to a CWD-relative
    ///    file the operator never asked for.
    /// 2. Otherwise, the first existing file in
    ///    [`DEFAULT_CONFIG_SEARCH_PATHS`]. A leftover `.toml` match is
    ///    returned too — the load path rejects it loudly rather than
    ///    silently skipping it.
    pub fn resolve_config_path() -> Option<String> {
        if let Some(path) = process_env("DGP_CONFIG") {
            if !path.is_empty() {
                return Some(path);
            }
        }
        for path in DEFAULT_CONFIG_SEARCH_PATHS {
            if std::path::Path::new(path).exists() {
                return Some(path.to_string());
            }
        }
        None
    }

    /// Load configuration: file first, then env var overrides on top.
    /// Environment variables always take precedence over file-based config.
    ///
    /// A `.toml` config — whether pointed at via `DGP_CONFIG` or found on
    /// the default search path — is a FATAL startup error (exit 1) with an
    /// actionable message ([`TOML_REMOVED_MSG`]). Falling back to defaults
    /// for a removed format would silently ignore the operator's intent.
    pub fn load() -> Self {
        let mut config = Self::load_unchecked();
        config.validate();
        config
    }

    /// [`Self::load`] without printing warnings (fields are still clamped by
    /// `check`). For `main`'s early load that only reads the blocking-thread
    /// count: the full load in `async_main` prints them, and printing both
    /// showed every config warning twice at startup.
    pub fn load_quiet() -> Self {
        let mut config = Self::load_unchecked();
        let _ = config.check();
        config
    }

    fn load_unchecked() -> Self {
        let mut config = if let Some(path) = process_env("DGP_CONFIG") {
            if path_is_toml(&path) {
                eprintln!("ERROR: DGP_CONFIG points at '{path}': {TOML_REMOVED_MSG}");
                std::process::exit(1);
            }
            match Self::from_file(&path) {
                Ok(c) => c,
                Err(e) => {
                    // An operator who set DGP_CONFIG intends THAT config.
                    // Falling back to full defaults on a parse error (a typo,
                    // an unset ${env:VAR}, malformed YAML) would silently boot
                    // with default auth/backends — a security downgrade the
                    // operator never asked for. Fail loud instead. (X-ray H7.)
                    eprintln!("ERROR: DGP_CONFIG='{path}' failed to parse: {e}");
                    eprintln!("Refusing to start with default config the operator did not intend.");
                    std::process::exit(1);
                }
            }
        } else {
            // Try default config file locations (YAML preferred; a
            // leftover `.toml` is rejected loudly, never skipped).
            let mut found = None;
            for path in DEFAULT_CONFIG_SEARCH_PATHS {
                if std::path::Path::new(path).exists() {
                    if path_is_toml(path) {
                        eprintln!("ERROR: found legacy TOML config '{path}': {TOML_REMOVED_MSG}");
                        std::process::exit(1);
                    }
                    // A config file that EXISTS on the search path but fails to
                    // parse must be fatal, not silently skipped → defaults
                    // (X-ray H7). The operator placed it there intentionally.
                    match Self::from_file(path) {
                        Ok(config) => {
                            found = Some(config);
                            break;
                        }
                        Err(e) => {
                            eprintln!("ERROR: config file '{path}' failed to parse: {e}");
                            eprintln!(
                                "Refusing to start with default config the operator did not intend."
                            );
                            std::process::exit(1);
                        }
                    }
                }
            }
            found.unwrap_or_default()
        };

        // Environment variables always override file config
        config.apply_env_overrides();
        config
    }

    /// Load config from an explicit path, with env var overrides applied on
    /// top and `validate()` run at the end.
    ///
    /// This is the `--config <path>` entry point. Without this helper, callers
    /// that specify a config path (via the CLI flag) had to remember to call
    /// `Self::apply_env_overrides` themselves — and the main startup path
    /// didn't, meaning `DGP_*` vars were ignored when `--config` was used
    /// but respected when it wasn't. The asymmetry surprised operators; this
    /// helper folds the behaviour of [`Self::load`] onto a caller-provided
    /// path so the two paths agree.
    pub fn load_from_path(path: &str) -> Result<Self, ConfigError> {
        let mut config = Self::from_file(path)?;
        config.apply_env_overrides();
        config.validate();
        Ok(config)
    }

    /// [`Self::load_from_path`] without printing warnings — see
    /// [`Self::load_quiet`].
    pub fn load_from_path_quiet(path: &str) -> Result<Self, ConfigError> {
        let mut config = Self::from_file(path)?;
        config.apply_env_overrides();
        let _ = config.check();
        Ok(config)
    }

    /// Name of the backend that receives buckets without an explicit route:
    /// `default_backend`, else the first named backend, else the synthesized
    /// `"default"` singleton. The ONE copy of the rule the engine router uses.
    pub fn default_backend_name(&self) -> String {
        match self.backends.first() {
            None => "default".to_string(),
            Some(first) => self
                .default_backend
                .clone()
                .unwrap_or_else(|| first.name.clone()),
        }
    }

    /// Definition of a backend by NAME. `"default"` means the singleton only
    /// while no named backends exist (the rule `check_fatal` enforces).
    pub fn backend_by_name(&self, name: &str) -> Option<&BackendConfig> {
        if self.backends.is_empty() {
            return (name == "default").then_some(&self.backend);
        }
        self.backends
            .iter()
            .find(|b| b.name == name)
            .map(|b| &b.backend)
    }

    /// Encryption config of a backend by NAME (same `"default"` rule as
    /// [`Self::backend_by_name`]).
    pub fn backend_encryption_by_name(&self, name: &str) -> Option<&BackendEncryptionConfig> {
        if self.backends.is_empty() {
            return (name == "default").then_some(&self.backend_encryption);
        }
        self.backends
            .iter()
            .find(|b| b.name == name)
            .map(|b| &b.encryption)
    }

    /// `(name, definition)` of the backend a bucket routes to by config: its
    /// explicit route, else [`Self::default_backend_name`]. `None` = the route
    /// names an undefined backend. Every "which backend is this bucket on"
    /// question (gates, sync client, migrate) goes through here; the source
    /// test `effective_backend_rule_has_one_home` refuses hand-rolled copies.
    pub fn effective_backend_for_bucket(&self, bucket: &str) -> Option<(String, &BackendConfig)> {
        let name = self
            .buckets
            .get(&bucket.to_ascii_lowercase())
            .and_then(|p| p.backend.clone())
            .unwrap_or_else(|| self.default_backend_name());
        self.backend_by_name(&name).map(|def| (name, def))
    }

    /// Backend that hosts `config_sync_bucket` (IAM sync, leases, reference
    /// locks). An S3 singleton keeps the role even beside named backends:
    /// that is where older releases always put the coordination bucket.
    /// Otherwise the sync bucket resolves like any bucket. The old
    /// singleton-only rule built a client for the unused filesystem default
    /// when only named backends existed, so HA was off with no error.
    pub fn coordination_backend(&self) -> Option<&BackendConfig> {
        if self.backends.is_empty() || matches!(self.backend, BackendConfig::S3 { .. }) {
            return Some(&self.backend);
        }
        let bucket = self.config_sync_bucket.as_deref()?;
        self.effective_backend_for_bucket(bucket)
            .map(|(_, def)| def)
    }

    /// Returns true if SigV4 authentication is enabled (both credentials are set).
    pub fn auth_enabled(&self) -> bool {
        self.access_key_id.is_some() && self.secret_access_key.is_some()
    }

    /// Pure classification of the auth configuration at startup. Decides
    /// *which* outcome the process should take; the caller (`startup.rs`)
    /// owns the logging and the `process::exit` so this stays unit-testable
    /// without spawning a process or capturing stderr.
    ///
    /// `iam_db_has_users`: the config DB holds IAM users (or cannot be read
    /// with this key, so it may). Those users, like declarative `iam_users`,
    /// are configured credentials: the proxy runs in IAM mode without a
    /// bootstrap pair.
    ///
    /// See [`AuthConfigOutcome`] for the meaning of each variant.
    pub fn classify_auth_config(&self, iam_db_has_users: bool) -> AuthConfigOutcome {
        // Normalize the authentication field: lowercase + trim whitespace.
        let auth_mode = self
            .authentication
            .as_deref()
            .map(|s| s.trim().to_ascii_lowercase());
        let auth_mode = auth_mode.as_deref();

        if self.auth_enabled() {
            // Credentials are set — auth is on regardless of the field. A
            // stray `authentication = "none"` is ignored (but worth a note).
            return AuthConfigOutcome::CredentialsEnabled {
                redundant_none: auth_mode == Some("none"),
            };
        }
        let declarative_users =
            matches!(self.iam_mode, crate::config_sections::IamMode::Declarative)
                && !self.iam_users.is_empty();
        if iam_db_has_users || declarative_users {
            return AuthConfigOutcome::IamUsers {
                redundant_none: auth_mode == Some("none"),
            };
        }

        match auth_mode {
            Some("none") => AuthConfigOutcome::OpenAccess,
            Some(_) => AuthConfigOutcome::UnrecognizedMode,
            None => AuthConfigOutcome::Missing,
        }
    }

    /// The startup banner's storage lines: EVERY backend with its type and
    /// location, the default marked. (The banner used to print only the
    /// legacy singleton, so a multi-backend proxy logged "Backend:
    /// Filesystem" and nothing about its S3 backends.)
    pub fn backend_banner_lines(&self) -> Vec<String> {
        fn describe(b: &BackendConfig) -> String {
            match b {
                BackendConfig::Filesystem { path } => {
                    format!("filesystem, path {}", path.display())
                }
                BackendConfig::S3 {
                    endpoint, region, ..
                } => match endpoint {
                    Some(ep) => format!("s3, endpoint {ep}, region {region}"),
                    None => format!("s3 (AWS), region {region}"),
                },
            }
        }
        if self.backends.is_empty() {
            return vec![format!("  Backend: {}", describe(&self.backend))];
        }
        let default = self
            .default_backend
            .clone()
            .unwrap_or_else(|| self.backends[0].name.clone());
        let mut out = vec![format!("  Backends ({}):", self.backends.len())];
        for nb in &self.backends {
            let mark = if nb.name == default { " (default)" } else { "" };
            out.push(format!(
                "    - {}{}: {}",
                nb.name,
                mark,
                describe(&nb.backend)
            ));
        }
        out
    }

    /// Returns true if TLS is enabled.
    pub fn tls_enabled(&self) -> bool {
        self.tls.as_ref().is_some_and(|t| t.enabled)
    }

    /// Decode a hash value: if it looks like base64 (no `$` prefix), decode it.
    /// Otherwise return as-is (raw bcrypt hash). Validates the result is a bcrypt hash.
    pub fn decode_hash(value: &str) -> String {
        let trimmed = value.trim();
        let hash = if trimmed.starts_with('$') {
            // Raw bcrypt hash like $2b$12$...
            trimmed.to_string()
        } else if !trimmed.is_empty() {
            // Try base64 decode
            use base64::Engine;
            match base64::engine::general_purpose::STANDARD.decode(trimmed) {
                Ok(bytes) => match String::from_utf8(bytes) {
                    Ok(decoded) if decoded.starts_with("$2") => decoded,
                    _ => {
                        eprintln!(
                            "WARNING: DGP_BOOTSTRAP_PASSWORD_HASH is not a valid bcrypt hash \
                             (base64 decoded but not bcrypt format). Login will fail."
                        );
                        trimmed.to_string()
                    }
                },
                Err(_) => {
                    eprintln!(
                        "WARNING: DGP_BOOTSTRAP_PASSWORD_HASH is not a valid bcrypt hash \
                         or base64-encoded hash. Login will fail."
                    );
                    trimmed.to_string()
                }
            }
        } else {
            String::new()
        };
        // Final validation: bcrypt hashes start with $2
        if !hash.is_empty() && !hash.starts_with("$2") {
            eprintln!(
                "WARNING: Bootstrap password hash does not look like bcrypt (expected $2b$... or $2a$...). \
                 Admin login will fail."
            );
        }
        hash
    }

    /// Ensure bootstrap_password_hash is set. Resolution order:
    /// 1. Already set in config (env var or config file) — use it.
    /// 2. Persisted state file `.deltaglider_bootstrap_hash` (or legacy `.deltaglider_admin_hash`).
    /// 3. Generate a random password, hash it, persist, and print to stderr.
    ///
    /// Accepts both raw bcrypt hash (`$2b$12$...`) and base64-encoded bcrypt hash.
    /// Base64 encoding avoids `$` escaping issues in Docker/shell/env vars.
    ///
    /// Returns the bcrypt hash (always raw, never base64).
    pub fn ensure_bootstrap_password_hash(&mut self) -> String {
        if let Some(ref hash) = self.bootstrap_password_hash {
            return Self::decode_hash(hash);
        }

        // Check new file first, fall back to legacy file name
        let new_file = std::path::Path::new(".deltaglider_bootstrap_hash");
        let legacy_file = std::path::Path::new(".deltaglider_admin_hash");
        let state_file = if new_file.exists() {
            new_file
        } else {
            legacy_file
        };
        if state_file.exists() {
            if let Ok(raw) = std::fs::read_to_string(state_file) {
                let hash = Self::decode_hash(raw.trim());
                if !hash.is_empty() {
                    self.bootstrap_password_hash = Some(hash.clone());
                    return hash;
                }
            }
        }

        // Generate a random 16-character password
        use rand::Rng;
        let mut rng = rand::thread_rng();
        let password: String = (0..16)
            .map(|_| {
                let idx = rng.gen_range(0..62);
                match idx {
                    0..=9 => (b'0' + idx) as char,
                    10..=35 => (b'a' + idx - 10) as char,
                    _ => (b'A' + idx - 36) as char,
                }
            })
            .collect();

        let hash = bcrypt::hash(&password, bcrypt::DEFAULT_COST).expect("bcrypt hashing failed");

        // Persist the hash (use new file name)
        let persist_file = std::path::Path::new(".deltaglider_bootstrap_hash");
        if let Err(e) = write_bootstrap_hash_file(persist_file, &hash) {
            eprintln!(
                "Warning: could not persist bootstrap hash to {}: {}",
                persist_file.display(),
                e
            );
        }

        // Print prominently to stderr — but only expose the plaintext password
        // when stderr is a TTY (interactive terminal). In containers/CI the
        // plaintext would leak into captured logs, so we print only the bcrypt
        // hash and tell the operator to set the env var.
        use std::io::IsTerminal;
        for line in first_run_banner(&password, &hash, std::io::stderr().is_terminal()) {
            eprintln!("{line}");
        }

        self.bootstrap_password_hash = Some(hash.clone());
        hash
    }

    /// Wrap this config in an `Arc<RwLock>` for shared mutable access.
    pub fn into_shared(self) -> SharedConfig {
        Arc::new(tokio::sync::RwLock::new(self))
    }

    /// Print all recognised environment variables in `.env` format, grouped by category.
    pub fn print_env_vars() {
        let mut current_category = "";
        for entry in ENV_VAR_REGISTRY {
            if entry.category != current_category {
                if !current_category.is_empty() {
                    println!();
                }
                println!("# {}", entry.category);
                current_category = entry.category;
            }
            println!("# {}", entry.description);
            println!("{}={}", entry.name, entry.example);
        }
    }

    /// Re-insert `${env:NAME}` references for every scalar whose value the
    /// loaded document originally resolved from the environment (see
    /// [`Self::env_refs`]). This is what makes the IaC round-trip lossless:
    /// the persisted file and the exported document carry the refs, not the
    /// materialized secrets.
    ///
    /// Mechanics: the config is serialized to a YAML value tree and every
    /// String scalar that EXACTLY equals a recorded env value is replaced by
    /// `${env:NAME}` (a short value found in several fields stays; see
    /// below). Deterministic on collisions (two names with the same
    /// value → the lexicographically-first name wins). Non-string scalars
    /// (a ref that expanded into a number/bool field) are left materialized,
    /// and so is a typed field that serializes as a string but cannot hold
    /// a ref (`listen_addr: SocketAddr`, an enum): that one target is
    /// skipped, never the whole reinsertion (which wrote every ref-sourced
    /// secret in plaintext).
    pub fn with_env_refs_reinserted(&self) -> Self {
        /// Shortest env value treated as a secret when it occurs in several
        /// fields (see the count rule below). 16: a region (`us-east-1`) or a
        /// host name shared by two fields is not a secret.
        const SHARED_REF_MIN_LEN: usize = 16;
        if self.env_refs.is_empty() {
            return self.clone();
        }
        // Inverse map value → ref-string; BTreeMap iteration makes the
        // first (lexicographically smallest) name win on duplicate values.
        let mut inverse: std::collections::BTreeMap<&str, String> =
            std::collections::BTreeMap::new();
        for (name, value) in &self.env_refs {
            if !value.is_empty() {
                inverse
                    .entry(value.as_str())
                    .or_insert_with(|| format!("${{env:{name}}}"));
            }
        }

        // Count how many string scalars in the whole tree hold each env value.
        // A SHORT value that appears in more than one field is ambiguous —
        // rewriting `9000` or `us-east-1` everywhere would COUPLE an unrelated
        // field to that env var — so it stays materialized. A secret-length
        // value (>= SHARED_REF_MIN_LEN) is rewritten in every field (S9): the
        // file used the ref twice, or a GUI edit copied the secret; either
        // way plaintext on disk is the worse outcome.
        fn count_values<'a>(
            v: &'a serde_yaml::Value,
            counts: &mut std::collections::HashMap<&'a str, u32>,
        ) {
            match v {
                serde_yaml::Value::String(s) => *counts.entry(s.as_str()).or_insert(0) += 1,
                serde_yaml::Value::Sequence(seq) => {
                    seq.iter().for_each(|i| count_values(i, counts))
                }
                serde_yaml::Value::Mapping(map) => {
                    map.iter().for_each(|(_, val)| count_values(val, counts))
                }
                _ => {}
            }
        }

        /// One step of a path into the value tree.
        enum Seg {
            Key(serde_yaml::Value),
            Idx(usize),
        }

        /// The path of every String scalar to rewrite, with its ref.
        fn collect(
            v: &serde_yaml::Value,
            path: &mut Vec<Seg>,
            inverse: &std::collections::BTreeMap<&str, String>,
            counts: &std::collections::HashMap<&str, u32>,
            out: &mut Vec<(Vec<Seg>, String)>,
        ) {
            match v {
                serde_yaml::Value::String(s) => {
                    if counts.get(s.as_str()).copied().unwrap_or(0) == 1
                        || s.len() >= SHARED_REF_MIN_LEN
                    {
                        if let Some(reference) = inverse.get(s.as_str()) {
                            let copy = path
                                .iter()
                                .map(|seg| match seg {
                                    Seg::Key(k) => Seg::Key(k.clone()),
                                    Seg::Idx(i) => Seg::Idx(*i),
                                })
                                .collect();
                            out.push((copy, reference.clone()));
                        }
                    }
                }
                serde_yaml::Value::Sequence(seq) => {
                    for (i, item) in seq.iter().enumerate() {
                        path.push(Seg::Idx(i));
                        collect(item, path, inverse, counts, out);
                        path.pop();
                    }
                }
                serde_yaml::Value::Mapping(map) => {
                    for (key, value) in map {
                        path.push(Seg::Key(key.clone()));
                        collect(value, path, inverse, counts, out);
                        path.pop();
                    }
                }
                _ => {}
            }
        }

        fn at<'a>(v: &'a mut serde_yaml::Value, path: &[Seg]) -> Option<&'a mut serde_yaml::Value> {
            path.iter().try_fold(v, |v, seg| match (seg, v) {
                (Seg::Key(k), serde_yaml::Value::Mapping(m)) => m.get_mut(k),
                (Seg::Idx(i), serde_yaml::Value::Sequence(s)) => s.get_mut(*i),
                _ => None,
            })
        }

        let mut tree = match serde_yaml::to_value(self) {
            Ok(tree) => tree,
            Err(e) => {
                // Serializing Config does not fail in practice (every
                // persist and export does it); nothing to rewrite without it.
                tracing::warn!("env-ref reinsertion: cannot serialize config ({e})");
                return self.clone();
            }
        };
        let mut targets = Vec::new();
        {
            let mut counts: std::collections::HashMap<&str, u32> = std::collections::HashMap::new();
            count_values(&tree, &mut counts);
            collect(&tree, &mut Vec::new(), &inverse, &counts, &mut targets);
        }
        let original = tree.clone();
        for (path, reference) in &targets {
            if let Some(slot) = at(&mut tree, path) {
                *slot = serde_yaml::Value::String(reference.clone());
            }
        }
        let mut cfg = match serde_yaml::from_value::<Config>(tree) {
            Ok(cfg) => cfg,
            Err(_) => {
                // Some target is a typed field that cannot hold a ref. Apply
                // the targets one at a time and keep each that still parses.
                let mut tree = original;
                let mut last_good: Option<Config> = None;
                for (path, reference) in &targets {
                    let Some(slot) = at(&mut tree, path) else {
                        continue;
                    };
                    let before =
                        std::mem::replace(slot, serde_yaml::Value::String(reference.clone()));
                    match serde_yaml::from_value::<Config>(tree.clone()) {
                        Ok(cfg) => last_good = Some(cfg),
                        Err(_) => {
                            if let Some(slot) = at(&mut tree, path) {
                                *slot = before;
                            }
                        }
                    }
                }
                match last_good {
                    Some(cfg) => cfg,
                    None => match serde_yaml::from_value::<Config>(tree) {
                        Ok(cfg) => cfg,
                        Err(e) => {
                            tracing::warn!("env-ref reinsertion: config does not round-trip ({e})");
                            return self.clone();
                        }
                    },
                }
            }
        };
        // `env_refs` / `env_shadow` are #[serde(skip)] — restore them so
        // chained serializers (and future persists) keep the provenance.
        cfg.env_refs = self.env_refs.clone();
        cfg.env_shadow = self.env_shadow.clone();
        cfg
    }

    /// Resolve any full-scalar `${env:NAME}` string field back to its value.
    /// The inverse of [`Self::with_env_refs_reinserted`], for the section-PUT
    /// ingest path: section GETs now EMIT refs for ref-sourced secrets, so a
    /// GUI round-trip echoes the literal `"${env:NAME}"` string back — without
    /// this resolution that literal would silently REPLACE the real secret.
    /// It also makes refs first-class in the GUI: an operator can type
    /// `${env:NAME}` into a field and it resolves like the file loader would.
    ///
    /// Lookup order per ref: recorded provenance (`env_refs`) → the ref's
    /// own `:-default` → hard error (fail loud, same contract as file load).
    /// The process environment is consulted only for names the operator put
    /// in `DGP_CONFIG_ENV_ALLOWLIST`: a section body is admin input, and
    /// reading any env var for it leaks secrets (S7). Newly resolved names are recorded into
    /// `env_refs` so future persists re-emit the ref. Only strings that are
    /// EXACTLY one ref resolve — mid-string refs in GUI fields stay literal.
    pub fn resolve_env_ref_scalars(&mut self) -> Result<(), ConfigError> {
        fn walk(
            v: &mut serde_yaml::Value,
            refs: &mut std::collections::BTreeMap<String, String>,
        ) -> Result<(), ConfigError> {
            match v {
                serde_yaml::Value::String(s) if is_env_ref(s) => {
                    // Record provenance ONLY when the lookup supplied the
                    // value — a ref satisfied by its `:-default` is not
                    // recorded, mirroring `expand_env_vars_recording`.
                    let mut hits: Vec<(String, String)> = Vec::new();
                    let resolved = expand_env_with(s, |name| {
                        // Provenance, else an allowlisted name only (S7):
                        // see `expand_env_admin`.
                        let v = expansion::admin_env_lookup(name, refs);
                        if let Some(val) = &v {
                            if !val.is_empty() {
                                hits.push((name.to_string(), val.clone()));
                            }
                        }
                        v
                    })?;
                    refs.extend(hits);
                    *s = resolved;
                    Ok(())
                }
                serde_yaml::Value::Sequence(seq) => {
                    for item in seq {
                        walk(item, refs)?;
                    }
                    Ok(())
                }
                serde_yaml::Value::Mapping(map) => {
                    for (_, value) in map.iter_mut() {
                        walk(value, refs)?;
                    }
                    Ok(())
                }
                _ => Ok(()),
            }
        }

        let mut refs = self.env_refs.clone();
        let mut tree =
            serde_yaml::to_value(&*self).map_err(|e| ConfigError::Parse(e.to_string()))?;
        walk(&mut tree, &mut refs)?;
        let mut resolved: Config =
            serde_yaml::from_value(tree).map_err(|e| ConfigError::Parse(e.to_string()))?;
        resolved.env_refs = refs;
        resolved.env_shadow = std::mem::take(&mut self.env_shadow);
        *self = resolved;
        Ok(())
    }

    /// Clone the config with *infrastructure* secrets redacted. Matches the
    /// Persistence variant: strips `bootstrap_password_hash` (it has its
    /// own dedicated rotation endpoint + sits in the encrypted IAM DB,
    /// never on the YAML on disk). Encryption keys STAY in the persisted
    /// YAML — if the operator put them there explicitly, persisting the
    /// in-memory config back to disk must preserve that choice (else a
    /// round-trip through `PATCH → persist → restart` silently strips
    /// the YAML key and the next boot falls back to env lookup; if no
    /// env var is set, historical-encrypted reads start erroring and
    /// new writes land plaintext while stamped as proxy-AES).
    ///
    /// Proxy SigV4 credentials and backend credentials are kept — the
    /// wizard, file-based deployment, and users reading the file on
    /// disk all depend on them being present.
    ///
    /// Use [`Self::redact_for_export`] for the admin-API `GET /export`
    /// flow that never trusts a downloadable file as a secret store.
    fn redact_for_persist(&self) -> Self {
        let mut export = self.clone();
        export.bootstrap_password_hash = None;
        export
    }

    /// True if any backend (singleton or named) uses SSE-KMS. Used to
    /// flag at export time that the (non-secret but account-revealing)
    /// `kms_key_id` ARN survives redaction.
    fn has_kms_encryption(&self) -> bool {
        matches!(self.backend_encryption.mode_tag(), "sse-kms")
            || self
                .backends
                .iter()
                .any(|b| b.encryption.mode_tag() == "sse-kms")
    }

    /// Export variant: strips infra secrets AND every per-backend
    /// encryption key (singleton + named list). Intended for the admin
    /// API download endpoint where operators read the YAML out of band.
    /// A persisted file pulled from the box might make it into GitOps
    /// or a bug report; we don't want encryption keys to follow it.
    fn redact_for_export(&self) -> Self {
        let mut export = self.redact_for_persist();
        // SSE-KMS keeps `kms_key_id` visible after redaction (it's an ARN,
        // not key material — operators need it to know WHICH key). But an
        // ARN discloses the AWS account id, region, and key alias, and this
        // export is the artifact that might land in GitOps or a bug report.
        // Flag it at export time so operators can scrub the ARN themselves
        // if those details are sensitive in their environment.
        if export.has_kms_encryption() {
            tracing::warn!(
                "exported config retains SSE-KMS key ARN(s) (kms_key_id) — \
                 these are not secret but disclose AWS account id / region / \
                 key alias; scrub before sharing if that is sensitive"
            );
        }
        export.backend_encryption.redact_secrets();
        for named in &mut export.backends {
            named.encryption.redact_secrets();
        }
        // Phase 3c.3: strip declarative-IAM secrets — per-user
        // secret_access_key and per-provider client_secret. These are
        // infra secrets that belong in env vars or a secret manager,
        // not in a downloadable YAML artifact. Persist-variant
        // preserves them (so a round-trip through PATCH → persist
        // doesn't lose the operator's in-memory values). `${env:...}`
        // references survive — they ARE the env-var/secret-manager form.
        for u in &mut export.iam_users {
            if !is_env_ref(&u.secret_access_key) {
                u.secret_access_key.clear();
            }
        }
        for p in &mut export.auth_providers {
            if !p.client_secret.as_deref().is_some_and(is_env_ref) {
                p.client_secret = None;
            }
        }
        export
    }

    /// Clone the config with *every* secret redacted: infra secrets plus all
    /// SigV4 credentials (top-level and per-backend). This is the right level
    /// of paranoia for the admin API `GET /export` endpoint (Phase 1): the
    /// operator reading the exported YAML must refill secrets from their
    /// secret manager, not copy them out of an API response.
    pub fn redact_all_secrets(&self) -> Self {
        // `${env:NAME}` references survive every redaction tier — a
        // reference is not a secret (see the env-ref round-trip docs on
        // [`Self::env_refs`]).
        fn clear_unless_ref(slot: &mut Option<String>) {
            if !slot.as_deref().is_some_and(is_env_ref) {
                *slot = None;
            }
        }
        // Reinsert env refs FIRST: callers chain `redact_all_secrets()` into
        // `to_canonical_yaml()`, and clearing a materialized secret before
        // reinsertion could match it would lose the reference.
        // Start from the FILE view: an env-overridden field shows what the
        // file says, never the env value (see [`Self::file_view`]).
        let file_view = self.file_view().unwrap_or_else(|e| {
            tracing::error!("file view failed ({e}); exporting with env-controlled fields cleared");
            let mut c = self.clone();
            c.clear_env_slots();
            c
        });
        let mut export = file_view.with_env_refs_reinserted().redact_for_export();
        // An access key id (bootstrap pair and every S3 backend) is an
        // identifier, not a secret: the operator must see which key is
        // configured. The secret stays hidden; an unchanged id keeps it on
        // apply (`preserve_sigv4_pair`).
        if let BackendConfig::S3 {
            ref mut secret_access_key,
            ..
        } = export.backend
        {
            clear_unless_ref(secret_access_key);
        }
        for named in &mut export.backends {
            if let BackendConfig::S3 {
                ref mut secret_access_key,
                ..
            } = named.backend
            {
                clear_unless_ref(secret_access_key);
            }
        }
        clear_unless_ref(&mut export.secret_access_key);
        // Webhook header values may carry bearer tokens. Mask the VALUE but keep
        // the KEY so the GUI shows which headers exist; the section-PUT preserve
        // path restores any value left as this sentinel. Endpoint URLs are left
        // visible on purpose (operators must verify them; credentials belong in
        // headers, not the URL).
        for value in export.event_delivery.webhook_headers.values_mut() {
            if !is_env_ref(value) {
                *value = REDACTED_SENTINEL.to_string();
            }
        }
        // Slack bot token is a secret too — mask it (keep Some so the GUI shows a
        // token IS configured), preserved on an untouched round-trip.
        if export
            .event_delivery
            .slack_bot_token
            .as_deref()
            .is_some_and(|t| !is_env_ref(t))
        {
            export.event_delivery.slack_bot_token = Some(REDACTED_SENTINEL.to_string());
        }
        // Slack INCOMING-WEBHOOK URLs are bearer-equivalent: the `hooks.slack.com`
        // path token IS the credential. In incoming-webhook mode (format=slack,
        // no bot token) mask the webhook URL(s) like any other secret, preserved
        // on an untouched round-trip. Raw-webhook URLs stay visible (their
        // credentials live in headers, which are already masked above).
        if export.event_delivery.format == crate::config_sections::EventDeliveryFormat::Slack
            && !export.event_delivery.uses_slack_bot_token()
        {
            if export
                .event_delivery
                .webhook_url
                .as_deref()
                .is_some_and(|u| !is_env_ref(u))
            {
                export.event_delivery.webhook_url = Some(REDACTED_SENTINEL.to_string());
            }
            for url in export.event_delivery.webhook_urls.iter_mut() {
                if !is_env_ref(url) {
                    *url = REDACTED_SENTINEL.to_string();
                }
            }
        }
        export
    }

    /// Serialize config to canonical YAML string.
    ///
    /// Emits the Phase 3 **sectioned** shape: top-level `admission:` /
    /// `access:` / `storage:` / `advanced:` groups, with each group omitted
    /// when it equals its default (minimal-diff GitOps-friendly output).
    /// Strips infra secrets (bootstrap hash and per-backend encryption
    /// keys; SigV4 credentials are kept — see [`Self::redact_all_secrets`]
    /// for the fully-redacted variant) so that `config show` and the admin
    /// `/export` endpoint never leak the bootstrap hash or the AES master
    /// key into disk artifacts. The persist path deliberately does NOT go
    /// through this function — see [`Self::persist_to_file`].
    ///
    /// The dual-shape deserializer accepts the legacy flat YAML too, but
    /// we only ever *emit* sectioned — legacy readers eventually disappear,
    /// the canonical artifact must be forward-shaped.
    pub fn to_canonical_yaml(&self) -> Result<String, ConfigError> {
        self.to_canonical_yaml_with(&process_env)
    }

    pub(crate) fn to_canonical_yaml_with(&self, env: EnvLookup) -> Result<String, ConfigError> {
        let export = self
            .file_view()?
            .with_env_refs_reinserted()
            .redact_for_export();
        let sectioned = crate::config_sections::SectionedConfig::from_flat(&export);
        let tree =
            serde_yaml::to_value(&sectioned).map_err(|e| ConfigError::Parse(e.to_string()))?;
        self.refuse_env_secret_leaks(&tree, env)?;
        // Serialize the typed value, not the tree: the tree widens f32 to
        // f64 (`0.42` would print as `0.41999998688697815`).
        serde_yaml::to_string(&sectioned).map_err(|e| ConfigError::Parse(e.to_string()))
    }

    /// Persist-variant serializers: preserve per-backend encryption keys
    /// that the operator configured in YAML. Strips only the bootstrap
    /// password hash (which sits in the encrypted IAM DB, not on disk).
    ///
    /// Rationale: if the admin API (or any code path that mutates the
    /// in-memory `Config`) round-trips through the export serializer,
    /// YAML-configured encryption keys silently vanish from disk. On
    /// next restart the engine falls back to env lookup; if no env var
    /// is set, historical-encrypted reads error and new writes land
    /// plaintext. The operator's in-memory state is correct but their
    /// on-disk source of truth disagrees.
    fn to_canonical_yaml_for_persist(&self) -> Result<String, ConfigError> {
        self.to_canonical_yaml_for_persist_with(&process_env)
    }

    pub(crate) fn to_canonical_yaml_for_persist_with(
        &self,
        env: EnvLookup,
    ) -> Result<String, ConfigError> {
        // The FILE view: env-overridden fields get the file's value back, so
        // an env value (a secret above all) never lands in the file.
        let export = self
            .file_view()?
            .with_env_refs_reinserted()
            .redact_for_persist();
        let sectioned = crate::config_sections::SectionedConfig::from_flat(&export);
        // The load path expands the WHOLE file text (`$$`→`$`, `${env:NAME}`→
        // value). A runtime-entered literal containing `$$` or a `${env:...}`-
        // shaped substring would therefore round-trip WRONG through persist→load
        // (`pay $$10` reloads as `pay $10`; a bearer token with `${env:X}` gets
        // expanded/erased). Escape materialized scalars ($→$$) so load's inverse
        // recovers the original. A re-inserted `${env:NAME}` ref (produced above)
        // is LEFT intact — it's meant to expand. Symmetric: load's `$$`→`$`
        // exactly undoes this, so repeated persist/load is stable.
        let mut tree =
            serde_yaml::to_value(&sectioned).map_err(|e| ConfigError::Parse(e.to_string()))?;
        self.refuse_env_secret_leaks(&tree, env)?;
        escape_dollar_for_persist(&mut tree);
        serde_yaml::to_string(&tree).map_err(|e| ConfigError::Parse(e.to_string()))
    }

    /// Persist the current config to a file atomically. Always writes
    /// canonical sectioned YAML; a `.toml` target path is refused with
    /// [`TOML_REMOVED_MSG`] (TOML support was removed in v1.4.1).
    ///
    /// Atomicity is achieved by writing to a sibling tempfile on the same
    /// filesystem, `fsync()`-ing it to force the bytes to disk, then
    /// `rename()`-ing over the target path. On POSIX systems `rename(2)` is
    /// atomic within a single filesystem, so a crash or power loss at any
    /// point leaves the target either fully old or fully new — never the
    /// truncated-mid-write corruption that a bare `fs::write` can produce.
    ///
    /// Uses the `_for_persist` serializers which preserve per-backend
    /// encryption keys. If the operator put keys in YAML, they stay
    /// there across admin-API mutations (which is an invariant the
    /// docs advertise — see `docs/product/reference/encryption-at-rest.md`).
    pub fn persist_to_file(&self, path: &str) -> Result<(), ConfigError> {
        if path_is_toml(path) {
            return Err(ConfigError::Parse(format!(
                "cannot persist config to `{path}`: {TOML_REMOVED_MSG}"
            )));
        }
        let content = self.to_canonical_yaml_for_persist()?;
        atomic_write(std::path::Path::new(path), content.as_bytes())
    }
}

/// What `Config::reapply_env_overrides` found. Paths are dotted flat
/// config paths.
#[derive(Debug, Default, PartialEq)]
pub struct EnvReapply {
    /// Env-controlled fields the edit changed: saved to the file only.
    pub edited: Vec<String>,
    /// Fields the edit set to exactly the env value while the file holds
    /// another value: treated as an echo, so the file keeps its value.
    pub echoed_over_file: Vec<String>,
    /// Secret variables now written to the file as `${env:NAME}` references.
    pub refs_added: Vec<String>,
}

/// Does any string scalar in `tree` equal `needle`?
pub(crate) fn tree_has_string(tree: &serde_yaml::Value, needle: &str) -> bool {
    match tree {
        serde_yaml::Value::String(s) => s == needle,
        serde_yaml::Value::Sequence(seq) => seq.iter().any(|v| tree_has_string(v, needle)),
        serde_yaml::Value::Mapping(m) => m.values().any(|v| tree_has_string(v, needle)),
        serde_yaml::Value::Tagged(t) => tree_has_string(&t.value, needle),
        _ => false,
    }
}

/// Is this scalar EXACTLY a re-inserted `${env:NAME}` / `${env:NAME:-default}`
/// reference (produced by `with_env_refs_reinserted`)? Such scalars must be left
/// intact so the load-time expander resolves them; every other scalar is a
/// materialized literal whose `$` must be escaped for persist.
fn is_whole_env_ref(s: &str) -> bool {
    s.strip_prefix("${env:")
        .and_then(|rest| rest.strip_suffix('}'))
        .map(|inner| !inner.contains('{') && !inner.contains('}'))
        .unwrap_or(false)
}

/// Escape `$`→`$$` in every materialized string scalar of the value tree so the
/// load-time env expander (`$$`→`$`) round-trips it losslessly. Whole `${env:…}`
/// refs are left intact (they are meant to expand). See
/// [`Config::to_canonical_yaml_for_persist`].
fn escape_dollar_for_persist(v: &mut serde_yaml::Value) {
    match v {
        serde_yaml::Value::String(s) => {
            if !is_whole_env_ref(s) && s.contains('$') {
                *s = s.replace('$', "$$");
            }
        }
        serde_yaml::Value::Sequence(seq) => seq.iter_mut().for_each(escape_dollar_for_persist),
        serde_yaml::Value::Mapping(map) => {
            for (_, val) in map.iter_mut() {
                escape_dollar_for_persist(val);
            }
        }
        _ => {}
    }
}

/// Write `bytes` to `path` atomically. The file is first written to a
/// sibling tempfile (same directory, guarantees same filesystem) with a
/// unique suffix, then fsynced and renamed over `path`. On POSIX systems
/// `rename(2)` within a filesystem is atomic — observers see either the old
/// file, the new file, or (very briefly) ENOENT; never a half-written file.
///
/// Sibling-tempfile is critical: cross-filesystem rename would fall back to
/// a copy+unlink that is *not* atomic.
pub fn atomic_write(path: &std::path::Path, bytes: &[u8]) -> Result<(), ConfigError> {
    use std::io::Write as _;

    let parent = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let filename = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("dgp_config");

    // Build a unique sibling tempfile name. Not using tempfile::NamedTempFile
    // here because we need control over the final rename target, and the
    // crate's persist() API would still do the rename for us — just with
    // more ceremony. OsRng is strictly overkill for a name suffix; a pid +
    // nanos + random u64 is collision-resistant enough for config files
    // written O(once per human action).
    use rand::Rng as _;
    let suffix: u64 = rand::thread_rng().gen();
    let tmp_name = format!(".{}.tmp.{:x}", filename, suffix);
    let tmp_path = parent.join(tmp_name);

    // Write + fsync the tempfile. Scope the File so it's closed before
    // rename — some platforms (notably Windows) won't rename over an open
    // file, and on POSIX closing-before-rename is cleaner regardless.
    //
    // The persisted config carries secrets (SigV4 and backend credentials,
    // AES keys), so the file is never world-readable: 0600 for a new file;
    // a rewrite keeps the owner/group bits of the file it replaces and drops
    // every "other" bit. The tempfile is created with that mode, so the
    // secrets are never readable by others, not even before the rename.
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
            let mode = std::fs::metadata(path)
                .map(|m| m.permissions().mode() & 0o660)
                .unwrap_or(0o600);
            options.mode(mode);
            mode
        };
        let mut f = options
            .open(&tmp_path)
            .map_err(|e| ConfigError::Io(format!("create {}: {}", tmp_path.display(), e)))?;
        // The create-time mode is filtered by the umask; set it exactly.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(mode))
                .map_err(|e| ConfigError::Io(format!("chmod {}: {}", tmp_path.display(), e)))?;
        }
        f.write_all(bytes)
            .map_err(|e| ConfigError::Io(format!("write {}: {}", tmp_path.display(), e)))?;
        f.sync_all()
            .map_err(|e| ConfigError::Io(format!("fsync {}: {}", tmp_path.display(), e)))?;
    }

    std::fs::rename(&tmp_path, path).map_err(|e| {
        // Best-effort cleanup: don't leak tempfiles when rename fails
        // (e.g. target is on a different filesystem — shouldn't happen
        // because we picked the parent directory, but defense in depth).
        let _ = std::fs::remove_file(&tmp_path);
        ConfigError::Io(format!(
            "rename {} -> {}: {}",
            tmp_path.display(),
            path.display(),
            e
        ))
    })
}

/// Configuration errors
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("IO error: {0}")]
    Io(String),

    #[error("Parse error: {0}")]
    Parse(String),

    #[error("config references ${{env:{0}}} but that environment variable is unset or empty (and no `:-default` was given)")]
    MissingEnvVar(String),

    #[error("malformed `${{...}}` reference in config: {0}")]
    BadEnvRef(String),

    #[error("environment variable {0} expands to a value containing a newline or control character; such a value cannot be substituted into the config without breaking its structure — quote it or use a file reference")]
    UnsafeEnvValue(String),
}

/// Lines printed on the first run that generates a bootstrap password.
/// With a TTY: the plaintext, once. Without (containers, CI): neither the
/// plaintext nor the hash, because captured logs outlive the process and
/// the hash is also the SQLCipher key of the IAM database (S8).
fn first_run_banner(password: &str, hash: &str, is_tty: bool) -> Vec<String> {
    let mut out = vec![String::new()];
    if is_tty {
        out.push("╔══════════════════════════════════════════════════════════════╗".into());
        out.push("║  BOOTSTRAP PASSWORD (first run — save this!)                ║".into());
        out.push("║                                                              ║".into());
        out.push(format!("║  Password: {:<49}║", password));
        out.push("║                                                              ║".into());
        out.push("║  This password appears ONCE. Store it securely.              ║".into());
        out.push("║  Set DGP_BOOTSTRAP_PASSWORD_HASH to skip auto-generation.   ║".into());
        out.push("╚══════════════════════════════════════════════════════════════╝".into());
    } else {
        let _ = hash; // deliberately not printed
        out.push(
            "BOOTSTRAP PASSWORD auto-generated (not a TTY — password and hash hidden).".into(),
        );
        out.push("  The hash is in .deltaglider_bootstrap_hash (mode 0600).".into());
        out.push("  To choose a password, change it in the admin GUI (this keeps".into());
        out.push("  the IAM database readable). Before any IAM user exists, you can".into());
        out.push(
            "  also run `printf '%s\\n' '<pw>' | deltaglider_proxy --set-bootstrap-password`"
                .into(),
        );
        out.push("  (it reads the password from stdin), or set".into());
        out.push("  DGP_BOOTSTRAP_PASSWORD_HASH before the first start.".into());
    }
    out.push(String::new());
    out
}

/// Write the bootstrap hash file with restrictive permissions (0600).
/// This file doubles as the SQLCipher encryption key, so it must not be
/// world-readable — not even transiently. Create it 0600 in one syscall
/// (not fs::write-then-chmod, which leaves an umask-wide window on a path
/// where the key is known-valid).
pub fn write_bootstrap_hash_file(path: &std::path::Path, hash: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        use std::os::unix::fs::PermissionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        // `mode(0o600)` only applies at CREATE; a pre-existing looser file
        // keeps its old mode — repair it via the handle on every rewrite.
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        f.write_all(hash.as_bytes())?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, hash)
    }
}

#[cfg(test)]
mod tests;
