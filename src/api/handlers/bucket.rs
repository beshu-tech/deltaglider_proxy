//! Bucket-level S3 handlers: CREATE, DELETE, HEAD, LIST, and sub-operations
//! (GetBucketLocation, GetBucketVersioning, ListMultipartUploads).

use super::{audit_log_s3, xml_response, AppState, S3Error};
use crate::api::extractors::ValidatedBucket;
use crate::api::xml::{
    BucketInfo, ListBucketResult, ListBucketsResult, ListMultipartUploadsResult, S3Object,
};
use crate::iam::AuthenticatedUser;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use std::sync::Arc;
use tracing::{info, instrument, warn};

/// Return NoSuchBucket for non-existent buckets.
/// Bucket enumeration prevention is handled at the auth middleware layer —
/// unauthenticated requests to non-existent buckets are rejected before
/// reaching handlers when auth is enabled.
fn no_such_bucket_error(bucket: &str) -> S3Error {
    S3Error::NoSuchBucket(bucket.to_string())
}

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
    /// v2: start listing after this key (used when no continuation-token)
    #[serde(rename = "start-after")]
    pub start_after: Option<String>,
    /// v2: whether to include owner info in response
    #[serde(rename = "fetch-owner")]
    pub fetch_owner: Option<bool>,
    /// Encoding type for keys/prefixes in the response (e.g. "url")
    #[serde(rename = "encoding-type")]
    pub encoding_type: Option<String>,
    /// GetBucketLocation query parameter
    pub location: Option<String>,
    /// GetBucketVersioning query parameter
    pub versioning: Option<String>,
    /// ListMultipartUploads query parameter
    pub uploads: Option<String>,
    /// ACL operations (GET/PUT with ?acl)
    pub acl: Option<String>,
    /// Tagging operations (GET/PUT with ?tagging)
    pub tagging: Option<String>,
    /// MinIO extension: include per-object user metadata in ListObjectsV2 response
    pub metadata: Option<bool>,
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
    auth_user: Option<axum::Extension<AuthenticatedUser>>,
) -> Result<Response, S3Error> {
    // Check for tagging request
    if query.tagging.is_some() {
        info!("GET bucket tagging (stub): {}", bucket);
        let exists = state.engine.load().head_bucket(&bucket).await?;
        if !exists {
            return Err(no_such_bucket_error(&bucket));
        }
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<Tagging><TagSet/></Tagging>"#;
        return Ok(xml_response(xml));
    }

    // Check for ACL request
    if query.acl.is_some() {
        info!("GET bucket ACL: {}", bucket);
        // Verify the bucket exists first; S3 returns 404 for ACL on non-existent buckets
        let exists = state.engine.load().head_bucket(&bucket).await?;
        if !exists {
            return Err(no_such_bucket_error(&bucket));
        }
        return get_acl_response();
    }

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
    // Gap 3: cap max_keys at 1000 (S3/MinIO standard upper bound)
    let max_keys = query.max_keys.unwrap_or(1000).min(1000);

    // v1 uses `marker`, v2 uses `continuation-token` — both serve as "start after" key
    // Gap 2 & 5: For v2, decode base64 continuation token; fall back to start_after
    let pagination_token = if is_v2 {
        if let Some(ref token) = query.continuation_token {
            // Gap 5: base64-decode the incoming continuation token
            let decoded = BASE64
                .decode(token)
                .ok()
                .and_then(|bytes| String::from_utf8(bytes).ok())
                .unwrap_or_else(|| token.clone());
            Some(decoded)
        } else {
            // Gap 2: when no continuation-token, use start-after as pagination start
            query.start_after.clone()
        }
    } else {
        query.marker.clone()
    };

    info!(
        "LIST {}/{}* (v{})",
        bucket,
        prefix,
        if is_v2 { "2" } else { "1" }
    );

    // MinIO extension: metadata=true enriches ListObjectsV2 with per-object user metadata
    let include_metadata = is_v2 && query.metadata.unwrap_or(false);

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
            pagination_token.as_deref(),
            include_metadata,
        )
        .await?;

    let s3_objects: Vec<S3Object> = page
        .objects
        .into_iter()
        .map(|(key, meta)| {
            let user_metadata = if include_metadata {
                Some(meta.all_amz_metadata())
            } else {
                None
            };
            S3Object::new(
                key,
                meta.file_size,
                meta.created_at,
                meta.etag(),
                user_metadata,
            )
        })
        .collect();

    // Gap 5: base64-encode the next continuation token before returning
    let next_token = page.next_continuation_token.map(|t| BASE64.encode(&t));

    let xml = if is_v2 {
        // Gap 1: pass encoding_type to v2 as well
        // Gap 4: pass fetch_owner
        // Gap 6: pass start_after for <StartAfter> element
        ListBucketResult::new_v2(
            bucket,
            prefix,
            delimiter,
            max_keys,
            s3_objects,
            page.common_prefixes,
            query.continuation_token,
            next_token,
            page.is_truncated,
            query.encoding_type,
            query.fetch_owner.unwrap_or(false),
            query.start_after,
        )
        .to_xml()
    } else {
        ListBucketResult::new_v1(
            bucket,
            prefix,
            delimiter.clone(),
            max_keys,
            s3_objects,
            page.common_prefixes,
            query.marker,
            next_token,
            page.is_truncated,
            query.encoding_type,
        )
        .to_xml()
    };

    Ok(xml_response(xml))
}

