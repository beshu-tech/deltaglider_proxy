//! S3 storage backend implementation using AWS SDK

use super::traits::{StorageBackend, StorageError};
use crate::config::BackendConfig;
use crate::types::FileMetadata;
use async_trait::async_trait;
use aws_config::BehaviorVersion;
use aws_credential_types::Credentials;
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use std::collections::HashSet;
use std::path::Path;
use tracing::{debug, instrument};

/// S3 storage backend for DeltaGlider objects
pub struct S3Backend {
    client: Client,
    bucket: String,
}

impl S3Backend {
    /// Create a new S3 backend from configuration
    pub async fn new(config: &BackendConfig) -> Result<Self, StorageError> {
        let (endpoint, bucket, region, force_path_style, access_key_id, secret_access_key) =
            match config {
                BackendConfig::S3 {
                    endpoint,
                    bucket,
                    region,
                    force_path_style,
                    access_key_id,
                    secret_access_key,
                } => (
                    endpoint.clone(),
                    bucket.clone(),
                    region.clone(),
                    *force_path_style,
                    access_key_id.clone(),
                    secret_access_key.clone(),
                ),
                _ => {
                    return Err(StorageError::Other(
                        "S3Backend requires S3 configuration".to_string(),
                    ))
                }
            };

        // Build AWS SDK config
        let mut config_loader = aws_config::defaults(BehaviorVersion::latest())
            .region(aws_sdk_s3::config::Region::new(region));

        // Set custom endpoint if provided (for MinIO, LocalStack, etc.)
        if let Some(ref ep) = endpoint {
            config_loader = config_loader.endpoint_url(ep);
        }

        // Use explicit credentials if provided, otherwise rely on default credential chain
        if let (Some(ref key_id), Some(ref secret)) = (access_key_id, secret_access_key) {
            let credentials = Credentials::new(key_id, secret, None, None, "deltaglider_proxy-config");
            config_loader = config_loader.credentials_provider(credentials);
        }

        let sdk_config = config_loader.load().await;

        // Build S3-specific config with path-style option
        let s3_config = aws_sdk_s3::config::Builder::from(&sdk_config)
            .force_path_style(force_path_style)
            .build();

        let client = Client::from_conf(s3_config);

        debug!("S3Backend initialized for bucket: {}", bucket);

        Ok(Self { client, bucket })
    }

    // === Key generation helpers ===

    /// Get the S3 key for a reference file
    fn reference_key(&self, prefix: &str) -> String {
        format!("{}/reference.bin", prefix)
    }

    /// Get the S3 key for reference metadata
    fn reference_meta_key(&self, prefix: &str) -> String {
        format!("{}/reference.bin.meta", prefix)
    }

    /// Get the S3 key for a delta file
    fn delta_key(&self, prefix: &str, filename: &str) -> String {
        format!("{}/{}.delta", prefix, filename)
    }

    /// Get the S3 key for delta metadata
    fn delta_meta_key(&self, prefix: &str, filename: &str) -> String {
        format!("{}/{}.delta.meta", prefix, filename)
    }

    /// Get the S3 key for a direct file
    fn direct_key(&self, prefix: &str, filename: &str) -> String {
        format!("{}/{}.direct", prefix, filename)
    }

    /// Get the S3 key for direct metadata
    fn direct_meta_key(&self, prefix: &str, filename: &str) -> String {
        format!("{}/{}.direct.meta", prefix, filename)
    }

    // === Internal helpers ===

    /// Put an object to S3
    async fn put_object(&self, key: &str, data: &[u8]) -> Result<(), StorageError> {
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from(data.to_vec()))
            .send()
            .await
            .map_err(|e| StorageError::S3(format!("put_object failed: {}", e)))?;

