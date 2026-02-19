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

/// ListObjects v1/v2 response
#[derive(Debug, Clone)]
pub struct ListBucketResult {
    pub name: String,
    pub prefix: String,
    pub delimiter: Option<String>,
    pub max_keys: u32,
    pub key_count: u32,
    pub is_truncated: bool,
    pub contents: Vec<S3Object>,
    pub common_prefixes: Vec<String>,
    /// v2 pagination
    pub continuation_token: Option<String>,
    pub next_continuation_token: Option<String>,
    /// v1 pagination
    pub marker: Option<String>,
    pub next_marker: Option<String>,
    /// Whether to URL-encode keys/prefixes in the XML response
    pub encoding_type: Option<String>,
    /// v1 vs v2 flag
    pub is_v1: bool,
}

impl ListBucketResult {
    /// Encode a key/prefix value: URL-encode if encoding_type is "url", otherwise XML-escape.
    fn encode_value(&self, s: &str) -> String {
        if self.encoding_type.as_deref() == Some("url") {
            urlencoding::encode(s).into_owned()
        } else {
            escape_xml(s)
        }
    }

    /// Create a ListObjects v1 response
    #[allow(clippy::too_many_arguments)]
    pub fn new_v1(
        name: String,
        prefix: String,
        delimiter: Option<String>,
        max_keys: u32,
        contents: Vec<S3Object>,
        common_prefixes: Vec<String>,
        marker: Option<String>,
        next_marker: Option<String>,
        is_truncated: bool,
        encoding_type: Option<String>,
    ) -> Self {
        let key_count = (contents.len() + common_prefixes.len()) as u32;
        Self {
            name,
            prefix,
            delimiter,
            max_keys,
            key_count,
            is_truncated,
            contents,
            common_prefixes,
            continuation_token: None,
            next_continuation_token: None,
            marker,
            next_marker,
            encoding_type,
            is_v1: true,
        }
    }

    /// Create a ListObjectsV2 response
    #[allow(clippy::too_many_arguments)]
    pub fn new_v2(
        name: String,
        prefix: String,
        delimiter: Option<String>,
        max_keys: u32,
        contents: Vec<S3Object>,
        common_prefixes: Vec<String>,
        continuation_token: Option<String>,
        next_continuation_token: Option<String>,
        is_truncated: bool,
    ) -> Self {
        let key_count = (contents.len() + common_prefixes.len()) as u32;
        Self {
            name,
            prefix,
            delimiter,
            max_keys,
            key_count,
            is_truncated,
            contents,
            common_prefixes,
            continuation_token,
            next_continuation_token,
            marker: None,
            next_marker: None,
            encoding_type: None,
            is_v1: false,
        }
    }

