// SPDX-License-Identifier: GPL-3.0-only

//! S3 sync for the IAM config database.
//!
//! When `DGP_CONFIG_SYNC_BUCKET` is set, the encrypted config DB file is
//! synchronized to/from S3 (default key `.deltaglider/config.db`, override
//! with `DGP_CONFIG_SYNC_KEY`). This enables
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
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, info, warn};

use crate::config::BackendConfig;
use crate::config_db::ConfigDb;
use crate::iam::external_auth::ExternalAuthManager;
use crate::iam::{IamIndex, IamState, SharedIamState};

/// Default S3 object key for the config database file (override with `DGP_CONFIG_SYNC_KEY`).
pub const DEFAULT_CONFIG_SYNC_OBJECT_KEY: &str = ".deltaglider/config.db";

/// Synchronizes the encrypted config DB file to/from S3.
pub struct ConfigDbSync {
    s3_client: Client,
    bucket: String,
    object_key: String,
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
        object_key: String,
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
            object_key,
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
            .key(&self.object_key)
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
            .key(&self.object_key)
            .send()
            .await
            .map_err(|e| format!("Failed to download config DB from S3: {}", e))?;

        let get_etag = get_result.e_tag().map(|s| s.to_string());
        if get_etag != remote_etag {
            return Err(format!(
                "Config DB changed during download (HEAD etag={:?}, GET etag={:?}); retry later",
                remote_etag, get_etag
            ));
        }

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
    ///
    /// Uses a conditional (compare-and-swap) PUT so two instances mutating
    /// IAM concurrently can't silently clobber each other's writes:
    ///   - if we've previously synced this object (`last_etag` is `Some`),
    ///     send `If-Match: <etag>` so the PUT fails with 412 when a peer
    ///     changed the remote copy since we last saw it;
    ///   - if we've never seen the remote object (`last_etag` is `None`),
    ///     send `If-None-Match: *` so the PUT fails with 412 if a peer
    ///     created it concurrently (instead of overwriting their copy).
    ///
    /// On a precondition failure the upload is reported as a conflict; the
    /// next poll cycle (`download_if_newer`) pulls the peer's version and the
    /// caller can re-apply on top of the reconciled DB.
    pub async fn upload(&self) -> Result<(), String> {
        let data = tokio::fs::read(&self.local_path)
            .await
            .map_err(|e| format!("Failed to read local config DB: {}", e))?;

        if data.is_empty() {
            return Err("Local config DB is empty — refusing to upload".to_string());
        }

        // Snapshot the ETag we expect the remote object to still carry. This is
        // the compare half of the compare-and-swap.
        let expected_etag = self.last_etag.read().await.clone();

        let mut put = self
            .s3_client
            .put_object()
            .bucket(&self.bucket)
            .key(&self.object_key)
            .body(ByteStream::from(data.clone()))
            .content_type("application/octet-stream");
        put = match &expected_etag {
            Some(etag) => put.if_match(etag),
            None => put.if_none_match("*"),
        };

        let put_result = match put.send().await {
            Ok(result) => result,
            Err(e) => {
                let err_str = format!("{}", e);
                if is_precondition_failed(&err_str) {
                    // A peer instance updated the remote config DB since we last
                    // synced. Forget our stale ETag so the next poll forces a
                    // fresh HEAD+GET, then surface the conflict to the caller.
                    *self.last_etag.write().await = None;
                    warn!(
                        "Config DB S3 upload conflict (bucket={}): remote copy changed since last sync \
                         (expected etag={:?}) — a peer instance wrote concurrently; will re-sync on next poll",
                        self.bucket, expected_etag
                    );
                    return Err(format!(
                        "Config DB upload conflict: remote copy changed since last sync \
                         (expected etag={:?}); re-sync and retry",
                        expected_etag
                    ));
                }
                return Err(format!("Failed to upload config DB to S3: {}", e));
            }
        };

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

    /// Download the raw config DB bytes from S3 without passphrase validation.
    /// Used by the recovery endpoint to try candidate passwords against the S3 copy.
    pub async fn download_raw(&self) -> Result<Vec<u8>, String> {
        let get_result = self
            .s3_client
            .get_object()
            .bucket(&self.bucket)
            .key(&self.object_key)
            .send()
            .await
            .map_err(|e| format!("Failed to download config DB from S3: {}", e))?;

        let body = get_result
            .body
            .collect()
            .await
            .map_err(|e| format!("Failed to read config DB body from S3: {}", e))?;

        let data = body.into_bytes().to_vec();
        if data.is_empty() {
            return Err("Config DB in S3 is empty".to_string());
        }

        Ok(data)
    }
}

/// Pure classifier: does this stringified S3 SDK error represent a failed
/// conditional-write precondition (HTTP 412)?
///
/// The conditional PUT in [`ConfigDbSync::upload`] relies on the backend
/// rejecting the request with `412 Precondition Failed` when the `If-Match`
/// / `If-None-Match` guard doesn't hold. AWS S3 and MinIO both surface this
/// as `PreconditionFailed` / a 412 status in the error display string.
/// Extracted as a pure fn so the decision is unit-testable without a live
/// S3 backend (per the project's "pure functions at decision points" rule).
fn is_precondition_failed(err_str: &str) -> bool {
    err_str.contains("PreconditionFailed")
        || err_str.contains("Precondition Failed")
        || err_str.contains("412")
}

/// Reopen the config DB file after an S3-sync download has replaced it
/// on disk, and rebuild the in-memory IAM index from the new content.
///
/// Moved into `config_db_sync` so it can be shared by:
///   - startup sync (`init_config_sync`)
///   - the periodic poll task (`spawn_config_sync_poll`)
///   - the operator-triggered `POST /api/admin/config/sync-now` endpoint
///
/// Previously lived in `src/startup.rs`, which is a binary-only module
/// (not re-exported by `lib.rs`), so the admin handler couldn't reach
/// it. Keeping this function in the library side preserves the "one
/// path for config-sync state application" invariant — any future
/// trigger mounts on top without re-implementing IAM index + external
/// auth rebuild.
///
/// Gracefully no-ops when `config_db` is `None` (legacy/open-access
/// mode, no IAM DB to reopen).
pub async fn reopen_and_rebuild_iam(
    config_db: &Option<Arc<Mutex<ConfigDb>>>,
    admin_password_hash: &str,
    iam_state: &SharedIamState,
    external_auth: &Option<Arc<ExternalAuthManager>>,
    context: &str,
) {
    let Some(db_arc) = config_db else {
        return;
    };
    let mut db = db_arc.lock().await;
    if let Err(e) = db.reopen(admin_password_hash) {
        warn!(
            "Config DB S3 sync ({}): failed to reopen after download: {}",
            context, e
        );
        return;
    }

    // Rebuild IAM index from the new DB
    let users = db.load_users().unwrap_or_default();
    let groups = db.load_groups().unwrap_or_default();
    let count = users.len();
    let group_count = groups.len();
    let state = IamIndex::build_iam_state(users, groups);
    if matches!(&state, IamState::Iam(_)) {
        info!(
            "IAM index rebuilt from S3-synced DB ({} users, {} groups) [{}]",
            count, group_count, context
        );
    }
    iam_state.store(Arc::new(state));

    // Rebuild ExternalAuthManager from the new DB. Release the DB
    // lock before the async discovery round — it can take seconds
    // against real OIDC providers.
    if let Some(ref ext_auth) = external_auth {
        let providers = db.load_auth_providers().unwrap_or_default();
        if !providers.is_empty() {
            ext_auth.rebuild(&providers);
            drop(db);
            ext_auth.discover_all().await;
            info!(
                "External auth providers rebuilt from S3-synced DB ({} providers) [{}]",
                ext_auth.provider_names().len(),
                context
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn precondition_failed_detected_from_common_shapes() {
        // S3-style service error display.
        assert!(is_precondition_failed(
            "service error: PreconditionFailed: At least one of the pre-conditions you specified did not hold"
        ));
        // MinIO / human-readable status text.
        assert!(is_precondition_failed(
            "unhandled error (Precondition Failed)"
        ));
        // Raw HTTP status code.
        assert!(is_precondition_failed(
            "dispatch failure: response status: 412"
        ));
    }

    #[test]
    fn non_precondition_errors_are_not_misclassified() {
        assert!(!is_precondition_failed(
            "dispatch failure: connection refused"
        ));
        assert!(!is_precondition_failed(
            "NoSuchBucket: bucket does not exist"
        ));
        assert!(!is_precondition_failed(
            "service error: AccessDenied (status 403)"
        ));
        assert!(!is_precondition_failed(""));
    }
}
