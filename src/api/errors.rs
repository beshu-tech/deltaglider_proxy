// SPDX-License-Identifier: BUSL-1.1

//! S3 error types and XML responses

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use thiserror::Error;

/// Escape XML special characters for embedding user-controlled
/// strings in S3 error responses. Lives here (rather than a separate
/// `xml` module) because this is the only remaining consumer once
/// the axum response builders moved into the s3s adapter.
fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// S3 API errors
#[derive(Debug, Error)]
pub enum S3Error {
    #[error("NoSuchKey: The specified key does not exist.")]
    NoSuchKey(String),

    #[error("NoSuchBucket: The specified bucket does not exist.")]
    NoSuchBucket(String),

    #[error("BucketNotEmpty: The bucket you tried to delete is not empty: {0}")]
    BucketNotEmpty(String),

    #[error("BucketAlreadyExists: The requested bucket name is not available.")]
    BucketAlreadyExists(String),

    #[error("EntityTooLarge: Your proposed upload exceeds the maximum allowed size.")]
    EntityTooLarge { size: u64, max: u64 },

    /// `EntityTooLarge` whose message names the limit (and how to raise it).
    #[error("EntityTooLarge: {0}")]
    EntityTooLargeReason(String),

    #[error("InternalError: {0}")]
    InternalError(String),

    #[error("InvalidArgument: {0}")]
    InvalidArgument(String),

    #[error("KeyTooLongError: {0}")]
    KeyTooLong(String),

    /// User or stored metadata over the limit (400, S3 code `MetadataTooLarge`).
    #[error("MetadataTooLarge: {0}")]
    MetadataTooLarge(String),

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

    /// AccessDenied with a specific reason in the `<Message>` (S3 code stays
    /// `AccessDenied`). Used by the admission chain so a denied client sees
    /// which block fired (`admission-deny:<block>`) — a deliberate operator
    /// debugging affordance, asserted by `tests/admission_test.rs`.
    #[error("{0}")]
    AccessDeniedReason(String),

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

    #[error("PreconditionFailed: At least one of the pre-conditions you specified did not hold.")]
    PreconditionFailed,

    /// 503 Service Unavailable with an operator-facing recovery message.
    /// Used when the proxy cannot serve S3 traffic (e.g. config DB locked).
    #[error("ServiceUnavailable: {0}")]
    ServiceUnavailable(String),
}

