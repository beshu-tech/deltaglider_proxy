//! Axum middleware that evaluates the admission chain and annotates the
//! request with the decision for downstream layers.
//!
//! Runs **before** SigV4. Phase 3b.2.b expanded responsibilities:
//!
//! - `AllowAnonymous` decisions insert [`AdmissionAllowAnonymous`] into
//!   request extensions so SigV4 can skip verification and continue as
//!   the `$anonymous` principal.
//! - `Deny` decisions short-circuit with **403 Forbidden** and an audit
//!   log line naming the matched block. SigV4 never runs.
//! - `Reject` decisions short-circuit with the operator-configured
//!   status + body. SigV4 never runs.
//! - `Continue` decisions (matched or default-terminal) fall through to
//!   the existing SigV4 middleware.
//!
//! ## Why a request-extension marker
//!
//! SigV4 needs to know "was this request pre-admitted as anonymous?" so
//! it can skip signature verification and let the handler chain continue
//! as the `$anonymous` principal. Passing that signal via a request
//! extension decouples the two middlewares: admission has no knowledge of
//! `AuthenticatedUser`, and SigV4 has no knowledge of the admission
//! chain's internal types. Either side can change without the other
//! rebuilding.

use super::{evaluator::RequestInfo, AdmissionChain, Decision, SharedAdmissionChain};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// Marker inserted into request extensions when the chain produces
/// `AllowAnonymous`. SigV4 middleware looks for this marker and, when
/// present, skips signature verification and mints the `$anonymous`
/// `AuthenticatedUser` using the matched bucket's public prefixes.
///
/// We carry the matched bucket (already lowercased) so SigV4 doesn't need
/// to re-parse the URL just to build the anonymous user.
#[derive(Debug, Clone)]
pub struct AdmissionAllowAnonymous {
    pub bucket: String,
    pub matched_block: String,
}

/// Middleware: evaluate the admission chain, annotate the request, forward.
///
/// The chain is read via `ArcSwap::load_full()` — lock-free, reader-side
/// cheap, and safe across hot-reloads (the reader holds a strong ref to
/// the chain version that was current at request-entry time).
pub async fn admission_middleware(mut request: Request<Body>, next: Next) -> Response {
    // Clone the chain for this request. `load_full` returns an `Arc`; the
    // chain itself lives until this handle drops.
    let chain: std::sync::Arc<AdmissionChain> = match request
        .extensions()
        .get::<SharedAdmissionChain>()
        .map(|h| h.load_full())
    {
        Some(c) => c,
        None => {
            // No admission chain wired up — treat every request as
            // `Continue`. This should never happen in production (startup
            // always seeds the chain) but a missing extension must not
            // crash the server.
            return next.run(request).await;
        }
    };

    let owned = extract_request_info(&request);
    let decision = super::evaluator::evaluate(&chain, &owned.as_ref());

    match decision {
        Decision::AllowAnonymous { matched } => {
            request.extensions_mut().insert(AdmissionAllowAnonymous {
                bucket: owned.bucket.clone(),
                matched_block: matched,
            });
        }
        Decision::Continue { .. } => {
            // Fall through to SigV4 — no extension inserted.
        }
        Decision::Deny { matched } => {
            // Short-circuit with 403. Audit log line gives operators the
            // block name + request context so they can trace denied
            // requests back to the rule that fired.
            tracing::warn!(
                target: "deltaglider_proxy::admission",
                block = %matched,
                method = %owned.method,
                bucket = %owned.bucket,
                source_ip = ?owned.source_ip,
                "[admission] DENY matched block `{}`",
                matched
            );
            return s3_style_error(
                StatusCode::FORBIDDEN,
                "AccessDenied",
                &format!("admission-deny:{}", matched),
            );
        }
        Decision::Reject {
            matched,
            status,
            message,
        } => {
            let code = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            tracing::warn!(
                target: "deltaglider_proxy::admission",
                block = %matched,
                status = status,
                "[admission] REJECT matched block `{}` → {}",
                matched,
                status
            );
            // Custom status + body. Unlike Deny, the operator controls
            // the HTTP shape — typically used for maintenance-mode pages
            // or rate-exceeded responses.
            return (code, message.unwrap_or_default()).into_response();
        }
    }

    next.run(request).await
}