/// Canned ACL response (full control for owner "dgp").
/// Used by both bucket and object ACL stubs.
pub(super) fn get_acl_response() -> Result<Response, S3Error> {
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<AccessControlPolicy>
    <Owner><ID>dgp</ID><DisplayName>deltaglider</DisplayName></Owner>
    <AccessControlList>
        <Grant>
            <Grantee xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:type="CanonicalUser">
                <ID>dgp</ID><DisplayName>deltaglider</DisplayName>
            </Grantee>
            <Permission>FULL_CONTROL</Permission>
        </Grant>
    </AccessControlList>
</AccessControlPolicy>"#;
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
/// Also handles PUT /{bucket}?acl (ACL stub)
#[instrument(skip(state))]
pub async fn create_bucket(
    State(state): State<Arc<AppState>>,
    Path(bucket): Path<String>,
    Query(query): Query<BucketGetQuery>,
    auth_user: Option<axum::Extension<AuthenticatedUser>>,
    headers: HeaderMap,
) -> Result<Response, S3Error> {
    // PUT /{bucket}?acl — accept and ignore (ACL stub)
    if query.acl.is_some() {
        info!("PUT bucket ACL (stub): {}", bucket);
        return Ok(StatusCode::OK.into_response());
    }

    // PUT /{bucket}?tagging — accept and ignore (tagging stub)
    if query.tagging.is_some() {
        info!("PUT bucket tagging (stub): {}", bucket);
        // Verify the bucket exists first; S3 returns 404 for tagging on non-existent buckets
        let exists = state.engine.load().head_bucket(&bucket).await?;
        if !exists {
            return Err(no_such_bucket_error(&bucket));
        }
        return Ok(StatusCode::OK.into_response());
    }

    // PUT /{bucket}?versioning — accept and ignore (versioning stub)
    if query.versioning.is_some() {
        warn!(
            "PUT bucket versioning (stub): {} — versioning is not supported, ignoring",
            bucket
        );
        return Ok(StatusCode::OK.into_response());
    }

    info!("CREATE bucket {}", bucket);

    // Validate bucket name per S3 spec
    validate_bucket_name(&bucket)?;

    // Create the real bucket on the storage backend
    state.engine.load().create_bucket(&bucket).await?;

    let user_name = auth_user
        .as_ref()
        .map(|u| u.name.as_str())
        .unwrap_or("anonymous");
    audit_log_s3("s3_create_bucket", user_name, &headers, &bucket, "");

    Ok((StatusCode::OK, [("Location", format!("/{}", bucket))], "").into_response())
}

/// Validate bucket name per S3 spec:
/// - 3-63 characters
/// - Only lowercase letters, numbers, hyphens
/// - Must start/end with letter or number
/// - Cannot be formatted as IP address
fn validate_bucket_name(name: &str) -> Result<(), S3Error> {
    let len = name.len();
    if !(3..=63).contains(&len) {
        return Err(S3Error::InvalidBucketName(format!(
            "Bucket name must be between 3 and 63 characters long, got {}",
            len
        )));
    }

    // Only lowercase letters, numbers, and hyphens
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(S3Error::InvalidBucketName(
            "Bucket name can only contain lowercase letters, numbers, and hyphens".to_string(),
        ));
    }

    // Must start with letter or number
    let first = name.chars().next().unwrap();
    if !first.is_ascii_alphanumeric() {
        return Err(S3Error::InvalidBucketName(
            "Bucket name must start with a letter or number".to_string(),
        ));
    }

    // Must end with letter or number
    let last = name.chars().last().unwrap();
    if !last.is_ascii_alphanumeric() {
        return Err(S3Error::InvalidBucketName(
            "Bucket name must end with a letter or number".to_string(),
        ));
    }

    // Cannot be formatted as IP address (four groups of 1-3 digits separated by dots)
    if is_ip_format(name) {
        return Err(S3Error::InvalidBucketName(
            "Bucket name must not be formatted as an IP address".to_string(),
        ));
    }

    Ok(())
}

