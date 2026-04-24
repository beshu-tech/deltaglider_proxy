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
    // Phase 3c.2: split admin routes into "always-on" (config, session,
    // backup-read, usage, diagnostics) and "iam-gated" (users / groups /
    // ext-auth mutations / legacy migrate). The iam-gated subrouter
    // layers `require_not_declarative` so every IAM mutation returns
    // 403 when `access.iam_mode` is `Declarative`. Read routes on the
    // same resources are intentionally NOT gated — the GUI must still
    // be able to display DB state for diagnostics in declarative mode.
    //
    // Both subrouters share `require_session` at the outermost layer
    // so the authentication invariant still applies uniformly.

    let iam_gated = Router::new()
        // IAM user management — POST/PUT/DELETE are gated.
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
        // IAM group management — POST/PUT/DELETE are gated.
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
        // IAM backup restore — export (GET) allowed; import (POST) gated.
        .route(
            "/_/api/admin/backup",
            get(admin::export_backup).post(admin::import_backup),
        )
        // Legacy migration: mutates IAM state as its whole purpose.
        .route("/_/api/admin/migrate", post(admin::migrate_legacy))
        // External auth provider management.
        .route(
            "/_/api/admin/ext-auth/providers",
            get(admin::external_auth::list_providers).post(admin::external_auth::create_provider),
        )
        .route(
            "/_/api/admin/ext-auth/providers/:id",
            put(admin::external_auth::update_provider)
                .delete(admin::external_auth::delete_provider),
        )
        .route(
            "/_/api/admin/ext-auth/providers/:id/test",
            post(admin::external_auth::test_provider),
        )
        // Group mapping rules.
        .route(
            "/_/api/admin/ext-auth/mappings",
            get(admin::external_auth::list_mappings).post(admin::external_auth::create_mapping),
        )
        .route(
            "/_/api/admin/ext-auth/mappings/:id",
            put(admin::external_auth::update_mapping).delete(admin::external_auth::delete_mapping),
        )
        .route(
            "/_/api/admin/ext-auth/mappings/preview",
            post(admin::external_auth::preview_mapping),
        )
        // External identities — read-only listing (no mutation), but
        // included here to keep all ext-auth routes colocated. The
        // gate is a no-op for GETs.
        .route(
            "/_/api/admin/ext-auth/identities",
            get(admin::external_auth::list_identities),
        )
        .route(
            "/_/api/admin/ext-auth/sync-memberships",
            post(admin::external_auth::sync_memberships),
        )
        .layer(middleware::from_fn_with_state(
            admin_state.clone(),
            admin::require_not_declarative,
        ));

    let protected = Router::new()
        .route("/_/api/admin/logout", post(admin::logout))
        .route(
            "/_/api/admin/config",
            get(admin::get_config).put(admin::update_config),
        )
        // Document-level config operations (Phase 1 — GitOps + Copy-as-YAML)
        .route("/_/api/admin/config/export", get(admin::export_config))
        .route(
            "/_/api/admin/config/declarative-iam-export",
            get(admin::export_declarative_iam),
        )
        .route("/_/api/admin/config/defaults", get(admin::config_defaults))
        .route(
            "/_/api/admin/config/validate",
            post(admin::validate_config_doc),
        )
        .route("/_/api/admin/config/apply", post(admin::apply_config_doc))
        // Trace accepts both POST (full body) and GET (query params — for
        // bookmarkable debug URLs per §3.2 of the admin UI revamp plan).
        .route(
            "/_/api/admin/config/trace",
            post(admin::trace_config).get(admin::trace_config_get),
        )
        // Section-level config operations (Wave 1 of the admin UI revamp).
        // Gives the UI a per-section scope between the field-level PATCH
        // (too granular) and the document-level APPLY (too coarse).
        .route(
            "/_/api/admin/config/section/:name",
            get(admin::get_section).put(admin::put_section),
        )
        .route(
            "/_/api/admin/config/section/:name/validate",
            post(admin::validate_section),
        )
        .route("/_/api/admin/password", put(admin::change_password))
        .route("/_/api/admin/session", get(admin::check_session))
        .route("/_/api/admin/test-s3", post(admin::test_s3_connection))
        // Operator-triggered config-sync pull. Useful for multi-replica
        // deployments that want immediate propagation instead of
        // waiting for the 5-minute poll tick.
        .route("/_/api/admin/config/sync-now", post(admin::sync_now))
        // Multi-backend management
        .route(
            "/_/api/admin/backends",
            get(admin::list_backends).post(admin::create_backend),
        )
        .route("/_/api/admin/backends/:name", delete(admin::delete_backend))
        // S3 session credentials (server-side credential storage)
        .route(
            "/_/api/admin/session/s3-credentials",
            get(admin::get_s3_session_creds)
                .put(admin::set_s3_session_creds)
                .delete(admin::clear_s3_session_creds),
        )
        // Usage scanner
        .route("/_/api/admin/usage/scan", post(admin::scan_usage))
        .route("/_/api/admin/usage", get(admin::get_usage))
        // Audit log viewer — recent ring of structured audit entries.
        // Read-only; no corresponding mutation route. Session-gated
        // via the surrounding `require_session` layer. Not IAM-gated
        // (all admins see the same log so there's no per-identity
        // filtering to do at this layer).
        .route("/_/api/admin/audit", get(admin::get_audit))
        // Replication: overview + per-rule controls. Session-gated,
        // not IAM-gated (admins manage replication the same way they
        // manage other storage config).
        .route(
            "/_/api/admin/replication",
            get(admin::replication_list_rules),
        )
        .route(
            "/_/api/admin/replication/rules/:name/run-now",
            post(admin::replication_run_now),
        )
        .route(
            "/_/api/admin/replication/rules/:name/pause",
            post(admin::replication_pause),
        )
        .route(
            "/_/api/admin/replication/rules/:name/resume",
            post(admin::replication_resume),
        )
        .route(
            "/_/api/admin/replication/rules/:name/history",
            get(admin::replication_history),
        )
        .route(
            "/_/api/admin/replication/rules/:name/failures",
            get(admin::replication_failures),
        )
        // Merge the IAM-gated subrouter in; it already carries its own
        // `require_not_declarative` layer. Both subrouters share the
        // outer `require_session`.
        .merge(iam_gated)
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
        // Monotonic rebuild counter. Public by design — exposes an opaque
        // number and is consumed by integration tests + internal tooling
        // to barrier on IAM mutations without a blind `sleep(1s)`.
        .route("/_/api/admin/iam/version", get(admin::iam_version))
        .route("/_/api/whoami", get(admin::whoami))
        // OAuth flow (public — browser redirects back here)
        .route(
            "/_/api/admin/oauth/authorize/:provider",
            get(admin::external_auth::oauth_authorize),
        )
        .route(
            "/_/api/admin/oauth/callback",
            get(admin::external_auth::oauth_callback),
        )
        // Recovery endpoint is public — the bootstrap hash may be invalid,
        // making session login impossible. Rate-limited internally.
        .route("/_/api/admin/recover-db", post(admin::recover_db))
        .with_state(admin_state.clone());

    // Health check (unauthenticated — needed for load balancer probes)
    // Operational endpoints — accessible without auth for monitoring systems
    // (Prometheus scrapers, load balancers, health checks)
    let operational_routes = Router::new()
        .route(
            "/_/health",
            get(deltaglider_proxy::api::handlers::health_check).with_state(s3_state.clone()),
        )
        .route(
            "/_/metrics",
            get(deltaglider_proxy::metrics::metrics_handler).with_state(s3_state.clone()),
        );

    // Stats endpoint — session-protected (reveals per-bucket storage sizes)
    let stats_route = Router::new()
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
        .merge(operational_routes)
        .merge(stats_route)
        .merge(static_routes)
        .layer({
            // SECURITY: In production (single-port architecture), CORS is not needed
            // because the UI is served from the same origin. allow_origin(Any) would
            // enable CSRF attacks against session-cookie-authenticated admin endpoints.
            // Only enable permissive CORS when DGP_CORS_PERMISSIVE=true (dev mode).
            let permissive = deltaglider_proxy::config::env_bool("DGP_CORS_PERMISSIVE", false);
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
