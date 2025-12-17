//! Configuration for DeltaGlider Proxy S3 server

use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::path::PathBuf;

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

    /// Default bucket name (single-bucket mode)
    #[serde(default = "default_bucket")]
    pub default_bucket: String,
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

        /// S3 bucket name for storing DeltaGlider objects
        bucket: String,

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
    "127.0.0.1:9000".parse().unwrap()
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

fn default_bucket() -> String {
    "default".to_string()
}

fn default_region() -> String {
    "us-east-1".to_string()
}

fn default_force_path_style() -> bool {
    true
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
            default_bucket: default_bucket(),
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

    /// Load configuration from environment variables (legacy support)
    pub fn from_env() -> Self {
        let mut config = Self::default();

        if let Ok(addr) = std::env::var("DELTAGLIDER_PROXY_LISTEN_ADDR") {
            if let Ok(parsed) = addr.parse() {
                config.listen_addr = parsed;
            }
        }

        // Check for S3 backend configuration
        if let Ok(bucket) = std::env::var("DELTAGLIDER_PROXY_S3_BUCKET") {
            config.backend = BackendConfig::S3 {
                endpoint: std::env::var("DELTAGLIDER_PROXY_S3_ENDPOINT").ok(),
                bucket,
                region: std::env::var("DELTAGLIDER_PROXY_S3_REGION")
                    .unwrap_or_else(|_| "us-east-1".to_string()),
                force_path_style: std::env::var("DELTAGLIDER_PROXY_S3_FORCE_PATH_STYLE")
                    .map(|v| v == "true" || v == "1")
                    .unwrap_or(true),
                access_key_id: std::env::var("AWS_ACCESS_KEY_ID").ok(),
                secret_access_key: std::env::var("AWS_SECRET_ACCESS_KEY").ok(),
            };
        } else if let Ok(dir) = std::env::var("DELTAGLIDER_PROXY_DATA_DIR") {
            config.backend = BackendConfig::Filesystem {
                path: PathBuf::from(dir),
            };
        }

        if let Ok(ratio) = std::env::var("DELTAGLIDER_PROXY_MAX_DELTA_RATIO") {
            if let Ok(parsed) = ratio.parse() {
                config.max_delta_ratio = parsed;
            }
        }

        if let Ok(size) = std::env::var("DELTAGLIDER_PROXY_MAX_OBJECT_SIZE") {
            if let Ok(parsed) = size.parse() {
                config.max_object_size = parsed;
            }
        }

        if let Ok(cache) = std::env::var("DELTAGLIDER_PROXY_CACHE_SIZE_MB") {
            if let Ok(parsed) = cache.parse() {
                config.cache_size_mb = parsed;
            }
        }

        if let Ok(bucket) = std::env::var("DELTAGLIDER_PROXY_DEFAULT_BUCKET") {
            config.default_bucket = bucket;
        }

        config
    }

    /// Load configuration from file if it exists, otherwise from environment
    pub fn load() -> Self {
        // Try config file first
        if let Ok(path) = std::env::var("DELTAGLIDER_PROXY_CONFIG") {
            if let Ok(config) = Self::from_file(&path) {
                return config;
            }
        }

        // Try default config file locations
        for path in &["deltaglider_proxy.toml", "/etc/deltaglider_proxy/config.toml"] {
            if std::path::Path::new(path).exists() {
                if let Ok(config) = Self::from_file(path) {
                    return config;
                }
            }
        }

        // Fall back to environment variables
        Self::from_env()
    }

    /// Get the data directory (for filesystem backend compatibility)
    pub fn data_dir(&self) -> PathBuf {
        match &self.backend {
            BackendConfig::Filesystem { path } => path.clone(),
            BackendConfig::S3 { .. } => PathBuf::from("/tmp/deltaglider_proxy-cache"),
        }
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
            bucket = "deltaglider-data"
            region = "us-east-1"
            force_path_style = true
        "#;

        let config: Config = toml::from_str(toml).unwrap();

        match config.backend {
            BackendConfig::S3 {
                endpoint,
                bucket,
                region,
                force_path_style,
                ..
            } => {
                assert_eq!(endpoint, Some("http://localhost:9000".to_string()));
                assert_eq!(bucket, "deltaglider-data");
                assert_eq!(region, "us-east-1");
                assert!(force_path_style);
            }
            _ => panic!("Expected S3 backend"),
        }
    }
}