impl S3Error {
    /// Get the S3 error code
    pub fn code(&self) -> &'static str {
        match self {
            S3Error::NoSuchKey(_) => "NoSuchKey",
            S3Error::NoSuchBucket(_) => "NoSuchBucket",
            S3Error::BucketNotEmpty(_) => "BucketNotEmpty",
            S3Error::BucketAlreadyExists(_) => "BucketAlreadyExists",
            S3Error::EntityTooLarge { .. } | S3Error::EntityTooLargeReason(_) => "EntityTooLarge",
            S3Error::InternalError(_) => "InternalError",
            S3Error::InvalidArgument(_) => "InvalidArgument",
            S3Error::KeyTooLong(_) => "KeyTooLongError",
            S3Error::MetadataTooLarge(_) => "MetadataTooLarge",
            S3Error::InvalidRequest(_) => "InvalidRequest",
            S3Error::MalformedXML => "MalformedXML",
            S3Error::NoSuchUpload(_) => "NoSuchUpload",
            S3Error::InvalidPart(_) => "InvalidPart",
            S3Error::InvalidPartOrder => "InvalidPartOrder",
            S3Error::BadDigest => "BadDigest",
            S3Error::InvalidDigest => "InvalidDigest",
            S3Error::NotImplemented(_) => "NotImplemented",
            S3Error::AccessDenied => "AccessDenied",
            S3Error::AccessDeniedReason(_) => "AccessDenied",
            S3Error::SignatureDoesNotMatch => "SignatureDoesNotMatch",
            S3Error::SlowDown(_) => "SlowDown",
            S3Error::RequestTimeTooSkewed => "RequestTimeTooSkewed",
            S3Error::InvalidBucketName(_) => "InvalidBucketName",
            S3Error::InvalidRange => "InvalidRange",
            S3Error::PreconditionFailed => "PreconditionFailed",
            S3Error::ServiceUnavailable(_) => "ServiceUnavailable",
        }
    }

    /// Get the HTTP status code
    pub fn status_code(&self) -> StatusCode {
        match self {
            S3Error::NoSuchKey(_) => StatusCode::NOT_FOUND,
            S3Error::NoSuchBucket(_) => StatusCode::NOT_FOUND,
            S3Error::BucketNotEmpty(_) => StatusCode::CONFLICT,
            S3Error::BucketAlreadyExists(_) => StatusCode::CONFLICT,
            S3Error::EntityTooLarge { .. } | S3Error::EntityTooLargeReason(_) => {
                StatusCode::PAYLOAD_TOO_LARGE
            }
            S3Error::InternalError(_) => StatusCode::INTERNAL_SERVER_ERROR,
            S3Error::InvalidArgument(_) => StatusCode::BAD_REQUEST,
            S3Error::KeyTooLong(_) => StatusCode::BAD_REQUEST,
            S3Error::MetadataTooLarge(_) => StatusCode::BAD_REQUEST,
            S3Error::InvalidRequest(_) => StatusCode::BAD_REQUEST,
            S3Error::MalformedXML => StatusCode::BAD_REQUEST,
            S3Error::NoSuchUpload(_) => StatusCode::NOT_FOUND,
            S3Error::InvalidPart(_) => StatusCode::BAD_REQUEST,
            S3Error::InvalidPartOrder => StatusCode::BAD_REQUEST,
            S3Error::BadDigest => StatusCode::BAD_REQUEST,
            S3Error::InvalidDigest => StatusCode::BAD_REQUEST,
            S3Error::NotImplemented(_) => StatusCode::NOT_IMPLEMENTED,
            S3Error::AccessDenied => StatusCode::FORBIDDEN,
            S3Error::AccessDeniedReason(_) => StatusCode::FORBIDDEN,
            S3Error::SignatureDoesNotMatch => StatusCode::FORBIDDEN,
            S3Error::SlowDown(_) => StatusCode::SERVICE_UNAVAILABLE,
            S3Error::RequestTimeTooSkewed => StatusCode::FORBIDDEN,
            S3Error::InvalidBucketName(_) => StatusCode::BAD_REQUEST,
            S3Error::InvalidRange => StatusCode::RANGE_NOT_SATISFIABLE,
            S3Error::PreconditionFailed => StatusCode::PRECONDITION_FAILED,
            S3Error::ServiceUnavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
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

        let body = self.to_xml(&request_id);

        let mut response = (status, [("Content-Type", "application/xml")], body).into_response();
        response.headers_mut().insert(
            "x-amz-request-id",
            axum::http::HeaderValue::from_str(&request_id).unwrap(),
        );
        response
    }
}

/// The client message for a key the filesystem backend cannot store.
pub(crate) const KEY_TOO_LONG_FS: &str = "Your key is too long for this storage backend: the \
     filesystem backend stores each '/'-separated part of a key as a file name of at most \
     255 bytes, including a '.delta' suffix on delta-compressed objects";

/// The client message for a key whose path is taken by the other kind of
/// entry on the filesystem backend (a directory where the object's file must
/// be, or a file where its parent directory must be).
pub(crate) const KEY_PATH_CONFLICT_FS: &str = "This key cannot be stored on the filesystem \
     backend: it stores key 'a' as a file and keys 'a/...' under a directory 'a', so an \
     object 'a' and an object under 'a/' cannot both exist. Delete the other object, or \
     use an S3 backend";