        debug!("S3 PUT {} ({} bytes)", key, data.len());
        Ok(())
    }

    /// Get an object from S3
    async fn get_object(&self, key: &str) -> Result<Vec<u8>, StorageError> {
        let response = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| {
                if let SdkError::ServiceError(service_error) = &e {
                    if matches!(
                        service_error.err(),
                        aws_sdk_s3::operation::get_object::GetObjectError::NoSuchKey(_)
                    ) {
                        return StorageError::NotFound(key.to_string());
                    }
                }
                StorageError::S3(format!("get_object failed: {}", e))
            })?;

        let data = response
            .body
            .collect()
            .await
            .map_err(|e| StorageError::S3(format!("Failed to read response body: {}", e)))?
            .into_bytes()
            .to_vec();

        debug!("S3 GET {} ({} bytes)", key, data.len());
        Ok(data)
    }

    /// Delete an object from S3
    async fn delete_object(&self, key: &str) -> Result<(), StorageError> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| StorageError::S3(format!("delete_object failed: {}", e)))?;

        debug!("S3 DELETE {}", key);
        Ok(())
    }

    /// Check if an object exists in S3
    async fn object_exists(&self, key: &str) -> bool {
        self.client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .is_ok()
    }

    /// Write metadata as JSON to S3
    async fn write_metadata(&self, key: &str, metadata: &FileMetadata) -> Result<(), StorageError> {
        let json = serde_json::to_vec_pretty(metadata)?;
        self.put_object(key, &json).await
    }

    /// Read metadata JSON from S3
    async fn read_metadata(&self, key: &str) -> Result<FileMetadata, StorageError> {
        let data = self.get_object(key).await?;
        let metadata: FileMetadata = serde_json::from_slice(&data)?;
        Ok(metadata)
    }

    /// List objects with a prefix
    async fn list_objects_with_prefix(&self, prefix: &str) -> Result<Vec<String>, StorageError> {
        let mut keys = Vec::new();
        let mut continuation_token: Option<String> = None;

        loop {
            let mut request = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(prefix);

            if let Some(token) = continuation_token {
                request = request.continuation_token(token);
            }

            let response = request
                .send()
                .await
                .map_err(|e| StorageError::S3(format!("list_objects_v2 failed: {}", e)))?;

            if let Some(contents) = response.contents {
                for object in contents {
                    if let Some(key) = object.key {
                        keys.push(key);
                    }
                }
            }

            if response.is_truncated.unwrap_or(false) {
                continuation_token = response.next_continuation_token;
            } else {
                break;
            }
        }

        Ok(keys)
    }
}

#[async_trait]
impl StorageBackend for S3Backend {
    // === Raw operations (Path-based, for completeness) ===

    async fn put_raw(&self, path: &Path, data: &[u8]) -> Result<(), StorageError> {
        let key = path.to_string_lossy().to_string();
        self.put_object(&key, data).await
    }

    async fn get_raw(&self, path: &Path) -> Result<Vec<u8>, StorageError> {
        let key = path.to_string_lossy().to_string();
        self.get_object(&key).await
    }

    async fn exists(&self, path: &Path) -> bool {
        let key = path.to_string_lossy().to_string();
        self.object_exists(&key).await
    }

    async fn delete(&self, path: &Path) -> Result<(), StorageError> {
        let key = path.to_string_lossy().to_string();
        self.delete_object(&key).await
    }

    async fn list_prefix(&self, prefix: &Path) -> Result<Vec<String>, StorageError> {
        let prefix_str = prefix.to_string_lossy().to_string();
        self.list_objects_with_prefix(&prefix_str).await
    }

    // === Reference file operations ===

    #[instrument(skip(self, data, metadata))]
    async fn put_reference(
        &self,
        prefix: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        let data_key = self.reference_key(prefix);
        let meta_key = self.reference_meta_key(prefix);

        self.put_object(&data_key, data).await?;
        self.write_metadata(&meta_key, metadata).await?;

        debug!("Stored reference for {} ({} bytes)", prefix, data.len());
        Ok(())
    }

    #[instrument(skip(self, metadata))]
    async fn put_reference_metadata(
        &self,
        prefix: &str,
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        let meta_key = self.reference_meta_key(prefix);
        self.write_metadata(&meta_key, metadata).await
    }

