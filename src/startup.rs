//! Server startup helpers — extracted from main.rs for file size.

use axum::{extract::DefaultBodyLimit, middleware, routing::get, Router};

use deltaglider_proxy::api::auth::sigv4_auth_middleware;
use deltaglider_proxy::api::handlers::{
    bucket_get_handler, create_bucket, delete_bucket, delete_object, delete_objects, get_object,
    head_bucket, head_object, head_root, list_buckets, post_object, put_object_or_copy, AppState,
};
use deltaglider_proxy::config::{BackendConfig, Config};
use deltaglider_proxy::config_db_sync::ConfigDbSync;
use deltaglider_proxy::iam::authorization_middleware;
use deltaglider_proxy::iam::{AuthConfig, IamState, SharedIamState};
use deltaglider_proxy::metrics::Metrics;
use deltaglider_proxy::rate_limiter::RateLimiter;
use std::io::IsTerminal;
use std::sync::Arc;
use std::time::Duration;
use tokio::signal;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::{layer::SubscriberExt, reload, util::SubscriberInitExt};

use crate::Cli;

// ---------------------------------------------------------------------------
// Extracted helpers
// ---------------------------------------------------------------------------

/// Re-export for binary crate convenience.
pub use deltaglider_proxy::config_db::config_db_path;

/// Initialize tracing with reload support.
/// Priority: RUST_LOG > DGP_LOG_LEVEL > --verbose > default.
pub fn init_tracing(cli: &Cli) -> reload::Handle<EnvFilter, tracing_subscriber::Registry> {
    let initial_filter = EnvFilter::try_from_default_env()
        .or_else(|_| std::env::var("DGP_LOG_LEVEL").map(EnvFilter::new))
        .unwrap_or_else(|_| {
            if cli.verbose {
                EnvFilter::new("deltaglider_proxy=trace,tower_http=trace")
            } else {
                EnvFilter::new("deltaglider_proxy=debug,tower_http=debug")
            }
        });

    let (filter_layer, reload_handle) = reload::Layer::new(initial_filter);
    tracing_subscriber::registry()
        .with(filter_layer)
        .with(tracing_subscriber::fmt::layer().with_ansi(std::io::stdout().is_terminal()))
        .init();

    reload_handle
}

/// Log the startup banner with config summary.
pub fn log_startup_banner(config: &Config) {
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
    if config.metadata_cache_mb == 0 {
        warn!("[cache] In-memory metadata cache is DISABLED (0 MB). Every HEAD/LIST will query storage.");
    } else {
        info!(
            "[cache] In-memory metadata cache: {} MB (object metadata for HEAD/LIST acceleration)",
            config.metadata_cache_mb
        );
    }
    if config.cache_size_mb == 0 {
        warn!("[cache] In-memory reference cache is DISABLED (0 MB). Every delta GET will read the full reference from storage.");
    } else if config.cache_size_mb < 1024 {
        warn!(
            "[cache] In-memory reference cache is only {} MB — recommend ≥1024 MB for production. Set cache_size_mb or DGP_CACHE_MB.",
            config.cache_size_mb
        );
    } else {
        info!(
            "[cache] In-memory reference cache: {} MB (delta reconstruction baselines)",
            config.cache_size_mb
        );
    }

    if config.auth_enabled() {
        info!(
            "  Authentication: SigV4 ENABLED (access key: {})",
            config.access_key_id.as_deref().unwrap_or("")
        );
    } else {
        warn!("  Authentication: DISABLED (open access) — set DGP_ACCESS_KEY_ID and DGP_SECRET_ACCESS_KEY to enable");
    }
}

/// Create Prometheus metrics and set initial gauges.
pub fn init_metrics(config: &Config) -> Arc<Metrics> {
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
    metrics
}

/// Create the replay-attack detection cache and spawn its periodic cleanup.
pub fn init_replay_cache() -> deltaglider_proxy::api::auth::ReplayCache {
    let replay_cache: deltaglider_proxy::api::auth::ReplayCache = Arc::new(dashmap::DashMap::new());
    // Cleanup cutoff must match the replay detection window (DGP_CLOCK_SKEW_SECONDS,
    // default 300s). Using a shorter cutoff would evict entries while they're still
    // within the valid clock-skew window, allowing replayed requests to succeed.
    let replay_window_secs: u64 = std::env::var("DGP_CLOCK_SKEW_SECONDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300);
    spawn_periodic(Duration::from_secs(60), {
        let cache = replay_cache.clone();
        move || {
            let cutoff = std::time::Instant::now() - Duration::from_secs(replay_window_secs);
            cache.retain(|_, instant: &mut std::time::Instant| *instant > cutoff);
        }
    });
    replay_cache
}

/// Spawn periodic cache health monitor (utilization + miss rate, every 60s).
pub fn spawn_cache_monitor(state: &Arc<AppState>, metrics: &Arc<Metrics>) {
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
                    "[cache] In-memory reference cache utilization {:.0}% ({}/{} MB, {} entries) — consider increasing cache_size_mb",
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
                    "[cache] In-memory reference cache miss rate {:.0}% ({}/{} in last 60s) — active deltaspaces may exceed cache capacity",
                    miss_pct, interval_misses, interval_total
                );
            }
        }
    });
}

