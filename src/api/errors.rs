//! S3 error types and XML responses

use super::xml::escape_xml;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use thiserror::Error;

/// S3 API errors
#[derive(Debug, Error)]
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

    #[error("NoSuchUpload: The specified multipart upload does not exist.")]
    NoSuchUpload(String),

    #[error("InvalidPart: {0}")]
    InvalidPart(String),

    #[error("InvalidPartOrder: The list of parts was not in ascending order.")]
    InvalidPartOrder,

    #[error("BadDigest: The Content-MD5 you specified did not match what we received.")]
    BadDigest,

    #[error("InvalidDigest: The Content-MD5 you specified is not valid.")]
    InvalidDigest,

    #[error("NotImplemented: {0}")]
    NotImplemented(String),

    #[error("AccessDenied: Access Denied")]
    AccessDenied,

    #[error("SignatureDoesNotMatch: The request signature we calculated does not match the signature you provided.")]
    SignatureDoesNotMatch,

    #[error("SlowDown: Please reduce your request rate.")]
    SlowDown(String),

    #[error("RequestTimeTooSkewed: The difference between the request time and the server's time is too large.")]
    RequestTimeTooSkewed,

    #[error("InvalidBucketName: The specified bucket is not valid.")]
    InvalidBucketName(String),

    #[error("InvalidRange: The requested range is not satisfiable.")]
    InvalidRange,

    #[error("NotModified")]
    NotModified { etag: String, last_modified: String },

    #[error("PreconditionFailed: At least one of the pre-conditions you specified did not hold.")]
    PreconditionFailed,
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
            S3Error::NoSuchUpload(_) => "NoSuchUpload",
            S3Error::InvalidPart(_) => "InvalidPart",
            S3Error::InvalidPartOrder => "InvalidPartOrder",
            S3Error::BadDigest => "BadDigest",
            S3Error::InvalidDigest => "InvalidDigest",
            S3Error::NotImplemented(_) => "NotImplemented",
            S3Error::AccessDenied => "AccessDenied",
            S3Error::SignatureDoesNotMatch => "SignatureDoesNotMatch",
            S3Error::SlowDown(_) => "SlowDown",
            S3Error::RequestTimeTooSkewed => "RequestTimeTooSkewed",
            S3Error::InvalidBucketName(_) => "InvalidBucketName",
            S3Error::InvalidRange => "InvalidRange",
            S3Error::NotModified { .. } => "NotModified",
            S3Error::PreconditionFailed => "PreconditionFailed",
        }
    }

    /// Get the HTTP status code
    pub fn status_code(&self) -> StatusCode {
        match self {
            S3Error::NoSuchKey(_) => StatusCode::NOT_FOUND,
            S3Error::NoSuchBucket(_) => StatusCode::NOT_FOUND,
            S3Error::BucketNotEmpty(_) => StatusCode::CONFLICT,
            S3Error::BucketAlreadyExists(_) => StatusCode::CONFLICT,
            S3Error::EntityTooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
            S3Error::InternalError(_) => StatusCode::INTERNAL_SERVER_ERROR,
            S3Error::InvalidArgument(_) => StatusCode::BAD_REQUEST,
            S3Error::InvalidRequest(_) => StatusCode::BAD_REQUEST,
            S3Error::MalformedXML => StatusCode::BAD_REQUEST,
            S3Error::NoSuchUpload(_) => StatusCode::NOT_FOUND,
            S3Error::InvalidPart(_) => StatusCode::BAD_REQUEST,
            S3Error::InvalidPartOrder => StatusCode::BAD_REQUEST,
            S3Error::BadDigest => StatusCode::BAD_REQUEST,
            S3Error::InvalidDigest => StatusCode::BAD_REQUEST,
            S3Error::NotImplemented(_) => StatusCode::NOT_IMPLEMENTED,
            S3Error::AccessDenied => StatusCode::FORBIDDEN,
            S3Error::SignatureDoesNotMatch => StatusCode::FORBIDDEN,
            S3Error::SlowDown(_) => StatusCode::SERVICE_UNAVAILABLE,
            S3Error::RequestTimeTooSkewed => StatusCode::FORBIDDEN,
            S3Error::InvalidBucketName(_) => StatusCode::BAD_REQUEST,
            S3Error::InvalidRange => StatusCode::RANGE_NOT_SATISFIABLE,
            S3Error::NotModified { .. } => StatusCode::NOT_MODIFIED,
            S3Error::PreconditionFailed => StatusCode::PRECONDITION_FAILED,
        }
    }

    /// Generate XML error response with a unique request ID.
    pub fn to_xml(&self, request_id: &str) -> String {
        let resource = match self {
            S3Error::NoSuchKey(key) => escape_xml(key),
            S3Error::NoSuchBucket(bucket) => escape_xml(bucket),
            _ => String::new(),
        };

        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<Error>
    <Code>{}</Code>
    <Message>{}</Message>
    <Resource>{}</Resource>
    <RequestId>{}</RequestId>
</Error>"#,
            self.code(),
            escape_xml(&self.to_string()),
            resource,
            request_id
        )
    }
}

