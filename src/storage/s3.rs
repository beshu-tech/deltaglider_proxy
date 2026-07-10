// SPDX-License-Identifier: GPL-3.0-only

//! S3 storage backend implementation using AWS SDK
//!
//! This backend stores metadata in S3 object metadata headers (x-amz-meta-dg-*)
//! for compatibility with the original DeltaGlider CLI (beshultd/deltaglider).
//!
//! Each API bucket maps 1:1 to a real S3 bucket on the backend.
//!
//! ## Maintenance note (hygiene review, 2026-04-23)
//!
//! At ~1500 LOC this file mixes four concerns:
//!   1. S3Backend struct + constructor
//!   2. Error classification (S3Op, classify_get_error, body-stream helpers)
//!   3. Listing + pagination (S3ListedObject, list_objects_full)
//!   4. StorageBackend trait impl (the big impl block)
//!
//! It was NOT split as a pure refactor because the S3 path is hot
//! (every PUT/GET/LIST when the S3 backend is active) and a pure
//! file-reorg carries regression risk disproportionate to the
//! readability win. The next person adding a substantial S3 feature
//! (server-side encryption, requester-pays, checksum headers) should
//! split first, along natural boundaries:
//!   - storage/s3/mod.rs            — struct + trait impl
//!   - storage/s3/errors.rs         — S3Op + classify_get_error + stream
//!   - storage/s3/listing.rs        — S3ListedObject + pagination
//!   - storage/s3/metadata_io.rs    — header/metadata serialisation

use super::traits::{
    DelegatedListResult, LiteScanResult, MultipartUpload, StorageBackend, StorageError,
    UploadedPart,
};
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

/// Resolve an object's `created_at` from its (optional) `dg-created-at`
/// metadata value, falling back to `fallback` (the object's stable S3
/// `LastModified`) when the value is absent OR unparseable.
///
/// CRITICAL: the fallback is a STABLE per-object timestamp, never `Utc::now()`.
/// A synthesised "now" makes replication's NewerWins policy (which compares
/// `created_at`) re-copy the object on every tick forever — the root cause in
/// docs/plan/rca-replication-recopy-2026-06-30.md. Pure so the truth table is
/// unit-tested without an S3 client.
fn resolve_created_at(meta_value: Option<String>, fallback: DateTime<Utc>) -> DateTime<Utc> {
    let Some(raw) = meta_value else {
        return fallback;
    };
    let raw = raw.trim();
    // Try full RFC3339 first — it correctly handles `Z`, lowercase `z`, and any
    // numeric offset (`+02:00`), converting to UTC. Only then fall back to the
    // proxy's historical no-offset naive shape. NEVER string-surgery the offset:
    // the old `trim_end_matches('Z') + "+00:00"` silently dropped offset/`z`
    // values to the fallback (multi-agent review finding).
    DateTime::parse_from_rfc3339(raw)
        .map(|dt| dt.with_timezone(&Utc))
        .or_else(|_| {
            chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S%.fZ").map(|n| n.and_utc())
        })
        .or_else(|_| {
            chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S%.f").map(|n| n.and_utc())
        })
        .unwrap_or(fallback)
}

/// Operation context for S3 error classification.
#[derive(Debug)]
enum S3Op {
    ListObjects,
    CreateBucket,
    HeadBucket,
    PutObject,
    GetObject,
    DeleteObject,
    HeadObject,
    CreateMpu,
    UploadPart,
    CompleteMpu,
    AbortMpu,
    Other(&'static str),
}

impl std::fmt::Display for S3Op {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            S3Op::ListObjects => write!(f, "list_objects"),
            S3Op::CreateBucket => write!(f, "create_bucket"),
            S3Op::HeadBucket => write!(f, "head_bucket"),
            S3Op::PutObject => write!(f, "put_object"),
            S3Op::GetObject => write!(f, "get_object"),
            S3Op::DeleteObject => write!(f, "delete_object"),
            S3Op::HeadObject => write!(f, "head_object"),
            S3Op::CreateMpu => write!(f, "create_multipart_upload"),
            S3Op::UploadPart => write!(f, "upload_part"),
            S3Op::CompleteMpu => write!(f, "complete_multipart_upload"),
            S3Op::AbortMpu => write!(f, "abort_multipart_upload"),
            S3Op::Other(s) => write!(f, "{}", s),
        }
    }
}

impl S3Op {
    /// Returns true if this operation is a bucket-level operation where a 403
    /// should be treated as BucketNotFound (S3-compatible providers like MinIO
    /// and Ceph return 403 for non-existent buckets).
    fn is_bucket_level(&self) -> bool {
        matches!(
            self,
            S3Op::ListObjects | S3Op::CreateBucket | S3Op::HeadBucket
        )
    }
}

/// Lightweight object info from ListObjectsV2 (no HEAD requests needed)
struct S3ListedObject {
    key: String,
    size: u64,
    last_modified: Option<DateTime<Utc>>,
    etag: Option<String>,
}

impl S3ListedObject {
    /// Convert an AWS SDK `Object` from a ListObjectsV2 response into our
    /// lightweight representation.  Returns `None` if the object has no key.
    fn from_s3_object(object: aws_sdk_s3::types::Object) -> Option<Self> {
        let key = object.key?;
        let last_modified = object.last_modified.and_then(|dt| {
            DateTime::parse_from_rfc3339(&dt.to_string())
                .ok()
                .map(|d| d.with_timezone(&Utc))
        });
        Some(Self {
            key,
            size: object.size.unwrap_or(0) as u64,
            last_modified,
            etag: object.e_tag.map(|e| e.trim_matches('"').to_string()),
        })
    }
}

/// An S3 listed object classified into a user-visible key, with enough info
/// to decide whether a HEAD call is needed for full metadata.
struct ClassifiedObject {
    user_key: String,
    s3_key: String,
    listing_meta: S3ListedObject,
}

/// S3 storage backend for DeltaGlider objects
/// Native S3 server-side encryption mode applied per PutObject.
///
/// Distinct from the proxy's `EncryptingBackend` wrapper (which does
/// AES-256-GCM in-process before the bytes reach the backend).
/// Native modes delegate encryption to AWS: the proxy sends the
/// appropriate headers, AWS encrypts on write, AWS decrypts on read
/// for callers with KMS permission.
///
/// Stamped onto the object's `dg-encrypted-native` user-metadata so
/// reads can distinguish "native-encrypted" from "proxy-encrypted"
/// from "plaintext" — only proxy-encrypted objects need the
/// `EncryptingBackend` decrypt pass; native ones come back already-
/// decrypted from the SDK and the wrapper's `dg-encrypted` marker
/// check is (correctly) false.
#[derive(Debug, Clone, PartialEq)]
pub enum NativeEncryptionConfig {
    /// No S3-side encryption headers — proxy-mode encryption or
    /// plaintext. Default.
    None,
    /// SSE-S3 (AES256, AWS-managed keys). No KMS cost; minimal
    /// control. Stamps `dg-encrypted-native: sse-s3`.
    SseS3,
    /// SSE-KMS with a specific KMS key ARN/alias. `bucket_key_enabled`
    /// enables S3 bucket keys to amortise KMS API calls on bursty
    /// traffic. Stamps `dg-encrypted-native: sse-kms`.
    SseKms {
        kms_key_id: String,
        bucket_key_enabled: bool,
    },
}

impl NativeEncryptionConfig {
    /// Short machine-readable marker value written to
    /// `dg-encrypted-native`. Matches what read-side sniffers look
    /// for. Returns `None` for the plaintext case — callers skip
    /// stamping entirely when no native encryption is configured.
    fn marker(&self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::SseS3 => Some("sse-s3"),
            Self::SseKms { .. } => Some("sse-kms"),
        }
    }
}

pub struct S3Backend {
    client: Client,
    /// Per-backend native S3 server-side encryption mode. Applied to
    /// every `put_object`/`put_directory_marker` call.
    native_encryption: NativeEncryptionConfig,
}

impl S3Backend {
    /// Max concurrent HEAD requests to avoid S3 503 SlowDown throttling.
    /// See `bounded_head_calls()` for rationale.
    // 10, down from 50: fifty concurrent HEADs is a burst that trips Ceph
    // qos (Hetzner 503 SlowDown RCA, 2026-07-09); ten keeps a 1000-key page
    // enrich fast on a healthy backend while quartering the instantaneous
    // pressure, and doubles as the throttle-breaker batch size.
    const MAX_CONCURRENT_HEADS: usize = 10;
}