/// Build IAM state from config (legacy single-credential or disabled).
pub fn init_iam_state(config: &Config) -> SharedIamState {
    Arc::new(arc_swap::ArcSwap::from_pointee(
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
    ))
}

/// Build the S3-compatible router with all routes and middleware layers.
pub fn build_s3_router(
    state: &Arc<AppState>,
    iam_state: &SharedIamState,
    metrics: &Arc<Metrics>,
    rate_limiter: &RateLimiter,
    replay_cache: &deltaglider_proxy::api::auth::ReplayCache,
    config: &Config,
) -> Router {
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
    Router::new()
        // Health and stats are under /_/ (see demo.rs) — not on the S3 router
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
        // Replay attack detection cache for SigV4
        .layer(axum::Extension(replay_cache.clone()))
        // Rate limiter extension for auth middleware
        .layer(axum::Extension(rate_limiter.clone()))
        // Metrics extension for auth middleware to extract
        .layer(axum::Extension(metrics.clone()))
        // Increase body size limit to match max_object_size config (default 2MB is too small)
        .layer(DefaultBodyLimit::max(config.max_object_size as usize))
        // Per-request timeout: prevents slow clients from holding concurrency slots forever.
        // Default: 300s. Override via DGP_REQUEST_TIMEOUT_SECS.
        // Returns HTTP 504 Gateway Timeout (appropriate for a proxy).
        .layer(tower_http::timeout::TimeoutLayer::with_status_code(
            axum::http::StatusCode::GATEWAY_TIMEOUT,
            std::time::Duration::from_secs(
                std::env::var("DGP_REQUEST_TIMEOUT_SECS")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(300u64),
            ),
        ))
        // Limit total concurrent in-flight requests to prevent resource exhaustion.
        // Default: 1024. Override via DGP_MAX_CONCURRENT_REQUESTS.
        .layer(tower::limit::ConcurrencyLimitLayer::new(
            std::env::var("DGP_MAX_CONCURRENT_REQUESTS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1024usize),
        ))
        // CORS must be outermost to handle OPTIONS preflight before auth
        .layer(CorsLayer::permissive())
        .with_state(state.clone())
}

/// Initialize the encrypted IAM config database. If it contains existing users,
/// switch to IAM mode immediately.
///
/// Returns `(config_db, mismatch)` where `mismatch` is true if the bootstrap
/// password hash doesn't match the existing DB encryption key.
pub fn init_config_db(
    admin_password_hash: &str,
    iam_state: &SharedIamState,
) -> (
    Option<Arc<tokio::sync::Mutex<deltaglider_proxy::config_db::ConfigDb>>>,
    bool,
) {
    let db_file = config_db_path();
    match deltaglider_proxy::config_db::ConfigDb::open_or_create(&db_file, admin_password_hash) {
        Ok(db) => {
            // If DB has existing users, switch to IAM mode
            if let Ok(users) = db.load_users() {
                if !users.is_empty() {
                    let groups = db.load_groups().unwrap_or_default();
                    info!(
                        "Loaded {} IAM users, {} groups from {}",
                        users.len(),
                        groups.len(),
                        db_file.display()
                    );
                    let state = deltaglider_proxy::iam::IamIndex::build_iam_state(users, groups);
                    iam_state.store(Arc::new(state));
                }
                // If no users exist, keep current IamState (Legacy or Disabled)
            }
            (Some(Arc::new(tokio::sync::Mutex::new(db))), false)
        }
        Err(e) => {
            // Preserve the existing DB as .bak instead of deleting — recovery needs it
            let bak_path = db_file.with_extension("db.bak");
            if db_file.exists() {
                if let Err(rename_err) = std::fs::rename(&db_file, &bak_path) {
                    warn!(
                        "Failed to backup config DB to {}: {}",
                        bak_path.display(),
                        rename_err
                    );
                } else {
                    error!(
                        "Bootstrap password does not match config DB — original preserved as {}. \
                         Use the admin GUI recovery wizard to resolve.",
                        bak_path.display()
                    );
                }
            } else {
                warn!(
                    "Config DB file does not exist: {} (error: {})",
                    db_file.display(),
                    e
                );
            }

            // Create a fresh DB so the proxy can start (in bootstrap/legacy mode)
            match deltaglider_proxy::config_db::ConfigDb::open_or_create(
                &db_file,
                admin_password_hash,
            ) {
                Ok(db) => {
                    info!("Created fresh IAM config database: {}", db_file.display());
                    (
                        Some(Arc::new(tokio::sync::Mutex::new(db))),
                        bak_path.exists(),
                    )
                }
                Err(e2) => {
                    error!(
                        "Failed to create fresh config database: {} — IAM disabled",
                        e2
                    );
                    (None, bak_path.exists())
                }
            }
        }
    }
}

/// Initialize config DB S3 sync if DGP_CONFIG_SYNC_BUCKET is set.
/// On startup: downloads from S3 if newer, reopens the DB, and rebuilds IAM index.
pub async fn init_config_sync(
    config: &Config,
    admin_password_hash: &str,
    config_db: &Option<Arc<tokio::sync::Mutex<deltaglider_proxy::config_db::ConfigDb>>>,
    iam_state: &SharedIamState,
) -> Option<Arc<ConfigDbSync>> {
    let sync_bucket = match &config.config_sync_bucket {
        Some(b) if !b.is_empty() => b.clone(),
        _ => {
            info!("Config DB S3 sync: disabled (set config_sync_bucket in TOML or DGP_CONFIG_SYNC_BUCKET env var)");
            return None;
        }
    };

    let db_file = config_db_path();

    let sync = match ConfigDbSync::new(
        &config.backend,
        sync_bucket.clone(),
        db_file,
        admin_password_hash.to_string(),
    )
    .await
    {
        Ok(s) => Arc::new(s),
        Err(e) => {
            warn!("Config DB S3 sync: failed to initialize: {}", e);
            return None;
        }
    };

    info!("Config DB S3 sync: enabled (bucket={})", sync_bucket);

    // Try to download a newer version from S3
    match sync.download_if_newer().await {
        Ok(true) => {
            reopen_and_rebuild_iam(config_db, admin_password_hash, iam_state, "startup").await;
        }
        Ok(false) => {
            info!("Config DB S3 sync: local copy is current");
        }
        Err(e) => {
            warn!("Config DB S3 sync: startup download failed: {}", e);
        }
    }

    Some(sync)
}

/// Reopen the config DB after an S3 download and rebuild the IAM index.
pub async fn reopen_and_rebuild_iam(
    config_db: &Option<Arc<tokio::sync::Mutex<deltaglider_proxy::config_db::ConfigDb>>>,
    admin_password_hash: &str,
    iam_state: &SharedIamState,
    context: &str,
) {
    if let Some(ref db_arc) = config_db {
        let mut db = db_arc.lock().await;
        if let Err(e) = db.reopen(admin_password_hash) {
            warn!(
                "Config DB S3 sync ({}): failed to reopen after download: {}",
                context, e
            );
        } else {
            // Rebuild IAM index from the new DB
            let users = db.load_users().unwrap_or_default();
            let groups = db.load_groups().unwrap_or_default();
            let count = users.len();
            let group_count = groups.len();
            let state = deltaglider_proxy::iam::IamIndex::build_iam_state(users, groups);
            if matches!(&state, IamState::Iam(_)) {
                info!(
                    "IAM index rebuilt from S3-synced DB ({} users, {} groups) [{}]",
                    count, group_count, context
                );
            }
            iam_state.store(Arc::new(state));
        }
    }
}

/// Spawn periodic config DB S3 sync poll (every 5 minutes).
pub fn spawn_config_sync_poll(
    sync: Arc<ConfigDbSync>,
    config_db: &Option<Arc<tokio::sync::Mutex<deltaglider_proxy::config_db::ConfigDb>>>,
    iam_state: &SharedIamState,
    admin_password_hash: &str,
) {
    let db_arc = config_db.clone();
    let iam = iam_state.clone();
    let password_hash = admin_password_hash.to_string();

    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(300));
        // Skip the immediate first tick (startup sync already ran)
        tick.tick().await;
        loop {
            tick.tick().await;
            match sync.poll_and_sync().await {
                Ok(true) => {
                    reopen_and_rebuild_iam(&db_arc, &password_hash, &iam, "periodic poll").await;
                }
                Ok(false) => {
                    tracing::debug!("Config DB S3 sync poll: no changes");
                }
                Err(e) => {
                    warn!("Config DB S3 sync poll failed: {}", e);
                }
            }
        }
    });
}

/// Build TLS config if enabled in config.
pub async fn init_tls(
    config: &Config,
) -> Result<Option<axum_server::tls_rustls::RustlsConfig>, Box<dyn std::error::Error>> {
    if config.tls_enabled() {
        let tls_cfg = config.tls.as_ref().unwrap();
        let rc = deltaglider_proxy::tls::build_rustls_config(tls_cfg).await?;
        if tls_cfg.cert_path.is_some() {
            info!("  TLS: enabled (user-provided certificate)");
        } else {
            warn!("  TLS: enabled (auto-generated self-signed certificate)");
        }
        Ok(Some(rc))
    } else {
        Ok(None)
    }
}

// ---------------------------------------------------------------------------
// Utilities
// ---------------------------------------------------------------------------

/// Spawn a background task that runs `f` every `interval`.
pub fn spawn_periodic(interval: Duration, f: impl Fn() + Send + 'static) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        loop {
            tick.tick().await;
            f();
        }
    });
}

/// Handle shutdown signals (SIGINT, SIGTERM)
pub async fn shutdown_signal() {
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