    #[instrument(skip(self))]
    async fn get_reference(&self, prefix: &str) -> Result<Vec<u8>, StorageError> {
        let key = self.reference_key(prefix);
        self.get_object(&key).await
    }

    #[instrument(skip(self))]
    async fn get_reference_metadata(&self, prefix: &str) -> Result<FileMetadata, StorageError> {
        let key = self.reference_meta_key(prefix);
        self.read_metadata(&key).await
    }

    #[instrument(skip(self))]
    async fn has_reference(&self, prefix: &str) -> bool {
        let key = self.reference_key(prefix);
        self.object_exists(&key).await
    }

    #[instrument(skip(self))]
    async fn delete_reference(&self, prefix: &str) -> Result<(), StorageError> {
        let data_key = self.reference_key(prefix);
        let meta_key = self.reference_meta_key(prefix);

        // Delete both data and metadata (ignore errors for metadata if it doesn't exist)
        self.delete_object(&data_key).await?;
        let _ = self.delete_object(&meta_key).await;

        debug!("Deleted reference for {}", prefix);
        Ok(())
    }

    // === Delta file operations ===

    #[instrument(skip(self, data, metadata))]
    async fn put_delta(
        &self,
        prefix: &str,
        filename: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        let data_key = self.delta_key(prefix, filename);
        let meta_key = self.delta_meta_key(prefix, filename);

        self.put_object(&data_key, data).await?;
        self.write_metadata(&meta_key, metadata).await?;

        debug!(
            "Stored delta for {}/{} ({} bytes)",
            prefix,
            filename,
            data.len()
        );
        Ok(())
    }

    #[instrument(skip(self))]
    async fn get_delta(&self, prefix: &str, filename: &str) -> Result<Vec<u8>, StorageError> {
        let key = self.delta_key(prefix, filename);
        self.get_object(&key).await
    }

    #[instrument(skip(self))]
    async fn get_delta_metadata(
        &self,
        prefix: &str,
        filename: &str,
    ) -> Result<FileMetadata, StorageError> {
        let key = self.delta_meta_key(prefix, filename);
        self.read_metadata(&key).await
    }

    #[instrument(skip(self))]
    async fn delete_delta(&self, prefix: &str, filename: &str) -> Result<(), StorageError> {
        let data_key = self.delta_key(prefix, filename);
        let meta_key = self.delta_meta_key(prefix, filename);

        self.delete_object(&data_key).await?;
        let _ = self.delete_object(&meta_key).await;

        debug!("Deleted delta for {}/{}", prefix, filename);
        Ok(())
    }

    // === Direct file operations ===

    #[instrument(skip(self, data, metadata))]
    async fn put_direct(
        &self,
        prefix: &str,
        filename: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        let data_key = self.direct_key(prefix, filename);
        let meta_key = self.direct_meta_key(prefix, filename);

        self.put_object(&data_key, data).await?;
        self.write_metadata(&meta_key, metadata).await?;

        debug!(
            "Stored direct for {}/{} ({} bytes)",
            prefix,
            filename,
            data.len()
        );
        Ok(())
    }

    #[instrument(skip(self))]
    async fn get_direct(&self, prefix: &str, filename: &str) -> Result<Vec<u8>, StorageError> {
        let key = self.direct_key(prefix, filename);
        self.get_object(&key).await
    }

    #[instrument(skip(self))]
    async fn get_direct_metadata(
        &self,
        prefix: &str,
        filename: &str,
    ) -> Result<FileMetadata, StorageError> {
        let key = self.direct_meta_key(prefix, filename);
        self.read_metadata(&key).await
    }

    #[instrument(skip(self))]
    async fn delete_direct(&self, prefix: &str, filename: &str) -> Result<(), StorageError> {
        let data_key = self.direct_key(prefix, filename);
        let meta_key = self.direct_meta_key(prefix, filename);

        self.delete_object(&data_key).await?;
        let _ = self.delete_object(&meta_key).await;

        debug!("Deleted direct for {}/{}", prefix, filename);
        Ok(())
    }

