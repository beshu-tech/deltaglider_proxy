//! Bucket-level S3 handlers: CREATE, DELETE, HEAD, LIST, and sub-operations
//! (GetBucketLocation, GetBucketVersioning, ListMultipartUploads).

use super::{xml_response, AppState, S3Error};
use crate::api::extractors::ValidatedBucket;
use crate::api::xml::{
    BucketInfo, ListBucketResult, ListBucketsResult, ListMultipartUploadsResult, S3Object,
};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use std::sync::Arc;
use tracing::{info, instrument};

/// Query parameters for bucket-level GET operations
#[derive(Debug, serde::Deserialize, Default)]
pub struct BucketGetQuery {
    pub prefix: Option<String>,
    pub delimiter: Option<String>,
    #[serde(rename = "list-type")]
    pub list_type: Option<u8>,
    #[serde(rename = "max-keys")]
    pub max_keys: Option<u32>,
    /// v2 pagination
    #[serde(rename = "continuation-token")]
    pub continuation_token: Option<String>,
    /// v1 pagination
    pub marker: Option<String>,
    /// Encoding type for keys/prefixes in the response (e.g. "url")
    #[serde(rename = "encoding-type")]
    pub encoding_type: Option<String>,
    /// GetBucketLocation query parameter
    pub location: Option<String>,
    /// GetBucketVersioning query parameter
    pub versioning: Option<String>,
    /// ListMultipartUploads query parameter
    pub uploads: Option<String>,
}

/// Bucket-level GET handler - dispatches to appropriate operation based on query params
/// GET /{bucket}?list-type=2&prefix=  -> ListObjectsV2
/// GET /{bucket}?location            -> GetBucketLocation
/// GET /{bucket}?versioning          -> GetBucketVersioning
/// GET /{bucket}?uploads             -> ListMultipartUploads
#[instrument(skip(state))]
pub async fn bucket_get_handler(
    State(state): State<Arc<AppState>>,
    ValidatedBucket(bucket): ValidatedBucket,
    Query(query): Query<BucketGetQuery>,
) -> Result<Response, S3Error> {
    // Check for GetBucketLocation
    if query.location.is_some() {
        info!("GET bucket location: {}", bucket);
        return get_bucket_location(&bucket).await;
    }

    // Check for GetBucketVersioning
    if query.versioning.is_some() {
        info!("GET bucket versioning: {}", bucket);
        return get_bucket_versioning(&bucket).await;
    }

    // Check for ListMultipartUploads
    if query.uploads.is_some() {
        info!("LIST multipart uploads: {}", bucket);
        let prefix = query.prefix.as_deref();
        return list_multipart_uploads(&state, &bucket, prefix).await;
    }

    // Default: ListObjects (v1 or v2)
    let is_v2 = query.list_type == Some(2);
    let prefix = query.prefix.unwrap_or_default();
    let delimiter = query.delimiter.clone();
    let max_keys = query.max_keys.unwrap_or(1000);

    // v1 uses `marker`, v2 uses `continuation-token` — both serve as "start after" key
    let pagination_token = if is_v2 {
        query.continuation_token.as_deref()
    } else {
        query.marker.as_deref()
    };

    info!(
        "LIST {}/{}* (v{})",
        bucket,
        prefix,
        if is_v2 { "2" } else { "1" }
    );

    // Engine handles prefix filtering, delimiter collapsing, and pagination as
    // a single atomic operation (they're coupled: CommonPrefixes count toward
    // max-keys and must be deduplicated across pages).
    let page = state
        .engine
        .load()
        .list_objects(
            &bucket,
            &prefix,
            delimiter.as_deref(),
            max_keys,
            pagination_token,
        )
        .await?;

    let s3_objects: Vec<S3Object> = page
        .objects
        .into_iter()
        .map(|(key, meta)| S3Object::new(key, meta.file_size, meta.created_at, meta.etag()))
        .collect();

    let xml = if is_v2 {
        ListBucketResult::new_v2(
            bucket,
            prefix,
            delimiter,
            max_keys,
            s3_objects,
            page.common_prefixes,
            query.continuation_token,
            page.next_continuation_token,
            page.is_truncated,
        )
        .to_xml()
    } else {
        ListBucketResult::new_v1(
            bucket,
            prefix,
            delimiter,
            max_keys,
            s3_objects,
            page.common_prefixes,
            query.marker,
            page.next_continuation_token,
            page.is_truncated,
            query.encoding_type,
        )
        .to_xml()
    };

    Ok(xml_response(xml))
}