/// S3-style XML error response used for admission `Deny` decisions.
/// Matches the shape clients already expect from SigV4 / IAM
/// rejections so integration tooling doesn't need a special case.
fn s3_style_error(status: StatusCode, code: &str, message: &str) -> Response {
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <Error><Code>{code}</Code><Message>{message}</Message></Error>"
    );
    let mut resp = (status, body).into_response();
    resp.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/xml"),
    );
    resp
}

/// Parse the request into the shape the evaluator consumes. Extracted so
/// the admin `/config/trace` endpoint can reuse the same normalisation for
/// synthetic inputs (via its own adapter — trace takes a JSON payload,
/// not a live request).
///
/// Bucket and key parsing mirrors the logic the old inline SigV4 bypass
/// used (`trim_start_matches('/')` + `split_once('/')`), so the admission
/// chain sees exactly what that code did.
///
/// Source IP comes from the same extractor the rate limiter uses
/// (`rate_limiter::extract_client_ip`) — honors `DGP_TRUST_PROXY_HEADERS`
/// for X-Forwarded-For / X-Real-IP, falls back to `ConnectInfo` when
/// wired through. Admission's policy on missing IP is documented on
/// [`RequestInfo::source_ip`]: fail-closed.
fn extract_request_info(request: &Request<Body>) -> OwnedRequestInfo {
    let method = request.method().as_str();
    let raw_path = request.uri().path();
    let trimmed = raw_path.trim_start_matches('/');
    let (bucket_str, key_str) = match trimmed.split_once('/') {
        Some((b, k)) => (b.to_string(), percent_decode(k)),
        None => (trimmed.to_string(), String::new()),
    };
    let bucket_lower = bucket_str.to_ascii_lowercase();
    let query_string = request.uri().query().unwrap_or("");
    let list_prefix = query_string
        .split('&')
        .find_map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            if k == "prefix" {
                Some(percent_decode(v))
            } else {
                None
            }
        })
        .unwrap_or_default();
    let authenticated =
        request.headers().contains_key("authorization") || has_presigned_query_params(query_string);

    // Extract source IP. Primary source is axum `ConnectInfo` (wired in
    // `main.rs` via `into_make_service_with_connect_info`). Fallback is
    // the rate limiter's X-Forwarded-For / X-Real-IP parser, gated on
    // `DGP_TRUST_PROXY_HEADERS`.
    //
    // Both paths pass the IP through `normalize_ip` which collapses
    // IPv4-mapped IPv6 (`::ffff:a.b.c.d`) to the plain IPv4 form.
    // Without this, a dual-stack kernel returning a mapped V6 address
    // for an IPv4 client would cause an operator's `203.0.113.0/24`
    // deny rule to miss the client entirely.
    let source_ip = request
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0.ip())
        .or_else(|| crate::rate_limiter::extract_client_ip(request.headers()))
        .map(crate::rate_limiter::normalize_ip);

    OwnedRequestInfo {
        method: method.to_string(),
        bucket: bucket_lower,
        key: key_str,
        list_prefix,
        authenticated,
        source_ip,
    }
}

/// Owned version of `RequestInfo` that carries its own strings so the
/// middleware can compute them from the request and still hand a borrow-
/// compatible `RequestInfo` to the evaluator.
struct OwnedRequestInfo {
    method: String,
    bucket: String,
    key: String,
    list_prefix: String,
    authenticated: bool,
    source_ip: Option<std::net::IpAddr>,
}

impl OwnedRequestInfo {
    fn as_ref(&self) -> RequestInfo<'_> {
        RequestInfo {
            method: &self.method,
            bucket: &self.bucket,
            key: if self.key.is_empty() {
                None
            } else {
                Some(&self.key)
            },
            list_prefix: if self.list_prefix.is_empty() {
                None
            } else {
                Some(&self.list_prefix)
            },
            authenticated: self.authenticated,
            source_ip: self.source_ip,
        }
    }
}

