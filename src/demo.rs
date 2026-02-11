//! Embedded demo UI served on a dedicated port.

use axum::{
    extract::Path,
    http::{header, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use rust_embed::Embed;
use tokio::net::TcpListener;
use tower_http::cors::CorsLayer;
use tracing::info;

#[derive(Embed)]
#[folder = "demo/s3-browser/ui/dist"]
struct DemoAssets;

/// Start the demo UI server on port `s3_port + 1`.
pub async fn serve(s3_port: u16) {
    let demo_port = s3_port + 1;
    let addr = format!("0.0.0.0:{demo_port}");

    let app = Router::new()
        .route("/", get(index))
        .route("/*path", get(static_or_fallback))
        .layer(CorsLayer::permissive());

    match TcpListener::bind(&addr).await {
        Ok(listener) => {
            info!("  Demo UI: http://localhost:{demo_port}");
            axum::serve(listener, app).await.ok();
        }
        Err(e) => {
            tracing::warn!("Demo UI failed to bind {addr}: {e}");
        }
    }
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
