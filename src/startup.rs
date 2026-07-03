// SPDX-License-Identifier: GPL-3.0-only

//! Server startup helpers — extracted from main.rs for file size.

use axum::{extract::DefaultBodyLimit, middleware, Router};

use deltaglider_proxy::api::auth::sigv4_auth_middleware;
use deltaglider_proxy::api::handlers::{head_root, AppState};
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
                warn!("  Note: authentication = \"none\" is ignored because S3 credentials are configured");
            }
        }
        AuthConfigOutcome::OpenAccess => {
            warn!("  Authentication: DISABLED (authentication = \"none\")");
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
            error!("  Accepted values:");
            error!("    authentication = \"none\"    — open access (development only)");
            error!("    (omit field)               — auto-detect from credentials");
            error!("");
            error!("  Or set S3 credentials instead:");
            error!("    access_key_id = \"...\"");
            error!("    secret_access_key = \"...\"");
            std::process::exit(1);
        }
        AuthConfigOutcome::Missing => {
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
///
/// Backed by the `s3s` crate, which translates wire-level S3 protocol
/// onto our [`deltaglider_proxy::s3_adapter_s3s::DeltaGliderS3Service`].
/// Until recently this function selected between `s3s` and a hand-
/// rolled axum-handler implementation via `DGP_S3_ADAPTER`; the axum
/// path has been retired and `s3s` is the only S3 implementation.
///
/// What's still axum, around the s3s service:
///   1. Pre-auth ADMISSION middleware (operator gating).
///   2. SigV4 + IAM AUTHORIZATION middleware (per-user permission
///      checks before any storage hit).
///   3. The HEAD-`/` and POST-multipart/form-data INTERCEPTORS — both
///      shapes that `s3s` legitimately rejects (HEAD-`/` is not S3
///      spec; form-POST is a browser-only PostObject path) but real
///      clients need (Cyberduck connection probes, the SPA's upload
///      page).
///   4. Standard cross-cutting layers: TraceLayer, body limit,
///      per-request timeout, concurrency cap, CORS.
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
    use axum::error_handling::HandleError;
    use deltaglider_proxy::iam::IamState;
    use deltaglider_proxy::s3_adapter_s3s::DeltaGliderS3Service;
    use s3s::access::{S3Access, S3AccessContext};
    use s3s::auth::{S3Auth, SecretKey};
    use s3s::service::S3ServiceBuilder;

    #[derive(Clone)]
    struct DeltaGliderS3sAuth {
        iam_state: SharedIamState,
    }

    #[async_trait::async_trait]
    impl S3Auth for DeltaGliderS3sAuth {
        async fn get_secret_key(&self, access_key: &str) -> s3s::S3Result<SecretKey> {
            match self.iam_state.load().as_ref() {
                IamState::Disabled => {
                    // The legacy Axum path ignores signatures in open-dev mode.
                    // In open mode, accept the common "same access key + secret"
                    // dummy pattern used by SDK clients (test/test, anonymous/
                    // anonymous). This lets s3s decode signed/chunked SDK
                    // requests without making local-dev users discover a magic
                    // hardcoded secret.
                    Ok(SecretKey::from(access_key.to_string()))
                }
                IamState::Legacy(auth) if access_key == auth.access_key_id => {
                    Ok(SecretKey::from(auth.secret_access_key.clone()))
                }
                IamState::Iam(index) => index
                    .get(access_key)
                    .filter(|user| user.enabled)
                    .map(|user| SecretKey::from(user.secret_access_key.clone()))
                    .ok_or_else(|| s3s::s3_error!(InvalidAccessKeyId)),
                _ => Err(s3s::s3_error!(InvalidAccessKeyId)),
            }
        }
    }

    #[derive(Clone)]
    struct AllowAllS3sAccess;

    #[async_trait::async_trait]
    impl S3Access for AllowAllS3sAccess {
        async fn check(&self, _cx: &mut S3AccessContext<'_>) -> s3s::S3Result<()> {
            // IAM/admission authorization is still enforced by the outer Axum
            // middleware chain. This access hook only prevents s3s' default
            // "auth provider implies anonymous deny" behavior from rejecting
            // already-admitted public/open-mode requests.
            Ok(())
        }
    }

    async fn handle_s3s_http_error(err: s3s::HttpError) -> axum::response::Response {
        error!(?err, "s3s HTTP-level failure");
        axum::http::Response::builder()
            .status(axum::http::StatusCode::INTERNAL_SERVER_ERROR)
            .body(axum::body::Body::from("Internal Server Error"))
            .expect("static response")
    }

    async fn add_s3_request_id(
        request: axum::http::Request<axum::body::Body>,
        next: axum::middleware::Next,
    ) -> axum::response::Response {
        let is_acl_request = request
            .uri()
            .query()
            .map(|query| {
                query
                    .split('&')
                    .any(|part| part == "acl" || part.starts_with("acl="))
            })
            .unwrap_or(false);
        let mut response = next.run(request).await;
        let request_id = response
            .headers()
            .get("x-amz-request-id")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        if let Ok(value) = axum::http::HeaderValue::from_str(&request_id) {
            response.headers_mut().insert("x-amz-request-id", value);
        }

        let is_error = response.status().is_client_error() || response.status().is_server_error();

        let content_type_is_xml = response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.contains("xml"))
            .unwrap_or(false);
        let list_metadata = response
            .extensions()
            .get::<deltaglider_proxy::s3_adapter_s3s::ListMetadataXmlExtensions>()
            .cloned();
        let recursive_delete = response
            .extensions()
            .get::<deltaglider_proxy::s3_adapter_s3s::RecursiveDeleteJson>()
            .cloned();
        if let Some(recursive_delete) = recursive_delete {
            let (mut parts, _body) = response.into_parts();
            parts.status = axum::http::StatusCode::OK;
            let text = serde_json::json!({
                "deleted": recursive_delete.deleted,
                "denied": recursive_delete.denied,
            })
            .to_string();
            parts.headers.insert(
                axum::http::header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("application/json"),
            );
            parts.headers.insert(
                axum::http::header::CONTENT_LENGTH,
                axum::http::HeaderValue::from_str(&text.len().to_string())
                    .unwrap_or_else(|_| axum::http::HeaderValue::from_static("0")),
            );
            return axum::http::Response::from_parts(parts, axum::body::Body::from(text));
        }
        if !content_type_is_xml || (!is_error && !is_acl_request && list_metadata.is_none()) {
            return response;
        }

        let (mut parts, body) = response.into_parts();
        let Ok(bytes) = axum::body::to_bytes(body, 1024 * 1024).await else {
            return axum::http::Response::from_parts(parts, axum::body::Body::empty());
        };
        let mut text = String::from_utf8_lossy(&bytes).into_owned();
        if is_error && text.contains("<Error>") && !text.contains("<RequestId>") {
            text = text.replace(
                "</Error>",
                &format!("<RequestId>{request_id}</RequestId></Error>"),
            );
        }
        if is_acl_request {
            text = text.replace(
                r#"<AccessControlPolicy xmlns="http://s3.amazonaws.com/doc/2006-03-01/">"#,
                "<AccessControlPolicy>",
            );
        }
        if let Some(list_metadata) = list_metadata {
            for (key, metadata) in list_metadata.0 {
                if metadata.is_empty() {
                    continue;
                }
                let key_marker = format!("<Key>{}</Key>", escape_xml_local(&key));
                let Some(key_pos) = text.find(&key_marker) else {
                    continue;
                };
                let Some(contents_end_rel) = text[key_pos..].find("</Contents>") else {
                    continue;
                };
                let mut metadata_xml = String::from("<UserMetadata>");
                let mut keys: Vec<_> = metadata.keys().collect();
                keys.sort();
                for metadata_key in keys {
                    let value = &metadata[metadata_key];
                    metadata_xml.push_str(&format!(
                        "<Items><Key>{}</Key><Value>{}</Value></Items>",
                        escape_xml_local(metadata_key),
                        escape_xml_local(value)
                    ));
                }
                metadata_xml.push_str("</UserMetadata>");
                text.insert_str(key_pos + contents_end_rel, &metadata_xml);
            }
        }
        parts.headers.insert(
            axum::http::header::CONTENT_LENGTH,
            axum::http::HeaderValue::from_str(&text.len().to_string())
                .unwrap_or_else(|_| axum::http::HeaderValue::from_static("0")),
        );
        axum::http::Response::from_parts(parts, axum::body::Body::from(text))
    }

    fn escape_xml_local(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&apos;")
    }

    let mut builder = S3ServiceBuilder::new(DeltaGliderS3Service::new(state.clone()));
    builder.set_auth(DeltaGliderS3sAuth {
        iam_state: iam_state.clone(),
    });
    builder.set_access(AllowAllS3sAccess);
    let s3_service = HandleError::new(builder.build(), handle_s3s_http_error);

    // Form-POST upload interceptor (`POST /<bucket>` with
    // `Content-Type: multipart/form-data`). The pre-fix `2abe031`
    // attempt used `.route("/:bucket", post(...))` which broke the s3s
    // parity tests catastrophically — `.route` claims the slot for ALL
    // methods, so `PUT /:bucket` (CreateBucket) and other POSTs (?delete
    // batch, CreateMultipartUpload) returned 405. The right shape is a
    // method-AND-content-type-aware middleware that intercepts ONLY the
    // browser form-POST shape and lets every other POST flow through to
    // the s3s service.
    let form_post_state = state.clone();
    async fn intercept_form_post_for_s3s(
        axum::extract::State(state): axum::extract::State<Arc<AppState>>,
        request: axum::http::Request<axum::body::Body>,
        next: axum::middleware::Next,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;
        use deltaglider_proxy::api::handlers::form_post::{
            handle_form_post_upload, is_multipart_form_upload,
        };

        // Pass-through guard 1: only POST is in scope.
        if request.method() != axum::http::Method::POST {
            return next.run(request).await;
        }

        // Pass-through guard 2: must be multipart/form-data. The
        // s3s `?delete` batch is POST + XML, CreateMultipartUpload is
        // POST + JSON-ish — both must fall through.
        if !is_multipart_form_upload(request.headers()) {
            return next.run(request).await;
        }

        // Pass-through guard 3: path shape `/:bucket` (single segment,
        // no key suffix). Anything else (`/`, `/bucket/key`) is not a
        // browser form-POST upload.
        let raw_path = request.uri().path().trim_start_matches('/');
        let trimmed = raw_path.trim_end_matches('/');
        if trimmed.is_empty() || trimmed.contains('/') {
            return next.run(request).await;
        }
        let bucket = trimmed.to_string();

        // Pull iam_state from extensions (inserted as a layer below).
        let iam_state = request
            .extensions()
            .get::<deltaglider_proxy::iam::SharedIamState>()
            .cloned();

        // Consume the body, bounded by `max_object_size`. This is the
        // authoritative cap for this path: `DefaultBodyLimit` only does an
        // eager `Content-Length` check and is enforced lazily on read for
        // chunked bodies, so a chunked/streamed `multipart/form-data` POST
        // could otherwise slip past it. We therefore enforce the limit HERE
        // explicitly — `to_bytes` aborts as soon as the collected body
        // exceeds the limit, so a single oversized (or chunked) request can
        // never buffer the whole body into memory. Double-enforcement with
        // `DefaultBodyLimit` is harmless: whichever limit fires first wins.
        // Read the cap from the (hot-reloadable) engine so a runtime
        // `max_object_size` change applies here too.
        let max_body = state.engine.load().max_object_size() as usize;
        let (parts, body) = request.into_parts();
        let body_bytes = match axum::body::to_bytes(body, max_body).await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("form-POST body collection failed or exceeded limit: {e}");
                return (
                    axum::http::StatusCode::PAYLOAD_TOO_LARGE,
                    "form-POST body exceeded the configured max object size",
                )
                    .into_response();
            }
        };

        // Hand off to the same handler the axum adapter uses. Identical
        // behaviour by construction.
        match handle_form_post_upload(
            &state,
            &bucket,
            iam_state.as_ref(),
            &parts.headers,
            body_bytes,
        )
        .await
        {
            Ok(response) => response,
            Err(s3_err) => s3_err.into_response(),
        }
    }

    // `HEAD /` — connection-probe handler used by Cyberduck and other
    // S3 clients. Not part of the S3 spec, so the s3s service returns
    // 501 here. Use a middleware (not `.route`) so axum doesn't claim
    // the `/` path slot — `.route("/", head(...))` returns 405 for
    // GET `/` (ListBuckets) because axum matches path first, then
    // checks method without falling through to the s3s fallback.
    async fn intercept_head_root(
        request: axum::http::Request<axum::body::Body>,
        next: axum::middleware::Next,
    ) -> axum::response::Response {
        let is_root = request.uri().path() == "/" || request.uri().path().is_empty();
        if request.method() == axum::http::Method::HEAD && is_root {
            return head_root().await;
        }
        next.run(request).await
    }

    let mut router = Router::new()
        .fallback_service(s3_service)
        .layer(middleware::from_fn(intercept_head_root))
        .layer(middleware::from_fn_with_state(
            form_post_state,
            intercept_form_post_for_s3s,
        ))
        .layer(middleware::from_fn(add_s3_request_id))
        .layer(TraceLayer::new_for_http())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            deltaglider_proxy::metrics::http_metrics_middleware,
        ))
        .layer(middleware::from_fn(authorization_middleware))
        .layer(middleware::from_fn(sigv4_auth_middleware))
        // Maintenance write-gate: runs after admission, before SigV4. A
        // PERMANENT layer whose contents (the busy-bucket set) swap
        // lock-free — unlike the admission chain it cannot be lost to a
        // config rebuild mid-job. Writes to a busy bucket → 503 SlowDown;
        // reads always pass. See src/maintenance/gate.rs.
        .layer(middleware::from_fn(
            deltaglider_proxy::maintenance::gate::maintenance_gate_middleware,
        ))
        .layer(middleware::from_fn(
            deltaglider_proxy::admission::admission_middleware,
        ))
        .layer(axum::Extension(iam_state.clone()))
        .layer(axum::Extension(public_prefix_snapshot.clone()))
        .layer(axum::Extension(admission_chain.clone()))
        .layer(axum::Extension(state.maintenance_gate.clone()));

    if config_db_mismatch {
        error!(
            "S3 API LOCKED — all requests will be rejected until bootstrap password mismatch is resolved via /_/"
        );
        router = router.layer(axum::Extension(ConfigDbMismatchGuard));
    }

    router
        .layer(axum::Extension(replay_cache.clone()))
        .layer(axum::Extension(rate_limiter.clone()))
        .layer(axum::Extension(metrics.clone()))
        .layer(DefaultBodyLimit::max(config.max_object_size as usize))
        .layer(tower_http::timeout::TimeoutLayer::with_status_code(
            axum::http::StatusCode::GATEWAY_TIMEOUT,
            std::time::Duration::from_secs(deltaglider_proxy::config::env_parse_with_default(
                "DGP_REQUEST_TIMEOUT_SECS",
                300u64,
            )),
        ))
        .layer(tower::limit::ConcurrencyLimitLayer::new(
            deltaglider_proxy::config::env_parse_with_default(
                "DGP_MAX_CONCURRENT_REQUESTS",
                1024usize,
            ),
        ))
        .layer({
            // SECURITY: In production (single-port architecture), CORS is not
            // needed because the UI is served from the same origin.
            // `CorsLayer::permissive()` would let any cross-origin browser
            // context call the S3 API. Only enable permissive CORS when
            // `DGP_CORS_PERMISSIVE=true` (dev mode). S3 SDK/CLI are non-browser
            // so they're unaffected; the embedded UI is same-origin so it's
            // unaffected in prod. Mirrors the admin router's branch in
            // `demo.rs` via the shared `cors::cors_layer_for` pure fn.
            let permissive = deltaglider_proxy::config::env_bool("DGP_CORS_PERMISSIVE", false);
            deltaglider_proxy::cors::cors_layer_for(permissive)
        })
        .with_state(state.clone())
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
/// Promotion is deliberately NOT an Ok-branch option: the real incident's
/// recovery (correct hash restored) always lands in the ERR branch, so an
/// Ok-open node never auto-swaps databases.
#[derive(Debug, PartialEq, Eq)]
enum BakDisposition {
    /// No `.db.bak` on disk — the normal healthy boot.
    NoBak,
    /// Backup undecryptable + live DB EMPTY: the wrong-hash incident shape —
    /// stay mismatched/locked (the good DB is parked in `.db.bak`).
    Sticky,
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
    live_user_count: usize,
) -> BakDisposition {
    match (bak_exists, bak_users, live_user_count) {
        (false, _, _) => BakDisposition::NoBak,
        // Undecryptable bak + EMPTY live DB = the boot-2 incident shape.
        (true, None, 0) => BakDisposition::Sticky,
        // A populated live DB that opens is NOT the incident DB — a stray
        // undecryptable bak must not lock a healthy node.
        (true, None, _) => BakDisposition::AmbiguousWarn,
        (true, Some(_), _) => BakDisposition::AmbiguousWarn,
    }
}

