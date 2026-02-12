//! S3 storage backend implementation using AWS SDK
//!
//! This backend stores metadata in S3 object metadata headers (x-amz-meta-dg-*)
//! for compatibility with the original DeltaGlider CLI (beshultd/deltaglider).
//!
//! Each API bucket maps 1:1 to a real S3 bucket on the backend.

use super::traits::{StorageBackend, StorageError};
use crate::config::BackendConfig;
use crate::types::{FileMetadata, StorageInfo};
use async_trait::async_trait;
use aws_config::BehaviorVersion;
use aws_credential_types::Credentials;
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures::stream::BoxStream;
use std::collections::{HashMap, HashSet};

use tracing::{debug, instrument};

/// S3 storage backend for DeltaGlider objects
pub struct S3Backend {
    client: Client,
}

impl S3Backend {
    /// Create a new S3 backend from configuration
    pub async fn new(config: &BackendConfig) -> Result<Self, StorageError> {
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
            let credentials =
                Credentials::new(key_id, secret, None, None, "deltaglider_proxy-config");
            config_loader = config_loader.credentials_provider(credentials);
        }

        let sdk_config = config_loader.load().await;

        // Build S3-specific config with path-style option
        let s3_config = aws_sdk_s3::config::Builder::from(&sdk_config)
            .force_path_style(force_path_style)
            .build();

        let client = Client::from_conf(s3_config);

        debug!("S3Backend initialized (multi-bucket mode)");

