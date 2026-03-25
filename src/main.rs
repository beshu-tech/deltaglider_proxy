//! DeltaGlider Proxy - S3-compatible object storage with DeltaGlider deduplication

mod demo;

use arc_swap::ArcSwap;
use axum::{extract::DefaultBodyLimit, middleware, routing::get, Router};
use clap::Parser;
use deltaglider_proxy::api::admin::AdminState;
use deltaglider_proxy::api::auth::sigv4_auth_middleware;
use deltaglider_proxy::api::handlers::{
    bucket_get_handler, create_bucket, delete_bucket, delete_object, delete_objects, get_object,
    get_stats, head_bucket, head_object, head_root, health_check, list_buckets, post_object,
    put_object_or_copy, AppState,
};
use deltaglider_proxy::config::{BackendConfig, Config};
use deltaglider_proxy::deltaglider::DynEngine;
use deltaglider_proxy::iam::authorization_middleware;
use deltaglider_proxy::iam::{AuthConfig, IamState, SharedIamState};
use deltaglider_proxy::metrics::Metrics;
use deltaglider_proxy::multipart::MultipartStore;
use deltaglider_proxy::session::SessionStore;
use std::io::IsTerminal;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::signal;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::{layer::SubscriberExt, reload, util::SubscriberInitExt};

/// Version string including build timestamp for --version output
fn version_long() -> &'static str {
    // e.g. "0.1.8 (built 2026-02-23T21:40:07Z)"
    static V: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    V.get_or_init(|| {
        format!(
            "{} (built {})",
            env!("CARGO_PKG_VERSION"),
            env!("DGP_BUILD_TIME"),
        )
    })
}

/// DeltaGlider Proxy — S3-compatible proxy with transparent delta compression
#[derive(Parser, Debug)]
#[command(name = "deltaglider_proxy")]
#[command(version = version_long())]
#[command(author, about, long_about = None)]
struct Cli {
    /// Path to configuration file
    #[arg(short, long, value_name = "FILE")]
    config: Option<String>,

    /// Listen address (overrides config)
    #[arg(short, long, value_name = "ADDR")]
    listen: Option<String>,

    /// Enable verbose logging
    #[arg(short, long)]
    verbose: bool,

    /// Run interactive configuration wizard
    #[arg(long)]
    init: bool,

    /// Set admin password from stdin, then exit
    #[arg(long)]
    set_admin_password: bool,

    /// Print all DGP_* environment variables in .env format, then exit
    #[arg(long)]
    show_env: bool,

