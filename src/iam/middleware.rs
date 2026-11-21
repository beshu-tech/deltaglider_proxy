// SPDX-License-Identifier: BUSL-1.1

//! Authorization middleware for axum — checks IAM permissions on each S3 request.

use axum::body::Body;
use axum::http::Request;
use axum::middleware::Next;
use axum::response::Response;
use iam_rs::Context;
use tracing::debug;

use super::types::{ListScope, S3Action};

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

/// Request-extension record: the authorization middleware denied this
/// request. The s3s access hook answers it once s3s has verified the
/// signature; no layer before that shows the verdict.
#[derive(Debug, Clone)]
pub struct AuthzDenied {
    pub user: String,
    pub action: S3Action,
    pub bucket: String,
    pub key: String,
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
    // POST /{bucket}?delete is a batch DELETE, not a write. Only on a bucket
    // path: s3s ignores `delete` on an object path, where the POST is a
    // write (CompleteMultipartUpload, CreateMultipartUpload).
    let batch_delete = target.is_batch_delete(method);
    if batch_delete {
        action = S3Action::Delete;
    }
    AuthzTarget {
        action,
        bucket,
        key,
        batch_delete,
    }
}

/// Axum middleware that checks IAM permissions after SigV4 authentication.
///
/// If an `AuthenticatedUser` is present in request extensions (inserted by
/// the SigV4 middleware in IAM mode), evaluates their permissions against
/// the requested action and resource. A denial is recorded as `AuthzDenied`
/// and answered by the s3s access hook after the signature is verified.
///
/// It runs before s3s verifies the signature, so it NEVER answers a request
/// itself (guard `the_middleware_never_answers_a_request_itself`).
///
/// In legacy mode or open access, no `AuthenticatedUser` is present and
/// the request passes through unchecked.
pub async fn authorization_middleware(mut request: Request<Body>, next: Next) -> Response {
    // OPTIONS (CORS preflight) always passes through without auth
    if request.method() == axum::http::Method::OPTIONS {
        return next.run(request).await;
    }

    // Only enforce if an AuthenticatedUser was inserted by SigV4 middleware
    let user = match super::request_principal(request.extensions()) {
        // An `Arc` clone: the principal is shared, never copied.
        Some(u) => u.clone(),
        None => return next.run(request).await,
    };

    let method = request.method().clone();
    // Authorize the resource s3s will SERVE (decoded path and query). An
    // undecodable path is recorded as a denial: s3s refuses it too.
    let target = match crate::api::request_target::RequestTarget::from_uri(request.uri()) {
        Ok(target) => target,
        Err(_) => {
            let action = classify_action(&method, request.uri().path());
            request.extensions_mut().insert(AuthzDenied {
                user: user.name.clone(),
                action,
                bucket: String::new(),
                key: String::new(),
            });
            return next.run(request).await;
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
        return next.run(request).await;
    }

    // Build IAM evaluation context from request. `base_context` holds the
    // request-wide keys; the per-key LIST filter uses it as is, because the
    // LIST keys below (`s3:prefix`, ...) describe the request, not each key.
    let mut base_context = Context::new();
    // aws:SourceIp — the TCP peer, or the XFF client when the peer is in
    // `DGP_TRUSTED_PROXY_CIDRS` (`extract_client_ip_with_peer`). Without a CIDR
    // list the XFF header is client-written, and a forged one would satisfy
    // an IP condition. Always set (peer fallback): a `null` value makes
    // `iam-rs` skip the condition silently.
    let peer_ip = request
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0.ip());
    super::permissions::insert_source_ip(
        &mut base_context,
        crate::rate_limiter::extract_client_ip_with_peer(request.headers(), peer_ip),
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
                user.permissions(),
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
                    user: user.clone(),
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
                    user: user.clone(),
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
        // Not answered here: an unverified caller must not learn a verdict.
        // The s3s access hook refuses once s3s verified the signature.
        debug!(
            "IAM denied: user='{}' action={:?} bucket='{}' key='{}'",
            user.name, action, bucket, key
        );
        request.extensions_mut().insert(AuthzDenied {
            user: user.name.clone(),
            action,
            bucket: bucket.to_string(),
            key: key.to_string(),
        });
        return next.run(request).await;
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

    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The places where `authorization_middleware` returns something other
    /// than `next.run(..).await`: a `return`, the last expression, or a `?`.
    fn own_answers(file: &str) -> Vec<String> {
        use syn::spanned::Spanned;
        use syn::visit::Visit;

        fn passes_on(e: &syn::Expr) -> bool {
            let syn::Expr::Await(a) = e else { return false };
            let syn::Expr::MethodCall(m) = &*a.base else {
                return false;
            };
            m.method == "run"
                && matches!(&*m.receiver, syn::Expr::Path(p) if p.path.is_ident("next"))
        }

        struct Scan(Vec<String>);
        impl<'ast> Visit<'ast> for Scan {
            fn visit_expr_return(&mut self, r: &'ast syn::ExprReturn) {
                if !r.expr.as_deref().is_some_and(passes_on) {
                    self.0
                        .push(format!("line {}: return", r.span().start().line));
                }
                syn::visit::visit_expr_return(self, r);
            }
            fn visit_expr_try(&mut self, t: &'ast syn::ExprTry) {
                self.0.push(format!("line {}: ?", t.span().start().line));
            }
        }

        let parsed = syn::parse_file(file).expect("the file parses");
        let func = parsed
            .items
            .iter()
            .find_map(|i| match i {
                syn::Item::Fn(f) if f.sig.ident == "authorization_middleware" => Some(f),
                _ => None,
            })
            .expect("the middleware is in the file");
        let mut scan = Scan(Vec::new());
        scan.visit_block(&func.block);
        match func.block.stmts.last() {
            Some(syn::Stmt::Expr(e, None)) if passes_on(e) => {}
            other => scan.0.push(format!(
                "line {}: the last expression",
                other.map_or(0, |s| s.span().start().line)
            )),
        }
        scan.0
    }

    /// Source guard: this middleware never answers a request itself. It
    /// runs before s3s verifies the signature, and any answer of its own
    /// (a verdict, a decode error) differs for a key id that exists and one
    /// that does not. It records a denial (`AuthzDenied`) and passes on; the
    /// s3s access hook answers after the signature is verified. There is no
    /// exception: OPTIONS, ListBuckets and a request with no principal also
    /// pass on.
    #[test]
    fn the_middleware_never_answers_a_request_itself() {
        let offenders = own_answers(include_str!("middleware.rs"));
        assert!(offenders.is_empty(), "{}", offenders.join("\n"));
    }

    /// The guard sees an own answer in every shape.
    #[test]
    fn the_answer_guard_catches_each_shape() {
        let wrap = |body: &str| {
            format!("async fn authorization_middleware(request: R, next: Next) -> Response {{ {body} }}")
        };
        assert!(own_answers(&wrap("next.run(request).await")).is_empty());
        assert!(own_answers(
            &wrap("if a {{ return next.run(request).await; }} next.run(request).await")
                .replace("{{", "{")
                .replace("}}", "}")
        )
        .is_empty());
        for bad in [
            "return E.into_response(); next.run(request).await",
            "if a { return Err(E); } next.run(request).await",
            "let x = f()?; next.run(request).await",
            "E.into_response()",
            "next.run(request).await; E.into_response()",
        ] {
            assert!(!own_answers(&wrap(bad)).is_empty(), "{bad}");
        }
    }

    /// s3s routes `?delete` to DeleteObjects only on a BUCKET path. On an
    /// object path it ignores the parameter: `POST /b/k?uploadId=u&delete`
    /// is CompleteMultipartUpload, a write. Classifying it as a delete let a
    /// delete-only user complete (write) an upload. Found by the
    /// middleware-vs-s3s contract test.
    #[test]
    fn delete_query_on_an_object_path_stays_a_write() {
        use crate::api::request_target::RequestTarget;
        let at = |path: &str, query: &str| {
            let t = RequestTarget::parse(path, Some(query)).unwrap();
            let a = authz_target(&axum::http::Method::POST, &t);
            (a.action, a.batch_delete)
        };
        assert_eq!(at("/b/k", "delete&uploadId=u1"), (S3Action::Write, false));
        assert_eq!(at("/b/k", "uploads&delete"), (S3Action::Write, false));
        assert_eq!(at("/b", "delete"), (S3Action::Delete, true));
        assert_eq!(at("/b", "%64elete"), (S3Action::Delete, true));
    }

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