        Ok(Self { client })
    }

    // === Key generation helpers ===

    /// Get the S3 key for a reference file
    fn reference_key(&self, prefix: &str) -> String {
        if prefix.is_empty() {
            "reference.bin".to_string()
        } else {
            format!("{}/reference.bin", prefix)
        }
    }

    /// Get the S3 key for a delta file
    fn delta_key(&self, prefix: &str, filename: &str) -> String {
        if prefix.is_empty() {
            format!("{}.delta", filename)
        } else {
            format!("{}/{}.delta", prefix, filename)
        }
    }

    /// Get the S3 key for a direct file
    fn direct_key(&self, prefix: &str, filename: &str) -> String {
        if prefix.is_empty() {
            format!("{}.direct", filename)
        } else {
            format!("{}/{}.direct", prefix, filename)
        }
    }

    // === Metadata conversion helpers ===

    /// Convert FileMetadata to S3 metadata headers (dg-* format)
    fn metadata_to_headers(&self, metadata: &FileMetadata) -> HashMap<String, String> {
        use crate::types::meta_keys as mk;
        let mut headers = HashMap::new();

        headers.insert(mk::TOOL.to_string(), metadata.tool.clone());
        headers.insert(
            mk::ORIGINAL_NAME.to_string(),
            metadata.original_name.clone(),
        );
        headers.insert(mk::FILE_SHA256.to_string(), metadata.file_sha256.clone());
        headers.insert(mk::FILE_SIZE.to_string(), metadata.file_size.to_string());
        headers.insert(mk::MD5.to_string(), metadata.md5.clone());
        if let Some(ref ct) = metadata.content_type {
            headers.insert("content-type".to_string(), ct.clone());
        }
        headers.insert(
            mk::CREATED_AT.to_string(),
            metadata
                .created_at
                .format("%Y-%m-%dT%H:%M:%S%.6fZ")
                .to_string(),
        );

        match &metadata.storage_info {
            StorageInfo::Reference { source_name } => {
                headers.insert(mk::NOTE.to_string(), "reference".to_string());
                headers.insert(mk::SOURCE_NAME.to_string(), source_name.clone());
            }
            StorageInfo::Delta {
                ref_key,
                ref_sha256,
                delta_size,
                delta_cmd,
            } => {
                headers.insert(mk::NOTE.to_string(), "delta".to_string());
                headers.insert(mk::REF_KEY.to_string(), ref_key.clone());
                headers.insert(mk::REF_SHA256.to_string(), ref_sha256.clone());
                headers.insert(mk::DELTA_SIZE.to_string(), delta_size.to_string());
                headers.insert(mk::DELTA_CMD.to_string(), delta_cmd.clone());
            }
            StorageInfo::Direct => {
                headers.insert(mk::NOTE.to_string(), "direct".to_string());
            }
        }

        for (key, value) in &metadata.user_metadata {
            headers.insert(format!("user-{}", key), value.clone());
        }

        headers
    }

    /// Convert S3 metadata headers to FileMetadata
    fn headers_to_metadata(
        &self,
        headers: &HashMap<String, String>,
    ) -> Result<FileMetadata, StorageError> {
        use crate::types::meta_keys as mk;

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

        let tool = get_value(&[mk::TOOL, "tool"])
            .ok_or_else(|| StorageError::Other(format!("Missing {}", mk::TOOL)))?;
        let original_name = get_value(&[
            mk::ORIGINAL_NAME,
            "original-name",
            mk::SOURCE_NAME,
            "source-name",
        ])
        .ok_or_else(|| StorageError::Other(format!("Missing {}", mk::ORIGINAL_NAME)))?;
        let file_sha256 = get_value(&[mk::FILE_SHA256, "file-sha256"])
            .ok_or_else(|| StorageError::Other(format!("Missing {}", mk::FILE_SHA256)))?;
        let file_size_str =
            get_value(&[mk::FILE_SIZE, "file-size"]).unwrap_or_else(|| "0".to_string());
        let file_size: u64 = file_size_str
            .parse()
            .map_err(|_| StorageError::Other(format!("Invalid file size: {}", file_size_str)))?;
        let created_at_str =
            get_value(&[mk::CREATED_AT, "created-at"]).unwrap_or_else(|| Utc::now().to_rfc3339());
        let created_at: DateTime<Utc> = {
            let ts = created_at_str.trim_end_matches('Z');
            DateTime::parse_from_rfc3339(&format!("{}+00:00", ts))
                .map(|dt| dt.with_timezone(&Utc))
                .or_else(|_| {
                    chrono::NaiveDateTime::parse_from_str(ts, "%Y-%m-%dT%H:%M:%S%.f")
                        .map(|ndt| ndt.and_utc())
                })
                .unwrap_or_else(|_| Utc::now())
        };

        let note = get_value(&[mk::NOTE, "note"]);
        let ref_key_opt = get_value(&[mk::REF_KEY, "ref-key"]);
        let is_reference = note.as_deref() == Some("reference");
        let is_delta = ref_key_opt.is_some()
            || note
                .as_ref()
                .map(|n| n == "delta" || n.starts_with("zero-diff"))
                .unwrap_or(false);

        let storage_info = if is_reference {
            let source_name = get_value(&[mk::SOURCE_NAME, "source-name"])
                .unwrap_or_else(|| original_name.clone());
            StorageInfo::Reference { source_name }
        } else if is_delta {
            let ref_key = ref_key_opt
                .ok_or_else(|| StorageError::Other(format!("Missing {}", mk::REF_KEY)))?;
            let ref_sha256 = get_value(&[mk::REF_SHA256, "ref-sha256"])
                .ok_or_else(|| StorageError::Other(format!("Missing {}", mk::REF_SHA256)))?;
            let delta_size_str =
                get_value(&[mk::DELTA_SIZE, "delta-size"]).unwrap_or_else(|| "0".to_string());
            let delta_size: u64 = delta_size_str.parse().map_err(|_| {
                StorageError::Other(format!("Invalid delta size: {}", delta_size_str))
            })?;
            let delta_cmd = get_value(&[mk::DELTA_CMD, "delta-cmd"]).unwrap_or_default();
            StorageInfo::Delta {
                ref_key,
                ref_sha256,
                delta_size,
                delta_cmd,
            }
        } else {
            StorageInfo::Direct
        };

        let md5 = headers
            .get(mk::MD5)
            .cloned()
            .unwrap_or_else(|| "".to_string());

        let user_metadata: std::collections::HashMap<String, String> = headers
            .iter()
            .filter_map(|(k, v)| {
                k.strip_prefix("user-")
                    .map(|suffix| (suffix.to_string(), v.clone()))
            })
            .collect();

        Ok(FileMetadata {
            tool,
            original_name,
            file_sha256,
            file_size,
            md5,
            created_at,
            content_type: headers.get("content-type").cloned(),
            user_metadata,
            storage_info,
        })
    }

    // === Internal helpers ===

    /// Put an object to S3 with metadata headers
    async fn put_object_with_metadata(
        &self,
        bucket: &str,
        key: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        let headers = self.metadata_to_headers(metadata);

        let mut request = self
            .client
            .put_object()
            .bucket(bucket)
            .key(key)
            .body(ByteStream::from(data.to_vec()))
            .content_type("application/octet-stream");

        for (k, v) in headers {
            request = request.metadata(k, v);
        }

        request
            .send()
            .await
            .map_err(|e| StorageError::S3(format!("put_object failed: {}", e)))?;

        debug!(
            "S3 PUT {}/{} ({} bytes) with DG metadata",
            bucket,
            key,
            data.len()
        );
        Ok(())
    }

    /// Get an object from S3
    async fn get_object(&self, bucket: &str, key: &str) -> Result<Vec<u8>, StorageError> {
        let response = self
            .client
            .get_object()
            .bucket(bucket)
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

        debug!("S3 GET {}/{} ({} bytes)", bucket, key, data.len());
        Ok(data)
    }

    /// Get object metadata from S3 headers
    async fn get_object_metadata(
        &self,
        bucket: &str,
        key: &str,
    ) -> Result<FileMetadata, StorageError> {
        let response = self
            .client
            .head_object()
            .bucket(bucket)
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
    async fn delete_s3_object(&self, bucket: &str, key: &str) -> Result<(), StorageError> {
        self.client
            .delete_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| StorageError::S3(format!("delete_object failed: {}", e)))?;

        debug!("S3 DELETE {}/{}", bucket, key);
        Ok(())
    }

    /// Check if an object exists in S3
    async fn object_exists(&self, bucket: &str, key: &str) -> bool {
        self.client
            .head_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .is_ok()
    }

    /// List objects with a prefix in a specific bucket
    async fn list_objects_with_prefix(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<Vec<String>, StorageError> {
        let mut keys = Vec::new();
        let mut continuation_token: Option<String> = None;

        loop {
            let mut request = self.client.list_objects_v2().bucket(bucket).prefix(prefix);

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
    // === Bucket operations ===

    #[instrument(skip(self))]
    async fn create_bucket(&self, bucket: &str) -> Result<(), StorageError> {
        self.client
            .create_bucket()
            .bucket(bucket)
            .send()
            .await
            .map_err(|e| StorageError::S3(format!("create_bucket failed: {}", e)))?;
        debug!("Created S3 bucket: {}", bucket);
        Ok(())
    }

    #[instrument(skip(self))]
    async fn delete_bucket(&self, bucket: &str) -> Result<(), StorageError> {
        self.client
            .delete_bucket()
            .bucket(bucket)
            .send()
            .await
            .map_err(|e| StorageError::S3(format!("delete_bucket failed: {}", e)))?;
        debug!("Deleted S3 bucket: {}", bucket);
        Ok(())
    }

    #[instrument(skip(self))]
    async fn list_buckets(&self) -> Result<Vec<String>, StorageError> {
        let response = self
            .client
            .list_buckets()
            .send()
            .await
            .map_err(|e| StorageError::S3(format!("list_buckets failed: {}", e)))?;

        let mut names: Vec<String> = response
            .buckets()
            .iter()
            .filter_map(|b| b.name().map(|n| n.to_string()))
            .collect();
        names.sort();
        debug!("Listed {} S3 buckets", names.len());
        Ok(names)
    }

    #[instrument(skip(self))]
    async fn head_bucket(&self, bucket: &str) -> Result<bool, StorageError> {
        match self.client.head_bucket().bucket(bucket).send().await {
            Ok(_) => Ok(true),
            Err(_) => Ok(false),
        }
    }

    // === Reference file operations ===

    #[instrument(skip(self, data, metadata))]
    async fn put_reference(
        &self,
        bucket: &str,
        prefix: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        let key = self.reference_key(prefix);
        self.put_object_with_metadata(bucket, &key, data, metadata)
            .await?;
        debug!(
            "Stored reference for {}/{} ({} bytes)",
            bucket,
            prefix,
            data.len()
        );
        Ok(())
    }

    #[instrument(skip(self, metadata))]
    async fn put_reference_metadata(
        &self,
        bucket: &str,
        prefix: &str,
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        let key = self.reference_key(prefix);
        let copy_source = format!("{}/{}", bucket, key);
        let headers = self.metadata_to_headers(metadata);

        let mut request = self
            .client
            .copy_object()
            .bucket(bucket)
            .copy_source(&copy_source)
            .key(&key)
            .metadata_directive(aws_sdk_s3::types::MetadataDirective::Replace);

        for (k, v) in headers {
            request = request.metadata(k, v);
        }

        request.send().await.map_err(|e| {
            StorageError::S3(format!("copy_object (metadata update) failed: {}", e))
        })?;

        debug!("Updated reference metadata for {}/{}", bucket, prefix);
        Ok(())
    }

    #[instrument(skip(self))]
    async fn get_reference(&self, bucket: &str, prefix: &str) -> Result<Vec<u8>, StorageError> {
        let key = self.reference_key(prefix);
        self.get_object(bucket, &key).await
    }

    #[instrument(skip(self))]
    async fn get_reference_metadata(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<FileMetadata, StorageError> {
        let key = self.reference_key(prefix);
        self.get_object_metadata(bucket, &key).await
    }

    #[instrument(skip(self))]
    async fn has_reference(&self, bucket: &str, prefix: &str) -> bool {
        let key = self.reference_key(prefix);
        self.object_exists(bucket, &key).await
    }

    #[instrument(skip(self))]
    async fn delete_reference(&self, bucket: &str, prefix: &str) -> Result<(), StorageError> {
        let key = self.reference_key(prefix);
        self.delete_s3_object(bucket, &key).await?;
        debug!("Deleted reference for {}/{}", bucket, prefix);
        Ok(())
    }

    // === Delta file operations ===

    #[instrument(skip(self, data, metadata))]
    async fn put_delta(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        let key = self.delta_key(prefix, filename);
        self.put_object_with_metadata(bucket, &key, data, metadata)
            .await?;
        debug!(
            "Stored delta for {}/{}/{} ({} bytes)",
            bucket,
            prefix,
            filename,
            data.len()
        );
        Ok(())
    }

    #[instrument(skip(self))]
    async fn get_delta(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<Vec<u8>, StorageError> {
        let key = self.delta_key(prefix, filename);
        self.get_object(bucket, &key).await
    }

    #[instrument(skip(self))]
    async fn get_delta_metadata(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<FileMetadata, StorageError> {
        let key = self.delta_key(prefix, filename);
        self.get_object_metadata(bucket, &key).await
    }

    #[instrument(skip(self))]
    async fn delete_delta(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<(), StorageError> {
        let key = self.delta_key(prefix, filename);
        self.delete_s3_object(bucket, &key).await?;
        debug!("Deleted delta for {}/{}/{}", bucket, prefix, filename);
        Ok(())
    }

    // === Direct file operations ===

    #[instrument(skip(self, data, metadata))]
    async fn put_direct(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        let key = self.direct_key(prefix, filename);
        self.put_object_with_metadata(bucket, &key, data, metadata)
            .await?;
        debug!(
            "Stored direct for {}/{}/{} ({} bytes)",
            bucket,
            prefix,
            filename,
            data.len()
        );
        Ok(())
    }

    #[instrument(skip(self))]
    async fn get_direct(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<Vec<u8>, StorageError> {
        let key = self.direct_key(prefix, filename);
        self.get_object(bucket, &key).await
    }

    #[instrument(skip(self))]
    async fn get_direct_metadata(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<FileMetadata, StorageError> {
        let key = self.direct_key(prefix, filename);
        self.get_object_metadata(bucket, &key).await
    }

    #[instrument(skip(self))]
    async fn delete_direct(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<(), StorageError> {
        let key = self.direct_key(prefix, filename);
        self.delete_s3_object(bucket, &key).await?;
        debug!("Deleted direct for {}/{}/{}", bucket, prefix, filename);
        Ok(())
    }

    // === Streaming operations ===

    #[instrument(skip(self))]
    async fn get_direct_stream(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<BoxStream<'static, Result<Bytes, StorageError>>, StorageError> {
        let key = self.direct_key(prefix, filename);
        let response = self
            .client
            .get_object()
            .bucket(bucket)
            .key(&key)
            .send()
            .await
            .map_err(|e| {
                if let SdkError::ServiceError(service_error) = &e {
                    if matches!(
                        service_error.err(),
                        aws_sdk_s3::operation::get_object::GetObjectError::NoSuchKey(_)
                    ) {
                        return StorageError::NotFound(key.clone());
                    }
                }
                StorageError::S3(format!("get_object failed: {}", e))
            })?;

        let data = response
            .body
            .collect()
            .await
            .map_err(|e| StorageError::S3(format!("Failed to read response body: {}", e)))?
            .into_bytes();

        debug!("S3 GET stream {}/{} ({} bytes)", bucket, key, data.len());
        Ok(Box::pin(futures::stream::once(async { Ok(data) })))
    }

    // === Scanning operations ===

    #[instrument(skip(self))]
    async fn scan_deltaspace(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<Vec<FileMetadata>, StorageError> {
        let search_prefix = if prefix.is_empty() {
            String::new()
        } else {
            format!("{}/", prefix)
        };
        let keys = self
            .list_objects_with_prefix(bucket, &search_prefix)
            .await?;

        let mut metadata_list = Vec::new();

        for key in keys {
            let is_reference = key == "reference.bin" || key.ends_with("/reference.bin");
            if !is_reference && !key.ends_with(".delta") && !key.ends_with(".direct") {
                continue;
            }
            if prefix.is_empty() && key.contains('/') {
                continue;
            }

            match self.get_object_metadata(bucket, &key).await {
                Ok(meta) => metadata_list.push(meta),
                Err(e) => {
                    debug!("Failed to read metadata from {}/{}: {}", bucket, key, e);
                }
            }
        }

        debug!(
            "Scanned {} objects in deltaspace {}/{}",
            metadata_list.len(),
            bucket,
            prefix
        );
        Ok(metadata_list)
    }

    #[instrument(skip(self))]
    async fn list_deltaspaces(&self, bucket: &str) -> Result<Vec<String>, StorageError> {
        let keys = self.list_objects_with_prefix(bucket, "").await?;
        let mut prefixes = HashSet::new();

        for key in keys {
            let is_reference = key == "reference.bin" || key.ends_with("/reference.bin");
            let is_delta_or_direct = key.ends_with(".delta") || key.ends_with(".direct");

            if is_reference || is_delta_or_direct {
                if let Some(idx) = key.rfind('/') {
                    let prefix = &key[..idx];
                    prefixes.insert(prefix.to_string());
                } else {
                    prefixes.insert(String::new());
                }
            }
        }

        let result: Vec<String> = prefixes.into_iter().collect();
        debug!("Found {} deltaspaces in bucket {}", result.len(), bucket);
        Ok(result)
    }

    #[instrument(skip(self))]
    async fn total_size(&self, bucket: Option<&str>) -> Result<u64, StorageError> {
        let buckets_to_scan = if let Some(b) = bucket {
            vec![b.to_string()]
        } else {
            self.list_buckets().await?
        };

        let mut total = 0u64;
        for b in &buckets_to_scan {
            let mut continuation_token: Option<String> = None;
            loop {
                let mut request = self.client.list_objects_v2().bucket(b);
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
        }

        debug!("Total S3 storage size: {} bytes", total);
        Ok(total)
    }
}
