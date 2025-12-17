//! S3 error types and XML responses

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use thiserror::Error;

/// S3 API errors
#[derive(Debug, Clone, Error)]
pub enum S3Error {
    #[error("NoSuchKey: The specified key does not exist.")]
    NoSuchKey(String),

    #[error("NoSuchBucket: The specified bucket does not exist.")]
    NoSuchBucket(String),

    #[error("BucketNotEmpty: The bucket you tried to delete is not empty.")]
    BucketNotEmpty(String),

    #[error("BucketAlreadyExists: The requested bucket name is not available.")]
    BucketAlreadyExists(String),

    #[error("EntityTooLarge: Your proposed upload exceeds the maximum allowed size.")]
    EntityTooLarge { size: u64, max: u64 },

    #[error("InternalError: We encountered an internal error. Please try again.")]
    InternalError(String),

    #[error("InvalidArgument: {0}")]
    InvalidArgument(String),

    #[error("InvalidRequest: {0}")]
    InvalidRequest(String),

    #[error("MalformedXML: The XML you provided was not well-formed.")]
    MalformedXML,
}

impl S3Error {
    /// Get the S3 error code
    pub fn code(&self) -> &'static str {
        match self {
            S3Error::NoSuchKey(_) => "NoSuchKey",
            S3Error::NoSuchBucket(_) => "NoSuchBucket",
            S3Error::BucketNotEmpty(_) => "BucketNotEmpty",
            S3Error::BucketAlreadyExists(_) => "BucketAlreadyExists",
            S3Error::EntityTooLarge { .. } => "EntityTooLarge",
            S3Error::InternalError(_) => "InternalError",
            S3Error::InvalidArgument(_) => "InvalidArgument",
            S3Error::InvalidRequest(_) => "InvalidRequest",
            S3Error::MalformedXML => "MalformedXML",
        }
    }

    /// Get the HTTP status code
    pub fn status_code(&self) -> StatusCode {
        match self {
            S3Error::NoSuchKey(_) => StatusCode::NOT_FOUND,
            S3Error::NoSuchBucket(_) => StatusCode::NOT_FOUND,
            S3Error::BucketNotEmpty(_) => StatusCode::CONFLICT,
            S3Error::BucketAlreadyExists(_) => StatusCode::CONFLICT,
            S3Error::EntityTooLarge { .. } => StatusCode::BAD_REQUEST,
            S3Error::InternalError(_) => StatusCode::INTERNAL_SERVER_ERROR,
            S3Error::InvalidArgument(_) => StatusCode::BAD_REQUEST,
            S3Error::InvalidRequest(_) => StatusCode::BAD_REQUEST,
            S3Error::MalformedXML => StatusCode::BAD_REQUEST,
        }
    }

    /// Generate XML error response
    pub fn to_xml(&self) -> String {
        let resource = match self {
            S3Error::NoSuchKey(key) => key.clone(),
            S3Error::NoSuchBucket(bucket) => bucket.clone(),
            _ => String::new(),
        };

        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<Error>
    <Code>{}</Code>
    <Message>{}</Message>
    <Resource>{}</Resource>
    <RequestId>00000000-0000-0000-0000-000000000000</RequestId>
</Error>"#,
            self.code(),
            self,
            resource
        )
    }
}

impl IntoResponse for S3Error {
    fn into_response(self) -> Response {
        let status = self.status_code();
        let body = self.to_xml();

        (status, [("Content-Type", "application/xml")], body).into_response()
    }
}

impl From<crate::storage::StorageError> for S3Error {
    fn from(err: crate::storage::StorageError) -> Self {
        match err {
            crate::storage::StorageError::NotFound(key) => S3Error::NoSuchKey(key),
            crate::storage::StorageError::TooLarge { size, max } => {
                S3Error::EntityTooLarge { size, max }
            }
            other => S3Error::InternalError(other.to_string()),
        }
    }
}
