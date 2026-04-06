//! Embedded demo UI and admin API, served under `/_/` on the main S3 port.

use axum::{
    extract::Path,
    http::{header, StatusCode},
    middleware,
    response::{Html, IntoResponse, Response},
    routing::{delete, get, post, put},
    Router,
};
use rust_embed::Embed;
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};

use deltaglider_proxy::api::admin::{self, AdminState};

#[derive(Embed)]
#[folder = "demo/s3-browser/ui/dist"]
struct DemoAssets;

/// Build the UI + admin API router, mounted under `/_/`.
///
/// This router is merged into the main S3 router BEFORE auth middleware,
/// so admin routes handle their own authentication (session cookies).
pub fn ui_router(admin_state: Arc<AdminState>) -> Router {
    // Admin API routes that require session authentication
    let protected = Router::new()
        .route("/_/api/admin/logout", post(admin::logout))
        .route(
            "/_/api/admin/config",
            get(admin::get_config).put(admin::update_config),
        )
        .route("/_/api/admin/password", put(admin::change_password))
        .route("/_/api/admin/session", get(admin::check_session))
        .route("/_/api/admin/test-s3", post(admin::test_s3_connection))
        // Multi-backend management
        .route(
            "/_/api/admin/backends",
            get(admin::list_backends).post(admin::create_backend),
        )
        .route("/_/api/admin/backends/:name", delete(admin::delete_backend))
        // IAM user management
        .route(
            "/_/api/admin/users",
            get(admin::list_users).post(admin::create_user),
        )
        .route(
            "/_/api/admin/users/:id",
            put(admin::update_user).delete(admin::delete_user),
        )
        .route(
            "/_/api/admin/users/:id/rotate-keys",
            post(admin::rotate_user_keys),
        )
        // IAM group management
        .route(
            "/_/api/admin/groups",
            get(admin::list_groups).post(admin::create_group),
        )
        .route(
            "/_/api/admin/groups/:id",
            put(admin::update_group).delete(admin::delete_group),
        )
        .route(
            "/_/api/admin/groups/:id/members",
            post(admin::add_group_member),
        )
        .route(
            "/_/api/admin/groups/:id/members/:user_id",
            delete(admin::remove_group_member),
        )
        // IAM backup/restore
        .route(
            "/_/api/admin/backup",
            get(admin::export_backup).post(admin::import_backup),
        )
        // S3 session credentials (server-side credential storage)
        .route(
            "/_/api/admin/session/s3-credentials",
            get(admin::get_s3_session_creds)
                .put(admin::set_s3_session_creds)
                .delete(admin::clear_s3_session_creds),
        )
        // Legacy migration
        .route("/_/api/admin/migrate", post(admin::migrate_legacy))
        // Usage scanner
        .route("/_/api/admin/usage/scan", post(admin::scan_usage))
        .route("/_/api/admin/usage", get(admin::get_usage))
        .layer(middleware::from_fn_with_state(
            admin_state.clone(),
            admin::require_session,
        ))
        .with_state(admin_state.clone());

    // Grab S3 state before admin_state is moved
    let s3_state = admin_state.s3_state.clone();

    // Public admin routes (no session required)
    let public_admin = Router::new()
        .route("/_/api/admin/login", post(admin::login))
        .route("/_/api/admin/login-as", post(admin::login_as))
        .route("/_/api/admin/policies", get(admin::get_canned_policies))
        .route("/_/api/whoami", get(admin::whoami))
        // Recovery endpoint is public — the bootstrap hash may be invalid,
        // making session login impossible. Rate-limited internally.
        .route("/_/api/admin/recover-db", post(admin::recover_db))
        .with_state(admin_state.clone());

    // Health check (unauthenticated — needed for load balancer probes)
    let health_route = Router::new().route(
        "/_/health",
        get(deltaglider_proxy::api::handlers::health_check).with_state(s3_state.clone()),
    );

    // Metrics/stats under /_/ — session-protected (sensitive operational data)
    let metrics_routes = Router::new()
        .route(
            "/_/metrics",
            get(deltaglider_proxy::metrics::metrics_handler).with_state(s3_state.clone()),
        )
        .route(
            "/_/stats",
            get(deltaglider_proxy::api::handlers::get_stats).with_state(s3_state),
        )
        .layer(middleware::from_fn_with_state(
            admin_state.clone(),
            admin::require_session,
        ))
        .with_state(admin_state.clone());

    // Static UI assets
    let static_routes = Router::new()
        .route("/_/", get(index))
        .route("/_/*path", get(static_or_fallback));

    Router::new()
        .merge(protected)
        .merge(public_admin)
        .merge(health_route)
        .merge(metrics_routes)
        .merge(static_routes)
        .layer({
            // SECURITY: In production (single-port architecture), CORS is not needed
            // because the UI is served from the same origin. allow_origin(Any) would
            // enable CSRF attacks against session-cookie-authenticated admin endpoints.
            // Only enable permissive CORS when DGP_CORS_PERMISSIVE=true (dev mode).
            let permissive = std::env::var("DGP_CORS_PERMISSIVE")
                .map(|v| v == "true" || v == "1")
                .unwrap_or(false);
            if permissive {
                CorsLayer::new()
                    .allow_origin(Any)
                    .allow_methods(Any)
                    .allow_headers(Any)
            } else {
                // Same-origin requests don't need CORS headers; cross-origin
                // requests are blocked by the browser's same-origin policy.
                CorsLayer::new()
            }
        })
}

async fn index() -> impl IntoResponse {
    serve_index()
}

async fn static_or_fallback(Path(path): Path<String>) -> impl IntoResponse {
    if let Some(content) = DemoAssets::get(&path) {
        let mime = mime_guess::from_path(&path).first_or_octet_stream();
        let cache = if path.starts_with("assets/") {
            "public, max-age=31536000, immutable"
        } else {
            "no-cache"
        };
        Response::builder()
            .header(header::CONTENT_TYPE, mime.as_ref())
            .header(header::CACHE_CONTROL, cache)
            .body(axum::body::Body::from(content.data.to_vec()))
            .unwrap()
            .into_response()
    } else {
        serve_index().into_response()
    }
}

fn serve_index() -> Response {
    match DemoAssets::get("index.html") {
        Some(content) => {
            let html = String::from_utf8_lossy(&content.data);
            (
                [(header::CACHE_CONTROL, "no-cache")],
                Html(html.into_owned()),
            )
                .into_response()
        }
        None => (StatusCode::NOT_FOUND, "Demo UI not built").into_response(),
    }
}