/// Check if a string looks like an IP address (e.g. 192.168.1.1)
fn is_ip_format(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() != 4 {
        return false;
    }
    parts
        .iter()
        .all(|p| !p.is_empty() && p.len() <= 3 && p.chars().all(|c| c.is_ascii_digit()))
}

/// DELETE bucket handler
/// DELETE /{bucket}
#[instrument(skip(state))]
pub async fn delete_bucket(
    State(state): State<Arc<AppState>>,
    ValidatedBucket(bucket): ValidatedBucket,
    auth_user: Option<axum::Extension<AuthenticatedUser>>,
    headers: HeaderMap,
) -> Result<Response, S3Error> {
    info!("DELETE bucket {}", bucket);

    // Check if bucket is empty (S3 requires buckets to be empty before deletion)
    let page = state
        .engine
        .load()
        .list_objects(&bucket, "", None, 1, None, false)
        .await?;
    if !page.objects.is_empty() {
        return Err(S3Error::BucketNotEmpty(bucket.to_string()));
    }

    // Delete the real bucket on the storage backend
    state.engine.load().delete_bucket(&bucket).await?;

    let user_name = auth_user
        .as_ref()
        .map(|u| u.name.as_str())
        .unwrap_or("anonymous");
    audit_log_s3("s3_delete_bucket", user_name, &headers, &bucket, "");

    Ok(StatusCode::NO_CONTENT.into_response())
}

/// HEAD bucket handler
/// HEAD /{bucket}
#[instrument(skip(state))]
pub async fn head_bucket(
    State(state): State<Arc<AppState>>,
    ValidatedBucket(bucket): ValidatedBucket,
    auth_user: Option<axum::Extension<AuthenticatedUser>>,
) -> Result<Response, S3Error> {
    info!("HEAD bucket {}", bucket);

    // Check if bucket exists on the storage backend
    let exists = state.engine.load().head_bucket(&bucket).await?;
    if !exists {
        return Err(no_such_bucket_error(&bucket));
    }

    Ok((StatusCode::OK, [("x-amz-bucket-region", "us-east-1")]).into_response())
}

/// LIST buckets handler
/// GET /
#[instrument(skip(state))]
pub async fn list_buckets(State(state): State<Arc<AppState>>) -> Result<Response, S3Error> {
    info!("LIST buckets");

    // List real buckets from storage backend with actual creation dates
    let mut bucket_list = state.engine.load().list_buckets_with_dates().await?;
    bucket_list.sort_by(|a, b| a.0.cmp(&b.0));

    let result = ListBucketsResult {
        owner_id: "deltaglider_proxy".to_string(),
        owner_display_name: "DeltaGlider Proxy".to_string(),
        buckets: bucket_list
            .into_iter()
            .map(|(name, creation_date)| BucketInfo {
                name,
                creation_date,
            })
            .collect(),
    };
    let xml = result.to_xml();

    Ok(xml_response(xml))
}
