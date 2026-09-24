// SPDX-License-Identifier: BUSL-1.1

//! Axum middleware that evaluates the admission chain and annotates the
//! request with the decision for downstream layers.
//!
//! Runs **before** SigV4. Four decision paths:
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

use crate::metrics::{record_http_request_total, Metrics};

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
            if let Some(metrics) = request.extensions().get::<std::sync::Arc<Metrics>>() {
                record_http_request_total(
                    metrics,
                    request.method().as_str(),
                    request.uri().path(),
                    StatusCode::FORBIDDEN,
                );
            }
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
            // Route through the canonical S3 error builder so the XML shape
            // (and the `x-amz-request-id` header) match every other
            // AccessDenied the proxy emits — while keeping the matched-block
            // name in the `<Message>`. That name is a deliberate
            // operator-debugging affordance (asserted by
            // tests/admission_test.rs): a denied client can see which rule
            // fired, mirroring how SigV4/IAM denials are already traceable.
            return crate::api::errors::S3Error::AccessDeniedReason(format!(
                "Blocked by access rule '{matched}'",
            ))
            .into_response();
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

/// Parse the request into the shape the evaluator consumes. Extracted so
/// the admin `/config/trace` endpoint can reuse the same normalisation for
/// synthetic inputs (via its own adapter — trace takes a JSON payload,
/// not a live request).
///
/// Bucket, key and list prefix come from `RequestTarget`, decoded as s3s
/// decodes them, so a block matches the resource s3s will serve.
///
/// Source IP comes from the same extractor the rate limiter uses
/// (`rate_limiter::extract_client_ip`) — honors `DGP_TRUST_PROXY_HEADERS`
/// for X-Forwarded-For / X-Real-IP, falls back to `ConnectInfo` when
/// wired through. Admission's policy on missing IP is documented on
/// [`RequestInfo::source_ip`]: fail-closed.
fn extract_request_info(request: &Request<Body>) -> OwnedRequestInfo {
    let query_string = request.uri().query().unwrap_or("");
    let authenticated =
        request.headers().contains_key("authorization") || has_presigned_query_params(query_string);

    // Extract source IP. Primary source is axum `ConnectInfo` (wired in
    // `main.rs` via `into_make_service_with_connect_info`). Fallback is
    // the rate limiter's X-Forwarded-For / X-Real-IP parser, gated on
    // `DGP_TRUST_PROXY_HEADERS`. Both paths pass the IP through
    // `normalize_ip`; see `OwnedRequestInfo::from_raw` for the
    // IPv4-mapped-IPv6 rationale.
    let source_ip = request
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0.ip())
        .or_else(|| crate::rate_limiter::extract_client_ip(request.headers()));

    OwnedRequestInfo::from_raw(
        request.method().as_str(),
        request.uri().path(),
        query_string,
        authenticated,
        source_ip,
    )
}

/// Owned version of `RequestInfo` that carries its own strings so the
/// middleware can compute them from the request and still hand a borrow-
/// compatible `RequestInfo` to the evaluator.
///
/// Built via [`OwnedRequestInfo::from_raw`] — shared between the live
/// HTTP path (`extract_request_info`) and the admin `/config/trace`
/// endpoint's synthetic-request builder. Keeping one parsing entry
/// point guarantees the trace handler and the middleware agree on
/// bucket/key extraction, case folding, percent decoding, and IP
/// normalisation.
pub(crate) struct OwnedRequestInfo {
    pub(crate) method: String,
    pub(crate) bucket: String,
    pub(crate) key: String,
    pub(crate) list_prefix: String,
    pub(crate) authenticated: bool,
    pub(crate) source_ip: Option<std::net::IpAddr>,
}

