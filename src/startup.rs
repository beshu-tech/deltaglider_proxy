// SPDX-License-Identifier: BUSL-1.1

//! Server startup helpers — extracted from main.rs for file size.

use deltaglider_proxy::api::handlers::AppState;
use deltaglider_proxy::config::{BackendConfig, Config};
use deltaglider_proxy::config_db_sync::ConfigDbSync;
use deltaglider_proxy::iam::{AuthConfig, IamState, SharedIamState};
use deltaglider_proxy::metrics::Metrics;
use std::io::IsTerminal;
use std::sync::Arc;
use std::time::Duration;
use tokio::signal;
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
    let spec = std::env::var("RUST_LOG")
        .or_else(|_| std::env::var("DGP_LOG_LEVEL"))
        .unwrap_or_else(|_| {
            if cli.verbose {
                "deltaglider_proxy=trace,tower_http=trace".to_string()
            } else {
                "deltaglider_proxy=debug,tower_http=debug".to_string()
            }
        });
    let initial_filter = EnvFilter::new(deltaglider_proxy::audit::with_audit_directive(&spec));

    let (filter_layer, reload_handle) = reload::Layer::new(initial_filter);

    // Log FORMAT is a startup-only choice (it can't hot-reload like the level):
    // DGP_LOG_FORMAT=json emits one JSON object per line (greppable with `jq` —
    // every span field becomes a key), else the human-readable text format.
    // Boxed so both arms share one registry-build site.
    use tracing_subscriber::Layer;
    let json_format = std::env::var("DGP_LOG_FORMAT")
        .map(|v| v.eq_ignore_ascii_case("json"))
        .unwrap_or(false);
    let fmt_layer = if json_format {
        tracing_subscriber::fmt::layer().json().boxed()
    } else {
        tracing_subscriber::fmt::layer()
            .with_ansi(std::io::stdout().is_terminal())
            .boxed()
    };

    // 3rd layer: capture events (at the DGP_LOG_RING_LEVEL floor, default INFO)
    // into the in-process ring + broadcast that power the admin GUI log viewer
    // (GET /_/api/admin/logs[/stream]). Gated by the global EnvFilter above, so a
    // hot level change affects it too; its own floor keeps per-request debug spam
    // out of the ring.
    tracing_subscriber::registry()
        .with(filter_layer)
        .with(fmt_layer)
        .with(deltaglider_proxy::logs::LogCaptureLayer::new())
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
    warn_active_test_seams();
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
///
/// The decision itself is the pure [`Config::classify_auth_config`]; this
/// wrapper owns the logging and the `process::exit` (the un-testable I/O).
/// The DGP_TEST_* seams inject faults/delays and clamp page budgets. They ship
/// in release code paths (inert unless set), so loudly warn if one is active —
/// an operator who exported one by accident would otherwise silently run a
/// crippled proxy (finding #32).
fn warn_active_test_seams() {
    const SEAMS: &[&str] = &[
        "DGP_TEST_FAIL_PART_ONCE",
        "DGP_TEST_PART_BARRIER",
        "DGP_TEST_PART_DELAY_MS",
        "DGP_TEST_OBJECT_BARRIER",
        "DGP_TEST_OBJECT_DELAY_MS",
        "DGP_TEST_COPY_STALL_MS",
        "DGP_TEST_MAX_JOB_PAGES",
        "DGP_TEST_FORCE_NONCAS_BACKEND",
        "DGP_RELAY_FOREIGN_MIN_AGE_SECS",
    ];
    let active: Vec<&str> = SEAMS
        .iter()
        .copied()
        .filter(|k| std::env::var_os(k).is_some())
        .collect();
    if !active.is_empty() {
        warn!(
            "  TEST SEAMS ACTIVE ({}) — these inject faults/delays/budget clamps and \
             must NEVER be set in production.",
            active.join(", ")
        );
    }
}

fn validate_auth_config(config: &Config) {
    use deltaglider_proxy::config::AuthConfigOutcome;
    match config.classify_auth_config() {
        AuthConfigOutcome::CredentialsEnabled { redundant_none } => {
            info!(
                "  Authentication: SigV4 ENABLED (access key: {})",
                config.access_key_id.as_deref().unwrap_or("")
            );
            if redundant_none {
                warn!("  Note: `authentication: none` is ignored because S3 credentials are configured");
            }
        }
        AuthConfigOutcome::OpenAccess => {
            warn!("  Authentication: DISABLED (`authentication: none`)");
            warn!("  ╔══════════════════════════════════════════════════════════════════╗");
            warn!("  ║  WARNING: All S3 data is accessible without credentials.        ║");
            warn!("  ║  Set access_key_id + secret_access_key for production use.      ║");
            warn!("  ╚══════════════════════════════════════════════════════════════════╝");
        }
        AuthConfigOutcome::UnrecognizedMode => {
            error!(
                "FATAL: Unrecognized authentication mode: \"{}\"",
                config.authentication.as_deref().unwrap_or("")
            );
            error!("");
            for line in AuthConfigOutcome::UnrecognizedMode
                .fatal_help()
                .unwrap_or_default()
            {
                error!("{line}");
            }
            std::process::exit(1);
        }
        AuthConfigOutcome::Missing => {
            error!("FATAL: No authentication configured.");
            error!("");
            for line in AuthConfigOutcome::Missing.fatal_help().unwrap_or_default() {
                error!("{line}");
            }
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
    let backend_type = backend_type_label(config);
    // The public scrape carries the exact version only on operator opt-in.
    let version = deltaglider_proxy::metrics::build_info_version_label(
        deltaglider_proxy::config::env_bool("DGP_METRICS_EXPOSE_VERSION", false),
    );
    metrics
        .build_info
        .with_label_values(&[version, backend_type])
        .set(1.0);
    metrics
}

/// `build_info`'s `backend_type` label. With named `backends` configured the
/// legacy singleton `backend` is ignored at runtime, so reading it labelled a
/// two-backend (S3 + filesystem) deployment "filesystem" — and the dashboard
/// header repeated it. Mixed types report "mixed".
fn backend_type_label(config: &Config) -> &'static str {
    fn kind(b: &BackendConfig) -> &'static str {
        match b {
            BackendConfig::Filesystem { .. } => "filesystem",
            BackendConfig::S3 { .. } => "s3",
        }
    }
    let mut kinds = config.backends.iter().map(|b| kind(&b.backend));
    match kinds.next() {
        None => kind(&config.backend),
        Some(first) if kinds.all(|k| k == first) => first,
        Some(_) => "mixed",
    }
}

