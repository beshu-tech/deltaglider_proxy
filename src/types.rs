//! Core types for DeltaGlider Proxy S3-compatible storage with DeltaGlider metadata

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Tool version identifier
pub const DELTAGLIDER_TOOL: &str = "deltaglider/0.1.0";

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
    /// Virtual bucket name
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

    /// Get the deltaspace identifier for this key
    /// Returns empty string for root-level files (no prefix folder created)
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
/// Stored as sidecar .meta JSON files alongside data files
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

    /// Direct storage (non-delta eligible or poor compression ratio)
    #[serde(rename = "direct")]
    Direct,
}

impl StorageInfo {
    /// Consistent human-readable label for logging and headers.
    pub fn label(&self) -> &'static str {
        match self {
            StorageInfo::Reference { .. } => "reference",
            StorageInfo::Delta { .. } => "delta",
            StorageInfo::Direct => "direct",
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
            storage_info: StorageInfo::Delta {
                ref_key,
                ref_sha256,
                delta_size,
                delta_cmd,
            },
        }
    }

    /// Create metadata for a direct file
    pub fn new_direct(
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
            storage_info: StorageInfo::Direct,
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
        assert!(json.contains("deltaglider/0.1.0"));
        assert!(json.contains("ref_key"));
        assert!(json.contains("delta_cmd"));

        // Deserialize back
        let parsed: FileMetadata = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_delta());
    }
}
