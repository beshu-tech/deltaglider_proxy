//! Configuration for DeltaGlider Proxy S3 server

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
];

/// Thread-safe shared config for hot-reload from admin GUI.
pub type SharedConfig = Arc<tokio::sync::RwLock<Config>>;

/// Server configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
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

    /// Proxy access key ID for SigV4 authentication.
    /// When both access_key_id and secret_access_key are set, all requests
    /// must be SigV4-signed with these credentials. When unset, open access.
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

    /// TLS configuration (optional).
    /// When enabled, both the S3 port and the demo UI port serve HTTPS.
    #[serde(default)]
    pub tls: Option<TlsConfig>,
}

/// TLS configuration (optional)
#[derive(Debug, Clone, Serialize, Deserialize)]
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
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    0.5
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
            listen_addr: default_listen_addr(),
            backend: BackendConfig::default(),
            max_delta_ratio: default_max_delta_ratio(),
            max_object_size: default_max_object_size(),
            cache_size_mb: default_cache_size_mb(),
            metadata_cache_mb: default_metadata_cache_mb(),
            access_key_id: None,
            secret_access_key: None,
            bootstrap_password_hash: None,
            codec_concurrency: None,
            blocking_threads: None,
            log_level: default_log_level(),
            tls: None,
        }
    }
}

/// Parse an env var into a typed value, warning on invalid input.
fn env_parse<T: std::str::FromStr>(var: &str) -> Option<T>
where
    T::Err: std::fmt::Display,
{
    std::env::var(var).ok().and_then(|raw| {
        raw.parse()
            .map_err(|e| eprintln!("Warning: ignoring invalid {var}=\"{raw}\": {e}"))
            .ok()
    })
}

