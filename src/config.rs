//! Configuration for DeltaGlider Proxy S3 server

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

/// A single entry in the environment variable registry.
pub struct EnvVarEntry {
    /// The environment variable name (e.g. `DGP_LISTEN_ADDR`)
    pub name: &'static str,
    /// Short human-readable description
    pub description: &'static str,
    /// Example value
    pub example: &'static str,
    /// Grouping category for display
    pub category: &'static str,
}

/// Single source of truth for every `DGP_*` environment variable.
///
/// A unit test enforces that this list matches `from_env()` exactly.
pub const ENV_VAR_REGISTRY: &[EnvVarEntry] = &[
    // ── Server ──────────────────────────────────────────────
    EnvVarEntry {
        name: "DGP_LISTEN_ADDR",
        description: "Listen address (ip:port)",
        example: "0.0.0.0:9000",
        category: "Server",
    },
    EnvVarEntry {
        name: "DGP_LOG_LEVEL",
        description: "Log level filter (overridden by RUST_LOG)",
        example: "deltaglider_proxy=debug,tower_http=debug",
        category: "Server",
    },
    EnvVarEntry {
        name: "DGP_CODEC_CONCURRENCY",
        description: "Max concurrent delta encode/decode ops (default: CPU cores)",
        example: "4",
        category: "Server",
    },
    EnvVarEntry {
        name: "DGP_BLOCKING_THREADS",
        description: "Max tokio blocking threads (default: 512)",
        example: "64",
        category: "Server",
    },
    EnvVarEntry {
        name: "DGP_CONFIG",
        description: "Path to TOML config file",
        example: "/etc/deltaglider_proxy/config.toml",
        category: "Server",
    },
    // ── Delta engine ────────────────────────────────────────
    EnvVarEntry {
        name: "DGP_MAX_DELTA_RATIO",
        description: "Max delta/original ratio to keep a delta (0.0–1.0)",
        example: "0.5",
        category: "Delta Engine",
    },
    EnvVarEntry {
        name: "DGP_MAX_OBJECT_SIZE",
        description: "Max object size in bytes for delta processing",
        example: "104857600",
        category: "Delta Engine",
    },
    EnvVarEntry {
        name: "DGP_CACHE_MB",
        description: "Reference cache size in MB",
        example: "100",
        category: "Delta Engine",
    },
    EnvVarEntry {
        name: "DGP_METADATA_CACHE_MB",
        description: "Metadata cache size in MB (object metadata, eliminates HEAD requests)",
        example: "50",
        category: "Delta Engine",
    },
    // ── Filesystem backend ──────────────────────────────────
    EnvVarEntry {
        name: "DGP_DATA_DIR",
        description: "Data directory (activates filesystem backend)",
        example: "./data",
        category: "Filesystem Backend",
    },
    // ── S3 backend ──────────────────────────────────────────
    EnvVarEntry {
        name: "DGP_S3_ENDPOINT",
        description: "S3 endpoint URL (activates S3 backend)",
        example: "http://localhost:9000",
        category: "S3 Backend",
    },
    EnvVarEntry {
        name: "DGP_S3_REGION",
        description: "AWS region",
        example: "us-east-1",
        category: "S3 Backend",
    },
    EnvVarEntry {
        name: "DGP_S3_PATH_STYLE",
        description: "Use path-style URLs (true/1 for MinIO/LocalStack)",
        example: "true",
        category: "S3 Backend",
    },
    EnvVarEntry {
        name: "DGP_BE_AWS_ACCESS_KEY_ID",
        description: "AWS access key for S3 backend",
        example: "minioadmin",
        category: "S3 Backend",
    },
    EnvVarEntry {
        name: "DGP_BE_AWS_SECRET_ACCESS_KEY",
        description: "AWS secret key for S3 backend",
        example: "minioadmin",
        category: "S3 Backend",
    },
    // ── Authentication ──────────────────────────────────────
    EnvVarEntry {
        name: "DGP_AUTHENTICATION",
        description:
            "Auth mode: omit to auto-detect (requires credentials), or \"none\" for open access",
        example: "none",
        category: "Authentication",
    },
    EnvVarEntry {
        name: "DGP_ACCESS_KEY_ID",
        description: "Proxy access key (enables SigV4 auth when both set)",
        example: "my-access-key",
        category: "Authentication",
    },
    EnvVarEntry {
        name: "DGP_SECRET_ACCESS_KEY",
        description: "Proxy secret key (enables SigV4 auth when both set)",
        example: "my-secret-key",
        category: "Authentication",
    },
    EnvVarEntry {
        name: "DGP_BOOTSTRAP_PASSWORD_HASH",
        description: "Bcrypt hash of bootstrap password (seeds DB encryption + admin GUI)",
        example: "$2b$12$...",
        category: "Authentication",
    },
    // ── TLS ─────────────────────────────────────────────────
    EnvVarEntry {
        name: "DGP_TLS_ENABLED",
        description: "Enable TLS (true/1)",
        example: "true",
        category: "TLS",
    },
    EnvVarEntry {
        name: "DGP_TLS_CERT",
        description: "Path to PEM certificate (auto-generates self-signed if omitted)",
        example: "/etc/ssl/certs/proxy.pem",
        category: "TLS",
    },
    EnvVarEntry {
        name: "DGP_TLS_KEY",
        description: "Path to PEM private key",
        example: "/etc/ssl/private/proxy-key.pem",
        category: "TLS",
    },
    // ── Config DB Sync ─────────────────────────────────────
    EnvVarEntry {
        name: "DGP_CONFIG_SYNC_BUCKET",
        description: "S3 bucket for config DB sync (enables multi-instance IAM sync)",
        example: "my-config-bucket",
        category: "Config Sync",
    },
    // ── Security / Runtime ─────────────────────────────────
    EnvVarEntry {
        name: "DGP_DEBUG_HEADERS",
        description: "Expose debug/fingerprinting headers (x-amz-storage-type etc.)",
        example: "true",
        category: "Server",
    },
    EnvVarEntry {
        name: "DGP_TRUST_PROXY_HEADERS",
        description: "Trust X-Forwarded-For/X-Real-IP for rate limiting and IAM conditions",
        example: "false",
        category: "Security",
    },
    EnvVarEntry {
        name: "DGP_SESSION_TTL_HOURS",
        description: "Admin session TTL in hours (default: 4)",
        example: "4",
        category: "Security",
    },
    EnvVarEntry {
        name: "DGP_MAX_MULTIPART_UPLOADS",
        description: "Max concurrent multipart uploads (default: 1000)",
        example: "1000",
        category: "Server",
    },
    EnvVarEntry {
        name: "DGP_CLOCK_SKEW_SECONDS",
        description: "SigV4 clock skew tolerance in seconds (default: 300)",
        example: "300",
        category: "Security",
    },
    EnvVarEntry {
        name: "DGP_MAX_CONCURRENT_REQUESTS",
        description: "Max concurrent HTTP requests (tower ConcurrencyLimit, default: 1024)",
        example: "1024",
        category: "Server",
    },
    EnvVarEntry {
        name: "DGP_CORS_PERMISSIVE",
        description: "Enable permissive CORS for dev mode (default: false)",
        example: "true",
        category: "Server",
    },
    EnvVarEntry {
        name: "DGP_REQUEST_TIMEOUT_SECS",
        description: "Per-request timeout in seconds (default: 300)",
        example: "300",
        category: "Server",
    },
    EnvVarEntry {
        name: "DGP_CODEC_TIMEOUT_SECS",
        description: "xdelta3 subprocess timeout in seconds (default: 60)",
        example: "60",
        category: "Delta Engine",
    },
    EnvVarEntry {
        name: "DGP_RATE_LIMIT_MAX_ATTEMPTS",
        description: "Max failed auth attempts before IP lockout (default: 100)",
        example: "100",
        category: "Security",
    },
    EnvVarEntry {
        name: "DGP_RATE_LIMIT_WINDOW_SECS",
        description: "Rate limit rolling window in seconds (default: 300)",
        example: "300",
        category: "Security",
    },
    EnvVarEntry {
        name: "DGP_RATE_LIMIT_LOCKOUT_SECS",
        description: "Rate limit lockout duration in seconds (default: 600)",
        example: "600",
        category: "Security",
    },
    EnvVarEntry {
        name: "DGP_REPLAY_WINDOW_SECS",
        description: "SigV4 replay detection window in seconds (default: 2)",
        example: "2",
        category: "Security",
    },
    EnvVarEntry {
        name: "DGP_SECURE_COOKIES",
        description: "Require HTTPS for admin session cookies (default: true)",
        example: "true",
        category: "Security",
    },
];

/// Default config filename used by `--init` and legacy persistence (TOML).
pub const DEFAULT_CONFIG_FILENAME: &str = "deltaglider_proxy.toml";

/// Default YAML config filename (preferred for new deployments).
pub const DEFAULT_YAML_CONFIG_FILENAME: &str = "deltaglider_proxy.yaml";

/// Ordered list of default config file locations. YAML is preferred over TOML
/// when both exist in the same directory.
pub const DEFAULT_CONFIG_SEARCH_PATHS: &[&str] = &[
    DEFAULT_YAML_CONFIG_FILENAME,
    "deltaglider_proxy.yml",
    DEFAULT_CONFIG_FILENAME,
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

    /// Reference cache size in MB
    #[serde(default = "default_cache_size_mb")]
    pub cache_size_mb: usize,

    /// Metadata cache size in MB (object metadata, eliminates HEAD requests).
    /// Default: 50 MB (~125K entries). Set to 0 to disable.
    #[serde(default = "default_metadata_cache_mb")]
    pub metadata_cache_mb: usize,

    /// Explicit authentication mode selector.
    ///
    /// Accepted values:
    ///   - `"none"` — Open access, no SigV4 verification. Must be explicit.
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
    /// Default: "deltaglider_proxy=debug,tower_http=debug"
    #[serde(default = "default_log_level")]
    pub log_level: String,

    /// S3 bucket for config DB sync (multi-instance IAM).
    /// When set, the encrypted config DB is synced to/from this S3 bucket.
    #[serde(default)]
    pub config_sync_bucket: Option<String>,

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

    /// Operator-authored admission blocks.
    ///
    /// Parsed from `admission.blocks:` in the sectioned YAML OR from
    /// `admission_blocks:` at the root of a flat-shape YAML (both
    /// shapes round-trip through `from_yaml_str` → `to_canonical_yaml`).
    /// TOML loading carries the field as well, though
    /// `admission_blocks:` typing is limited in TOML and operators
    /// should prefer YAML for admission authoring.
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
}

