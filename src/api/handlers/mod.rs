//! S3 API request handlers
//!
//! Split into submodules by domain:
//! - `object` — GET, HEAD, PUT, DELETE for individual objects
//! - `bucket` — Bucket CRUD and listing
//! - `multipart` — Multipart upload lifecycle
//! - `status` — Health check and aggregate stats

mod bucket;
mod multipart;
mod object;
mod status;

use super::errors::S3Error;
use crate::deltaglider::DynEngine;
use crate::multipart::MultipartStore;
use crate::types::{FileMetadata, StorageInfo};
use arc_swap::ArcSwap;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use std::sync::Arc;

// Re-export all public handlers and types so callers don't change.
pub use bucket::{
    bucket_get_handler, create_bucket, delete_bucket, head_bucket, list_buckets, BucketGetQuery,
};
pub use multipart::post_object;
pub use object::{delete_object, delete_objects, get_object, head_object, put_object_or_copy};
pub use status::{get_stats, head_root, health_check, HealthResponse, StatsQuery, StatsResponse};

/// Application state shared across handlers
pub struct AppState {
    pub engine: ArcSwap<DynEngine>,
    pub multipart: Arc<MultipartStore>,
}

/// Query parameters for object-level operations (multipart upload)
#[derive(Debug, serde::Deserialize, Default)]
pub struct ObjectQuery {
    /// CreateMultipartUpload (POST with ?uploads)
    pub uploads: Option<String>,
    /// UploadPart / CompleteMultipartUpload (with ?uploadId)
    #[serde(rename = "uploadId")]
    pub upload_id: Option<String>,
    /// UploadPart (PUT with ?partNumber)
    #[serde(rename = "partNumber")]
    pub part_number: Option<u32>,
}

// ---------------------------------------------------------------------------
// Shared utility functions used across handler submodules
// ---------------------------------------------------------------------------

/// Build response headers for an object including DeltaGlider custom metadata.
fn build_object_headers(metadata: &FileMetadata) -> HeaderMap {
    let stored_size = metadata.delta_size().unwrap_or(metadata.file_size);
    let content_type = metadata
        .content_type
        .clone()
        .unwrap_or_else(|| "application/octet-stream".to_string());

    let mut headers = HeaderMap::new();
    headers.insert("ETag", hval(&metadata.etag()));
    headers.insert("Content-Length", hval(&metadata.file_size.to_string()));
    headers.insert("Content-Type", hval(&content_type));
    headers.insert(
        "Last-Modified",
        hval(
            &metadata
                .created_at
                .format("%a, %d %b %Y %H:%M:%S GMT")
                .to_string(),
        ),
    );
    headers.insert("x-amz-storage-type", hval(metadata.storage_info.label()));
    headers.insert("x-deltaglider-stored-size", hval(&stored_size.to_string()));

    // DeltaGlider custom metadata (x-amz-meta-dg-*)
    use crate::types::meta_keys as mk;
    headers.insert(mk::H_TOOL, hval(&metadata.tool));
    headers.insert(mk::H_ORIGINAL_NAME, hval(&metadata.original_name));
    headers.insert(mk::H_FILE_SHA256, hval(&metadata.file_sha256));
    headers.insert(mk::H_FILE_SIZE, hval(&metadata.file_size.to_string()));

    match &metadata.storage_info {
        StorageInfo::Reference { source_name } => {
            headers.insert(mk::H_NOTE, hval("reference"));
            headers.insert(mk::H_SOURCE_NAME, hval(source_name));
        }
        StorageInfo::Delta {
            ref_key,
            ref_sha256,
            delta_size,
            delta_cmd,
        } => {
            headers.insert(mk::H_NOTE, hval("delta"));
            headers.insert(mk::H_REF_KEY, hval(ref_key));
            headers.insert(mk::H_REF_SHA256, hval(ref_sha256));
            headers.insert(mk::H_DELTA_SIZE, hval(&delta_size.to_string()));
            headers.insert(mk::H_DELTA_CMD, hval(delta_cmd));
        }
        StorageInfo::Passthrough => {
            headers.insert(mk::H_NOTE, hval("passthrough"));
        }
    }

    // User-provided custom metadata (x-amz-meta-*)
    for (key, value) in &metadata.user_metadata {
        let header_name = format!("x-amz-meta-{}", key);
        if let Ok(name) = axum::http::header::HeaderName::from_bytes(header_name.as_bytes()) {
            headers.insert(name, hval(value));
        }
    }

    headers
}

fn hval(s: &str) -> HeaderValue {
    HeaderValue::from_bytes(s.as_bytes()).unwrap_or_else(|_| HeaderValue::from_static(""))
}

/// Build an XML response with correct Content-Type header.
fn xml_response(xml: impl Into<String>) -> Response {
    (
        StatusCode::OK,
        [("Content-Type", "application/xml")],
        xml.into(),
    )
        .into_response()
}

/// Extract Content-Type header as an owned String.
fn extract_content_type(headers: &HeaderMap) -> Option<String> {
    headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

/// Parse request body as UTF-8 string, mapping errors to MalformedXML.
fn body_to_utf8(body: &axum::body::Bytes) -> Result<String, S3Error> {
    String::from_utf8(body.to_vec()).map_err(|_| S3Error::MalformedXML)
}

/// Extract user-provided x-amz-meta-* headers, excluding DeltaGlider internal metadata (dg-*).
fn extract_user_metadata(headers: &HeaderMap) -> std::collections::HashMap<String, String> {
    use crate::types::meta_keys as mk;
    headers
        .iter()
        .filter_map(|(name, value)| {
            let name_str = name.as_str();
            if let Some(suffix) = name_str.strip_prefix(mk::AMZ_META_PREFIX) {
                if !suffix.starts_with("dg-") {
                    if let Ok(v) = value.to_str() {
                        return Some((suffix.to_string(), v.to_string()));
                    }
                }
            }
            None
        })
        .collect()
}

/// Decode base64 string to bytes (for Content-MD5 validation)
fn base64_decode(input: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(input.trim())
        .ok()
}