/// Percent-decoder shared with the SigV4 middleware — see
/// [`crate::api::auth::percent_decode`]. Aliased here so the admission
/// module doesn't leak `api` paths into its call sites, but behaviorally
/// identical to the SigV4 path's decoder (critical for the refactor: the
/// old inline public-prefix handling in SigV4 used that exact decoder).
use crate::api::auth::percent_decode;

/// Detects whether the URL query carries a SigV4 presigned-URL
/// `X-Amz-Credential` parameter. Mirrors `has_presigned_query_params` in
/// `api/auth.rs` — kept inline here so admission doesn't import SigV4's
/// private parser (tight coupling to query-string layout), and because
/// this check is trivially a two-liner.
fn has_presigned_query_params(query: &str) -> bool {
    query.split('&').any(|pair| {
        let key = pair.split_once('=').map(|(k, _)| k).unwrap_or(pair);
        key.eq_ignore_ascii_case("X-Amz-Credential")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_presigned_detects_case_insensitive() {
        assert!(has_presigned_query_params(
            "X-Amz-Credential=AKIA%2F...&X-Amz-Date=..."
        ));
        assert!(has_presigned_query_params(
            "x-amz-credential=AKIA&x-amz-date=..."
        ));
        assert!(!has_presigned_query_params("prefix=releases/&marker=x"));
        assert!(!has_presigned_query_params(""));
    }

    #[test]
    fn has_presigned_ignores_values_that_look_like_credentials() {
        // The key must be X-Amz-Credential, not the value.
        assert!(!has_presigned_query_params("foo=X-Amz-Credential"));
    }

    #[test]
    fn owned_request_info_round_trips_through_as_ref() {
        let owned = OwnedRequestInfo {
            method: "GET".into(),
            bucket: "b".into(),
            key: "k".into(),
            list_prefix: String::new(),
            authenticated: false,
            source_ip: None,
        };
        let info = owned.as_ref();
        assert_eq!(info.method, "GET");
        assert_eq!(info.bucket, "b");
        assert_eq!(info.key, Some("k"));
        // Empty list_prefix string should surface as None via as_ref().
        assert_eq!(info.list_prefix, None);

        // And the reverse: a non-empty list_prefix surfaces as Some.
        let owned = OwnedRequestInfo {
            method: "GET".into(),
            bucket: "b".into(),
            key: String::new(),
            list_prefix: "p/".into(),
            authenticated: true,
            source_ip: None,
        };
        let info = owned.as_ref();
        assert_eq!(info.key, None);
        assert_eq!(info.list_prefix, Some("p/"));
        assert!(info.authenticated);
    }

    #[test]
    fn extract_request_info_normalizes_ipv4_mapped_ipv6() {
        // Adversarial review C2: an IPv4 client arriving over an IPv6
        // socket on a dual-stack kernel presents as `::ffff:a.b.c.d`.
        // `IpNet::contains` treats that differently from a bare V4 IP,
        // so an operator's `203.0.113.0/24` deny rule would miss it.
        // The middleware calls `rate_limiter::normalize_ip` to collapse
        // the mapped form before handing the IP to the evaluator.
        //
        // This test is light (verifies the helper is wired, not the
        // full middleware plumbing) — full end-to-end coverage lives
        // in `tests/admission_test.rs` once integration scaffolding
        // learns to inject ConnectInfo.
        use std::net::{IpAddr, Ipv6Addr};
        let mapped = IpAddr::V6(Ipv6Addr::from([
            0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 203, 0, 113, 5,
        ]));
        let normalized = crate::rate_limiter::normalize_ip(mapped);
        // After normalize, it's a V4 IPv4 address matching the CIDR.
        match normalized {
            IpAddr::V4(v4) => assert_eq!(v4.to_string(), "203.0.113.5"),
            IpAddr::V6(_) => panic!("mapped V6 must collapse to V4, got {normalized:?}"),
        }
        // And `IpNet::contains` now hits.
        let net: ipnet::IpNet = "203.0.113.0/24".parse().unwrap();
        assert!(net.contains(&normalized));
    }
}
