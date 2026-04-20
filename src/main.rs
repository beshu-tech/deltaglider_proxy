//! DeltaGlider Proxy - S3-compatible object storage with DeltaGlider deduplication

mod demo;
mod startup;
use startup::*;

use arc_swap::ArcSwap;
use axum::middleware;
use clap::{Parser, Subcommand};
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
use tracing::info;

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
    #[arg(short, long, value_name = "FILE", global = true)]
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

    /// Optional subcommand. When present, the server is not started — the
    /// subcommand runs to completion and the process exits.
    #[command(subcommand)]
    command: Option<Command>,
}

/// Top-level subcommands. Grows in later phases (admission, apply, …).
#[derive(Subcommand, Debug)]
enum Command {
    /// Configuration-file tooling (migrate, schema, apply, ...).
    Config {
        #[command(subcommand)]
        action: ConfigCommand,
    },
    /// Admission-chain tooling (trace, ...).
    Admission {
        #[command(subcommand)]
        action: AdmissionCommand,
    },
}

#[derive(Subcommand, Debug)]
enum ConfigCommand {
    /// Convert a TOML (or YAML) config file to canonical YAML on stdout.
    Migrate {
        /// Input config file (TOML or YAML).
        #[arg(value_name = "INPUT")]
        input: String,
        /// Write output to a file instead of stdout.
        #[arg(long, value_name = "OUTPUT")]
        out: Option<String>,
    },
    /// Emit the JSON Schema for the canonical Config shape (for CI / YAML LSP).
    Schema {
        /// Write schema to a file instead of stdout.
        #[arg(long, value_name = "OUTPUT")]
        out: Option<String>,
    },
    /// Push a full YAML config document to a running server via the admin API.
    ///
    /// Reads the admin bootstrap password from the DGP_BOOTSTRAP_PASSWORD
    /// environment variable (NOT a CLI flag — argv would leak it via `ps`).
    Apply {
        /// YAML file to apply.
        #[arg(value_name = "FILE")]
        file: String,
        /// Server URL. Defaults to http://127.0.0.1:9000.
        #[arg(long, value_name = "URL")]
        server: Option<String>,
        /// Per-request timeout in seconds. Defaults to 30.
        #[arg(long, value_name = "SECS")]
        timeout: Option<u64>,
    },
}

#[derive(Subcommand, Debug)]
enum AdmissionCommand {
    /// Dry-run a synthetic request through the admission chain.
    ///
    /// Emits the admission decision as JSON on stdout. Requires the admin
    /// bootstrap password in `DGP_BOOTSTRAP_PASSWORD`.
    Trace {
        /// HTTP method (GET, HEAD, PUT, POST, DELETE, …).
        #[arg(long, value_name = "METHOD")]
        method: String,
        /// Request path, e.g. `/my-bucket/releases/v1.zip`.
        #[arg(long, value_name = "PATH")]
        path: String,
        /// Treat the synthetic request as SigV4-signed.
        #[arg(long)]
        authenticated: bool,
        /// Optional query string (e.g. `prefix=releases/`).
        #[arg(long, value_name = "QUERY")]
        query: Option<String>,
        /// Server URL. Defaults to http://127.0.0.1:9000.
        #[arg(long, value_name = "URL")]
        server: Option<String>,
        /// Per-request timeout in seconds. Defaults to 30.
        #[arg(long, value_name = "SECS")]
        timeout: Option<u64>,
    },
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    // Subcommand dispatch (runs synchronously, exits before tokio runtime).
    if let Some(ref cmd) = cli.command {
        use deltaglider_proxy::cli::config::{
            admission_trace, apply, migrate, schema, AdminClientOpts, TraceArgs,
        };
        let code = match cmd {
            Command::Config { action } => match action {
                ConfigCommand::Migrate { input, out } => migrate(input, out.as_deref()),
                ConfigCommand::Schema { out } => schema(out.as_deref()),
                ConfigCommand::Apply {
                    file,
                    server,
                    timeout,
                } => {
                    let mut opts = AdminClientOpts::default();
                    if let Some(s) = server.as_deref() {
                        opts.server = s.to_string();
                    }
                    if let Some(t) = timeout {
                        opts.timeout_secs = *t;
                    }
                    apply(file, opts)
                }
            },
            Command::Admission { action } => match action {
                AdmissionCommand::Trace {
                    method,
                    path,
                    authenticated,
                    query,
                    server,
                    timeout,
                } => {
                    let mut opts = AdminClientOpts::default();
                    if let Some(s) = server.as_deref() {
                        opts.server = s.to_string();
                    }
                    if let Some(t) = timeout {
                        opts.timeout_secs = *t;
                    }
                    admission_trace(
                        TraceArgs {
                            method: method.clone(),
                            path: path.clone(),
                            authenticated: *authenticated,
                            query: query.clone(),
                        },
                        opts,
                    )
                }
            },
        };
        std::process::exit(code);
    }