impl OwnedRequestInfo {
    /// Build an `OwnedRequestInfo` from already-extracted raw inputs.
    ///
    /// - `method` — uppercased via `to_ascii_uppercase`.
    /// - `path` / `query` — decoded through `RequestTarget` exactly as s3s
    ///   decodes them (`%2F` is a separator, query names decoded, `+` is a
    ///   space); bucket lowercased. The query may carry a leading `?`.
    /// - `authenticated` — caller's responsibility to determine
    ///   (Authorization header or presigned query param for the
    ///   HTTP path; explicit body field for trace).
    /// - `source_ip` — passed through `rate_limiter::normalize_ip`
    ///   which collapses IPv4-mapped IPv6 (`::ffff:a.b.c.d`) to the
    ///   plain V4 form. Without this a dual-stack kernel returning
    ///   a mapped V6 address for an IPv4 client would cause an
    ///   operator's `203.0.113.0/24` deny rule to miss.
    pub(crate) fn from_raw(
        method: &str,
        path: &str,
        query: &str,
        authenticated: bool,
        source_ip: Option<std::net::IpAddr>,
    ) -> Self {
        // Tolerate a leading `?` on the query, matching the trace
        // endpoint's operator-convenience behavior.
        let query_trimmed = query.strip_prefix('?').unwrap_or(query);
        // Decode bucket, key and query exactly as s3s will, so a block that
        // names `prod` also matches `/pro%64/...`. A target that does not
        // decode matches no bucket: s3s refuses it with `InvalidURI`.
        let target = RequestTarget::parse(path, Some(query_trimmed)).unwrap_or(RequestTarget {
            path: String::new(),
            query: Vec::new(),
        });
        let (bucket_raw, key_raw) = target.bucket_and_key();
        let (bucket_raw, key_raw) = (bucket_raw.to_string(), key_raw.to_string());
        let list_prefix = target.query_value("prefix").unwrap_or_default().to_string();

        OwnedRequestInfo {
            method: method.to_ascii_uppercase(),
            bucket: bucket_raw.to_ascii_lowercase(),
            key: key_raw,
            list_prefix,
            authenticated,
            source_ip: source_ip.map(crate::rate_limiter::normalize_ip),
        }
    }

    pub(crate) fn as_ref(&self) -> RequestInfo<'_> {
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

use crate::api::request_target::RequestTarget;

/// Whether s3s treats the query as SigV4-presigned
/// (`RequestTarget::is_presigned_v4`: `X-Amz-Signature`, name decoded,
/// case-sensitive).
fn has_presigned_query_params(query: &str) -> bool {
    RequestTarget::parse("/", Some(query)).is_ok_and(|t| t.is_presigned_v4())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Same test as the SigV4 middleware and s3s: the `X-Amz-Signature`
    /// parameter (see `RequestTarget::is_presigned_v4`).
    #[test]
    fn has_presigned_matches_s3s() {
        assert!(has_presigned_query_params(
            "X-Amz-Credential=AKIA%2F...&X-Amz-Date=...&X-Amz-Signature=ab"
        ));
        assert!(!has_presigned_query_params("prefix=releases/&marker=x"));
        assert!(!has_presigned_query_params(""));
        assert!(!has_presigned_query_params("foo=X-Amz-Signature"));
    }

    /// Admission matches the bucket and key s3s will serve, not the raw
    /// text: a block naming `prod` must also see `/pro%64/...`.
    #[test]
    fn from_raw_decodes_bucket_key_and_prefix_like_s3s() {
        let info = OwnedRequestInfo::from_raw("get", "/pro%64/secre%74.txt", "", false, None);
        assert_eq!(
            (info.bucket.as_str(), info.key.as_str()),
            ("prod", "secret.txt")
        );
        let info = OwnedRequestInfo::from_raw("GET", "/prod%2Fk", "", false, None);
        assert_eq!((info.bucket.as_str(), info.key.as_str()), ("prod", "k"));
        let info = OwnedRequestInfo::from_raw("GET", "/prod", "?%70refix=a+b", false, None);
        assert_eq!(info.list_prefix, "a b");
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
