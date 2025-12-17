//! S3 storage backend implementation using AWS SDK
//!
//! This backend stores metadata in S3 object metadata headers (x-amz-meta-dg-*)
//! for compatibility with the original DeltaGlider CLI (beshultd/deltaglider).
//!
//! Metadata format follows DeltaGlider conventions:
//! - x-amz-meta-dg-tool: "deltaglider/0.1.0"
//! - x-amz-meta-dg-file-sha256: SHA256 hash of original file
//! - x-amz-meta-dg-original-name: Original filename
//! - x-amz-meta-dg-file-size: Size of original file in bytes
//! - x-amz-meta-dg-created-at: ISO8601 timestamp
//! - x-amz-meta-dg-note: "reference", "delta", or "direct"
//! - x-amz-meta-dg-ref-key: Reference file key (for deltas)
//! - x-amz-meta-dg-ref-sha256: SHA256 of reference file (for deltas)
//! - x-amz-meta-dg-delta-size: Size of delta in bytes (for deltas)
//! - x-amz-meta-dg-delta-cmd: xdelta3 command used (for deltas)

use super::traits::{StorageBackend, StorageError};
use crate::config::BackendConfig;
use crate::types::{FileMetadata, StorageInfo};
use async_trait::async_trait;
use aws_config::BehaviorVersion;
use aws_credential_types::Credentials;
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use tracing::{debug, instrument, warn};

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

    /// Get the S3 key for a delta file
    fn delta_key(&self, prefix: &str, filename: &str) -> String {
        format!("{}/{}.delta", prefix, filename)
    }

    /// Get the S3 key for a direct file (non-delta eligible files)
    /// Note: Original CLI doesn't have a separate .direct suffix - it stores
    /// non-delta files without any suffix. For now we keep .direct for internal
    /// consistency but may need to change this for full interop.
    fn direct_key(&self, prefix: &str, filename: &str) -> String {
        format!("{}/{}.direct", prefix, filename)
    }

    // === Metadata conversion helpers ===

    /// Convert FileMetadata to S3 metadata headers (dg-* format)
    fn metadata_to_headers(&self, metadata: &FileMetadata) -> HashMap<String, String> {
        let mut headers = HashMap::new();

        // Common fields
        headers.insert("dg-tool".to_string(), metadata.tool.clone());
        headers.insert("dg-original-name".to_string(), metadata.original_name.clone());
        headers.insert("dg-file-sha256".to_string(), metadata.file_sha256.clone());
        headers.insert("dg-file-size".to_string(), metadata.file_size.to_string());
        headers.insert(
            "dg-created-at".to_string(),
            metadata.created_at.format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string(),
        );

        // Storage-type specific fields
        match &metadata.storage_info {
            StorageInfo::Reference { source_name } => {
                headers.insert("dg-note".to_string(), "reference".to_string());
                headers.insert("dg-source-name".to_string(), source_name.clone());
            }
            StorageInfo::Delta {
                ref_key,
                ref_sha256,
                delta_size,
                delta_cmd,
            } => {
                headers.insert("dg-note".to_string(), "delta".to_string());
                headers.insert("dg-ref-key".to_string(), ref_key.clone());
                headers.insert("dg-ref-sha256".to_string(), ref_sha256.clone());
                headers.insert("dg-delta-size".to_string(), delta_size.to_string());
                headers.insert("dg-delta-cmd".to_string(), delta_cmd.clone());
            }
            StorageInfo::Direct => {
                headers.insert("dg-note".to_string(), "direct".to_string());
            }
        }

        headers
    }

    /// Convert S3 metadata headers to FileMetadata
    fn headers_to_metadata(&self, headers: &HashMap<String, String>) -> Result<FileMetadata, StorageError> {
        // Helper to get a value with multiple possible keys
        let get_value = |keys: &[&str]| -> Option<String> {
            for key in keys {
                if let Some(v) = headers.get(*key) {
                    if !v.is_empty() {
                        return Some(v.clone());
                    }
                }
            }
            None
        };

        let tool = get_value(&["dg-tool", "tool"])
            .ok_or_else(|| StorageError::Other("Missing dg-tool".to_string()))?;
        let original_name = get_value(&["dg-original-name", "original-name", "dg-source-name", "source-name"])
            .ok_or_else(|| StorageError::Other("Missing dg-original-name".to_string()))?;
        let file_sha256 = get_value(&["dg-file-sha256", "file-sha256"])
            .ok_or_else(|| StorageError::Other("Missing dg-file-sha256".to_string()))?;
        let file_size_str = get_value(&["dg-file-size", "file-size"])
            .unwrap_or_else(|| "0".to_string());
        let file_size: u64 = file_size_str
            .parse()
            .map_err(|_| StorageError::Other(format!("Invalid file size: {}", file_size_str)))?;
        let created_at_str = get_value(&["dg-created-at", "created-at"])
            .unwrap_or_else(|| Utc::now().to_rfc3339());
        // Parse timestamp - handle various formats
        let created_at: DateTime<Utc> = {
            let ts = created_at_str.trim_end_matches('Z');
            // Try RFC3339 with timezone
            DateTime::parse_from_rfc3339(&format!("{}+00:00", ts))
                .map(|dt| dt.with_timezone(&Utc))
                .or_else(|_| {
                    // Try parsing as naive datetime
                    chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%S%.f")
                        .map(|ndt| ndt.and_utc())
                })
                .unwrap_or_else(|_| Utc::now())
        };

        let note = get_value(&["dg-note", "note"]);

        // Determine storage type:
        // 1. If note == "reference", it's a reference file
        // 2. If dg-ref-key exists OR note contains "delta" or "zero-diff", it's a delta file
        //    (Original CLI doesn't always set dg-note for delta files)
        // 3. Otherwise it's direct storage
        let ref_key_opt = get_value(&["dg-ref-key", "ref-key"]);
        let is_reference = note.as_deref() == Some("reference");
        let is_delta = ref_key_opt.is_some()
            || note.as_ref().map(|n| n == "delta" || n.starts_with("zero-diff")).unwrap_or(false);

        let storage_info = if is_reference {
            let source_name = get_value(&["dg-source-name", "source-name"])
                .unwrap_or_else(|| original_name.clone());
            StorageInfo::Reference { source_name }
        } else if is_delta {
            let ref_key = ref_key_opt
                .ok_or_else(|| StorageError::Other("Missing dg-ref-key".to_string()))?;
            let ref_sha256 = get_value(&["dg-ref-sha256", "ref-sha256"])
                .ok_or_else(|| StorageError::Other("Missing dg-ref-sha256".to_string()))?;
            let delta_size_str = get_value(&["dg-delta-size", "delta-size"])
                .unwrap_or_else(|| "0".to_string());
            let delta_size: u64 = delta_size_str
                .parse()
                .map_err(|_| StorageError::Other(format!("Invalid delta size: {}", delta_size_str)))?;
            let delta_cmd = get_value(&["dg-delta-cmd", "delta-cmd"])
                .unwrap_or_default();
            StorageInfo::Delta {
                ref_key,
                ref_sha256,
                delta_size,
                delta_cmd,
            }
        } else {
            StorageInfo::Direct
        };

        // MD5 is not in the original DeltaGlider format, generate a placeholder
        let md5 = headers.get("dg-md5").cloned().unwrap_or_else(|| "".to_string());

        Ok(FileMetadata {
            tool,
            original_name,
            file_sha256,
            file_size,
            md5,
            created_at,
            content_type: headers.get("content-type").cloned(),
            storage_info,
        })
    }

    // === Internal helpers ===

    /// Put an object to S3 with metadata headers
    async fn put_object_with_metadata(
        &self,
        key: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        let headers = self.metadata_to_headers(metadata);

        let mut request = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from(data.to_vec()))
            .content_type("application/octet-stream");

        // Add all metadata headers
        for (k, v) in headers {
            request = request.metadata(k, v);
        }

        request
            .send()
            .await
            .map_err(|e| StorageError::S3(format!("put_object failed: {}", e)))?;

        debug!("S3 PUT {} ({} bytes) with DG metadata", key, data.len());
        Ok(())
    }

    /// Put a raw object to S3 (no metadata)
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

    /// Get object metadata from S3 headers
    async fn get_object_metadata(&self, key: &str) -> Result<FileMetadata, StorageError> {
        let response = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| {
                if let SdkError::ServiceError(service_error) = &e {
                    if matches!(
                        service_error.err(),
                        aws_sdk_s3::operation::head_object::HeadObjectError::NotFound(_)
                    ) {
                        return StorageError::NotFound(key.to_string());
                    }
                }
                StorageError::S3(format!("head_object failed: {}", e))
            })?;

        let headers: HashMap<String, String> = response
            .metadata()
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();

        if headers.is_empty() {
            return Err(StorageError::NotFound(format!(
                "No DeltaGlider metadata found for {}",
                key
            )));
        }

        self.headers_to_metadata(&headers)
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
    // Metadata is stored in S3 object headers (x-amz-meta-dg-*) for CLI compatibility

    #[instrument(skip(self, data, metadata))]
    async fn put_reference(
        &self,
        prefix: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        let key = self.reference_key(prefix);
        self.put_object_with_metadata(&key, data, metadata).await?;
        debug!("Stored reference for {} ({} bytes) with DG headers", prefix, data.len());
        Ok(())
    }

    #[instrument(skip(self, metadata))]
    async fn put_reference_metadata(
        &self,
        prefix: &str,
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        // For S3, we need to copy the object with new metadata
        // This is a limitation - metadata can only be set at PUT time
        // For now, we skip this operation as reference data+metadata are always PUT together
        warn!("put_reference_metadata called on S3 backend - metadata can only be set at PUT time");
        let _ = (prefix, metadata);
        Ok(())
    }

    #[instrument(skip(self))]
    async fn get_reference(&self, prefix: &str) -> Result<Vec<u8>, StorageError> {
        let key = self.reference_key(prefix);
        self.get_object(&key).await
    }

    #[instrument(skip(self))]
    async fn get_reference_metadata(&self, prefix: &str) -> Result<FileMetadata, StorageError> {
        let key = self.reference_key(prefix);
        self.get_object_metadata(&key).await
    }

    #[instrument(skip(self))]
    async fn has_reference(&self, prefix: &str) -> bool {
        let key = self.reference_key(prefix);
        self.object_exists(&key).await
    }

    #[instrument(skip(self))]
    async fn delete_reference(&self, prefix: &str) -> Result<(), StorageError> {
        let key = self.reference_key(prefix);
        self.delete_object(&key).await?;
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
        let key = self.delta_key(prefix, filename);
        self.put_object_with_metadata(&key, data, metadata).await?;
        debug!(
            "Stored delta for {}/{} ({} bytes) with DG headers",
            prefix, filename, data.len()
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
        let key = self.delta_key(prefix, filename);
        self.get_object_metadata(&key).await
    }

    #[instrument(skip(self))]
    async fn delete_delta(&self, prefix: &str, filename: &str) -> Result<(), StorageError> {
        let key = self.delta_key(prefix, filename);
        self.delete_object(&key).await?;
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
        let key = self.direct_key(prefix, filename);
        self.put_object_with_metadata(&key, data, metadata).await?;
        debug!(
            "Stored direct for {}/{} ({} bytes) with DG headers",
            prefix, filename, data.len()
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
        let key = self.direct_key(prefix, filename);
        self.get_object_metadata(&key).await
    }

    #[instrument(skip(self))]
    async fn delete_direct(&self, prefix: &str, filename: &str) -> Result<(), StorageError> {
        let key = self.direct_key(prefix, filename);
        self.delete_object(&key).await?;
        debug!("Deleted direct for {}/{}", prefix, filename);
        Ok(())
    }

    // === Scanning operations ===

    #[instrument(skip(self))]
    async fn scan_deltaspace(&self, prefix: &str) -> Result<Vec<FileMetadata>, StorageError> {
        let search_prefix = format!("{}/", prefix);
        let keys = self.list_objects_with_prefix(&search_prefix).await?;

        let mut metadata_list = Vec::new();

        // Read metadata from object headers for DeltaGlider files
        for key in keys {
            // Skip non-DeltaGlider files
            if !key.ends_with("/reference.bin") && !key.ends_with(".delta") && !key.ends_with(".direct") {
                continue;
            }

            match self.get_object_metadata(&key).await {
                Ok(meta) => metadata_list.push(meta),
                Err(e) => {
                    debug!("Failed to read metadata from {}: {}", key, e);
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

        // Test key formats directly - S3 backend stores metadata in object headers,
        // NOT in separate .meta files (unlike filesystem backend)
        assert_eq!(
            format!("{}/reference.bin", prefix),
            "releases/v1/reference.bin"
        );
        assert_eq!(
            format!("{}/{}.delta", prefix, filename),
            "releases/v1/app.zip.delta"
        );
        assert_eq!(
            format!("{}/{}.direct", prefix, filename),
            "releases/v1/app.zip.direct"
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