    // Interactive config wizard (runs synchronously, exits before tokio runtime)
    if cli.init {
        match deltaglider_proxy::init::run_interactive_init(
            deltaglider_proxy::config::DEFAULT_CONFIG_FILENAME,
        ) {
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
        .unwrap_or(false); // default false — secure-by-default, see rate_limiter::trust_proxy_headers()
    if trust_proxy {
        info!("  Proxy headers: trusted (DGP_TRUST_PROXY_HEADERS=true) — X-Forwarded-For/X-Real-IP used for rate limiting and aws:SourceIp");
    } else if trust_proxy_explicit.is_none() {
        info!("  Proxy headers: untrusted (default) — set DGP_TRUST_PROXY_HEADERS=true if behind a reverse proxy (nginx, Caddy, ALB) to enable IP-based rate limiting and aws:SourceIp IAM conditions");
    } else {
        info!("  Proxy headers: untrusted (DGP_TRUST_PROXY_HEADERS=false) — rate limiting requires ConnectInfo (not yet implemented); aws:SourceIp conditions will not match");
    }

    // --- App state ---
    let usage_scanner = Arc::new(UsageScanner::new());
    let state = Arc::new(AppState {
        engine: ArcSwap::from_pointee(engine),
        multipart,
        metrics: metrics.clone(),
        usage_scanner: usage_scanner.clone(),
    });

    // --- Background monitors ---
    spawn_cache_monitor(&state, &metrics);

    // --- IAM ---
    let iam_state = init_iam_state(&config);

    // --- Admin / sessions / config DB (must be before S3 router for mismatch guard) ---
    let admin_password_hash = config.ensure_bootstrap_password_hash();
    let session_store = Arc::new(SessionStore::new());
    spawn_periodic(Duration::from_secs(300), {
        let sessions = session_store.clone();
        move || sessions.cleanup_expired()
    });
    let shared_config = config.clone().into_shared();
    let (config_db, config_db_mismatch) = init_config_db(&admin_password_hash, &iam_state);

    // --- Public prefix snapshot (lock-free, hot-swappable) ---
    let public_prefix_snapshot: deltaglider_proxy::bucket_policy::SharedPublicPrefixSnapshot =
        std::sync::Arc::new(arc_swap::ArcSwap::new(std::sync::Arc::new(
            deltaglider_proxy::bucket_policy::PublicPrefixSnapshot::from_config(&config.buckets),
        )));

    // --- Admission chain (lock-free, hot-swappable) ---
    // Derived from bucket config + operator-authored admission blocks
    // (Phase 3b.2.a schema surface). `build_shared_chain_from_parts`
    // wraps the resulting chain in the same `Arc<ArcSwap<_>>` shape as
    // the public-prefix snapshot so hot-reload sites reuse the swap
    // pattern. Operator-authored blocks log a startup warn explaining
    // they are inert until Phase 3b.2.b lands.
    let admission_chain: deltaglider_proxy::admission::SharedAdmissionChain =
        deltaglider_proxy::admission::build_shared_chain_from_parts(
            &config.buckets,
            &config.admission_blocks,
        );

    // --- S3 router ---
    let app = build_s3_router(
        &state,
        &iam_state,
        &metrics,
        &rate_limiter,
        &replay_cache,
        &config,
        config_db_mismatch,
        &public_prefix_snapshot,
        &admission_chain,
    );

    // --- External auth (OAuth/OIDC) ---
    let external_auth = {
        use deltaglider_proxy::iam::external_auth::ExternalAuthManager;
        let manager = Arc::new(ExternalAuthManager::new());
        if let Some(ref db_mutex) = config_db {
            let db = db_mutex.lock().await;
            let providers = db.load_auth_providers().unwrap_or_default();
            if !providers.is_empty() {
                manager.rebuild(&providers);
                drop(db);
                manager.discover_all().await;
                info!(
                    "  External auth: {} provider(s) configured",
                    manager.provider_names().len()
                );
            }
        }
        // Periodic cleanup for expired pending OAuth flows (every 60s)
        spawn_periodic(Duration::from_secs(60), {
            let mgr = manager.clone();
            move || mgr.cleanup_expired_pending()
        });
        Some(manager)
    };

    // --- Config DB S3 sync ---
    let config_sync = init_config_sync(
        &config,
        &admin_password_hash,
        &config_db,
        &iam_state,
        &external_auth,
    )
    .await;

    // Start periodic config DB S3 poll (every 5 minutes)
    if let Some(ref sync) = config_sync {
        spawn_config_sync_poll(
            sync.clone(),
            &config_db,
            &iam_state,
            &external_auth,
            &admin_password_hash,
        );
    }

    // Resolve the authoritative config-file path at startup and freeze it in
    // AdminState. `--config` wins over `resolve_config_path()` which walks
    // the env var + default search paths. If neither is set, the field stays
    // None and any future persist falls back to the canonical default.
    let config_file_path = cli.config.clone().or_else(Config::resolve_config_path);

    let admin_state = Arc::new(AdminState {
        password_hash: parking_lot::RwLock::new(admin_password_hash),
        sessions: session_store,
        config: shared_config,
        config_file_path,
        log_reload: log_reload_handle,
        s3_state: state.clone(),
        iam_state,
        config_db,
        usage_scanner: usage_scanner.clone(),
        rate_limiter,
        config_sync,
        config_db_mismatch,
        external_auth,
        public_prefix_snapshot,
        admission_chain,
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
        // `into_make_service_with_connect_info::<SocketAddr>` surfaces the
        // peer IP to middlewares via `axum::extract::ConnectInfo`. The
        // admission chain's source-ip predicates depend on this — without
        // it, operator-authored deny rules keyed on `source_ip_list` would
        // be silently inert in the default deployment (no reverse proxy
        // setting X-Forwarded-For). See adversarial review of Phase 3b.2.b
        // for the failure mode.
        axum_server::bind_rustls(config.listen_addr, rustls_config)
            .handle(handle)
            .serve(app.into_make_service_with_connect_info::<std::net::SocketAddr>())
            .await?;
    } else {
        let listener = TcpListener::bind(&config.listen_addr).await?;
        info!(
            "DeltaGlider Proxy listening on http://{}",
            config.listen_addr
        );
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    }

    info!("Server shutdown complete");
    Ok(())
}