impl From<crate::storage::StorageError> for S3Error {
    fn from(err: crate::storage::StorageError) -> Self {
        match err {
            crate::storage::StorageError::NotFound(key) => S3Error::NoSuchKey(key),
            crate::storage::StorageError::InvalidKey(msg) => S3Error::InvalidArgument(msg),
            crate::storage::StorageError::KeyTooLong(msg) => S3Error::KeyTooLong(msg),
            crate::storage::StorageError::Io(e)
                if crate::storage::io_error_is_name_too_long(&e) =>
            {
                S3Error::KeyTooLong(KEY_TOO_LONG_FS.to_string())
            }
            crate::storage::StorageError::Io(e)
                if crate::storage::io_error_is_path_type_conflict(&e) =>
            {
                S3Error::InvalidRequest(KEY_PATH_CONFLICT_FS.to_string())
            }
            crate::storage::StorageError::MetadataTooLarge(msg) => S3Error::MetadataTooLarge(msg),
            crate::storage::StorageError::InvalidRange(_) => S3Error::InvalidRange,
            crate::storage::StorageError::BucketNotFound(b) => S3Error::NoSuchBucket(b),
            crate::storage::StorageError::BucketNotEmpty(b) => S3Error::BucketNotEmpty(b),
            crate::storage::StorageError::AlreadyExists(b) => S3Error::BucketAlreadyExists(b),
            crate::storage::StorageError::TooLarge { size, max } => {
                S3Error::EntityTooLarge { size, max }
            }
            crate::storage::StorageError::DiskFull => S3Error::InternalError(
                "Insufficient storage space. The server's disk is full.".to_string(),
            ),
            // E-P1-1: backend throttling propagates as a 503 SlowDown
            // so AWS-SDK clients honour the spec retry/backoff
            // contract. Pre-fix this fell into the catch-all below,
            // surfacing as a 500 InternalError that SDKs treat as
            // permanent.
            crate::storage::StorageError::Throttled(_) => S3Error::SlowDown(
                "Backend signalled transient pressure; please retry with backoff.".to_string(),
            ),
            // The message names the op and bucket (and the backend, when
            // S3Backend knows it), never credentials or endpoint URLs.
            crate::storage::StorageError::Unavailable(msg) => {
                S3Error::ServiceUnavailable(format!("the storage backend did not answer: {msg}"))
            }
            other => S3Error::InternalError(sanitise_for_client(&other)),
        }
    }
}

/// Translate backend error text into something safe to send to an S3
/// client. The full error is preserved for `tracing::error!` + the
/// audit ring; only the response body gets the sanitised version.
///
/// Motivated by E4 in the adversarial audit: backend `StorageError::Other`,
/// `EngineError::ChecksumMismatch`, and friends stringified into
/// response bodies could reveal computed/expected hashes, absolute
/// filesystem paths, or backend implementation details (MinIO debug
/// strings, S3 request IDs, xdelta3 stderr). None of that belongs in
/// a client's hands — they can't act on it, and it helps attackers
/// fingerprint the stack.
///
/// The return value is deliberately generic. Operators read the real
/// error in logs; clients see "Internal server error."
pub fn sanitise_for_client(err: &dyn std::fmt::Display) -> String {
    // Log the full detail exactly once per sanitisation; a caller that
    // logs 500s skips an error whose cause this line already carries
    // (`S3Error::cause_is_logged`). The default target (this module): the
    // log filter is a list of crate targets (`deltaglider_proxy=…`), and an
    // event under any other target is dropped, as the old
    // `dgp::sanitised_error` was.
    tracing::error!("sanitised 500, cause: {}", err);
    SANITISED_500.to_string()
}

/// The client text of a sanitised 500.
const SANITISED_500: &str = "Internal server error. See server logs for details.";

