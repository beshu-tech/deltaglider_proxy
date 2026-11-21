// SPDX-License-Identifier: BUSL-1.1

//! THE length limit of the key and the query text of an S3 request URL, which
//! the proxy matches against policies: an object key, a prefix, a marker, a
//! delimiter. S3 allows 1024 bytes in a key. The key of the path is checked for
//! EVERY method (s3s refuses a longer one anyway), and so is the query text of
//! a LIST; both are refused BEFORE authorization, with the error that AWS
//! answers: `KeyTooLongError` for a key, `InvalidArgument` for a query value.
//! Key text that is not in the URL (the keys of a DeleteObjects body, a copy
//! source, the keys and prefixes of the admin bulk endpoints) is bounded where
//! a policy reads it: `CompiledPolicy::evaluate` denies text over the same
//! limit for every principal that is not an unconditional admin, so an object
//! whose key is longer (the filesystem backend limits a path segment, not the
//! whole key) stays reachable for an admin through those entry points. The
//! form POST key field calls [`check_key`] itself (source guard
//! `every_s3_text_input_is_length_checked`).

use crate::api::request_target::RequestTarget;
use crate::api::S3Error;

/// The longest object key, in bytes (S3's limit).
pub const MAX_KEY_BYTES: usize = 1024;

/// The longest `prefix`, `start-after`, `marker`, `key-marker` or `delimiter`.
pub const MAX_QUERY_TEXT_BYTES: usize = 1024;

/// The query parameters whose text a LIST matches against keys. The opaque
/// `continuation-token` is not here: it is not text that a policy sees.
const QUERY_TEXT_PARAMS: &[&str] = &["prefix", "start-after", "marker", "key-marker", "delimiter"];

/// Pure: `key` is at most [`MAX_KEY_BYTES`] bytes, else `KeyTooLongError`.
pub fn check_key(key: &str) -> Result<(), S3Error> {
    if key.len() > MAX_KEY_BYTES {
        return Err(S3Error::KeyTooLong(format!(
            "Your key is {} bytes long; the longest key is {MAX_KEY_BYTES} bytes",
            key.len()
        )));
    }
    Ok(())
}

/// Pure: the value of query parameter `name` is at most
/// [`MAX_QUERY_TEXT_BYTES`] bytes, else `InvalidArgument`.
pub fn check_query_text(name: &str, value: &str) -> Result<(), S3Error> {
    if value.len() > MAX_QUERY_TEXT_BYTES {
        return Err(S3Error::InvalidArgument(format!(
            "The value of {name} is {} bytes long; the longest value is {MAX_QUERY_TEXT_BYTES} bytes",
            value.len()
        )));
    }
    Ok(())
}

/// Pure: the key of the path (whatever the method) and every text parameter
/// of the query of a decoded request target.
pub fn check_target(target: &RequestTarget) -> Result<(), S3Error> {
    check_key(target.key_as_s3s_sees_it())?;
    for (name, value) in &target.query {
        if QUERY_TEXT_PARAMS.contains(&name.as_str()) {
            check_query_text(name, value)?;
        }
    }
    Ok(())
}

/// Axum middleware: refuse an over-long key or query text before
/// authentication and authorization see it. A target that does not decode is
/// left to the layers below, which refuse it.
// `Err` is the early-response short-circuit axum middleware idiom.
#[allow(clippy::result_large_err)]
pub async fn input_limits_middleware(
    request: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> Result<axum::response::Response, axum::response::Response> {
    use axum::response::IntoResponse;
    if let Ok(target) = RequestTarget::from_uri(request.uri()) {
        check_target(&target).map_err(IntoResponse::into_response)?;
    }
    Ok(next.run(request).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(path: &str, query: &str) -> RequestTarget {
        RequestTarget::parse(path, Some(query)).unwrap()
    }

    /// The middleware refuses before the handler runs, and lets a key of
    /// the limit and the opaque continuation token through.
    #[tokio::test]
    async fn the_middleware_answers_before_the_handler_runs() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;
        let app = || {
            axum::Router::new()
                .fallback(|| async { StatusCode::OK })
                .layer(axum::middleware::from_fn(input_limits_middleware))
        };
        let status = |uri: String| async move {
            app()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap()
        };
        let long = "a".repeat(1025);
        let response = status(format!("/b?list-type=2&prefix={long}")).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        assert!(String::from_utf8_lossy(&body).contains("<Code>InvalidArgument</Code>"));
        // A long key is refused for every method.
        for method in ["PUT", "POST", "GET", "HEAD", "DELETE"] {
            let response = app()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(format!("/b/{long}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{method}");
            let body = axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .unwrap();
            assert!(String::from_utf8_lossy(&body).contains("<Code>KeyTooLongError</Code>"));
        }
        let ok = "a".repeat(1024);
        assert_eq!(status(format!("/b/{ok}")).await.status(), StatusCode::OK);
        assert_eq!(
            status(format!("/b?prefix={ok}&continuation-token={long}"))
                .await
                .status(),
            StatusCode::OK
        );
    }

    #[test]
    fn a_key_of_1024_bytes_passes_and_1025_does_not() {
        assert!(check_key(&"k".repeat(1024)).is_ok());
        let err = check_key(&"k".repeat(1025)).unwrap_err();
        assert_eq!(err.code(), "KeyTooLongError");
    }

    #[test]
    fn the_limit_counts_bytes_not_characters() {
        // 513 two-byte characters are 1026 bytes.
        assert!(check_key(&"é".repeat(512)).is_ok());
        assert!(check_key(&"é".repeat(513)).is_err());
    }

    #[test]
    fn every_text_parameter_is_limited_and_the_token_is_not() {
        let long = "p".repeat(1025);
        for name in ["prefix", "start-after", "marker", "key-marker", "delimiter"] {
            let err = check_target(&target("/b", &format!("{name}={long}"))).unwrap_err();
            assert_eq!(err.code(), "InvalidArgument", "{name}");
            assert!(check_target(&target("/b", &format!("{name}={}", "p".repeat(1024)))).is_ok());
        }
        assert!(check_target(&target("/b", &format!("continuation-token={long}"))).is_ok());
    }

    /// Leading slashes are part of the key that s3s hands on: `/b/` and 2000
    /// slashes and `k` is a key of 2001 bytes, whatever the policy trims.
    #[test]
    fn leading_slashes_of_the_key_count() {
        let slashes = "/".repeat(2000);
        let err = check_target(&target(&format!("/b/{slashes}k"), "")).unwrap_err();
        assert_eq!(err.code(), "KeyTooLongError");
        assert!(check_target(&target(&format!("/b/{}k", "/".repeat(1023)), "")).is_ok());
        assert!(check_target(&target(&format!("/b/{}k", "/".repeat(1024)), "")).is_err());
    }

    #[test]
    fn the_key_of_the_path_is_limited_after_decoding() {
        let key = "k".repeat(1025);
        let err = check_target(&target(&format!("/b/{key}"), "")).unwrap_err();
        assert_eq!(err.code(), "KeyTooLongError");
        // The decoded text counts, not the percent-encoded spelling.
        let encoded = "%6B".repeat(1024);
        assert!(check_target(&target(&format!("/b/{encoded}"), "")).is_ok());
    }
}
