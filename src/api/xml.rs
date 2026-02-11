//! S3 XML response builders and parsers

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// S3 object in list response
#[derive(Debug, Clone, Serialize)]
pub struct S3Object {
    pub key: String,
    pub size: u64,
    pub last_modified: DateTime<Utc>,
    pub etag: String,
    pub storage_class: String,
}

impl S3Object {
    pub fn new(key: String, size: u64, last_modified: DateTime<Utc>, etag: String) -> Self {
        Self {
            key,
            size,
            last_modified,
            etag,
            storage_class: "STANDARD".to_string(),
        }
    }
}

/// ListObjectsV2 response
#[derive(Debug, Clone)]
pub struct ListBucketResult {
    pub name: String,
    pub prefix: String,
    pub max_keys: u32,
    pub key_count: u32,
    pub is_truncated: bool,
    pub contents: Vec<S3Object>,
    pub continuation_token: Option<String>,
    pub next_continuation_token: Option<String>,
}

impl ListBucketResult {
    pub fn new_v2(
        name: String,
        prefix: String,
        max_keys: u32,
        contents: Vec<S3Object>,
        continuation_token: Option<String>,
        next_continuation_token: Option<String>,
        is_truncated: bool,
    ) -> Self {
        let key_count = contents.len() as u32;
        Self {
            name,
            prefix,
            max_keys,
            key_count,
            is_truncated,
            contents,
            continuation_token,
            next_continuation_token,
        }
    }

