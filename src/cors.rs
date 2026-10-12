// SPDX-License-Identifier: BUSL-1.1

//! CORS layer construction for the S3 + admin routers.
//!
//! Both routers share the same policy: permissive CORS only when
//! `DGP_CORS_PERMISSIVE=true` (dev mode); otherwise a restrictive
//! `CorsLayer::new()` that emits no CORS headers. In production the UI is
//! same-origin (single-port architecture), so cross-origin browser requests
//! are blocked by the browser's same-origin policy without any CORS config.
//!
//! The decision (`env_bool("DGP_CORS_PERMISSIVE", false)`) is made at the call
//! site; [`cors_layer_for`] is a pure `bool → CorsLayer` mapping so the
//! decision is unit-testable without booting axum or touching the process
//! environment.

use tower_http::cors::{Any, CorsLayer};

/// Build a [`CorsLayer`] reflecting the permissive flag.
///
/// `permissive = true` → allows any origin, method, and header (dev mode,
/// mirrors the admin router's `DGP_CORS_PERMISSIVE` branch in `demo.rs`),
/// and exposes the write-fact response headers to the calling page.
/// `permissive = false` → restrictive `CorsLayer::new()` (no CORS headers;
/// same-origin requests work, cross-origin requests are blocked by the
/// browser's same-origin policy).
///
/// Pure: takes the already-resolved boolean so it can be unit-tested without
/// reading the environment.
///
/// SECURITY: the permissive branch deliberately does NOT call
/// `.allow_credentials(true)`. With `Any` origin, tower-http will not emit
/// `Access-Control-Allow-Credentials`, so a browser never sends the
/// `dgp_session` cookie cross-origin — keeping CLAUDE.md's "never permissive
/// CORS with cookie auth" invariant. Do not add credentials here.
pub fn cors_layer_for(permissive: bool) -> CorsLayer {
    if permissive {
        CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any)
            .expose_headers(
                crate::s3_adapter_s3s::WRITE_FACT_HEADERS.map(axum::http::HeaderName::from_static),
            )
    } else {
        CorsLayer::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{header, Request};
    use axum::{routing::get, Router};
    use tower::ServiceExt; // oneshot

    /// Drive a cross-origin GET through the layer and return the response's
    /// `Access-Control-Allow-*` headers — the actual HTTP contract a browser
    /// sees, not a Debug snapshot of the builder.
    async fn cors_headers(permissive: bool) -> (Option<String>, Option<String>) {
        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(cors_layer_for(permissive));
        let req = Request::builder()
            .uri("/")
            .header(header::ORIGIN, "https://evil.example")
            .body(axum::body::Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        let h = res.headers();
        let allow_origin = h
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .map(|v| v.to_str().unwrap().to_string());
        let allow_creds = h
            .get(header::ACCESS_CONTROL_ALLOW_CREDENTIALS)
            .map(|v| v.to_str().unwrap().to_string());
        (allow_origin, allow_creds)
    }

    #[tokio::test]
    async fn permissive_emits_wildcard_origin_and_no_credentials() {
        let (origin, creds) = cors_headers(true).await;
        assert_eq!(
            origin.as_deref(),
            Some("*"),
            "permissive must allow any origin"
        );
        // The security invariant: wildcard origin with NO credentials header, so
        // the browser never sends the session cookie cross-origin.
        assert_eq!(creds, None, "must NOT allow credentials in permissive mode");
    }

    /// A cross-origin page can read the write-fact headers of a PUT.
    #[tokio::test]
    async fn permissive_exposes_the_write_fact_headers() {
        let app = Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(cors_layer_for(true));
        let req = Request::builder()
            .uri("/")
            .header(header::ORIGIN, "https://app.example")
            .body(axum::body::Body::empty())
            .unwrap();
        let res = app.oneshot(req).await.unwrap();
        let exposed = res
            .headers()
            .get(header::ACCESS_CONTROL_EXPOSE_HEADERS)
            .expect("expose header")
            .to_str()
            .unwrap()
            .to_string();
        for name in crate::s3_adapter_s3s::WRITE_FACT_HEADERS {
            assert!(exposed.contains(name), "{name} not in {exposed}");
        }
    }

    #[tokio::test]
    async fn restrictive_emits_no_allow_origin_header() {
        let (origin, creds) = cors_headers(false).await;
        assert_eq!(origin, None, "restrictive must emit no Allow-Origin header");
        assert_eq!(creds, None);
    }
}
