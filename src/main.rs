//! DeltaGlider Proxy - S3-compatible object storage with DeltaGlider deduplication

use axum::{extract::DefaultBodyLimit, routing::get, Router};
use clap::Parser;
use std::sync::Arc;
use deltaglider_proxy::api::handlers::{
    create_bucket, delete_bucket, delete_object, delete_objects, get_object, head_bucket,
    head_object, health_check, list_buckets, list_objects, post_object, put_object_or_copy, AppState,
};
use deltaglider_proxy::config::{BackendConfig, Config};
use deltaglider_proxy::deltaglider::DynEngine;
use deltaglider_proxy::storage::FilesystemBackend;
use tokio::net::TcpListener;
use tower_http::trace::TraceLayer;
use tokio::signal;
use tracing::{info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

/// DeltaGlider Proxy - DeltaGlider compression for S3 storage
#[derive(Parser, Debug)]
#[command(name = "deltaglider_proxy")]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Path to configuration file
    #[arg(short, long, value_name = "FILE")]
    config: Option<String>,

    /// Listen address (overrides config)
    #[arg(short, long, value_name = "ADDR")]
    listen: Option<String>,

    /// Default bucket name (overrides config)
    #[arg(short, long, value_name = "BUCKET")]
    bucket: Option<String>,

    /// Enable verbose logging
    #[arg(short, long)]
    verbose: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    // Initialize tracing
    let log_level = if cli.verbose {
        "deltaglider_proxy=trace,tower_http=trace"
    } else {
        "deltaglider_proxy=debug,tower_http=debug"
    };

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| log_level.into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    // Load configuration from file if specified, otherwise use default loading
    let mut config = if let Some(ref path) = cli.config {
        Config::from_file(path)?
    } else {
        Config::load()
    };

    // CLI overrides
    if let Some(ref addr) = cli.listen {
        config.listen_addr = addr.parse()?;
    }
    if let Some(ref bucket) = cli.bucket {
        config.default_bucket = bucket.clone();
    }
    info!("Starting DeltaGlider Proxy S3 server");
    info!("  Listen address: {}", config.listen_addr);

    match &config.backend {
        BackendConfig::Filesystem { path } => {
            info!("  Backend: Filesystem");
            info!("  Data directory: {:?}", path);
        }
        BackendConfig::S3 {
            endpoint,
            bucket,
            region,
            ..
        } => {
            info!("  Backend: S3");
            info!("  Bucket: {}", bucket);
            info!("  Region: {}", region);
            if let Some(ep) = endpoint {
                info!("  Endpoint: {}", ep);
            }
        }
    }

    info!("  Max delta ratio: {}", config.max_delta_ratio);
    info!(
        "  Max object size: {} MB",
        config.max_object_size / 1024 / 1024
    );
    info!("  Cache size: {} MB", config.cache_size_mb);

    // Check for orphaned data files from interrupted writes (filesystem only)
    if let BackendConfig::Filesystem { ref path } = config.backend {
        if let Ok(backend) = FilesystemBackend::new(path.clone()).await {
            backend.warn_orphaned_files().await;
        }
    }

    // Create engine (async initialization with dynamic backend)
    let engine = DynEngine::new(&config).await?;
    let state = Arc::new(AppState {
        engine,
        default_bucket: config.default_bucket.clone(),
    });

    // Build router with S3-style paths
    // S3 API paths:
    //   GET / - list buckets
    //   PUT /{bucket} - create bucket
    //   DELETE /{bucket} - delete bucket
    //   HEAD /{bucket} - head bucket
    //   GET /{bucket}?list-type=2 - list objects
    //   POST /{bucket}?delete - delete multiple objects
    //   PUT /{bucket}/{key...} - upload object (or copy with x-amz-copy-source)
    //   GET /{bucket}/{key...} - download object
    //   HEAD /{bucket}/{key...} - get object metadata
    //   DELETE /{bucket}/{key...} - delete object
    let app = Router::new()
        // Health check endpoint
        .route("/health", get(health_check))
        // Root: list buckets
        .route("/", get(list_buckets))
        // Object operations (wildcard routes first - more specific)
        .route(
            "/:bucket/*key",
            get(get_object)
                .put(put_object_or_copy)
                .delete(delete_object)
                .head(head_object)
                .post(post_object),
        )
        // Bucket operations (without trailing slash)
        .route(
            "/:bucket",
            get(list_objects)
                .put(create_bucket)
                .delete(delete_bucket)
                .head(head_bucket)
                .post(delete_objects),
        )
        // Bucket operations (with trailing slash)
        .route(
            "/:bucket/",
            get(list_objects)
                .put(create_bucket)
                .delete(delete_bucket)
                .head(head_bucket)
                .post(delete_objects),
        )
        .layer(TraceLayer::new_for_http())
        // Increase body size limit to match max_object_size config (default 2MB is too small)
        .layer(DefaultBodyLimit::max(config.max_object_size as usize))
        .with_state(state);

    // Start server with graceful shutdown
    let listener = TcpListener::bind(&config.listen_addr).await?;
    info!("DeltaGlider Proxy listening on http://{}", config.listen_addr);

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    info!("Server shutdown complete");
    Ok(())
}

/// Handle shutdown signals (SIGINT, SIGTERM)
async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            warn!("Received Ctrl+C, initiating graceful shutdown...");
        }
        _ = terminate => {
            warn!("Received SIGTERM, initiating graceful shutdown...");
        }
    }
}
