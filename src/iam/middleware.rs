//! Authorization middleware for axum — checks IAM permissions on each S3 request.

use axum::body::Body;
use axum::http::Request;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use iam_rs::Context;
use tracing::debug;

use super::types::{AuthenticatedUser, S3Action};

/// Map an HTTP method + path to an S3 action.
fn classify_action(method: &axum::http::Method, path: &str) -> S3Action {
    let is_bucket_level = path.trim_matches('/').split('/').count() <= 1;

    match *method {
        axum::http::Method::GET | axum::http::Method::HEAD => {
            if is_bucket_level {
                S3Action::List
            } else {
                S3Action::Read
            }
        }
        axum::http::Method::PUT => {
            if is_bucket_level {
                S3Action::Admin
            } else {
                S3Action::Write
            }
        }
        axum::http::Method::DELETE => {
            if is_bucket_level {
                S3Action::Admin
            } else {
                S3Action::Delete
            }
        }
        axum::http::Method::POST => {
            // POST is used for multipart uploads, batch delete, etc.
            // Check query string for ?delete (batch delete)
            S3Action::Write
        }
        _ => S3Action::Admin, // Unknown methods require admin permissions
    }
}

/// Extract bucket and key from the URI path (path-style: /{bucket}/{key...}).
fn parse_bucket_key(path: &str) -> (&str, &str) {
    let trimmed = path.trim_start_matches('/');
    match trimmed.split_once('/') {
        Some((bucket, key)) => (bucket, key),
        None => (trimmed, ""),
    }
}

