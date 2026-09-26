// SPDX-License-Identifier: BUSL-1.1

//! Authorization middleware for axum — checks IAM permissions on each S3 request.

use axum::body::Body;
use axum::http::Request;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use iam_rs::Context;
use tracing::debug;

use super::types::{AuthenticatedUser, ListScope, S3Action};
use crate::metrics::{record_http_request_total, Metrics};

/// Map an HTTP method + DECODED path to an S3 action. Bucket-level vs
/// object-level comes from `RequestTarget::bucket_and_key`, the same split
/// the resource check uses, so the two can never disagree about a path.
fn classify_action(method: &axum::http::Method, path: &str) -> S3Action {
    let target = crate::api::request_target::RequestTarget {
        path: path.to_string(),
        query: Vec::new(),
    };
    let is_bucket_level = target.bucket_and_key().1.is_empty();

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

/// The action and resource the authorization middleware checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthzTarget<'a> {
    pub action: S3Action,
    pub bucket: &'a str,
    pub key: &'a str,
    /// `POST /{bucket}?delete`: the keys are in the body; the adapter
    /// checks each one.
    pub batch_delete: bool,
}

/// THE mapping from a decoded request to what IAM authorizes. Pure, so the
/// middleware-vs-s3s contract test checks the exact decision input.
pub fn authz_target<'a>(
    method: &axum::http::Method,
    target: &'a crate::api::request_target::RequestTarget,
) -> AuthzTarget<'a> {
    let mut action = classify_action(method, &target.path);
    let (bucket, key) = target.bucket_and_key();
    // POST /{bucket}?delete is a batch DELETE, not a write.
    let is_delete = *method == axum::http::Method::POST && target.has_query("delete");
    if is_delete {
        action = S3Action::Delete;
    }
    AuthzTarget {
        action,
        bucket,
        key,
        batch_delete: is_delete && key.is_empty(),
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
// `Err` is the early-response short-circuit axum middleware idiom; boxing an
// `http::Response` on the per-request hot path to please `result_large_err`
// (clippy ≥ 1.98) buys nothing.
#[allow(clippy::result_large_err)]
pub async fn authorization_middleware(
    mut request: Request<Body>,
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
    // Authorize the resource s3s will SERVE: its decoded path and query, not
    // the raw text (`/bucket%2Fkey` is an object read, `%70refix=` is
    // `prefix=`). A path that does not decode is refused here; s3s would
    // refuse it too.
    let target = match crate::api::request_target::RequestTarget::from_uri(request.uri()) {
        Ok(target) => target,
        Err(_) => {
            return Err(
                crate::api::S3Error::InvalidArgument("Invalid URI encoding".into()).into_response(),
            )
        }
    };
    let path = target.path.as_str();
    let AuthzTarget {
        action,
        bucket,
        key,
        batch_delete: is_batch_delete,
    } = authz_target(&method, &target);

    // ListBuckets (GET /) is filtered at the handler level, not denied outright.
    // This lets IAM users see only the buckets they have permissions on. Only
    // the real service root: `//x` has an empty bucket too, but it is not
    // ListBuckets and must not skip IAM.
    if path == "/" && action == S3Action::List {
        return Ok(next.run(request).await);
    }

    // Build IAM evaluation context from request. `base_context` holds the
    // request-wide keys; the per-key LIST filter uses it as is, because the
    // LIST keys below (`s3:prefix`, ...) describe the request, not each key.
    let mut base_context = Context::new();
    // aws:SourceIp — the TCP peer, or the XFF client when the peer is in
    // `DGP_TRUSTED_PROXY_CIDRS` (`extract_trusted_client_ip`). Without a CIDR
    // list the XFF header is client-written, and a forged one would satisfy
    // an IP condition. Always set (peer fallback): a `null` value makes
    // `iam-rs` skip the condition silently.
    let peer_ip = request
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0.ip());
    super::permissions::insert_source_ip(
        &mut base_context,
        crate::rate_limiter::extract_trusted_client_ip(request.headers(), peer_ip),
    );
    let mut context = base_context.clone();

    // s3:prefix — from query parameter on LIST requests
    if action == S3Action::List {
        // AWS IAM evaluates root bucket LIST as `s3:prefix == ""` even when
        // the client omits the `prefix` query parameter. Without this default,
        // a condition like `StringLike: { "s3:prefix": "" }` can never match
        // the common `GET /bucket?list-type=2&delimiter=/` root listing.
        context.insert(
            "s3:prefix".to_string(),
            iam_rs::ContextValue::String(String::new()),
        );
        if let Some(prefix) = target.query_value("prefix") {
            context.insert(
                "s3:prefix".to_string(),
                iam_rs::ContextValue::String(prefix.to_string()),
            );
        }
        if let Some(delimiter) = target.query_value("delimiter") {
            context.insert(
                "s3:delimiter".to_string(),
                iam_rs::ContextValue::String(delimiter.to_string()),
            );
        }
        if let Some(n) = target
            .query_value("max-keys")
            .and_then(|v| v.parse::<f64>().ok())
        {
            context.insert("s3:max-keys".to_string(), iam_rs::ContextValue::Number(n));
        }
    }

    // ListObjects (GET /bucket) — four-way evaluation with post-auth scope marker:
    //
    // 1. If an Allow covers the full requested bucket/prefix AND policies grant
    //    unrestricted Read/List on that space, the request is allowed AND
    //    marked `ListScope::Unrestricted` — no per-key filtering needed.
    //
    // 2. If an explicit Deny matches (including condition-based Deny like
    //    `s3:prefix ".*"`), the request is blocked immediately — Deny always wins.
    //
    // 3. If no Allow covers the prefix outright but the user has *any*
    //    permission referencing this bucket (e.g. `bucket/alice/*`), the
    //    request is ADMITTED but marked `ListScope::Filtered`. The handler
    //    MUST then filter returned keys by per-key permission. This closes
    //    the C1 IAM LIST bypass (previously unfiltered list leaked every key).
    //
    // 4. Anonymous users get no fallback: only public-prefix policies apply,
    //    so if iam-rs didn't match Allow at step 1, they're denied.
    //
    // The "any permission on bucket" fallback is preserved because it matches
    // AWS: a user with s3:GetObject on bucket/* can still ListBucket even
    // without an explicit s3:ListBucket statement. What's NEW in this fix is
    // that the handler must FILTER, not return everything wholesale.
    let (allowed, list_scope) = if action == S3Action::List && key.is_empty() {
        // Extract the requested prefix (may be empty).
        let requested_prefix = target.query_value("prefix").unwrap_or_default().to_string();

        if user.can_with_context(action, bucket, key, &context) {
            // Policies matched with the prefix-aware context. Decide whether
            // the coverage is unrestricted (user can see every key in the
            // prefix space) or prefix-scoped (handler must filter).
            let unrestricted = super::permissions::has_unrestricted_allow_for_bucket_prefix(
                &user.permissions,
                bucket,
                &requested_prefix,
            );
            let scope = if unrestricted {
                Some(ListScope::Unrestricted)
            } else {
                // iam-rs said yes but the policy is narrower than the
                // requested prefix (e.g. condition-based) → filter anyway.
                // Defence in depth: if we can't prove coverage is
                // unrestricted, assume it isn't.
                Some(ListScope::Filtered {
                    user: Box::new(user.clone()),
                    context: Box::new(base_context.clone()),
                })
            };
            (true, scope)
        } else if user.is_explicitly_denied(action, bucket, key, &context) {
            // An explicit Deny matched (possibly via condition) — blocked
            (false, None)
        } else if user.is_anonymous() {
            // Anonymous users must NOT use the can_see_bucket fallback —
            // it would allow unscoped LIST, leaking keys outside public prefixes.
            (false, None)
        } else if user.can_see_bucket(bucket) {
            // No explicit Allow on the prefix, but the user has SOME
            // permission on this bucket. Admit with filtering enforced.
            (
                true,
                Some(ListScope::Filtered {
                    user: Box::new(user.clone()),
                    context: Box::new(base_context.clone()),
                }),
            )
        } else {
            (false, None)
        }
    } else if is_batch_delete {
        (
            super::permissions::may_attempt_batch_delete(&user, bucket, &context),
            None,
        )
    } else {
        (user.can_with_context(action, bucket, key, &context), None)
    };

    if !allowed {
        // Not a credential failure: s3s never checks this signature.
        if let Some(outcome) = request.extensions().get::<crate::api::auth::AuthOutcome>() {
            outcome.mark_authz_denied();
        }
        debug!(
            "IAM denied: user='{}' action={:?} bucket='{}' key='{}'",
            user.name, action, bucket, key
        );
        if let Some(metrics) = request.extensions().get::<std::sync::Arc<Metrics>>() {
            record_http_request_total(
                metrics,
                method.as_str(),
                path,
                axum::http::StatusCode::FORBIDDEN,
            );
        }
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

    // Hand the ListScope marker to the LIST handler so it knows whether to
    // filter keys. Only inserted for LIST bucket-level; other actions read
    // the AuthenticatedUser directly and don't need this marker.
    if let Some(scope) = list_scope {
        request.extensions_mut().insert(scope);
    }

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
        // Same split as the resource check: `//x` is an object in bucket "",
        // `/b//k` is object `k`.
        assert_eq!(
            classify_action(&axum::http::Method::GET, "//x"),
            S3Action::Read
        );
        assert_eq!(
            classify_action(&axum::http::Method::GET, "/b//k"),
            S3Action::Read
        );
    }
}

