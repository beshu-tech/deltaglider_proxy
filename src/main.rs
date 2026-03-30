//! DeltaGlider Proxy - S3-compatible object storage with DeltaGlider deduplication

mod demo;
mod startup;
use startup::*;

use arc_swap::ArcSwap;
use axum::middleware;
use clap::Parser;
use deltaglider_proxy::api::admin::AdminState;
use deltaglider_proxy::api::handlers::{debug_headers_enabled, AppState};
use deltaglider_proxy::config::Config;
use deltaglider_proxy::deltaglider::DynEngine;
use deltaglider_proxy::multipart::MultipartStore;
use deltaglider_proxy::rate_limiter::RateLimiter;
use deltaglider_proxy::session::SessionStore;
use deltaglider_proxy::usage_scanner::UsageScanner;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tracing::{info, warn};

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

    /// Set bootstrap password from stdin, then exit.
    /// WARNING: Changing the bootstrap password invalidates the encrypted IAM database.
    #[arg(long, alias = "set-admin-password")]
    set_bootstrap_password: bool,

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

    // Set bootstrap password from stdin (runs synchronously, exits before tokio runtime)
    if cli.set_bootstrap_password {
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
        // Validate password quality
        if let Err(msg) = deltaglider_proxy::api::admin::validate_password(password) {
            eprintln!("Error: {}", msg);
            std::process::exit(1);
        }
        let hash = bcrypt::hash(password, bcrypt::DEFAULT_COST).expect("bcrypt hashing failed");
        // Write to new file, keep old file name as fallback for existing deployments
        let state_file = ".deltaglider_bootstrap_hash";
        deltaglider_proxy::config::write_bootstrap_hash_file(
            std::path::Path::new(state_file),
            &hash,
        )
        .expect("Failed to write bootstrap hash file");
        eprintln!();
        eprintln!("⚠ WARNING: If an encrypted IAM database exists, it will become");
        eprintln!("  unreadable on next restart (encrypted with the old password).");
        eprintln!("  All IAM users will be lost. The proxy will return to bootstrap mode.");
        eprintln!();
        eprintln!("Bootstrap password hash written to {state_file}");
        // Print base64-encoded version for Docker/env var use (no $ escaping needed)
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&hash);
        eprintln!();
        eprintln!("For Docker/env vars (base64, no escaping needed):");
        eprintln!("  DGP_BOOTSTRAP_PASSWORD_HASH={b64}");
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
    // --- Logging ---
    let log_reload_handle = init_tracing(&cli);

    // --- Configuration ---
    let mut config = if let Some(ref path) = cli.config {
        Config::from_file(path)?
    } else {
        Config::load()
    };
    if let Some(ref addr) = cli.listen {
        config.listen_addr = addr.parse()?;
    }
    log_startup_banner(&config);

    // --- Metrics ---
    let metrics = init_metrics(&config);

    // --- Engine ---
    let engine = DynEngine::new(&config, Some(metrics.clone())).await?;
    if engine.is_cli_available() {
        info!("  xdelta3 CLI: available (legacy delta interop enabled)");
    } else {
        return Err("xdelta3 CLI not found. Install xdelta3 before starting the proxy.".into());
    }
    metrics
        .cache_max_bytes
        .set(engine.cache_max_capacity() as f64);

    // --- Multipart uploads ---
    let multipart = Arc::new(MultipartStore::new(config.max_object_size));
    spawn_periodic(Duration::from_secs(300), {
        let mp = multipart.clone();
        move || mp.cleanup_expired(Duration::from_secs(3600))
    });

    // --- Rate limiter & replay cache ---
    let rate_limiter = RateLimiter::default_auth();
    spawn_periodic(Duration::from_secs(300), {
        let rl = rate_limiter.clone();
        move || rl.cleanup_expired()
    });
    let replay_cache = init_replay_cache();

    // --- Debug headers ---
    if debug_headers_enabled() {
        info!("  Debug headers: enabled (DGP_DEBUG_HEADERS=true)");
    }

    // --- Proxy header trust ---
    let trust_proxy_explicit = std::env::var("DGP_TRUST_PROXY_HEADERS").ok();
    let trust_proxy = trust_proxy_explicit
        .as_deref()
        .map(|v| v == "true" || v == "1")
        .unwrap_or(true); // default true — see rate_limiter::trust_proxy_headers()
    if trust_proxy {
        if trust_proxy_explicit.is_none() {
            warn!("  Proxy headers: trusted (default) — X-Forwarded-For/X-Real-IP headers are used for rate limiting and IAM conditions. If this proxy is exposed directly to the internet (no reverse proxy), clients can spoof IPs. Set DGP_TRUST_PROXY_HEADERS=false to disable, or =true to silence this warning.");
        } else {
            info!("  Proxy headers: trusted (DGP_TRUST_PROXY_HEADERS=true) — X-Forwarded-For/X-Real-IP used for rate limiting and aws:SourceIp");
        }
    } else {
        info!("  Proxy headers: untrusted (DGP_TRUST_PROXY_HEADERS=false) — rate limiting requires ConnectInfo (not yet implemented); aws:SourceIp conditions will not match");
    }

    // --- App state ---
    let state = Arc::new(AppState {
        engine: ArcSwap::from_pointee(engine),
        multipart,
        metrics: metrics.clone(),
    });

    // --- Background monitors ---
    spawn_cache_monitor(&state, &metrics);

    // --- IAM ---
    let iam_state = init_iam_state(&config);

    // --- S3 router ---
    let app = build_s3_router(
        &state,
        &iam_state,
        &metrics,
        &rate_limiter,
        &replay_cache,
        &config,
    );

    // --- Admin / sessions / config DB ---
    let admin_password_hash = config.ensure_bootstrap_password_hash();
    let session_store = Arc::new(SessionStore::new());
    spawn_periodic(Duration::from_secs(300), {
        let sessions = session_store.clone();
        move || sessions.cleanup_expired()
    });
    let shared_config = config.clone().into_shared();
    let config_db = init_config_db(&admin_password_hash, &iam_state);

    // --- Config DB S3 sync ---
    let config_sync = init_config_sync(&config, &admin_password_hash, &config_db, &iam_state).await;

    // Start periodic config DB S3 poll (every 5 minutes)
    if let Some(ref sync) = config_sync {
        spawn_config_sync_poll(sync.clone(), &config_db, &iam_state, &admin_password_hash);
    }

    let admin_state = Arc::new(AdminState {
        password_hash: parking_lot::RwLock::new(admin_password_hash),
        sessions: session_store,
        config: shared_config,
        log_reload: log_reload_handle,
        s3_state: state.clone(),
        iam_state,
        config_db,
        usage_scanner: Arc::new(UsageScanner::new()),
        rate_limiter,
        config_sync,
    });

    // --- TLS ---
    let rustls_config = init_tls(&config).await?;

    // --- Merge UI + security headers ---
    let app = demo::ui_router(admin_state).merge(app);
    info!("  Dashboard: http://{}/_/", config.listen_addr);

    let tls_enabled = config.tls_enabled();
    let app = app.layer(middleware::from_fn(
        move |request: axum::extract::Request, next: axum::middleware::Next| async move {
            let mut response = next.run(request).await;
            let headers = response.headers_mut();
            headers.insert("x-content-type-options", "nosniff".parse().unwrap());
            headers.insert("x-frame-options", "DENY".parse().unwrap());
            if tls_enabled {
                headers.insert(
                    "strict-transport-security",
                    "max-age=31536000; includeSubDomains".parse().unwrap(),
                );
            }
            response
        },
    ));

    // --- Start server ---
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