/// Per-backend encryption-at-rest configuration.
///
/// Replaces the former global `advanced.encryption_key` single-key model.
/// Each named backend declares its OWN mode; the engine wraps each
/// backend independently (see `src/deltaglider/engine/mod.rs` backend
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
/// `resolve_encryption` in engine/mod.rs) is load-bearing: two
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
    /// shim during mode transitions (see engine/mod.rs resolver).
    Aes256GcmProxy {
        /// 64-char hex key. Infra secret; stripped by redactors.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key: Option<String>,
        /// Optional stable id. Max 64 chars, `[A-Za-z0-9_.-]` only
        /// (S3 user-metadata header-safe).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key_id: Option<String>,
        /// Decrypt-only shim: preserve the ability to READ objects
        /// that were written with a previous key. Unused in `Aes256GcmProxy`
        /// mode (use `key` to rotate); reserved for transitions TO
        /// native modes.
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
        #[serde(default = "default_true")]
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

/// Default function for `bucket_key_enabled` serde attribute.
pub(crate) fn default_true() -> bool {
    true
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
        match self {
            Self::None { legacy_key, .. } => {
                *legacy_key = None;
            }
            Self::Aes256GcmProxy {
                key, legacy_key, ..
            } => {
                *key = None;
                *legacy_key = None;
            }
            Self::SseKms {
                legacy_key,
                kms_key_id: _,
                ..
            } => {
                // kms_key_id is an ARN — NOT secret. Operators need to
                // see it to know WHICH KMS key.
                *legacy_key = None;
            }
            Self::SseS3 { legacy_key, .. } => {
                *legacy_key = None;
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
pub(crate) fn apply_backend_encryption_env(backend_name: &str, enc: &mut BackendEncryptionConfig) {
    let (key_env, kms_env): (String, String) = if backend_name == "default" {
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
    };

    match enc {
        BackendEncryptionConfig::Aes256GcmProxy { key, .. } => {
            if let Ok(env_key) = std::env::var(&key_env) {
                if !env_key.is_empty() {
                    *key = Some(env_key);
                }
            }
        }
        BackendEncryptionConfig::SseKms { kms_key_id, .. } => {
            if let Ok(env_arn) = std::env::var(&kms_env) {
                if !env_arn.is_empty() {
                    *kms_key_id = env_arn;
                }
            }
        }
        // None and SseS3 carry no secrets beyond the legacy shim;
        // env vars for primary-key material are silently ignored
        // (the shim key stays in YAML/config and is not env-overridden).
        BackendEncryptionConfig::None { .. } | BackendEncryptionConfig::SseS3 { .. } => {}
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
        #[serde(default = "default_force_path_style")]
        force_path_style: bool,

        /// AWS access key ID (optional, can use env/instance credentials)
        #[serde(default)]
        access_key_id: Option<String>,

        /// AWS secret access key (optional, can use env/instance credentials)
        #[serde(default)]
        secret_access_key: Option<String>,
    },
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

pub(crate) fn default_cache_size_mb() -> usize {
    100
}

pub(crate) fn default_metadata_cache_mb() -> usize {
    50
}

fn default_region() -> String {
    "us-east-1".to_string()
}

fn default_force_path_style() -> bool {
    true
}

pub(crate) fn default_log_level() -> String {
    "deltaglider_proxy=debug,tower_http=debug".to_string()
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
            cache_size_mb: default_cache_size_mb(),
            metadata_cache_mb: default_metadata_cache_mb(),
            authentication: None,
            access_key_id: None,
            secret_access_key: None,
            bootstrap_password_hash: None,
            codec_concurrency: None,
            blocking_threads: None,
            log_level: default_log_level(),
            config_sync_bucket: None,
            tls: None,
            buckets: std::collections::BTreeMap::new(),
            backends: Vec::new(),
            default_backend: None,
            backend_encryption: BackendEncryptionConfig::default(),
            admission_blocks: Vec::new(),
            iam_mode: crate::config_sections::IamMode::default(),
        }
    }
}

/// Parse an env var into a typed value, warning on invalid input.
pub fn env_parse<T: std::str::FromStr>(var: &str) -> Option<T>
where
    T::Err: std::fmt::Display,
{
    std::env::var(var).ok().and_then(|raw| {
        raw.parse()
            .map_err(|e| eprintln!("Warning: ignoring invalid {var}=\"{raw}\": {e}"))
            .ok()
    })
}

/// Parse an env var into a typed value, returning `default` if absent or invalid.
/// Logs a warning on invalid input (same as `env_parse`).
pub fn env_parse_with_default<T: std::str::FromStr>(var: &str, default: T) -> T
where
    T::Err: std::fmt::Display,
{
    env_parse(var).unwrap_or(default)
}

/// Parse a boolean env var, returning `default` if absent or
/// unrecognised. Accepts (case-insensitive, trimmed): `true`, `1`,
/// `yes`, `on` as true; `false`, `0`, `no`, `off` as false.
///
/// Unrecognised values log a warning and fall back to `default` so
/// operator typos don't silently flip behaviour in either direction.
pub fn env_bool(var: &str, default: bool) -> bool {
    let Ok(raw) = std::env::var(var) else {
        return default;
    };
    let trimmed = raw.trim();
    match trimmed.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => true,
        "false" | "0" | "no" | "off" => false,
        other => {
            tracing::warn!(
                target: "deltaglider_proxy::config",
                var = %var,
                value = %other,
                "env var `{}={}` is not a recognised boolean (expected true/1/yes/on or \
                 false/0/no/off); falling back to default={}",
                var,
                other,
                default
            );
            default
        }
    }
}

/// Supported config file formats, inferred from file extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigFormat {
    Toml,
    Yaml,
}

impl ConfigFormat {
    /// Infer the format from a file path's extension. Defaults to TOML for
    /// unknown/missing extensions (backwards compatibility).
    pub fn from_path(path: &str) -> Self {
        match std::path::Path::new(path)
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s.to_ascii_lowercase())
            .as_deref()
        {
            Some("yaml") | Some("yml") => ConfigFormat::Yaml,
            _ => ConfigFormat::Toml,
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
        "authentication",
        "access_key_id",
        "secret_access_key",
        "bootstrap_password_hash",
        "admin_password_hash", // legacy alias for bootstrap_password_hash
        "codec_concurrency",
        "blocking_threads",
        "log_level",
        "config_sync_bucket",
        "backend_encryption",
        "tls",
        "buckets",
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

impl Config {
    /// Load configuration from a file. Dispatches on extension: `.yaml`/`.yml`
    /// → YAML, anything else → TOML.
    pub fn from_file(path: &str) -> Result<Self, ConfigError> {
        match ConfigFormat::from_path(path) {
            ConfigFormat::Yaml => Self::from_yaml_file(path),
            ConfigFormat::Toml => Self::from_toml_file(path),
        }
    }

    /// Parse TOML content into a validated `Config`.
    ///
    /// Runs the same `normalize_shorthands` pipeline as the YAML
    /// loader, so callers (including `config lint`) see identical
    /// semantic validation regardless of input format.
    ///
    /// Unlike [`Self::from_toml_file`], this does NOT emit the TOML
    /// deprecation warning — the warning is the `from_toml_file`
    /// caller's signal ("loaded TOML from disk"), not "parsed TOML
    /// bytes" which also fires from lint / tests / in-process use.
    pub fn from_toml_str(content: &str) -> Result<Self, ConfigError> {
        let mut config: Config =
            toml::from_str(content).map_err(|e| ConfigError::Parse(e.to_string()))?;
        config.normalize_shorthands()?;
        Ok(config)
    }

    /// Load configuration from a TOML file explicitly.
    ///
    /// **Deprecated**: TOML is being phased out in favor of YAML. Every
    /// successful load emits a `tracing::warn!` pointing at the
    /// migration tool. The flag `DGP_SILENCE_TOML_DEPRECATION=1`
    /// silences the warning for environments that cannot upgrade
    /// immediately (e.g. vendored config in a container image).
    ///
    /// Removal timeline: TOML support stays through the next two minor
    /// releases (grace period for migration), then the loader returns
    /// `ConfigError::Parse("TOML is no longer supported; use YAML")`
    /// unconditionally. `deltaglider_proxy config migrate` converts in
    /// one shot.
    pub fn from_toml_file(path: &str) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path).map_err(|e| ConfigError::Io(e.to_string()))?;
        let config = Self::from_toml_str(&content)?;

        // Phase 6 deprecation warn. Fires exactly once per load (no
        // per-request overhead — config loads only at startup and on
        // explicit apply). Silencable via env var for operators who
        // know about the deprecation and cannot upgrade yet.
        if !env_bool("DGP_SILENCE_TOML_DEPRECATION", false) {
            tracing::warn!(
                target: "deltaglider_proxy::config",
                path = %path,
                "[config] TOML config format is deprecated. Convert to YAML with \
                 `deltaglider_proxy config migrate {} --out deltaglider_proxy.yaml` \
                 and point the server at the new file. TOML support will be removed \
                 in a future minor release. Set DGP_SILENCE_TOML_DEPRECATION=1 to \
                 suppress this warning.",
                path
            );
        }

        Ok(config)
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
        Self::from_yaml_str(&content)
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
                    serde_yaml::from_value(doc).map_err(|e| ConfigError::Parse(e.to_string()))?;
                sectioned.into_flat().map_err(ConfigError::Parse)?
            }
            ConfigShape::Flat => {
                serde_yaml::from_value(doc).map_err(|e| ConfigError::Parse(e.to_string()))?
            }
            ConfigShape::Mixed {
                flat_keys,
                section_keys,
            } => {
                return Err(ConfigError::Parse(format!(
                    "config YAML mixes legacy flat keys ({}) with Phase 3+ section headers ({}); \
                     pick one shape. The canonical export (`deltaglider_proxy config migrate` / \
                     the admin API `/config/export`) always emits the sectioned shape.",
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
        for (name, policy) in self.buckets.iter_mut() {
            policy
                .normalize()
                .map_err(|e| ConfigError::Parse(format!("bucket `{}`: {}", name, e)))?;
        }
        // Validate admission blocks on EVERY load path — the sectioned
        // loader also calls `AdmissionSpec::validate` inside
        // `into_flat`, but that bypasses flat-shape YAML and TOML.
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
    fn apply_env_overrides(&mut self) {
        if let Ok(addr) = std::env::var("DGP_LISTEN_ADDR") {
            match addr.parse() {
                Ok(parsed) => self.listen_addr = parsed,
                Err(e) => eprintln!("Warning: ignoring invalid DGP_LISTEN_ADDR=\"{addr}\": {e}"),
            }
        }

        // Check for S3 backend configuration
        if std::env::var("DGP_S3_ENDPOINT").is_ok() || std::env::var("DGP_S3_REGION").is_ok() {
            self.backend = BackendConfig::S3 {
                endpoint: std::env::var("DGP_S3_ENDPOINT").ok(),
                region: std::env::var("DGP_S3_REGION").unwrap_or_else(|_| "us-east-1".to_string()),
                force_path_style: std::env::var("DGP_S3_PATH_STYLE")
                    .map(|v| v == "true" || v == "1")
                    .unwrap_or(true),
                access_key_id: std::env::var("DGP_BE_AWS_ACCESS_KEY_ID").ok(),
                secret_access_key: std::env::var("DGP_BE_AWS_SECRET_ACCESS_KEY").ok(),
            };
        } else if let Ok(dir) = std::env::var("DGP_DATA_DIR") {
            self.backend = BackendConfig::Filesystem {
                path: PathBuf::from(dir),
            };
        }

        if let Some(v) = env_parse::<f32>("DGP_MAX_DELTA_RATIO") {
            self.max_delta_ratio = v;
        }
        if let Some(v) = env_parse::<u64>("DGP_MAX_OBJECT_SIZE") {
            self.max_object_size = v;
        }
        if let Some(v) = env_parse::<usize>("DGP_CACHE_MB") {
            self.cache_size_mb = v;
        }
        if let Some(v) = env_parse::<usize>("DGP_METADATA_CACHE_MB") {
            self.metadata_cache_mb = v;
        }
        if let Some(v) = env_parse::<usize>("DGP_CODEC_CONCURRENCY") {
            self.codec_concurrency = Some(v);
        }
        if let Some(v) = env_parse::<usize>("DGP_BLOCKING_THREADS") {
            self.blocking_threads = Some(v);
        }

        // Authentication mode
        if let Ok(v) = std::env::var("DGP_AUTHENTICATION") {
            self.authentication = Some(v);
        }

        // Proxy authentication credentials
        if let Ok(v) = std::env::var("DGP_ACCESS_KEY_ID") {
            self.access_key_id = Some(v);
        }
        if let Ok(v) = std::env::var("DGP_SECRET_ACCESS_KEY") {
            self.secret_access_key = Some(v);
        }

        // Admin GUI password hash
        if let Ok(v) = std::env::var("DGP_BOOTSTRAP_PASSWORD_HASH")
            .or_else(|_| std::env::var("DGP_ADMIN_PASSWORD_HASH"))
        {
            self.bootstrap_password_hash = Some(v);
        }

        // Log level (runtime operational)
        if let Ok(level) = std::env::var("DGP_LOG_LEVEL") {
            self.log_level = level;
        }

        // Config DB S3 sync
        if let Ok(bucket) = std::env::var("DGP_CONFIG_SYNC_BUCKET") {
            self.config_sync_bucket = Some(bucket);
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
            apply_backend_encryption_env(&named.name, &mut named.encryption);
        }
        // Singleton backend path: uses the unadorned `DGP_ENCRYPTION_KEY`
        // / `DGP_SSE_KMS_KEY_ID` so single-backend deployments keep a
        // short env-var name. The synthetic backend name is "default".
        apply_backend_encryption_env("default", &mut self.backend_encryption);

        // TLS configuration
        if let Ok(enabled) = std::env::var("DGP_TLS_ENABLED") {
            if enabled == "true" || enabled == "1" {
                self.tls = Some(TlsConfig {
                    enabled: true,
                    cert_path: std::env::var("DGP_TLS_CERT").ok(),
                    key_path: std::env::var("DGP_TLS_KEY").ok(),
                });
            }
        }
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
    ///    [`DEFAULT_CONFIG_SEARCH_PATHS`]. YAML is preferred over TOML.
    pub fn resolve_config_path() -> Option<String> {
        if let Ok(path) = std::env::var("DGP_CONFIG") {
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
    pub fn load() -> Self {
        let mut config = if let Ok(path) = std::env::var("DGP_CONFIG") {
            match Self::from_file(&path) {
                Ok(c) => c,
                Err(e) => {
                    eprintln!(
                        "WARNING: Failed to parse config file '{}': {} — using defaults",
                        path, e
                    );
                    Self::default()
                }
            }
        } else {
            // Try default config file locations (YAML first, then TOML)
            let mut found = None;
            for path in DEFAULT_CONFIG_SEARCH_PATHS {
                if std::path::Path::new(path).exists() {
                    if let Ok(config) = Self::from_file(path) {
                        found = Some(config);
                        break;
                    }
                }
            }
            found.unwrap_or_default()
        };

        // Environment variables always override file config
        config.apply_env_overrides();
        config.validate();
        config
    }

    /// Load config from an explicit path, with env var overrides applied on
    /// top and `validate()` run at the end.
    ///
    /// This is the `--config <path>` entry point. Without this helper, callers
    /// that specify a config path (via the CLI flag) had to remember to call
    /// [`Self::apply_env_overrides`] themselves — and the main startup path
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

    /// Check the config for problems. Returns a list of human-readable
    /// warnings; also clears fields that cannot be satisfied (currently just
    /// unresolvable `default_backend`).
    ///
    /// Single source of truth for config validation. The startup path calls
    /// [`Self::validate`] which is a thin wrapper that logs each warning to
    /// stderr; the admin API calls `check` directly to return warnings as
    /// structured data.
    pub fn check(&mut self) -> Vec<String> {
        let mut warnings = Vec::new();
        // NaN and infinity are valid YAML float literals (`.nan` / `.inf`) but
        // break the downstream ratio test: NaN comparisons are always false, so
        // NaN silently disables delta compression; INFINITY > 1.0 is true so a
        // naive warning fires, but the value survives and causes every file to
        // be stored as a delta regardless of size. Clamp both to the default
        // so neither can corrupt compression decisions.
        if self.max_delta_ratio.is_nan() {
            warnings.push("max_delta_ratio is NaN — replacing with default 0.75".to_string());
            self.max_delta_ratio = default_max_delta_ratio();
        } else if self.max_delta_ratio.is_infinite() {
            warnings.push("max_delta_ratio is infinite — replacing with default 0.75".to_string());
            self.max_delta_ratio = default_max_delta_ratio();
        } else if self.max_delta_ratio < 0.0 || self.max_delta_ratio > 1.0 {
            warnings.push(format!(
                "max_delta_ratio={} is outside [0.0, 1.0] — delta compression decisions may behave unexpectedly",
                self.max_delta_ratio
            ));
        }
        if self.max_object_size == 0 {
            warnings.push("max_object_size=0 will reject all uploads".to_string());
        }
        // Reject duplicate backend names. The routing layer keys on name, so
        // a second `{ name: "x", ... }` silently shadows the first — and if
        // the list is ever reordered (sort, filter, de-dup elsewhere) routing
        // changes without warning. Warn so operators know a duplicate is
        // present; the first entry wins at runtime.
        if self.backends.len() > 1 {
            let mut seen = std::collections::HashSet::new();
            let mut duplicates = std::collections::BTreeSet::new();
            for backend in &self.backends {
                if !seen.insert(backend.name.as_str()) {
                    duplicates.insert(backend.name.as_str());
                }
            }
            if !duplicates.is_empty() {
                warnings.push(format!(
                    "duplicate backend name(s) found: {:?} — the first entry wins at routing time; remove duplicates to silence this warning",
                    duplicates.iter().collect::<Vec<_>>()
                ));
            }
        }

        if let Some(ref default) = self.default_backend {
            if !self.backends.is_empty() && !self.backends.iter().any(|b| &b.name == default) {
                warnings.push(format!(
                    "default_backend='{}' not found in backends list {:?} — clearing",
                    default,
                    self.backends.iter().map(|b| &b.name).collect::<Vec<_>>()
                ));
                self.default_backend = None;
            }
        }
        for (bucket, policy) in &self.buckets {
            if let Some(ref backend) = policy.backend {
                if !self.backends.is_empty() && !self.backends.iter().any(|b| &b.name == backend) {
                    warnings.push(format!(
                        "bucket '{}' routes to unknown backend '{}' — route will be ignored",
                        bucket, backend
                    ));
                }
            }
        }

        // Per-backend encryption validation. Each named backend + the
        // legacy singleton gets checked against:
        //   * native modes (SseKms / SseS3) on filesystem backends → error.
        //   * Aes256GcmProxy with no key configured (after env-var
        //     resolution) → warning (key must come from env at runtime;
        //     this is informational so operators notice a missing
        //     DGP_*_ENCRYPTION_KEY before a read fails).
        //   * key_id charset: must match `[A-Za-z0-9_.-]{1,64}`.
        //   * collisions: two backends declaring the same `key_id`
        //     but different `key` bytes → error.
        let mut explicit_key_ids: std::collections::BTreeMap<
            String,
            Vec<(String, Option<String>)>,
        > = std::collections::BTreeMap::new();
        let mut validate_entry = |label: &str,
                                  backend: &BackendConfig,
                                  enc: &BackendEncryptionConfig,
                                  warnings: &mut Vec<String>| {
            if matches!(backend, BackendConfig::Filesystem { .. })
                && matches!(
                    enc,
                    BackendEncryptionConfig::SseKms { .. } | BackendEncryptionConfig::SseS3 { .. }
                )
            {
                warnings.push(format!(
                    "backend '{}' uses a native S3 encryption mode ({}) on a filesystem \
                     backend — native modes require S3. Change mode to 'aes256-gcm-proxy' \
                     or 'none'.",
                    label,
                    enc.mode_tag()
                ));
            }
            if let BackendEncryptionConfig::Aes256GcmProxy {
                key,
                key_id,
                legacy_key: _,
                legacy_key_id: _,
            } = enc
            {
                if key.is_none() {
                    warnings.push(format!(
                        "backend '{}' uses aes256-gcm-proxy but no key is configured in \
                         YAML — set it via env var ({}).",
                        label,
                        if label == "default" {
                            "DGP_ENCRYPTION_KEY".to_string()
                        } else {
                            format!(
                                "DGP_BACKEND_{}_ENCRYPTION_KEY",
                                env_suffix_for_backend_name(label)
                            )
                        }
                    ));
                }
                if let Some(kid) = key_id {
                    if !is_valid_key_id(kid) {
                        warnings.push(format!(
                            "backend '{}' has encryption.key_id='{}' — must match \
                             [A-Za-z0-9_.-]{{1,64}} (S3 user-metadata header-safe).",
                            label, kid
                        ));
                    }
                    if let Some(k) = key {
                        explicit_key_ids
                            .entry(kid.clone())
                            .or_default()
                            .push((label.to_string(), Some(k.clone())));
                    }
                }
            }
        };
        validate_entry(
            "default",
            &self.backend,
            &self.backend_encryption,
            &mut warnings,
        );
        for named in &self.backends {
            validate_entry(
                &named.name,
                &named.backend,
                &named.encryption,
                &mut warnings,
            );
        }
        // Cross-backend: same explicit key_id but different keys.
        for (kid, entries) in &explicit_key_ids {
            if entries.len() > 1 {
                let distinct_keys: std::collections::BTreeSet<&Option<String>> =
                    entries.iter().map(|(_, k)| k).collect();
                if distinct_keys.len() > 1 {
                    let names: Vec<String> = entries.iter().map(|(n, _)| n.clone()).collect();
                    warnings.push(format!(
                        "backends {:?} share key_id='{}' but declare DIFFERENT keys. \
                         Either make the keys identical (intentional cross-backend portability) \
                         or give each backend a distinct key_id.",
                        names, kid
                    ));
                }
            }
        }

        // Env-var suffix collision check (correctness x-ray C4):
        // Two backends whose names normalise to the same env suffix
        // would both read the SAME env var
        // (`DGP_BACKEND_<SUFFIX>_ENCRYPTION_KEY`). When both land with
        // the same key material but different derived key_ids (because
        // the raw backend NAME feeds into `derive_key_id`, not the
        // suffix), objects written by one cannot be read by the other.
        // Detectable at config load time; warn loudly so operators
        // rename one.
        if self.backends.len() > 1 {
            let mut suffix_owners: std::collections::BTreeMap<String, Vec<String>> =
                std::collections::BTreeMap::new();
            for backend in &self.backends {
                // Skip names that already normalize to the bare
                // "DEFAULT" suffix — that collides with the singleton
                // `DGP_ENCRYPTION_KEY` path, which is an independent
                // surface; the operator probably meant a DIFFERENT
                // collision warning which would confuse the signal.
                let suffix = env_suffix_for_backend_name(&backend.name);
                suffix_owners
                    .entry(suffix)
                    .or_default()
                    .push(backend.name.clone());
            }
            for (suffix, names) in &suffix_owners {
                if names.len() > 1 {
                    warnings.push(format!(
                        "backends {:?} normalize to the same env-var suffix '{}' — \
                         all would read DGP_BACKEND_{}_ENCRYPTION_KEY, but each has a \
                         distinct derived key_id. Rename one of them to avoid silent \
                         cross-backend key sharing (Aes256GcmProxy cross-decrypt will fail \
                         with 'key id mismatch').",
                        names, suffix, suffix
                    ));
                }
            }
        }

        warnings
    }

    /// Run [`Self::check`] and log each warning to stderr. Used by the
    /// startup path where eprintln is the right sink.
    pub fn validate(&mut self) {
        for warning in self.check() {
            eprintln!("Warning: {}", warning);
        }
    }

    /// Returns true if SigV4 authentication is enabled (both credentials are set).
    pub fn auth_enabled(&self) -> bool {
        self.access_key_id.is_some() && self.secret_access_key.is_some()
    }

    /// Returns true if TLS is enabled.
    pub fn tls_enabled(&self) -> bool {
        self.tls.as_ref().is_some_and(|t| t.enabled)
    }

    /// Decode a hash value: if it looks like base64 (no `$` prefix), decode it.
    /// Otherwise return as-is (raw bcrypt hash). Validates the result is a bcrypt hash.
    fn decode_hash(value: &str) -> String {
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
    /// 1. Already set in config (env var or TOML) — use it.
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
        eprintln!();
        if std::io::stderr().is_terminal() {
            eprintln!("╔══════════════════════════════════════════════════════════════╗");
            eprintln!("║  BOOTSTRAP PASSWORD (first run — save this!)                ║");
            eprintln!("║                                                              ║");
            eprintln!("║  Password: {:<49}║", password);
            eprintln!("║                                                              ║");
            eprintln!("║  This password appears ONCE. Store it securely.              ║");
            eprintln!("║  Set DGP_BOOTSTRAP_PASSWORD_HASH to skip auto-generation.   ║");
            eprintln!("╚══════════════════════════════════════════════════════════════╝");
        } else {
            eprintln!("BOOTSTRAP PASSWORD auto-generated (not a TTY — plaintext hidden).");
            eprintln!("  Hash: {}", hash);
            eprintln!("  Set DGP_BOOTSTRAP_PASSWORD_HASH={}", hash);
            eprintln!("  Or run interactively to see the plaintext password.");
        }
        eprintln!();

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

    /// Print an example TOML config derived from `Config::default()`.
    ///
    /// The default section is programmatic (any new `#[serde(default)]` field
    /// appears automatically). A commented-out S3 + TLS + auth variant is
    /// appended so every option is visible.
    pub fn print_example_toml() {
        let default_cfg = Config::default();
        let base = toml::to_string_pretty(&default_cfg).expect("Config serializes to TOML");
        println!("# DeltaGlider Proxy — example configuration");
        println!("# Generated from compiled-in defaults\n");
        println!("{base}");

        // Append commented-out advanced sections
        let mut extra = String::new();
        let _ = writeln!(
            extra,
            "# ── S3 backend (uncomment to switch from filesystem) ──"
        );
        let _ = writeln!(extra, "# [backend]");
        let _ = writeln!(extra, "# type = \"s3\"");
        let _ = writeln!(extra, "# endpoint = \"http://localhost:9000\"");
        let _ = writeln!(extra, "# region = \"us-east-1\"");
        let _ = writeln!(extra, "# force_path_style = true");
        let _ = writeln!(extra, "# access_key_id = \"minioadmin\"");
        let _ = writeln!(extra, "# secret_access_key = \"minioadmin\"");
        let _ = writeln!(extra);
        let _ = writeln!(extra, "# ── Proxy authentication (SigV4) ──");
        let _ = writeln!(extra, "# access_key_id = \"my-access-key\"");
        let _ = writeln!(extra, "# secret_access_key = \"my-secret-key\"");
        let _ = writeln!(extra);
        let _ = writeln!(extra, "# ── TLS ──");
        let _ = writeln!(extra, "# [tls]");
        let _ = writeln!(extra, "# enabled = true");
        let _ = writeln!(extra, "# cert_path = \"/etc/ssl/certs/proxy.pem\"");
        let _ = writeln!(extra, "# key_path = \"/etc/ssl/private/proxy-key.pem\"");
        print!("{extra}");
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

    /// Export variant: strips infra secrets AND every per-backend
    /// encryption key (singleton + named list). Intended for the admin
    /// API download endpoint where operators read the YAML out of band.
    /// A persisted file pulled from the box might make it into GitOps
    /// or a bug report; we don't want encryption keys to follow it.
    fn redact_for_export(&self) -> Self {
        let mut export = self.redact_for_persist();
        export.backend_encryption.redact_secrets();
        for named in &mut export.backends {
            named.encryption.redact_secrets();
        }
        export
    }

    /// Clone the config with *every* secret redacted: infra secrets plus all
    /// SigV4 credentials (top-level and per-backend). This is the right level
    /// of paranoia for the admin API `GET /export` endpoint (Phase 1): the
    /// operator reading the exported YAML must refill secrets from their
    /// secret manager, not copy them out of an API response.
    pub fn redact_all_secrets(&self) -> Self {
        let mut export = self.redact_for_export();
        if let BackendConfig::S3 {
            ref mut access_key_id,
            ref mut secret_access_key,
            ..
        } = export.backend
        {
            *access_key_id = None;
            *secret_access_key = None;
        }
        for named in &mut export.backends {
            if let BackendConfig::S3 {
                ref mut access_key_id,
                ref mut secret_access_key,
                ..
            } = named.backend
            {
                *access_key_id = None;
                *secret_access_key = None;
            }
        }
        export.access_key_id = None;
        export.secret_access_key = None;
        export
    }

    /// Serialize config to TOML string (strips infra secrets: bootstrap
    /// hash and per-backend encryption keys). SigV4 credentials are kept —
    /// see [`Self::redact_all_secrets`] for the fully-redacted export
    /// variant.
    ///
    /// **Used for `GET /config/export` and `config migrate`** where the
    /// emitted YAML/TOML is a downloadable artifact. The persist path
    /// deliberately does NOT go through this function — see
    /// [`Self::persist_to_file`] for the rationale.
    pub fn to_toml_string(&self) -> Result<String, ConfigError> {
        let export = self.redact_for_export();
        toml::to_string_pretty(&export).map_err(|e| ConfigError::Parse(e.to_string()))
    }

    /// Serialize config to canonical YAML string.
    ///
    /// Emits the Phase 3 **sectioned** shape: top-level `admission:` /
    /// `access:` / `storage:` / `advanced:` groups, with each group omitted
    /// when it equals its default (minimal-diff GitOps-friendly output).
    /// Strips infra secrets (same policy as `to_toml_string`) so that
    /// `config migrate`, `config show`, and the admin `/export` endpoint
    /// never leak the bootstrap hash or the AES master key into disk
    /// artifacts.
    ///
    /// The dual-shape deserializer accepts the legacy flat YAML too, but
    /// we only ever *emit* sectioned — legacy readers eventually disappear,
    /// the canonical artifact must be forward-shaped.
    pub fn to_canonical_yaml(&self) -> Result<String, ConfigError> {
        let export = self.redact_for_export();
        let sectioned = crate::config_sections::SectionedConfig::from_flat(&export);
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
    fn to_toml_string_for_persist(&self) -> Result<String, ConfigError> {
        let export = self.redact_for_persist();
        toml::to_string_pretty(&export).map_err(|e| ConfigError::Parse(e.to_string()))
    }

    fn to_canonical_yaml_for_persist(&self) -> Result<String, ConfigError> {
        let export = self.redact_for_persist();
        let sectioned = crate::config_sections::SectionedConfig::from_flat(&export);
        serde_yaml::to_string(&sectioned).map_err(|e| ConfigError::Parse(e.to_string()))
    }

    /// Persist the current config to a file atomically. Dispatches on
    /// extension: `.yaml` / `.yml` writes YAML, anything else writes TOML.
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
    ///
    /// Returns `ConfigError::Parse` when the target is a TOML file but
    /// the config holds YAML-only fields. Rationale: operator-authored
    /// admission blocks (especially the tagged `Reject { type, status,
    /// message }` action) and `iam_mode: declarative` don't round-trip
    /// cleanly through TOML's `toml::to_string_pretty`, and a silent
    /// half-persist (in-memory state updated, on-disk file missing the
    /// new fields) is the worst failure mode possible — next restart
    /// silently reverts.
    pub fn persist_to_file(&self, path: &str) -> Result<(), ConfigError> {
        let format = ConfigFormat::from_path(path);
        if matches!(format, ConfigFormat::Toml) {
            self.check_toml_persistable(path)?;
        }
        let content = match format {
            ConfigFormat::Yaml => self.to_canonical_yaml_for_persist()?,
            ConfigFormat::Toml => self.to_toml_string_for_persist()?,
        };
        atomic_write(std::path::Path::new(path), content.as_bytes())
    }

    /// Refuse to persist to TOML when the config holds fields that are
    /// YAML-only (admission blocks with tagged actions, declarative IAM
    /// mode). Called from `persist_to_file` before serialisation so
    /// the operator sees an actionable error instead of a silent
    /// half-persist.
    fn check_toml_persistable(&self, path: &str) -> Result<(), ConfigError> {
        let mut blockers: Vec<&'static str> = Vec::new();
        if !self.admission_blocks.is_empty() {
            blockers.push("admission.blocks");
        }
        if self.iam_mode != crate::config_sections::IamMode::default() {
            blockers.push("access.iam_mode");
        }
        if blockers.is_empty() {
            return Ok(());
        }
        Err(ConfigError::Parse(format!(
            "cannot persist runtime config to `{path}` (TOML): the in-memory config holds \
             YAML-only fields ({}). Convert the target file to YAML first — run \
             `deltaglider_proxy config migrate {path} --out {path_yaml}` — and point the \
             server at the new file, then re-apply.",
            blockers.join(", "),
            path_yaml = std::path::Path::new(path).with_extension("yaml").display()
        )))
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
    {
        let mut f = std::fs::File::create(&tmp_path)
            .map_err(|e| ConfigError::Io(format!("create {}: {}", tmp_path.display(), e)))?;
        f.write_all(bytes)
            .map_err(|e| ConfigError::Io(format!("write {}: {}", tmp_path.display(), e)))?;
        f.sync_all()
            .map_err(|e| ConfigError::Io(format!("fsync {}: {}", tmp_path.display(), e)))?;
    }

    // Match the permission posture of fs::write for non-sensitive config
    // files (0644 on Unix). For hash-bearing files, callers already use the
    // dedicated `write_bootstrap_hash_file` helper that sets 0600 separately.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o644));
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
}

/// Write the bootstrap hash file with restrictive permissions (0600).
/// This file doubles as the SQLCipher encryption key, so it must not be
/// world-readable.
pub fn write_bootstrap_hash_file(path: &std::path::Path, hash: &str) -> std::io::Result<()> {
    std::fs::write(path, hash)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert_eq!(config.listen_addr.port(), 9000);
        assert!(matches!(config.backend, BackendConfig::Filesystem { .. }));
    }

    #[test]
    fn test_config_parse_filesystem() {
        let toml = r#"
            listen_addr = "0.0.0.0:8080"
            max_delta_ratio = 0.3

            [backend]
            type = "filesystem"
            path = "/var/lib/deltaglider_proxy"
        "#;

        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(config.listen_addr.port(), 8080);
        assert_eq!(config.max_delta_ratio, 0.3);

        match config.backend {
            BackendConfig::Filesystem { path } => {
                assert_eq!(path, PathBuf::from("/var/lib/deltaglider_proxy"));
            }
            _ => panic!("Expected filesystem backend"),
        }
    }

    #[test]
    fn test_config_parse_s3() {
        let toml = r#"
            listen_addr = "0.0.0.0:8080"

            [backend]
            type = "s3"
            endpoint = "http://localhost:9000"
            region = "us-east-1"
            force_path_style = true
        "#;

        let config: Config = toml::from_str(toml).unwrap();

        match config.backend {
            BackendConfig::S3 {
                endpoint,
                region,
                force_path_style,
                ..
            } => {
                assert_eq!(endpoint, Some("http://localhost:9000".to_string()));
                assert_eq!(region, "us-east-1");
                assert!(force_path_style);
            }
            _ => panic!("Expected S3 backend"),
        }
    }

    /// Ensure every env var read in `from_env()` is present in the registry.
    #[test]
    fn test_registry_completeness() {
        // All env var names referenced in from_env() — extracted manually and
        // kept in sync by this test.
        let used_in_from_env: &[&str] = &[
            "DGP_LISTEN_ADDR",
            "DGP_S3_ENDPOINT",
            "DGP_S3_REGION",
            "DGP_S3_PATH_STYLE",
            "DGP_BE_AWS_ACCESS_KEY_ID",
            "DGP_BE_AWS_SECRET_ACCESS_KEY",
            "DGP_DATA_DIR",
            "DGP_MAX_DELTA_RATIO",
            "DGP_MAX_OBJECT_SIZE",
            "DGP_CACHE_MB",
            "DGP_METADATA_CACHE_MB",
            "DGP_CODEC_CONCURRENCY",
            "DGP_BLOCKING_THREADS",
            "DGP_AUTHENTICATION",
            "DGP_ACCESS_KEY_ID",
            "DGP_SECRET_ACCESS_KEY",
            "DGP_BOOTSTRAP_PASSWORD_HASH",
            "DGP_LOG_LEVEL",
            "DGP_TLS_ENABLED",
            "DGP_TLS_CERT",
            "DGP_TLS_KEY",
        ];

        let registry_names: Vec<&str> = super::ENV_VAR_REGISTRY.iter().map(|e| e.name).collect();

        // Every var used in from_env must be in the registry
        for var in used_in_from_env {
            assert!(
                registry_names.contains(var),
                "Env var {var} is used in from_env() but missing from ENV_VAR_REGISTRY"
            );
        }

        // Every registry entry must be referenced somewhere in the codebase.
        // Vars not in from_env() are read at other call sites (startup, session, etc.).
        let used_outside_from_env: &[&str] = &[
            "DGP_CONFIG",                  // config::load()
            "DGP_CONFIG_SYNC_BUCKET",      // startup::init_config_sync()
            "DGP_DEBUG_HEADERS",           // api::handlers::debug_headers_enabled()
            "DGP_TRUST_PROXY_HEADERS",     // rate_limiter::trust_proxy_headers()
            "DGP_SESSION_TTL_HOURS",       // session::default_session_ttl()
            "DGP_MAX_MULTIPART_UPLOADS",   // multipart::default_max_uploads()
            "DGP_CLOCK_SKEW_SECONDS",      // api::auth + startup replay cache
            "DGP_MAX_CONCURRENT_REQUESTS", // startup::build_s3_router()
            "DGP_CORS_PERMISSIVE",         // demo::ui_router()
            "DGP_REQUEST_TIMEOUT_SECS",    // startup::build_s3_router()
            "DGP_CODEC_TIMEOUT_SECS",      // deltaglider::codec::codec_timeout()
            "DGP_RATE_LIMIT_MAX_ATTEMPTS", // rate_limiter::default_auth()
            "DGP_RATE_LIMIT_WINDOW_SECS",  // rate_limiter::default_auth()
            "DGP_RATE_LIMIT_LOCKOUT_SECS", // rate_limiter::default_auth()
            "DGP_REPLAY_WINDOW_SECS",      // api::auth replay detection
            "DGP_SECURE_COOKIES",          // api::admin::auth::secure_cookies()
        ];
        for name in &registry_names {
            if used_outside_from_env.contains(name) {
                continue;
            }
            assert!(
                used_in_from_env.contains(name),
                "Env var {name} is in ENV_VAR_REGISTRY but not used in from_env() or listed in used_outside_from_env"
            );
        }
    }

    #[test]
    fn test_print_env_vars_output() {
        // Capture stdout by running the function in a string buffer
        // We just verify it doesn't panic and covers all registry entries
        let mut output = String::new();
        let mut current_category = "";
        for entry in super::ENV_VAR_REGISTRY {
            if entry.category != current_category {
                if !current_category.is_empty() {
                    output.push('\n');
                }
                use std::fmt::Write;
                let _ = writeln!(output, "# {}", entry.category);
                current_category = entry.category;
            }
            use std::fmt::Write;
            let _ = writeln!(output, "# {}", entry.description);
            let _ = writeln!(output, "{}={}", entry.name, entry.example);
        }

        // Spot-check some entries
        assert!(output.contains("DGP_LISTEN_ADDR=0.0.0.0:9000"));
        assert!(output.contains("DGP_CACHE_MB=100"));
        assert!(output.contains("# Server"));
        assert!(output.contains("# TLS"));
    }

    #[test]
    fn test_authentication_field_deserializes() {
        let toml = r#"
            listen_addr = "127.0.0.1:9000"
            authentication = "none"

            [backend]
            type = "filesystem"
            path = "/tmp/test"
        "#;
        let config: Config = toml::from_str(toml).unwrap();
        assert_eq!(
            config.authentication.as_deref(),
            Some("none"),
            "authentication field must be deserialized from TOML"
        );
    }

    #[test]
    fn test_authentication_field_absent_is_none() {
        let toml = r#"
            listen_addr = "127.0.0.1:9000"

            [backend]
            type = "filesystem"
            path = "/tmp/test"
        "#;
        let config: Config = toml::from_str(toml).unwrap();
        assert!(
            config.authentication.is_none(),
            "absent authentication field must be None"
        );
    }

    #[test]
    fn test_print_example_toml_is_valid() {
        // The base TOML from Config::default() must round-trip
        let default_cfg = Config::default();
        let toml_str = toml::to_string_pretty(&default_cfg).unwrap();
        let parsed: Config = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.listen_addr, default_cfg.listen_addr);
        assert_eq!(parsed.cache_size_mb, default_cfg.cache_size_mb);
        assert_eq!(parsed.max_delta_ratio, default_cfg.max_delta_ratio);
    }

    // ── YAML parity tests (Phase 0) ──────────────────────────────────────

    #[test]
    fn test_config_format_from_path() {
        assert_eq!(ConfigFormat::from_path("foo.yaml"), ConfigFormat::Yaml);
        assert_eq!(ConfigFormat::from_path("foo.YAML"), ConfigFormat::Yaml);
        assert_eq!(ConfigFormat::from_path("foo.yml"), ConfigFormat::Yaml);
        assert_eq!(ConfigFormat::from_path("foo.toml"), ConfigFormat::Toml);
        assert_eq!(ConfigFormat::from_path("foo"), ConfigFormat::Toml);
        assert_eq!(ConfigFormat::from_path("/etc/dgp.txt"), ConfigFormat::Toml);
    }

    #[test]
    fn test_yaml_parse_filesystem() {
        let yaml = r#"
listen_addr: "0.0.0.0:8080"
max_delta_ratio: 0.3
backend:
  type: filesystem
  path: /var/lib/deltaglider_proxy
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.listen_addr.port(), 8080);
        assert_eq!(config.max_delta_ratio, 0.3);
        match config.backend {
            BackendConfig::Filesystem { path } => {
                assert_eq!(path, PathBuf::from("/var/lib/deltaglider_proxy"));
            }
            _ => panic!("Expected filesystem backend"),
        }
    }

    #[test]
    fn test_yaml_parse_s3() {
        let yaml = r#"
listen_addr: "0.0.0.0:8080"
backend:
  type: s3
  endpoint: http://localhost:9000
  region: us-east-1
  force_path_style: true
"#;
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        match config.backend {
            BackendConfig::S3 {
                endpoint,
                region,
                force_path_style,
                ..
            } => {
                assert_eq!(endpoint, Some("http://localhost:9000".to_string()));
                assert_eq!(region, "us-east-1");
                assert!(force_path_style);
            }
            _ => panic!("Expected S3 backend"),
        }
    }

    #[test]
    fn test_yaml_round_trip_default() {
        let default_cfg = Config::default();
        let yaml_str = default_cfg.to_canonical_yaml().unwrap();
        let parsed: Config = serde_yaml::from_str(&yaml_str).unwrap();
        assert_eq!(parsed.listen_addr, default_cfg.listen_addr);
        assert_eq!(parsed.cache_size_mb, default_cfg.cache_size_mb);
        assert_eq!(parsed.max_delta_ratio, default_cfg.max_delta_ratio);
        assert_eq!(parsed.defaults_version, default_cfg.defaults_version);
    }

    #[test]
    fn test_yaml_toml_parity_filesystem() {
        // Same semantic content in both formats → same in-memory shape.
        let toml = r#"
listen_addr = "127.0.0.1:9500"
max_delta_ratio = 0.25
cache_size_mb = 128
metadata_cache_mb = 64

[backend]
type = "filesystem"
path = "/srv/dgp"
"#;
        let yaml = r#"
listen_addr: "127.0.0.1:9500"
max_delta_ratio: 0.25
cache_size_mb: 128
metadata_cache_mb: 64
backend:
  type: filesystem
  path: /srv/dgp
"#;
        let toml_cfg: Config = toml::from_str(toml).unwrap();
        let yaml_cfg: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(toml_cfg.listen_addr, yaml_cfg.listen_addr);
        assert_eq!(toml_cfg.max_delta_ratio, yaml_cfg.max_delta_ratio);
        assert_eq!(toml_cfg.cache_size_mb, yaml_cfg.cache_size_mb);
        assert_eq!(toml_cfg.metadata_cache_mb, yaml_cfg.metadata_cache_mb);
        match (toml_cfg.backend, yaml_cfg.backend) {
            (BackendConfig::Filesystem { path: a }, BackendConfig::Filesystem { path: b }) => {
                assert_eq!(a, b)
            }
            _ => panic!("Both backends should be filesystem"),
        }
    }

    #[test]
    fn test_from_file_dispatches_by_extension() {
        let dir = tempfile::tempdir().unwrap();
        let toml_path = dir.path().join("a.toml");
        std::fs::write(&toml_path, "listen_addr = \"127.0.0.1:9100\"\n").unwrap();
        let cfg = Config::from_file(toml_path.to_str().unwrap()).unwrap();
        assert_eq!(cfg.listen_addr.port(), 9100);

        let yaml_path = dir.path().join("b.yaml");
        std::fs::write(&yaml_path, "listen_addr: \"127.0.0.1:9200\"\n").unwrap();
        let cfg = Config::from_file(yaml_path.to_str().unwrap()).unwrap();
        assert_eq!(cfg.listen_addr.port(), 9200);

        // .yml also dispatches to YAML
        let yml_path = dir.path().join("c.yml");
        std::fs::write(&yml_path, "listen_addr: \"127.0.0.1:9300\"\n").unwrap();
        let cfg = Config::from_file(yml_path.to_str().unwrap()).unwrap();
        assert_eq!(cfg.listen_addr.port(), 9300);
    }

    #[test]
    fn test_defaults_version_absent_means_v1() {
        let yaml = "listen_addr: \"127.0.0.1:9000\"\n";
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.defaults_version, DefaultsVersion::V1);
    }

    #[test]
    fn test_defaults_version_explicit_v1() {
        let yaml = "defaults: v1\nlisten_addr: \"127.0.0.1:9000\"\n";
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.defaults_version, DefaultsVersion::V1);
    }

    #[test]
    fn test_canonical_yaml_omits_default_defaults_version() {
        // When defaults_version equals its default, it should not appear in the
        // exported canonical YAML (keeps the file minimal).
        let cfg = Config::default();
        let yaml = cfg.to_canonical_yaml().unwrap();
        assert!(
            !yaml.contains("defaults:"),
            "canonical YAML must omit the defaults field when it equals V1"
        );
    }

    #[test]
    fn test_canonical_yaml_strips_infra_secrets() {
        // to_canonical_yaml matches to_toml_string: infra secrets only.
        // Full redaction (incl. SigV4 creds) goes through redact_all_secrets.
        let cfg = Config {
            access_key_id: Some("AKIAKEEPME".into()),
            secret_access_key: Some("kept-for-file-persistence".into()),
            bootstrap_password_hash: Some("$2b$12$xxxxxxxxxxxxxxxxxxxxxx".into()),
            backend_encryption: BackendEncryptionConfig::Aes256GcmProxy {
                key: Some("deadbeef-hex-encryption-key".into()),
                key_id: Some("singleton-kid".into()),
                legacy_key: None,
                legacy_key_id: None,
            },
            ..Config::default()
        };

        let yaml = cfg.to_canonical_yaml().unwrap();
        // Infra secrets are stripped
        assert!(!yaml.contains("$2b$"));
        assert!(
            !yaml.contains("deadbeef-hex-encryption-key"),
            "singleton encryption key must be redacted from canonical YAML, got:\n{yaml}"
        );
        // Non-secret id survives so operators can still see encryption is on.
        assert!(
            yaml.contains("singleton-kid"),
            "key_id is not a secret; must survive redaction"
        );
        // SigV4 creds survive — the wizard/file deployment path depends on this
        assert!(yaml.contains("AKIAKEEPME"));
        assert!(yaml.contains("kept-for-file-persistence"));
    }

    #[test]
    fn test_redact_all_secrets_full_paranoia() {
        let mut cfg = Config {
            access_key_id: Some("AKIASHOULDNOTAPPEAR".into()),
            secret_access_key: Some("secret-should-not-appear".into()),
            bootstrap_password_hash: Some("$2b$12$xxxxxxxxxxxxxxxxxxxxxx".into()),
            backend_encryption: BackendEncryptionConfig::Aes256GcmProxy {
                key: Some("deadbeef-hex-encryption-key".into()),
                key_id: None,
                legacy_key: Some("legacy-deadbeef-should-also-redact".into()),
                legacy_key_id: Some("legacy-kid".into()),
            },
            backend: BackendConfig::S3 {
                endpoint: Some("http://minio:9000".into()),
                region: "us-east-1".into(),
                force_path_style: true,
                access_key_id: Some("BACKEND-SECRET-ID".into()),
                secret_access_key: Some("BACKEND-SECRET-KEY".into()),
            },
            ..Config::default()
        };
        cfg.backends.push(NamedBackendConfig {
            name: "hetzner".into(),
            backend: BackendConfig::S3 {
                endpoint: Some("https://fsn1.your-objectstorage.com".into()),
                region: "eu-central-1".into(),
                force_path_style: true,
                access_key_id: Some("NAMED-SECRET-ID".into()),
                secret_access_key: Some("NAMED-SECRET-KEY".into()),
            },
            encryption: BackendEncryptionConfig::Aes256GcmProxy {
                key: Some("NAMED-ENCRYPTION-KEY-SHOULD-REDACT".into()),
                key_id: Some("hetzner-kid".into()),
                legacy_key: None,
                legacy_key_id: None,
            },
        });

        let redacted = cfg.redact_all_secrets();
        let yaml = serde_yaml::to_string(&redacted).unwrap();
        // Top-level proxy creds
        assert!(!yaml.contains("AKIASHOULDNOTAPPEAR"));
        assert!(!yaml.contains("secret-should-not-appear"));
        // Bootstrap + encryption (primary + legacy on singleton, primary on named)
        assert!(!yaml.contains("$2b$"));
        assert!(!yaml.contains("deadbeef-hex-encryption-key"));
        assert!(
            !yaml.contains("legacy-deadbeef-should-also-redact"),
            "legacy_key (the decrypt-only-shim slot) must also redact"
        );
        assert!(
            !yaml.contains("NAMED-ENCRYPTION-KEY-SHOULD-REDACT"),
            "per-named-backend encryption keys must redact"
        );
        // Primary backend creds
        assert!(!yaml.contains("BACKEND-SECRET-ID"));
        assert!(!yaml.contains("BACKEND-SECRET-KEY"));
        // Named backend creds
        assert!(!yaml.contains("NAMED-SECRET-ID"));
        assert!(!yaml.contains("NAMED-SECRET-KEY"));
        // Non-secret fields survive: backend names, regions, non-secret key_ids.
        assert!(yaml.contains("hetzner"));
        assert!(yaml.contains("eu-central-1"));
        assert!(
            yaml.contains("hetzner-kid"),
            "key_id (not a secret) must survive — operators need to see which backend \
             is encrypted under which id"
        );
        assert!(
            yaml.contains("legacy-kid"),
            "legacy_key_id (not a secret, just an id) must survive"
        );
    }

    // ──────────────────────────────────────────────────────────────
    // Per-backend encryption — new config shape
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn test_backend_encryption_yaml_roundtrip_none() {
        // Default variant: should not serialize at all (skip_serializing_if).
        let cfg = BackendEncryptionConfig::default();
        assert!(is_default_encryption(&cfg));
    }

    #[test]
    fn test_backend_encryption_yaml_roundtrip_aes() {
        let yaml = r#"
mode: aes256-gcm-proxy
key: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
key_id: eu-2026-04
"#;
        let parsed: BackendEncryptionConfig = serde_yaml::from_str(yaml).unwrap();
        match &parsed {
            BackendEncryptionConfig::Aes256GcmProxy { key, key_id, .. } => {
                assert!(key.as_deref().unwrap().starts_with("0123"));
                assert_eq!(key_id.as_deref(), Some("eu-2026-04"));
            }
            other => panic!("unexpected variant: {other:?}"),
        }
        // Round-trip
        let emitted = serde_yaml::to_string(&parsed).unwrap();
        assert!(emitted.contains("mode: aes256-gcm-proxy"));
        assert!(emitted.contains("key_id: eu-2026-04"));
    }

    #[test]
    fn test_backend_encryption_yaml_roundtrip_sse_kms() {
        let yaml = r#"
mode: sse-kms
kms_key_id: arn:aws:kms:us-east-1:123456789012:key/abcd
bucket_key_enabled: false
"#;
        let parsed: BackendEncryptionConfig = serde_yaml::from_str(yaml).unwrap();
        match &parsed {
            BackendEncryptionConfig::SseKms {
                kms_key_id,
                bucket_key_enabled,
                ..
            } => {
                assert!(kms_key_id.contains("key/abcd"));
                assert!(!bucket_key_enabled);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn test_backend_encryption_yaml_default_bucket_key_enabled_true() {
        let yaml = r#"
mode: sse-kms
kms_key_id: arn:aws:kms:us-east-1:1:key/x
"#;
        let parsed: BackendEncryptionConfig = serde_yaml::from_str(yaml).unwrap();
        match parsed {
            BackendEncryptionConfig::SseKms {
                bucket_key_enabled, ..
            } => assert!(bucket_key_enabled),
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn test_named_backend_with_encryption_roundtrips_through_config() {
        let yaml = r#"
backends:
  - name: eu
    type: filesystem
    path: /tmp/eu
    encryption:
      mode: aes256-gcm-proxy
      key: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
      key_id: eu-kid
  - name: us
    type: filesystem
    path: /tmp/us
"#;
        let cfg: Config = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.backends.len(), 2);
        assert!(matches!(
            cfg.backends[0].encryption,
            BackendEncryptionConfig::Aes256GcmProxy { .. }
        ));
        assert!(matches!(
            cfg.backends[1].encryption,
            BackendEncryptionConfig::None { .. }
        ));
    }

    #[test]
    fn test_global_encryption_key_field_no_longer_accepted() {
        // Regression: the old `encryption_key:` at the config root must
        // be rejected. No legacy — nobody shipped with it — so a YAML
        // still carrying it is an outdated doc that should fail loudly.
        let yaml = r#"
encryption_key: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
"#;
        // Flat-root Config with serde's default (no deny_unknown_fields):
        // silently ignored. That's fine — the flat shape is legacy-reading
        // surface, and after this refactor it just drops the field.
        // The point is the field is NOT present on the parsed struct.
        let cfg: Config = serde_yaml::from_str(yaml).unwrap_or_default();
        // No top-level encryption_key field exists anymore; the only
        // encryption surfaces are `backend_encryption` (singleton) and
        // `backends[*].encryption` (list).
        assert!(matches!(
            cfg.backend_encryption,
            BackendEncryptionConfig::None { .. }
        ));
    }

    #[test]
    fn test_is_valid_key_id_charset() {
        assert!(is_valid_key_id("eu-2026-04"));
        assert!(is_valid_key_id("a"));
        assert!(is_valid_key_id("a.b_c-d"));
        assert!(is_valid_key_id(&"x".repeat(64)));
        // Empty
        assert!(!is_valid_key_id(""));
        // Too long
        assert!(!is_valid_key_id(&"x".repeat(65)));
        // Forbidden chars
        assert!(!is_valid_key_id("has space"));
        assert!(!is_valid_key_id("has/slash"));
        assert!(!is_valid_key_id("has:colon"));
        assert!(!is_valid_key_id("héllo")); // non-ASCII
    }

    #[test]
    fn test_env_suffix_normalises_name() {
        assert_eq!(env_suffix_for_backend_name("eu-archive"), "EU_ARCHIVE");
        assert_eq!(env_suffix_for_backend_name("default"), "DEFAULT");
        assert_eq!(env_suffix_for_backend_name("a.b-c"), "A_B_C");
    }

    #[test]
    fn test_check_rejects_sse_kms_on_filesystem() {
        let mut cfg = Config {
            backends: vec![NamedBackendConfig {
                name: "local".into(),
                backend: BackendConfig::Filesystem {
                    path: "/tmp/x".into(),
                },
                encryption: BackendEncryptionConfig::SseKms {
                    kms_key_id: "arn:aws:kms:...".into(),
                    bucket_key_enabled: true,
                    legacy_key: None,
                    legacy_key_id: None,
                },
            }],
            ..Config::default()
        };
        let warnings = cfg.check();
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("local") && w.contains("sse-kms")),
            "sse-kms on filesystem must warn, got {:?}",
            warnings
        );
    }

    #[test]
    fn test_check_warns_aes_without_key() {
        // Make sure the env-var fallback doesn't accidentally satisfy
        // the check (our tests may run with DGP_ENCRYPTION_KEY set from
        // prior tests — scope the fixture to a unique name).
        let mut cfg = Config {
            backends: vec![NamedBackendConfig {
                name: "unconfigured-xyz-42".into(),
                backend: BackendConfig::Filesystem {
                    path: "/tmp/x".into(),
                },
                encryption: BackendEncryptionConfig::Aes256GcmProxy {
                    key: None,
                    key_id: None,
                    legacy_key: None,
                    legacy_key_id: None,
                },
            }],
            ..Config::default()
        };
        let warnings = cfg.check();
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("unconfigured-xyz-42") && w.contains("DGP_BACKEND_")),
            "aes mode with no key must produce env-var hint, got {:?}",
            warnings
        );
    }

    #[test]
    fn test_check_detects_key_id_collision_with_different_keys() {
        let mut cfg = Config {
            backends: vec![
                NamedBackendConfig {
                    name: "a".into(),
                    backend: BackendConfig::Filesystem {
                        path: "/tmp/a".into(),
                    },
                    encryption: BackendEncryptionConfig::Aes256GcmProxy {
                        key: Some("K1".into()),
                        key_id: Some("shared".into()),
                        legacy_key: None,
                        legacy_key_id: None,
                    },
                },
                NamedBackendConfig {
                    name: "b".into(),
                    backend: BackendConfig::Filesystem {
                        path: "/tmp/b".into(),
                    },
                    encryption: BackendEncryptionConfig::Aes256GcmProxy {
                        key: Some("K2".into()),
                        key_id: Some("shared".into()),
                        legacy_key: None,
                        legacy_key_id: None,
                    },
                },
            ],
            ..Config::default()
        };
        let warnings = cfg.check();
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("shared") && w.contains("DIFFERENT")),
            "shared key_id + different keys must warn, got {:?}",
            warnings
        );
    }

    #[test]
    fn test_check_allows_shared_key_id_with_same_key() {
        // Same kid + same key = intentional cross-backend portability.
        // This is the documented escape hatch; MUST NOT warn.
        let mut cfg = Config {
            backends: vec![
                NamedBackendConfig {
                    name: "primary".into(),
                    backend: BackendConfig::Filesystem {
                        path: "/tmp/a".into(),
                    },
                    encryption: BackendEncryptionConfig::Aes256GcmProxy {
                        key: Some("SAME".into()),
                        key_id: Some("portable".into()),
                        legacy_key: None,
                        legacy_key_id: None,
                    },
                },
                NamedBackendConfig {
                    name: "replica".into(),
                    backend: BackendConfig::Filesystem {
                        path: "/tmp/b".into(),
                    },
                    encryption: BackendEncryptionConfig::Aes256GcmProxy {
                        key: Some("SAME".into()),
                        key_id: Some("portable".into()),
                        legacy_key: None,
                        legacy_key_id: None,
                    },
                },
            ],
            ..Config::default()
        };
        let warnings = cfg.check();
        assert!(
            !warnings
                .iter()
                .any(|w| w.contains("portable") && w.contains("DIFFERENT")),
            "identical key_id + identical key must NOT warn, got {:?}",
            warnings
        );
    }

    #[test]
    fn test_check_rejects_invalid_key_id_charset() {
        let mut cfg = Config {
            backends: vec![NamedBackendConfig {
                name: "bad".into(),
                backend: BackendConfig::Filesystem {
                    path: "/tmp/x".into(),
                },
                encryption: BackendEncryptionConfig::Aes256GcmProxy {
                    key: Some("K".into()),
                    key_id: Some("has space!".into()),
                    legacy_key: None,
                    legacy_key_id: None,
                },
            }],
            ..Config::default()
        };
        let warnings = cfg.check();
        assert!(
            warnings.iter().any(|w| w.contains("has space!")),
            "invalid key_id charset must warn, got {:?}",
            warnings
        );
    }

    #[test]
    fn test_persist_to_file_dispatches_by_extension() {
        let dir = tempfile::tempdir().unwrap();
        // Deliberately non-default listen_addr so the sectioned canonical
        // YAML exporter surfaces an `advanced:` block — a default Config
        // round-trips to an (intentionally) empty YAML document, which
        // would make this dispatcher test vacuous.
        let cfg = Config {
            listen_addr: "127.0.0.1:9099".parse().unwrap(),
            ..Config::default()
        };

        let yaml_path = dir.path().join("out.yaml");
        cfg.persist_to_file(yaml_path.to_str().unwrap()).unwrap();
        let content = std::fs::read_to_string(&yaml_path).unwrap();
        assert!(
            content.contains("listen_addr:"),
            "YAML output must use : separator, got: {content}"
        );
        assert!(
            content.contains("advanced:"),
            "sectioned YAML must group listen_addr under `advanced:`, got: {content}"
        );

        let toml_path = dir.path().join("out.toml");
        cfg.persist_to_file(toml_path.to_str().unwrap()).unwrap();
        let content = std::fs::read_to_string(&toml_path).unwrap();
        assert!(
            content.contains("listen_addr ="),
            "TOML output must use = separator, got: {content}"
        );
    }

    #[test]
    fn test_persist_to_toml_rejects_admission_blocks() {
        // H4 from deep correctness review: refuse to persist sectioned-
        // only fields (admission blocks / iam_mode: declarative) to a
        // TOML target. Silent half-persist is the worst possible
        // failure mode — the in-memory runtime has the new state but
        // the on-disk file reverts on next restart.
        use crate::admission::spec::{ActionSpec, AdmissionBlockSpec, MatchSpec, SimpleAction};
        let cfg = Config {
            admission_blocks: vec![AdmissionBlockSpec {
                name: "deny-bad".into(),
                match_: MatchSpec::default(),
                action: ActionSpec::Simple(SimpleAction::Deny),
            }],
            ..Config::default()
        };
        let dir = tempfile::tempdir().unwrap();
        let toml_path = dir.path().join("out.toml");
        let err = cfg
            .persist_to_file(toml_path.to_str().unwrap())
            .expect_err("persist to TOML with admission blocks must fail");
        let msg = format!("{err}");
        assert!(
            msg.contains("admission.blocks") && msg.contains("YAML"),
            "error must name the offending field + point at YAML, got: {msg}"
        );
    }

    #[test]
    fn test_persist_to_toml_rejects_declarative_iam_mode() {
        let cfg = Config {
            iam_mode: crate::config_sections::IamMode::Declarative,
            ..Config::default()
        };
        let dir = tempfile::tempdir().unwrap();
        let toml_path = dir.path().join("out.toml");
        let err = cfg
            .persist_to_file(toml_path.to_str().unwrap())
            .expect_err("persist to TOML with iam_mode: declarative must fail");
        let msg = format!("{err}");
        assert!(
            msg.contains("access.iam_mode") && msg.contains("YAML"),
            "error must name the offending field + point at YAML, got: {msg}"
        );
    }

    #[test]
    fn test_persist_to_yaml_accepts_admission_blocks() {
        // Symmetric: the same config persists fine to a YAML target.
        use crate::admission::spec::{ActionSpec, AdmissionBlockSpec, MatchSpec, SimpleAction};
        let cfg = Config {
            admission_blocks: vec![AdmissionBlockSpec {
                name: "deny-bad".into(),
                match_: MatchSpec::default(),
                action: ActionSpec::Simple(SimpleAction::Deny),
            }],
            iam_mode: crate::config_sections::IamMode::Declarative,
            ..Config::default()
        };
        let dir = tempfile::tempdir().unwrap();
        let yaml_path = dir.path().join("out.yaml");
        cfg.persist_to_file(yaml_path.to_str().unwrap()).unwrap();
        let content = std::fs::read_to_string(&yaml_path).unwrap();
        assert!(content.contains("deny-bad"));
        assert!(content.contains("iam_mode: declarative"));
    }

    #[test]
    fn test_example_toml_migrates_to_valid_yaml() {
        // The canonical example file must round-trip through migrate.
        let example_path = "deltaglider_proxy.toml.example";
        if !std::path::Path::new(example_path).exists() {
            // Test is best-effort when run outside the repo root; skip silently.
            return;
        }
        let toml_cfg = Config::from_file(example_path).unwrap();
        let yaml = toml_cfg.to_canonical_yaml().unwrap();
        // Round-trip goes through the dual-shape deserializer: the canonical
        // exporter emits sectioned YAML, and only `from_yaml_str` knows how
        // to collapse it back into the flat in-memory Config.
        let yaml_cfg = Config::from_yaml_str(&yaml).unwrap();
        assert_eq!(toml_cfg.listen_addr, yaml_cfg.listen_addr);
        assert_eq!(toml_cfg.max_delta_ratio, yaml_cfg.max_delta_ratio);
        assert_eq!(toml_cfg.cache_size_mb, yaml_cfg.cache_size_mb);
    }

    // ── Correctness regressions (post Phase-1 audit) ────────────────────

    #[test]
    fn test_atomic_write_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("cfg.yaml");
        atomic_write(&target, b"hello: world\n").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello: world\n");
    }

    #[test]
    fn test_atomic_write_overwrites_existing() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("cfg.yaml");
        std::fs::write(&target, b"old: value\n").unwrap();
        atomic_write(&target, b"new: value\n").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new: value\n");
    }

    #[test]
    fn test_atomic_write_leaves_no_tempfile_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("cfg.yaml");
        atomic_write(&target, b"ok\n").unwrap();
        // The sibling tempfile (named ".cfg.yaml.tmp.<hex>") must not leak.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().into_string().unwrap())
            .filter(|n| n.starts_with(".cfg.yaml.tmp."))
            .collect();
        assert!(
            leftovers.is_empty(),
            "atomic_write leaked tempfiles: {leftovers:?}"
        );
    }

    #[test]
    fn test_atomic_write_fails_when_parent_missing() {
        let dir = tempfile::tempdir().unwrap();
        // Parent directory does not exist — write must fail cleanly with
        // an IO error, not a panic or a silent success.
        let target = dir.path().join("does_not_exist").join("cfg.yaml");
        let err = atomic_write(&target, b"x").unwrap_err();
        assert!(
            matches!(err, ConfigError::Io(_)),
            "expected ConfigError::Io, got {err:?}"
        );
    }

    #[test]
    fn test_check_handles_nan_delta_ratio() {
        let mut cfg = Config {
            max_delta_ratio: f32::NAN,
            ..Config::default()
        };
        let warnings = cfg.check();
        assert!(
            warnings.iter().any(|w| w.contains("NaN")),
            "expected NaN warning, got {warnings:?}"
        );
        assert!(
            !cfg.max_delta_ratio.is_nan(),
            "NaN ratio should have been replaced with a sane default"
        );
        assert!(
            (cfg.max_delta_ratio - default_max_delta_ratio()).abs() < f32::EPSILON,
            "NaN ratio should be replaced with default 0.75, got {}",
            cfg.max_delta_ratio
        );
    }

    #[test]
    fn test_check_flags_out_of_range_ratio() {
        let mut cfg = Config {
            max_delta_ratio: 1.5,
            ..Config::default()
        };
        let warnings = cfg.check();
        assert!(
            warnings.iter().any(|w| w.contains("max_delta_ratio")),
            "expected out-of-range warning, got {warnings:?}"
        );
        // Out-of-range values survive (they're a sanity warning, not a fix).
        assert!((cfg.max_delta_ratio - 1.5).abs() < f32::EPSILON);
    }

    #[test]
    fn test_check_clamps_infinity_delta_ratio() {
        // YAML `.inf` deserializes to f32::INFINITY. INFINITY > 1.0 is true
        // (the old warning fired) but the value would have survived and
        // silently stored every file as a delta regardless of size. Clamp
        // to the default alongside NaN.
        let mut cfg = Config {
            max_delta_ratio: f32::INFINITY,
            ..Config::default()
        };
        let warnings = cfg.check();
        assert!(
            warnings.iter().any(|w| w.contains("infinite")),
            "expected infinity warning, got {warnings:?}"
        );
        assert!(
            !cfg.max_delta_ratio.is_infinite(),
            "infinity should have been replaced, got {}",
            cfg.max_delta_ratio
        );
        assert!(
            (cfg.max_delta_ratio - default_max_delta_ratio()).abs() < f32::EPSILON,
            "infinity should be replaced with default 0.75, got {}",
            cfg.max_delta_ratio
        );
    }

    #[test]
    fn test_check_warns_on_duplicate_backend_names() {
        // Routing keys on backend.name. A duplicate silently shadows the
        // second entry; the first wins at runtime. Warn so the operator
        // knows the config is ambiguous.
        let mut cfg = Config {
            backends: vec![
                NamedBackendConfig {
                    name: "shared".into(),
                    backend: BackendConfig::Filesystem { path: "/a".into() },
                    encryption: BackendEncryptionConfig::default(),
                },
                NamedBackendConfig {
                    name: "unique".into(),
                    backend: BackendConfig::Filesystem { path: "/b".into() },
                    encryption: BackendEncryptionConfig::default(),
                },
                NamedBackendConfig {
                    name: "shared".into(),
                    backend: BackendConfig::Filesystem { path: "/c".into() },
                    encryption: BackendEncryptionConfig::default(),
                },
            ],
            ..Config::default()
        };
        let warnings = cfg.check();
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("duplicate backend name") && w.contains("shared")),
            "expected duplicate-name warning, got {warnings:?}"
        );
    }

    #[test]
    fn test_check_no_warning_when_backend_names_unique() {
        let mut cfg = Config {
            backends: vec![
                NamedBackendConfig {
                    name: "a".into(),
                    backend: BackendConfig::Filesystem { path: "/a".into() },
                    encryption: BackendEncryptionConfig::default(),
                },
                NamedBackendConfig {
                    name: "b".into(),
                    backend: BackendConfig::Filesystem { path: "/b".into() },
                    encryption: BackendEncryptionConfig::default(),
                },
            ],
            ..Config::default()
        };
        let warnings = cfg.check();
        assert!(
            !warnings.iter().any(|w| w.contains("duplicate")),
            "no duplicate warning expected when names are unique, got {warnings:?}"
        );
    }

    #[test]
    fn test_resolve_config_path_honors_env_even_when_missing() {
        // DGP_CONFIG pointing at a non-existent file must STILL be returned
        // — the operator's explicit intent beats silent fallthrough that
        // would redirect admin-API persists to an unrelated file.
        let guard = EnvGuard::set("DGP_CONFIG", "/tmp/definitely-does-not-exist.yaml");
        let resolved = Config::resolve_config_path();
        assert_eq!(resolved, Some("/tmp/definitely-does-not-exist.yaml".into()));
        drop(guard);
    }

    #[test]
    fn test_resolve_config_path_empty_env_falls_through() {
        // An empty-string env var must not hijack resolution.
        let guard = EnvGuard::set("DGP_CONFIG", "");
        let _ = Config::resolve_config_path(); // may be None or search-path hit; either is fine
        drop(guard);
    }

    /// Test-only RAII guard that sets an env var on construction and
    /// unsets it on drop. Prevents one test from polluting another when
    /// they exercise environment-driven behavior.
    struct EnvGuard {
        key: &'static str,
        prior: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let prior = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self { key, prior }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.prior.take() {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[test]
    fn test_buckets_field_is_ordered() {
        // BTreeMap iteration must yield keys in sorted order. This is the
        // stability guarantee that makes canonical YAML export byte-stable.
        let mut cfg = Config::default();
        cfg.buckets.insert(
            "zeta".into(),
            crate::bucket_policy::BucketPolicyConfig::default(),
        );
        cfg.buckets.insert(
            "alpha".into(),
            crate::bucket_policy::BucketPolicyConfig::default(),
        );
        cfg.buckets.insert(
            "mu".into(),
            crate::bucket_policy::BucketPolicyConfig::default(),
        );
        let yaml = cfg.to_canonical_yaml().unwrap();
        // Extract the order in which bucket keys appear — must be sorted.
        let alpha = yaml.find("alpha:").unwrap();
        let mu = yaml.find("mu:").unwrap();
        let zeta = yaml.find("zeta:").unwrap();
        assert!(
            alpha < mu && mu < zeta,
            "bucket keys must appear in sorted order; got YAML:\n{yaml}"
        );
    }

    // ── Phase 3a: dual-shape deserialize ────────────────────────────────

    #[test]
    fn test_from_yaml_str_accepts_flat_shape() {
        // Legacy shape: keys at the document root. Still works.
        let yaml = r#"
listen_addr: "127.0.0.1:9123"
max_delta_ratio: 0.3
cache_size_mb: 256
"#;
        let cfg = Config::from_yaml_str(yaml).unwrap();
        assert_eq!(cfg.listen_addr.port(), 9123);
        assert!((cfg.max_delta_ratio - 0.3).abs() < f32::EPSILON);
        assert_eq!(cfg.cache_size_mb, 256);
    }

    #[test]
    fn test_from_yaml_str_accepts_sectioned_shape() {
        // Phase 3 canonical shape: four top-level sections.
        let yaml = r#"
advanced:
  listen_addr: "127.0.0.1:9124"
  max_delta_ratio: 0.2
  cache_size_mb: 512
access:
  access_key_id: "AKIA"
  secret_access_key: "s3cret"
"#;
        let cfg = Config::from_yaml_str(yaml).unwrap();
        assert_eq!(cfg.listen_addr.port(), 9124);
        assert!((cfg.max_delta_ratio - 0.2).abs() < f32::EPSILON);
        assert_eq!(cfg.cache_size_mb, 512);
        assert_eq!(cfg.access_key_id.as_deref(), Some("AKIA"));
        assert_eq!(cfg.secret_access_key.as_deref(), Some("s3cret"));
    }

    #[test]
    fn test_from_yaml_str_empty_document_yields_default() {
        let cfg = Config::from_yaml_str("").unwrap();
        assert_eq!(cfg, Config::default());
        let cfg2 = Config::from_yaml_str("   \n\t\n").unwrap();
        assert_eq!(cfg2, Config::default());
    }

    #[test]
    fn test_from_yaml_str_bare_defaults_key_is_flat_compatible() {
        // `defaults: v1` is valid at the root of BOTH shapes — looks_sectioned
        // returns false (no section keys, no flat-only keys), and the flat
        // deserializer handles it.
        let yaml = "defaults: v1\n";
        let cfg = Config::from_yaml_str(yaml).unwrap();
        assert_eq!(cfg.defaults_version, DefaultsVersion::V1);
    }

    #[test]
    fn test_from_yaml_str_sectioned_roundtrips_canonical_output() {
        // The canonical exporter emits sectioned YAML. That YAML, fed back
        // through `from_yaml_str`, must reconstruct the same Config. This is
        // the GitOps invariant: export → apply is a no-op.
        let original = Config {
            listen_addr: "10.0.0.1:9000".parse().unwrap(),
            max_delta_ratio: 0.15,
            cache_size_mb: 333,
            access_key_id: Some("AKIAROUND".into()),
            secret_access_key: Some("roundtrip".into()),
            ..Config::default()
        };
        let yaml = original.to_canonical_yaml().unwrap();
        // Must be sectioned.
        assert!(
            yaml.contains("advanced:") || yaml.contains("access:"),
            "canonical YAML must be sectioned, got:\n{yaml}"
        );
        let roundtripped = Config::from_yaml_str(&yaml).unwrap();
        assert_eq!(original.listen_addr, roundtripped.listen_addr);
        assert_eq!(original.max_delta_ratio, roundtripped.max_delta_ratio);
        assert_eq!(original.cache_size_mb, roundtripped.cache_size_mb);
        assert_eq!(original.access_key_id, roundtripped.access_key_id);
        assert_eq!(original.secret_access_key, roundtripped.secret_access_key);
    }

    #[test]
    fn test_from_yaml_str_mixed_shape_is_hard_error() {
        // A doc with BOTH a flat key (`listen_addr:`) AND a section key
        // (`storage:`) must be rejected — picking either shape would drop
        // half of what the operator wrote.
        let yaml = r#"
listen_addr: "127.0.0.1:9125"
storage:
  default_backend: "hetzner"
"#;
        let err = Config::from_yaml_str(yaml)
            .expect_err("mixed flat+sectioned must be rejected, not silently merged");
        let msg = format!("{err}");
        assert!(
            msg.contains("listen_addr") && msg.contains("storage"),
            "error must name BOTH the flat and the section key that collided, got: {msg}"
        );
    }

    #[test]
    fn test_from_yaml_str_typo_inside_section_is_hard_error() {
        // Typo inside a section — `default_backnd` instead of
        // `default_backend` — must be rejected loudly, not silently
        // defaulted. This is the Phase 3a promise that motivates
        // `#[serde(deny_unknown_fields)]` on every section type.
        let yaml = r#"
storage:
  default_backnd: "hetzner"
"#;
        let err = Config::from_yaml_str(yaml)
            .expect_err("unknown field inside `storage:` must be rejected");
        let msg = format!("{err}");
        assert!(
            msg.contains("default_backnd"),
            "error must name the offending field, got: {msg}"
        );
    }

    #[test]
    fn test_from_yaml_str_unknown_section_is_hard_error() {
        // Typo at the root: `storge:` instead of `storage:`. Because the
        // doc lacks any known section OR flat key, it classifies as flat,
        // and `Config` has a permissive serde… but we DO want the root-
        // level section-key typo to surface in practice. Right now this
        // is accepted silently (the classifier routes to flat and flat
        // lacks deny_unknown_fields). We document the current behavior
        // so a future tightening of `Config` to `deny_unknown_fields`
        // has a test anchor.
        let yaml = r#"
storge:
  default_backend: "hetzner"
"#;
        // Currently: classified as flat, silently accepted as default.
        // This is NOT ideal but matches pre-Phase-3a behavior for any
        // unknown top-level key. Tightening requires a one-release
        // deprecation window to avoid breaking operators who've been
        // relying on silently-ignored fields.
        let cfg = Config::from_yaml_str(yaml).unwrap();
        assert_eq!(
            cfg,
            Config::default(),
            "unknown root key currently silently ignored (pre-existing Config behavior)"
        );
    }

    #[test]
    fn test_defaults_version_current_is_v1() {
        // Pinning test: if a future release changes the default
        // DefaultsVersion to V2, an export that omitted `defaults:`
        // (because it equalled the old default) will re-import at the
        // new default, which is a silent version drift. Bumping this
        // assertion on purpose forces the release engineer to think
        // about migration.
        assert_eq!(DefaultsVersion::default(), DefaultsVersion::V1);
        assert!(
            DefaultsVersion::V1.is_default(),
            "DefaultsVersion::V1 must be the current default; a future V2 must update this \
             assertion AND provide a migration path for YAML files that omitted `defaults:`"
        );
    }

    // ── Phase 3b.1: `public: true` shorthand through the full load path ──

    #[test]
    fn test_bucket_public_shorthand_normalised_on_yaml_load() {
        // A sectioned YAML with `public: true` on a bucket expands to
        // `public_prefixes: [""]` after normalisation, which is the form
        // the runtime (PublicPrefixSnapshot) already knows how to serve.
        let yaml = r#"
storage:
  buckets:
    my-bucket:
      public: true
"#;
        let cfg = Config::from_yaml_str(yaml).unwrap();
        let policy = cfg.buckets.get("my-bucket").unwrap();
        assert_eq!(policy.public_prefixes, vec![String::new()]);
        assert_eq!(policy.public, Some(true));
    }

    #[test]
    fn test_bucket_public_shorthand_conflict_rejected_at_load() {
        // Mixing `public: true` with `public_prefixes` is operator error
        // — the loader must refuse, not silently pick one.
        let yaml = r#"
storage:
  buckets:
    my-bucket:
      public: true
      public_prefixes:
        - "releases/"
"#;
        let err =
            Config::from_yaml_str(yaml).expect_err("public + public_prefixes must be rejected");
        let msg = format!("{err}");
        assert!(
            msg.contains("my-bucket"),
            "error must name the offending bucket, got: {msg}"
        );
        assert!(
            msg.contains("public"),
            "error must mention `public`, got: {msg}"
        );
    }

    #[test]
    fn test_bucket_public_shorthand_roundtrip_via_canonical_yaml() {
        // Round-trip: YAML with `public: true` → load → export → re-load.
        // Canonical exporter must use the shorthand form again (cleaner
        // GitOps diffs), and the re-loaded config must be identical to
        // the first load.
        let yaml = r#"
storage:
  buckets:
    my-bucket:
      public: true
"#;
        let cfg1 = Config::from_yaml_str(yaml).unwrap();
        let exported = cfg1.to_canonical_yaml().unwrap();
        assert!(
            exported.contains("public: true"),
            "canonical export must use the shorthand form, got:\n{exported}"
        );
        assert!(
            !exported.contains("public_prefixes:"),
            "canonical export must NOT emit the long form when shorthand applies, got:\n{exported}"
        );
        let cfg2 = Config::from_yaml_str(&exported).unwrap();
        assert_eq!(
            cfg1.buckets, cfg2.buckets,
            "bucket policies must round-trip losslessly"
        );
    }

    #[test]
    fn test_bucket_specific_prefixes_roundtrip_without_shorthand() {
        // When a bucket has specific prefixes (not the `[""]` sentinel),
        // the exporter must keep the long form — shorthand only applies
        // to the unambiguous "entire bucket is public" case.
        let yaml = r#"
storage:
  buckets:
    semi-public:
      public_prefixes:
        - "releases/"
        - "docs/"
"#;
        let cfg = Config::from_yaml_str(yaml).unwrap();
        let exported = cfg.to_canonical_yaml().unwrap();
        assert!(
            exported.contains("public_prefixes:"),
            "specific-prefix config must round-trip as long form, got:\n{exported}"
        );
        assert!(
            !exported.contains("public: true"),
            "shorthand must not be emitted for multi-prefix config, got:\n{exported}"
        );
    }

    // ── Phase 3b.1: storage shorthand through the full load path ──────

    /// The T1 acceptance example from the plan: a 5-line config that
    /// loads, starts, and serves S3 traffic. The acceptance gate is
    /// "loads without error" — downstream startup is tested separately.
    #[test]
    fn test_t1_five_line_example_loads() {
        // Five lines, counting only non-blank content:
        //   1. storage:
        //   2.   s3: http://minio:9000
        //   3.   access_key_id: AKIAEXAMPLE
        //   4.   secret_access_key: SECRET
        //   5.   buckets:
        // (the empty bucket map is elided in YAML counting)
        let yaml = r#"
storage:
  s3: http://minio:9000
  access_key_id: AKIAEXAMPLE
  secret_access_key: SECRET
"#;
        let cfg = Config::from_yaml_str(yaml).unwrap();
        match &cfg.backend {
            BackendConfig::S3 {
                endpoint,
                region,
                access_key_id,
                secret_access_key,
                ..
            } => {
                assert_eq!(endpoint.as_deref(), Some("http://minio:9000"));
                assert_eq!(region, "us-east-1");
                assert_eq!(access_key_id.as_deref(), Some("AKIAEXAMPLE"));
                assert_eq!(secret_access_key.as_deref(), Some("SECRET"));
            }
            other => panic!("T1 example must yield S3 backend, got {other:?}"),
        }
    }

    #[test]
    fn test_filesystem_shorthand_loads_via_yaml() {
        let yaml = r#"
storage:
  filesystem: /var/lib/dgp
"#;
        let cfg = Config::from_yaml_str(yaml).unwrap();
        match &cfg.backend {
            BackendConfig::Filesystem { path } => {
                assert_eq!(path.to_str(), Some("/var/lib/dgp"));
            }
            other => panic!("filesystem shorthand must yield Filesystem backend, got {other:?}"),
        }
    }

    #[test]
    fn test_storage_shorthand_plus_explicit_backend_is_rejected_at_load() {
        // Operator error surfaces cleanly from the full load path.
        let yaml = r#"
storage:
  s3: http://minio:9000
  backend:
    type: filesystem
    path: /explicit
"#;
        let err =
            Config::from_yaml_str(yaml).expect_err("shorthand + explicit backend must be rejected");
        let msg = format!("{err}");
        assert!(
            msg.contains("shorthand") || msg.contains("backend"),
            "error must explain the shorthand/backend conflict, got: {msg}"
        );
    }

    // ── Phase 3b.2.a: operator-authored admission blocks ─────────────

    #[test]
    fn test_admission_blocks_deserialize_and_roundtrip() {
        let yaml = r#"
admission:
  blocks:
    - name: deny-bad-ips
      match:
        source_ip_list: ["203.0.113.5", "198.51.100.0/24"]
      action: deny
    - name: maint
      match:
        config_flag: "maintenance_mode"
      action:
        type: reject
        status: 503
        message: "back soon"
"#;
        let cfg = Config::from_yaml_str(yaml).unwrap();
        assert_eq!(cfg.admission_blocks.len(), 2);
        assert_eq!(cfg.admission_blocks[0].name, "deny-bad-ips");
        assert_eq!(cfg.admission_blocks[1].name, "maint");

        // Round-trip through canonical YAML must preserve everything.
        let exported = cfg.to_canonical_yaml().unwrap();
        assert!(exported.contains("admission:"));
        assert!(exported.contains("deny-bad-ips"));
        assert!(exported.contains("maint"));
        let cfg2 = Config::from_yaml_str(&exported).unwrap();
        assert_eq!(cfg.admission_blocks, cfg2.admission_blocks);
    }

    #[test]
    fn test_admission_duplicate_block_names_rejected_at_load() {
        let yaml = r#"
admission:
  blocks:
    - name: same
      match: {}
      action: continue
    - name: same
      match: {}
      action: deny
"#;
        let err = Config::from_yaml_str(yaml)
            .expect_err("duplicate admission block names must be rejected");
        assert!(format!("{err}").contains("duplicate"));
    }

    #[test]
    fn test_admission_invalid_reject_status_rejected_at_load() {
        let yaml = r#"
admission:
  blocks:
    - name: bad
      match: {}
      action:
        type: reject
        status: 200
"#;
        let err = Config::from_yaml_str(yaml).expect_err("reject with 2xx status must be rejected");
        let msg = format!("{err}");
        assert!(
            msg.contains("4xx") || msg.contains("5xx"),
            "error must point at the status range, got: {msg}"
        );
    }

    #[test]
    fn test_admission_unknown_field_in_match_rejected() {
        // `deny_unknown_fields` on MatchSpec catches field typos.
        let yaml = r#"
admission:
  blocks:
    - name: typo
      match:
        source_ips: ["1.2.3.4"]
      action: deny
"#;
        let err = Config::from_yaml_str(yaml).expect_err("typo in match field must be rejected");
        assert!(format!("{err}").contains("source_ips"));
    }

    #[test]
    fn test_admission_empty_omitted_on_default_export() {
        // A Config with no operator-authored blocks must not emit an
        // `admission:` section — keeps default-config YAML minimal.
        let cfg = Config::default();
        let exported = cfg.to_canonical_yaml().unwrap();
        assert!(
            !exported.contains("admission:"),
            "empty admission must be omitted, got:\n{exported}"
        );
    }

    // ── Phase 3b.2.a hardening: flat-shape + classifier coverage ──────

    #[test]
    fn test_admission_blocks_flat_shape_also_validates() {
        // H1 from adversarial review: the flat-shape load path must
        // ALSO run AdmissionSpec::validate so duplicate names / bad
        // reject status don't slip through.
        let yaml = r#"
listen_addr: "127.0.0.1:9000"
admission_blocks:
  - name: same
    match: {}
    action: deny
  - name: same
    match: {}
    action: continue
"#;
        let err = Config::from_yaml_str(yaml)
            .expect_err("duplicate block name on flat-shape path must fail");
        assert!(
            format!("{err}").contains("duplicate"),
            "error must say duplicate, got: {err}"
        );
    }

    #[test]
    fn test_admission_blocks_flat_only_keys_coverage() {
        // M2 from adversarial review: `admission_blocks:` at flat root
        // is valid (flat shape preservation), but mixing it with the
        // sectioned `admission:` must be rejected as a mixed doc.
        let yaml = r#"
admission_blocks:
  - name: x
    match: {}
    action: continue
admission:
  blocks: []
"#;
        let err = Config::from_yaml_str(yaml)
            .expect_err("mixed admission_blocks + admission must be rejected");
        let msg = format!("{err}");
        assert!(
            msg.contains("mix") || msg.contains("flat") || msg.contains("section"),
            "error must explain the mixed-shape, got: {msg}"
        );
    }

    // ── Phase 3c.1: iam_mode enum ──────────────────────────────────────

    #[test]
    fn test_iam_mode_default_is_gui() {
        let cfg = Config::default();
        assert_eq!(cfg.iam_mode, crate::config_sections::IamMode::Gui);
    }

    #[test]
    fn test_iam_mode_omitted_from_default_export() {
        // Minimalism invariant: default deployments don't gain an
        // `iam_mode: gui` line in their exported YAML.
        let cfg = Config::default();
        let exported = cfg.to_canonical_yaml().unwrap();
        assert!(
            !exported.contains("iam_mode"),
            "default iam_mode must be omitted, got:\n{exported}"
        );
    }

    #[test]
    fn test_iam_mode_declarative_roundtrips_through_sectioned_yaml() {
        let yaml = r#"
access:
  iam_mode: declarative
"#;
        let cfg = Config::from_yaml_str(yaml).unwrap();
        assert_eq!(cfg.iam_mode, crate::config_sections::IamMode::Declarative);
        let exported = cfg.to_canonical_yaml().unwrap();
        assert!(
            exported.contains("iam_mode: declarative"),
            "declarative mode must survive round-trip, got:\n{exported}"
        );
        let reloaded = Config::from_yaml_str(&exported).unwrap();
        assert_eq!(reloaded.iam_mode, cfg.iam_mode);
    }

    #[test]
    fn test_iam_mode_flat_shape_also_accepts() {
        let yaml = r#"
listen_addr: "127.0.0.1:9000"
iam_mode: declarative
"#;
        let cfg = Config::from_yaml_str(yaml).unwrap();
        assert_eq!(cfg.iam_mode, crate::config_sections::IamMode::Declarative);
    }

    #[test]
    fn test_iam_mode_unknown_variant_rejected() {
        let yaml = r#"
access:
  iam_mode: wat
"#;
        let err = Config::from_yaml_str(yaml).expect_err("unknown iam_mode value must be rejected");
        let msg = format!("{err}");
        assert!(
            msg.contains("wat") || msg.contains("iam_mode"),
            "error must explain the offending value, got: {msg}"
        );
    }

    #[test]
    fn test_admission_blocks_flat_shape_loads_without_sectioned_wrapper() {
        // Flat-shape YAML with only `admission_blocks:` at root must
        // parse without error (classifier routes to Flat because there
        // are no section keys).
        let yaml = r#"
admission_blocks:
  - name: deny-bad
    match:
      source_ip_list: ["203.0.113.5"]
    action: deny
"#;
        let cfg = Config::from_yaml_str(yaml).unwrap();
        assert_eq!(cfg.admission_blocks.len(), 1);
        assert_eq!(cfg.admission_blocks[0].name, "deny-bad");
    }

    #[test]
    fn test_check_warns_on_env_suffix_collision() {
        // Correctness x-ray C4: "eu-archive" and "eu.archive" both
        // normalize to EU_ARCHIVE, so both would read the SAME env
        // var DGP_BACKEND_EU_ARCHIVE_ENCRYPTION_KEY. With the same
        // key material but distinct derived key_ids (the raw name
        // feeds into derive_key_id), objects written by one backend
        // can't be read by the other. check() now warns loudly.
        let mut cfg = Config {
            backends: vec![
                NamedBackendConfig {
                    name: "eu-archive".into(),
                    backend: BackendConfig::Filesystem { path: "/a".into() },
                    encryption: BackendEncryptionConfig::default(),
                },
                NamedBackendConfig {
                    name: "eu.archive".into(),
                    backend: BackendConfig::Filesystem { path: "/b".into() },
                    encryption: BackendEncryptionConfig::default(),
                },
            ],
            ..Config::default()
        };
        let warnings = cfg.check();
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("env-var suffix") && w.contains("EU_ARCHIVE")),
            "expected env-suffix collision warning, got {:?}",
            warnings
        );
    }

    #[test]
    fn test_check_no_env_suffix_warning_for_distinct_names() {
        let mut cfg = Config {
            backends: vec![
                NamedBackendConfig {
                    name: "one".into(),
                    backend: BackendConfig::Filesystem { path: "/a".into() },
                    encryption: BackendEncryptionConfig::default(),
                },
                NamedBackendConfig {
                    name: "two".into(),
                    backend: BackendConfig::Filesystem { path: "/b".into() },
                    encryption: BackendEncryptionConfig::default(),
                },
            ],
            ..Config::default()
        };
        let warnings = cfg.check();
        assert!(
            !warnings.iter().any(|w| w.contains("env-var suffix")),
            "distinct names must not trip env-suffix collision, got {:?}",
            warnings
        );
    }

    #[test]
    fn test_persist_to_file_preserves_yaml_stored_encryption_keys() {
        // Regression for xray-finding C1: before the fix,
        // `persist_to_file` → `to_canonical_yaml` → `redact_infra_secrets`
        // would strip per-backend encryption keys from the on-disk
        // YAML on every admin write. Operator-configured keys
        // silently disappeared on the next `PATCH … /config/section`
        // round-trip, and the next server restart fell back to env
        // lookup; if no env var was set, historical encrypted reads
        // started erroring and new writes landed plaintext.
        //
        // The persist path now uses the _for_persist serializers
        // which only strip the bootstrap password hash.
        let dir = tempfile::tempdir().unwrap();
        let yaml_path = dir.path().join("out.yaml");
        let cfg = Config {
            listen_addr: "127.0.0.1:9099".parse().unwrap(),
            backends: vec![NamedBackendConfig {
                name: "b".into(),
                backend: BackendConfig::Filesystem {
                    path: "/tmp/x".into(),
                },
                encryption: BackendEncryptionConfig::Aes256GcmProxy {
                    // 64-char hex key — realistic shape.
                    key: Some(
                        "0101010101010101010101010101010101010101010101010101010101010101".into(),
                    ),
                    key_id: Some("test-kid".into()),
                    legacy_key: Some(
                        "0202020202020202020202020202020202020202020202020202020202020202".into(),
                    ),
                    legacy_key_id: Some("test-legacy-kid".into()),
                },
            }],
            ..Config::default()
        };

        cfg.persist_to_file(yaml_path.to_str().unwrap()).unwrap();

        let persisted = std::fs::read_to_string(&yaml_path).unwrap();
        assert!(
            persisted.contains("0101010101010101010101010101010101010101010101010101010101010101"),
            "persisted YAML must preserve the primary encryption key, got:\n{}",
            persisted
        );
        assert!(
            persisted.contains("0202020202020202020202020202020202020202020202020202020202020202"),
            "persisted YAML must preserve the legacy encryption key, got:\n{}",
            persisted
        );

        // Export serialization (the downloadable artifact) MUST continue
        // to redact.
        let exported = cfg.to_canonical_yaml().unwrap();
        assert!(
            !exported.contains("0101010101010101010101010101010101010101010101010101010101010101"),
            "exported YAML must redact the primary encryption key, got:\n{}",
            exported
        );
        assert!(
            !exported.contains("0202020202020202020202020202020202020202020202020202020202020202"),
            "exported YAML must redact the legacy encryption key, got:\n{}",
            exported
        );
    }
}
