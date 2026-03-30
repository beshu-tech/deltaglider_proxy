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

use tracing::{debug, instrument, warn};

/// Operation context for S3 error classification.
#[derive(Debug)]
enum S3Op {
    ListObjects,
    CreateBucket,
    PutObject,
    GetObject,
    DeleteObject,
    HeadObject,
    Other(&'static str),
}

impl std::fmt::Display for S3Op {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            S3Op::ListObjects => write!(f, "list_objects"),
            S3Op::CreateBucket => write!(f, "create_bucket"),
            S3Op::PutObject => write!(f, "put_object"),
            S3Op::GetObject => write!(f, "get_object"),
            S3Op::DeleteObject => write!(f, "delete_object"),
            S3Op::HeadObject => write!(f, "head_object"),
            S3Op::Other(s) => write!(f, "{}", s),
        }
    }
}

impl S3Op {
    /// Returns true if this operation is a bucket-level operation where a 403
    /// should be treated as BucketNotFound (S3-compatible providers like MinIO
    /// and Ceph return 403 for non-existent buckets).
    fn is_bucket_level(&self) -> bool {
        matches!(self, S3Op::ListObjects | S3Op::CreateBucket)
    }
}

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
}

/// S3 storage backend for DeltaGlider objects
pub struct S3Backend {
    client: Client,
}

impl S3Backend {
    /// Max concurrent HEAD requests to avoid S3 503 SlowDown throttling.
    /// See `bounded_head_calls()` for rationale.
    const MAX_CONCURRENT_HEADS: usize = 50;

    /// Convert an AWS SDK ListObjectsV2 `Object` into our lightweight `S3ListedObject`.
    fn convert_s3_object(object: aws_sdk_s3::types::Object) -> Option<S3ListedObject> {
        let key = object.key?;
        let last_modified = object.last_modified.and_then(|dt| {
            DateTime::parse_from_rfc3339(&dt.to_string())
                .ok()
                .map(|d| d.with_timezone(&Utc))
        });
        Some(S3ListedObject {
            key,
            size: object.size.unwrap_or(0) as u64,
            last_modified,
            etag: object.e_tag.map(|e| e.trim_matches('"').to_string()),
        })
    }
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

        // Build S3 client directly — no aws-config needed since we use static credentials.
        // Disable automatic request checksums (CRC32/CRC64) added by the SDK by default.
        // S3-compatible stores (Hetzner, MinIO, Backblaze B2) reject these headers with
        // BadRequest. Setting WhenRequired preserves compatibility with both AWS S3 and
        // S3-compatible endpoints. See: Python deltaglider [6.1.1] for the equivalent fix.
        let mut s3_config_builder = aws_sdk_s3::config::Builder::new()
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

    /// Classify an S3 SDK error with full diagnostic context.
    ///
    /// Logs bucket, key, body size, HTTP status, error code, and request-id
    /// for production debugging (per Python DeltaGlider team recommendations).
    /// Maps bucket-level 403 to BucketNotFound (Hetzner, Ceph return 403 for
    /// non-existent buckets to prevent enumeration).
    fn classify_s3_error(
        bucket: &str,
        e: &SdkError<impl std::fmt::Debug>,
        op: S3Op,
    ) -> StorageError {
        // Extract diagnostic details from the SDK error
        let (status, request_id) = if let SdkError::ServiceError(ref svc) = e {
            let raw = svc.raw();
            let status = raw.status().as_u16();
            let rid = raw.headers().get("x-amz-request-id").unwrap_or("-");
            (Some(status), rid.to_string())
        } else {
            (None, "-".to_string())
        };

        // Log full context for production debugging
        warn!(
            "S3 error: op={} bucket={} status={} request_id={} error={:?}",
            op,
            bucket,
            status
                .map(|s| s.to_string())
                .unwrap_or_else(|| "-".to_string()),
            request_id,
            e,
        );

        let debug_str = format!("{:?}", e);
        // Explicit NoSuchBucket in the error body → bucket doesn't exist.
        if debug_str.contains("NoSuchBucket") {
            return StorageError::BucketNotFound(bucket.to_string());
        }
        // Some S3-compatible providers (MinIO, Ceph) return 403 for non-existent
        // buckets to prevent bucket enumeration. Only treat 403 as BucketNotFound
        // if the operation is bucket-level. Object-level 403 errors are genuine
        // AccessDenied and should not be misclassified.
        if let Some(s) = status {
            if s == 403 && op.is_bucket_level() {
                return StorageError::BucketNotFound(bucket.to_string());
            }
        }
        StorageError::S3(format!(
            "{} failed (status={}): {}",
            op,
            status.unwrap_or(0),
            e
        ))
    }

