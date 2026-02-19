//! Configuration for DeltaGlider Proxy S3 server

use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

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

    /// Proxy access key ID for SigV4 authentication.
    /// When both access_key_id and secret_access_key are set, all requests
    /// must be SigV4-signed with these credentials. When unset, open access.
    #[serde(default)]
    pub access_key_id: Option<String>,

    /// Proxy secret access key for SigV4 authentication.
    /// Must be set together with access_key_id.
    #[serde(default)]
    pub secret_access_key: Option<String>,

    /// Bcrypt hash of the admin GUI password.
    /// Set via DGP_ADMIN_PASSWORD_HASH env var, or auto-generated on first run.
    #[serde(default)]
    pub admin_password_hash: Option<String>,

    /// Log level filter string.
    /// Set via config file, DGP_LOG_LEVEL env var, or admin GUI. Overridden by RUST_LOG.
    /// Default: "deltaglider_proxy=debug,tower_http=debug"
    #[serde(default = "default_log_level")]
    pub log_level: String,
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
            access_key_id: None,
            secret_access_key: None,
            admin_password_hash: None,
            log_level: default_log_level(),
        }
    }
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
            if let Ok(parsed) = addr.parse() {
                config.listen_addr = parsed;
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

        if let Ok(ratio) = std::env::var("DGP_MAX_DELTA_RATIO") {
            if let Ok(parsed) = ratio.parse() {
                config.max_delta_ratio = parsed;
            }
        }

        if let Ok(size) = std::env::var("DGP_MAX_OBJECT_SIZE") {
            if let Ok(parsed) = size.parse() {
                config.max_object_size = parsed;
            }
        }

        if let Ok(cache) = std::env::var("DGP_CACHE_MB") {
            if let Ok(parsed) = cache.parse() {
                config.cache_size_mb = parsed;
            }
        }

        // Proxy authentication credentials
        config.access_key_id = std::env::var("DGP_ACCESS_KEY_ID").ok();
        config.secret_access_key = std::env::var("DGP_SECRET_ACCESS_KEY").ok();

        // Admin GUI password hash
        config.admin_password_hash = std::env::var("DGP_ADMIN_PASSWORD_HASH").ok();

        // Log level (runtime operational)
        if let Ok(level) = std::env::var("DGP_LOG_LEVEL") {
            config.log_level = level;
        }

        config
    }

    /// Load configuration from file if it exists, otherwise from environment
    pub fn load() -> Self {
        // Try config file first
        if let Ok(path) = std::env::var("DGP_CONFIG") {
            if let Ok(config) = Self::from_file(&path) {
                return config;
            }
        }

        // Try default config file locations
        for path in &[
            "deltaglider_proxy.toml",
            "/etc/deltaglider_proxy/config.toml",
        ] {
            if std::path::Path::new(path).exists() {
                if let Ok(config) = Self::from_file(path) {
                    return config;
                }
            }
        }

        // Fall back to environment variables
        Self::from_env()
    }

    /// Returns true if SigV4 authentication is enabled (both credentials are set).
    pub fn auth_enabled(&self) -> bool {
        self.access_key_id.is_some() && self.secret_access_key.is_some()
    }

    /// Ensure admin_password_hash is set. Resolution order:
    /// 1. Already set in config (env var or TOML) — use it.
    /// 2. Persisted state file `.deltaglider_admin_hash` — load it.
    /// 3. Generate a random password, hash it, persist, and print to stderr.
    ///
    /// Returns the bcrypt hash.
    pub fn ensure_admin_password_hash(&mut self) -> String {
        if let Some(ref hash) = self.admin_password_hash {
            return hash.clone();
        }

        let state_file = std::path::Path::new(".deltaglider_admin_hash");
        if state_file.exists() {
            if let Ok(hash) = std::fs::read_to_string(state_file) {
                let hash = hash.trim().to_string();
                if !hash.is_empty() {
                    self.admin_password_hash = Some(hash.clone());
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

        // Persist the hash
        if let Err(e) = std::fs::write(state_file, &hash) {
            eprintln!(
                "Warning: could not persist admin hash to {}: {}",
                state_file.display(),
                e
            );
        }

        // Print prominently to stderr
        eprintln!();
        eprintln!("╔══════════════════════════════════════════════════════════╗");
        eprintln!("║  ADMIN PASSWORD (first run — save this!)                ║");
        eprintln!("║                                                          ║");
        eprintln!("║  Password: {:<45}║", password);
        eprintln!("║                                                          ║");
        eprintln!("║  Set DGP_ADMIN_PASSWORD_HASH to skip auto-generation.   ║");
        eprintln!("╚══════════════════════════════════════════════════════════╝");
        eprintln!();

        self.admin_password_hash = Some(hash.clone());
        hash
    }

    /// Wrap this config in an `Arc<RwLock>` for shared mutable access.
    pub fn into_shared(self) -> SharedConfig {
        Arc::new(tokio::sync::RwLock::new(self))
    }

    /// Serialize config to TOML string (excludes admin_password_hash for security).
    pub fn to_toml_string(&self) -> Result<String, ConfigError> {
        // Clone and strip the admin hash before serializing
        let mut export = self.clone();
        export.admin_password_hash = None;
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
}
