//! Custom Axum extractors for S3 API validation
//!
//! These extractors provide automatic validation of S3 request parameters,
//! eliminating repetitive validation code from handlers.

use super::errors::S3Error;
use super::handlers::AppState;
use axum::{
    async_trait,
    extract::{FromRef, FromRequestParts, Path},
    http::request::Parts,
};
use std::sync::Arc;

/// Reject bucket names that could cause path traversal or other mischief.
/// This is a security-critical check: on the filesystem backend, the bucket
/// name becomes a directory under the data root. Without validation, names
/// like `..` or `../../etc` would escape the data directory.
///
/// Uses the same S3 bucket naming rules as `create_bucket`:
/// 3-63 chars, lowercase ASCII + digits + hyphens + dots, no `..`.
fn validate_bucket(name: &str) -> Result<(), S3Error> {
    if name.is_empty() {
        return Err(S3Error::InvalidArgument(
            "Bucket name cannot be empty".to_string(),
        ));
    }
    let len = name.len();
    if !(3..=63).contains(&len) {
        return Err(S3Error::InvalidBucketName(format!(
            "Bucket name must be between 3 and 63 characters long, got {}",
            len
        )));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.')
    {
        return Err(S3Error::InvalidBucketName(
            "Bucket name can only contain lowercase letters, numbers, hyphens, and dots"
                .to_string(),
        ));
    }
    if name.contains("..") {
        return Err(S3Error::InvalidBucketName(
            "Bucket name must not contain consecutive dots".to_string(),
        ));
    }
    // Must start and end with alphanumeric (S3 spec)
    if !name.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        return Err(S3Error::InvalidBucketName(
            "Bucket name must start with a letter or number".to_string(),
        ));
    }
    if !name.ends_with(|c: char| c.is_ascii_alphanumeric()) {
        return Err(S3Error::InvalidBucketName(
            "Bucket name must end with a letter or number".to_string(),
        ));
    }
    Ok(())
}

/// Validated bucket extractor
///
/// Validates that the bucket name in the path is non-empty and syntactically
/// valid per S3 naming rules. Rejects names that could cause path traversal.
///
/// # Example
/// ```text
/// async fn list_objects(
///     State(state): State<Arc<AppState>>,
///     ValidatedBucket(bucket): ValidatedBucket,
///     Query(query): Query<ListQuery>,
/// ) -> Result<Response, S3Error> {
///     // bucket is guaranteed to be valid here
/// }
/// ```
#[derive(Debug, Clone)]
pub struct ValidatedBucket(pub String);

impl std::ops::Deref for ValidatedBucket {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[async_trait]
impl<S> FromRequestParts<S> for ValidatedBucket
where
    S: Send + Sync,
    Arc<AppState>: FromRef<S>,
{
    type Rejection = S3Error;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Path(bucket): Path<String> = Path::from_request_parts(parts, state)
            .await
            .map_err(|_| S3Error::InvalidArgument("Invalid bucket path".to_string()))?;

        validate_bucket(&bucket)?;

        Ok(ValidatedBucket(bucket))
    }
}

/// Validated bucket and key extractor
///
/// Validates the bucket name and normalizes the key by removing leading
/// slashes. Any bucket name is accepted (multi-bucket support).
///
/// # Example
/// ```text
/// async fn get_object(
///     State(state): State<Arc<AppState>>,
///     ValidatedPath { bucket, key }: ValidatedPath,
/// ) -> Result<Response, S3Error> {
///     // bucket is validated, key is normalized (no leading slashes)
/// }
/// ```
#[derive(Debug, Clone)]
pub struct ValidatedPath {
    pub bucket: String,
    pub key: String,
}

#[async_trait]
impl<S> FromRequestParts<S> for ValidatedPath
where
    S: Send + Sync,
    Arc<AppState>: FromRef<S>,
{
    type Rejection = S3Error;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let Path((bucket, key)): Path<(String, String)> = Path::from_request_parts(parts, state)
            .await
            .map_err(|_| S3Error::InvalidArgument("Invalid bucket/key path".to_string()))?;

        validate_bucket(&bucket)?;

        // Normalize key by removing leading slashes
        let key = key.trim_start_matches('/').to_string();

        // Reject keys containing path traversal segments
        if key.split('/').any(|seg| seg == ".." || seg == ".") {
            return Err(S3Error::InvalidArgument(
                "Key must not contain '.' or '..' path segments".to_string(),
            ));
        }

        Ok(ValidatedPath { bucket, key })
    }
}