    // === Key generation helpers ===

    /// Join a prefix and filename into an S3 key, omitting the prefix if empty.
    fn prefixed_key(prefix: &str, filename: &str) -> String {
        if prefix.is_empty() {
            filename.to_string()
        } else {
            format!("{}/{}", prefix, filename)
        }
    }

    /// Get the S3 key for a reference file
    fn reference_key(&self, prefix: &str) -> String {
        Self::prefixed_key(prefix, "reference.bin")
    }

    /// Get the S3 key for a delta file
    fn delta_key(&self, prefix: &str, filename: &str) -> String {
        Self::prefixed_key(prefix, &format!("{}.delta", filename))
    }

    /// Get the S3 key for a passthrough file (stored with original filename, no suffix)
    fn passthrough_key(&self, prefix: &str, filename: &str) -> String {
        Self::prefixed_key(prefix, filename)
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
                ref_path,
                ref_sha256,
                delta_size,
                delta_cmd,
            } => {
                headers.insert(mk::NOTE.to_string(), "delta".to_string());
                // Write as dg-ref-path (new canonical name)
                headers.insert(mk::REF_PATH.to_string(), ref_path.clone());
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
        // Read ref path: try new name (dg-ref-path) first, fall back to legacy (dg-ref-key, ref-key)
        let ref_path_opt = get_value(&[mk::REF_PATH, mk::REF_KEY, "ref-path", "ref-key"]);
        let is_reference = note.as_deref() == Some("reference");
        let is_delta = ref_path_opt.is_some()
            || note
                .as_ref()
                .map(|n| n == "delta" || n.starts_with("zero-diff"))
                .unwrap_or(false);

        let storage_info = if is_reference {
            let source_name = get_value(&[mk::SOURCE_NAME, "source-name"])
                .unwrap_or_else(|| original_name.clone());
            StorageInfo::Reference { source_name }
        } else if is_delta {
            let raw_ref_path = ref_path_opt
                .ok_or_else(|| StorageError::Other(format!("Missing {}", mk::REF_PATH)))?;
            // Normalize: if absolute (legacy), extract just the filename (typically "reference.bin")
            let ref_path = if raw_ref_path.contains('/') {
                raw_ref_path
                    .rsplit('/')
                    .next()
                    .unwrap_or(&raw_ref_path)
                    .to_string()
            } else {
                raw_ref_path
            };
            let ref_sha256 = get_value(&[mk::REF_SHA256, "ref-sha256"])
                .ok_or_else(|| StorageError::Other(format!("Missing {}", mk::REF_SHA256)))?;
            let delta_size_str =
                get_value(&[mk::DELTA_SIZE, "delta-size"]).unwrap_or_else(|| "0".to_string());
            let delta_size: u64 = delta_size_str.parse().map_err(|_| {
                StorageError::Other(format!("Invalid delta size: {}", delta_size_str))
            })?;
            let delta_cmd = get_value(&[mk::DELTA_CMD, "delta-cmd"]).unwrap_or_default();
            StorageInfo::Delta {
                ref_path,
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

    /// Put an object to S3 with metadata headers.
    /// Retries on transient errors (400 BadRequest from Hetzner, 503 SlowDown)
    /// with exponential backoff. Data is already fully buffered — retry is safe.
    async fn put_object_with_metadata(
        &self,
        bucket: &str,
        key: &str,
        data: &[u8],
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        let headers = self.metadata_to_headers(metadata);

        // S3 has a 2KB limit on total user metadata size. Warn if we're close.
        let total_meta_size: usize = headers.iter().map(|(k, v)| k.len() + v.len()).sum();
        if total_meta_size > 2048 {
            return Err(StorageError::Other(format!(
                "DG metadata exceeds S3's 2KB limit ({} bytes) for {}/{}",
                total_meta_size, bucket, key
            )));
        }

        let backoff_ms = [100, 200, 400];

        for attempt in 0..=backoff_ms.len() {
            let mut request = self
                .client
                .put_object()
                .bucket(bucket)
                .key(key)
                .body(ByteStream::from(data.to_vec()))
                .content_type("application/octet-stream");

            for (k, v) in &headers {
                request = request.metadata(k.clone(), v.clone());
            }

            match request.send().await {
                Ok(_) => {
                    if attempt > 0 {
                        debug!(
                            "S3 PUT {}/{} succeeded on attempt {} ({} bytes)",
                            bucket,
                            key,
                            attempt + 1,
                            data.len()
                        );
                    } else {
                        debug!(
                            "S3 PUT {}/{} ({} bytes) with DG metadata",
                            bucket,
                            key,
                            data.len()
                        );
                    }
                    return Ok(());
                }
                Err(e) => {
                    let is_retryable = if let SdkError::ServiceError(ref svc) = e {
                        let status = svc.raw().status().as_u16();
                        // Hetzner returns transient 400s with connection:close and no
                        // request-id (~1-2% of requests). 503 is standard SlowDown.
                        status == 400 || status == 503
                    } else {
                        // Network/dispatch errors are retryable
                        matches!(e, SdkError::DispatchFailure(_) | SdkError::TimeoutError(_))
                    };

                    if is_retryable && attempt < backoff_ms.len() {
                        warn!(
                            "S3 PUT {}/{} ({} bytes) failed (attempt {}), retrying in {}ms: {:?}",
                            bucket,
                            key,
                            data.len(),
                            attempt + 1,
                            backoff_ms[attempt],
                            e,
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(
                            backoff_ms[attempt] as u64,
                        ))
                        .await;
                        continue;
                    }

                    return Err(Self::classify_s3_error(bucket, &e, S3Op::PutObject));
                }
            }
        }

        // Unreachable: the loop always returns (success on Ok, error on final attempt).
        // Kept as a safety net — if control flow changes, this is better than silent success.
        unreachable!("retry loop must return on every path")
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
                Self::classify_s3_error(bucket, &e, S3Op::GetObject)
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
                Self::classify_s3_error(bucket, &e, S3Op::HeadObject)
            })?;

        let headers: HashMap<String, String> = response
            .metadata()
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();

        // Try parsing DG metadata from headers. If headers are empty or
        // corrupted (missing required fields), fall back to passthrough metadata
        // from the HEAD response itself.
        if !headers.is_empty() {
            match self.headers_to_metadata(&headers) {
                Ok(meta) => return Ok(meta),
                Err(e) => {
                    warn!(
                        "PATHOLOGICAL | Missing/corrupt DG metadata for {}/{} — falling back to passthrough. \
                         This file was likely copied without --metadata flag. Error: {}",
                        bucket, key, e
                    );
                }
            }
        }

        // Object exists on upstream S3 but has no (or corrupt) DeltaGlider metadata.
        // This is a pathological condition — delta features are disabled for this object.
        let is_delta_file = key.ends_with(".delta");
        let is_reference = key.ends_with("reference.bin");
        if is_delta_file || is_reference {
            warn!(
                "PATHOLOGICAL | {} file {}/{} has NO DG metadata! \
                 Delta reconstruction will not work. Was this file copied without preserving S3 metadata? \
                 Re-copy with: rclone copy src:bucket dst:bucket --metadata",
                if is_reference { "Reference" } else { "Delta" },
                bucket,
                key
            );
        }
        // Treat as passthrough with best-effort metadata from HEAD response.
        let file_size = response.content_length().unwrap_or(0).max(0) as u64;
        let last_modified = response
            .last_modified()
            .and_then(|t| {
                DateTime::parse_from_rfc3339(&t.to_string())
                    .ok()
                    .map(|dt| dt.with_timezone(&Utc))
            })
            .unwrap_or_else(Utc::now);
        let etag = response.e_tag().unwrap_or_default().to_string();
        let content_type = response.content_type().map(|s| s.to_string());
        Ok(FileMetadata::fallback(
            key.rsplit('/').next().unwrap_or(key).to_string(),
            file_size,
            etag,
            last_modified,
            content_type,
            StorageInfo::Passthrough,
        ))
    }

    /// Delete an object from S3
    async fn delete_s3_object(&self, bucket: &str, key: &str) -> Result<(), StorageError> {
        self.client
            .delete_object()
            .bucket(bucket)
            .key(key)
            .send()
            .await
            .map_err(|e| Self::classify_s3_error(bucket, &e, S3Op::DeleteObject))?;

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

            // Skip internal deltaspace files: reference.bin and anything inside .dg/
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

    /// Resolve classified objects to `(user_key, FileMetadata)` pairs using
    /// listing data only (no HEAD calls). Deduplicates by user key, keeping
    /// the latest version.
    fn resolve_classified_lite(
        classified: Vec<ClassifiedObject>,
        mut seed_results: Vec<(String, FileMetadata)>,
    ) -> Vec<(String, FileMetadata)> {
        let mut latest: HashMap<String, FileMetadata> = HashMap::new();

        for entry in classified {
            let is_delta = entry.s3_key.ends_with(".delta");
            let storage_info = if is_delta {
                StorageInfo::delta_stub(entry.listing_meta.size)
            } else {
                StorageInfo::Passthrough
            };
            let meta = Self::fallback_metadata_from_listing(
                &entry.listing_meta,
                &entry.user_key,
                storage_info,
            );

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
        FileMetadata::fallback(
            user_key.rsplit('/').next().unwrap_or(user_key).to_string(),
            obj.size,
            obj.etag.clone().unwrap_or_default(),
            obj.last_modified.unwrap_or_else(Utc::now),
            None,
            storage_info,
        )
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
                .map_err(|e| Self::classify_s3_error(bucket, &e, S3Op::ListObjects))?;

            if let Some(contents) = response.contents {
                results.extend(contents.into_iter().filter_map(Self::convert_s3_object));
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
            .map_err(|e| Self::classify_s3_error(bucket, &e, S3Op::CreateBucket))?;
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
            .map_err(|e| Self::classify_s3_error(bucket, &e, S3Op::Other("delete_bucket")))?;
        debug!("Deleted S3 bucket: {}", bucket);
        Ok(())
    }

    #[instrument(skip(self))]
    async fn list_buckets(&self) -> Result<Vec<String>, StorageError> {
        let dated = self.list_buckets_with_dates().await?;
        Ok(dated.into_iter().map(|(name, _)| name).collect())
    }

    #[instrument(skip(self))]
    async fn list_buckets_with_dates(&self) -> Result<Vec<(String, DateTime<Utc>)>, StorageError> {
        let response = self
            .client
            .list_buckets()
            .send()
            .await
            .map_err(|e| StorageError::S3(format!("list_buckets failed: {}", e)))?;

        let mut buckets: Vec<(String, DateTime<Utc>)> = response
            .buckets()
            .iter()
            .filter_map(|b| {
                b.name().map(|n| {
                    let created = b
                        .creation_date()
                        .and_then(|d| {
                            let secs = d.secs();
                            let nanos = d.subsec_nanos();
                            chrono::DateTime::from_timestamp(secs, nanos)
                        })
                        .unwrap_or_else(Utc::now);
                    (n.to_string(), created)
                })
            })
            .collect();
        buckets.sort_by(|a, b| a.0.cmp(&b.0));
        debug!("Listed {} S3 buckets", buckets.len());
        Ok(buckets)
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
            Self::classify_s3_error(bucket, &e, S3Op::Other("copy_object (metadata update)"))
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
                Self::classify_s3_error(bucket, &e, S3Op::GetObject)
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

    #[instrument(skip(self))]
    async fn get_passthrough_stream_range(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        start: u64,
        end: u64,
    ) -> Result<(BoxStream<'static, Result<Bytes, StorageError>>, u64), StorageError> {
        let key = self.passthrough_key(prefix, filename);
        let range_header = format!("bytes={}-{}", start, end);
        let response = self
            .client
            .get_object()
            .bucket(bucket)
            .key(&key)
            .range(&range_header)
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
                Self::classify_s3_error(bucket, &e, S3Op::GetObject)
            })?;

        let content_length = response.content_length.unwrap_or(0) as u64;
        debug!(
            "S3 GET range stream {}/{} ({}, {} bytes)",
            bucket, key, range_header, content_length
        );

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
        Ok((Box::pin(stream), content_length))
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
                    StorageInfo::delta_stub(obj.size)
                } else if obj.key.ends_with("/reference.bin") || obj.key == "reference.bin" {
                    StorageInfo::Reference {
                        source_name: String::new(),
                    }
                } else {
                    StorageInfo::Passthrough
                };

                FileMetadata::fallback(
                    original_name,
                    obj.size,
                    obj.etag.unwrap_or_default(),
                    obj.last_modified.unwrap_or_else(Utc::now),
                    None,
                    storage_info,
                )
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
                    .map_err(|e| Self::classify_s3_error(b, &e, S3Op::ListObjects))?;

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
                .map_err(|e| Self::classify_s3_error(bucket, &e, S3Op::ListObjects))?;

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

    /// Enrich listed objects with full metadata from bounded HEAD calls.
    /// Maps user-visible keys back to actual S3 keys (appending `.delta` for
    /// delta files) and fires parallel HEAD requests with concurrency control.
    async fn enrich_list_metadata(
        &self,
        bucket: &str,
        objects: Vec<(String, FileMetadata)>,
    ) -> Result<Vec<(String, FileMetadata)>, StorageError> {
        // Build a mapping from S3 key -> user key so we can HEAD the right
        // objects and map results back.
        let s3_keys: Vec<String> = objects
            .iter()
            .map(|(user_key, meta)| {
                if meta.is_delta() {
                    // Delta files are stored with .delta suffix
                    let obj = crate::types::ObjectKey::parse("_", user_key);
                    let prefix = obj.prefix;
                    let filename = obj.filename;
                    if prefix.is_empty() {
                        format!("{}.delta", filename)
                    } else {
                        format!("{}/{}.delta", prefix, filename)
                    }
                } else {
                    user_key.clone()
                }
            })
            .collect();

        let head_results = self
            .bounded_head_calls(bucket, s3_keys.iter().map(|s| s.as_str()))
            .await;

        let enriched: Vec<(String, FileMetadata)> = objects
            .into_iter()
            .zip(s3_keys.iter())
            .map(|((user_key, fallback_meta), s3_key)| {
                if let Some(head_meta) = head_results.get(s3_key) {
                    (user_key, head_meta.clone())
                } else {
                    (user_key, fallback_meta)
                }
            })
            .collect();

        debug!(
            "Enriched {} objects with HEAD metadata in {}",
            enriched.len(),
            bucket
        );
        Ok(enriched)
    }

    async fn bulk_list_objects(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<Vec<(String, FileMetadata)>, StorageError> {
        let listed = self.list_objects_full(bucket, prefix).await?;
        let (classified, dir_markers) = Self::classify_listed_objects(listed);

        // Build FileMetadata from LIST data only — no HEAD calls.
        // DG metadata (storage type, delta size, SHA) is fetched lazily via
        // HEAD when clients actually need it (GUI enrichKeys, inspector panel).
        //
        // NOTE: For delta files, file_size = delta size (not original size).
        // This is a known trade-off: accurate original sizes require HEAD per
        // delta file. The GUI handles this via lazy HEAD enrichment for visible
        // files. Third-party clients see the stored (delta) size, which is
        // technically correct from an S3 perspective.
        let results: Vec<(String, FileMetadata)> =
            Self::resolve_classified_lite(classified, dir_markers);

        debug!(
            "Bulk listed {} objects (lite, no HEAD) in {}/{}",
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
                .map_err(|e| Self::classify_s3_error(bucket, &e, S3Op::ListObjects))?;

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
                raw_objects.extend(contents.into_iter().filter_map(Self::convert_s3_object));
            }

            if response.is_truncated.unwrap_or(false) {
                upstream_token = response.next_continuation_token;
            } else {
                break;
            }
        }

        // Classify and build lite metadata (no HEAD calls — same as bulk_list_objects).
        let (classified, dir_markers) = Self::classify_listed_objects(raw_objects);
        let objects: Vec<(String, FileMetadata)> =
            Self::resolve_classified_lite(classified, dir_markers);

        // Apply max_keys across both objects and common_prefixes (interleaved)
        let common_prefixes: Vec<String> = all_common_prefixes.into_iter().collect();

        let page = crate::deltaglider::interleave_and_paginate(
            objects,
            common_prefixes,
            max_keys,
            continuation_token,
        );

        debug!(
            "Delegated list: {} objects + {} prefixes in {}/{}",
            page.objects.len(),
            page.common_prefixes.len(),
            bucket,
            prefix
        );

        Ok(Some(DelegatedListResult {
            objects: page.objects,
            common_prefixes: page.common_prefixes,
            is_truncated: page.is_truncated,
            next_continuation_token: page.next_continuation_token,
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
            .map_err(|e| Self::classify_s3_error(bucket, &e, S3Op::PutObject))?;

        debug!("Created directory marker: {}/{}", bucket, key);
        Ok(())
    }
}
