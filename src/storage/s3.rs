//! S3 storage backend implementation using AWS SDK
//!
//! This backend stores metadata in S3 object metadata headers (x-amz-meta-dg-*)
//! for compatibility with the original DeltaGlider CLI (beshultd/deltaglider).
//!
//! Each API bucket maps 1:1 to a real S3 bucket on the backend.

use super::traits::{DelegatedListResult, StorageBackend, StorageError};
use crate::config::BackendConfig;
use crate::types::{FileMetadata, StorageInfo};
use async_trait::async_trait;
use aws_credential_types::Credentials;
use aws_sdk_s3::config::BehaviorVersion;
use aws_sdk_s3::error::SdkError;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures::stream::BoxStream;
use futures::StreamExt;
use std::collections::{HashMap, HashSet};

use tracing::{debug, instrument};

/// Lightweight object info from ListObjectsV2 (no HEAD requests needed)
struct S3ListedObject {
    key: String,
    size: u64,
    last_modified: Option<DateTime<Utc>>,
    etag: Option<String>,
}

/// An S3 listed object classified into a user-visible key, with enough info
/// to decide whether a HEAD call is needed for full metadata.
struct ClassifiedObject {
    user_key: String,
    s3_key: String,
    listing_meta: S3ListedObject,
    /// Delta files need a HEAD call to get the real original file_size.
    needs_head: bool,
}

/// S3 storage backend for DeltaGlider objects
pub struct S3Backend {
    client: Client,
}

impl S3Backend {
    /// Max concurrent HEAD requests to avoid S3 503 SlowDown throttling.
    /// See `bounded_head_calls()` for rationale.
    const MAX_CONCURRENT_HEADS: usize = 50;
}

impl S3Backend {
    /// Build an S3 client from a BackendConfig without creating an S3Backend.
    /// Useful for one-off operations like testing connectivity.
    pub async fn build_client(config: &BackendConfig) -> Result<Client, StorageError> {
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

        // Require explicit credentials — never fall back to the default AWS credential chain
        // (env vars, ~/.aws/credentials, instance metadata, etc.)
        let credentials = match (access_key_id, secret_access_key) {
            (Some(ref key_id), Some(ref secret)) => {
                Credentials::new(key_id, secret, None, None, "deltaglider_proxy-config")
            }
            _ => {
                return Err(StorageError::Other(
                    "S3 backend requires explicit credentials: set DGP_BE_AWS_ACCESS_KEY_ID and DGP_BE_AWS_SECRET_ACCESS_KEY".to_string(),
                ));
            }
        };

        // Build S3 client directly — no aws-config needed since we use static credentials
        let mut s3_config_builder = aws_sdk_s3::config::Builder::new()
            .behavior_version(BehaviorVersion::latest())
            .region(aws_sdk_s3::config::Region::new(region))
            .credentials_provider(credentials)
            .force_path_style(force_path_style);

        if let Some(ref ep) = endpoint {
            s3_config_builder = s3_config_builder.endpoint_url(ep);
        }

        Ok(Client::from_conf(s3_config_builder.build()))
    }

    /// Create a new S3 backend from configuration
    pub async fn new(config: &BackendConfig) -> Result<Self, StorageError> {
        let client = Self::build_client(config).await?;
        debug!("S3Backend initialized (multi-bucket mode)");
        Ok(Self { client })
    }

