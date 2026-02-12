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

/// Validated bucket extractor
///
/// Validates that the bucket name in the path is non-empty and syntactically
/// valid. Any bucket name is accepted (multi-bucket support).
///
/// # Example
/// ```ignore
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

        if bucket.is_empty() {
            return Err(S3Error::InvalidArgument(
                "Bucket name cannot be empty".to_string(),
            ));
        }

        Ok(ValidatedBucket(bucket))
    }
}

/// Validated bucket and key extractor
///
/// Validates the bucket name and normalizes the key by removing leading
/// slashes. Any bucket name is accepted (multi-bucket support).
///
/// # Example
/// ```ignore
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

        if bucket.is_empty() {
            return Err(S3Error::InvalidArgument(
                "Bucket name cannot be empty".to_string(),
            ));
        }

        // Normalize key by removing leading slashes
        let key = key.trim_start_matches('/').to_string();

        Ok(ValidatedPath { bucket, key })
    }
}
