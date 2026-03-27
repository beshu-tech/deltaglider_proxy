//! S3 sync for the IAM config database.
//!
//! When `DGP_CONFIG_SYNC_BUCKET` is set, the encrypted config DB file is
//! synchronized to/from S3 at `.deltaglider/config.db`. This enables
//! multi-instance deployments to share IAM state.
//!
//! - On startup: download from S3 if the ETag differs from the local copy.
//! - After IAM mutations: upload the local DB to S3.
//! - Every 5 minutes: poll S3 ETag and download if changed.

use aws_credential_types::Credentials;
use aws_sdk_s3::config::BehaviorVersion;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info};

use crate::config::BackendConfig;
use crate::config_db::ConfigDb;

/// S3 key for the config database file.
const S3_CONFIG_KEY: &str = ".deltaglider/config.db";

/// Synchronizes the encrypted config DB file to/from S3.
pub struct ConfigDbSync {
    s3_client: Client,
    bucket: String,
    local_path: PathBuf,
    last_etag: Arc<RwLock<Option<String>>>,
    /// The local bootstrap password hash, used to validate downloaded DBs.
    bootstrap_password_hash: String,
}

impl ConfigDbSync {
    /// Create a new sync instance from the backend config and sync bucket name.
    ///
    /// Uses the same S3 credentials as the storage backend (DGP_BE_AWS_ACCESS_KEY_ID etc).
    /// Returns `None` if the backend is not S3 or credentials are missing.
    pub async fn new(
        backend_config: &BackendConfig,
        sync_bucket: String,
        local_path: PathBuf,
        bootstrap_password_hash: String,
    ) -> Result<Self, String> {
        let client = Self::build_client(backend_config).await?;

        // Clean up orphaned .db.tmp files from previous interrupted downloads
        let tmp_path = local_path.with_extension("db.tmp");
        if tmp_path.exists() {
            let _ = std::fs::remove_file(&tmp_path);
        }

        Ok(Self {
            s3_client: client,
            bucket: sync_bucket,
            local_path,
            last_etag: Arc::new(RwLock::new(None)),
            bootstrap_password_hash,
        })
    }

    /// Build an S3 client from BackendConfig, reusing the same credentials.
    async fn build_client(config: &BackendConfig) -> Result<Client, String> {
        let (endpoint, region, force_path_style, access_key_id, secret_access_key) = match config {
            BackendConfig::S3 {
                endpoint,
                region,
                force_path_style,
                access_key_id,
                secret_access_key,
                ..
            } => (
                endpoint.clone(),
                region.clone(),
                *force_path_style,
                access_key_id.clone(),
                secret_access_key.clone(),
            ),
            BackendConfig::Filesystem { .. } => {
                return Err("Config DB S3 sync requires an S3 backend. \
                     Set DGP_CONFIG_SYNC_BUCKET only when using the S3 backend."
                    .to_string());
            }
        };

        let credentials = match (access_key_id, secret_access_key) {
            (Some(ref key_id), Some(ref secret)) => {
                Credentials::new(key_id, secret, None, None, "deltaglider_proxy-config-sync")
            }
            _ => {
                return Err("Config DB S3 sync requires backend S3 credentials \
                     (DGP_BE_AWS_ACCESS_KEY_ID and DGP_BE_AWS_SECRET_ACCESS_KEY)"
                    .to_string());
            }
        };

        let mut builder = aws_sdk_s3::config::Builder::new()
            .behavior_version(BehaviorVersion::latest())
            .region(aws_sdk_s3::config::Region::new(region))
            .credentials_provider(credentials)
            .force_path_style(force_path_style)
            .request_checksum_calculation(
                aws_sdk_s3::config::RequestChecksumCalculation::WhenRequired,
            )
            .response_checksum_validation(
                aws_sdk_s3::config::ResponseChecksumValidation::WhenRequired,
            );

        if let Some(ref ep) = endpoint {
            builder = builder.endpoint_url(ep);
        }

        Ok(Client::from_conf(builder.build()))
    }

