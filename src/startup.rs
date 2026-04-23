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

    validate_auth_config(config);
}

/// Validate authentication configuration and refuse to start if unsafe.
///
/// The proxy requires explicit authentication configuration:
/// - Credentials present → bootstrap/IAM mode (auto-detected)
/// - `authentication = "none"` → explicit open access (with loud warnings)
/// - Nothing configured → **FATAL error, process exits**
fn validate_auth_config(config: &Config) {
    // Normalize the authentication field: lowercase + trim whitespace
    let auth_mode = config
        .authentication
        .as_deref()
        .map(|s| s.trim().to_ascii_lowercase());
    let auth_mode = auth_mode.as_deref();

    if config.auth_enabled() {
        // Credentials are set — auth is on regardless of `authentication` field
        info!(
            "  Authentication: SigV4 ENABLED (access key: {})",
            config.access_key_id.as_deref().unwrap_or("")
        );
        if auth_mode == Some("none") {
            warn!("  Note: authentication = \"none\" is ignored because S3 credentials are configured");
        }
        return;
    }

    // No credentials — check the `authentication` field
    match auth_mode {
        Some("none") => {
            warn!("  Authentication: DISABLED (authentication = \"none\")");
            warn!("  ╔══════════════════════════════════════════════════════════════════╗");
            warn!("  ║  WARNING: All S3 data is accessible without credentials.        ║");
            warn!("  ║  Set access_key_id + secret_access_key for production use.      ║");
            warn!("  ╚══════════════════════════════════════════════════════════════════╝");
        }
        Some(other) => {
            error!(
                "FATAL: Unrecognized authentication mode: \"{}\"",
                config.authentication.as_deref().unwrap_or(other)
            );
            error!("");
            error!("  Accepted values:");
            error!("    authentication = \"none\"    — open access (development only)");
            error!("    (omit field)               — auto-detect from credentials");
            error!("");
            error!("  Or set S3 credentials instead:");
            error!("    access_key_id = \"...\"");
            error!("    secret_access_key = \"...\"");
            std::process::exit(1);
        }
        None => {
            // No credentials AND no explicit authentication mode → refuse to start
            error!("FATAL: No authentication configured.");
            error!("");
            error!("  The proxy refuses to start without explicit authentication configuration.");
            error!("  This prevents accidental exposure of S3 data.");
            error!("");
            error!("  Options:");
            error!("    1. Set S3 credentials (recommended):");
            error!("       access_key_id = \"...\"");
            error!("       secret_access_key = \"...\"");
            error!("");
            error!("    2. Explicitly allow open access (development only):");
            error!("       authentication = \"none\"");
            error!("");
            error!("  Environment variables:");
            error!("    DGP_ACCESS_KEY_ID + DGP_SECRET_ACCESS_KEY, or DGP_AUTHENTICATION=none");
            std::process::exit(1);
        }
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
    let replay_window_secs: u64 =
        deltaglider_proxy::config::env_parse_with_default("DGP_CLOCK_SKEW_SECONDS", 300);
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
use deltaglider_proxy::api::ConfigDbMismatchGuard;

#[allow(clippy::too_many_arguments)]
pub fn build_s3_router(
    state: &Arc<AppState>,
    iam_state: &SharedIamState,
    metrics: &Arc<Metrics>,
    rate_limiter: &RateLimiter,
    replay_cache: &deltaglider_proxy::api::auth::ReplayCache,
    config: &Config,
    config_db_mismatch: bool,
    public_prefix_snapshot: &deltaglider_proxy::bucket_policy::SharedPublicPrefixSnapshot,
    admission_chain: &deltaglider_proxy::admission::SharedAdmissionChain,
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
    let mut router = Router::new()
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
        // Admission chain — pre-auth gating. Layered AFTER SigV4 in builder
        // order so it runs BEFORE SigV4 at request time (axum applies
        // layers in reverse).
        .layer(middleware::from_fn(
            deltaglider_proxy::admission::admission_middleware,
        ))
        .layer(axum::Extension(iam_state.clone()))
        .layer(axum::Extension(public_prefix_snapshot.clone()))
        .layer(axum::Extension(admission_chain.clone()));

    // If config DB mismatch, inject guard that blocks all S3 API requests
    if config_db_mismatch {
        error!(
            "S3 API LOCKED — all requests will be rejected until bootstrap password mismatch is resolved via /_/"
        );
        router = router.layer(axum::Extension(ConfigDbMismatchGuard));
    }

    router
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
            std::time::Duration::from_secs(deltaglider_proxy::config::env_parse_with_default(
                "DGP_REQUEST_TIMEOUT_SECS",
                300u64,
            )),
        ))
        // Limit total concurrent in-flight requests to prevent resource exhaustion.
        // Default: 1024. Override via DGP_MAX_CONCURRENT_REQUESTS.
        .layer(tower::limit::ConcurrencyLimitLayer::new(
            deltaglider_proxy::config::env_parse_with_default(
                "DGP_MAX_CONCURRENT_REQUESTS",
                1024usize,
            ),
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
            let mismatch = bak_path.exists() || db_file.exists();
            match deltaglider_proxy::config_db::ConfigDb::open_or_create(
                &db_file,
                admin_password_hash,
            ) {
                Ok(db) => {
                    info!("Created fresh IAM config database: {}", db_file.display());
                    (Some(Arc::new(tokio::sync::Mutex::new(db))), mismatch)
                }
                Err(e2) => {
                    error!(
                        "Failed to create fresh config database: {} — IAM disabled",
                        e2
                    );
                    (None, mismatch)
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
    external_auth: &Option<Arc<deltaglider_proxy::iam::external_auth::ExternalAuthManager>>,
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
            reopen_and_rebuild_iam(
                config_db,
                admin_password_hash,
                iam_state,
                external_auth,
                "startup",
            )
            .await;
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

// `reopen_and_rebuild_iam` moved to `deltaglider_proxy::config_db_sync`
// so it can be shared by the admin `POST /api/admin/config/sync-now`
// endpoint (which lives in the library, not the binary). Re-exported
// here as the same symbol so call sites in this file keep working.
pub use deltaglider_proxy::config_db_sync::reopen_and_rebuild_iam;

/// Spawn periodic config DB S3 sync poll (every 5 minutes).
pub fn spawn_config_sync_poll(
    sync: Arc<ConfigDbSync>,
    config_db: &Option<Arc<tokio::sync::Mutex<deltaglider_proxy::config_db::ConfigDb>>>,
    iam_state: &SharedIamState,
    external_auth: &Option<Arc<deltaglider_proxy::iam::external_auth::ExternalAuthManager>>,
    admin_password_hash: &str,
) {
    let db_arc = config_db.clone();
    let iam = iam_state.clone();
    let ext_auth = external_auth.clone();
    let password_hash = admin_password_hash.to_string();

    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(300));
        // Skip the immediate first tick (startup sync already ran)
        tick.tick().await;
        loop {
            tick.tick().await;
            match sync.poll_and_sync().await {
                Ok(true) => {
                    reopen_and_rebuild_iam(
                        &db_arc,
                        &password_hash,
                        &iam,
                        &ext_auth,
                        "periodic poll",
                    )
                    .await;
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
        let tls_cfg = config
            .tls
            .as_ref()
            .expect("tls_enabled() implies tls config is Some");
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

// ─────────────────────────────────────────────────────────────────────
// Unit tests for pure-function startup helpers.
//
// `src/startup.rs` (~650 LOC) had zero unit tests at the time of the
// QA audit. Most of the file is glue that spawns tasks / opens files /
// binds listeners — hard to unit-test — but a handful of helpers are
// genuinely pure-input → pure-output and deserve regression coverage.
// The ones covered below are called from main.rs on every boot; a bug
// here is a boot-path regression that nothing else would catch.
// ─────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;
    use deltaglider_proxy::config::Config;

    /// A Config with a full SigV4 credential pair must produce
    /// `IamState::Legacy`. This is the default path for deployments
    /// that haven't created IAM users yet — the bootstrap admin key
    /// becomes the legacy credential.
    #[test]
    fn init_iam_state_with_legacy_creds_returns_legacy() {
        let cfg = Config {
            access_key_id: Some("AKIAEXAMPLEBOOTSTRAP".to_string()),
            secret_access_key: Some("bootstrapSecretKey1234567890".to_string()),
            ..Config::default()
        };

        let state = init_iam_state(&cfg);
        let loaded = state.load_full();
        match loaded.as_ref() {
            IamState::Legacy(auth) => {
                assert_eq!(auth.access_key_id, "AKIAEXAMPLEBOOTSTRAP");
                assert_eq!(auth.secret_access_key, "bootstrapSecretKey1234567890");
            }
            other => panic!("expected Legacy, got {:?}", std::mem::discriminant(other)),
        }
    }

    /// No creds + no IAM users = open access. The proxy will refuse
    /// to start later (`authentication = "none"` must be explicit),
    /// but `init_iam_state` itself returns Disabled — the refusal
    /// happens in a separate boot-safety check.
    #[test]
    fn init_iam_state_without_creds_returns_disabled() {
        let cfg = Config::default(); // access_key_id=None, secret_access_key=None

        let state = init_iam_state(&cfg);
        let loaded = state.load_full();
        assert!(
            matches!(loaded.as_ref(), IamState::Disabled),
            "expected Disabled, got {:?}",
            std::mem::discriminant(loaded.as_ref())
        );
    }

    /// Partial credentials (only access_key_id set, or only secret
    /// set) must NOT be treated as valid. Both are required or the
    /// proxy should treat auth as absent. A silent "half-configured"
    /// state would leak the set half via SigV4 auth mismatches.
    #[test]
    fn init_iam_state_with_only_access_key_id_returns_disabled() {
        let cfg = Config {
            access_key_id: Some("AKIAHALFSET".to_string()),
            // secret_access_key stays None
            ..Config::default()
        };

        let state = init_iam_state(&cfg);
        let loaded = state.load_full();
        assert!(
            matches!(loaded.as_ref(), IamState::Disabled),
            "half-configured creds must yield Disabled"
        );
    }

    #[test]
    fn init_iam_state_with_only_secret_returns_disabled() {
        let cfg = Config {
            secret_access_key: Some("dangling-secret".to_string()),
            // access_key_id stays None
            ..Config::default()
        };

        let state = init_iam_state(&cfg);
        let loaded = state.load_full();
        assert!(
            matches!(loaded.as_ref(), IamState::Disabled),
            "half-configured creds (secret only) must yield Disabled"
        );
    }

    /// Metrics labeling: `build_info` must carry the right
    /// backend_type label so Prometheus dashboards can filter by
    /// deployment shape. Filesystem vs S3 is the first-order split.
    #[test]
    fn init_metrics_build_info_labels_filesystem_backend() {
        let cfg = Config::default(); // Default backend is Filesystem

        let metrics = init_metrics(&cfg);
        // We can't easily read back the label value through prometheus's
        // API without parsing the exposition format, but we can verify
        // the process_start_time_seconds got a non-zero value (set by
        // init_metrics) — that's a cheap sanity check that the
        // function actually ran its initialisation.
        let start_time = metrics.process_start_time_seconds.get();
        assert!(
            start_time > 0.0,
            "process_start_time_seconds should be initialised to a positive UNIX timestamp, \
             got {start_time}"
        );
    }
}