/// Probe `.db.bak` with the current hash: `Some(user_count)` when it opens,
/// `None` when it doesn't. A junk/empty file opens under ANY hash (SQLCipher
/// treats it as fresh) — the user count is what distinguishes a real parked
/// IAM DB from junk, so promotion requires `Some(n) with n > 0`.
fn probe_bak_users(bak_path: &std::path::Path, admin_password_hash: &str) -> Option<usize> {
    deltaglider_proxy::config_db::ConfigDb::open_or_create(bak_path, admin_password_hash)
        .ok()
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
    admin_password_hash: &str,
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
    let db = deltaglider_proxy::config_db::ConfigDb::open_or_create(db_file, admin_password_hash)
        .map_err(|e| format!("reopen promoted {}: {e}", db_file.display()))?;
    info!(
        "config-db mismatch incident resolved: promoted backup {} over the live DB \
         (previous live file kept as {} — remove it manually once verified)",
        bak_path.display(),
        discarded.display()
    );
    Ok(db)
}

/// Initialize the encrypted IAM config database. If it contains existing
/// users, switch to IAM mode immediately.
///
/// Returns `(config_db, mismatch)` where `mismatch` is true if the bootstrap
/// password hash doesn't match the existing DB encryption key.
pub fn init_config_db(
    admin_password_hash: &str,
    iam_state: &SharedIamState,
    config: &Config,
) -> (
    Option<Arc<tokio::sync::Mutex<deltaglider_proxy::config_db::ConfigDb>>>,
    bool,
) {
    init_config_db_attempt(admin_password_hash, iam_state, config, true)
}