impl IntoResponse for S3Error {
    fn into_response(self) -> Response {
        let status = self.status_code();
        let request_id = uuid::Uuid::new_v4().to_string();

        // NotModified has no body per HTTP spec, but MUST include ETag and Last-Modified (RFC 7232)
        if let S3Error::NotModified {
            ref etag,
            ref last_modified,
        } = self
        {
            let mut headers = axum::http::HeaderMap::new();
            headers.insert(
                "x-amz-request-id",
                axum::http::HeaderValue::from_str(&request_id).unwrap(),
            );
            headers.insert("ETag", axum::http::HeaderValue::from_str(etag).unwrap());
            headers.insert(
                "Last-Modified",
                axum::http::HeaderValue::from_str(last_modified).unwrap(),
            );
            return (status, headers).into_response();
        }

        let body = self.to_xml(&request_id);

        let mut response = (status, [("Content-Type", "application/xml")], body).into_response();
        response.headers_mut().insert(
            "x-amz-request-id",
            axum::http::HeaderValue::from_str(&request_id).unwrap(),
        );
        response
    }
}

impl From<crate::storage::StorageError> for S3Error {
    fn from(err: crate::storage::StorageError) -> Self {
        match err {
            crate::storage::StorageError::NotFound(key) => S3Error::NoSuchKey(key),
            crate::storage::StorageError::BucketNotFound(b) => S3Error::NoSuchBucket(b),
            crate::storage::StorageError::BucketNotEmpty(b) => S3Error::BucketNotEmpty(b),
            crate::storage::StorageError::AlreadyExists(b) => S3Error::BucketAlreadyExists(b),
            crate::storage::StorageError::TooLarge { size, max } => {
                S3Error::EntityTooLarge { size, max }
            }
            crate::storage::StorageError::DiskFull => S3Error::InternalError(
                "Insufficient storage space. The server's disk is full.".to_string(),
            ),
            other => S3Error::InternalError(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: EntityTooLarge must return 413, not 400.
    /// S3 clients rely on the status code to distinguish size errors from bad requests.
    #[test]
    fn entity_too_large_returns_413() {
        let err = S3Error::EntityTooLarge {
            size: 200,
            max: 100,
        };
        assert_eq!(err.status_code(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(err.status_code().as_u16(), 413);
    }

    /// Verify all S3 error status codes match S3 API specification.
    #[test]
    fn error_status_codes_match_s3_spec() {
        assert_eq!(
            S3Error::NoSuchKey("k".into()).status_code(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            S3Error::NoSuchBucket("b".into()).status_code(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            S3Error::BucketNotEmpty("b".into()).status_code(),
            StatusCode::CONFLICT
        );
        assert_eq!(S3Error::AccessDenied.status_code(), StatusCode::FORBIDDEN);
        assert_eq!(
            S3Error::SignatureDoesNotMatch.status_code(),
            StatusCode::FORBIDDEN
        );
    }

    /// SlowDown (codec backpressure) must return 503 with the correct S3 error code.
    #[test]
    fn slow_down_returns_503() {
        let err = S3Error::SlowDown("busy".into());
        assert_eq!(err.status_code(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(err.status_code().as_u16(), 503);
        assert_eq!(err.code(), "SlowDown");
    }
}