#[cfg(test)]
mod classify_action_proptests {
    use super::classify_action;
    use crate::iam::types::S3Action;
    use axum::http::Method;
    use proptest::prelude::*;

    fn method(i: u8) -> Method {
        match i % 6 {
            0 => Method::GET,
            1 => Method::HEAD,
            2 => Method::PUT,
            3 => Method::DELETE,
            4 => Method::POST,
            _ => Method::PATCH,
        }
    }

    proptest! {
        /// Never panics on arbitrary method+path.
        #[test]
        fn never_panics(mi in any::<u8>(), path in ".{0,120}") {
            let _ = classify_action(&method(mi), &path);
        }

        /// SECURITY INVARIANT: a mutating HTTP method must NEVER classify as a
        /// read-only action (Read or List). A regression here would let a
        /// read-only IAM user perform writes/deletes.
        #[test]
        fn mutating_methods_never_map_to_read(path in ".{0,120}") {
            for m in [Method::PUT, Method::DELETE, Method::POST, Method::PATCH] {
                let a = classify_action(&m, &path);
                prop_assert!(
                    !matches!(a, S3Action::Read | S3Action::List),
                    "mutating method {m} on {path:?} mapped to read-only {a:?}"
                );
            }
        }

        /// Read methods (GET/HEAD) never escalate to a write-class action.
        #[test]
        fn read_methods_never_escalate(path in ".{0,120}") {
            for m in [Method::GET, Method::HEAD] {
                let a = classify_action(&m, &path);
                prop_assert!(
                    matches!(a, S3Action::Read | S3Action::List),
                    "read method {m} on {path:?} escalated to {a:?}"
                );
            }
        }
    }
}