impl S3Error {
    /// True for a 500 whose cause `sanitise_for_client` already logged: a
    /// second log line would repeat only the generic client text.
    pub fn cause_is_logged(&self) -> bool {
        matches!(self, S3Error::InternalError(m) if m == SANITISED_500)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Source guard: a `tracing` target outside `deltaglider_proxy` is
    /// dropped by the default filter (`deltaglider_proxy=debug,…`, plus the
    /// audit directive `deltaglider_proxy::audit`), so the event never
    /// reaches a log. Every literal `target: "…"` names the crate.
    #[test]
    fn no_log_event_uses_a_target_outside_the_crate() {
        use crate::source_scan::{prod_lines, prod_sources};
        let key = ["target", ": \""].concat();
        let mut offenders = Vec::new();
        for (rel, text) in prod_sources("src") {
            for (n, line) in prod_lines(&text) {
                let Some(at) = line.find(key.as_str()) else {
                    continue;
                };
                let value = &line[at + key.len()..];
                let Some(end) = value.find('"') else {
                    continue;
                };
                // `target: "x".into()` is a struct field, not a log target.
                let is_log_target = !value[end + 1..].starts_with('.');
                if is_log_target && !value[..end].starts_with("deltaglider_proxy") {
                    offenders.push(format!("{rel}:{n}: {}", line.trim()));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "a log target outside the crate (the default filter drops it):\n{}",
            offenders.join("\n")
        );
    }

    /// A filesystem key whose path is the other kind of entry is a client
    /// error that names the limitation, never a retried 500 (s3surface-9).
    #[test]
    fn filesystem_path_type_conflict_is_invalid_request() {
        for errno in [libc::EISDIR, libc::ENOTDIR] {
            let err: S3Error =
                crate::storage::StorageError::Io(std::io::Error::from_raw_os_error(errno)).into();
            assert_eq!(err.code(), "InvalidRequest", "errno {errno}");
            assert_eq!(err.status_code(), StatusCode::BAD_REQUEST);
            assert!(err.to_string().contains("filesystem backend"));
        }
        let other: S3Error =
            crate::storage::StorageError::Io(std::io::Error::from_raw_os_error(libc::EIO)).into();
        assert_eq!(other.code(), "InternalError");
    }

    /// Every surface answers oversized metadata with the S3 code
    /// `MetadataTooLarge`; form POST and stored metadata answered
    /// `InvalidArgument` (s3surface-18).
    #[test]
    fn metadata_too_large_has_its_own_code() {
        let err: S3Error = crate::storage::StorageError::MetadataTooLarge("big".into()).into();
        assert_eq!(err.code(), "MetadataTooLarge");
        assert_eq!(err.status_code(), StatusCode::BAD_REQUEST);
        assert!(err.to_xml("id").contains("<Code>MetadataTooLarge</Code>"));
        let form = S3Error::MetadataTooLarge(
            crate::api::handlers::object_helpers::user_metadata_too_large_message(3000),
        );
        assert_eq!(form.code(), "MetadataTooLarge");
        assert!(form.to_string().contains("3000 bytes"));
    }

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

    /// E-P1-1 regression: backend `Throttled` must surface as
    /// `S3Error::SlowDown` (HTTP 503), not `S3Error::InternalError`
    /// (HTTP 500). Pre-fix the `From<StorageError>` catch-all swept
    /// `StorageError::S3("...status=503...")` into `InternalError`,
    /// breaking the AWS-SDK retry/backoff contract.
    #[test]
    fn throttled_storage_error_maps_to_slow_down() {
        let storage = crate::storage::StorageError::Throttled(
            "PutObject throttled (status=503): SlowDown".into(),
        );
        let s3: S3Error = storage.into();
        assert_eq!(s3.status_code(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(s3.status_code().as_u16(), 503);
        assert_eq!(s3.code(), "SlowDown");
    }

    /// E4 security fix: `sanitise_for_client` must return a generic string,
    /// NOT leak the underlying error text into the response body.
    #[test]
    fn sanitise_for_client_returns_generic_string() {
        let leaky = "/var/lib/dgp/secrets/backup.db contains hash abc123def456";
        let sanitised = sanitise_for_client(&leaky);
        assert!(
            !sanitised.contains("abc123def456"),
            "sanitised output must not contain hashes: {}",
            sanitised
        );
        assert!(
            !sanitised.contains("/var/lib/dgp"),
            "sanitised output must not contain filesystem paths: {}",
            sanitised
        );
        assert!(
            sanitised.starts_with("Internal server error"),
            "sanitised output should signal internal error: {}",
            sanitised
        );
    }

    /// Verify the StorageError::Other conversion uses the sanitiser —
    /// not the raw err.to_string().
    #[test]
    fn storage_error_other_is_sanitised_in_s3_internal_error() {
        use crate::storage::StorageError;

        let leaky = StorageError::Other(
            "/secret/path.db MD5 mismatch: expected 0xDEAD got 0xBEEF".to_string(),
        );
        let s3: S3Error = leaky.into();
        match s3 {
            S3Error::InternalError(msg) => {
                assert!(!msg.contains("0xDEAD"));
                assert!(!msg.contains("/secret/path.db"));
                assert!(msg.starts_with("Internal server error"));
            }
            other => panic!("expected InternalError, got {:?}", other),
        }
    }
}