    /// Check S3 for a newer config DB file and download it if the ETag differs.
    ///
    /// Returns `true` if a new version was downloaded (caller should reopen the DB).
    pub async fn download_if_newer(&self) -> Result<bool, String> {
        // HEAD to get current ETag
        let head_result = self
            .s3_client
            .head_object()
            .bucket(&self.bucket)
            .key(S3_CONFIG_KEY)
            .send()
            .await;

        let remote_etag = match head_result {
            Ok(head) => head.e_tag().map(|s| s.to_string()),
            Err(e) => {
                let err_str = format!("{}", e);
                if err_str.contains("404")
                    || err_str.contains("NoSuchKey")
                    || err_str.contains("Not Found")
                {
                    debug!(
                        "Config DB not found in S3 (bucket={}) — using local copy",
                        self.bucket
                    );
                    return Ok(false);
                }
                return Err(format!("Failed to HEAD config DB in S3: {}", e));
            }
        };

        // Compare with our last known ETag
        let current_etag = self.last_etag.read().await;
        if *current_etag == remote_etag {
            debug!("Config DB S3 ETag unchanged — no download needed");
            return Ok(false);
        }
        drop(current_etag);

        // Download the file
        let get_result = self
            .s3_client
            .get_object()
            .bucket(&self.bucket)
            .key(S3_CONFIG_KEY)
            .send()
            .await
            .map_err(|e| format!("Failed to download config DB from S3: {}", e))?;

        let body = get_result
            .body
            .collect()
            .await
            .map_err(|e| format!("Failed to read config DB body from S3: {}", e))?;

        let data = body.into_bytes();
        if data.is_empty() {
            return Err("Downloaded config DB from S3 is empty".to_string());
        }

        // Write to a temp file first, then validate before replacing
        let tmp_path = self.local_path.with_extension("db.tmp");
        tokio::fs::write(&tmp_path, &data)
            .await
            .map_err(|e| format!("Failed to write temp config DB: {}", e))?;

        // Validate we can open the downloaded DB with our local bootstrap password.
        // If the remote DB was encrypted with a different password, we must NOT replace
        // our local copy — it would be unreadable and break IAM.
        match ConfigDb::open_or_create(&tmp_path, &self.bootstrap_password_hash) {
            Ok(_) => {
                debug!("Downloaded config DB passed passphrase validation");
            }
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp_path).await;
                tracing::warn!(
                    "Config DB downloaded from S3 is encrypted with a different bootstrap password — \
                     NOT replacing local copy: {}",
                    e
                );
                return Ok(false);
            }
        }

        tokio::fs::rename(&tmp_path, &self.local_path)
            .await
            .map_err(|e| format!("Failed to rename temp config DB: {}", e))?;

        // Update stored ETag
        *self.last_etag.write().await = remote_etag;

        info!(
            "Config DB downloaded from S3 (bucket={}, size={} bytes)",
            self.bucket,
            data.len()
        );
        Ok(true)
    }

    /// Upload the local config DB file to S3.
    pub async fn upload(&self) -> Result<(), String> {
        let data = tokio::fs::read(&self.local_path)
            .await
            .map_err(|e| format!("Failed to read local config DB: {}", e))?;

        if data.is_empty() {
            return Err("Local config DB is empty — refusing to upload".to_string());
        }

        let put_result = self
            .s3_client
            .put_object()
            .bucket(&self.bucket)
            .key(S3_CONFIG_KEY)
            .body(ByteStream::from(data.clone()))
            .content_type("application/octet-stream")
            .send()
            .await
            .map_err(|e| format!("Failed to upload config DB to S3: {}", e))?;

        // Store the ETag from the PUT response
        if let Some(etag) = put_result.e_tag() {
            *self.last_etag.write().await = Some(etag.to_string());
        }

        info!(
            "Config DB uploaded to S3 (bucket={}, size={} bytes)",
            self.bucket,
            data.len()
        );
        Ok(())
    }

    /// Poll S3 for ETag changes. Called periodically (every 5 minutes).
    /// Returns `true` if a new version was downloaded.
    pub async fn poll_and_sync(&self) -> Result<bool, String> {
        self.download_if_newer().await
    }
}
