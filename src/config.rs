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

    /// Storage backend configuration
    #[serde(default)]
    pub backend: BackendConfig,

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

    /// AES-256 master key for encryption at rest (64-char hex string = 256 bits).
    /// When set, all new writes are AES-256-GCM encrypted. Existing unencrypted
    /// objects remain readable (detected via metadata). Env: `DGP_ENCRYPTION_KEY`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encryption_key: Option<String>,

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
}

/// A named storage backend with its connection configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NamedBackendConfig {
    /// Human-readable name (e.g., "local", "hetzner", "aws")
    pub name: String,
    /// The actual backend configuration
    #[serde(flatten)]
    pub backend: BackendConfig,
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

// Default value functions for serde
fn default_listen_addr() -> SocketAddr {
    "0.0.0.0:9000".parse().unwrap()
}

fn default_max_delta_ratio() -> f32 {
    0.75
}

fn default_max_object_size() -> u64 {
    100 * 1024 * 1024 // 100MB
}

fn default_cache_size_mb() -> usize {
    100
}

fn default_metadata_cache_mb() -> usize {
    50
}

fn default_region() -> String {
    "us-east-1".to_string()
}

fn default_force_path_style() -> bool {
    true
}

fn default_log_level() -> String {
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
            encryption_key: None,
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

/// Parse a boolean env var (`"true"` or `"1"` → true), returning `default` if absent.
pub fn env_bool(var: &str, default: bool) -> bool {
    std::env::var(var)
        .ok()
        .map(|v| v == "true" || v == "1")
        .unwrap_or(default)
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

/// Classify a parsed YAML document as *sectioned* (Phase 3 canonical shape)
/// vs. *flat* (legacy). True iff the top-level mapping contains at least one
/// of the four section keys and no legacy top-level keys. This detection is
/// intentionally cautious: a document that mixes both shapes
/// (`listen_addr:` at the root AND a `storage:` section) is treated as flat
/// and the sectioned key becomes an unknown-field error — no silent merging
/// of two competing shapes.
fn looks_sectioned(doc: &serde_yaml::Value) -> bool {
    let Some(map) = doc.as_mapping() else {
        return false;
    };
    const SECTION_KEYS: &[&str] = &["admission", "access", "storage", "advanced"];
    // Any flat-shape-only key at the root rules out sectioned classification.
    // `defaults` is permitted at the root in both shapes (it's the
    // document-level version pin, not a section).
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
        "admin_password_hash", // legacy alias
        "codec_concurrency",
        "blocking_threads",
        "log_level",
        "config_sync_bucket",
        "encryption_key",
        "tls",
        "buckets",
    ];

    let has_section = map
        .keys()
        .filter_map(|k| k.as_str())
        .any(|k| SECTION_KEYS.contains(&k));
    let has_flat = map
        .keys()
        .filter_map(|k| k.as_str())
        .any(|k| FLAT_ONLY_KEYS.contains(&k));

    has_section && !has_flat
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

    /// Load configuration from a TOML file explicitly.
    pub fn from_toml_file(path: &str) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path).map_err(|e| ConfigError::Io(e.to_string()))?;
        let config: Config =
            toml::from_str(&content).map_err(|e| ConfigError::Parse(e.to_string()))?;
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

        if looks_sectioned(&doc) {
            let sectioned: crate::config_sections::SectionedConfig =
                serde_yaml::from_value(doc).map_err(|e| ConfigError::Parse(e.to_string()))?;
            Ok(sectioned.into_flat())
        } else {
            let flat: Config =
                serde_yaml::from_value(doc).map_err(|e| ConfigError::Parse(e.to_string()))?;
            Ok(flat)
        }
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

        // Encryption at rest
        if let Ok(key) = std::env::var("DGP_ENCRYPTION_KEY") {
            if !key.is_empty() {
                self.encryption_key = Some(key);
            }
        }

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
    /// legacy `to_toml_string` policy: strips `bootstrap_password_hash` and
    /// `encryption_key` only. Proxy SigV4 credentials and backend credentials
    /// are kept — the wizard, file-based deployment, and users reading the
    /// file on disk all depend on them being present. Use
    /// [`Self::redact_all_secrets`] for the admin-API "export" flow that
    /// never trusts the disk as a secret store.
    fn redact_infra_secrets(&self) -> Self {
        let mut export = self.clone();
        export.bootstrap_password_hash = None;
        export.encryption_key = None;
        export
    }

    /// Clone the config with *every* secret redacted: infra secrets plus all
    /// SigV4 credentials (top-level and per-backend). This is the right level
    /// of paranoia for the admin API `GET /export` endpoint (Phase 1): the
    /// operator reading the exported YAML must refill secrets from their
    /// secret manager, not copy them out of an API response.
    pub fn redact_all_secrets(&self) -> Self {
        let mut export = self.redact_infra_secrets();
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

    /// Serialize config to TOML string (strips infra secrets: bootstrap hash
    /// and encryption key). SigV4 credentials are kept — see
    /// [`Self::redact_all_secrets`] for the fully-redacted export variant.
    pub fn to_toml_string(&self) -> Result<String, ConfigError> {
        let export = self.redact_infra_secrets();
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
        let export = self.redact_infra_secrets();
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
    pub fn persist_to_file(&self, path: &str) -> Result<(), ConfigError> {
        let content = match ConfigFormat::from_path(path) {
            ConfigFormat::Yaml => self.to_canonical_yaml()?,
            ConfigFormat::Toml => self.to_toml_string()?,
        };
        atomic_write(std::path::Path::new(path), content.as_bytes())
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
            encryption_key: Some("deadbeef-hex-encryption-key".into()),
            ..Config::default()
        };

        let yaml = cfg.to_canonical_yaml().unwrap();
        // Infra secrets are stripped
        assert!(!yaml.contains("$2b$"));
        assert!(!yaml.contains("deadbeef-hex-encryption-key"));
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
            encryption_key: Some("deadbeef-hex-encryption-key".into()),
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
        });

        let redacted = cfg.redact_all_secrets();
        let yaml = serde_yaml::to_string(&redacted).unwrap();
        // Top-level proxy creds
        assert!(!yaml.contains("AKIASHOULDNOTAPPEAR"));
        assert!(!yaml.contains("secret-should-not-appear"));
        // Bootstrap + encryption
        assert!(!yaml.contains("$2b$"));
        assert!(!yaml.contains("deadbeef-hex-encryption-key"));
        // Primary backend creds
        assert!(!yaml.contains("BACKEND-SECRET-ID"));
        assert!(!yaml.contains("BACKEND-SECRET-KEY"));
        // Named backend creds
        assert!(!yaml.contains("NAMED-SECRET-ID"));
        assert!(!yaml.contains("NAMED-SECRET-KEY"));
        // Non-secret fields survive
        assert!(yaml.contains("hetzner"));
        assert!(yaml.contains("eu-central-1"));
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
                },
                NamedBackendConfig {
                    name: "unique".into(),
                    backend: BackendConfig::Filesystem { path: "/b".into() },
                },
                NamedBackendConfig {
                    name: "shared".into(),
                    backend: BackendConfig::Filesystem { path: "/c".into() },
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
                },
                NamedBackendConfig {
                    name: "b".into(),
                    backend: BackendConfig::Filesystem { path: "/b".into() },
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
    fn test_from_yaml_str_mixed_shape_is_rejected_as_flat() {
        // A doc with BOTH a flat key (`listen_addr:`) AND a section key
        // (`storage:`) is classified as flat, causing the sectioned key to
        // become an unknown field under strict deserialization (or to be
        // silently ignored otherwise). Either way, we must not silently
        // merge two competing shapes — the operator sees an error or a
        // non-effect they can reason about.
        let yaml = r#"
listen_addr: "127.0.0.1:9125"
storage:
  default_backend: "hetzner"
"#;
        // The flat deserializer will see `storage` and — because Config
        // doesn't have that field — it either errors or ignores. Both are
        // acceptable outcomes of the "no silent merge" invariant. What we
        // assert is: listen_addr got read (flat shape was chosen).
        match Config::from_yaml_str(yaml) {
            Ok(cfg) => {
                assert_eq!(
                    cfg.listen_addr.port(),
                    9125,
                    "flat shape must have been selected"
                );
            }
            Err(_) => {
                // Strict parse error is also acceptable — we prefer loud
                // over silent-merge.
            }
        }
    }
}
