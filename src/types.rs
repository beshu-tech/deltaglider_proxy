//! Core types for DeltaGlider Proxy S3-compatible storage with DeltaGlider metadata

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;

/// Tool version identifier — uses crate name and version from Cargo.toml
pub const DELTAGLIDER_TOOL: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

/// S3 metadata key names (stored as `x-amz-meta-{KEY}` in S3 headers).
/// Used in both storage/s3.rs (metadata_to_headers/headers_to_metadata)
/// and api/handlers.rs (build_metadata_headers).
///
/// The `H_*` constants are the full HTTP header names, derived from the bare
/// keys via `concat!` so they can never desync.
pub mod meta_keys {
    pub const TOOL: &str = "dg-tool";
    pub const ORIGINAL_NAME: &str = "dg-original-name";
    pub const FILE_SHA256: &str = "dg-file-sha256";
    pub const FILE_SIZE: &str = "dg-file-size";
    pub const MD5: &str = "dg-md5";
    pub const CREATED_AT: &str = "dg-created-at";
    pub const NOTE: &str = "dg-note";
    pub const SOURCE_NAME: &str = "dg-source-name";
    pub const REF_KEY: &str = "dg-ref-key";
    pub const REF_SHA256: &str = "dg-ref-sha256";
    pub const DELTA_SIZE: &str = "dg-delta-size";
    pub const DELTA_CMD: &str = "dg-delta-cmd";

    /// S3 response header prefix for user-defined metadata.
    pub const AMZ_META_PREFIX: &str = "x-amz-meta-";

    // Full x-amz-meta-dg-* header names — derived from bare keys to prevent desync.
    pub const H_TOOL: &str = concat!("x-amz-meta-", "dg-tool");
    pub const H_ORIGINAL_NAME: &str = concat!("x-amz-meta-", "dg-original-name");
    pub const H_FILE_SHA256: &str = concat!("x-amz-meta-", "dg-file-sha256");
    pub const H_FILE_SIZE: &str = concat!("x-amz-meta-", "dg-file-size");
    pub const H_NOTE: &str = concat!("x-amz-meta-", "dg-note");
    pub const H_SOURCE_NAME: &str = concat!("x-amz-meta-", "dg-source-name");
    pub const H_REF_KEY: &str = concat!("x-amz-meta-", "dg-ref-key");
    pub const H_REF_SHA256: &str = concat!("x-amz-meta-", "dg-ref-sha256");
    pub const H_DELTA_SIZE: &str = concat!("x-amz-meta-", "dg-delta-size");
    pub const H_DELTA_CMD: &str = concat!("x-amz-meta-", "dg-delta-cmd");
}

/// Errors that can occur when validating user-provided bucket/key inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyValidationError(String);

impl fmt::Display for KeyValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for KeyValidationError {}

/// S3 object key parsed into components
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ObjectKey {
    /// Bucket name
    pub bucket: String,
    /// Parent path = DeltaSpace identifier (empty string for root)
    pub prefix: String,
    /// Object filename
    pub filename: String,
}

impl ObjectKey {
    /// Parse a full S3-style key into components
    pub fn parse(bucket: &str, key: &str) -> Self {
        let key = key.trim_start_matches('/');
        let (prefix, filename) = match key.rfind('/') {
            Some(idx) => (key[..idx].to_string(), key[idx + 1..].to_string()),
            None => (String::new(), key.to_string()),
        };
        Self {
            bucket: bucket.to_string(),
            prefix,
            filename,
        }
    }

    /// Get the full key (prefix + filename)
    pub fn full_key(&self) -> String {
        if self.prefix.is_empty() {
            self.filename.clone()
        } else {
            format!("{}/{}", self.prefix, self.filename)
        }
    }

    /// Get the deltaspace identifier for this key.
    /// This is the prefix path within the bucket (no bucket name included).
    /// Bucket routing is handled at the storage layer.
    pub fn deltaspace_id(&self) -> String {
        self.prefix.clone()
    }

    /// Validate this key for object operations (PUT/GET/HEAD/DELETE).
    pub fn validate_object(&self) -> Result<(), KeyValidationError> {
        validate_key_path(&self.prefix, true)?;
        validate_key_path(&self.filename, false)?;
        if self.filename.is_empty() {
            return Err(KeyValidationError(
                "Object key must not be empty".to_string(),
            ));
        }
        if self.filename == "." || self.filename == ".." {
            return Err(KeyValidationError("Invalid object filename".to_string()));
        }
        Ok(())
    }

    /// Validate a list/query prefix for traversal and encoding hazards.
    pub fn validate_prefix(prefix: &str) -> Result<(), KeyValidationError> {
        validate_key_path(prefix, true)
    }
}

impl fmt::Display for ObjectKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.bucket, self.full_key())
    }
}

