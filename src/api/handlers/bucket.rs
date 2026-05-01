//! Bucket-level S3 handlers: CREATE, DELETE, HEAD, LIST, and sub-operations
//! (GetBucketLocation, GetBucketVersioning, ListMultipartUploads).

use super::{audit_log_s3, xml_response, AppState, S3Error};
use crate::api::extractors::ValidatedBucket;
use crate::api::xml::{
    BucketInfo, ListBucketResult, ListBucketsResult, ListMultipartUploadsResult, S3Object,
};
use crate::iam::{
    user_can_see_common_prefix, user_can_see_listed_key, AuthenticatedUser, ListScope,
};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use std::sync::Arc;
use tracing::{info, instrument};

/// Return NoSuchBucket for non-existent buckets.
/// Bucket enumeration prevention is handled at the auth middleware layer —
/// unauthenticated requests to non-existent buckets are rejected before
/// reaching handlers when auth is enabled.
fn no_such_bucket_error(bucket: &str) -> S3Error {
    S3Error::NoSuchBucket(bucket.to_string())
}

/// Resolve `head_bucket` and turn `false` into a `NoSuchBucket` error.
/// Shared by every bucket subresource that should answer 404 for
/// ghost buckets (GetBucketLocation / GetBucketVersioning /
/// ListMultipartUploads / etc.).
async fn require_bucket_exists(state: &Arc<AppState>, bucket: &str) -> Result<(), S3Error> {
    let exists = state.engine.load().head_bucket(bucket).await?;
    if !exists {
        return Err(no_such_bucket_error(bucket));
    }
    Ok(())
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
    /// ListMultipartUploads pagination: cap on page size (default 1000).
    #[serde(rename = "max-uploads")]
    pub max_uploads: Option<u32>,
    /// ListMultipartUploads pagination: skip uploads whose (key, upload-id)
    /// is ≤ (key-marker, upload-id-marker).
    #[serde(rename = "key-marker")]
    pub key_marker: Option<String>,
    #[serde(rename = "upload-id-marker")]
    pub upload_id_marker: Option<String>,
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
    uri: Uri,
    auth_user: Option<axum::Extension<AuthenticatedUser>>,
    list_scope: Option<axum::Extension<ListScope>>,
) -> Result<Response, S3Error> {
    // The `auth_user` is still used by logging/audit elsewhere, but the
    // filtering decision lives in `list_scope`, set by the IAM middleware.
    let _ = &auth_user;
    // GET /{bucket}?tagging — return 501 NotImplemented.
    // M4 correctness fix (consistent with object tagging): we don't
    // store bucket tags, so returning a hardcoded empty TagSet
    // silently lied to clients relying on tags for downstream policy.
    if query.tagging.is_some() {
        info!("GET bucket tagging: unsupported (returning 501)");
        // M4 fix: 404 wins over 501 when the bucket doesn't exist.
        require_bucket_exists(&state, &bucket).await?;
        return Err(S3Error::NotImplemented(
            "Bucket tagging is not supported by this proxy".to_string(),
        ));
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
    //
    // M3 fix: gate every bucket subresource on bucket existence so the
    // proxy doesn't answer for ghosts. Pre-fix, GetBucketLocation /
    // GetBucketVersioning / ListMultipartUploads happily returned 200
    // for buckets that didn't exist.
    if query.location.is_some() {
        info!("GET bucket location: {}", bucket);
        require_bucket_exists(&state, &bucket).await?;
        return get_bucket_location(&bucket).await;
    }

    // Check for GetBucketVersioning
    if query.versioning.is_some() {
        info!("GET bucket versioning: {}", bucket);
        require_bucket_exists(&state, &bucket).await?;
        return get_bucket_versioning(&bucket).await;
    }

    // Check for ListMultipartUploads
    if query.uploads.is_some() {
        info!("LIST multipart uploads: {}", bucket);
        require_bucket_exists(&state, &bucket).await?;
        let prefix = query.prefix.as_deref();
        return list_multipart_uploads(
            &state,
            &bucket,
            prefix,
            query.key_marker.as_deref().unwrap_or(""),
            query.upload_id_marker.as_deref().unwrap_or(""),
            query.max_uploads.unwrap_or(1000),
        )
        .await;
    }

    // Default: ListObjects (v1 or v2)
    reject_duplicate_list_query_params(uri.query())?;
    let is_v2 = query.list_type == Some(2);
    let prefix = query.prefix.unwrap_or_default();
    let delimiter = query.delimiter.clone();
    // Gap 3: cap max_keys at 1000 (S3/MinIO standard upper bound)
    if let Some(0) = query.max_keys {
        return Err(S3Error::InvalidArgument(
            "max-keys must be greater than 0".into(),
        ));
    }
    let max_keys = query.max_keys.unwrap_or(1000).min(1000);

    // v1 uses `marker`, v2 uses `continuation-token` — both serve as "start after" key
    // Gap 2 & 5: For v2, decode base64 continuation token; fall back to start_after
    let pagination_token = if is_v2 {
        if let Some(ref token) = query.continuation_token {
            // Gap 5: base64-decode the incoming continuation token
            let bytes = BASE64
                .decode(token)
                .map_err(|_| S3Error::InvalidArgument("Invalid continuation token".into()))?;
            let decoded = String::from_utf8(bytes)
                .map_err(|_| S3Error::InvalidArgument("Invalid continuation token".into()))?;
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

    // C1 security fix: when the middleware flagged this LIST as
    // `ListScope::Filtered` (the caller had prefix-scoped permissions that
    // don't cover the full requested prefix), we MUST post-filter each key
    // and common-prefix against the user's actual per-key permissions.
    // Pre-fix, unfiltered keys leaked out of scope — any key in `bucket`
    // was visible once the `can_see_bucket` fallback admitted the request.
    //
    // Design note: we filter AFTER the engine pages. This means the client's
    // `max_keys` acts as a server-side inspection cap, not a guaranteed
    // returned count — the resulting page may have fewer objects than
    // requested. `is_truncated` + the engine's `next_continuation_token`
    // remain honest (reflect engine-level cursor, not filter count). This
    // matches AWS's documented behaviour for policies that mix allows with
    // prefix-scoped denies.
    let (filtered_objects, filtered_common_prefixes, was_filtered) = match &list_scope {
        Some(axum::Extension(ListScope::Filtered { user })) => {
            let objects: Vec<_> = page
                .objects
                .into_iter()
                .filter(|(k, _)| user_can_see_listed_key(user, &bucket, k, &prefix))
                .collect();
            // For CommonPrefixes, treat the prefix itself as a key under
            // the bucket; if the user can see nothing under that prefix
            // (no permission referencing it), drop it.
            let common_prefixes: Vec<_> = page
                .common_prefixes
                .into_iter()
                .filter(|p| user_can_see_common_prefix(user, &bucket, p))
                .collect();
            (objects, common_prefixes, true)
        }
        _ => (page.objects, page.common_prefixes, false),
    };

    let s3_objects: Vec<S3Object> = filtered_objects
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
            filtered_common_prefixes,
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
            filtered_common_prefixes,
            query.marker,
            next_token,
            page.is_truncated,
            query.encoding_type,
        )
        .to_xml()
    };

    let mut resp = xml_response(xml).into_response();
    if was_filtered {
        // Informational header so operators/SDKs can tell a filtered page
        // from an unrestricted one. Not load-bearing for correctness.
        resp.headers_mut().insert(
            "x-amz-meta-dg-list-filtered",
            axum::http::HeaderValue::from_static("true"),
        );
    }
    Ok(resp)
}

/// Reject duplicate query keys whose values affect authorization, listing
/// scope, or pagination. Without this, IAM middleware can evaluate the first
/// `prefix=` while serde extraction uses another value for the actual list.
fn reject_duplicate_list_query_params(query: Option<&str>) -> Result<(), S3Error> {
    let Some(query) = query else {
        return Ok(());
    };
    let sensitive = [
        "prefix",
        "delimiter",
        "max-keys",
        "continuation-token",
        "start-after",
        "marker",
        "encoding-type",
        "fetch-owner",
    ];
    let mut seen = std::collections::HashSet::new();
    for pair in query.split('&').filter(|s| !s.is_empty()) {
        let raw_key = pair.split_once('=').map(|(k, _)| k).unwrap_or(pair);
        let key = crate::api::auth::percent_decode(raw_key);
        if sensitive.iter().any(|s| s.eq_ignore_ascii_case(&key)) && !seen.insert(key.clone()) {
            return Err(S3Error::InvalidArgument(format!(
                "duplicate query parameter '{}' is not allowed",
                key
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod query_validation_tests {
    use super::*;

    #[test]
    fn duplicate_prefix_query_is_rejected() {
        assert!(reject_duplicate_list_query_params(Some("list-type=2&prefix=a")).is_ok());
        assert!(matches!(
            reject_duplicate_list_query_params(Some("list-type=2&prefix=a&prefix=b")),
            Err(S3Error::InvalidArgument(_))
        ));
    }

    #[test]
    fn duplicate_percent_encoded_sensitive_key_is_rejected() {
        assert!(matches!(
            reject_duplicate_list_query_params(Some("prefix=a&pre%66ix=b")),
            Err(S3Error::InvalidArgument(_))
        ));
    }
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

/// ListMultipartUploads handler (L1 correctness fix: honours
/// max-uploads, key-marker, upload-id-marker query params instead of
/// hardcoding is_truncated=false).
///
/// GET /{bucket}?uploads
async fn list_multipart_uploads(
    state: &Arc<AppState>,
    bucket: &str,
    prefix: Option<&str>,
    key_marker: &str,
    upload_id_marker: &str,
    max_uploads: u32,
) -> Result<Response, S3Error> {
    let capped = max_uploads.clamp(1, 1000);
    let (uploads, is_truncated, next_key, next_upload_id) = state.multipart.list_uploads_paginated(
        Some(bucket),
        prefix,
        key_marker,
        upload_id_marker,
        capped,
    );
    let result = ListMultipartUploadsResult {
        bucket: bucket.to_string(),
        uploads,
        prefix: prefix.unwrap_or("").to_string(),
        max_uploads: capped,
        is_truncated,
        key_marker: key_marker.to_string(),
        upload_id_marker: upload_id_marker.to_string(),
        next_key_marker: next_key,
        next_upload_id_marker: next_upload_id,
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
    // Validate bucket name per S3 spec — must run before any stub returns
    // to prevent path traversal via unvalidated bucket names
    validate_bucket_name(&bucket)?;

    // PUT /{bucket}?acl — return 501 NotImplemented.
    //
    // M4 fix: we don't persist ACLs (the proxy uses IAM permissions
    // instead). Pre-fix this returned 200 OK while silently discarding
    // the ACL XML, leading clients to believe their grants had been
    // applied. NoSuchBucket precedence is preserved by the bucket-
    // existence check (404 wins over 501).
    if query.acl.is_some() {
        info!("PUT bucket ACL: unsupported (returning 501)");
        require_bucket_exists(&state, &bucket).await?;
        return Err(S3Error::NotImplemented(
            "Bucket ACLs are not supported by this proxy; use IAM policies instead".to_string(),
        ));
    }

    // PUT /{bucket}?tagging — return 501 NotImplemented.
    // L1 fix: 404 wins over 501 when the bucket doesn't exist.
    if query.tagging.is_some() {
        info!("PUT bucket tagging: unsupported (returning 501)");
        require_bucket_exists(&state, &bucket).await?;
        return Err(S3Error::NotImplemented(
            "Bucket tagging is not supported by this proxy".to_string(),
        ));
    }

    // PUT /{bucket}?versioning — return 501 NotImplemented.
    // M4 fix: pre-fix this accepted Suspended/Enabled XML and returned
    // 200 OK while ignoring it. Clients that thought versioning was
    // active would lose history on overwrite. 404 wins over 501.
    if query.versioning.is_some() {
        info!("PUT bucket versioning: unsupported (returning 501)");
        require_bucket_exists(&state, &bucket).await?;
        return Err(S3Error::NotImplemented(
            "Bucket versioning is not supported by this proxy".to_string(),
        ));
    }

    info!("CREATE bucket {}", bucket);

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

    // Only lowercase letters, numbers, hyphens, and dots
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.')
    {
        return Err(S3Error::InvalidBucketName(
            "Bucket name can only contain lowercase letters, numbers, hyphens, and dots"
                .to_string(),
        ));
    }
    // No consecutive dots
    if name.contains("..") {
        return Err(S3Error::InvalidBucketName(
            "Bucket name must not contain consecutive dots".to_string(),
        ));
    }

    // Must start with letter or number
    // Safety: length validated above (3..=63), so first/last are always Some.
    let first = name.chars().next().expect("bucket name length >= 3");
    if !first.is_ascii_alphanumeric() {
        return Err(S3Error::InvalidBucketName(
            "Bucket name must start with a letter or number".to_string(),
        ));
    }

    // Must end with letter or number
    let last = name.chars().last().expect("bucket name length >= 3");
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

    // H2 security fix: also refuse if there are active multipart uploads
    // targeting this bucket. Pre-fix, DeleteBucket returned 204 while
    // MultipartStore still held uploads for the now-deleted bucket —
    // UploadPart kept accepting bytes, ListMultipartUploads reported
    // uploads on a ghost bucket, and CompleteMultipartUpload would 404
    // via ensure_bucket_exists but leave the MPU state stuck.
    let mpu_count = state.multipart.count_uploads_for_bucket(&bucket);
    if mpu_count > 0 {
        return Err(S3Error::BucketNotEmpty(format!(
            "{} ({} multipart upload(s) in progress; abort them first)",
            bucket, mpu_count
        )));
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
pub async fn list_buckets(
    State(state): State<Arc<AppState>>,
    auth_user: Option<axum::Extension<AuthenticatedUser>>,
) -> Result<Response, S3Error> {
    info!("LIST buckets");

    // List real buckets from storage backend with actual creation dates
    let mut bucket_list = state.engine.load().list_buckets_with_dates().await?;
    bucket_list.sort_by(|a, b| a.0.cmp(&b.0));

    // IAM: filter to only buckets the user has ANY permission on.
    // A user with resource "my-bucket/prefix/*" should still see "my-bucket" in the list.
    if let Some(axum::Extension(ref user)) = auth_user {
        bucket_list.retain(|(name, _)| user.can_see_bucket(name));
    }

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

// ────────────────────────────────────────────────────────────────────
// Unit tests for bucket-name validation.
//
// Replaces 7 integration tests in tests/s3_compat_test.rs
// (`test_create_bucket_*`) that each spawned a full TestServer just
// to verify this pure function. A parametric unit test covers the
// same ground in <1ms with no network, no process spawn, no HTTP/XML
// round-trip.
//
// `validate_bucket_name` here is stricter than `validate_bucket` in
// `extractors.rs` — it adds the IP-format rejection that S3 requires
// at bucket creation time. The general-purpose extractor validator
// runs on every request path and doesn't enforce IP-format because
// pre-existing buckets named like IPs might legitimately exist in
// upstream storage.
// ────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::{is_ip_format, validate_bucket_name};
    use crate::api::errors::S3Error;

    /// Every name the spec considers valid must return Ok. Values
    /// chosen to exercise the edge of each rule (min-length, hyphens,
    /// digits-only, dots, mixed).
    #[test]
    fn validate_bucket_name_accepts_valid() {
        for name in [
            "abc",
            "my-bucket",
            "test123",
            "a-b-c",
            "abc-def-123",
            "a.b.c",
        ] {
            assert!(
                validate_bucket_name(name).is_ok(),
                "name {name:?} should be accepted but was rejected"
            );
        }
    }

    /// Parametric table of every invalid-shape case the integration
    /// tests used to cover, one line per case. Each returns
    /// `InvalidBucketName`. The tuple: (name, reason-label) — the
    /// label is never asserted on, it's there so a failure message
    /// points at WHICH case broke.
    #[test]
    fn validate_bucket_name_rejects_invalid_shapes() {
        let cases: &[(&str, &str)] = &[
            // Length
            ("ab", "2-char is below the 3-char minimum"),
            // 64 × 'a' — just over the 63-char ceiling.
            (
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "64-char is above the 63-char maximum",
            ),
            // Character-set
            ("MyBucket", "uppercase ASCII is forbidden"),
            ("my_bucket", "underscore is forbidden"),
            ("my bucket", "whitespace is forbidden"),
            ("my+bucket", "+ is forbidden"),
            // Shape (starts / ends)
            ("-my-bucket", "must start with alphanumeric"),
            ("my-bucket-", "must end with alphanumeric"),
            (".my-bucket", "must start with alphanumeric (dot)"),
            // Repeated delimiters
            ("my..bucket", "consecutive dots are forbidden"),
            // IP-format (this rule exists ONLY here, not in the extractor)
            ("192.168.1.1", "four dotted digit-groups is IP-format"),
            ("10.0.0.1", "four dotted digit-groups is IP-format"),
        ];

        for (name, why) in cases {
            let got = validate_bucket_name(name);
            assert!(
                matches!(got, Err(S3Error::InvalidBucketName(_))),
                "expected InvalidBucketName for {name:?} ({why}), got {got:?}"
            );
        }
    }

    /// Truth table for `is_ip_format`. Keeps the helper honest as
    /// the bucket-naming rules evolve (e.g. if AWS ever allows
    /// IPv6-like names, the rule would need to change here).
    #[test]
    fn is_ip_format_truth_table() {
        // Yes: exactly four groups of 1-3 ASCII digits, dot-separated.
        assert!(is_ip_format("1.2.3.4"));
        assert!(is_ip_format("192.168.1.1"));
        assert!(is_ip_format("255.255.255.255"));

        // No: wrong shape.
        assert!(!is_ip_format("1.2.3"), "three parts is not IP-format");
        assert!(!is_ip_format("1.2.3.4.5"), "five parts is not IP-format");
        assert!(!is_ip_format("a.b.c.d"), "letters are not IP-format");
        assert!(
            !is_ip_format("1234.5.6.7"),
            "group > 3 digits is not IP-format"
        );
        assert!(!is_ip_format(".1.2.3"), "leading dot → empty first group");
        assert!(!is_ip_format("1..2.3"), "middle empty group");
    }

    // ────────────────────────────────────────────────────────────────
    // Property-based tests (proptest).
    //
    // The enumerated cases above document specific invariants; these
    // generalize from "we checked these 27 cases" to "we checked 256
    // random cases per property, per run". Proptest shrinks failing
    // inputs to a minimal reproducer — instead of "property failed on
    // some 73-character input", you get "property failed on 'A'".
    //
    // Default 256 cases/property. Bump via `PROPTEST_CASES=N` in the
    // environment. Seed is deterministic per-property (`Config::
    // default` + body-hash), so reruns of the same code path are
    // reproducible.
    // ────────────────────────────────────────────────────────────────

    use proptest::prelude::*;

    /// Independent "this name satisfies every rule" predicate. Kept
    /// separate from the validator so the tests below can assert
    /// equivalence between the two without being circular.
    fn is_syntactically_valid(name: &str) -> bool {
        let len = name.chars().count();
        if !(3..=63).contains(&len) {
            return false;
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.')
        {
            return false;
        }
        if name.contains("..") {
            return false;
        }
        let first = name.chars().next().unwrap();
        let last = name.chars().last().unwrap();
        if !first.is_ascii_alphanumeric() || !last.is_ascii_alphanumeric() {
            return false;
        }
        if is_ip_format(name) {
            return false;
        }
        true
    }

    proptest! {
        /// Universality: for ANY ASCII string the validator's Ok/Err
        /// output must agree with the independent predicate above.
        /// If the two disagree for any input, either the validator
        /// drifted from the spec or the predicate is wrong — either
        /// way, we want to know.
        #[test]
        fn prop_validator_matches_spec(s in "\\PC*") {
            let validator_result = validate_bucket_name(&s).is_ok();
            let spec_result = is_syntactically_valid(&s);
            prop_assert_eq!(
                validator_result,
                spec_result,
                "drift between validator and spec for input {:?}",
                s
            );
        }

        /// Generator: build a structurally-valid name (chars in the
        /// allowed set, 3–63 chars, starts + ends alphanumeric, no
        /// double-dots). Filter out IP-format strings because those
        /// are a legitimate rejection case even though every other
        /// rule passes. Every accepted name must be Ok.
        #[test]
        fn prop_well_formed_names_are_accepted(
            name in "[a-z0-9][a-z0-9.-]{1,61}[a-z0-9]"
                .prop_filter("no consecutive dots", |s| !s.contains(".."))
                .prop_filter("not IP-format", |s| !is_ip_format(s))
        ) {
            prop_assert!(
                validate_bucket_name(&name).is_ok(),
                "well-formed name {:?} should be accepted",
                name
            );
        }

        /// Negative generator #1: contains at least one forbidden
        /// character (uppercase, underscore, whitespace, symbol).
        /// The forbidden chars make the whole name invalid regardless
        /// of its length or shape.
        #[test]
        fn prop_names_with_forbidden_chars_are_rejected(
            prefix in "[a-z0-9]{1,10}",
            bad in "[A-Z_ +!@#$%^&*]",
            suffix in "[a-z0-9]{1,10}",
        ) {
            let name = format!("{prefix}{bad}{suffix}");
            prop_assert!(
                validate_bucket_name(&name).is_err(),
                "name with forbidden char {:?} should be rejected: {:?}",
                bad,
                name
            );
        }

        /// Negative generator #2: length outside 3..=63. Character
        /// set is intentionally clean so the ONLY reason for
        /// rejection is length.
        #[test]
        fn prop_names_outside_length_range_are_rejected(
            // 0, 1, 2 char names (too short), 64..128 char names (too long).
            // proptest OneOf-style via two strategies.
            name in prop_oneof![
                "[a-z0-9]{0,2}",
                "[a-z0-9]{64,128}",
            ]
        ) {
            prop_assert!(
                validate_bucket_name(&name).is_err(),
                "length-{} name {:?} should be rejected",
                name.len(),
                name
            );
        }

        /// IP-format detector round-trip: given 4 groups of 1-3
        /// digits joined by `.`, is_ip_format must return true.
        /// Given any strictly different shape, false.
        #[test]
        fn prop_ip_format_four_digit_groups(
            a in "[0-9]{1,3}",
            b in "[0-9]{1,3}",
            c in "[0-9]{1,3}",
            d in "[0-9]{1,3}",
        ) {
            let s = format!("{a}.{b}.{c}.{d}");
            prop_assert!(
                is_ip_format(&s),
                "expected IP-format true for {:?}",
                s
            );
        }

        /// is_ip_format is false for any string without EXACTLY four
        /// dots. The generator produces strings with 0, 1, 2, 3, 5, or
        /// 6 dot-separated digit groups — never 4.
        #[test]
        fn prop_ip_format_false_for_non_four_parts(
            parts in prop::collection::vec("[0-9]{1,3}", 1..=3)
                .prop_union(prop::collection::vec("[0-9]{1,3}", 5..=6))
        ) {
            let s = parts.join(".");
            prop_assert!(
                !is_ip_format(&s),
                "expected IP-format false for {}-part input {:?}",
                parts.len(),
                s
            );
        }
    }
}
