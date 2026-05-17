// SPDX-License-Identifier: GPL-3.0-only

//! DeltaGlider Proxy - S3-compatible object storage with DeltaGlider deduplication

mod demo;
mod startup;
use startup::*;

use arc_swap::ArcSwap;
use axum::middleware;
use clap::{Parser, Subcommand};
use deltaglider_proxy::api::admin::AdminState;
use deltaglider_proxy::api::handlers::{debug_headers_enabled, AppState};
use deltaglider_proxy::config::{env_parse_with_default, Config};
use deltaglider_proxy::deltaglider::DynEngine;
use deltaglider_proxy::multipart::MultipartStore;
use deltaglider_proxy::rate_limiter::RateLimiter;
use deltaglider_proxy::session::SessionStore;
use deltaglider_proxy::usage_scanner::UsageScanner;
use std::sync::Arc;
use std::time::{Duration, Instant};
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

/// Top-level subcommands. The `config` and `admission` families
/// operate on the proxy's own config / request pipeline. The S3
/// commands (`cp`, `ls`, `rm`, `stats`, `verify`, `sync`, `migrate`,
/// `purge`, `get-bucket-acl`, `put-bucket-acl`) talk directly to an
/// S3 endpoint and apply the same delta-storage layout the proxy
/// uses — same toolchain, no Python deltaglider dependency.
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
    /// List S3 buckets or objects (AWS-CLI-shaped output).
    Ls(deltaglider_proxy::cli::ls::LsArgs),
    /// Remove S3 objects (single-key or recursive prefix-delete).
    Rm(deltaglider_proxy::cli::rm::RmArgs),
    /// Copy files between local paths and S3 with transparent delta compression.
    Cp(deltaglider_proxy::cli::cp::CpArgs),
    /// Bucket statistics: original/stored bytes, savings %, deltaspace health.
    Stats(deltaglider_proxy::cli::stats::StatsArgs),
    /// Verify the integrity of a DeltaGlider-stored object (SHA256 round-trip).
    Verify(deltaglider_proxy::cli::verify::VerifyArgs),
    /// Get a bucket's ACL (AWS-CLI shape).
    GetBucketAcl(deltaglider_proxy::cli::bucket_acl::GetArgs),
    /// Update a bucket's ACL (canned-ACL or grant-* flags).
    PutBucketAcl(deltaglider_proxy::cli::bucket_acl::PutArgs),
    /// Migrate a deltaspace between buckets / accounts via the engine.
    Migrate(deltaglider_proxy::cli::migrate::MigrateArgs),
    /// Sync a directory between local and S3 (or between two S3 prefixes).
    Sync(deltaglider_proxy::cli::sync::SyncArgs),
    /// Purge expired Python-toolchain rehydration cache entries
    /// (`.deltaglider/tmp/*` with past `dg-expires-at`).
    Purge(deltaglider_proxy::cli::purge::PurgeArgs),
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
    /// Validate a config file without applying or hitting a server.
    ///
    /// Runs the same pre-apply validation the admin API's
    /// `/config/validate` endpoint uses: shape classification, serde
    /// deny-unknown-fields, shorthand normalization, admission-block
    /// semantic validation. Exit status 0 = clean; 4 = parse error;
    /// 6 = validation error (emits warnings on stderr regardless).
    ///
    /// Intended for CI pipelines: `deltaglider_proxy config lint
    /// path/to/config.yaml` in a pre-merge hook catches operator
    /// typos before they reach production.
    Lint {
        /// Config file to validate (YAML or TOML).
        #[arg(value_name = "FILE")]
        file: String,
    },
    /// Emit the defaults + docstrings for every Config field as JSON.
    ///
    /// Backs YAML LSP autocompletion, operator documentation, and the
    /// `config init` wizard (Phase 4+). Drives off the schemars
    /// `JsonSchema` derives already on every Config struct — so the
    /// output tracks schema changes automatically.
    Defaults {
        /// Write output to a file instead of stdout.
        #[arg(long, value_name = "OUTPUT")]
        out: Option<String>,
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
            admission_trace, apply, defaults, lint, migrate, schema, AdminClientOpts, TraceArgs,
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
                ConfigCommand::Lint { file } => lint(file),
                ConfigCommand::Defaults { out } => defaults(out.as_deref()),
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
            // S3 subcommands are async — each owns its own tokio
            // runtime so the proxy-startup path stays cold when the
            // binary is invoked purely as a CLI.
            Command::Ls(args) => run_cli_async(deltaglider_proxy::cli::ls::run(args.clone())),
            Command::Rm(args) => run_cli_async(deltaglider_proxy::cli::rm::run(args.clone())),
            Command::Cp(args) => run_cli_async(deltaglider_proxy::cli::cp::run(args.clone())),
            Command::Stats(args) => run_cli_async(deltaglider_proxy::cli::stats::run(args.clone())),
            Command::Verify(args) => {
                run_cli_async(deltaglider_proxy::cli::verify::run(args.clone()))
            }
            Command::GetBucketAcl(args) => {
                run_cli_async(deltaglider_proxy::cli::bucket_acl::get_run(args.clone()))
            }
            Command::PutBucketAcl(args) => {
                run_cli_async(deltaglider_proxy::cli::bucket_acl::put_run(args.clone()))
            }
            Command::Migrate(args) => {
                run_cli_async(deltaglider_proxy::cli::migrate::run(args.clone()))
            }
            Command::Sync(args) => run_cli_async(deltaglider_proxy::cli::sync::run(args.clone())),
            Command::Purge(args) => run_cli_async(deltaglider_proxy::cli::purge::run(args.clone())),
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
    //
    // `load_from_path` folds the same env-override + validate pipeline as
    // `Config::load()` onto the CLI-provided path, so `DGP_*` vars override
    // the file identically whether or not `--config` is specified. (The old
    // `from_file(path).unwrap_or_else(load)` path quietly dropped env
    // overrides when the file parsed, which broke `DGP_BOOTSTRAP_PASSWORD`
    // specifically — documented in the revamp plan's risks table.)
    let pre_config = if let Some(ref path) = cli.config {
        deltaglider_proxy::config::Config::load_from_path(path)
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

/// Run a single async CLI subcommand on its own small tokio runtime.
/// The proxy's full runtime is heavyweight (blocking threads, the
/// scheduler-monitor task, etc.); CLI commands need a one-off tokio
/// runtime that exits with the process.
fn run_cli_async<F: std::future::Future<Output = i32>>(fut: F) -> i32 {
    match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt.block_on(fut),
        Err(e) => {
            eprintln!("error: failed to start tokio runtime: {e}");
            deltaglider_proxy::cli::config::EXIT_IO
        }
    }
}

async fn async_main(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    // --- Logging ---
    let log_reload_handle = init_tracing(&cli);

    // --- Configuration ---
    // Use `load_from_path` for explicit --config so env overrides still apply
    // (parity with the implicit search path taken by `Config::load()`). See
    // the pre_config block in `main()` for the same fix.
    let mut config = if let Some(ref path) = cli.config {
        Config::load_from_path(path)?
    } else {
        Config::load()
    };
    if let Some(ref addr) = cli.listen {
        config.listen_addr = addr.parse()?;
    }

    // Apply `advanced.log_level` from the config file to the already-
    // initialised tracing filter — caught during browser testing of
    // v0.8.0 where `advanced.log_level: info` was silently ignored
    // because `init_tracing` only reads env vars. Priority:
    //   RUST_LOG > DGP_LOG_LEVEL > config.log_level > --verbose > default.
    // The first two were already honoured by `init_tracing`; we only
    // reload from `config.log_level` if neither env var was set (so
    // env-driven deployments keep their semantics) and the config
    // value differs from what init_tracing chose.
    if std::env::var("RUST_LOG").is_err() && std::env::var("DGP_LOG_LEVEL").is_err() {
        match config.log_level.parse::<tracing_subscriber::EnvFilter>() {
            Ok(filter) => {
                if let Err(e) = log_reload_handle.reload(filter) {
                    eprintln!(
                        "Warning: could not apply log_level={:?} from config: {}",
                        config.log_level, e
                    );
                }
            }
            Err(e) => {
                eprintln!(
                    "Warning: ignoring invalid log_level={:?} from config: {}",
                    config.log_level, e
                );
            }
        }
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
    let multipart_sweep_interval_secs: u64 =
        env_parse_with_default("DGP_MULTIPART_SWEEP_INTERVAL_SECS", 300);
    let multipart_sweep_max_age_secs: u64 =
        env_parse_with_default("DGP_MULTIPART_SWEEP_MAX_AGE_SECS", 3600);
    let multipart_completing_timeout_secs: u64 = env_parse_with_default(
        "DGP_MULTIPART_COMPLETING_TIMEOUT_SECS",
        multipart_sweep_max_age_secs,
    );
    let multipart_sweep_interval = Duration::from_secs(multipart_sweep_interval_secs.max(1));
    let multipart_sweep_max_age = Duration::from_secs(multipart_sweep_max_age_secs.max(1));
    let multipart_completing_timeout =
        Duration::from_secs(multipart_completing_timeout_secs.max(1));
    let startup_sweep_begin = Instant::now();
    let startup_sweep = multipart.sweep_orphan_relay_artifacts();
    metrics
        .multipart_sweep_runs_total
        .with_label_values(&["startup"])
        .inc();
    metrics
        .multipart_sweep_duration_seconds
        .with_label_values(&["startup"])
        .observe(startup_sweep_begin.elapsed().as_secs_f64());
    metrics
        .multipart_sweep_orphan_relay_dirs_total
        .inc_by(startup_sweep.orphan_relay_dirs_removed);
    metrics
        .multipart_sweep_orphan_relay_files_total
        .inc_by(startup_sweep.orphan_relay_files_removed);
    if startup_sweep.orphan_relay_dirs_removed > 0 || startup_sweep.orphan_relay_files_removed > 0 {
        info!(
            "multipart startup sweep removed {} orphan relay dirs and {} orphan relay files",
            startup_sweep.orphan_relay_dirs_removed, startup_sweep.orphan_relay_files_removed
        );
    }

    spawn_periodic(multipart_sweep_interval, {
        let mp = multipart.clone();
        let metrics = metrics.clone();
        move || {
            let begin = Instant::now();
            let report = mp.cleanup_expired(multipart_sweep_max_age, multipart_completing_timeout);
            let orphan_report = mp.sweep_orphan_relay_artifacts();
            metrics
                .multipart_sweep_runs_total
                .with_label_values(&["periodic"])
                .inc();
            metrics
                .multipart_sweep_duration_seconds
                .with_label_values(&["periodic"])
                .observe(begin.elapsed().as_secs_f64());
            metrics
                .multipart_swept_uploads_total
                .with_label_values(&["open"])
                .inc_by(report.swept_open_uploads);
            metrics
                .multipart_swept_uploads_total
                .with_label_values(&["completing"])
                .inc_by(report.swept_completing_uploads);
            metrics
                .multipart_sweep_reclaimed_bytes_total
                .inc_by(report.reclaimed_bytes);
            metrics
                .multipart_sweep_orphan_relay_dirs_total
                .inc_by(orphan_report.orphan_relay_dirs_removed);
            metrics
                .multipart_sweep_orphan_relay_files_total
                .inc_by(orphan_report.orphan_relay_files_removed);
            metrics
                .multipart_sweep_last_uploads_reclaimed
                .set(report.total_uploads_swept() as f64);
            metrics
                .multipart_sweep_last_reclaimed_bytes
                .set(report.reclaimed_bytes as f64);
        }
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

    // --- App state ---
    let usage_scanner = Arc::new(UsageScanner::new());
    let state = Arc::new(AppState {
        engine: ArcSwap::from_pointee(engine),
        multipart,
        metrics: metrics.clone(),
        usage_scanner: usage_scanner.clone(),
        config_db: config_db.clone(),
        form_post_replay: Arc::new(dashmap::DashMap::new()),
    });

    // --- Background monitors ---
    spawn_cache_monitor(&state, &metrics);

    if !config_db_mismatch {
        deltaglider_proxy::lifecycle::scheduler::spawn_scheduler(
            shared_config.clone(),
            config_db.clone(),
            state.clone(),
        );
        if let Some(db) = config_db.as_ref() {
            deltaglider_proxy::replication::scheduler::spawn_scheduler(
                shared_config.clone(),
                db.clone(),
                state.clone(),
            );
            deltaglider_proxy::event_delivery::spawn_dispatcher(shared_config.clone(), db.clone());
        }
    }

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
        delta_efficiency_scanner: Arc::new(
            deltaglider_proxy::api::admin::DeltaEfficiencyScanner::new(),
        ),
        bucket_scanner: deltaglider_proxy::api::admin::BucketScanner::load(
            std::path::PathBuf::from(".deltaglider_scans"),
        ),
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
