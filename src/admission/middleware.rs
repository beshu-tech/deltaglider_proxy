//! Axum middleware that evaluates the admission chain and annotates the
//! request with the decision for downstream layers.
//!
//! Runs **before** SigV4. Today its only observable effect is inserting
//! [`AdmissionDecisionMarker`] into request extensions; a future variant
//! will return `403`/`429` responses directly when the chain produces a
//! `Deny`/`RateLimit` decision. Keeping that logic in the middleware
//! (rather than in SigV4) is what lets the later actions short-circuit
//! SigV4 entirely.
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
use axum::http::Request;
use axum::middleware::Next;
use axum::response::Response;

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
    }

    next.run(request).await
}

/// Parse the request into the shape the evaluator consumes. Extracted so
/// the admin `/config/trace` endpoint can reuse the same normalisation for
/// synthetic inputs (via its own adapter — trace takes a JSON payload,
/// not a live request).
///
/// Bucket and key parsing mirrors the logic the old inline SigV4 bypass
/// used (`trim_start_matches('/')` + `split_once('/')`), so the admission
/// chain sees exactly what that code did.
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

    OwnedRequestInfo {
        method: method.to_string(),
        bucket: bucket_lower,
        key: key_str,
        list_prefix,
        authenticated,
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
        };
        let info = owned.as_ref();
        assert_eq!(info.key, None);
        assert_eq!(info.list_prefix, Some("p/"));
        assert!(info.authenticated);
    }
}
