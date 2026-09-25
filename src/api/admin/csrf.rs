// SPDX-License-Identifier: BUSL-1.1

//! Same-origin proof for state-changing admin requests (S5, CSRF).
//!
//! The session cookie is `SameSite=Strict`, which stops cross-SITE requests.
//! It does not stop a page on a sibling subdomain (same site, other origin)
//! from POSTing with the cookie attached. Browsers mark every request with
//! `Sec-Fetch-Site` and cross-origin POSTs with `Origin`, so a request that
//! carries either header must show it came from our own origin. A request
//! with neither header is not from a browser page (curl, `config apply`):
//! a CSRF attack needs a victim browser, so those pass.

use axum::extract::Request;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// Pure decision: does this request prove it is same-origin (or not from a
/// browser at all)? `hosts` are the authorities this server answers as
/// (`Host`, plus `X-Forwarded-Host` when proxy headers are trusted).
pub fn is_same_origin_request(
    method: &Method,
    sec_fetch_site: Option<&str>,
    origin: Option<&str>,
    hosts: &[&str],
) -> bool {
    if matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) {
        return true;
    }
    if let Some(site) = sec_fetch_site {
        // `none` = user-initiated (address bar, bookmark): no page involved.
        return matches!(
            site.trim().to_ascii_lowercase().as_str(),
            "same-origin" | "none"
        );
    }
    match origin {
        None => true,
        Some(o) => {
            let Some((_, authority)) = o.trim().split_once("://") else {
                return false; // includes the opaque `null` origin
            };
            let authority = authority.trim_end_matches('/');
            hosts
                .iter()
                .any(|h| h.trim().eq_ignore_ascii_case(authority))
        }
    }
}

fn header<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name).and_then(|v| v.to_str().ok())
}

/// Axum middleware: 403 for a state-changing request that a browser marks as
/// cross-origin. Skipped in dev mode (`DGP_CORS_PERMISSIVE=true`), where the
/// UI is served from another origin on purpose.
pub async fn require_same_origin(req: Request, next: Next) -> Response {
    if crate::config::env_bool("DGP_CORS_PERMISSIVE", false) {
        return next.run(req).await;
    }
    let h = req.headers();
    let mut hosts: Vec<&str> = header(h, "host").into_iter().collect();
    if crate::rate_limiter::trust_proxy_headers() {
        if let Some(fh) = header(h, "x-forwarded-host") {
            hosts.extend(fh.split(',').map(str::trim));
        }
    }
    if is_same_origin_request(
        req.method(),
        header(h, "sec-fetch-site"),
        header(h, "origin"),
        &hosts,
    ) {
        return next.run(req).await;
    }
    (
        StatusCode::FORBIDDEN,
        axum::Json(serde_json::json!({
            "error": "cross_origin_request",
            "message": "state-changing admin requests must come from the proxy's own origin. Behind a reverse proxy, forward the original Host header (or set DGP_TRUST_PROXY_HEADERS=true so X-Forwarded-Host counts)"
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: &[&str] = &["s3.acme.example"];

    #[test]
    fn safe_methods_always_pass() {
        assert!(is_same_origin_request(
            &Method::GET,
            Some("cross-site"),
            Some("https://evil.example"),
            H
        ));
    }

    #[test]
    fn fetch_metadata_decides_when_present() {
        let post = Method::POST;
        assert!(is_same_origin_request(&post, Some("same-origin"), None, H));
        assert!(is_same_origin_request(&post, Some("none"), None, H));
        // A sibling subdomain is same-SITE: SameSite=Strict lets it through.
        assert!(!is_same_origin_request(&post, Some("same-site"), None, H));
        assert!(!is_same_origin_request(&post, Some("cross-site"), None, H));
        // Fetch metadata wins over a matching Origin.
        assert!(!is_same_origin_request(
            &Method::DELETE,
            Some("same-site"),
            Some("https://s3.acme.example"),
            H
        ));
    }

    #[test]
    fn origin_fallback_for_older_browsers() {
        let put = Method::PUT;
        assert!(is_same_origin_request(
            &put,
            None,
            Some("https://s3.acme.example"),
            H
        ));
        assert!(is_same_origin_request(
            &put,
            None,
            Some("https://S3.ACME.example/"),
            H
        ));
        assert!(!is_same_origin_request(
            &put,
            None,
            Some("https://ui.acme.example"),
            H
        ));
        assert!(!is_same_origin_request(&put, None, Some("null"), H));
        assert!(is_same_origin_request(
            &put,
            None,
            Some("http://localhost:9000"),
            &["localhost:9000"]
        ));
    }

    #[test]
    fn non_browser_clients_pass() {
        assert!(is_same_origin_request(&Method::POST, None, None, H));
    }
}