fn validate_key_path(value: &str, allow_slashes: bool) -> Result<(), KeyValidationError> {
    if value.contains('\0') {
        return Err(KeyValidationError(
            "Key must not contain NUL bytes".to_string(),
        ));
    }
    if value.contains('\\') {
        return Err(KeyValidationError(
            "Key must not contain backslashes".to_string(),
        ));
    }
    if !allow_slashes && value.contains('/') {
        return Err(KeyValidationError("Key must not contain '/'".to_string()));
    }

    for segment in value.split('/') {
        if segment == ".." {
            return Err(KeyValidationError(
                "Key must not contain '..' path segments".to_string(),
            ));
        }
    }

    Ok(())
}

/// Per-file metadata following DeltaGlider schema
/// Stored as `user.dg.metadata` extended attributes on data file inodes
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileMetadata {
    /// Tool version: "deltaglider/0.1.0"
    pub tool: String,

    /// Original filename before storage transformation
    pub original_name: String,

    /// SHA256 hash of the hydrated (original) file content
    pub file_sha256: String,

    /// Size of the hydrated (original) file in bytes
    pub file_size: u64,

    /// MD5 hash for S3 ETag compatibility
    pub md5: String,

    /// Creation timestamp (UTC ISO8601)
    pub created_at: DateTime<Utc>,

    /// Content-Type header if provided
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,

    /// User-provided custom metadata (x-amz-meta-* headers, stored without the prefix)
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub user_metadata: HashMap<String, String>,

    /// Storage type specific fields
    #[serde(flatten)]
    pub storage_info: StorageInfo,
}

/// Storage-type specific metadata fields
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "note")]
pub enum StorageInfo {
    /// Reference file - base for delta compression
    #[serde(rename = "reference")]
    Reference {
        /// Original S3 key that became the reference
        source_name: String,
    },

    /// Delta-compressed file
    #[serde(rename = "delta")]
    Delta {
        /// Path to reference file (e.g., "releases/reference.bin")
        ref_key: String,
        /// SHA256 of the reference file
        ref_sha256: String,
        /// Size of the delta file in bytes
        delta_size: u64,
        /// xdelta3 command used for encoding
        delta_cmd: String,
    },

    /// Passthrough storage — stored as-is with original filename (non-delta eligible or poor compression ratio)
    #[serde(rename = "passthrough", alias = "direct")]
    Passthrough,
}

impl StorageInfo {
    /// Consistent human-readable label for logging and headers.
    pub fn label(&self) -> &'static str {
        match self {
            StorageInfo::Reference { .. } => "reference",
            StorageInfo::Delta { .. } => "delta",
            StorageInfo::Passthrough => "passthrough",
        }
    }
}

impl FileMetadata {
    /// Create metadata for a new reference file
    pub fn new_reference(
        original_name: String,
        source_name: String,
        sha256: String,
        md5: String,
        size: u64,
        content_type: Option<String>,
    ) -> Self {
        Self {
            tool: DELTAGLIDER_TOOL.to_string(),
            original_name,
            file_sha256: sha256,
            file_size: size,
            md5,
            created_at: Utc::now(),
            content_type,
            user_metadata: HashMap::new(),
            storage_info: StorageInfo::Reference { source_name },
        }
    }

    /// Create metadata for a delta file
    #[allow(clippy::too_many_arguments)]
    pub fn new_delta(
        original_name: String,
        sha256: String,
        md5: String,
        file_size: u64,
        ref_key: String,
        ref_sha256: String,
        delta_size: u64,
        content_type: Option<String>,
    ) -> Self {
        let delta_cmd = format!(
            "xdelta3 -e -9 -s reference.bin {} {}.delta",
            original_name, original_name
        );
        Self {
            tool: DELTAGLIDER_TOOL.to_string(),
            original_name,
            file_sha256: sha256,
            file_size,
            md5,
            created_at: Utc::now(),
            content_type,
            user_metadata: HashMap::new(),
            storage_info: StorageInfo::Delta {
                ref_key,
                ref_sha256,
                delta_size,
                delta_cmd,
            },
        }
    }

    /// Create metadata for a passthrough file (stored as-is with original name)
    pub fn new_passthrough(
        original_name: String,
        sha256: String,
        md5: String,
        size: u64,
        content_type: Option<String>,
    ) -> Self {
        Self {
            tool: DELTAGLIDER_TOOL.to_string(),
            original_name,
            file_sha256: sha256,
            file_size: size,
            md5,
            created_at: Utc::now(),
            content_type,
            user_metadata: HashMap::new(),
            storage_info: StorageInfo::Passthrough,
        }
    }

    /// Create metadata for an S3 directory marker (zero-byte "folder/" object).
    pub fn directory_marker(key: &str) -> Self {
        Self {
            tool: DELTAGLIDER_TOOL.to_string(),
            original_name: key.to_string(),
            file_sha256: String::new(),
            file_size: 0,
            md5: "d41d8cd98f00b204e9800998ecf8427e".to_string(),
            created_at: Utc::now(),
            content_type: Some("application/x-directory".to_string()),
            user_metadata: HashMap::new(),
            storage_info: StorageInfo::Passthrough,
        }
    }