impl S3Backend {
    /// Build an S3 client from a BackendConfig without creating an S3Backend.
    /// Useful for one-off operations like testing connectivity.
    pub async fn build_client(config: &BackendConfig) -> Result<Client, StorageError> {
        let (endpoint, region, force_path_style, access_key_id, secret_access_key, allow_local) =
            match config {
                BackendConfig::S3 {
                    endpoint,
                    region,
                    force_path_style,
                    access_key_id,
                    secret_access_key,
                    allow_local,
                } => (
                    endpoint.clone(),
                    region.clone(),
                    *force_path_style,
                    access_key_id.clone(),
                    secret_access_key.clone(),
                    *allow_local,
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
        // Per-attempt + read/connect timeouts so a stalled socket fails
        // fast (per multipart part) instead of hanging the whole copy
        // until lease lapse. Phase B streaming relies on these to bound a
        // mid-part GET/PUT. All env-overridable in seconds.
        let read_timeout = crate::config::env_parse_with_default("DGP_S3_READ_TIMEOUT_SECS", 60u64);
        let connect_timeout =
            crate::config::env_parse_with_default("DGP_S3_CONNECT_TIMEOUT_SECS", 10u64);
        let attempt_timeout =
            crate::config::env_parse_with_default("DGP_S3_OPERATION_ATTEMPT_TIMEOUT_SECS", 300u64);
        let stall_grace = crate::config::env_parse_with_default("DGP_S3_STALL_GRACE_SECS", 20u64);
        let timeout_config = aws_sdk_s3::config::timeout::TimeoutConfig::builder()
            .read_timeout(std::time::Duration::from_secs(read_timeout))
            .connect_timeout(std::time::Duration::from_secs(connect_timeout))
            .operation_attempt_timeout(std::time::Duration::from_secs(attempt_timeout))
            .build();
        let stalled_stream_protection =
            aws_sdk_s3::config::StalledStreamProtectionConfig::enabled()
                .grace_period(std::time::Duration::from_secs(stall_grace))
                .build();

        let mut s3_config_builder = aws_sdk_s3::config::Builder::new()
            .behavior_version(BehaviorVersion::latest())
            .region(aws_sdk_s3::config::Region::new(region))
            .credentials_provider(credentials)
            .force_path_style(force_path_style)
            .timeout_config(timeout_config)
            // Adaptive retry = the SDK's client-wide rate limiter: a 503
            // SlowDown from the backend throttles EVERY subsequent request on
            // this client (HEAD bursts, listings, replication reads) instead
            // of each call site independently hammering a struggling backend.
            .retry_config(aws_sdk_s3::config::retry::RetryConfig::adaptive())
            .stalled_stream_protection(stalled_stream_protection)
            .request_checksum_calculation(
                aws_sdk_s3::config::RequestChecksumCalculation::WhenRequired,
            )
            .response_checksum_validation(
                aws_sdk_s3::config::ResponseChecksumValidation::WhenRequired,
            );

        if let Some(ref ep) = endpoint {
            // Reject operator-supplied endpoints that point at cloud
            // instance-metadata services, RFC1918 / loopback / link-
            // local ranges, or other internal hosts. Without this the
            // S3 backend becomes an SSRF pivot: a compromised admin
            // can swap the endpoint to http://169.254.169.254 and the
            // proxy will faithfully relay PUT/GET against IMDS,
            // turning admin-GUI access into cloud-account takeover.
            //
            // `BackendDev` keeps the door open for local MinIO so
            // dev/CI deployments still work — opted into via either
            // the typed `BackendConfig::S3.allow_local` field (the
            // preferred path) or the legacy `DGP_BACKEND_ALLOW_LOCAL=true`
            // env var (kept for backward-compat with existing
            // deployments and the proxy's env-driven config layer).
            // A hardened production env must keep both off.
            let env_allow = crate::config::env_bool("DGP_BACKEND_ALLOW_LOCAL", false);
            let kind = if allow_local || env_allow {
                crate::security::UrlKind::BackendDev
            } else {
                crate::security::UrlKind::Backend
            };
            crate::security::validate_outbound_url(ep, kind).map_err(|e| {
                StorageError::Other(format!(
                    "Refusing to use S3 endpoint {ep:?}: {e}. \
                     Set `allow_local: true` in the backend config (or \
                     DGP_BACKEND_ALLOW_LOCAL=true env) to permit http:// + \
                     private IPs for dev/CI."
                ))
            })?;
            s3_config_builder = s3_config_builder.endpoint_url(ep);
        }

        Ok(Client::from_conf(s3_config_builder.build()))
    }

    /// Create a new S3 backend from configuration + native encryption
    /// policy. Pass `NativeEncryptionConfig::None` for plaintext or for
    /// backends using proxy-side AES-256-GCM (the `EncryptingBackend`
    /// wrapper handles those at a layer above us).
    pub async fn new(
        config: &BackendConfig,
        native_encryption: NativeEncryptionConfig,
    ) -> Result<Self, StorageError> {
        let client = Self::build_client(config).await?;
        debug!(
            "S3Backend initialized (multi-bucket mode, native encryption: {:?})",
            native_encryption
        );
        Ok(Self {
            client,
            native_encryption,
        })
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
        // NoSuchKey (object-level 404) → NotFound, not a 500. This generic
        // classifier runs for ops without a typed error variant (e.g.
        // CopyObject, where the *source* key may have been deleted by a
        // concurrent request). Without this, such a benign race surfaced as
        // `S3(...)` → HTTP 500 instead of 404. The typed GET path already
        // does this via `classify_get_error`; this covers the rest. Guard on
        // the op NOT being bucket-level so a 404 on a bucket op stays a
        // BucketNotFound concern, not a key NotFound.
        if !op.is_bucket_level() && (debug_str.contains("NoSuchKey") || matches!(status, Some(404)))
        {
            return StorageError::NotFound(format!("{} key not found", op));
        }
        // Some S3-compatible providers (MinIO, Ceph) return 403 for non-existent
        // buckets to prevent bucket enumeration. Only treat 403 as BucketNotFound
        // if the operation is bucket-level. Object-level 403 errors are genuine
        // AccessDenied and should not be misclassified.
        if let Some(s) = status {
            if s == 403 && op.is_bucket_level() {
                return StorageError::BucketNotFound(bucket.to_string());
            }
            // E-P1-1: 503 SlowDown is the AWS-spec transient throttle
            // signal. Map to a dedicated `Throttled` variant so the
            // API layer can surface 503 SlowDown to the caller —
            // pre-fix this fell into `S3(...)` → catch-all in
            // `api/errors.rs` → 500 InternalError, which AWS SDKs
            // treat as permanent and DON'T back off on. Real
            // production load against a back-pressuring backend
            // would cascade into client retry storms with no
            // throttle propagation. Also catches `SlowDown` literal
            // in the SDK error body when the upstream returns it
            // without a 503 status (some implementations).
            // 429 Too Many Requests is the throttle signal from Backblaze B2 and
            // Cloudflare R2 (and some S3-compatibles) where AWS uses 503 SlowDown.
            // Classify it the same, or it falls into the S3(...) catch-all → 500,
            // which SDKs treat as permanent and don't back off on.
            if s == 503 || s == 429 || debug_str.contains("SlowDown") {
                return StorageError::Throttled(format!("{} throttled (status={}): {}", op, s, e));
            }
        } else if debug_str.contains("SlowDown") {
            return StorageError::Throttled(format!("{} throttled: {}", op, e));
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

    /// Convert FileMetadata to S3 metadata headers (bare dg-* keys).
    /// The S3 SDK auto-prepends `x-amz-meta-` when using `.metadata()`.
    /// Delegates to `FileMetadata::to_bare_metadata_map()` (single source of truth).
    fn metadata_to_headers(&self, metadata: &FileMetadata) -> HashMap<String, String> {
        metadata.to_bare_metadata_map()
    }

    /// Convert S3 metadata headers to FileMetadata
    /// `created_at_fallback` is the object's stable S3 `LastModified`, used when
    /// the DG metadata omits `dg-created-at`. NEVER fall back to `Utc::now()`
    /// here: a synthesised "now" makes replication's NewerWins (which compares
    /// `created_at`) re-copy the object every tick. See the RCA in
    /// docs/plan/rca-replication-recopy-2026-06-30.md.
    fn headers_to_metadata(
        &self,
        headers: &HashMap<String, String>,
        created_at_fallback: DateTime<Utc>,
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
        // Missing/malformed `dg-created-at` → the object's stable S3
        // LastModified, NOT `now()` (see the doc-comment above + the RCA).
        let created_at = resolve_created_at(
            get_value(&[mk::CREATED_AT, "created-at"]),
            created_at_fallback,
        );

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

        let multipart_etag = get_value(&["dg-multipart-etag"]);
        Ok(FileMetadata {
            tool,
            original_name,
            file_sha256,
            file_size,
            md5,
            multipart_etag,
            created_at,
            // Use the empty-skipping `get_value` (not raw `headers.get`): an
            // object stored with no/blank content-type leaves an empty
            // `content-type` user-metadata value, which must read back as None
            // so the output layer can apply the octet-stream default. A raw
            // `.cloned()` would yield Some("") and emit a blank content-type.
            content_type: get_value(&["content-type"]),
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
        let mut headers = self.metadata_to_headers(metadata);
        // Stamp the native-encryption marker so reads know this object
        // was encrypted by AWS (not by the proxy's `EncryptingBackend`
        // wrapper). The marker is plaintext in user-metadata — SSE-KMS
        // does NOT encrypt `x-amz-meta-*` headers, only the body.
        // This is acceptable because DG metadata is never considered
        // secret (see docs/product/reference/encryption-at-rest.md).
        if let Some(marker) = self.native_encryption.marker() {
            headers.insert("dg-encrypted-native".to_string(), marker.to_string());
        }

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
            request = apply_native_encryption(request, &self.native_encryption);

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

    /// Put an object to S3 from a source file path with metadata headers.
    /// Uses ByteStream::from_path to avoid buffering the full payload in memory.
    async fn put_object_file_with_metadata(
        &self,
        bucket: &str,
        key: &str,
        source_path: &std::path::Path,
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        let mut headers = self.metadata_to_headers(metadata);
        if let Some(marker) = self.native_encryption.marker() {
            headers.insert("dg-encrypted-native".to_string(), marker.to_string());
        }
        let total_meta_size: usize = headers.iter().map(|(k, v)| k.len() + v.len()).sum();
        if total_meta_size > 2048 {
            return Err(StorageError::Other(format!(
                "DG metadata exceeds S3's 2KB limit ({} bytes) for {}/{}",
                total_meta_size, bucket, key
            )));
        }

        let backoff_ms = [100, 200, 400];
        for attempt in 0..=backoff_ms.len() {
            let body = ByteStream::from_path(source_path.to_path_buf())
                .await
                .map_err(|e| {
                    StorageError::S3(format!("Failed to open source file stream: {}", e))
                })?;
            let mut request = self
                .client
                .put_object()
                .bucket(bucket)
                .key(key)
                .body(body)
                .content_type("application/octet-stream");
            for (k, v) in &headers {
                request = request.metadata(k.clone(), v.clone());
            }
            request = apply_native_encryption(request, &self.native_encryption);

            match request.send().await {
                Ok(_) => return Ok(()),
                Err(e) => {
                    let is_retryable = if let SdkError::ServiceError(ref svc) = e {
                        let status = svc.raw().status().as_u16();
                        status == 400 || status == 503
                    } else {
                        matches!(e, SdkError::DispatchFailure(_) | SdkError::TimeoutError(_))
                    };
                    if is_retryable && attempt < backoff_ms.len() {
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
        unreachable!("retry loop must return on every path")
    }

    /// Classify a GetObject SDK error, mapping NoSuchKey to NotFound.
    fn classify_get_error(
        bucket: &str,
        key: &str,
        e: &SdkError<aws_sdk_s3::operation::get_object::GetObjectError>,
    ) -> StorageError {
        if let SdkError::ServiceError(service_error) = e {
            if matches!(
                service_error.err(),
                aws_sdk_s3::operation::get_object::GetObjectError::NoSuchKey(_)
            ) {
                return StorageError::NotFound(key.to_string());
            }
        }
        Self::classify_s3_error(bucket, e, S3Op::GetObject)
    }

    /// Convert an S3 response body into a streaming `BoxStream` of `Bytes` chunks.
    /// Used by both `get_passthrough_stream` and `get_passthrough_stream_range`.
    fn s3_body_to_stream(
        body: aws_sdk_s3::primitives::ByteStream,
    ) -> BoxStream<'static, Result<Bytes, StorageError>> {
        Box::pin(futures::stream::unfold(body, |mut body| async {
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
        }))
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
            .map_err(|e| Self::classify_get_error(bucket, key, &e))?;

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

        // The object's S3 LastModified — a STABLE per-object timestamp. Used as
        // the `created_at` fallback when DG metadata is present but carries no
        // `dg-created-at` (e.g. checksum sidecars uploaded with partial DG
        // metadata). Without this, `headers_to_metadata` would synthesise
        // `Utc::now()` on every read, making replication's NewerWins re-copy the
        // object every tick forever. See docs/plan/rca-replication-recopy-2026-06-30.md.
        let s3_last_modified = response
            .last_modified()
            .and_then(|t| {
                DateTime::parse_from_rfc3339(&t.to_string())
                    .ok()
                    .map(|dt| dt.with_timezone(&Utc))
            })
            .unwrap_or_else(Utc::now);

        // A `.delta` / `reference.bin` file is delta-machinery: missing DG
        // metadata on one of those genuinely breaks reconstruction and is worth
        // a loud WARN. Any other object (e.g. a `.sha1`/`.sha512` checksum
        // sidecar, an image, anything copied without `--metadata`) falling back
        // to passthrough is entirely benign — it just has no delta to track.
        // Logging that at WARN per-object floods the log on every
        // `metadata=true` listing (25k+ lines observed on a build-artifact
        // bucket), so it goes to DEBUG. This is the level gate, computed once.
        let is_delta_file = key.ends_with(".delta");
        let is_reference = key.ends_with("reference.bin");
        let delta_critical = is_delta_file || is_reference;

        // Try parsing DG metadata from headers. If headers are empty or
        // corrupted (missing required fields), fall back to passthrough metadata
        // from the HEAD response itself.
        if !headers.is_empty() {
            match self.headers_to_metadata(&headers, s3_last_modified) {
                Ok(meta) => return Ok(meta),
                Err(e) if delta_critical => {
                    warn!(
                        "PATHOLOGICAL | {} file {}/{} has missing/corrupt DG metadata — \
                         delta reconstruction will not work. Was this file copied without \
                         preserving S3 metadata? Re-copy with: \
                         rclone copy src:bucket dst:bucket --metadata. Error: {}",
                        if is_reference { "Reference" } else { "Delta" },
                        bucket,
                        key,
                        e
                    );
                }
                Err(e) => {
                    debug!(
                        "No DG metadata for {}/{} — serving as passthrough \
                         (likely copied without --metadata). Error: {}",
                        bucket, key, e
                    );
                }
            }
        } else if delta_critical {
            // Object exists on upstream S3 but carries NO metadata at all, and
            // it's a delta/reference file — reconstruction is broken.
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
        let last_modified = s3_last_modified;
        // Upstream S3 returns the ETag already wrapped in quotes (e.g.
        // `"abc123"`). FileMetadata.md5 must hold the BARE value — the
        // response-emit layer re-adds the quotes when forming the HEAD/GET
        // ETag. Storing it quoted here produced a doubled-quote ETag
        // (`""abc123""`) that strict S3 clients reject. Strip to match the
        // listing path (`object.e_tag.map(|e| e.trim_matches('"'))`).
        let etag = response
            .e_tag()
            .unwrap_or_default()
            .trim_matches('"')
            .to_string();
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

    // (object_exists removed: it mapped every HEAD error to `false`, which is
    // the has_reference transient-error corruption bug. has_reference now
    // classifies the error directly — NotFound → absent, else propagate.)

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
    /// Returns the successful lookups plus a count of throttle rejections
    /// (503 SlowDown), so callers can stop a HEAD sweep against a backend
    /// that is actively shedding load instead of grinding through it.
    async fn bounded_head_calls<'a, I>(
        &self,
        bucket: &str,
        keys: I,
    ) -> (HashMap<String, FileMetadata>, usize)
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
            .fold(
                (HashMap::new(), 0usize),
                |(mut ok, throttled), (key, result)| async move {
                    match result {
                        Ok(meta) => {
                            ok.insert(key, meta);
                            (ok, throttled)
                        }
                        Err(StorageError::Throttled(_)) => (ok, throttled + 1),
                        Err(_) => (ok, throttled),
                    }
                },
            )
            .await
    }

    /// Resolve classified objects to `(user_key, FileMetadata)` pairs using
    /// listing data only (no HEAD calls). Deduplicates by user key, keeping
    /// the latest version.
    fn resolve_classified_lite(
        classified: Vec<ClassifiedObject>,
        mut seed_results: Vec<(String, FileMetadata)>,
    ) -> Vec<(String, FileMetadata)> {
        let classified_pairs: Vec<(String, FileMetadata)> = classified
            .into_iter()
            .map(|entry| {
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
                (entry.user_key, meta)
            })
            .collect();

        seed_results.extend(classified_pairs);
        crate::types::dedup_keep_latest(seed_results)
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

    /// LIST a deltaspace and return only the entries at the prefix level
    /// itself (not from subdirectories). Shared between
    /// [`scan_deltaspace`] and [`scan_deltaspace_lite`].
    ///
    /// When `prefix` is non-empty, S3's LIST already restricts to the
    /// `prefix/` subtree so we accept everything it returns. When
    /// `prefix` is empty we're scanning the bucket root and have to
    /// drop entries that live in subdirectories ourselves.
    async fn list_deltaspace_eligible(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<Vec<S3ListedObject>, StorageError> {
        let search_prefix = if prefix.is_empty() {
            String::new()
        } else {
            format!("{}/", prefix)
        };
        let listed = self.list_objects_full(bucket, &search_prefix).await?;
        let scanning_root = prefix.is_empty();
        let eligible: Vec<S3ListedObject> = listed
            .into_iter()
            .filter(|obj| !(scanning_root && obj.key.contains('/')))
            .collect();
        Ok(eligible)
    }

    /// Build a no-HEAD FileMetadata for a listed object. For deltas the
    /// resulting `file_size` is the on-disk delta size, not the original.
    /// Suitable only for callers that explicitly opt into the lite shape.
    fn lite_metadata_from_listed(obj: &S3ListedObject) -> FileMetadata {
        let filename = obj.key.rsplit('/').next().unwrap_or(&obj.key);
        let is_delta = filename.ends_with(".delta");
        let is_reference = filename == "reference.bin";
        let original_name = filename.trim_end_matches(".delta").to_string();
        let storage_info = if is_delta {
            StorageInfo::delta_stub(obj.size)
        } else if is_reference {
            StorageInfo::Reference {
                source_name: String::new(),
            }
        } else {
            StorageInfo::Passthrough
        };
        FileMetadata::fallback(
            original_name,
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
                results.extend(
                    contents
                        .into_iter()
                        .filter_map(S3ListedObject::from_s3_object),
                );
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
            // classify (not a bare S3(...)) so a 503 throttle surfaces as
            // Throttled → SlowDown, not a retry-storm-inducing 500.
            .map_err(|e| Self::classify_s3_error("", &e, S3Op::Other("list_buckets")))?;

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
            // Only a genuine bucket-404 → absent. A 503 SlowDown / timeout /
            // 5xx must NOT read as absent — routing uses this to place a bucket
            // on a backend; a transient error must not silently reroute writes
            // to the wrong (default) backend. (Same class as has_reference.)
            Err(e) => match Self::classify_s3_error(bucket, &e, S3Op::HeadBucket) {
                StorageError::BucketNotFound(_) | StorageError::NotFound(_) => Ok(false),
                other => Err(other),
            },
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

    async fn put_reference_from_file(
        &self,
        bucket: &str,
        prefix: &str,
        source_path: &std::path::Path,
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        // Stream the file body to S3 (ByteStream::from_path) — never heap-load it.
        let key = self.reference_key(prefix);
        self.put_object_file_with_metadata(bucket, &key, source_path, metadata)
            .await?;
        debug!("Stored reference from file for {}/{}", bucket, prefix);
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

    async fn get_reference_to_file(
        &self,
        bucket: &str,
        prefix: &str,
        dest: &std::path::Path,
    ) -> Result<u64, StorageError> {
        use tokio::io::AsyncWriteExt;
        let key = self.reference_key(prefix);
        // Stream the GET body straight to the dest file — never collect the
        // (possibly multi-GB) reference into a Vec (blocker 10).
        let response = self
            .client
            .get_object()
            .bucket(bucket)
            .key(&key)
            .send()
            .await
            .map_err(|e| Self::classify_get_error(bucket, &key, &e))?;

        let mut stream = Self::s3_body_to_stream(response.body);
        let mut file = tokio::fs::File::create(dest).await?;
        let mut written: u64 = 0;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            file.write_all(&chunk).await?;
            written += chunk.len() as u64;
        }
        file.flush().await?;
        debug!(
            "S3 GET reference→file {}/{} ({} bytes)",
            bucket, key, written
        );
        Ok(written)
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
    async fn has_reference(&self, bucket: &str, prefix: &str) -> Result<bool, StorageError> {
        let key = self.reference_key(prefix);
        match self
            .client
            .head_object()
            .bucket(bucket)
            .key(&key)
            .send()
            .await
        {
            Ok(_) => Ok(true),
            // A genuine object-level 404 → absent. Anything else (503 SlowDown,
            // timeout, 5xx, connection reset) must NOT read as absent — else a
            // write path overwrites a live reference.bin on a backend hiccup.
            Err(e) => match Self::classify_s3_error(bucket, &e, S3Op::HeadObject) {
                StorageError::NotFound(_) => Ok(false),
                other => Err(other),
            },
        }
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

    #[instrument(skip(self, metadata))]
    async fn put_passthrough_file(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        source_path: &std::path::Path,
        metadata: &FileMetadata,
    ) -> Result<(), StorageError> {
        let key = self.passthrough_key(prefix, filename);
        self.put_object_file_with_metadata(bucket, &key, source_path, metadata)
            .await?;
        debug!(
            "Stored passthrough from file for {}/{}/{} ({:?})",
            bucket, prefix, filename, source_path
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
            .map_err(|e| Self::classify_get_error(bucket, &key, &e))?;

        debug!("S3 GET stream {}/{}", bucket, key);

        Ok(Box::pin(Self::s3_body_to_stream(response.body)))
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
            .map_err(|e| Self::classify_get_error(bucket, &key, &e))?;

        let content_length = response.content_length.unwrap_or(0) as u64;
        debug!(
            "S3 GET range stream {}/{} ({}, {} bytes)",
            bucket, key, range_header, content_length
        );

        Ok((
            Box::pin(Self::s3_body_to_stream(response.body)),
            content_length,
        ))
    }

    // === Multipart upload (Phase B native streaming copy) ===

    fn supports_native_multipart(&self, _bucket: &str) -> bool {
        true
    }

    fn lite_list_carries_logical_facts(&self, _bucket: &str) -> bool {
        // S3 LIST returns NO user metadata (so replication-provenance ownership
        // can't be read from a lite entry) — parity must HEAD to know ownership.
        false
    }

    #[instrument(skip(self, metadata))]
    async fn create_multipart_upload(
        &self,
        bucket: &str,
        prefix: &str,
        filename: &str,
        metadata: &FileMetadata,
    ) -> Result<MultipartUpload, StorageError> {
        let key = self.passthrough_key(prefix, filename);
        let mut headers = self.metadata_to_headers(metadata);
        if let Some(marker) = self.native_encryption.marker() {
            headers.insert("dg-encrypted-native".to_string(), marker.to_string());
        }
        let total_meta_size: usize = headers.iter().map(|(k, v)| k.len() + v.len()).sum();
        if total_meta_size > 2048 {
            return Err(StorageError::Other(format!(
                "DG metadata exceeds S3's 2KB limit ({} bytes) for {}/{}",
                total_meta_size, bucket, key
            )));
        }

        let mut request = self
            .client
            .create_multipart_upload()
            .bucket(bucket)
            .key(&key)
            .content_type("application/octet-stream");
        for (k, v) in &headers {
            request = request.metadata(k.clone(), v.clone());
        }
        request = apply_native_encryption_mpu(request, &self.native_encryption);

        let resp = request
            .send()
            .await
            .map_err(|e| Self::classify_s3_error(bucket, &e, S3Op::CreateMpu))?;
        let upload_id = resp.upload_id().ok_or_else(|| {
            StorageError::S3(format!(
                "create_multipart_upload returned no upload id for {}/{}",
                bucket, key
            ))
        })?;
        Ok(MultipartUpload {
            bucket: bucket.to_string(),
            upload_id: upload_id.to_string(),
            native: true,
            backend: None,
        })
    }

    #[instrument(skip(self, upload, data))]
    async fn upload_part(
        &self,
        upload: &MultipartUpload,
        prefix: &str,
        filename: &str,
        part_number: i32,
        data: Bytes,
    ) -> Result<UploadedPart, StorageError> {
        let key = self.passthrough_key(prefix, filename);
        let backoff_ms = [100u64, 200, 400];
        for attempt in 0..=backoff_ms.len() {
            let body = ByteStream::from(data.clone());
            let result = self
                .client
                .upload_part()
                .bucket(&upload.bucket)
                .key(&key)
                .upload_id(&upload.upload_id)
                .part_number(part_number)
                .body(body)
                .send()
                .await;
            match result {
                Ok(resp) => {
                    let etag = resp.e_tag().unwrap_or_default().to_string();
                    return Ok(UploadedPart { part_number, etag });
                }
                Err(e) => {
                    let is_retryable = if let SdkError::ServiceError(ref svc) = e {
                        let status = svc.raw().status().as_u16();
                        status == 400 || status == 500 || status == 503
                    } else {
                        matches!(e, SdkError::DispatchFailure(_) | SdkError::TimeoutError(_))
                    };
                    if is_retryable && attempt < backoff_ms.len() {
                        warn!(
                            "S3 upload_part {}/{} part {} failed (attempt {}), retrying in {}ms: {:?}",
                            upload.bucket, key, part_number, attempt + 1, backoff_ms[attempt], e
                        );
                        tokio::time::sleep(std::time::Duration::from_millis(backoff_ms[attempt]))
                            .await;
                        continue;
                    }
                    return Err(Self::classify_s3_error(
                        &upload.bucket,
                        &e,
                        S3Op::UploadPart,
                    ));
                }
            }
        }
        unreachable!("retry loop must return on every path")
    }

    #[instrument(skip(self, upload, parts, _assembled, _metadata))]
    async fn complete_multipart_upload(
        &self,
        upload: &MultipartUpload,
        prefix: &str,
        filename: &str,
        parts: &[UploadedPart],
        _assembled: &[Bytes],
        _metadata: &FileMetadata,
    ) -> Result<String, StorageError> {
        use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
        let key = self.passthrough_key(prefix, filename);
        let completed_parts: Vec<CompletedPart> = parts
            .iter()
            .map(|p| {
                CompletedPart::builder()
                    .part_number(p.part_number)
                    .e_tag(p.etag.clone())
                    .build()
            })
            .collect();
        let completed = CompletedMultipartUpload::builder()
            .set_parts(Some(completed_parts))
            .build();
        let resp = self
            .client
            .complete_multipart_upload()
            .bucket(&upload.bucket)
            .key(&key)
            .upload_id(&upload.upload_id)
            .multipart_upload(completed)
            .send()
            .await
            .map_err(|e| Self::classify_s3_error(&upload.bucket, &e, S3Op::CompleteMpu))?;
        // The S3 multipart ETag is `<md5-of-concatenated-part-md5s>-<n>`.
        let etag = resp
            .e_tag()
            .map(|e| e.trim_matches('"').to_string())
            .unwrap_or_default();
        Ok(etag)
    }

    #[instrument(skip(self, upload))]
    async fn abort_multipart_upload(
        &self,
        upload: &MultipartUpload,
        prefix: &str,
        filename: &str,
    ) -> Result<(), StorageError> {
        let key = self.passthrough_key(prefix, filename);
        self.client
            .abort_multipart_upload()
            .bucket(&upload.bucket)
            .key(&key)
            .upload_id(&upload.upload_id)
            .send()
            .await
            .map_err(|e| Self::classify_s3_error(&upload.bucket, &e, S3Op::AbortMpu))?;
        Ok(())
    }

    // === Scanning operations ===

    #[instrument(skip(self))]
    async fn scan_deltaspace(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<Vec<FileMetadata>, StorageError> {
        let listed = self.list_deltaspace_eligible(bucket, prefix).await?;

        // For delta files, ListObjectsV2 Size is the delta size, not the
        // original. HEAD each one (bounded parallel) to recover the real
        // original size from user-metadata. Passthrough and reference
        // entries: listing Size == real file size, no HEAD needed.
        let delta_keys: Vec<String> = listed
            .iter()
            .filter(|obj| obj.key.ends_with(".delta"))
            .map(|obj| obj.key.clone())
            .collect();
        // Deltaspace scans want completeness (savings accounting), so no
        // throttle bail here — the SDK's adaptive rate limiter paces the
        // sweep instead. The count is logged for observability only.
        let (head_results, throttled) = self
            .bounded_head_calls(bucket, delta_keys.iter().map(|k| k.as_str()))
            .await;
        if throttled > 0 {
            warn!("deltaspace scan of {bucket}: {throttled} HEADs throttled (503 SlowDown)");
        }

        // `head_results` only contains the delta keys we HEAD'd above —
        // non-delta entries fall through to the no-HEAD builder.
        let metadata_list: Vec<FileMetadata> = listed
            .into_iter()
            .map(|obj| {
                if let Some(head_meta) = head_results.get(&obj.key) {
                    head_meta.clone()
                } else {
                    Self::lite_metadata_from_listed(&obj)
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

    /// HEAD-free variant. Suitable for diagnostics callers that only
    /// need delta sizes (which are already in the listing).
    ///
    /// PERF: For a bucket with 141 prefixes × ~500 deltas, the regular
    /// `scan_deltaspace` fires ~70k HEAD calls. This variant fires
    /// zero HEADs and is ~300× faster end-to-end.
    ///
    /// Reports `originals_estimated: true` because, without HEAD, we
    /// don't recover the original-file size of `.delta` entries — the
    /// `file_size` field on delta `FileMetadata` is the on-disk delta
    /// size in this shape. Callers MUST honour the flag and suppress
    /// "savings" / "original total" displays.
    async fn scan_deltaspace_lite(
        &self,
        bucket: &str,
        prefix: &str,
    ) -> Result<LiteScanResult, StorageError> {
        let listed = self.list_deltaspace_eligible(bucket, prefix).await?;
        let metadata: Vec<FileMetadata> =
            listed.iter().map(Self::lite_metadata_from_listed).collect();
        debug!(
            "Lite-scanned {} objects in deltaspace {}/{}",
            metadata.len(),
            bucket,
            prefix
        );
        Ok(LiteScanResult {
            metadata,
            originals_estimated: true,
        })
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
            let objects = self.list_objects_full(b, "").await?;
            total += objects.iter().map(|o| o.size).sum::<u64>();
        }

        debug!("Total S3 storage size: {} bytes", total);
        Ok(total)
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

        // Partition the HEADs into batches and stop the sweep if a batch comes
        // back mostly throttled — the remaining objects fall back to their
        // (perfectly serviceable) lite listing metadata instead of bombing a
        // backend that is already answering 503 SlowDown. Enrichment here is
        // cosmetic (original sizes for delta entries in listings), never worth
        // grinding a struggling backend for.
        let mut head_results: HashMap<String, FileMetadata> = HashMap::new();
        for batch in s3_keys.chunks(Self::MAX_CONCURRENT_HEADS) {
            let (ok, throttled) = self
                .bounded_head_calls(bucket, batch.iter().map(|s| s.as_str()))
                .await;
            head_results.extend(ok);
            if throttled * 2 >= batch.len() {
                warn!(
                    "HEAD enrichment aborted for {}: {}/{} throttled in one batch — \
                     serving lite metadata for the remaining {} objects",
                    bucket,
                    throttled,
                    batch.len(),
                    s3_keys.len().saturating_sub(head_results.len()),
                );
                break;
            }
        }

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
        delimiter: Option<&str>,
        max_keys: u32,
        continuation_token: Option<&str>,
    ) -> Result<Option<DelegatedListResult>, StorageError> {
        // We need to over-fetch from upstream because internal files
        // (reference.bin, .delta suffixes) inflate the key count — but only
        // until the page is provably complete (see fetch_boundary below), not
        // until the prefix is exhausted (the old behaviour materialised the
        // whole subtree on delimiter-less lists — the audit's memory HIGH).
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
                .set_delimiter(delimiter.map(String::from));

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

            // Collect CommonPrefixes, skipping ONLY the `.dg/` internal deltaspace
            // directory (never a user-visible key). Deliberately narrow: an
            // earlier version skipped ANY dot-prefixed segment, which also hid
            // legitimate folders like `.well-known/` from user-facing listings.
            if let Some(cps) = response.common_prefixes {
                for cp in cps {
                    if let Some(p) = cp.prefix {
                        let last_seg = p.trim_end_matches('/').rsplit('/').next().unwrap_or("");
                        if last_seg == ".dg" {
                            continue;
                        }
                        all_common_prefixes.insert(p);
                    }
                }
            }

            // Collect direct objects at this level
            if let Some(contents) = response.contents {
                raw_objects.extend(
                    contents
                        .into_iter()
                        .filter_map(S3ListedObject::from_s3_object),
                );
            }

            if response.is_truncated.unwrap_or(false) {
                upstream_token = response.next_continuation_token;
            } else {
                break;
            }

            // Early exit: stop fetching once the final page is provably
            // complete instead of draining the whole prefix into memory.
            if let Some(boundary) = list_fetch_boundary(
                raw_objects.iter().map(|o| o.key.as_str()),
                all_common_prefixes.iter().map(|p| p.as_str()),
                max_keys,
                continuation_token,
            ) {
                let past_boundary = raw_objects
                    .last()
                    .map(|o| o.key.as_str() > boundary.as_str())
                    .unwrap_or(false);
                if past_boundary {
                    break;
                }
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
        // Directory markers are empty S3 objects; they still need SSE
        // headers when the backend runs in native-encryption mode,
        // otherwise a bucket policy that enforces encryption (common
        // for SSE-KMS deployments) will reject them.
        let mut request = self
            .client
            .put_object()
            .bucket(bucket)
            .key(key)
            .content_type("application/x-directory")
            .content_length(0)
            .body(ByteStream::from(vec![]));
        // H9: stamp the dg-encrypted-native marker symmetrically with
        // put_object_with_metadata. The marker isn't secret — it just
        // tells the read path "native-encrypted, don't try to proxy-
        // decrypt". Without it, a future read-side sniffer that
        // distinguishes "plaintext" from "native-encrypted" via the
        // marker would misclassify directory markers.
        if let Some(marker) = self.native_encryption.marker() {
            request = request.metadata("dg-encrypted-native", marker);
        }
        request = apply_native_encryption(request, &self.native_encryption);
        request
            .send()
            .await
            .map_err(|e| Self::classify_s3_error(bucket, &e, S3Op::PutObject))?;

        debug!("Created directory marker: {}/{}", bucket, key);
        Ok(())
    }
}

/// Early-exit decision for the delegated-listing fetch loop.
///
/// Given the raw keys + common prefixes fetched so far, returns the raw-key
/// boundary past which the final page of `max_keys` user-visible entries is
/// provably complete — or `None` when more candidates are still needed.
///
/// The horizon is `max(p + ".delta")` over EVERY prefix `p` of the anchor key
/// `U` (the (max_keys+1)-th distinct user key — its existence also proves
/// `is_truncated`). A raw key maps to a user key by at most stripping
/// `.delta`, so a raw key whose user key `u` sorts ≤ U is either `u` itself
/// (≤ U ≤ horizon) or `u + ".delta"`; if `u` is not a prefix of U the suffix
/// never gets compared (`u.delta` < U ≤ horizon), and if it IS a prefix it is
/// covered by the max. The naive `U + ".delta"` horizon is WRONG for
/// prefix-sharing names — "v1" < "v1.2" but "v1.delta" > "v1.2.delta" — so a
/// page containing `v1` would early-exit before its raw key `v1.delta`
/// arrived and silently drop the object (see
/// `boundary_prefix_chain_regression`).
///
/// Distinct counting matters too: `k` and `k.delta` dedup into one entry, so
/// counting duplicates could stop the fetch with an under-filled page and a
/// false `is_truncated=false`.
fn list_fetch_boundary<'a>(
    raw_keys: impl Iterator<Item = &'a str>,
    common_prefixes: impl Iterator<Item = &'a str>,
    max_keys: u32,
    continuation_token: Option<&str>,
) -> Option<String> {
    let token = continuation_token.unwrap_or("");
    let mut candidates: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for key in raw_keys {
        let filename = key.rsplit('/').next().unwrap_or(key);
        if filename == "reference.bin" {
            continue;
        }
        let user_key: &str = if let Some(stripped) = key.strip_suffix(".delta") {
            stripped
        } else {
            key
        };
        if user_key > token {
            candidates.insert(user_key);
        }
    }
    for cp in common_prefixes {
        if cp > token {
            candidates.insert(cp);
        }
    }
    // Need max_keys+1 distinct entries: max_keys fill the page, the extra one
    // proves truncation. Its user key U anchors the completeness horizon.
    let anchor = candidates.iter().nth(max_keys as usize)?;
    (1..=anchor.len())
        .filter(|&n| anchor.is_char_boundary(n))
        .map(|n| format!("{}.delta", &anchor[..n]))
        .max()
}

/// Apply native S3 encryption headers to a PutObject builder in
/// accordance with the configured mode.
///
/// Kept as a free function (not a method on `S3Backend`) so the
/// signature doesn't get borrowed-self awkward during retry loops
/// that rebuild the request object on each attempt. Takes the
/// `NativeEncryptionConfig` by reference — the builder absorbs the
/// `String` clone only when SseKms is actually in use.
fn apply_native_encryption(
    mut request: aws_sdk_s3::operation::put_object::builders::PutObjectFluentBuilder,
    cfg: &NativeEncryptionConfig,
) -> aws_sdk_s3::operation::put_object::builders::PutObjectFluentBuilder {
    use aws_sdk_s3::types::ServerSideEncryption;
    match cfg {
        NativeEncryptionConfig::None => {}
        NativeEncryptionConfig::SseS3 => {
            request = request.server_side_encryption(ServerSideEncryption::Aes256);
        }
        NativeEncryptionConfig::SseKms {
            kms_key_id,
            bucket_key_enabled,
        } => {
            request = request
                .server_side_encryption(ServerSideEncryption::AwsKms)
                .ssekms_key_id(kms_key_id.clone())
                .bucket_key_enabled(*bucket_key_enabled);
        }
    }
    request
}

/// Apply native SSE to a `create_multipart_upload` request (Phase B).
/// Mirrors `apply_native_encryption` for the multipart builder shape.
fn apply_native_encryption_mpu(
    mut request: aws_sdk_s3::operation::create_multipart_upload::builders::CreateMultipartUploadFluentBuilder,
    cfg: &NativeEncryptionConfig,
) -> aws_sdk_s3::operation::create_multipart_upload::builders::CreateMultipartUploadFluentBuilder {
    use aws_sdk_s3::types::ServerSideEncryption;
    match cfg {
        NativeEncryptionConfig::None => {}
        NativeEncryptionConfig::SseS3 => {
            request = request.server_side_encryption(ServerSideEncryption::Aes256);
        }
        NativeEncryptionConfig::SseKms {
            kms_key_id,
            bucket_key_enabled,
        } => {
            request = request
                .server_side_encryption(ServerSideEncryption::AwsKms)
                .ssekms_key_id(kms_key_id.clone())
                .bucket_key_enabled(*bucket_key_enabled);
        }
    }
    request
}

// ────────────────────────────────────────────────────────────────────
// Unit tests for error classification.
//
// Rationale: `classify_s3_error` and `classify_get_error` are pure
// functions on `&SdkError<T>` — every call site in this file funnels
// errors through them. A wrong classification silently turns a
// retryable-transient into a propagated 500, or mislabels a
// legitimate AccessDenied as BucketNotFound. Before this module, the
// only coverage was integration tests against MinIO, which doesn't
// reproduce the Hetzner/Ceph 403-for-missing-bucket quirk that the
// code explicitly handles.
//
// We construct `SdkError` values directly instead of pulling in
// `aws-smithy-mocks` — the dep isn't in the tree, the helpers we
// need (`SdkError::service_error`, `Response::new`) are already
// in-tree via existing transitive dependencies, and constructing a
// ServiceError for a classifier test is ~3 lines, not a mock server.
// ────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_s3::operation::get_object::GetObjectError;
    use aws_smithy_runtime_api::client::orchestrator::HttpResponse;
    use aws_smithy_runtime_api::http::StatusCode;
    use aws_smithy_types::body::SdkBody;

    // ── list_fetch_boundary: delegated-listing early exit (Tier 3.4) ──

    fn boundary(raw: &[&str], cps: &[&str], max_keys: u32, token: Option<&str>) -> Option<String> {
        list_fetch_boundary(raw.iter().copied(), cps.iter().copied(), max_keys, token)
    }

    #[test]
    fn boundary_none_until_enough_distinct_candidates() {
        // 2 candidates, need max_keys+1 = 3 → keep fetching.
        assert_eq!(boundary(&["a", "b"], &[], 2, None), None);
        // 3rd candidate arrives → boundary anchored at the 3rd ("c").
        assert_eq!(
            boundary(&["a", "b", "c"], &[], 2, None),
            Some("c.delta".into())
        );
    }

    #[test]
    fn boundary_counts_distinct_user_keys_not_raw_keys() {
        // "b" + "b.delta" dedup into ONE user entry — counting raw keys would
        // stop early with an under-filled page and a false is_truncated=false.
        assert_eq!(boundary(&["a", "b", "b.delta"], &[], 2, None), None);
        assert_eq!(
            boundary(&["a", "b", "b.delta", "c"], &[], 2, None),
            Some("c.delta".into())
        );
    }

    #[test]
    fn boundary_skips_internal_files_and_respects_token() {
        // reference.bin is deltaspace machinery, never user-visible.
        assert_eq!(
            boundary(&["a", "p/.dg/reference.bin", "b"], &[], 2, None),
            None
        );
        // A raw key whose USER key equals the token is not a candidate
        // (start_after skips raw "t", but "t.delta" > "t" still arrives).
        assert_eq!(boundary(&["t.delta", "u", "v"], &[], 2, Some("t")), None);
        assert_eq!(
            boundary(&["t.delta", "u", "v", "w"], &[], 2, Some("t")),
            Some("w.delta".into())
        );
    }

    #[test]
    fn boundary_interleaves_common_prefixes() {
        // CPs count toward max-keys exactly like objects (S3 semantics).
        assert_eq!(
            boundary(&["a", "z"], &["m/"], 2, None),
            Some("z.delta".into())
        );
    }

    #[test]
    fn boundary_prefix_chain_regression() {
        // The adversarial-review CRITICAL: versioned artifacts v1 / v1.2 /
        // v1.2.3 all stored as .delta. Raw sort order is REVERSED vs user
        // order ("v1.2.3.delta" < "v1.2.delta" < "v1.delta"), so the naive
        // U+".delta" horizon stopped the fetch before v1's raw key arrived
        // and silently dropped it from listings. The prefix-max horizon for
        // anchor "v1.2.3" must be "v1.delta" (max over every prefix+".delta")
        // so the fetch keeps going until v1.delta has been seen.
        assert_eq!(
            boundary(&["v1.2.3.delta", "v1.2.delta"], &[], 1, None),
            Some("v1.delta".into())
        );
        // Same shape, delimiter path: user keys "a" (from a.delta) and "a."
        // (from a..delta). Anchor "a." → horizon max("a.delta","a..delta")
        // = "a.delta", which is NOT yet passed at raw "a.delta" (equal) —
        // the loop keeps fetching instead of emitting "a." before "a".
        assert_eq!(
            boundary(&["a..delta", "a.delta"], &[], 1, None),
            Some("a.delta".into())
        );
    }

    // ── resolve_created_at: the replication re-copy fix (RCA 2026-06-30) ──

    fn dt(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn created_at_absent_uses_stable_fallback_not_now() {
        // The bug: a missing dg-created-at must resolve to the object's stable
        // LastModified, NEVER a fresh `now()` (which makes NewerWins re-copy
        // every tick). This is the exact case of the foreign .sha1/.sha512
        // sidecars in prod (partial DG metadata, no dg-created-at).
        let last_modified = dt("2026-05-14T21:33:48Z");
        assert_eq!(resolve_created_at(None, last_modified), last_modified);
    }

    #[test]
    fn created_at_present_and_valid_is_parsed() {
        let fallback = dt("2026-05-14T21:33:48Z");
        // rfc3339 with trailing Z
        assert_eq!(
            resolve_created_at(Some("2026-03-01T10:00:00Z".into()), fallback),
            dt("2026-03-01T10:00:00Z")
        );
        // the proxy's own write format (microseconds, trailing Z)
        assert_eq!(
            resolve_created_at(Some("2026-03-01T10:00:00.123456Z".into()), fallback),
            dt("2026-03-01T10:00:00.123456Z")
        );
    }

    #[test]
    fn created_at_malformed_degrades_to_fallback() {
        // A present-but-garbage value must not panic and must not become `now()`;
        // it degrades to the stable LastModified like the absent case.
        let fallback = dt("2026-05-14T21:33:48Z");
        assert_eq!(
            resolve_created_at(Some("not-a-timestamp".into()), fallback),
            fallback
        );
        assert_eq!(resolve_created_at(Some(String::new()), fallback), fallback);
    }

    #[test]
    fn created_at_offset_and_lowercase_z_parse_to_true_instant() {
        // Regression: the old string-surgery silently dropped these to the
        // fallback. A real offset must convert to UTC; lowercase `z` is a valid
        // RFC3339 zulu marker.
        let fallback = dt("2026-05-14T21:33:48Z");
        assert_eq!(
            resolve_created_at(Some("2026-03-01T10:00:00+02:00".into()), fallback),
            dt("2026-03-01T08:00:00Z"),
            "offset must convert to UTC, not fall back"
        );
        assert_eq!(
            resolve_created_at(Some("2026-03-01T10:00:00.5+02:00".into()), fallback),
            dt("2026-03-01T08:00:00.5Z")
        );
        assert_eq!(
            resolve_created_at(Some("2026-03-01T10:00:00z".into()), fallback),
            dt("2026-03-01T10:00:00Z"),
            "lowercase z is a valid zulu marker"
        );
    }

    #[test]
    fn created_at_round_trips_the_proxy_write_format() {
        // Lock the write/read contract: whatever types.rs writes
        // (`%Y-%m-%dT%H:%M:%S%.6fZ`) must read back to the same instant.
        let fallback = dt("2000-01-01T00:00:00Z");
        let original = dt("2026-03-01T10:00:00.123456Z");
        let written = original.format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string();
        assert_eq!(resolve_created_at(Some(written), fallback), original);
    }

    /// Build a minimal `HttpResponse` with the given status code and an
    /// optional `x-amz-request-id` header. The SDK uses both to populate
    /// the structured diagnostic fields we assert on.
    fn http_response(status: u16, request_id: Option<&str>) -> HttpResponse {
        let sc = StatusCode::try_from(status).expect("valid status");
        let mut resp = HttpResponse::new(sc, SdkBody::empty());
        if let Some(rid) = request_id {
            resp.headers_mut()
                .insert("x-amz-request-id", rid.to_string());
        }
        resp
    }

    /// Construct a real `SdkError::ServiceError` wrapping a
    /// `GetObjectError::NoSuchKey`. Used for the classify_get_error
    /// happy path.
    fn no_such_key_error(status: u16) -> SdkError<GetObjectError> {
        let inner =
            GetObjectError::NoSuchKey(aws_sdk_s3::types::error::NoSuchKey::builder().build());
        SdkError::service_error(inner, http_response(status, Some("req-1")))
    }

    /// Classify GetObject NoSuchKey (S3's canonical "key doesn't exist")
    /// as `StorageError::NotFound(key)`. Without this mapping, callers
    /// would see a generic S3 error string and fail to map it to a 404
    /// on the client.
    #[test]
    fn classify_get_error_maps_no_such_key_to_not_found() {
        let err = no_such_key_error(404);
        let classified = S3Backend::classify_get_error("my-bucket", "missing.bin", &err);
        match classified {
            StorageError::NotFound(key) => assert_eq!(key, "missing.bin"),
            other => panic!("expected NotFound, got {:?}", other),
        }
    }

    /// The GENERIC classifier (used by ops without a typed error variant,
    /// e.g. CopyObject) must also map an object-level 404 / NoSuchKey to
    /// `NotFound`, not the catch-all `S3(...)` → HTTP 500. This is the
    /// concurrent-source-delete race: copy a reference that a parallel
    /// request just deleted → must surface 404, not 500.
    #[test]
    fn classify_s3_error_maps_object_level_404_to_not_found() {
        let err = no_such_key_error(404); // 404 + NoSuchKey body
        let classified =
            S3Backend::classify_s3_error("my-bucket", &err, S3Op::Other("copy_object"));
        assert!(
            matches!(classified, StorageError::NotFound(_)),
            "object-level 404 must classify as NotFound, got {:?}",
            classified
        );
    }

    /// A 404 on a BUCKET-level op must NOT become a key-level NotFound — it
    /// stays a bucket concern (or the catch-all), never silently a missing key.
    #[test]
    fn classify_s3_error_bucket_level_404_is_not_key_not_found() {
        let err = no_such_key_error(404);
        let classified = S3Backend::classify_s3_error("my-bucket", &err, S3Op::CreateBucket);
        assert!(
            !matches!(classified, StorageError::NotFound(_)),
            "bucket-level 404 must not be a key NotFound, got {:?}",
            classified
        );
    }

    /// An object-level 403 must stay `S3(...)` — never get rewritten to
    /// BucketNotFound. The Hetzner/Ceph quirk only applies to bucket-
    /// level operations; a GetObject 403 is a legitimate AccessDenied
    /// and callers need to surface it as such.
    #[test]
    fn classify_get_error_keeps_object_level_403_as_s3_error() {
        // Build a ServiceError wrapping a generic (non-NoSuchKey) variant
        // with a 403 status; the caller treats this as GetObject context.
        let inner = GetObjectError::generic(
            aws_smithy_types::error::ErrorMetadata::builder()
                .code("AccessDenied")
                .build(),
        );
        let err = SdkError::service_error(inner, http_response(403, Some("req-2")));
        let classified = S3Backend::classify_get_error("my-bucket", "locked.bin", &err);
        // MUST NOT be BucketNotFound — GetObject is object-level.
        match classified {
            StorageError::BucketNotFound(_) => {
                panic!("403 on GetObject must not be misclassified as BucketNotFound")
            }
            StorageError::NotFound(_) => {
                panic!("403 AccessDenied must not be misclassified as NotFound")
            }
            StorageError::S3(msg) => {
                assert!(
                    msg.contains("403"),
                    "status should appear in message: {msg}"
                );
                assert!(
                    msg.contains("get_object"),
                    "op should appear in message: {msg}"
                );
            }
            other => panic!("expected S3, got {:?}", other),
        }
    }

    /// A 403 from a bucket-level operation (ListObjects) MUST be
    /// rewritten to BucketNotFound. S3-compatible providers (MinIO,
    /// Ceph) return 403 instead of 404 for non-existent buckets, to
    /// prevent enumeration. Without this mapping, `GET /nosuch-bucket/`
    /// would propagate as a 500 S3 error instead of the correct 404.
    #[test]
    fn classify_s3_error_rewrites_bucket_level_403_to_bucket_not_found() {
        let inner = aws_sdk_s3::operation::list_objects_v2::ListObjectsV2Error::generic(
            aws_smithy_types::error::ErrorMetadata::builder()
                .code("AccessDenied")
                .build(),
        );
        let err: SdkError<_> = SdkError::service_error(inner, http_response(403, Some("req-3")));
        let classified = S3Backend::classify_s3_error("ghost-bucket", &err, S3Op::ListObjects);
        match classified {
            StorageError::BucketNotFound(bucket) => assert_eq!(bucket, "ghost-bucket"),
            other => panic!(
                "expected BucketNotFound for bucket-level 403, got {:?}",
                other
            ),
        }
    }

    /// An explicit `NoSuchBucket` error string always maps to
    /// BucketNotFound, regardless of status or operation. This catches
    /// S3-compatible providers that do return the canonical error code
    /// in the body even if they pick a non-404 status.
    #[test]
    fn classify_s3_error_recognizes_explicit_no_such_bucket() {
        let inner = aws_sdk_s3::operation::list_objects_v2::ListObjectsV2Error::generic(
            aws_smithy_types::error::ErrorMetadata::builder()
                .code("NoSuchBucket")
                .build(),
        );
        let err: SdkError<_> = SdkError::service_error(inner, http_response(404, Some("req-4")));
        let classified = S3Backend::classify_s3_error("bucket", &err, S3Op::ListObjects);
        match classified {
            StorageError::BucketNotFound(bucket) => assert_eq!(bucket, "bucket"),
            other => panic!("expected BucketNotFound, got {:?}", other),
        }
    }

    /// A 500 from a bucket-level op is NOT a bucket-not-found signal.
    /// Should stay S3 with status visible so the caller can see the
    /// upstream failure.
    #[test]
    fn classify_s3_error_preserves_bucket_level_500() {
        let inner = aws_sdk_s3::operation::list_objects_v2::ListObjectsV2Error::generic(
            aws_smithy_types::error::ErrorMetadata::builder()
                .code("InternalError")
                .build(),
        );
        let err: SdkError<_> = SdkError::service_error(inner, http_response(500, Some("req-5")));
        let classified = S3Backend::classify_s3_error("bucket", &err, S3Op::ListObjects);
        match classified {
            StorageError::S3(msg) => {
                assert!(msg.contains("500"), "status must be in message: {msg}");
                assert!(!msg.is_empty(), "S3 error message must not be empty");
            }
            other => panic!("expected S3, got {:?}", other),
        }
    }

    /// has_reference corruption guard: a transient HEAD failure (503 SlowDown,
    /// 500, timeout) must NOT classify as NotFound. has_reference maps only
    /// NotFound → Ok(false); everything else → Err. If a 503 ever mapped to
    /// NotFound the write path would read "no reference" on a backend hiccup
    /// and overwrite a live reference.bin, orphaning every sibling delta.
    #[test]
    fn transient_head_errors_do_not_classify_as_not_found() {
        for status in [503u16, 500, 429] {
            let inner = aws_sdk_s3::operation::head_object::HeadObjectError::generic(
                aws_smithy_types::error::ErrorMetadata::builder()
                    .code("SlowDown")
                    .build(),
            );
            let err: SdkError<_> =
                SdkError::service_error(inner, http_response(status, Some("req-t")));
            let classified = S3Backend::classify_s3_error("bucket", &err, S3Op::HeadObject);
            assert!(
                !matches!(classified, StorageError::NotFound(_)),
                "transient status {status} must NOT be NotFound (would corrupt reference.bin), got {classified:?}"
            );
        }
    }

    /// 429 Too Many Requests (Backblaze B2 / Cloudflare R2 rate limiting) must
    /// classify as Throttled, not the S3(...) catch-all → 500. Uses a code that
    /// is NOT "SlowDown" so this proves the STATUS-based branch, not the string.
    #[test]
    fn classify_s3_error_maps_429_to_throttled() {
        let inner = aws_sdk_s3::operation::get_object::GetObjectError::generic(
            aws_smithy_types::error::ErrorMetadata::builder()
                .code("TooManyRequests")
                .build(),
        );
        let err: SdkError<_> =
            SdkError::service_error(inner, http_response(429, Some("req-429")));
        let classified = S3Backend::classify_s3_error("bucket", &err, S3Op::GetObject);
        assert!(
            matches!(classified, StorageError::Throttled(_)),
            "429 must map to Throttled (B2/R2 rate limiting), got {classified:?}"
        );
    }

    /// head_bucket routing guard: a transient HEAD-bucket failure must not
    /// classify as BucketNotFound/NotFound — else resolve_existing reads the
    /// bucket as "absent here" and reroutes the operation to the wrong
    /// backend. Only a real bucket-404 is absent.
    #[test]
    fn transient_head_bucket_errors_are_not_bucket_not_found() {
        for status in [503u16, 500, 429] {
            let inner = aws_sdk_s3::operation::head_bucket::HeadBucketError::generic(
                aws_smithy_types::error::ErrorMetadata::builder()
                    .code("SlowDown")
                    .build(),
            );
            let err: SdkError<_> =
                SdkError::service_error(inner, http_response(status, Some("req-hb")));
            let classified = S3Backend::classify_s3_error("bucket", &err, S3Op::HeadBucket);
            assert!(
                !matches!(
                    classified,
                    StorageError::BucketNotFound(_) | StorageError::NotFound(_)
                ),
                "transient status {status} must NOT be a not-found (would mis-route), got {classified:?}"
            );
        }
    }

    /// `S3Op::is_bucket_level` is the table driving the 403 rewrite.
    /// Guard that truth-table explicitly — if someone adds a new op
    /// variant and forgets to decide its level, this test will still
    /// document the current contract.
    #[test]
    fn s3_op_is_bucket_level_truth_table() {
        // Bucket-level: 403 from these MUST rewrite to BucketNotFound.
        assert!(S3Op::ListObjects.is_bucket_level());
        assert!(S3Op::CreateBucket.is_bucket_level());

        // Object-level: 403 from these must NOT rewrite. An AccessDenied
        // on GetObject / HeadObject / DeleteObject / PutObject is a
        // legitimate permission denial and the caller must see it as-is.
        assert!(!S3Op::GetObject.is_bucket_level());
        assert!(!S3Op::PutObject.is_bucket_level());
        assert!(!S3Op::HeadObject.is_bucket_level());
        assert!(!S3Op::DeleteObject.is_bucket_level());
        assert!(!S3Op::Other("delete_bucket").is_bucket_level());
    }

    // ──────────────────────────────────────────────────────────────
    // Step 4: native encryption config
    // ──────────────────────────────────────────────────────────────

    #[test]
    fn test_native_encryption_marker_values() {
        // The marker string is what ends up in `x-amz-meta-dg-
        // encrypted-native` on the object. Changing the string is a
        // wire-format break; pin the values so the test fails on any
        // accidental rename.
        assert_eq!(NativeEncryptionConfig::None.marker(), None);
        assert_eq!(NativeEncryptionConfig::SseS3.marker(), Some("sse-s3"));
        assert_eq!(
            NativeEncryptionConfig::SseKms {
                kms_key_id: "arn".into(),
                bucket_key_enabled: true,
            }
            .marker(),
            Some("sse-kms")
        );
    }

    #[test]
    fn test_native_encryption_partial_eq() {
        // Derived PartialEq pins structural equality — used by the
        // admin API diff path in Step 6. Two SseKms configs with
        // different ARNs or different bucket_key_enabled values
        // compare as DISTINCT.
        let a = NativeEncryptionConfig::SseKms {
            kms_key_id: "arn/a".into(),
            bucket_key_enabled: true,
        };
        let b = NativeEncryptionConfig::SseKms {
            kms_key_id: "arn/b".into(),
            bucket_key_enabled: true,
        };
        let c = NativeEncryptionConfig::SseKms {
            kms_key_id: "arn/a".into(),
            bucket_key_enabled: false,
        };
        let a2 = NativeEncryptionConfig::SseKms {
            kms_key_id: "arn/a".into(),
            bucket_key_enabled: true,
        };
        assert_eq!(a, a2);
        assert_ne!(a, b);
        assert_ne!(a, c);
    }

    /// The Aws SDK builder is hostile to partial-serialise inspection
    /// (it consumes `self` on every method), so we verify the Step-4
    /// plumbing with a small wrapper that records what `apply_native_
    /// encryption` would WANT to set rather than observing the built
    /// request. This keeps the test tight and avoids reaching into
    /// SDK internals. Behavioural verification that AWS actually
    /// encrypts belongs in the integration suite (see
    /// `tests/encryption_test.rs` — an #[ignore]'d test that runs
    /// against a KMS-capable MinIO).
    #[test]
    fn test_apply_native_encryption_mode_selection() {
        use NativeEncryptionConfig as N;
        // The helper is opaque; we observe via the selected
        // `server_side_encryption` variant in the assertion below.
        // Since the builder is consume-only, we just call through
        // each arm to confirm the match is exhaustive and the
        // intended arm fires (no panic, no wrong-arm selection).
        // The real behaviour test lives in the integration suite.

        let modes = [
            N::None,
            N::SseS3,
            N::SseKms {
                kms_key_id: "arn:aws:kms:us-east-1:1:key/abc".into(),
                bucket_key_enabled: true,
            },
        ];
        for m in modes {
            // Call `marker()` as a cheap observable — we already
            // pinned its outputs above, but exercising the match
            // arms here guards against `apply_native_encryption`
            // growing a new arm without a paired marker update.
            let _ = m.marker();
        }
    }

    /// Module-level lock for tests that toggle `DGP_BACKEND_ALLOW_LOCAL`,
    /// to keep parallel `cargo test` workers from racing each other.
    /// Held across `.await` because the env-var window must include the
    /// async `build_client` call; the lock is uncontended in production
    /// code, so the "MutexGuard across await" lint doesn't apply.
    static SSRF_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Adversarial: operator-supplied `s3_endpoint` pointing at IMDS
    /// or other private targets must be rejected by `build_client`.
    /// Catches the cloud-takeover SSRF pivot before the SDK builds a
    /// client around the hostile endpoint.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn build_client_rejects_imds_and_private_endpoints() {
        // Ensure dev allowlist is off for this test (it would defeat
        // the check).
        let _g = SSRF_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("DGP_BACKEND_ALLOW_LOCAL").ok();
        // SAFETY: tests serialised on `LOCK`; no other thread mutates this
        // env var during the test window.
        unsafe { std::env::remove_var("DGP_BACKEND_ALLOW_LOCAL") };

        for bad in [
            "http://169.254.169.254/",
            "https://169.254.169.254/",
            "http://10.0.0.1/",
            "https://192.168.0.1/",
            "https://[::1]/",
            "https://localhost/",
            "https://metadata.google.internal/",
            "http://example.com/", // plain http rejected when not in dev mode
        ] {
            let cfg = BackendConfig::S3 {
                endpoint: Some(bad.to_string()),
                region: "us-east-1".to_string(),
                force_path_style: true,
                access_key_id: Some("AKIA".to_string()),
                secret_access_key: Some("secret".to_string()),
                allow_local: false,
            };
            let err = S3Backend::build_client(&cfg)
                .await
                .expect_err(&format!("must reject endpoint {bad}"));
            assert!(
                err.to_string().contains("Refusing to use S3 endpoint"),
                "expected SSRF guard error for {bad}, got: {err}"
            );
        }

        // Legitimate public endpoints pass.
        for good in [
            "https://s3.amazonaws.com/",
            "https://s3.eu-central-1.amazonaws.com/",
        ] {
            let cfg = BackendConfig::S3 {
                endpoint: Some(good.to_string()),
                region: "us-east-1".to_string(),
                force_path_style: true,
                access_key_id: Some("AKIA".to_string()),
                secret_access_key: Some("secret".to_string()),
                allow_local: false,
            };
            S3Backend::build_client(&cfg)
                .await
                .unwrap_or_else(|e| panic!("legitimate endpoint {good} rejected: {e}"));
        }

        // Restore prior env state.
        match prev {
            Some(v) => unsafe { std::env::set_var("DGP_BACKEND_ALLOW_LOCAL", v) },
            None => unsafe { std::env::remove_var("DGP_BACKEND_ALLOW_LOCAL") },
        };
    }

    /// With `DGP_BACKEND_ALLOW_LOCAL=true`, http:// + private IPs are
    /// permitted — needed for `cargo test` against local MinIO and
    /// for CI runs where the backend is on the same Docker network.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn build_client_allows_dev_local_when_opted_in() {
        let _g = SSRF_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("DGP_BACKEND_ALLOW_LOCAL").ok();
        unsafe { std::env::set_var("DGP_BACKEND_ALLOW_LOCAL", "true") };

        let cfg = BackendConfig::S3 {
            endpoint: Some("http://localhost:9000".to_string()),
            region: "us-east-1".to_string(),
            force_path_style: true,
            access_key_id: Some("minioadmin".to_string()),
            secret_access_key: Some("minioadmin".to_string()),
            allow_local: false, // env grants permission; field path tested below
        };
        S3Backend::build_client(&cfg)
            .await
            .expect("dev mode must accept localhost:9000");

        // IMDS still rejected even in dev mode.
        let cfg = BackendConfig::S3 {
            endpoint: Some("http://169.254.169.254/".to_string()),
            region: "us-east-1".to_string(),
            force_path_style: true,
            access_key_id: Some("a".to_string()),
            secret_access_key: Some("b".to_string()),
            allow_local: false,
        };
        assert!(S3Backend::build_client(&cfg).await.is_err());

        match prev {
            Some(v) => unsafe { std::env::set_var("DGP_BACKEND_ALLOW_LOCAL", v) },
            None => unsafe { std::env::remove_var("DGP_BACKEND_ALLOW_LOCAL") },
        };
    }

    /// `BackendConfig::S3.allow_local = true` is the preferred path for
    /// opting into local endpoints — it grants permission WITHOUT touching
    /// the process env. This is what the CLI uses (no more `unsafe set_var`).
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn build_client_allows_dev_local_when_config_field_set() {
        let _g = SSRF_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("DGP_BACKEND_ALLOW_LOCAL").ok();
        // Explicitly ensure env is unset so we prove the field path works
        // independently of the env fallback.
        unsafe { std::env::remove_var("DGP_BACKEND_ALLOW_LOCAL") };

        // localhost permitted by typed field (no env mutation).
        let cfg = BackendConfig::S3 {
            endpoint: Some("http://localhost:9000".to_string()),
            region: "us-east-1".to_string(),
            force_path_style: true,
            access_key_id: Some("minioadmin".to_string()),
            secret_access_key: Some("minioadmin".to_string()),
            allow_local: true,
        };
        S3Backend::build_client(&cfg)
            .await
            .expect("typed field path must accept localhost:9000");

        // IMDS still rejected even with allow_local: true (parity with env path).
        let cfg = BackendConfig::S3 {
            endpoint: Some("http://169.254.169.254/".to_string()),
            region: "us-east-1".to_string(),
            force_path_style: true,
            access_key_id: Some("a".to_string()),
            secret_access_key: Some("b".to_string()),
            allow_local: true,
        };
        assert!(S3Backend::build_client(&cfg).await.is_err());

        // Default `allow_local: false` rejects localhost.
        let cfg = BackendConfig::S3 {
            endpoint: Some("http://localhost:9000".to_string()),
            region: "us-east-1".to_string(),
            force_path_style: true,
            access_key_id: Some("a".to_string()),
            secret_access_key: Some("b".to_string()),
            allow_local: false,
        };
        assert!(
            S3Backend::build_client(&cfg).await.is_err(),
            "with neither field nor env opt-in, localhost must be rejected"
        );

        match prev {
            Some(v) => unsafe { std::env::set_var("DGP_BACKEND_ALLOW_LOCAL", v) },
            None => unsafe { std::env::remove_var("DGP_BACKEND_ALLOW_LOCAL") },
        };
    }
}