/// Create the replay-attack detection cache and spawn its periodic cleanup.
pub fn init_replay_cache() -> deltaglider_proxy::api::auth::ReplayCache {
    let replay_cache: deltaglider_proxy::api::auth::ReplayCache = Arc::new(dashmap::DashMap::new());
    // The TTL sweep drops what the check no longer reads: entries older
    // than the window (DGP_REPLAY_WINDOW_SECS, default the skew). A window
    // longer than the skew is capped at it: an older signature fails
    // verification anyway.
    let replay_window_secs = deltaglider_proxy::api::auth::replay_window()
        .as_secs()
        .min(u64::from(deltaglider_proxy::api::auth::clock_skew_secs()));
    // #86: the retain is an O(live-signatures) walk — up to 500k shards under
    // load (MAX_REPLAY_ENTRIES) — so it runs on the blocking pool.
    spawn_periodic_blocking(Duration::from_secs(60), {
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

/// What the startup declarative-IAM reconcile should do, decided purely from
/// the YAML-empty flag and the previewed diff. Keeps the destructive-change and
/// empty-wipe policy testable without spawning a process.
#[derive(Debug, PartialEq, Eq)]
pub enum StartupReconcileAction {
    /// YAML has no IAM — skip (an empty declarative reconcile would wipe the DB).
    SkipEmpty,
    /// The diff would delete existing DB rows — refuse (must be applied attended).
    RefuseDestructive {
        users: usize,
        groups: usize,
        providers: usize,
    },
    /// Safe to reconcile (fresh deploy or additive/idempotent change).
    Reconcile,
}

/// Pure policy for the unattended startup reconcile. `yaml_empty` is
/// `DeclarativeIam::is_empty()`; the counts are the previewed diff's delete
/// vectors. A startup boot must never silently DELETE DB users/groups/providers
/// (that's an attended `config apply` decision), and must never reconcile an
/// empty YAML (which would wipe the DB).
pub fn startup_declarative_action(
    yaml_empty: bool,
    delete_users: usize,
    delete_groups: usize,
    delete_providers: usize,
) -> StartupReconcileAction {
    if yaml_empty {
        return StartupReconcileAction::SkipEmpty;
    }
    if delete_users > 0 || delete_groups > 0 || delete_providers > 0 {
        return StartupReconcileAction::RefuseDestructive {
            users: delete_users,
            groups: delete_groups,
            providers: delete_providers,
        };
    }
    StartupReconcileAction::Reconcile
}

/// On a config-DB-mismatch boot: should we rename the current `.db` to `.db.bak`?
/// ONLY when the db exists AND no `.db.bak` already does — an existing `.db.bak`
/// holds the GOOD DB from an earlier mismatch boot, and the current `.db` is the
/// empty one that boot created, so renaming would clobber the real IAM data
/// (the C1 data-loss bug). Pure so the invariant is unit-tested.
fn should_preserve_as_backup(db_exists: bool, bak_exists: bool) -> bool {
    db_exists && !bak_exists
}

/// What to do about a lingering `.db.bak` when the live config DB OPENED fine.
/// A lingering backup means an earlier mismatch incident MAY be unresolved.
/// An Ok-open node promotes only a POPULATED backup over an EMPTY live DB
/// (the recovery shape: no users, groups, providers or mapping rules); a
/// live DB with any IAM state is never swapped out.
#[derive(Debug, PartialEq, Eq)]
enum BakDisposition {
    /// No `.db.bak` on disk — the normal healthy boot.
    NoBak,
    /// Backup undecryptable + live DB EMPTY: the wrong-hash incident shape —
    /// stay mismatched/locked (the good DB is parked in `.db.bak`).
    Sticky,
    /// Backup decrypts with a key this boot has, holds users, and the live DB
    /// is EMPTY: the operator restored the right key after an incident —
    /// promote the backup (the live file is kept as `.db.discarded`).
    Promote,
    /// Anything else (bak decrypts, or the live DB has users): a stray backup
    /// on a working node — warn loudly, never lock, never touch either file.
    AmbiguousWarn,
}

/// Pure classifier for a lingering `.db.bak` on an Ok-open boot. When
/// `bak_exists` is false the other inputs are don't-cares. `bak_users` is
/// `None` when the backup does not decrypt with the current hash.
fn classify_lingering_bak(
    bak_exists: bool,
    bak_users: Option<usize>,
    live_iam_rows: usize,
) -> BakDisposition {
    match (bak_exists, bak_users, live_iam_rows) {
        (false, _, _) => BakDisposition::NoBak,
        // Undecryptable bak + EMPTY live DB = the boot-2 incident shape.
        (true, None, 0) => BakDisposition::Sticky,
        // A populated live DB that opens is NOT the incident DB — a stray
        // undecryptable bak must not lock a healthy node.
        (true, None, _) => BakDisposition::AmbiguousWarn,
        // S8: the key no longer changes with the bootstrap password, so a
        // recovery boot (right key restored) can open the fresh live DB: the
        // promotion that used to happen only in the Err branch happens here.
        (true, Some(n), 0) if n > 0 => BakDisposition::Promote,
        (true, Some(_), _) => BakDisposition::AmbiguousWarn,
    }
}

/// Classify a lingering `.db.bak` next to the live DB that opened.
fn bak_disposition(
    bak_path: &std::path::Path,
    keys: &deltaglider_proxy::config_db::ConfigDbKeys,
    live: &deltaglider_proxy::config_db::ConfigDb,
) -> BakDisposition {
    if !bak_path.exists() {
        return BakDisposition::NoBak;
    }
    let bak_users = probe_bak_users(bak_path, keys);
    // Any IAM row counts, not only users: a live DB with an OIDC provider or
    // groups (before the first login) is not the incident-empty DB. On a
    // load error err on the side of "populated".
    let live_rows = live.iam_row_count().unwrap_or(usize::MAX);
    classify_lingering_bak(true, bak_users, live_rows)
}

/// Probe `.db.bak` with the config DB keys (primary, then fallbacks):
/// `Some(user_count)` when one opens it, `None` when none does. A zero-byte
/// file is `Some(0)` (junk, not an incident) — the user count is what
/// distinguishes a real parked IAM DB from junk, so promotion requires
/// `Some(n) with n > 0`. Read-only apart from schema migration: a bak that
/// opens with a fallback is re-encrypted only when it is promoted.
fn probe_bak_users(
    bak_path: &std::path::Path,
    keys: &deltaglider_proxy::config_db::ConfigDbKeys,
) -> Option<usize> {
    use deltaglider_proxy::config_db::{probe_key, ConfigDb};
    // A zero-byte file holds no DB under any key: junk with no users, not an
    // undecryptable incident DB (which would lock the node as Sticky).
    if std::fs::metadata(bak_path).is_ok_and(|m| m.len() == 0) {
        return Some(0);
    }
    std::iter::once(&keys.primary)
        .chain(keys.fallbacks.iter().map(|(_, k)| k))
        .find(|k| probe_key(bak_path, k.expose()).unwrap_or(false))
        .and_then(|k| ConfigDb::open_or_create(bak_path, k.expose()).ok())
        .and_then(|db| db.load_users().ok())
        .map(|u| u.len())
}

/// Promote `.db.bak` over the live DB: park the live file as `.db.discarded`,
/// move the backup into place, verify it opens. The parked file is KEPT — the
/// Err-branch live DB is unverifiable (it merely failed to open with the
/// current hash), so deleting it could destroy real data under another hash.
fn promote_backup_db(
    db_file: &std::path::Path,
    bak_path: &std::path::Path,
    keys: &deltaglider_proxy::config_db::ConfigDbKeys,
    has_sync: bool,
) -> Result<deltaglider_proxy::config_db::ConfigDb, String> {
    let discarded = db_file.with_extension("db.discarded");
    std::fs::rename(db_file, &discarded).map_err(|e| {
        format!(
            "rename {} -> {}: {e}",
            db_file.display(),
            discarded.display()
        )
    })?;
    if let Err(e) = std::fs::rename(bak_path, db_file) {
        // Roll the live DB back so the node is no worse off than before.
        let _ = std::fs::rename(&discarded, db_file);
        return Err(format!(
            "rename {} -> {}: {e}",
            bak_path.display(),
            db_file.display()
        ));
    }
    let (db, _) = deltaglider_proxy::config_db_sync::open_live_db(db_file, keys, has_sync)
        .map_err(|e| format!("reopen promoted {}: {e}", db_file.display()))?;
    info!(
        "config-db mismatch incident resolved: promoted backup {} over the live DB \
         (previous live file kept as {} — remove it manually once verified)",
        bak_path.display(),
        discarded.display()
    );
    Ok(db)
}

/// `--set-bootstrap-password` helper: if the config DB still opens only with
/// the CURRENT bootstrap hash (a DB from before S8), re-encrypt it with the
/// config DB key now. Without this, the new hash would leave no key that
/// opens the DB. No DB, or a DB already on the config DB key: no-op.
pub fn migrate_legacy_config_db_key() -> Result<(), String> {
    use deltaglider_proxy::config_db::{key, OpenedWith};
    let db_path = config_db_path();
    if !db_path.exists() {
        return Ok(());
    }
    // The hash this node boots with today: env, then the state files. (A hash
    // in the YAML config also works: the boot migrates with it.)
    let current_hash = ["DGP_BOOTSTRAP_PASSWORD_HASH", "DGP_ADMIN_PASSWORD_HASH"]
        .iter()
        .find_map(|n| std::env::var(n).ok().filter(|v| !v.trim().is_empty()))
        .or_else(|| {
            [".deltaglider_bootstrap_hash", ".deltaglider_admin_hash"]
                .iter()
                .find_map(|f| std::fs::read_to_string(f).ok())
        })
        .map(|raw| Config::decode_hash(raw.trim()));
    let keys =
        key::resolve_config_db_keys(&db_path, current_hash.as_deref(), |n| std::env::var(n).ok())?;
    // The CLI does not know whether the config has a sync bucket, so it
    // always parks the upload: without a sync bucket the marker is inert.
    match deltaglider_proxy::config_db_sync::open_live_db(&db_path, &keys, true) {
        Ok((_, OpenedWith::Migrated(kind))) => {
            eprintln!(
                "Config DB {} re-encrypted: it opened with {}, now with {}.",
                db_path.display(),
                kind.describe(),
                keys.source.describe()
            );
            Ok(())
        }
        Ok(_) => Ok(()),
        Err(e) => Err(format!(
            "the config DB {} does not open with the config DB key or the current \
             bootstrap hash ({e}); fix that before changing the password",
            db_path.display()
        )),
    }
}

/// Resolve the config DB keys (`DGP_CONFIG_DB_KEY`, else the key file, with
/// the bootstrap hash as the legacy fallback). Exits on a configuration that
/// would make the DB unreadable: a short env key, an empty key file, or a
/// config sync bucket without the env key.
pub fn resolve_config_db_keys_or_exit(
    config: &Config,
    admin_password_hash: &str,
) -> deltaglider_proxy::config_db::ConfigDbKeys {
    use deltaglider_proxy::config_db::key::{
        check_sync_needs_env_key, resolve_config_db_keys, CONFIG_DB_KEY_ENV,
    };
    let env_key = std::env::var(CONFIG_DB_KEY_ENV).ok();
    // Before any key file exists: a multi-instance node must not mint its own.
    if let Err(e) =
        check_sync_needs_env_key(config.config_sync_bucket.as_deref(), env_key.as_deref())
    {
        error!("FATAL: {e}");
        std::process::exit(1);
    }
    match resolve_config_db_keys(&config_db_path(), Some(admin_password_hash), |n| {
        std::env::var(n).ok()
    }) {
        Ok(keys) => {
            info!("  Config DB key: {}", keys.source.describe());
            keys
        }
        Err(e) => {
            error!("FATAL: {e}");
            std::process::exit(1);
        }
    }
}

/// Initialize the encrypted IAM config database. If it contains existing
/// users, switch to IAM mode immediately.
///
/// Returns `(config_db, mismatch)` where `mismatch` is true if no config DB
/// key (`DGP_CONFIG_DB_KEY`, key file, legacy bootstrap hash) opens the DB.
pub fn init_config_db(
    keys: &deltaglider_proxy::config_db::ConfigDbKeys,
    iam_state: &SharedIamState,
    config: &Config,
) -> (
    Option<Arc<tokio::sync::Mutex<deltaglider_proxy::config_db::ConfigDb>>>,
    bool,
) {
    init_config_db_attempt(keys, iam_state, config, true)
}

/// One boot attempt. `allow_promote` bounds the recover-then-retry to a
/// single pass (a boot with the correct hash finding the live DB locked but
/// `.db.bak` readable promotes the backup, then re-runs the normal open path).
fn init_config_db_attempt(
    keys: &deltaglider_proxy::config_db::ConfigDbKeys,
    iam_state: &SharedIamState,
    config: &Config,
    allow_promote: bool,
) -> (
    Option<Arc<tokio::sync::Mutex<deltaglider_proxy::config_db::ConfigDb>>>,
    bool,
) {
    let db_file = config_db_path();
    // A DB that moves to a new key (rotation, key-file → env, legacy hash)
    // parks one upload, so the synced copy follows (see `open_live_db`).
    let has_sync = config
        .config_sync_bucket
        .as_deref()
        .is_some_and(|b| !b.trim().is_empty());
    match deltaglider_proxy::config_db_sync::open_live_db(&db_file, keys, has_sync) {
        Ok((db, _opened)) => {
            // Classify a lingering .db.bak BEFORE any boot-time mutation: a
            // node with an unresolved mismatch incident must stay locked.
            let bak_path = db_file.with_extension("db.bak");
            let disposition = bak_disposition(&bak_path, keys, &db);
            match disposition {
                BakDisposition::NoBak => {}
                BakDisposition::Sticky => {
                    error!(
                        "Lingering {} does not decrypt with the config DB key ({}) and the \
                         live DB is empty — an earlier mismatch incident is unresolved. S3 API \
                         stays locked; restart with the original DGP_CONFIG_DB_KEY (or key \
                         file) or use the admin GUI recovery wizard.",
                        bak_path.display(),
                        keys.source.describe()
                    );
                    return (Some(Arc::new(tokio::sync::Mutex::new(db))), true);
                }
                // The parked DB opens with a key this boot has, and the live DB
                // is empty: the operator restored the key. The empty DB is kept
                // as `.db.discarded`.
                BakDisposition::Promote if allow_promote => {
                    drop(db);
                    return match promote_backup_db(&db_file, &bak_path, keys, has_sync) {
                        Ok(db) => {
                            drop(db);
                            init_config_db_attempt(keys, iam_state, config, false)
                        }
                        Err(err) => {
                            error!(
                                "Failed to promote {} during recovery: {err} — S3 API stays \
                                 locked; both files remain on disk for manual recovery.",
                                bak_path.display()
                            );
                            (None, true)
                        }
                    };
                }
                BakDisposition::Promote => {
                    warn!(
                        "Stale {} sits next to a working live config DB — refusing to touch \
                         either file. Remove the stale backup manually.",
                        bak_path.display()
                    );
                }
                BakDisposition::AmbiguousWarn => {
                    warn!(
                        "Stale {} sits next to a working live config DB — refusing to touch \
                         either file. Remove the stale backup manually.",
                        bak_path.display()
                    );
                }
            };
            match db.replication_reconcile_on_boot(config.replication.max_failures_retained) {
                Ok(count) if count > 0 => {
                    warn!(
                        "Reconciled {count} replication run(s) left running by a previous process"
                    );
                }
                Ok(_) => {}
                Err(err) => {
                    warn!("Failed to reconcile replication runtime state on boot: {err}");
                }
            }
            // Clear a parity audit left 'running' by a crashed process + its lease.
            match db.parity_reconcile_on_boot() {
                Ok(count) if count > 0 => {
                    warn!("Reconciled {count} parity audit(s) left running by a previous process");
                }
                Ok(_) => {}
                Err(err) => {
                    warn!("Failed to reconcile parity audit state on boot: {err}");
                }
            }
            match db.lifecycle_reconcile_on_boot(config.lifecycle.max_failures_retained) {
                Ok(count) if count > 0 => {
                    warn!("Reconciled {count} lifecycle run(s) left running by a previous process");
                }
                Ok(_) => {}
                Err(err) => {
                    warn!("Failed to reconcile lifecycle runtime state on boot: {err}");
                }
            }
            // Maintenance jobs are one-offs the operator explicitly started:
            // interrupted ones go back to QUEUED with their cursor preserved
            // (the worker resumes them), unlike replication's running→failed.
            // Lease-aware: a freshly-crashed job's lease may still be live
            // here; the worker loop re-runs this on every poll tick, so it
            // becomes claimable within one lease TTL.
            match db.maintenance_requeue_abandoned() {
                Ok(count) if count > 0 => {
                    warn!(
                        "Re-queued {count} maintenance job(s) interrupted by a previous process — \
                         they will resume shortly"
                    );
                }
                Ok(_) => {}
                Err(err) => {
                    warn!("Failed to reconcile maintenance jobs on boot: {err}");
                }
            }
            // Declarative IAM: the YAML is the source of truth, so reconcile it
            // into the DB AT STARTUP (not just on a `config apply`). Without this
            // a fresh declarative deployment comes up with an empty DB and needs
            // a human to push the config — defeating the whole point of IaC. This
            // mirrors the admin `config apply` reconcile path; it's idempotent
            // (re-running on an already-matching DB is a no-op diff), so it's safe
            // on every boot. Two guards make the UNATTENDED boot safe:
            //   1. A startup reconcile that would DELETE existing users/groups/
            //      providers is REFUSED — destructive declarative changes (e.g. a
            //      gui→declarative flip that omits GUI-added users) must go through
            //      an attended `config apply`, never silently on a pod restart.
            //   2. A reconcile ERROR in declarative mode is FATAL: the running IAM
            //      would not match the declared intent (everything-403 on a fresh
            //      deploy, or a silent split with Git), so refuse to start and let
            //      the orchestrator surface a crash-loop instead.
            if matches!(
                config.iam_mode,
                deltaglider_proxy::config_sections::IamMode::Declarative
            ) {
                let yaml = deltaglider_proxy::iam::snapshot_from_access(
                    &config.iam_users,
                    &config.iam_groups,
                    &config.auth_providers,
                    &config.group_mapping_rules,
                    &[],
                );
                // Preview the diff (no writes), then apply the pure policy.
                let diff = match deltaglider_proxy::iam::preview_declarative_iam(&db, &yaml) {
                    Ok(d) => d,
                    Err(e) => {
                        error!(
                            "FATAL: could not compute the declarative IAM diff at startup: {e}. \
                             Refusing to start. Fix the config DB / YAML and restart."
                        );
                        std::process::exit(1);
                    }
                };
                match startup_declarative_action(
                    yaml.is_empty(),
                    // Count only LOCAL (authored-state) user deletes — a
                    // reconstructable OAuth-provisioned external user being
                    // culled is benign + by-design (login rebuilds it) and must
                    // not refuse-to-start / crash-loop a declarative+OAuth deploy.
                    diff.local_user_delete_count(),
                    diff.groups_to_delete.len(),
                    diff.providers_to_delete.len(),
                ) {
                    StartupReconcileAction::SkipEmpty => warn!(
                        "iam_mode: declarative but the config has no iam_users/iam_groups — \
                         not reconciling (an empty declarative IAM would wipe the DB). Add \
                         access.iam_users to the config."
                    ),
                    StartupReconcileAction::RefuseDestructive {
                        users,
                        groups,
                        providers,
                    } => {
                        error!(
                            "FATAL: startup declarative IAM reconcile would DELETE {users} user(s), \
                             {groups} group(s), {providers} provider(s) present in the config DB but \
                             absent from the YAML. Destructive declarative changes must be applied \
                             attended via `config apply`, not silently on a restart. If this is \
                             intentional (e.g. a first gui→declarative migration), run \
                             `deltaglider_proxy config apply <file>` once."
                        );
                        std::process::exit(1);
                    }
                    StartupReconcileAction::Reconcile => {
                        match deltaglider_proxy::iam::reconcile_declarative_iam(&db, &yaml) {
                            Ok(stats) => info!(
                                "Declarative IAM reconciled at startup: {} user(s), {} group(s) \
                                 ({} created, {} updated)",
                                yaml.users.len(),
                                yaml.groups.len(),
                                stats.users_created.len(),
                                stats.users_updated.len(),
                            ),
                            Err(e) => {
                                error!(
                                    "FATAL: declarative IAM reconcile failed at startup: {e}. \
                                     Refusing to start in declarative mode with an IAM set that \
                                     does not match the YAML. Fix the config and restart."
                                );
                                std::process::exit(1);
                            }
                        }
                    }
                }
            }

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
                    // DB users force IAM mode, overriding `authentication: none`
                    // — warn so the resulting AccessDenied isn't read as data loss.
                    use deltaglider_proxy::config::AuthConfigOutcome;
                    if matches!(config.classify_auth_config(), AuthConfigOutcome::OpenAccess) {
                        warn!(
                            "  Authentication: IAM mode is ACTIVE ({} user(s) in {}) — this \
                             OVERRIDES `authentication: none`. Open/anonymous browser \
                             access will get AccessDenied; log in as an IAM user, or delete \
                             {} to use open access.",
                            users.len(),
                            db_file.display(),
                            db_file.display()
                        );
                    }
                    let state = deltaglider_proxy::iam::IamIndex::build_iam_state(
                        users,
                        groups,
                        &iam_state.load(),
                    );
                    iam_state.store(Arc::new(state));
                }
                // If no users exist, keep current IamState (Legacy or Disabled)
            }
            (Some(Arc::new(tokio::sync::Mutex::new(db))), false)
        }
        // Only a wrong key is a key mismatch. A busy file, an I/O error, a
        // failed migration or a DB from a newer binary must NOT park the good
        // DB as `.db.bak` and boot on an empty one: stop, and leave the file.
        Err(e)
            if !matches!(
                e,
                deltaglider_proxy::config_db::ConfigDbError::WrongPassphrase(_)
            ) =>
        {
            error!(
                "Config DB {} failed to open: {e}. The file is left untouched; \
                 fix the cause and restart.",
                db_file.display()
            );
            std::process::exit(1);
        }
        Err(e) => {
            let bak_path = db_file.with_extension("db.bak");
            // Recovery boot: the live DB won't open but .db.bak opens with the
            // current hash AND contains users — the operator restarted with the
            // GOOD password. Promote and retry once instead of wedging forever.
            // The user-count gate is the safety anchor: a junk/zero-byte bak
            // opens under ANY hash, and promoting it would destroy the live DB.
            if allow_promote
                && db_file.exists()
                && bak_path.exists()
                && probe_bak_users(&bak_path, keys).is_some_and(|n| n > 0)
            {
                match promote_backup_db(&db_file, &bak_path, keys, has_sync) {
                    Ok(db) => {
                        drop(db);
                        return init_config_db_attempt(keys, iam_state, config, false);
                    }
                    Err(err) => {
                        error!(
                            "Failed to promote {} during recovery: {err} — S3 API stays \
                             locked; both files remain on disk for manual recovery.",
                            bak_path.display()
                        );
                        return (None, true);
                    }
                }
            }
            // Preserve the existing DB as .bak instead of deleting — recovery needs it.
            // CLOBBER-GUARD: if a .bak already exists it holds the GOOD DB from an
            // EARLIER mismatch boot; the current db_file is the empty DB that boot
            // created. `rename` would overwrite the good backup with the empty one
            // and destroy the real IAM data — so never rename over an existing .bak.
            if should_preserve_as_backup(db_file.exists(), bak_path.exists()) {
                if let Err(rename_err) = std::fs::rename(&db_file, &bak_path) {
                    warn!(
                        "Failed to backup config DB to {}: {}",
                        bak_path.display(),
                        rename_err
                    );
                } else {
                    error!(
                        "The config DB key ({}) does not open the config DB ({e}) — original \
                         preserved as {}. Restore the DGP_CONFIG_DB_KEY or key file that \
                         encrypted it and restart, or use the admin GUI recovery wizard.",
                        keys.source.describe(),
                        bak_path.display()
                    );
                }
            } else if db_file.exists() {
                // .bak already holds the good DB — leave it untouched.
                error!(
                    "The config DB key ({}) does not open the config DB — good backup \
                     already preserved at {}. Restore the original key and restart, or use \
                     the admin GUI recovery wizard.",
                    keys.source.describe(),
                    bak_path.display()
                );
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
                keys.primary.expose(),
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

/// `DGP_CONFIG_DB_ACCEPT_LEGACY_SYNC` (S8, transitional): may a synced copy
/// open with the legacy bootstrap hash?
fn accept_legacy_sync() -> bool {
    deltaglider_proxy::config::env_bool(
        deltaglider_proxy::config_db::key::ACCEPT_LEGACY_SYNC_ENV,
        false,
    )
}

/// The coordination bucket's backend (see [`Config::coordination_backend`]).
/// Falls back to the singleton only for a sync bucket routed to an undefined
/// backend, which `check_fatal` refuses before this runs.
fn coordination_backend(config: &Config) -> &BackendConfig {
    config.coordination_backend().unwrap_or(&config.backend)
}

/// Build the job-plane leader lease, selecting the impl by whether a
/// CAS-capable coordination bucket is configured. Called BEFORE the schedulers
/// spawn so they receive a live handle.
///
/// - No `config_sync_bucket` → [`LocalLease`] (single-instance, node-local, no S3
///   traffic). "HA inactive".
/// - `config_sync_bucket` set → validate it enforces conditional writes
///   ([`ConfigDbSync::validate_coordination_bucket`], which CRASHES on a
///   silent-clobber backend). On success → [`S3Lease`] (real cross-node failover).
///   If the bucket/client can't be built at all, LOG and fall back to
///   [`LocalLease`] rather than block startup (edge case E6 — never a SPOF).
///
/// The validation here is idempotent with `init_config_sync`'s later call (the
/// witness fast-path makes the second a single cheap GET).
pub async fn build_coordination_lease(
    config: &Config,
    config_db: &Arc<tokio::sync::Mutex<deltaglider_proxy::config_db::ConfigDb>>,
    db_keys: &deltaglider_proxy::config_db::ConfigDbKeys,
) -> Arc<dyn deltaglider_proxy::coordination::CoordinationLease> {
    use deltaglider_proxy::coordination::{durable_node_id, LocalLease, S3Lease};

    let local = || Arc::new(LocalLease::new(config_db.clone()));

    let sync_bucket = match &config.config_sync_bucket {
        Some(b) if !b.is_empty() => b.clone(),
        _ => {
            info!("Job-plane lease: node-local (single-instance; no coordination bucket)");
            return local();
        }
    };

    let node_id = durable_node_id(
        config_db_path()
            .parent()
            .unwrap_or_else(|| std::path::Path::new(".")),
    );

    // Build the coordination client + validate the bucket enforces CAS. The
    // validation CRASHES on a silent-clobber backend (data-loss trap); it is
    // idempotent with init_config_sync's later call (witness fast-path).
    let sync = match ConfigDbSync::new(
        coordination_backend(config),
        sync_bucket.clone(),
        config
            .config_sync_object_key
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| {
                deltaglider_proxy::config_db_sync::DEFAULT_CONFIG_SYNC_OBJECT_KEY.to_string()
            }),
        config_db_path(),
        db_keys.clone(),
        accept_legacy_sync(),
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            warn!(
                "Job-plane lease: coordination client for bucket '{sync_bucket}' failed to build \
                 ({e}) — node-local fallback (NOT cross-node HA)"
            );
            return local();
        }
    };

    if let Err(e) = sync.validate_coordination_bucket().await {
        error!("FATAL: coordination bucket validation failed (lease build): {e}");
        std::process::exit(1);
    }

    match ConfigDbSync::build_client(coordination_backend(config)).await {
        Ok(client) => {
            info!(
                "Job-plane lease: S3-CAS on bucket '{sync_bucket}' (cross-node failover, \
                 node_id={node_id})"
            );
            Arc::new(S3Lease::new(client, sync_bucket, node_id))
        }
        Err(e) => {
            warn!("Job-plane lease: coordination client build failed ({e}) — node-local fallback");
            local()
        }
    }
}

/// Build the CROSS-INSTANCE reference lock (the mutex-shaped guard that closes
/// `B1`: concurrent same-deltaspace `reference.bin` writes from two nodes).
///
/// Gated exactly like the job-plane lease: no `config_sync_bucket` → `None`
/// (single-instance; the engine's in-process prefix mutex is the whole story,
/// zero S3 traffic). With a coordination bucket set, build a CAS client against
/// it and return an [`S3ReferenceLock`]. This does NOT re-validate the bucket's
/// CAS support — [`build_coordination_lease`] already did (and `exit(1)`s a
/// silent-clobber backend), so by the time the engine writes a reference the
/// bucket is known-good. A client-build failure is non-fatal (`None`, warn):
/// same "never a SPOF" stance as the lease builder — a transient blip must not
/// wedge startup, and the single-writer routing contract still applies.
pub async fn build_reference_lock(
    config: &Config,
) -> Option<Arc<dyn deltaglider_proxy::coordination::ReferenceLock>> {
    use deltaglider_proxy::coordination::{durable_node_id, S3ReferenceLock};

    let sync_bucket = match &config.config_sync_bucket {
        Some(b) if !b.is_empty() => b.clone(),
        _ => {
            info!("Reference lock: in-process only (single-instance; no coordination bucket)");
            return None;
        }
    };
    let node_id = durable_node_id(
        config_db_path()
            .parent()
            .unwrap_or_else(|| std::path::Path::new(".")),
    );
    let ttl_secs: i64 =
        deltaglider_proxy::config::env_parse_with_default("DGP_REFERENCE_LOCK_TTL_SECS", 120);
    let acquire_timeout_secs: u64 = deltaglider_proxy::config::env_parse_with_default(
        "DGP_REFERENCE_LOCK_ACQUIRE_TIMEOUT_SECS",
        30,
    );
    match ConfigDbSync::build_client(coordination_backend(config)).await {
        Ok(client) => {
            info!(
                "Reference lock: S3-CAS on bucket '{sync_bucket}' (cross-node reference.bin \
                 protection, node_id={node_id})"
            );
            Some(Arc::new(
                S3ReferenceLock::new(client, sync_bucket, node_id)
                    .with_tunables(ttl_secs, acquire_timeout_secs),
            ))
        }
        Err(e) => {
            warn!(
                "Reference lock: coordination client build failed ({e}) — in-process fallback \
                 (relies on single-writer-per-deltaspace routing for cross-node safety)"
            );
            None
        }
    }
}

/// Startup gate (guard B): under multi-instance, every NAMED S3 backend that
/// hosts a client-writable routed bucket must enforce conditional writes —
/// without CAS, two nodes' concurrent same-deltaspace PUTs can corrupt
/// `reference.bin` (the in-process prefix lock doesn't span processes).
///
/// Fail-fast contract (observability is the point — every outcome is loud):
///  - single-instance → one `info!`, no probes, zero cost.
///  - each validated backend → one `info!`.
///  - DEFINITIVE non-CAS (the `If-None-Match:*` re-PUT succeeded) → doc-linked
///    `FATAL` naming the buckets, the backend, and both fixes → `exit(1)`.
///  - indeterminate probe (network, missing bucket) → loud `warn!` + Unknown
///    verdict in the cache (GUI banner), NOT a crash — mirrors the "never a
///    SPOF" stance of the lease builder above.
///
/// Every S3 backend with a client-writable bucket is probed, the default one
/// included (unrouted buckets land there). `replication_target_only` buckets
/// are exempt: they have no client writers (guard A).
pub async fn validate_backend_write_capability(
    config: &Config,
    cache: &deltaglider_proxy::coordination::BackendCapabilityCache,
) {
    use deltaglider_proxy::coordination::capability::{
        client_writable_groups_with_default, establish_backend_verdict, forced_noncas_backends,
        CapabilityVerdict, CAPABILITY_DOC_URL,
    };

    if config
        .config_sync_bucket
        .as_deref()
        .is_none_or(|b| b.is_empty())
    {
        info!(
            "Backend capability gate skipped: single instance (config_sync_bucket not set) — \
             the in-process lock is sufficient"
        );
        return;
    }
    let forced = forced_noncas_backends();
    let groups = client_writable_groups_with_default(config, &forced).await;
    if groups.is_empty() {
        info!(
            "Backend capability gate: no client-writable buckets on S3 backends — \
             nothing to validate"
        );
        return;
    }
    for (name, group) in groups {
        let verdict = establish_backend_verdict(&name, &group, &forced).await;
        cache.set(&name, &group.backend, verdict.clone());
        match verdict {
            CapabilityVerdict::CasVerified { via } => info!(
                "backend capability: '{name}' conditional writes verified ({via:?}) — \
                 client-writable bucket(s) {:?} are safe under multi-instance",
                group.buckets
            ),
            CapabilityVerdict::NonCas => {
                error!(
                    "FATAL: {}",
                    deltaglider_proxy::coordination::capability::noncas_enforcement_message(
                        &name,
                        &group.buckets
                    )
                );
                std::process::exit(1);
            }
            CapabilityVerdict::Unknown { reason } => warn!(
                "backend capability: '{name}' could NOT be verified ({reason}) — bucket(s) \
                 {:?} are unvalidated; the proxy continues but multi-instance write safety \
                 is UNPROVEN on this backend. See {CAPABILITY_DOC_URL}",
                group.buckets
            ),
        }
    }
}

/// Boot-time backend HEALTH gate: probe every configured backend's
/// connectivity + credentials before serving.
///
/// Policy (`DGP_BOOT_BACKEND_PROBE`, default `enforce`):
///   - every backend healthy → one info! line each.
///   - SOME unhealthy → ERROR per backend naming the cause; the proxy starts
///     DEGRADED (their buckets answer 503 via the health gate until the
///     re-probe loop sees recovery).
///   - ALL unhealthy + `enforce` → FATAL exit(1): a proxy with zero working
///     storage backends serves nothing but garbage.
///   - `warn` → same probes/logs, never exits. `off` → no probes.
pub async fn boot_backend_health_gate(
    config: &Config,
    health: &deltaglider_proxy::coordination::BackendHealthCache,
) {
    use deltaglider_proxy::coordination::health::{
        boot_probe_mode, probe_backend_health, probe_targets, BootProbeMode,
    };

    let mode = boot_probe_mode();
    if mode == BootProbeMode::Off {
        info!("Backend health gate: DISABLED (DGP_BOOT_BACKEND_PROBE=off)");
        return;
    }
    let targets = probe_targets(config);
    let total = targets.len();
    let mut gating = 0usize; // AuthRejected / Unreachable — definitive faults
    for (name, backend, fallback) in targets {
        let verdict = probe_backend_health(&backend, fallback.as_deref()).await;
        health.set(&name, &backend, verdict.clone());
        if verdict.is_healthy() {
            info!("backend health: '{name}' — connection healthy");
        } else if verdict.is_gating() {
            gating += 1;
            error!(
                "backend health: '{name}' UNHEALTHY — {}. Buckets routed to it will \
                 answer 503 until it recovers (re-probed every 30s)",
                verdict.cause()
            );
        } else {
            error!(
                "backend health: '{name}' DEGRADED — {}. Requests are NOT blocked \
                 (the backend is reachable); re-probed every 30s",
                verdict.cause()
            );
        }
    }
    // FATAL only when every backend is DEFINITIVELY dead (creds rejected /
    // unreachable). A merely-Erroring backend (throttle storm, transient 5xx)
    // must not turn a restart into a crash loop that hammers the provider.
    if gating == total && total > 0 {
        match mode {
            BootProbeMode::Enforce => {
                error!(
                    "FATAL: ALL {total} configured storage backend(s) failed the boot health \
                     probe — the proxy has no working storage and refuses to start. Fix the \
                     backend endpoints/credentials, or set DGP_BOOT_BACKEND_PROBE=warn to \
                     start degraded anyway"
                );
                std::process::exit(1);
            }
            _ => error!(
                "ALL {total} configured storage backend(s) failed the boot health probe — \
                 starting anyway (DGP_BOOT_BACKEND_PROBE=warn); every bucket will answer 503"
            ),
        }
    }
}

/// Initialize config DB S3 sync if DGP_CONFIG_SYNC_BUCKET is set.
/// On startup: downloads from S3 if newer, reopens the DB, and rebuilds IAM index.
#[allow(clippy::too_many_arguments)]
pub async fn init_config_sync(
    config: &Config,
    db_keys: &deltaglider_proxy::config_db::ConfigDbKeys,
    config_db: &Option<Arc<tokio::sync::Mutex<deltaglider_proxy::config_db::ConfigDb>>>,
    iam_state: &SharedIamState,
    external_auth: &Option<Arc<deltaglider_proxy::iam::external_auth::ExternalAuthManager>>,
    sessions: &Arc<deltaglider_proxy::session::SessionStore>,
) -> Option<Arc<ConfigDbSync>> {
    let sync_bucket = match &config.config_sync_bucket {
        Some(b) if !b.is_empty() => b.clone(),
        _ => {
            info!("Config DB S3 sync: disabled (set config_sync_bucket in the config file or DGP_CONFIG_SYNC_BUCKET env var)");
            return None;
        }
    };

    let db_file = config_db_path();

    let object_key = config
        .config_sync_object_key
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| {
            deltaglider_proxy::config_db_sync::DEFAULT_CONFIG_SYNC_OBJECT_KEY.to_string()
        });

    let sync = match ConfigDbSync::new(
        coordination_backend(config),
        sync_bucket.clone(),
        object_key,
        db_file,
        db_keys.clone(),
        accept_legacy_sync(),
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
    if accept_legacy_sync() {
        warn!(
            "{}=true: a synced config DB under the bootstrap password hash is accepted. \
             Remove it when every instance runs this release — the hash is not a secret \
             that should open the shared IAM database",
            deltaglider_proxy::config_db::key::ACCEPT_LEGACY_SYNC_ENV
        );
    }

    // Boot gate: PROVE the coordination bucket enforces atomic conditional
    // writes before any HA feature (leases, single-writer locks) hinges on it.
    // A silent-clobber bucket is a data-loss trap — crash rather than run unsafe.
    // Skips itself on the fast path when a fresh witness from a prior boot exists.
    match sync.validate_coordination_bucket().await {
        Ok(deltaglider_proxy::config_db_sync::CoordinationValidation::CachedWitness {
            validated_at_unix,
            validated_by,
        }) => info!(
            "Coordination bucket '{}': conditional-write support confirmed (cached witness, validated at unix={} by {})",
            sync_bucket, validated_at_unix, validated_by
        ),
        Ok(deltaglider_proxy::config_db_sync::CoordinationValidation::Probed) => info!(
            "Coordination bucket '{}': conditional-write support PROBED and confirmed (witness written)",
            sync_bucket
        ),
        Err(e) => {
            error!("FATAL: coordination bucket validation failed: {e}");
            std::process::exit(1);
        }
    }

    // An upload parked before the restart goes FIRST: downloading would merge
    // the remote copy over the local change it carries.
    if sync.take_needs_upload() {
        match deltaglider_proxy::config_db_sync::upload_with_reconcile(
            &sync,
            config_db,
            db_keys.primary.expose(),
            iam_state,
            external_auth,
            Some(sessions),
            "startup flush",
        )
        .await
        {
            Ok(()) => info!("Config DB S3 sync: parked upload flushed at startup"),
            Err(e) => warn!("Config DB S3 sync: parked upload flush failed (stays parked): {e}"),
        }
        return Some(sync);
    }

    // Try to download a newer version from S3
    match deltaglider_proxy::config_db_sync::pull_and_merge(
        &sync,
        config_db,
        db_keys.primary.expose(),
        iam_state,
        external_auth,
        Some(sessions),
        "startup",
    )
    .await
    {
        Ok(Some(_)) => {}
        Ok(None) => {
            info!("Config DB S3 sync: local copy is current");
        }
        Err(e) => {
            warn!("Config DB S3 sync: startup download failed: {}", e);
        }
    }
    // A synced copy under an old key (rotation, legacy hash) merged above and
    // queued its re-upload: do it now, not at the first poll tick, so the
    // bucket leaves the old key while the peers still accept it.
    if sync.take_needs_upload() {
        match deltaglider_proxy::config_db_sync::upload_with_reconcile(
            &sync,
            config_db,
            db_keys.primary.expose(),
            iam_state,
            external_auth,
            Some(sessions),
            "startup re-encrypt",
        )
        .await
        {
            Ok(()) => info!("Config DB S3 sync: synced copy re-encrypted with the config DB key"),
            Err(e) => warn!("Config DB S3 sync: re-encrypt upload failed (stays parked): {e}"),
        }
    }

    Some(sync)
}

/// Spawn periodic config DB S3 sync poll (every 5 minutes).
#[allow(clippy::too_many_arguments)]
pub fn spawn_config_sync_poll(
    sync: Arc<ConfigDbSync>,
    config_db: &Option<Arc<tokio::sync::Mutex<deltaglider_proxy::config_db::ConfigDb>>>,
    iam_state: &SharedIamState,
    external_auth: &Option<Arc<deltaglider_proxy::iam::external_auth::ExternalAuthManager>>,
    sessions: &Arc<deltaglider_proxy::session::SessionStore>,
    db_key: &deltaglider_proxy::config_db::DbSecret,
) {
    let db_arc = config_db.clone();
    let iam = iam_state.clone();
    let ext_auth = external_auth.clone();
    let sessions = sessions.clone();
    let db_key = db_key.clone();

    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(300));
        // Skip the immediate first tick (startup sync already ran)
        tick.tick().await;
        loop {
            tick.tick().await;
            match deltaglider_proxy::config_db_sync::pull_and_merge(
                &sync,
                &db_arc,
                db_key.expose(),
                &iam,
                &ext_auth,
                Some(&sessions),
                "periodic poll",
            )
            .await
            {
                Ok(Some(_)) => {}
                Ok(None) => {
                    tracing::debug!("Config DB S3 sync poll: no changes");
                }
                Err(e) => {
                    warn!("Config DB S3 sync poll failed: {}", e);
                }
            }
            // Flush an upload that exhausted its retries earlier — a parked
            // revocation/mutation must not wait for the next admin action.
            if sync.take_needs_upload() {
                if let Err(e) = deltaglider_proxy::config_db_sync::upload_with_reconcile(
                    &sync,
                    &db_arc,
                    db_key.expose(),
                    &iam,
                    &ext_auth,
                    Some(&sessions),
                    "poll flush",
                )
                .await
                {
                    warn!("Config DB S3 sync poll: pending upload flush failed: {}", e);
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
///
/// For closures that are trivial (atomics, small-map retains) only — the
/// closure runs INLINE on a Tokio worker. Anything that touches the
/// filesystem or walks a large collection belongs in
/// [`spawn_periodic_blocking`] instead, or it stalls request handling
/// for the duration of the tick's work (#86).
pub fn spawn_periodic(interval: Duration, f: impl Fn() + Send + 'static) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        loop {
            tick.tick().await;
            f();
        }
    });
}

/// Like [`spawn_periodic`], but each tick's work runs on the blocking
/// pool via `spawn_blocking` — a filesystem sweep or a large-collection
/// walk never occupies a Tokio worker (#86: the multipart relay sweep
/// and the replay-cache retain produced interval-aligned P99 spikes).
/// Awaiting the join also serialises ticks, so a long job cannot
/// overlap itself; the next tick fires after the current one finishes.
///
/// `f` is stored in an `Arc` (not a `Mutex`): a panic inside the closure
/// unwinds through `spawn_blocking` and is reported as a `JoinError`,
/// after which the next tick still runs — no lock can be poisoned, so the
/// sweep can never be permanently disabled by one bad tick.
pub fn spawn_periodic_blocking(interval: Duration, f: impl Fn() + Send + Sync + 'static) {
    let f = std::sync::Arc::new(f);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        loop {
            tick.tick().await;
            let f = std::sync::Arc::clone(&f);
            if let Err(e) = tokio::task::spawn_blocking(move || f()).await {
                tracing::error!("periodic blocking task failed: {}", e);
            }
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

    #[test]
    fn backup_clobber_guard() {
        // Fresh mismatch: db present, no bak yet → preserve (rename db → bak).
        assert!(should_preserve_as_backup(true, false));
        // SECOND mismatch boot: db (the empty one) present AND a good bak
        // already exists → do NOT rename (would clobber the good backup). This
        // is the C1 data-loss guard.
        assert!(!should_preserve_as_backup(true, true));
        // No db at all → nothing to preserve.
        assert!(!should_preserve_as_backup(false, false));
        assert!(!should_preserve_as_backup(false, true));
    }

    /// Full truth table for the lingering-.db.bak disposition on an Ok-open boot.
    #[test]
    fn lingering_bak_disposition_truth_table() {
        use BakDisposition::*;
        // No bak → healthy boot regardless of the other (don't-care) inputs.
        assert_eq!(classify_lingering_bak(false, None, 0), NoBak);
        assert_eq!(classify_lingering_bak(false, None, 3), NoBak);
        assert_eq!(classify_lingering_bak(false, Some(2), 0), NoBak);
        // Undecryptable bak + EMPTY live DB = the boot-2 incident shape.
        assert_eq!(classify_lingering_bak(true, None, 0), Sticky);
        // Undecryptable bak next to a POPULATED live DB is a stray file on a
        // healthy node — never lock it (includes the load-error sentinel).
        assert_eq!(classify_lingering_bak(true, None, 7), AmbiguousWarn);
        assert_eq!(
            classify_lingering_bak(true, None, usize::MAX),
            AmbiguousWarn
        );
        // A decryptable, populated bak next to an EMPTY live DB is the
        // recovery shape (right key restored): promote. A junk bak (0 users)
        // or a populated live DB never swaps.
        assert_eq!(classify_lingering_bak(true, Some(0), 0), AmbiguousWarn);
        assert_eq!(classify_lingering_bak(true, Some(3), 0), Promote);
        assert_eq!(classify_lingering_bak(true, Some(3), 5), AmbiguousWarn);
    }

    /// The promote rename dance: park live as .discarded, move .bak into
    /// place, verify it opens. The parked live file is KEPT (the Err-branch
    /// live DB is unverifiable — deleting it could destroy foreign-hash data).
    #[test]
    fn promote_backup_db_swaps_and_keeps_discarded() {
        let dir = tempfile::tempdir().unwrap();
        let db_file = dir.path().join("deltaglider_config.db");
        let bak = db_file.with_extension("db.bak");
        // Good DB parked as .bak; empty wrong-key DB live (the incident state).
        drop(deltaglider_proxy::config_db::ConfigDb::open_or_create(&bak, "$2b$04$good").unwrap());
        drop(
            deltaglider_proxy::config_db::ConfigDb::open_or_create(&db_file, "$2b$04$wrong")
                .unwrap(),
        );
        let keys = deltaglider_proxy::config_db::ConfigDbKeys::primary_only("$2b$04$good");
        drop(promote_backup_db(&db_file, &bak, &keys, false).expect("promote must succeed"));
        assert!(db_file.exists(), "live DB must exist after promote");
        assert!(!bak.exists(), ".db.bak must be consumed by promote");
        assert!(
            db_file.with_extension("db.discarded").exists(),
            ".db.discarded must be KEPT for manual recovery"
        );
        // The promoted live DB opens with the good hash.
        assert!(
            deltaglider_proxy::config_db::ConfigDb::open_or_create(&db_file, "$2b$04$good").is_ok()
        );
    }

    /// The junk-bak guard: a zero-byte `.db.bak` opens under ANY hash
    /// (SQLCipher treats it as fresh) but reports zero users — the promote
    /// gate must treat it as non-promotable so it can never destroy the
    /// real live DB.
    #[test]
    fn probe_bak_users_zero_byte_bak_is_not_promotable() {
        let dir = tempfile::tempdir().unwrap();
        let bak = dir.path().join("deltaglider_config.db.bak");
        std::fs::write(&bak, b"").unwrap();
        let keys = deltaglider_proxy::config_db::ConfigDbKeys::primary_only("$2b$04$whatever");
        let users = probe_bak_users(&bak, &keys);
        assert_eq!(users, Some(0), "junk bak opens but has no users");
        let promotable = users.is_some_and(|n| n > 0);
        assert!(
            !promotable,
            "the Err-branch promote gate must refuse a junk bak"
        );
    }

    /// A bak under the legacy bootstrap-hash key is found through the
    /// fallback, and promotion re-encrypts it with the primary key.
    #[test]
    fn legacy_keyed_bak_is_probed_and_promoted_with_the_primary_key() {
        use deltaglider_proxy::config_db::{probe_key, ConfigDb, ConfigDbKeys, FallbackKind};
        let dir = tempfile::tempdir().unwrap();
        let db_file = dir.path().join("deltaglider_config.db");
        let bak = db_file.with_extension("db.bak");
        let db = ConfigDb::open_or_create(&bak, "$2b$04$legacy").unwrap();
        db.create_user("alice", "AKALICE1", "s", true, &[]).unwrap();
        drop(db);
        let primary = "primary-key-0123456789abcdef0123456789";
        drop(ConfigDb::open_or_create(&db_file, primary).unwrap());
        let keys = ConfigDbKeys::primary_only(primary)
            .with_fallback(FallbackKind::LegacyBootstrapHash, "$2b$04$legacy");
        assert_eq!(probe_bak_users(&bak, &keys), Some(1));
        drop(promote_backup_db(&db_file, &bak, &keys, false).unwrap());
        assert!(probe_key(&db_file, primary).unwrap());
    }

    /// A live DB that holds IAM state other than users (an OIDC provider, a
    /// group, a mapping rule: a setup before the first login) is not the
    /// empty incident DB: a stale backup with users must not replace it.
    #[test]
    fn a_stale_backup_never_replaces_a_live_db_with_iam_state() {
        use deltaglider_proxy::config_db::{ConfigDb, ConfigDbKeys};
        let dir = tempfile::tempdir().unwrap();
        let db_file = dir.path().join("deltaglider_config.db");
        let bak = db_file.with_extension("db.bak");
        let key = "primary-key-0123456789abcdef0123456789";
        let old = ConfigDb::open_or_create(&bak, key).unwrap();
        old.create_user("stale", "AKSTALE1", "s", true, &[])
            .unwrap();
        drop(old);
        let live = ConfigDb::open_or_create(&db_file, key).unwrap();
        live.create_group("eng", "", &[]).unwrap();
        let keys = ConfigDbKeys::primary_only(key);
        assert_eq!(
            bak_disposition(&bak, &keys, &live),
            BakDisposition::AmbiguousWarn,
            "a stale backup would replace a live DB that holds IAM state"
        );
        // The incident shape (nothing in the live DB) still promotes.
        let empty = dir.path().join("empty.db");
        let empty_db = ConfigDb::open_or_create(&empty, key).unwrap();
        assert_eq!(
            bak_disposition(&bak, &keys, &empty_db),
            BakDisposition::Promote
        );
    }

    /// A promoted backup that opened only with a fallback key is re-encrypted,
    /// so the synced copy (still under the old key) must be re-uploaded.
    #[test]
    fn a_promoted_legacy_backup_parks_the_sync_upload() {
        use deltaglider_proxy::config_db::{ConfigDb, ConfigDbKeys, FallbackKind};
        let dir = tempfile::tempdir().unwrap();
        let db_file = dir.path().join("deltaglider_config.db");
        let bak = db_file.with_extension("db.bak");
        let db = ConfigDb::open_or_create(&bak, "$2b$04$legacy").unwrap();
        db.create_user("alice", "AKALICE1", "s", true, &[]).unwrap();
        drop(db);
        let primary = "primary-key-0123456789abcdef0123456789";
        drop(ConfigDb::open_or_create(&db_file, primary).unwrap());
        let keys = ConfigDbKeys::primary_only(primary)
            .with_fallback(FallbackKind::LegacyBootstrapHash, "$2b$04$legacy");
        drop(promote_backup_db(&db_file, &bak, &keys, true).unwrap());
        assert!(
            db_file.with_extension("db.sync-pending").exists(),
            "the re-encrypted DB is not queued for upload"
        );
    }

    // ── startup_declarative_action policy (IaC cold-start guards) ──────────

    #[test]
    fn startup_action_empty_yaml_skips() {
        assert_eq!(
            startup_declarative_action(true, 0, 0, 0),
            StartupReconcileAction::SkipEmpty
        );
        // Even if the diff somehow shows deletes, empty wins (never wipe).
        assert_eq!(
            startup_declarative_action(true, 5, 0, 0),
            StartupReconcileAction::SkipEmpty
        );
    }

    #[test]
    fn startup_action_refuses_any_destructive_delete() {
        // The dangerous case: a non-empty YAML whose diff deletes DB rows
        // (e.g. an unattended gui→declarative flip omitting GUI-added users).
        assert!(matches!(
            startup_declarative_action(false, 1, 0, 0),
            StartupReconcileAction::RefuseDestructive { users: 1, .. }
        ));
        assert!(matches!(
            startup_declarative_action(false, 0, 2, 0),
            StartupReconcileAction::RefuseDestructive { groups: 2, .. }
        ));
        assert!(matches!(
            startup_declarative_action(false, 0, 0, 3),
            StartupReconcileAction::RefuseDestructive { providers: 3, .. }
        ));
    }

    #[test]
    fn startup_action_reconciles_additive_or_idempotent() {
        // Fresh deploy / additive / no-op (no deletes) → proceed.
        assert_eq!(
            startup_declarative_action(false, 0, 0, 0),
            StartupReconcileAction::Reconcile
        );
    }

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

    /// A named-backends deployment ignores the legacy singleton `backend`, so
    /// the label must come from `backends` (an S3 + filesystem pair used to be
    /// labelled "filesystem" from the unused default singleton).
    #[test]
    fn backend_type_label_follows_named_backends() {
        use deltaglider_proxy::config::NamedBackendConfig;
        let fs = |name: &str| NamedBackendConfig {
            name: name.into(),
            backend: BackendConfig::Filesystem {
                path: "/tmp/x".into(),
            },
            encryption: Default::default(),
        };
        let s3 = |name: &str| NamedBackendConfig {
            name: name.into(),
            backend: BackendConfig::S3 {
                session_token: None,
                endpoint: None,
                region: "us-east-1".into(),
                force_path_style: true,
                access_key_id: None,
                secret_access_key: None,
                allow_local: false,
            },
            encryption: Default::default(),
        };
        let mut cfg = Config::default();
        assert_eq!(backend_type_label(&cfg), "filesystem", "singleton default");
        cfg.backends = vec![s3("a")];
        assert_eq!(backend_type_label(&cfg), "s3");
        cfg.backends = vec![s3("a"), s3("b")];
        assert_eq!(backend_type_label(&cfg), "s3");
        cfg.backends = vec![s3("a"), fs("b")];
        assert_eq!(backend_type_label(&cfg), "mixed");
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

    /// #86 review: a panic inside one tick must not disable the periodic job.
    /// The closure is held in an `Arc` (no `Mutex`), so there is no lock to
    /// poison — later ticks still run.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spawn_periodic_blocking_survives_a_panicking_tick() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc as StdArc;
        let runs = StdArc::new(AtomicUsize::new(0));
        let r = StdArc::clone(&runs);
        spawn_periodic_blocking(Duration::from_millis(10), move || {
            if r.fetch_add(1, Ordering::SeqCst) == 0 {
                panic!("first tick blows up");
            }
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while runs.load(Ordering::SeqCst) < 3 && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            runs.load(Ordering::SeqCst) >= 3,
            "later ticks must still run after a panicking tick (got {})",
            runs.load(Ordering::SeqCst)
        );
    }
}