    /// Get ETag value (quoted MD5)
    pub fn etag(&self) -> String {
        format!("\"{}\"", self.md5)
    }

    /// Check if this is a reference file
    pub fn is_reference(&self) -> bool {
        matches!(self.storage_info, StorageInfo::Reference { .. })
    }

    /// Check if this is a delta file
    pub fn is_delta(&self) -> bool {
        matches!(self.storage_info, StorageInfo::Delta { .. })
    }

    /// Get the delta size if this is a delta file
    pub fn delta_size(&self) -> Option<u64> {
        match &self.storage_info {
            StorageInfo::Delta { delta_size, .. } => Some(*delta_size),
            _ => None,
        }
    }

    /// Get compression ratio if this is a delta file
    pub fn compression_ratio(&self) -> Option<f32> {
        match &self.storage_info {
            StorageInfo::Delta { delta_size, .. } => {
                Some(*delta_size as f32 / self.file_size as f32)
            }
            _ => None,
        }
    }
}

/// Result of a storage operation
#[derive(Debug, Clone)]
pub struct StoreResult {
    pub metadata: FileMetadata,
    /// Actual bytes written to storage (may be less than original for deltas)
    pub stored_size: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_object_key_parse() {
        let key = ObjectKey::parse("mybucket", "releases/v1.0.0/app.zip");
        assert_eq!(key.bucket, "mybucket");
        assert_eq!(key.prefix, "releases/v1.0.0");
        assert_eq!(key.filename, "app.zip");
        assert_eq!(key.deltaspace_id(), "releases/v1.0.0");
    }

    #[test]
    fn test_object_key_parse_root() {
        let key = ObjectKey::parse("mybucket", "file.zip");
        assert_eq!(key.prefix, "");
        assert_eq!(key.filename, "file.zip");
        assert_eq!(key.deltaspace_id(), ""); // Root-level files have empty deltaspace_id
    }

    #[test]
    fn test_object_key_parse_leading_slash() {
        let key = ObjectKey::parse("mybucket", "/path/to/file.zip");
        assert_eq!(key.prefix, "path/to");
        assert_eq!(key.filename, "file.zip");
    }

    #[test]
    fn test_reference_metadata() {
        let meta = FileMetadata::new_reference(
            "app.zip".to_string(),
            "releases/v1.0/app.zip".to_string(),
            "abc123".to_string(),
            "def456".to_string(),
            1024,
            Some("application/zip".to_string()),
        );
        assert!(meta.is_reference());
        assert!(!meta.is_delta());
        assert_eq!(meta.tool, DELTAGLIDER_TOOL);
    }

    #[test]
    fn test_delta_metadata() {
        let meta = FileMetadata::new_delta(
            "app.zip".to_string(),
            "abc123".to_string(),
            "def456".to_string(),
            1024,
            "releases/v1.0/reference.bin".to_string(),
            "ref_sha".to_string(),
            256,
            None,
        );
        assert!(meta.is_delta());
        assert_eq!(meta.delta_size(), Some(256));
        assert_eq!(meta.compression_ratio(), Some(0.25));
    }

    #[test]
    fn test_metadata_serialization() {
        let meta = FileMetadata::new_delta(
            "app.zip".to_string(),
            "abc123".to_string(),
            "def456".to_string(),
            1024,
            "releases/reference.bin".to_string(),
            "ref_sha".to_string(),
            256,
            None,
        );
        let json = serde_json::to_string_pretty(&meta).unwrap();
        assert!(json.contains(DELTAGLIDER_TOOL));
        assert!(json.contains("ref_key"));
        assert!(json.contains("delta_cmd"));

        // Deserialize back
        let parsed: FileMetadata = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_delta());
    }

    // === Key validation security tests ===

    #[test]
    fn test_validate_rejects_path_traversal() {
        let key = ObjectKey::parse("bucket", "../../../etc/passwd");
        assert!(key.validate_object().is_err());
    }

    #[test]
    fn test_validate_rejects_backslash() {
        let key = ObjectKey::parse("bucket", "path\\file");
        assert!(key.validate_object().is_err());
    }

    #[test]
    fn test_validate_rejects_nul_byte() {
        let key = ObjectKey::parse("bucket", "path\0file");
        assert!(key.validate_object().is_err());
    }

    #[test]
    fn test_validate_rejects_empty_filename() {
        let key = ObjectKey::parse("bucket", "prefix/");
        assert!(key.validate_object().is_err());
    }

    #[test]
    fn test_validate_rejects_dot_dot_filename() {
        let key = ObjectKey::parse("bucket", "..");
        assert!(key.validate_object().is_err());
    }

    #[test]
    fn test_validate_prefix_rejects_traversal() {
        assert!(ObjectKey::validate_prefix("../bad").is_err());
    }

    #[test]
    fn test_validate_prefix_allows_normal() {
        assert!(ObjectKey::validate_prefix("releases/v1.0/").is_ok());
    }
}
