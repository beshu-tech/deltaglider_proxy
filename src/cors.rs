// SPDX-License-Identifier: GPL-3.0-only

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
/// mirrors the admin router's `DGP_CORS_PERMISSIVE` branch in `demo.rs`).
/// `permissive = false` → restrictive `CorsLayer::new()` (no CORS headers;
/// same-origin requests work, cross-origin requests are blocked by the
/// browser's same-origin policy).
///
/// Pure: takes the already-resolved boolean so it can be unit-tested without
/// reading the environment.
pub fn cors_layer_for(permissive: bool) -> CorsLayer {
    if permissive {
        CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any)
    } else {
        CorsLayer::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permissive_allows_any_origin_methods_headers() {
        // Structural equality via Debug: CorsLayer has private fields and no
        // PartialEq impl, but it derives Debug, so the Debug representation is
        // a faithful, deterministic snapshot of its config.
        let expected = CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any);
        assert_eq!(
            format!("{:?}", cors_layer_for(true)),
            format!("{:?}", expected)
        );
    }

    #[test]
    fn restrictive_emits_no_cors_headers() {
        let expected = CorsLayer::new();
        assert_eq!(
            format!("{:?}", cors_layer_for(false)),
            format!("{:?}", expected)
        );
    }

    #[test]
    fn permissive_and_restrictive_differ() {
        // Sanity: the two branches must not produce identical layers.
        assert_ne!(
            format!("{:?}", cors_layer_for(true)),
            format!("{:?}", cors_layer_for(false))
        );
    }
}