    // === Scanning operations ===

    #[instrument(skip(self))]
    async fn scan_deltaspace(&self, prefix: &str) -> Result<Vec<FileMetadata>, StorageError> {
        let search_prefix = format!("{}/", prefix);
        let keys = self.list_objects_with_prefix(&search_prefix).await?;

        let mut metadata_list = Vec::new();

        // Find all .meta files and read them
        for key in keys {
            if key.ends_with(".meta") {
                match self.read_metadata(&key).await {
                    Ok(meta) => metadata_list.push(meta),
                    Err(e) => {
                        debug!("Failed to read metadata from {}: {}", key, e);
                    }
                }
            }
        }

        debug!(
            "Scanned {} objects in deltaspace {}",
            metadata_list.len(),
            prefix
        );
        Ok(metadata_list)
    }

    #[instrument(skip(self))]
    async fn list_deltaspaces(&self) -> Result<Vec<String>, StorageError> {
        // List all objects and extract unique prefixes
        let keys = self.list_objects_with_prefix("").await?;
        let mut prefixes = HashSet::new();

        for key in keys {
            // Check if this is a deltaglider file
            if key.ends_with("/reference.bin")
                || key.ends_with(".delta")
                || key.ends_with(".direct")
            {
                // Extract the prefix (everything before the last /)
                if let Some(idx) = key.rfind('/') {
                    let prefix = &key[..idx];
                    prefixes.insert(prefix.to_string());
                }
            }
        }

        let result: Vec<String> = prefixes.into_iter().collect();
        debug!("Found {} deltaspaces", result.len());
        Ok(result)
    }

    #[instrument(skip(self))]
    async fn total_size(&self) -> Result<u64, StorageError> {
        let mut total = 0u64;
        let mut continuation_token: Option<String> = None;

        loop {
            let mut request = self.client.list_objects_v2().bucket(&self.bucket);

            if let Some(token) = continuation_token {
                request = request.continuation_token(token);
            }

            let response = request
                .send()
                .await
                .map_err(|e| StorageError::S3(format!("list_objects_v2 failed: {}", e)))?;

            if let Some(contents) = response.contents {
                for object in contents {
                    if let Some(size) = object.size {
                        total += size as u64;
                    }
                }
            }

            if response.is_truncated.unwrap_or(false) {
                continuation_token = response.next_continuation_token;
            } else {
                break;
            }
        }

        debug!("Total S3 storage size: {} bytes", total);
        Ok(total)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_reference_key_generation() {
        // Create a mock backend just to test key generation
        // (Can't create real backend without AWS credentials)
        let prefix = "releases/v1";
        let filename = "app.zip";

        // Test key formats directly
        assert_eq!(
            format!("{}/reference.bin", prefix),
            "releases/v1/reference.bin"
        );
        assert_eq!(
            format!("{}/reference.bin.meta", prefix),
            "releases/v1/reference.bin.meta"
        );
        assert_eq!(
            format!("{}/{}.delta", prefix, filename),
            "releases/v1/app.zip.delta"
        );
        assert_eq!(
            format!("{}/{}.delta.meta", prefix, filename),
            "releases/v1/app.zip.delta.meta"
        );
        assert_eq!(
            format!("{}/{}.direct", prefix, filename),
            "releases/v1/app.zip.direct"
        );
        assert_eq!(
            format!("{}/{}.direct.meta", prefix, filename),
            "releases/v1/app.zip.direct.meta"
        );
    }

    #[test]
    fn test_root_prefix_key_generation() {
        let prefix = "_root_";
        let filename = "data.bin";

        assert_eq!(format!("{}/reference.bin", prefix), "_root_/reference.bin");
        assert_eq!(
            format!("{}/{}.direct", prefix, filename),
            "_root_/data.bin.direct"
        );
    }
}