/// GetBucketLocation handler
/// GET /{bucket}?location
async fn get_bucket_location(_bucket: &str) -> Result<Response, S3Error> {
    // Return a fixed location - we use us-east-1 as default
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<LocationConstraint xmlns="http://s3.amazonaws.com/doc/2006-03-01/">us-east-1</LocationConstraint>"#;
    Ok(xml_response(xml))
}

/// GetBucketVersioning handler
/// GET /{bucket}?versioning
async fn get_bucket_versioning(_bucket: &str) -> Result<Response, S3Error> {
    // Return empty VersioningConfiguration - versioning is not enabled
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<VersioningConfiguration xmlns="http://s3.amazonaws.com/doc/2006-03-01/"/>"#;
    Ok(xml_response(xml))
}

/// ListMultipartUploads handler
/// GET /{bucket}?uploads
async fn list_multipart_uploads(
    state: &Arc<AppState>,
    bucket: &str,
    prefix: Option<&str>,
) -> Result<Response, S3Error> {
    let uploads = state.multipart.list_uploads(Some(bucket), prefix);
    let result = ListMultipartUploadsResult {
        bucket: bucket.to_string(),
        uploads,
        prefix: prefix.unwrap_or("").to_string(),
        max_uploads: 1000,
        is_truncated: false,
    };
    let xml = result.to_xml();
    Ok(xml_response(xml))
}

/// CREATE bucket handler
/// PUT /{bucket}
#[instrument(skip(state))]
pub async fn create_bucket(
    State(state): State<Arc<AppState>>,
    Path(bucket): Path<String>,
) -> Result<Response, S3Error> {
    info!("CREATE bucket {}", bucket);

    if bucket.is_empty() {
        return Err(S3Error::InvalidArgument(
            "Bucket name cannot be empty".to_string(),
        ));
    }

    // Create the real bucket on the storage backend
    state.engine.load().create_bucket(&bucket).await?;

    Ok((StatusCode::OK, [("Location", format!("/{}", bucket))], "").into_response())
}

/// DELETE bucket handler
/// DELETE /{bucket}
#[instrument(skip(state))]
pub async fn delete_bucket(
    State(state): State<Arc<AppState>>,
    ValidatedBucket(bucket): ValidatedBucket,
) -> Result<Response, S3Error> {
    info!("DELETE bucket {}", bucket);

    // Check if bucket is empty (S3 requires buckets to be empty before deletion)
    let page = state
        .engine
        .load()
        .list_objects(&bucket, "", None, 1, None)
        .await?;
    if !page.objects.is_empty() {
        return Err(S3Error::BucketNotEmpty(bucket.to_string()));
    }

    // Delete the real bucket on the storage backend
    state.engine.load().delete_bucket(&bucket).await?;

    Ok(StatusCode::NO_CONTENT.into_response())
}

/// HEAD bucket handler
/// HEAD /{bucket}
#[instrument(skip(state))]
pub async fn head_bucket(
    State(state): State<Arc<AppState>>,
    ValidatedBucket(bucket): ValidatedBucket,
) -> Result<Response, S3Error> {
    info!("HEAD bucket {}", bucket);

    // Check if bucket exists on the storage backend
    let exists = state.engine.load().head_bucket(&bucket).await?;
    if !exists {
        return Err(S3Error::NoSuchBucket(bucket.to_string()));
    }

    Ok((StatusCode::OK, [("x-amz-bucket-region", "us-east-1")]).into_response())
}

/// LIST buckets handler
/// GET /
#[instrument(skip(state))]
pub async fn list_buckets(State(state): State<Arc<AppState>>) -> Result<Response, S3Error> {
    info!("LIST buckets");

    // List real buckets from storage backend
    let mut bucket_list = state.engine.load().list_buckets().await?;
    bucket_list.sort();

    let result = ListBucketsResult {
        owner_id: "deltaglider_proxy".to_string(),
        owner_display_name: "DeltaGlider Proxy".to_string(),
        buckets: bucket_list
            .into_iter()
            .map(|name| BucketInfo {
                name,
                creation_date: Utc::now(),
            })
            .collect(),
    };
    let xml = result.to_xml();

    Ok(xml_response(xml))
}