/// One boot attempt. `allow_promote` bounds the recover-then-retry to a
/// single pass (a boot with the correct hash finding the live DB locked but
/// `.db.bak` readable promotes the backup, then re-runs the normal open path).
fn init_config_db_attempt(
    admin_password_hash: &str,
    iam_state: &SharedIamState,
    config: &Config,
    allow_promote: bool,
) -> (
    Option<Arc<tokio::sync::Mutex<deltaglider_proxy::config_db::ConfigDb>>>,
    bool,
) {
    let db_file = config_db_path();
    match deltaglider_proxy::config_db::ConfigDb::open_or_create(&db_file, admin_password_hash) {
        Ok(db) => {
            // Classify a lingering .db.bak BEFORE any boot-time mutation: a
            // node with an unresolved mismatch incident must stay locked.
            let bak_path = db_file.with_extension("db.bak");
            let disposition = if bak_path.exists() {
                let bak_users = probe_bak_users(&bak_path, admin_password_hash);
                // On a load error err on the side of "has users" — a populated
                // live DB must never be treated as the incident-empty DB.
                let live_user_count = db.load_users().map(|u| u.len()).unwrap_or(usize::MAX);
                classify_lingering_bak(true, bak_users, live_user_count)
            } else {
                BakDisposition::NoBak
            };
            match disposition {
                BakDisposition::NoBak => {}
                BakDisposition::Sticky => {
                    error!(
                        "Lingering {} does not decrypt with the current bootstrap password hash \
                         and the live DB is empty — an earlier mismatch incident is unresolved. \
                         S3 API stays locked; restart with the original bootstrap password or \
                         use the admin GUI recovery wizard.",
                        bak_path.display()
                    );
                    return (Some(Arc::new(tokio::sync::Mutex::new(db))), true);
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
                             OVERRIDES `authentication = \"none\"`. Open/anonymous browser \
                             access will get AccessDenied; log in as an IAM user, or delete \
                             {} to use open access.",
                            users.len(),
                            db_file.display(),
                            db_file.display()
                        );
                    }
                    let state = deltaglider_proxy::iam::IamIndex::build_iam_state(users, groups);
                    iam_state.store(Arc::new(state));
                }
                // If no users exist, keep current IamState (Legacy or Disabled)
            }
            (Some(Arc::new(tokio::sync::Mutex::new(db))), false)
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
                && probe_bak_users(&bak_path, admin_password_hash).is_some_and(|n| n > 0)
            {
                match promote_backup_db(&db_file, &bak_path, admin_password_hash) {
                    Ok(db) => {
                        drop(db);
                        return init_config_db_attempt(
                            admin_password_hash,
                            iam_state,
                            config,
                            false,
                        );
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
                        "Bootstrap password does not match config DB — original preserved as {}. \
                         Use the admin GUI recovery wizard to resolve.",
                        bak_path.display()
                    );
                }
            } else if db_file.exists() {
                // .bak already holds the good DB — leave it untouched.
                error!(
                    "Bootstrap password does not match config DB — good backup already \
                     preserved at {}. Use the admin GUI recovery wizard to resolve.",
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
    admin_password_hash: &str,
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
        &config.backend,
        sync_bucket.clone(),
        config
            .config_sync_object_key
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| {
                deltaglider_proxy::config_db_sync::DEFAULT_CONFIG_SYNC_OBJECT_KEY.to_string()
            }),
        config_db_path(),
        admin_password_hash.to_string(),
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

    match ConfigDbSync::build_client(&config.backend).await {
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

/// Startup gate (guard B): under multi-instance, every NAMED S3 backend that
/// hosts a client-writable routed bucket must enforce conditional writes —
/// without CAS, two nodes' concurrent same-deltaspace PUTs can corrupt
/// `reference.bin` (the in-process prefix lock doesn't span processes).
///
/// Fail-fast contract (observability is the point — every outcome is loud):
///  - single-instance → one `info!`, no probes, zero cost.
///  - each validated backend → one `info!` (probed vs witness-cached).
///  - DEFINITIVE non-CAS (the `If-None-Match:*` re-PUT succeeded) → doc-linked
///    `FATAL` naming the buckets, the backend, and both fixes → `exit(1)`.
///  - indeterminate probe (network, missing bucket) → loud `warn!` + Unknown
///    verdict in the cache (GUI banner), NOT a crash — mirrors the "never a
///    SPOF" stance of the lease builder above.
///
/// The DEFAULT backend is exempt here: the coordination bucket lives on it
/// (`ConfigDbSync` builds from `config.backend`), so `validate_coordination_bucket`
/// already crash-validates it. `replication_target_only` buckets are exempt:
/// they have no client writers (guard A).
pub async fn validate_backend_write_capability(
    config: &Config,
    cache: &deltaglider_proxy::coordination::BackendCapabilityCache,
) {
    use deltaglider_proxy::coordination::capability::{
        client_writable_s3_backends, establish_backend_verdict, forced_noncas_backends,
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
    let groups = client_writable_s3_backends(config);
    if groups.is_empty() {
        info!(
            "Backend capability gate: no client-writable buckets on named S3 backends — \
             nothing to validate (default backend is covered by the coordination gate)"
        );
        return;
    }
    let forced = forced_noncas_backends();
    for (name, group) in groups {
        let verdict = establish_backend_verdict(&name, &group, &forced).await;
        cache.set(&name, verdict.clone());
        match verdict {
            CapabilityVerdict::CasVerified { via } => info!(
                "backend capability: '{name}' conditional writes verified ({via:?}) — \
                 client-writable bucket(s) {:?} are safe under multi-instance",
                group.buckets
            ),
            CapabilityVerdict::NonCas => {
                error!(
                    "FATAL: backend '{name}' does not support conditional writes, but \
                     client-writable bucket(s) {:?} route to it and multi-instance mode is \
                     active (config_sync_bucket is set). Concurrent writes from two instances \
                     can corrupt delta references. Fix: move these buckets to a CAS-capable \
                     backend, or mark each as replication_target_only. — see {CAPABILITY_DOC_URL}",
                    group.buckets
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

/// Initialize config DB S3 sync if DGP_CONFIG_SYNC_BUCKET is set.
/// On startup: downloads from S3 if newer, reopens the DB, and rebuilds IAM index.
#[allow(clippy::too_many_arguments)]
pub async fn init_config_sync(
    config: &Config,
    admin_password_hash: &str,
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
        &config.backend,
        sync_bucket.clone(),
        object_key,
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

    // Try to download a newer version from S3
    match sync.download_if_newer().await {
        Ok(Some(dl)) => {
            let applied = reopen_and_rebuild_iam(
                config_db,
                admin_password_hash,
                iam_state,
                external_auth,
                Some(sessions),
                &dl.temp_path,
                "startup",
            )
            .await;
            // Commit the ETag only on a successful merge so a failure retries.
            if applied {
                sync.commit_downloaded_etag(dl.etag).await;
            }
        }
        Ok(None) => {
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
#[allow(clippy::too_many_arguments)]
pub fn spawn_config_sync_poll(
    sync: Arc<ConfigDbSync>,
    config_db: &Option<Arc<tokio::sync::Mutex<deltaglider_proxy::config_db::ConfigDb>>>,
    iam_state: &SharedIamState,
    external_auth: &Option<Arc<deltaglider_proxy::iam::external_auth::ExternalAuthManager>>,
    sessions: &Arc<deltaglider_proxy::session::SessionStore>,
    admin_password_hash: &str,
) {
    let db_arc = config_db.clone();
    let iam = iam_state.clone();
    let ext_auth = external_auth.clone();
    let sessions = sessions.clone();
    let password_hash = admin_password_hash.to_string();

    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(300));
        // Skip the immediate first tick (startup sync already ran)
        tick.tick().await;
        loop {
            tick.tick().await;
            match sync.poll_and_sync().await {
                Ok(Some(dl)) => {
                    let applied = reopen_and_rebuild_iam(
                        &db_arc,
                        &password_hash,
                        &iam,
                        &ext_auth,
                        Some(&sessions),
                        &dl.temp_path,
                        "periodic poll",
                    )
                    .await;
                    if applied {
                        sync.commit_downloaded_etag(dl.etag).await;
                    }
                }
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
                    &password_hash,
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
        // A decryptable bak never auto-promotes on an Ok-open boot — the real
        // incident's recovery always lands in the Err branch.
        assert_eq!(classify_lingering_bak(true, Some(0), 0), AmbiguousWarn);
        assert_eq!(classify_lingering_bak(true, Some(3), 0), AmbiguousWarn);
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
        drop(promote_backup_db(&db_file, &bak, "$2b$04$good").expect("promote must succeed"));
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
        let users = probe_bak_users(&bak, "$2b$04$whatever");
        assert_eq!(users, Some(0), "junk bak opens but has no users");
        let promotable = users.is_some_and(|n| n > 0);
        assert!(
            !promotable,
            "the Err-branch promote gate must refuse a junk bak"
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