/// Axum middleware that checks IAM permissions after SigV4 authentication.
///
/// If an `AuthenticatedUser` is present in request extensions (inserted by
/// the SigV4 middleware in IAM mode), evaluates their permissions against
/// the requested action and resource. Denies with 403 if not permitted.
///
/// In legacy mode or open access, no `AuthenticatedUser` is present and
/// the request passes through unchecked.
pub async fn authorization_middleware(
    request: Request<Body>,
    next: Next,
) -> Result<Response, Response> {
    // OPTIONS (CORS preflight) always passes through without auth
    if request.method() == axum::http::Method::OPTIONS {
        return Ok(next.run(request).await);
    }

    // Only enforce if an AuthenticatedUser was inserted by SigV4 middleware
    let user = match request.extensions().get::<AuthenticatedUser>() {
        Some(u) => u.clone(),
        None => return Ok(next.run(request).await),
    };

    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let query = request.uri().query().unwrap_or("");

    // Determine the S3 action
    let mut action = classify_action(&method, &path);

    // POST /{bucket}?delete is a batch DELETE, not a write.
    // Must check for exact "delete" query parameter, not substring
    // (otherwise ?delimiter= would also match).
    if method == axum::http::Method::POST
        && query
            .split('&')
            .any(|p| p == "delete" || p.starts_with("delete="))
    {
        action = S3Action::Delete;
    }

    let (bucket, key) = parse_bucket_key(&path);

    // ListBuckets (GET /) is filtered at the handler level, not denied outright.
    // This lets IAM users see only the buckets they have permissions on.
    if bucket.is_empty() && action == S3Action::List {
        return Ok(next.run(request).await);
    }

    // Build IAM evaluation context from request
    let mut context = Context::new();

    // s3:prefix — from query parameter on LIST requests
    if action == S3Action::List {
        if let Some(query_str) = request.uri().query() {
            for param in query_str.split('&') {
                if let Some(value) = param.strip_prefix("prefix=") {
                    let decoded = urlencoding::decode(value).unwrap_or_default();
                    context.insert(
                        "s3:prefix".to_string(),
                        iam_rs::ContextValue::String(decoded.into_owned()),
                    );
                } else if let Some(value) = param.strip_prefix("delimiter=") {
                    let decoded = urlencoding::decode(value).unwrap_or_default();
                    context.insert(
                        "s3:delimiter".to_string(),
                        iam_rs::ContextValue::String(decoded.into_owned()),
                    );
                } else if let Some(value) = param.strip_prefix("max-keys=") {
                    if let Ok(n) = value.parse::<f64>() {
                        context.insert("s3:max-keys".to_string(), iam_rs::ContextValue::Number(n));
                    }
                }
            }
        }
    }

    // aws:SourceIp — from proxy headers (only when DGP_TRUST_PROXY_HEADERS=true)
    // Uses the same trust check as rate_limiter to prevent IP spoofing.
    if let Some(ip) = crate::rate_limiter::extract_client_ip(request.headers()) {
        context.insert(
            "aws:SourceIp".to_string(),
            iam_rs::ContextValue::String(ip.to_string()),
        );
    }

    // ListObjects (GET /bucket) — three-way evaluation:
    //
    // 1. If an Allow rule matches (possibly via trailing-slash alt-path in evaluate_iam),
    //    the request is allowed. This handles `"resources": ["bucket/*"]` matching bucket-level LIST.
    //
    // 2. If an explicit Deny matches (including condition-based Deny like `s3:prefix ".*"`),
    //    the request is blocked immediately — Deny always wins.
    //
    // 3. If neither Allow nor Deny matched (implicit deny from no matching rules),
    //    fall back to bucket visibility: allow LIST if the user has ANY permission
    //    that references this bucket. This ensures users with prefix-scoped permissions
    //    (e.g. `"bucket/myprefix/*"`) can still LIST the bucket to discover their objects.
    //
    // This matches AWS behaviour where a user with s3:GetObject on bucket/* can
    // ListBucket even without an explicit s3:ListBucket statement.
    let allowed = if action == S3Action::List && key.is_empty() {
        if user.can_with_context(action, bucket, key, &context) {
            true
        } else if user.is_explicitly_denied(action, bucket, key, &context) {
            // An explicit Deny matched (possibly via condition) — blocked
            false
        } else if user.name == "$anonymous" {
            // Anonymous users must NOT use the can_see_bucket fallback —
            // it would allow unscoped LIST, leaking keys outside public prefixes.
            false
        } else {
            // No explicit deny — fall back to bucket visibility
            user.can_see_bucket(bucket)
        }
    } else {
        user.can_with_context(action, bucket, key, &context)
    };

    if !allowed {
        debug!(
            "IAM denied: user='{}' action={:?} bucket='{}' key='{}'",
            user.name, action, bucket, key
        );
        // Audit-log every IAM denial.
        //
        // Previously this was `debug!`-only, which made runtime
        // debugging of 403s a black box — operators had to flip the
        // tracing filter to debug and replay the request. With the
        // in-memory audit ring (Wave 11), denials now show up
        // immediately in `/_/admin/diagnostics/audit` with the
        // exact resolved (action, bucket, key) the check evaluated.
        //
        // `target` carries the S3 action + bucket/key so the admin
        // GUI's filter box can find specific denials fast.
        crate::audit::audit_log(
            "access_denied",
            &user.name,
            &format!("{:?}", action),
            request.headers(),
            bucket,
            key,
        );
        // Drain up to 64KB of the request body before returning 403 so the client
        // receives a clean error response instead of "connection reset". Without this,
        // axum drops the unread body and closes the connection mid-upload, breaking
        // AWS CLI and other S3 clients that expect a proper HTTP error response.
        // 64KB is enough for S3 SDKs to read the error; larger bodies get a
        // connection reset (acceptable, and limits DoS surface).
        let _ = axum::body::to_bytes(request.into_body(), 64 * 1024).await;
        return Err(crate::api::S3Error::AccessDenied.into_response());
    }

    debug!(
        "IAM allowed: user='{}' action={:?} bucket='{}' key='{}'",
        user.name, action, bucket, key
    );

    Ok(next.run(request).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_classify_action_unknown_method_requires_admin() {
        let action = classify_action(&axum::http::Method::PATCH, "/bucket/key");
        assert_eq!(action, S3Action::Admin);
        let action = classify_action(&axum::http::Method::TRACE, "/bucket/key");
        assert_eq!(action, S3Action::Admin);
    }

    #[test]
    fn test_parse_bucket_key() {
        assert_eq!(
            parse_bucket_key("/my-bucket/key.txt"),
            ("my-bucket", "key.txt")
        );
        assert_eq!(parse_bucket_key("/my-bucket/"), ("my-bucket", ""));
        assert_eq!(parse_bucket_key("/my-bucket"), ("my-bucket", ""));
        assert_eq!(parse_bucket_key("/"), ("", ""));
    }

    #[test]
    fn test_classify_action_mapping() {
        assert_eq!(
            classify_action(&axum::http::Method::GET, "/bucket/key"),
            S3Action::Read
        );
        assert_eq!(
            classify_action(&axum::http::Method::GET, "/bucket"),
            S3Action::List
        );
        assert_eq!(
            classify_action(&axum::http::Method::GET, "/"),
            S3Action::List
        );
        assert_eq!(
            classify_action(&axum::http::Method::PUT, "/bucket/key"),
            S3Action::Write
        );
        assert_eq!(
            classify_action(&axum::http::Method::PUT, "/bucket"),
            S3Action::Admin
        );
        assert_eq!(
            classify_action(&axum::http::Method::DELETE, "/bucket/key"),
            S3Action::Delete
        );
        assert_eq!(
            classify_action(&axum::http::Method::DELETE, "/bucket"),
            S3Action::Admin
        );
        assert_eq!(
            classify_action(&axum::http::Method::POST, "/bucket/key"),
            S3Action::Write
        );
    }
}