    /// Convert to S3 XML format (v1 or v2 depending on construction)
    pub fn to_xml(&self) -> String {
        let mut xml = String::new();
        xml.push_str(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
        xml.push('\n');
        xml.push_str(r#"<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">"#);
        xml.push('\n');

        xml.push_str(&format!("  <Name>{}</Name>\n", escape_xml(&self.name)));
        xml.push_str(&format!(
            "  <Prefix>{}</Prefix>\n",
            self.encode_value(&self.prefix)
        ));
        if let Some(ref delim) = self.delimiter {
            xml.push_str(&format!("  <Delimiter>{}</Delimiter>\n", escape_xml(delim)));
        }
        if let Some(ref enc) = self.encoding_type {
            xml.push_str(&format!(
                "  <EncodingType>{}</EncodingType>\n",
                escape_xml(enc)
            ));
        }
        xml.push_str(&format!("  <MaxKeys>{}</MaxKeys>\n", self.max_keys));

        if self.is_v1 {
            // v1: <Marker>, <NextMarker>, no <KeyCount>
            xml.push_str(&format!(
                "  <Marker>{}</Marker>\n",
                self.encode_value(self.marker.as_deref().unwrap_or(""))
            ));
            xml.push_str(&format!(
                "  <IsTruncated>{}</IsTruncated>\n",
                self.is_truncated
            ));
            if self.is_truncated {
                if let Some(ref nm) = self.next_marker {
                    xml.push_str(&format!(
                        "  <NextMarker>{}</NextMarker>\n",
                        self.encode_value(nm)
                    ));
                }
            }
        } else {
            // v2: <KeyCount>, <ContinuationToken>, <NextContinuationToken>
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
        }

        for obj in &self.contents {
            xml.push_str("  <Contents>\n");
            xml.push_str(&format!("    <Key>{}</Key>\n", self.encode_value(&obj.key)));
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

        for cp in &self.common_prefixes {
            xml.push_str("  <CommonPrefixes>\n");
            xml.push_str(&format!("    <Prefix>{}</Prefix>\n", self.encode_value(cp)));
            xml.push_str("  </CommonPrefixes>\n");
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

// ============================================================================
// Multipart Upload Request/Response
// ============================================================================

/// Part in a CompleteMultipartUpload request
#[derive(Debug, Clone, Deserialize)]
pub struct CompletePart {
    #[serde(rename = "PartNumber")]
    pub part_number: u32,
    #[serde(rename = "ETag")]
    pub etag: String,
}

/// CompleteMultipartUpload request body
#[derive(Debug, Clone, Deserialize)]
pub struct CompleteMultipartUploadRequest {
    #[serde(rename = "Part")]
    pub parts: Vec<CompletePart>,
}

impl CompleteMultipartUploadRequest {
    /// Parse from XML body
    pub fn from_xml(xml: &str) -> Result<Self, quick_xml::DeError> {
        quick_xml::de::from_str(xml)
    }
}

/// InitiateMultipartUpload response
#[derive(Debug, Clone)]
pub struct InitiateMultipartUploadResult {
    pub bucket: String,
    pub key: String,
    pub upload_id: String,
}

impl InitiateMultipartUploadResult {
    pub fn to_xml(&self) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<InitiateMultipartUploadResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Bucket>{}</Bucket>
  <Key>{}</Key>
  <UploadId>{}</UploadId>
</InitiateMultipartUploadResult>"#,
            escape_xml(&self.bucket),
            escape_xml(&self.key),
            escape_xml(&self.upload_id),
        )
    }
}

/// CompleteMultipartUpload response
#[derive(Debug, Clone)]
pub struct CompleteMultipartUploadResult {
    pub location: String,
    pub bucket: String,
    pub key: String,
    pub etag: String,
}

impl CompleteMultipartUploadResult {
    pub fn to_xml(&self) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<CompleteMultipartUploadResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Location>{}</Location>
  <Bucket>{}</Bucket>
  <Key>{}</Key>
  <ETag>{}</ETag>
</CompleteMultipartUploadResult>"#,
            escape_xml(&self.location),
            escape_xml(&self.bucket),
            escape_xml(&self.key),
            escape_xml(&self.etag),
        )
    }
}

/// Part info for ListParts response
#[derive(Debug, Clone)]
pub struct PartInfo {
    pub part_number: u32,
    pub etag: String,
    pub size: u64,
    pub last_modified: DateTime<Utc>,
}

/// ListParts response
#[derive(Debug, Clone)]
pub struct ListPartsResult {
    pub bucket: String,
    pub key: String,
    pub upload_id: String,
    pub parts: Vec<PartInfo>,
    pub max_parts: u32,
    pub is_truncated: bool,
}

impl ListPartsResult {
    pub fn to_xml(&self) -> String {
        let mut xml = String::new();
        xml.push_str(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
        xml.push('\n');
        xml.push_str(r#"<ListPartsResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">"#);
        xml.push('\n');
        xml.push_str(&format!(
            "  <Bucket>{}</Bucket>\n",
            escape_xml(&self.bucket)
        ));
        xml.push_str(&format!("  <Key>{}</Key>\n", escape_xml(&self.key)));
        xml.push_str(&format!(
            "  <UploadId>{}</UploadId>\n",
            escape_xml(&self.upload_id)
        ));
        xml.push_str(&format!("  <MaxParts>{}</MaxParts>\n", self.max_parts));
        xml.push_str(&format!(
            "  <IsTruncated>{}</IsTruncated>\n",
            self.is_truncated
        ));

        for part in &self.parts {
            xml.push_str("  <Part>\n");
            xml.push_str(&format!(
                "    <PartNumber>{}</PartNumber>\n",
                part.part_number
            ));
            xml.push_str(&format!("    <ETag>{}</ETag>\n", escape_xml(&part.etag)));
            xml.push_str(&format!("    <Size>{}</Size>\n", part.size));
            xml.push_str(&format!(
                "    <LastModified>{}</LastModified>\n",
                part.last_modified.format("%Y-%m-%dT%H:%M:%S%.3fZ")
            ));
            xml.push_str("  </Part>\n");
        }

        xml.push_str("</ListPartsResult>");
        xml
    }
}

/// Upload info for ListMultipartUploads response
#[derive(Debug, Clone)]
pub struct UploadInfo {
    pub key: String,
    pub upload_id: String,
    pub initiated: DateTime<Utc>,
}

/// ListMultipartUploads response
#[derive(Debug, Clone)]
pub struct ListMultipartUploadsResult {
    pub bucket: String,
    pub uploads: Vec<UploadInfo>,
    pub prefix: String,
    pub max_uploads: u32,
    pub is_truncated: bool,
}

impl ListMultipartUploadsResult {
    pub fn to_xml(&self) -> String {
        let mut xml = String::new();
        xml.push_str(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
        xml.push('\n');
        xml.push_str(
            r#"<ListMultipartUploadsResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">"#,
        );
        xml.push('\n');
        xml.push_str(&format!(
            "  <Bucket>{}</Bucket>\n",
            escape_xml(&self.bucket)
        ));
        xml.push_str("  <KeyMarker/>\n");
        xml.push_str("  <UploadIdMarker/>\n");
        if !self.prefix.is_empty() {
            xml.push_str(&format!(
                "  <Prefix>{}</Prefix>\n",
                escape_xml(&self.prefix)
            ));
        }
        xml.push_str(&format!(
            "  <MaxUploads>{}</MaxUploads>\n",
            self.max_uploads
        ));
        xml.push_str(&format!(
            "  <IsTruncated>{}</IsTruncated>\n",
            self.is_truncated
        ));

        for upload in &self.uploads {
            xml.push_str("  <Upload>\n");
            xml.push_str(&format!("    <Key>{}</Key>\n", escape_xml(&upload.key)));
            xml.push_str(&format!(
                "    <UploadId>{}</UploadId>\n",
                escape_xml(&upload.upload_id)
            ));
            xml.push_str(&format!(
                "    <Initiated>{}</Initiated>\n",
                upload.initiated.format("%Y-%m-%dT%H:%M:%S%.3fZ")
            ));
            xml.push_str("  </Upload>\n");
        }

        xml.push_str("</ListMultipartUploadsResult>");
        xml
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_escape_xml() {
        assert_eq!(escape_xml("a<b>c"), "a&lt;b&gt;c");
        assert_eq!(escape_xml("a&b"), "a&amp;b");
    }

    #[test]
    fn test_delete_request_from_xml_basic() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<Delete>
  <Object><Key>file1.txt</Key></Object>
  <Object><Key>file2.txt</Key></Object>
</Delete>"#;
        let req = DeleteRequest::from_xml(xml).unwrap();
        assert_eq!(req.objects.len(), 2);
        assert_eq!(req.objects[0].key, "file1.txt");
        assert_eq!(req.objects[1].key, "file2.txt");
        assert!(req.quiet.is_none());
    }

    #[test]
    fn test_delete_request_from_xml_quiet() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<Delete>
  <Quiet>true</Quiet>
  <Object><Key>file1.txt</Key></Object>
</Delete>"#;
        let req = DeleteRequest::from_xml(xml).unwrap();
        assert_eq!(req.quiet, Some(true));
        assert_eq!(req.objects.len(), 1);
    }

    #[test]
    fn test_delete_request_from_xml_malformed() {
        let xml = "this is not valid xml at all <<<>>>";
        let result = DeleteRequest::from_xml(xml);
        assert!(result.is_err());
    }
}