    /// Print an example TOML config with all options, then exit
    #[arg(long)]
    show_toml: bool,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    // Interactive config wizard (runs synchronously, exits before tokio runtime)
    if cli.init {
        match deltaglider_proxy::init::run_interactive_init("deltaglider_proxy.toml") {
            Ok(()) => std::process::exit(0),
            Err(e) => {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
    }

    // Set admin password from stdin (runs synchronously, exits before tokio runtime)
    if cli.set_admin_password {
        use std::io::BufRead;
        let mut line = String::new();
        std::io::stdin()
            .lock()
            .read_line(&mut line)
            .expect("Failed to read password from stdin");
        let password = line.trim_end_matches('\n').trim_end_matches('\r');
        if password.is_empty() {
            eprintln!("Error: password must not be empty");
            std::process::exit(1);
        }
        let hash = bcrypt::hash(password, bcrypt::DEFAULT_COST).expect("bcrypt hashing failed");
        let state_file = ".deltaglider_admin_hash";
        std::fs::write(state_file, &hash).expect("Failed to write admin hash file");
        eprintln!("Admin password hash written to {state_file}");
        std::process::exit(0);
    }

    // Dump env vars or example TOML and exit (no runtime needed)
    if cli.show_env {
        Config::print_env_vars();
        std::process::exit(0);
    }
    if cli.show_toml {
        Config::print_example_toml();
        std::process::exit(0);
    }

    // PERF: Config is loaded TWICE intentionally — once here (before the tokio
    // runtime exists) to read blocking_threads, and again inside async_main()
    // for the full async initialization. We cannot build the runtime with the
    // right blocking thread count unless we read the config first.
    // Do NOT remove this "redundant" config load — it gates runtime construction.
    let pre_config = if let Some(ref path) = cli.config {
        deltaglider_proxy::config::Config::from_file(path)
            .unwrap_or_else(|_| deltaglider_proxy::config::Config::load())
    } else {
        deltaglider_proxy::config::Config::load()
    };

    // PERF: Explicit runtime builder instead of `#[tokio::main]` so we can
    // configure `max_blocking_threads` from config/env (DGP_BLOCKING_THREADS).
    // The default tokio blocking pool (512 threads) is excessive for most
    // deployments and wastes memory. Do NOT replace with `#[tokio::main]`
    // unless you find another way to configure blocking threads before the
    // runtime starts.
    let mut runtime_builder = tokio::runtime::Builder::new_multi_thread();
    runtime_builder.enable_all();
    if let Some(bt) = pre_config.blocking_threads {
        runtime_builder.max_blocking_threads(bt);
    }
    let runtime = runtime_builder.build()?;

    runtime.block_on(async_main(cli))
}

async fn async_main(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing with reload support
    // Priority: RUST_LOG > DGP_LOG_LEVEL > --verbose > default
    let initial_filter = EnvFilter::try_from_default_env()
        .or_else(|_| std::env::var("DGP_LOG_LEVEL").map(EnvFilter::new))
        .unwrap_or_else(|_| {
            if cli.verbose {
                EnvFilter::new("deltaglider_proxy=trace,tower_http=trace")
            } else {
                EnvFilter::new("deltaglider_proxy=debug,tower_http=debug")
            }
        });

    let (filter_layer, log_reload_handle) = reload::Layer::new(initial_filter);
    tracing_subscriber::registry()
        .with(filter_layer)
        .with(tracing_subscriber::fmt::layer().with_ansi(std::io::stdout().is_terminal()))
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
    info!(
        "Starting DeltaGlider Proxy v{} (built {})",
        env!("CARGO_PKG_VERSION"),
        env!("DGP_BUILD_TIME"),
    );
    info!("  Listen address: {}", config.listen_addr);

    match &config.backend {
        BackendConfig::Filesystem { path } => {
            info!("  Backend: Filesystem");
            info!("  Data directory: {:?}", path);
        }
        BackendConfig::S3 {
            endpoint, region, ..
        } => {
            info!("  Backend: S3");
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
    if config.cache_size_mb == 0 {
        warn!("[cache] Reference cache is DISABLED (0 MB). Every delta GET will read the full reference from storage.");
    } else if config.cache_size_mb < 1024 {
        warn!(
            "[cache] Reference cache is only {} MB — recommend ≥1024 MB for production. Set cache_size_mb or DGP_CACHE_MB.",
            config.cache_size_mb
        );
    } else {
        info!("[cache] Reference cache: {} MB", config.cache_size_mb);
    }

    if config.auth_enabled() {
        info!(
            "  Authentication: SigV4 ENABLED (access key: {})",
            config.access_key_id.as_deref().unwrap_or("")
        );
    } else {
        warn!("  Authentication: DISABLED (open access) — set DGP_ACCESS_KEY_ID and DGP_SECRET_ACCESS_KEY to enable");
    }

    // Create Prometheus metrics
    let metrics = Arc::new(Metrics::new());
    metrics.process_start_time_seconds.set(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64(),
    );
    let backend_type = match &config.backend {
        BackendConfig::Filesystem { .. } => "filesystem",
        BackendConfig::S3 { .. } => "s3",
    };
    metrics
        .build_info
        .with_label_values(&[env!("CARGO_PKG_VERSION"), backend_type])
        .set(1.0);

    // Create engine (async initialization with dynamic backend)
    let engine = DynEngine::new(&config, Some(metrics.clone())).await?;
    if engine.is_cli_available() {
        info!("  xdelta3 CLI: available (legacy delta interop enabled)");
    } else {
        return Err("xdelta3 CLI not found. Install xdelta3 before starting the proxy.".into());
    }

    // Set constant cache_max_bytes gauge once at startup
    metrics
        .cache_max_bytes
        .set(engine.cache_max_capacity() as f64);

    let multipart = Arc::new(MultipartStore::new(config.max_object_size));

    // Spawn periodic cleanup task for expired multipart uploads
    spawn_periodic(Duration::from_secs(300), {
        let mp = multipart.clone();
        move || mp.cleanup_expired(Duration::from_secs(3600))
    });

    let state = Arc::new(AppState {
        engine: ArcSwap::from_pointee(engine),
        multipart,
        metrics: metrics.clone(),
    });

    // Spawn periodic cache health monitor (every 60s)
    {
        use std::sync::atomic::{AtomicU64, Ordering};
        let cache_max_bytes = state.engine.load().cache_max_capacity();
        let monitor_state = state.clone();
        let prev_hits = Arc::new(AtomicU64::new(metrics.cache_hits_total.get()));
        let prev_misses = Arc::new(AtomicU64::new(metrics.cache_misses_total.get()));
        let monitor_metrics = metrics.clone();

        spawn_periodic(Duration::from_secs(60), move || {
            let engine = monitor_state.engine.load();

            // Check utilization
            let used = engine.cache_weighted_size();
            if cache_max_bytes > 0 {
                let pct = (used as f64 / cache_max_bytes as f64) * 100.0;
                let entries = engine.cache_entry_count();
                let used_mb = used / (1024 * 1024);
                let max_mb = cache_max_bytes / (1024 * 1024);
                if pct > 90.0 {
                    tracing::warn!(
                        "[cache] utilization {:.0}% ({}/{} MB, {} entries) — consider increasing cache_size_mb",
                        pct, used_mb, max_mb, entries
                    );
                }
            }

            // Check miss rate over interval
            let cur_hits = monitor_metrics.cache_hits_total.get();
            let cur_misses = monitor_metrics.cache_misses_total.get();
            let prev_h = prev_hits.swap(cur_hits, Ordering::Relaxed);
            let prev_m = prev_misses.swap(cur_misses, Ordering::Relaxed);
            let interval_hits = cur_hits.saturating_sub(prev_h);
            let interval_misses = cur_misses.saturating_sub(prev_m);
            let interval_total = interval_hits + interval_misses;
            if interval_total >= 10 {
                let miss_pct = (interval_misses as f64 / interval_total as f64) * 100.0;
                if miss_pct > 50.0 {
                    tracing::warn!(
                        "[cache] miss rate {:.0}% ({}/{} in last 60s) — active deltaspaces may exceed cache capacity",
                        miss_pct, interval_misses, interval_total
                    );
                }
            }
        });
    }

    // Build IAM state as a hot-swappable ArcSwap so the admin API can
    // update credentials/users without a restart.
    // Supports legacy single-credential mode and multi-user IAM.
    let iam_state: SharedIamState = Arc::new(arc_swap::ArcSwap::from_pointee(
        if let (Some(ref key_id), Some(ref secret)) =
            (&config.access_key_id, &config.secret_access_key)
        {
            IamState::Legacy(AuthConfig {
                access_key_id: key_id.clone(),
                secret_access_key: secret.clone(),
            })
        } else {
            IamState::Disabled
        },
    ));

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
        // Health check and stats endpoints
        .route("/health", get(health_check))
        .route("/stats", get(get_stats))
        // Root: list buckets + HEAD probe for S3 client compatibility (Cyberduck, etc.)
        .route("/", get(list_buckets).head(head_root))
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
            get(bucket_get_handler)
                .put(create_bucket)
                .delete(delete_bucket)
                .head(head_bucket)
                .post(delete_objects),
        )
        // Bucket operations (with trailing slash)
        .route(
            "/:bucket/",
            get(bucket_get_handler)
                .put(create_bucket)
                .delete(delete_bucket)
                .head(head_bucket)
                .post(delete_objects),
        )
        .layer(TraceLayer::new_for_http())
        // HTTP metrics middleware (records request counts, durations, sizes)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            deltaglider_proxy::metrics::http_metrics_middleware,
        ))
        // IAM authorization (checks permissions after auth, before handlers)
        .layer(middleware::from_fn(authorization_middleware))
        // SigV4 authentication (looks up user, verifies signature)
        .layer(middleware::from_fn(sigv4_auth_middleware))
        .layer(axum::Extension(iam_state.clone()))
        // Metrics extension for auth middleware to extract
        .layer(axum::Extension(metrics.clone()))
        // Increase body size limit to match max_object_size config (default 2MB is too small)
        .layer(DefaultBodyLimit::max(config.max_object_size as usize))
        // CORS must be outermost to handle OPTIONS preflight before auth
        .layer(CorsLayer::permissive())
        .with_state(state.clone());

    // Admin GUI: ensure password hash is available, create session store
    let admin_password_hash = config.ensure_admin_password_hash();
    let session_store = Arc::new(SessionStore::new());

    // Spawn periodic cleanup for expired admin sessions (every 5 minutes)
    spawn_periodic(Duration::from_secs(300), {
        let sessions = session_store.clone();
        move || sessions.cleanup_expired()
    });

    let shared_config = config.clone().into_shared();

    let admin_state = Arc::new(AdminState {
        password_hash: parking_lot::RwLock::new(admin_password_hash),
        sessions: session_store,
        config: shared_config,
        log_reload: log_reload_handle,
        s3_state: state.clone(),
        iam_state,
    });

    // Build TLS config if enabled
    let rustls_config = if config.tls_enabled() {
        let tls_cfg = config.tls.as_ref().unwrap();
        let rc = deltaglider_proxy::tls::build_rustls_config(tls_cfg).await?;
        if tls_cfg.cert_path.is_some() {
            info!("  TLS: enabled (user-provided certificate)");
        } else {
            warn!("  TLS: enabled (auto-generated self-signed certificate)");
        }
        Some(rc)
    } else {
        None
    };

    // Start embedded demo UI on a separate port (S3 port + 1)
    let s3_port = config.listen_addr.port();
    tokio::spawn(demo::serve(s3_port, admin_state, rustls_config.clone()));

    // Start S3 server with graceful shutdown
    if let Some(rustls_config) = rustls_config {
        let handle = axum_server::Handle::new();
        let shutdown_handle = handle.clone();
        tokio::spawn(async move {
            shutdown_signal().await;
            shutdown_handle.graceful_shutdown(Some(Duration::from_secs(10)));
        });

        info!(
            "DeltaGlider Proxy listening on https://{}",
            config.listen_addr
        );
        axum_server::bind_rustls(config.listen_addr, rustls_config)
            .handle(handle)
            .serve(app.into_make_service())
            .await?;
    } else {
        let listener = TcpListener::bind(&config.listen_addr).await?;
        info!(
            "DeltaGlider Proxy listening on http://{}",
            config.listen_addr
        );
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown_signal())
            .await?;
    }

    info!("Server shutdown complete");
    Ok(())
}

/// Spawn a background task that runs `f` every `interval`.
fn spawn_periodic(interval: Duration, f: impl Fn() + Send + 'static) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        loop {
            tick.tick().await;
            f();
        }
    });
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