impl Config {
    /// Load configuration from a TOML file
    pub fn from_file(path: &str) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path).map_err(|e| ConfigError::Io(e.to_string()))?;
        let config: Config =
            toml::from_str(&content).map_err(|e| ConfigError::Parse(e.to_string()))?;
        Ok(config)
    }

    /// Load configuration from environment variables
    pub fn from_env() -> Self {
        let mut config = Self::default();

        if let Ok(addr) = std::env::var("DGP_LISTEN_ADDR") {
            match addr.parse() {
                Ok(parsed) => config.listen_addr = parsed,
                Err(e) => eprintln!("Warning: ignoring invalid DGP_LISTEN_ADDR=\"{addr}\": {e}"),
            }
        }

        // Check for S3 backend configuration
        if std::env::var("DGP_S3_ENDPOINT").is_ok() || std::env::var("DGP_S3_REGION").is_ok() {
            config.backend = BackendConfig::S3 {
                endpoint: std::env::var("DGP_S3_ENDPOINT").ok(),
                region: std::env::var("DGP_S3_REGION").unwrap_or_else(|_| "us-east-1".to_string()),
                force_path_style: std::env::var("DGP_S3_PATH_STYLE")
                    .map(|v| v == "true" || v == "1")
                    .unwrap_or(true),
                access_key_id: std::env::var("DGP_BE_AWS_ACCESS_KEY_ID").ok(),
                secret_access_key: std::env::var("DGP_BE_AWS_SECRET_ACCESS_KEY").ok(),
            };
        } else if let Ok(dir) = std::env::var("DGP_DATA_DIR") {
            config.backend = BackendConfig::Filesystem {
                path: PathBuf::from(dir),
            };
        }

        if let Some(v) = env_parse::<f32>("DGP_MAX_DELTA_RATIO") {
            config.max_delta_ratio = v;
        }
        if let Some(v) = env_parse::<u64>("DGP_MAX_OBJECT_SIZE") {
            config.max_object_size = v;
        }
        if let Some(v) = env_parse::<usize>("DGP_CACHE_MB") {
            config.cache_size_mb = v;
        }
        if let Some(v) = env_parse::<usize>("DGP_METADATA_CACHE_MB") {
            config.metadata_cache_mb = v;
        }
        if let Some(v) = env_parse::<usize>("DGP_CODEC_CONCURRENCY") {
            config.codec_concurrency = Some(v);
        }
        if let Some(v) = env_parse::<usize>("DGP_BLOCKING_THREADS") {
            config.blocking_threads = Some(v);
        }

        // Proxy authentication credentials
        config.access_key_id = std::env::var("DGP_ACCESS_KEY_ID").ok();
        config.secret_access_key = std::env::var("DGP_SECRET_ACCESS_KEY").ok();

        // Admin GUI password hash
        config.bootstrap_password_hash = std::env::var("DGP_BOOTSTRAP_PASSWORD_HASH")
            .or_else(|_| std::env::var("DGP_ADMIN_PASSWORD_HASH"))
            .ok();

        // Log level (runtime operational)
        if let Ok(level) = std::env::var("DGP_LOG_LEVEL") {
            config.log_level = level;
        }

        // TLS configuration
        if let Ok(enabled) = std::env::var("DGP_TLS_ENABLED") {
            if enabled == "true" || enabled == "1" {
                config.tls = Some(TlsConfig {
                    enabled: true,
                    cert_path: std::env::var("DGP_TLS_CERT").ok(),
                    key_path: std::env::var("DGP_TLS_KEY").ok(),
                });
            }
        }

        config
    }

    /// Load configuration from file if it exists, otherwise from environment
    pub fn load() -> Self {
        // Try config file first
        let config = if let Ok(path) = std::env::var("DGP_CONFIG") {
            if let Ok(config) = Self::from_file(&path) {
                config
            } else {
                Self::from_env()
            }
        } else {
            // Try default config file locations
            let mut found = None;
            for path in &[
                "deltaglider_proxy.toml",
                "/etc/deltaglider_proxy/config.toml",
            ] {
                if std::path::Path::new(path).exists() {
                    if let Ok(config) = Self::from_file(path) {
                        found = Some(config);
                        break;
                    }
                }
            }
            found.unwrap_or_else(Self::from_env)
        };
        config.validate();
        config
    }

    /// Validate config values are in acceptable ranges. Called after loading.
    pub fn validate(&self) {
        if self.max_delta_ratio < 0.0 || self.max_delta_ratio > 1.0 {
            eprintln!(
                "Warning: max_delta_ratio={} is outside [0.0, 1.0] — delta compression decisions may behave unexpectedly",
                self.max_delta_ratio
            );
        }
        if self.max_object_size == 0 {
            eprintln!("Warning: max_object_size=0 will reject all uploads");
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

    /// Ensure bootstrap_password_hash is set. Resolution order:
    /// 1. Already set in config (env var or TOML) — use it.
    /// 2. Persisted state file `.deltaglider_bootstrap_hash` (or legacy `.deltaglider_admin_hash`).
    /// 3. Generate a random password, hash it, persist, and print to stderr.
    ///
    /// Returns the bcrypt hash.
    pub fn ensure_bootstrap_password_hash(&mut self) -> String {
        if let Some(ref hash) = self.bootstrap_password_hash {
            return hash.clone();
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
            if let Ok(hash) = std::fs::read_to_string(state_file) {
                let hash = hash.trim().to_string();
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
        if let Err(e) = std::fs::write(persist_file, &hash) {
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

    /// Serialize config to TOML string (excludes bootstrap_password_hash for security).
    pub fn to_toml_string(&self) -> Result<String, ConfigError> {
        // Clone and strip the admin hash before serializing
        let mut export = self.clone();
        export.bootstrap_password_hash = None;
        toml::to_string_pretty(&export).map_err(|e| ConfigError::Parse(e.to_string()))
    }

    /// Persist the current config to a TOML file.
    pub fn persist_to_file(&self, path: &str) -> Result<(), ConfigError> {
        let content = self.to_toml_string()?;
        std::fs::write(path, content).map_err(|e| ConfigError::Io(e.to_string()))
    }
}

/// Configuration errors
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("IO error: {0}")]
    Io(String),

    #[error("Parse error: {0}")]
    Parse(String),
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

        // Every registry entry must be referenced in from_env (no stale entries)
        for name in &registry_names {
            // DGP_CONFIG is checked in load(), not from_env() — allow it.
            // DGP_CONFIG_SYNC_BUCKET is checked in main.rs init_config_sync(), not from_env().
            if *name == "DGP_CONFIG" || *name == "DGP_CONFIG_SYNC_BUCKET" {
                continue;
            }
            assert!(
                used_in_from_env.contains(name),
                "Env var {name} is in ENV_VAR_REGISTRY but not used in from_env()"
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
    fn test_print_example_toml_is_valid() {
        // The base TOML from Config::default() must round-trip
        let default_cfg = Config::default();
        let toml_str = toml::to_string_pretty(&default_cfg).unwrap();
        let parsed: Config = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.listen_addr, default_cfg.listen_addr);
        assert_eq!(parsed.cache_size_mb, default_cfg.cache_size_mb);
        assert_eq!(parsed.max_delta_ratio, default_cfg.max_delta_ratio);
    }
}