    /// Convert to S3 XML format
    pub fn to_xml(&self) -> String {
        let mut xml = String::new();
        xml.push_str(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
        xml.push('\n');
        xml.push_str(r#"<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">"#);
        xml.push('\n');

        xml.push_str(&format!("  <Name>{}</Name>\n", escape_xml(&self.name)));
        xml.push_str(&format!(
            "  <Prefix>{}</Prefix>\n",
            escape_xml(&self.prefix)
        ));
        xml.push_str(&format!("  <MaxKeys>{}</MaxKeys>\n", self.max_keys));
        xml.push_str(&format!("  <KeyCount>{}</KeyCount>\n", self.key_count));
        xml.push_str(&format!(
            "  <IsTruncated>{}</IsTruncated>\n",
            self.is_truncated
        ));

        if let Some(ref token) = self.continuation_token {
            xml.push_str(&format!(
                "  <ContinuationToken>{}</ContinuationToken>\n",
                escape_xml(token)
            ));
        }

        if let Some(ref token) = self.next_continuation_token {
            xml.push_str(&format!(
                "  <NextContinuationToken>{}</NextContinuationToken>\n",
                escape_xml(token)
            ));
        }

        for obj in &self.contents {
            xml.push_str("  <Contents>\n");
            xml.push_str(&format!("    <Key>{}</Key>\n", escape_xml(&obj.key)));
            xml.push_str(&format!(
                "    <LastModified>{}</LastModified>\n",
                obj.last_modified.format("%Y-%m-%dT%H:%M:%S%.3fZ")
            ));
            xml.push_str(&format!("    <ETag>{}</ETag>\n", escape_xml(&obj.etag)));
            xml.push_str(&format!("    <Size>{}</Size>\n", obj.size));
            xml.push_str(&format!(
                "    <StorageClass>{}</StorageClass>\n",
                obj.storage_class
            ));
            xml.push_str("  </Contents>\n");
        }

        xml.push_str("</ListBucketResult>");
        xml
    }
}

/// Escape special XML characters
pub fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

// ============================================================================
// DeleteObjects Request/Response
// ============================================================================

/// Delete request object
#[derive(Debug, Clone, Deserialize)]
pub struct DeleteObjectIdentifier {
    #[serde(rename = "Key")]
    pub key: String,
    #[serde(rename = "VersionId")]
    pub version_id: Option<String>,
}

/// Delete request body
#[derive(Debug, Clone, Deserialize)]
pub struct DeleteRequest {
    #[serde(rename = "Quiet")]
    pub quiet: Option<bool>,
    #[serde(rename = "Object")]
    pub objects: Vec<DeleteObjectIdentifier>,
}

impl DeleteRequest {
    /// Parse from XML body
    pub fn from_xml(xml: &str) -> Result<Self, quick_xml::DeError> {
        quick_xml::de::from_str(xml)
    }
}

/// Result of deleting a single object
#[derive(Debug, Clone)]
pub struct DeletedObject {
    pub key: String,
    pub version_id: Option<String>,
}

/// Error deleting a single object
#[derive(Debug, Clone)]
pub struct DeleteError {
    pub key: String,
    pub version_id: Option<String>,
    pub code: String,
    pub message: String,
}

/// DeleteObjects response
#[derive(Debug, Clone)]
pub struct DeleteResult {
    pub deleted: Vec<DeletedObject>,
    pub errors: Vec<DeleteError>,
}

impl DeleteResult {
    pub fn to_xml(&self, quiet: bool) -> String {
        let mut xml = String::new();
        xml.push_str(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
        xml.push('\n');
        xml.push_str(r#"<DeleteResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">"#);
        xml.push('\n');

        // Only include Deleted elements if not quiet
        if !quiet {
            for deleted in &self.deleted {
                xml.push_str("  <Deleted>\n");
                xml.push_str(&format!("    <Key>{}</Key>\n", escape_xml(&deleted.key)));
                if let Some(ref vid) = deleted.version_id {
                    xml.push_str(&format!("    <VersionId>{}</VersionId>\n", escape_xml(vid)));
                }
                xml.push_str("  </Deleted>\n");
            }
        }

        // Always include errors
        for error in &self.errors {
            xml.push_str("  <Error>\n");
            xml.push_str(&format!("    <Key>{}</Key>\n", escape_xml(&error.key)));
            if let Some(ref vid) = error.version_id {
                xml.push_str(&format!("    <VersionId>{}</VersionId>\n", escape_xml(vid)));
            }
            xml.push_str(&format!("    <Code>{}</Code>\n", escape_xml(&error.code)));
            xml.push_str(&format!(
                "    <Message>{}</Message>\n",
                escape_xml(&error.message)
            ));
            xml.push_str("  </Error>\n");
        }

        xml.push_str("</DeleteResult>");
        xml
    }
}

// ============================================================================
// CopyObject Response
// ============================================================================

/// CopyObject response
#[derive(Debug, Clone)]
pub struct CopyObjectResult {
    pub etag: String,
    pub last_modified: DateTime<Utc>,
}

impl CopyObjectResult {
    pub fn to_xml(&self) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<CopyObjectResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <ETag>{}</ETag>
  <LastModified>{}</LastModified>
</CopyObjectResult>"#,
            escape_xml(&self.etag),
            self.last_modified.format("%Y-%m-%dT%H:%M:%S%.3fZ")
        )
    }
}

// ============================================================================
// ListBuckets Response
// ============================================================================

/// Bucket info for ListBuckets
#[derive(Debug, Clone)]
pub struct BucketInfo {
    pub name: String,
    pub creation_date: DateTime<Utc>,
}

/// ListBuckets response
#[derive(Debug, Clone)]
pub struct ListBucketsResult {
    pub owner_id: String,
    pub owner_display_name: String,
    pub buckets: Vec<BucketInfo>,
}

impl ListBucketsResult {
    pub fn to_xml(&self) -> String {
        let mut xml = String::new();
        xml.push_str(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
        xml.push('\n');
        xml.push_str(r#"<ListAllMyBucketsResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">"#);
        xml.push('\n');

        xml.push_str("  <Owner>\n");
        xml.push_str(&format!("    <ID>{}</ID>\n", escape_xml(&self.owner_id)));
        xml.push_str(&format!(
            "    <DisplayName>{}</DisplayName>\n",
            escape_xml(&self.owner_display_name)
        ));
        xml.push_str("  </Owner>\n");

        xml.push_str("  <Buckets>\n");
        for bucket in &self.buckets {
            xml.push_str("    <Bucket>\n");
            xml.push_str(&format!(
                "      <Name>{}</Name>\n",
                escape_xml(&bucket.name)
            ));
            xml.push_str(&format!(
                "      <CreationDate>{}</CreationDate>\n",
                bucket.creation_date.format("%Y-%m-%dT%H:%M:%S%.3fZ")
            ));
            xml.push_str("    </Bucket>\n");
        }
        xml.push_str("  </Buckets>\n");

        xml.push_str("</ListAllMyBucketsResult>");
        xml
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_list_bucket_result_xml() {
        let result = ListBucketResult::new_v2(
            "test-bucket".to_string(),
            "prefix/".to_string(),
            1000,
            vec![S3Object::new(
                "prefix/file.txt".to_string(),
                1024,
                Utc::now(),
                "\"abc123\"".to_string(),
            )],
            None,
            None,
            false,
        );

        let xml = result.to_xml();
        assert!(xml.contains("<Name>test-bucket</Name>"));
        assert!(xml.contains("<Key>prefix/file.txt</Key>"));
        assert!(xml.contains("<Size>1024</Size>"));
    }

    #[test]
    fn test_escape_xml() {
        assert_eq!(escape_xml("a<b>c"), "a&lt;b&gt;c");
        assert_eq!(escape_xml("a&b"), "a&amp;b");
    }
}