    /// Classify an S3 SDK error, mapping bucket-level access/existence errors
    /// to `StorageError::BucketNotFound`.
    ///
    /// Many S3 providers (e.g. Hetzner, Ceph) return `AccessDenied` (403) instead
    /// of `NoSuchBucket` for non-existent buckets to prevent bucket enumeration.
    fn classify_s3_error(
        bucket: &str,
        e: &SdkError<impl std::fmt::Debug>,
        context: &str,
    ) -> StorageError {
        let debug_str = format!("{:?}", e);
        if debug_str.contains("NoSuchBucket") {
            return StorageError::BucketNotFound(bucket.to_string());
        }
        // Many providers return 403 AccessDenied for non-existent buckets
        if let SdkError::ServiceError(ref svc) = e {
            let raw = svc.raw();
            if raw.status().as_u16() == 403 {
                return StorageError::BucketNotFound(bucket.to_string());
            }
        }
        StorageError::S3(format!("{} failed: {}", context, e))
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

    /// Get the S3 key for a passthrough file (stored with original filename, no suffix)
    fn passthrough_key(&self, prefix: &str, filename: &str) -> String {
        if prefix.is_empty() {
            filename.to_string()
        } else {
            format!("{}/{}", prefix, filename)
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
            StorageInfo::Passthrough => {
                headers.insert(mk::NOTE.to_string(), "passthrough".to_string());
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
            StorageInfo::Passthrough
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
            .map_err(|e| Self::classify_s3_error(bucket, &e, "put_object"))?;

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
                Self::classify_s3_error(bucket, &e, "get_object")
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
                Self::classify_s3_error(bucket, &e, "head_object")
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
            .map_err(|e| Self::classify_s3_error(bucket, &e, "delete_object"))?;

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

    // === Listing classification helpers ===
    //
    // Both `bulk_list_objects` and `list_objects_delegated` need to:
    //   1. Classify raw S3 keys into user-visible objects vs internal files
    //   2. Fire parallel HEAD calls for delta files (listing size != original size)
    //   3. Build FileMetadata from HEAD results or listing fallback
    //   4. Dedup by user key, keeping the latest version
    //
    // These helpers centralise that logic so changes only need to happen once.

    /// Classify a batch of S3 listed objects into user-visible entries and
    /// directory markers. Internal files (reference.bin) are filtered out.
    fn classify_listed_objects(
        objects: Vec<S3ListedObject>,
    ) -> (Vec<ClassifiedObject>, Vec<(String, FileMetadata)>) {
        let mut classified = Vec::new();
        let mut dir_markers = Vec::new();

        for obj in objects {
            let filename = obj.key.rsplit('/').next().unwrap_or(&obj.key);

            // Directory marker: zero-byte key ending with '/'
            if obj.key.ends_with('/') && obj.size == 0 {
                dir_markers.push((obj.key.clone(), FileMetadata::directory_marker(&obj.key)));
                continue;
            }

            // Skip internal reference files
            if filename == "reference.bin" {
                continue;
            }

            let key_prefix = if obj.key.contains('/') {
                &obj.key[..obj.key.len() - filename.len() - 1]
            } else {
                ""
            };

            let is_delta = filename.ends_with(".delta");
            let original_name = if is_delta {
                filename.trim_end_matches(".delta").to_string()
            } else {
                filename.to_string()
            };

            let user_key = if key_prefix.is_empty() {
                original_name
            } else {
                format!("{}/{}", key_prefix, original_name)
            };

            classified.push(ClassifiedObject {
                user_key,
                s3_key: obj.key.clone(),
                listing_meta: obj,
                needs_head: is_delta,
            });
        }

        (classified, dir_markers)
    }

    /// Fire bounded parallel HEAD calls for a set of S3 keys, returning metadata
    /// for each key that responded successfully.
    ///
    /// PERF: Uses `buffer_unordered(MAX_CONCURRENT_HEADS)` instead of `join_all()`
    /// to avoid blasting thousands of concurrent HEADs at S3 (which triggers 503
    /// SlowDown throttling). Do NOT replace with `join_all()`.
    ///
    /// LIFETIME SUBTLETY: Keys and bucket are cloned into owned Strings and futures
    /// are collected into a Vec BEFORE streaming. Without this, the async closures
    /// capture `&self` and `&str` which can't satisfy the `'static` bound that
    /// `buffer_unordered` requires.
    async fn bounded_head_calls<'a, I>(
        &self,
        bucket: &str,
        keys: I,
    ) -> HashMap<String, FileMetadata>
    where
        I: Iterator<Item = &'a str>,
    {
        let head_futs: Vec<_> = keys
            .map(|key| {
                let key = key.to_string();
                let bucket = bucket.to_string();
                async move {
                    let meta_result = self.get_object_metadata(&bucket, &key).await;
                    (key, meta_result)
                }
            })
            .collect();
        futures::stream::iter(head_futs)
            .buffer_unordered(Self::MAX_CONCURRENT_HEADS)
            .filter_map(|(key, result)| async move { result.ok().map(|meta| (key, meta)) })
            .collect()
            .await
    }

    /// Fire parallel HEAD calls for classified objects that need them (deltas),
    /// then resolve each entry to final `(user_key, FileMetadata)` pairs,
    /// deduplicating by user key (keeping the latest version).
    async fn resolve_classified_metadata(
        &self,
        bucket: &str,
        classified: Vec<ClassifiedObject>,
        mut seed_results: Vec<(String, FileMetadata)>,
    ) -> Vec<(String, FileMetadata)> {
        let head_results = self
            .bounded_head_calls(
                bucket,
                classified
                    .iter()
                    .filter(|c| c.needs_head)
                    .map(|c| c.s3_key.as_str()),
            )
            .await;

        // Dedup by user key, keeping latest version
        let mut latest: HashMap<String, FileMetadata> = HashMap::new();

        for entry in classified {
            let meta = if entry.needs_head {
                if let Some(head_meta) = head_results.get(&entry.s3_key) {
                    head_meta.clone()
                } else {
                    Self::fallback_metadata_from_listing(
                        &entry.listing_meta,
                        &entry.user_key,
                        StorageInfo::Delta {
                            ref_key: String::new(),
                            ref_sha256: String::new(),
                            delta_size: entry.listing_meta.size,
                            delta_cmd: String::new(),
                        },
                    )
                }
            } else {
                Self::fallback_metadata_from_listing(
                    &entry.listing_meta,
                    &entry.user_key,
                    StorageInfo::Passthrough,
                )
            };

            match latest.entry(entry.user_key) {
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(meta);
                }
                std::collections::hash_map::Entry::Occupied(mut e) => {
                    if meta.created_at > e.get().created_at {
                        e.insert(meta);
                    }
                }
            }
        }

        for (key, meta) in latest {
            seed_results.push((key, meta));
        }

        seed_results.sort_by(|a, b| a.0.cmp(&b.0));
        seed_results
    }

    /// Build a best-effort FileMetadata from S3 listing info alone (no HEAD).
    /// Used when HEAD fails or isn't needed (passthrough files).
    fn fallback_metadata_from_listing(
        obj: &S3ListedObject,
        user_key: &str,
        storage_info: StorageInfo,
    ) -> FileMetadata {
        use crate::types::DELTAGLIDER_TOOL;
        FileMetadata {
            tool: DELTAGLIDER_TOOL.to_string(),
            original_name: user_key.rsplit('/').next().unwrap_or(user_key).to_string(),
            file_sha256: String::new(),
            file_size: obj.size,
            md5: obj.etag.clone().unwrap_or_default(),
            created_at: obj.last_modified.unwrap_or_else(Utc::now),
            content_type: None,
            user_metadata: HashMap::new(),
            storage_info,
        }
    }

    /// List objects with a prefix in a specific bucket (keys only)
    async fn list_objects_with_prefix(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<Vec<String>, StorageError> {
        let objects = self.list_objects_full(bucket, prefix).await?;
        Ok(objects.into_iter().map(|o| o.key).collect())
    }

    /// List objects with a prefix, returning full listing info (size, last_modified, etag)
    async fn list_objects_full(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<Vec<S3ListedObject>, StorageError> {
        let mut results = Vec::new();
        let mut continuation_token: Option<String> = None;

        loop {
            let mut request = self.client.list_objects_v2().bucket(bucket).prefix(prefix);

            if let Some(token) = continuation_token {
                request = request.continuation_token(token);
            }

            let response = request
                .send()
                .await
                .map_err(|e| Self::classify_s3_error(bucket, &e, "list_objects_v2"))?;

            if let Some(contents) = response.contents {
                for object in contents {
                    if let Some(key) = object.key {
                        let last_modified = object.last_modified.and_then(|dt| {
                            DateTime::parse_from_rfc3339(&dt.to_string())
                                .ok()
                                .map(|d| d.with_timezone(&Utc))
                        });
                        results.push(S3ListedObject {
                            key,
                            size: object.size.unwrap_or(0) as u64,
                            last_modified,
                            etag: object.e_tag.map(|e| e.trim_matches('"').to_string()),
                        });
                    }
                }
            }

            if response.is_truncated.unwrap_or(false) {
                continuation_token = response.next_continuation_token;
            } else {
                break;
            }
        }

        Ok(results)
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
            .map_err(|e| Self::classify_s3_error(bucket, &e, "create_bucket"))?;
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
            .map_err(|e| Self::classify_s3_error(bucket, &e, "delete_bucket"))?;
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

        request
            .send()
            .await
            .map_err(|e| Self::classify_s3_error(bucket, &e, "copy_object (metadata update)"))?;

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

    // === Passthrough file operations (stored with original filename) ===

    #[instrument(skip(self, data, metadata))]
    async fn put_passthrough(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        let key = self.passthrough_key(prefix, filename);
        self.put_object_with_metadata(bucket, &key, data, metadata)
            .await?;
        debug!(
            "Stored passthrough for {}/{}/{} ({} bytes)",
            bucket,
            prefix,
            filename,
            data.len()
        );
        Ok(())
    }

    #[instrument(skip(self))]
    async fn get_passthrough(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<Vec<u8>, StorageError> {
        let key = self.passthrough_key(prefix, filename);
        self.get_object(bucket, &key).await
    }

    #[instrument(skip(self))]
    async fn get_passthrough_metadata(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<FileMetadata, StorageError> {
        let key = self.passthrough_key(prefix, filename);
        self.get_object_metadata(bucket, &key).await
    }

    #[instrument(skip(self))]
    async fn delete_passthrough(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<(), StorageError> {
        let key = self.passthrough_key(prefix, filename);
        self.delete_s3_object(bucket, &key).await?;
        debug!("Deleted passthrough for {}/{}/{}", bucket, prefix, filename);
        Ok(())
    }

    // === Streaming operations ===

    #[instrument(skip(self))]
    async fn get_passthrough_stream(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
    ) -> Result<BoxStream<'static, Result<Bytes, StorageError>>, StorageError> {
        let key = self.passthrough_key(prefix, filename);
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
                Self::classify_s3_error(bucket, &e, "get_object")
            })?;

        debug!("S3 GET stream {}/{}", bucket, key);

        // Stream chunks directly from the S3 response body without buffering.
        let stream = futures::stream::unfold(response.body, |mut body| async {
            match body.try_next().await {
                Ok(Some(chunk)) => Some((Ok(chunk), body)),
                Ok(None) => None,
                Err(e) => Some((
                    Err(StorageError::S3(format!(
                        "Failed to read response body: {}",
                        e
                    ))),
                    body,
                )),
            }
        });
        Ok(Box::pin(stream))
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
        let listed = self.list_objects_full(bucket, &search_prefix).await?;

        // Filter to only files at this prefix level (not in subdirectories)
        let eligible: Vec<S3ListedObject> = listed
            .into_iter()
            .filter(|obj| {
                let key = &obj.key;
                // Skip subdirectory contents when scanning root
                if prefix.is_empty() && key.contains('/') {
                    return false;
                }
                true
            })
            .collect();

        // For delta files, ListObjectsV2 Size is the delta size, not the original.
        // Fetch real metadata from S3 headers via parallel HEAD calls for deltas.
        // Passthrough and reference files: listing Size == real file size.
        let mut delta_keys: Vec<String> = Vec::new();
        let mut items: Vec<(S3ListedObject, bool)> = Vec::new(); // (obj, is_delta)

        for obj in eligible {
            let is_delta = obj.key.ends_with(".delta");
            if is_delta {
                delta_keys.push(obj.key.clone());
            }
            items.push((obj, is_delta));
        }

        let head_results = self
            .bounded_head_calls(bucket, delta_keys.iter().map(|k| k.as_str()))
            .await;

        let metadata_list: Vec<FileMetadata> = items
            .into_iter()
            .map(|(obj, is_delta)| {
                // Use HEAD metadata for deltas when available (has real file_size)
                if is_delta {
                    if let Some(head_meta) = head_results.get(&obj.key) {
                        return head_meta.clone();
                    }
                }

                let filename = obj.key.rsplit('/').next().unwrap_or(&obj.key);

                let original_name = filename.trim_end_matches(".delta").to_string();

                let storage_info = if is_delta {
                    StorageInfo::Delta {
                        ref_key: String::new(),
                        ref_sha256: String::new(),
                        delta_size: obj.size,
                        delta_cmd: String::new(),
                    }
                } else if obj.key.ends_with("/reference.bin") || obj.key == "reference.bin" {
                    StorageInfo::Reference {
                        source_name: String::new(),
                    }
                } else {
                    StorageInfo::Passthrough
                };

                FileMetadata {
                    tool: crate::types::DELTAGLIDER_TOOL.to_string(),
                    original_name,
                    file_sha256: String::new(),
                    file_size: obj.size,
                    md5: obj.etag.unwrap_or_default(),
                    created_at: obj.last_modified.unwrap_or_else(Utc::now),
                    content_type: None,
                    user_metadata: HashMap::new(),
                    storage_info,
                }
            })
            .collect();

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
            // Every file in the bucket belongs to a deltaspace.
            // Delta files end with .delta, references are reference.bin,
            // passthrough files keep their original names.
            if let Some(idx) = key.rfind('/') {
                let prefix = &key[..idx];
                prefixes.insert(prefix.to_string());
            } else {
                prefixes.insert(String::new());
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
                    .map_err(|e| Self::classify_s3_error(b, &e, "list_objects_v2"))?;

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

    async fn list_directory_markers(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<Vec<String>, StorageError> {
        let mut markers = Vec::new();
        let mut continuation_token: Option<String> = None;

        loop {
            let mut request = self.client.list_objects_v2().bucket(bucket).prefix(prefix);

            if let Some(token) = continuation_token {
                request = request.continuation_token(token);
            }

            let response = request
                .send()
                .await
                .map_err(|e| Self::classify_s3_error(bucket, &e, "list_directory_markers"))?;

            if let Some(contents) = response.contents {
                for object in contents {
                    if let Some(key) = object.key {
                        if key.ends_with('/') && object.size.unwrap_or(0) == 0 {
                            markers.push(key);
                        }
                    }
                }
            }

            if response.is_truncated.unwrap_or(false) {
                continuation_token = response.next_continuation_token;
            } else {
                break;
            }
        }

        Ok(markers)
    }

    async fn bulk_list_objects(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<Vec<(String, FileMetadata)>, StorageError> {
        let listed = self.list_objects_full(bucket, prefix).await?;
        let (classified, dir_markers) = Self::classify_listed_objects(listed);
        let results = self
            .resolve_classified_metadata(bucket, classified, dir_markers)
            .await;

        debug!(
            "Bulk listed {} objects in {}/{}",
            results.len(),
            bucket,
            prefix
        );
        Ok(results)
    }

    /// Optimised listing that delegates delimiter collapsing to upstream S3.
    ///
    /// Instead of fetching *every* object and collapsing in-memory, we ask S3
    /// to handle the delimiter, which means S3 returns CommonPrefixes directly
    /// and only the objects at the current level appear in Contents.
    #[instrument(skip(self))]
    async fn list_objects_delegated(
        &self,
        bucket: &str,
        prefix: &str,
        delimiter: &str,
        max_keys: u32,
        continuation_token: Option<&str>,
    ) -> Result<Option<DelegatedListResult>, StorageError> {
        // We need to over-fetch from upstream because internal files
        // (reference.bin, .delta suffixes) inflate the key count.
        // Fetch in pages until we have enough user-visible entries.
        let mut all_common_prefixes = std::collections::BTreeSet::new();
        let mut raw_objects: Vec<S3ListedObject> = Vec::new();
        let mut upstream_token: Option<String> = None;
        let mut first_page = true;

        // When the engine gives us a continuation_token it's a *user-visible* key.
        // We use start_after to skip past it on upstream S3.
        let start_after = continuation_token.map(|s| s.to_string());

        loop {
            let mut request = self
                .client
                .list_objects_v2()
                .bucket(bucket)
                .prefix(prefix)
                .delimiter(delimiter);

            // On the first page use start_after; on subsequent pages use
            // the upstream continuation token.
            if first_page {
                if let Some(ref sa) = start_after {
                    request = request.start_after(sa);
                }
                first_page = false;
            } else if let Some(ref token) = upstream_token {
                request = request.continuation_token(token);
            }

            let response = request
                .send()
                .await
                .map_err(|e| Self::classify_s3_error(bucket, &e, "list_objects_delegated"))?;

            // Collect CommonPrefixes
            if let Some(cps) = response.common_prefixes {
                for cp in cps {
                    if let Some(p) = cp.prefix {
                        all_common_prefixes.insert(p);
                    }
                }
            }

            // Collect direct objects at this level
            if let Some(contents) = response.contents {
                for object in contents {
                    if let Some(key) = object.key {
                        let last_modified = object.last_modified.and_then(|dt| {
                            DateTime::parse_from_rfc3339(&dt.to_string())
                                .ok()
                                .map(|d| d.with_timezone(&Utc))
                        });
                        raw_objects.push(S3ListedObject {
                            key,
                            size: object.size.unwrap_or(0) as u64,
                            last_modified,
                            etag: object.e_tag.map(|e| e.trim_matches('"').to_string()),
                        });
                    }
                }
            }

            if response.is_truncated.unwrap_or(false) {
                upstream_token = response.next_continuation_token;
            } else {
                break;
            }
        }

        // Classify and resolve metadata using shared helpers
        let (classified, dir_markers) = Self::classify_listed_objects(raw_objects);
        let objects = self
            .resolve_classified_metadata(bucket, classified, dir_markers)
            .await;

        // Apply max_keys across both objects and common_prefixes (interleaved)
        let common_prefixes: Vec<String> = all_common_prefixes.into_iter().collect();

        // Interleave and paginate (CommonPrefixes count toward max_keys)
        enum Entry {
            Obj(String, Box<FileMetadata>),
            Prefix(String),
        }
        let mut entries: Vec<(String, Entry)> = Vec::new();
        for (key, meta) in objects {
            entries.push((key.clone(), Entry::Obj(key, Box::new(meta))));
        }
        for cp in common_prefixes {
            entries.push((cp.clone(), Entry::Prefix(cp)));
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));

        let max = max_keys as usize;
        let is_truncated = entries.len() > max;
        if entries.len() > max {
            entries.truncate(max);
        }
        let next_token = if is_truncated {
            entries.last().map(|(key, _)| key.clone())
        } else {
            None
        };

        let mut final_objects = Vec::new();
        let mut final_prefixes = Vec::new();
        for (_, entry) in entries {
            match entry {
                Entry::Obj(key, meta) => final_objects.push((key, *meta)),
                Entry::Prefix(p) => final_prefixes.push(p),
            }
        }

        debug!(
            "Delegated list: {} objects + {} prefixes in {}/{}",
            final_objects.len(),
            final_prefixes.len(),
            bucket,
            prefix
        );

        Ok(Some(DelegatedListResult {
            objects: final_objects,
            common_prefixes: final_prefixes,
            is_truncated,
            next_continuation_token: next_token,
        }))
    }

    async fn put_directory_marker(&self, bucket: &str, key: &str) -> Result<(), StorageError> {
        self.client
            .put_object()
            .bucket(bucket)
            .key(key)
            .content_type("application/x-directory")
            .content_length(0)
            .body(ByteStream::from(vec![]))
            .send()
            .await
            .map_err(|e| Self::classify_s3_error(bucket, &e, "put_directory_marker"))?;

        debug!("Created directory marker: {}/{}", bucket, key);
        Ok(())
    }
}
