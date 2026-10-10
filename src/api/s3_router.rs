// SPDX-License-Identifier: BUSL-1.1

//! The S3 router: the s3s service and every axum layer around it.
//!
//! In the library (not `startup.rs`) so the middleware-vs-s3s contract test
//! (`api::s3s_contract_tests`) drives the exact production stack; only the
//! `S3` impl and the access hook are swappable, for recording.

use std::sync::Arc;

use axum::{extract::DefaultBodyLimit, middleware, Router};
use s3s::access::S3Access;
use tower_http::trace::TraceLayer;

use crate::api::auth::sigv4_auth_middleware;
use crate::api::handlers::{head_root, AppState};
use crate::api::ConfigDbMismatchGuard;
use crate::config::Config;
use crate::iam::{authorization_middleware, SharedIamState};
use crate::metrics::Metrics;
use crate::rate_limiter::RateLimiter;

/// Whether the request is an `?acl` subresource request, with the query
/// decoded as s3s decodes it (`%61cl` is `acl`).
fn is_acl_request(uri: &axum::http::Uri) -> bool {
    crate::api::request_target::RequestTarget::from_uri(uri).is_ok_and(|t| t.has_query("acl"))
}

/// Whether a response is a HEAD answer for a byte range. s3s applies
/// `S3Response.status` only to custom routes, so HeadObject with `Range`
/// left the adapter as 200 with a `Content-Range`; S3 answers 206
/// (s3surface-6). GetObject gets its 206 from s3s itself.
fn head_range_is_partial(
    method: &axum::http::Method,
    status: axum::http::StatusCode,
    headers: &axum::http::HeaderMap,
) -> bool {
    method == axum::http::Method::HEAD
        && status == axum::http::StatusCode::OK
        && headers.contains_key(axum::http::header::CONTENT_RANGE)
}

/// The per-backend share of the request slots (`DGP_BACKEND_SHARE_PERCENT`
/// of `DGP_MAX_CONCURRENT_REQUESTS`). One global cap served every bucket,
/// so the requests to a slow backend (slow, so it passes its health probe)
/// held every slot, and the requests to all other backends queued behind
/// them. With more than one backend, a request to a bucket takes a slot of
/// its backend's share, or gets 503 SlowDown at once when the share is
/// full; it then runs in its backend's spool scope (`spool::scoped`).
#[derive(Clone)]
struct BackendShare {
    /// Slots per backend.
    cap: usize,
    slots: Arc<parking_lot::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Semaphore>>>>,
    config: crate::config::SharedConfig,
    state: Arc<AppState>,
}

impl BackendShare {
    fn new(config: &Config, shared: &crate::config::SharedConfig, state: &Arc<AppState>) -> Self {
        let total = config.tuning.max_concurrent_requests;
        let percent = usize::from(config.tuning.backend_share_percent);
        Self {
            cap: (total.saturating_mul(percent) / 100).max(1),
            slots: Default::default(),
            config: shared.clone(),
            state: state.clone(),
        }
    }

    /// The backend of the bucket that `path` names, when more than one
    /// backend is configured. Never waits for the config lock (an apply
    /// holds it while it probes): while it is held, no share applies.
    fn backend_of(&self, path: &str) -> Option<String> {
        let bucket = crate::maintenance::gate::bucket_from_path(path)?;
        let cfg = self.config.try_read().ok()?;
        if cfg.backends.len() < 2 {
            return None;
        }
        use crate::storage::StorageBackend;
        self.state
            .engine
            .load()
            .storage()
            .resolved_backend_name(&bucket)
            .or_else(|| {
                cfg.effective_backend_for_bucket(&bucket)
                    .map(|(name, _)| name)
            })
    }

    fn semaphore(&self, backend: &str) -> Arc<tokio::sync::Semaphore> {
        self.slots
            .lock()
            .entry(backend.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(self.cap)))
            .clone()
    }
}

/// Middleware of [`BackendShare`].
async fn backend_share(
    axum::extract::State(share): axum::extract::State<BackendShare>,
    request: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let Some(backend) = share.backend_of(request.uri().path()) else {
        return next.run(request).await;
    };
    let Ok(_slot) = share.semaphore(&backend).try_acquire_owned() else {
        tracing::warn!(
            "backend '{backend}' holds its share of the request slots ({}); a request to it got 503 SlowDown",
            share.cap
        );
        // The message does not name the backend: the caller is not verified yet.
        let mut response = crate::api::S3Error::SlowDown(
            "Too many concurrent requests to the storage of this bucket; retry shortly".into(),
        )
        .into_response();
        crate::api::errors::ensure_retry_after(response.status(), response.headers_mut());
        return response;
    };
    crate::deltaglider::spool::scoped(&backend, next.run(request)).await
}

/// Build the S3-compatible router with all routes and middleware layers.
///
/// Backed by the `s3s` crate, which translates wire-level S3 protocol
/// onto our [`crate::s3_adapter_s3s::DeltaGliderS3Service`], the only S3
/// implementation.
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
///
/// The production S3 router: [`build_s3_router_with`] over the
/// [`DeltaGliderS3Service`](crate::s3_adapter_s3s::DeltaGliderS3Service)
/// and the [`VerifiedIdentityS3sAccess`](crate::api::s3s_hooks::VerifiedIdentityS3sAccess)
/// hook.
pub fn build_s3_router(deps: RouterDeps<'_>) -> Router {
    let service = crate::s3_adapter_s3s::DeltaGliderS3Service::new(
        deps.state.clone(),
        deps.shared_config.clone(),
    );
    build_s3_router_with(
        deps,
        service,
        crate::api::s3s_hooks::VerifiedIdentityS3sAccess,
    )
}

/// What the S3 router is built from: the shared state that every layer and
/// the s3s service read.
pub struct RouterDeps<'a> {
    pub state: &'a Arc<AppState>,
    pub iam_state: &'a SharedIamState,
    pub metrics: &'a Arc<Metrics>,
    pub rate_limiter: &'a RateLimiter,
    pub replay_cache: &'a crate::api::auth::ReplayCache,
    /// The config at build time (layer limits, CORS, timeouts).
    pub config: &'a Config,
    pub config_db_mismatch: bool,
    pub public_prefix_snapshot: &'a crate::bucket_policy::SharedPublicPrefixSnapshot,
    pub admission_chain: &'a crate::admission::SharedAdmissionChain,
    /// The live, hot-reloaded config.
    pub shared_config: &'a crate::config::SharedConfig,
}

/// [`build_s3_router`] with the `S3` impl and the s3s access hook given
/// (the contract test records through them).
pub fn build_s3_router_with<S, A>(deps: RouterDeps<'_>, s3: S, access: A) -> Router
where
    S: s3s::S3,
    A: S3Access,
{
    let RouterDeps {
        state,
        iam_state,
        metrics,
        rate_limiter,
        replay_cache,
        config,
        config_db_mismatch,
        public_prefix_snapshot,
        admission_chain,
        shared_config,
    } = deps;
    use crate::api::s3s_hooks::DeltaGliderS3sAuth;
    use axum::error_handling::HandleError;
    use s3s::service::S3ServiceBuilder;

    async fn handle_s3s_http_error(err: s3s::HttpError) -> axum::response::Response {
        tracing::error!(?err, "s3s HTTP-level failure");
        axum::http::Response::builder()
            .status(axum::http::StatusCode::INTERNAL_SERVER_ERROR)
            .body(axum::body::Body::from("Internal Server Error"))
            .expect("static response")
    }

    async fn add_s3_request_id(
        request: axum::http::Request<axum::body::Body>,
        next: axum::middleware::Next,
    ) -> axum::response::Response {
        let is_acl_request = is_acl_request(request.uri());
        let method = request.method().clone();
        let mut response = next.run(request).await;
        if head_range_is_partial(&method, response.status(), response.headers()) {
            *response.status_mut() = axum::http::StatusCode::PARTIAL_CONTENT;
        }
        let status = response.status();
        crate::api::errors::ensure_retry_after(status, response.headers_mut());
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
            .get::<crate::s3_adapter_s3s::ListMetadataXmlExtensions>()
            .cloned();
        if !content_type_is_xml || (!is_error && !is_acl_request && list_metadata.is_none()) {
            return response;
        }

        // A metadata=true page of 1000 long keys with their metadata is
        // several MiB. Above the cap the body is gone (to_bytes consumed
        // it): answer 500, never an empty 200 that reads as a broken list.
        const REWRITE_BODY_CAP: usize = 64 * 1024 * 1024;
        let (mut parts, body) = response.into_parts();
        let Ok(bytes) = axum::body::to_bytes(body, REWRITE_BODY_CAP).await else {
            tracing::error!("S3 response body over {REWRITE_BODY_CAP} bytes; cannot rewrite it");
            let text = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?><Error><Code>InternalError</Code>\
                 <Message>The response is too large to return.</Message>\
                 <RequestId>{request_id}</RequestId></Error>"
            );
            parts.status = axum::http::StatusCode::INTERNAL_SERVER_ERROR;
            parts.headers.remove(axum::http::header::CONTENT_LENGTH);
            return axum::http::Response::from_parts(parts, axum::body::Body::from(text));
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
            text = list_metadata.insert_into(&text);
        }
        parts.headers.insert(
            axum::http::header::CONTENT_LENGTH,
            axum::http::HeaderValue::from_str(&text.len().to_string())
                .unwrap_or_else(|_| axum::http::HeaderValue::from_static("0")),
        );
        axum::http::Response::from_parts(parts, axum::body::Body::from(text))
    }

    let mut builder = S3ServiceBuilder::new(s3);
    builder.set_auth(DeltaGliderS3sAuth {
        iam_state: iam_state.clone(),
    });
    builder.set_access(access);
    builder.set_config(crate::api::s3s_hooks::s3s_config(
        config.tuning.clock_skew_secs,
    ));
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
        use crate::api::handlers::form_post::{form_post_bucket, handle_form_post_upload};
        use axum::response::IntoResponse;

        // THE form-POST predicate, shared with the SigV4 deferral: `POST
        // /<bucket>`, multipart/form-data, no query, no Authorization,
        // bucket decoded as s3s and admission decode it. Every other POST
        // (`?delete`, CreateMultipartUpload, `//bucket`) goes to s3s.
        let Some(bucket) = form_post_bucket(request.method(), request.uri(), request.headers())
        else {
            return next.run(request).await;
        };

        // Pull iam_state from extensions (inserted as a layer below).
        let iam_state = request
            .extensions()
            .get::<crate::iam::SharedIamState>()
            .cloned();

        // Pull the rate limiter + client IP so a failed form-POST signature
        // feeds the brute-force lockout, under the form-POST key of the
        // address (`rate_limiter::FORM_POST_KEY`; the SigV4 middleware checks
        // that lockout before it defers form-POSTs to this handler). WITHOUT
        // this the form-POST endpoint would be the one auth surface with no
        // rate limiting. Extracted here (before the body is consumed) because
        // `into_parts` moves the extensions.
        let rate_limiter = request
            .extensions()
            .get::<crate::rate_limiter::RateLimiter>()
            .cloned();
        let metrics = request.extensions().get::<Arc<Metrics>>().cloned();
        let peer_ip = request
            .extensions()
            .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
            .map(|ci| ci.0.ip());

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
                // Proper S3 XML error (EntityTooLarge) so SDKs/Cyberduck parse it,
                // matching the PUT path's collect_blob_limited behaviour.
                return crate::api::S3Error::EntityTooLarge {
                    size: 0,
                    max: max_body as u64,
                }
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
            &parts.extensions,
            body_bytes,
            peer_ip,
        )
        .await
        {
            Ok(response) => response,
            Err(s3_err) => {
                // Every auth-class rejection is counted in the metric; only
                // a signature that did not match feeds the per-IP limiter,
                // like the SigV4 path. A form with no signature fields, an
                // unknown key or an IAM denial is no secret guess, and
                // counting it let anyone lock out a shared peer IP.
                let auth_class = matches!(
                    s3_err,
                    crate::api::S3Error::AccessDenied
                        | crate::api::S3Error::AccessDeniedReason(_)
                        | crate::api::S3Error::SignatureDoesNotMatch
                );
                if auth_class {
                    if let Some(m) = &metrics {
                        m.auth_attempts_total.with_label_values(&["failure"]).inc();
                        m.auth_failures_total
                            .with_label_values(&["form_post_denied"])
                            .inc();
                    }
                }
                if matches!(s3_err, crate::api::S3Error::SignatureDoesNotMatch) {
                    if let (Some(rl), Some(ip)) = (&rate_limiter, &peer_ip) {
                        let ip = crate::rate_limiter::extract_client_ip_with_peer(
                            &parts.headers,
                            Some(*ip),
                        );
                        if let Some(ip) = ip {
                            let locked =
                                rl.record_key_failure(&ip, crate::rate_limiter::FORM_POST_KEY);
                            if locked {
                                tracing::warn!(
                                    "SECURITY | event=brute_force_lockout | surface=form_post | ip={ip} | bucket={bucket}"
                                );
                            }
                        }
                    }
                }
                s3_err.into_response()
            }
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
            crate::metrics::http_metrics_middleware,
        ))
        .layer(middleware::from_fn(authorization_middleware))
        // Maintenance in-flight write counter. A PERMANENT layer whose
        // contents (the busy-bucket set) swap lock-free — unlike the
        // admission chain it cannot be lost to a config rebuild mid-job.
        // It only COUNTS writes. The refusals (writes to a busy bucket →
        // 503 SlowDown; any verb to a bucket on an UNHEALTHY backend → 503
        // naming the backend) run in `check_verified_request`, called from
        // the s3s access hook and the form-POST handler, i.e. after the
        // signature is verified: the 503 names internal state that an
        // unauthenticated caller must not learn. See src/maintenance/gate.rs.
        .layer(middleware::from_fn(
            crate::maintenance::gate::maintenance_gate_middleware,
        ))
        .layer(middleware::from_fn(sigv4_auth_middleware))
        .layer(middleware::from_fn(crate::admission::admission_middleware))
        .layer(axum::Extension(iam_state.clone()))
        .layer(axum::Extension(public_prefix_snapshot.clone()))
        .layer(axum::Extension(admission_chain.clone()))
        .layer(axum::Extension(state.maintenance_gate.clone()))
        .layer(axum::Extension(
            crate::coordination::health::BackendHealthGate {
                health: state.backend_health.clone(),
                config: shared_config.clone(),
                app: state.clone(),
            },
        ));

    if config_db_mismatch {
        tracing::error!(
            "S3 API LOCKED — all requests will be rejected until the config DB key mismatch is resolved via /_/"
        );
        router = router.layer(axum::Extension(ConfigDbMismatchGuard));
    }

    router
        .layer(axum::Extension(replay_cache.clone()))
        .layer(axum::Extension(crate::api::auth::ReplayWindow(
            config.tuning.replay_window(),
        )))
        .layer(axum::Extension(rate_limiter.clone()))
        .layer(axum::Extension(metrics.clone()))
        .layer(DefaultBodyLimit::max(config.max_object_size as usize))
        // Inside the global cap: a request takes its backend's slot only
        // once it has a global one, and gives a full share back at once.
        .layer(middleware::from_fn_with_state(
            BackendShare::new(config, shared_config, state),
            backend_share,
        ))
        .layer(tower::limit::ConcurrencyLimitLayer::new(
            config.tuning.max_concurrent_requests,
        ))
        // OUTSIDE the global cap: the wait for a slot counts against the
        // request deadline. Inside, a queued request waited without one.
        .layer(tower_http::timeout::TimeoutLayer::with_status_code(
            axum::http::StatusCode::GATEWAY_TIMEOUT,
            std::time::Duration::from_secs(config.tuning.request_timeout_secs),
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
            crate::cors::cors_layer_for(config.tuning.cors_permissive)
        })
        .with_state(state.clone())
}

#[cfg(test)]
mod backend_share_tests {
    use super::*;
    use tower::ServiceExt;

    /// The production S3 router over `config` (open access).
    async fn router(config: &Config) -> Router {
        let engine = crate::deltaglider::DynEngine::new(config, None)
            .await
            .unwrap();
        engine.create_bucket("downloads").await.unwrap();
        engine
            .store("downloads", "notes.txt", b"hello", None, Default::default())
            .await
            .unwrap();
        let state = AppState::for_tests(engine);
        let iam: SharedIamState = Arc::new(arc_swap::ArcSwap::from_pointee(
            crate::iam::IamState::Disabled,
        ));
        let snapshot: crate::bucket_policy::SharedPublicPrefixSnapshot =
            Arc::new(arc_swap::ArcSwap::from_pointee(
                crate::bucket_policy::PublicPrefixSnapshot::from_config(&config.buckets),
            ));
        let chain = crate::admission::build_shared_chain_from_parts(&config.buckets, &[]);
        let metrics = Arc::new(Metrics::new());
        let rate_limiter = RateLimiter::new(
            100,
            std::time::Duration::from_secs(300),
            std::time::Duration::from_secs(600),
        );
        let replay_cache: crate::api::auth::ReplayCache = Default::default();
        let shared_config = config.clone().into_shared();
        build_s3_router(RouterDeps {
            state: &state,
            iam_state: &iam,
            metrics: &metrics,
            rate_limiter: &rate_limiter,
            replay_cache: &replay_cache,
            config,
            config_db_mismatch: false,
            public_prefix_snapshot: &snapshot,
            admission_chain: &chain,
            shared_config: &shared_config,
        })
    }

    fn request(method: &str, path: &str) -> axum::http::Request<axum::body::Body> {
        axum::http::Request::builder()
            .method(method)
            .uri(path)
            .header("host", "localhost:9000")
            .body(axum::body::Body::empty())
            .unwrap()
    }

    /// One global cap of request slots for every bucket: requests to a
    /// slow backend (slow, so its probe passes and it is not gated) held
    /// every slot, and the requests to every other backend, the local disk
    /// and the UI queued behind them with no deadline.
    #[tokio::test]
    async fn a_slow_backend_cannot_take_every_request_slot() {
        let (endpoint, fake) = crate::storage::fake_s3::start().await;
        let disk = tempfile::tempdir().unwrap();
        let mut config = Config::from_yaml_str(&format!(
            "storage:\n  backends:\n    - name: hetzner-fsn1\n      type: s3\n      \
             endpoint: \"{endpoint}\"\n      region: us-east-1\n      force_path_style: true\n      \
             access_key_id: k\n      secret_access_key: s\n      allow_local: true\n    \
             - name: local-disk\n      type: filesystem\n      path: {}\n  \
             default_backend: local-disk\n  buckets:\n    releases: {{ backend: hetzner-fsn1 }}\n    \
             downloads: {{ backend: local-disk }}\n",
            disk.path().display()
        ))
        .unwrap();
        config.tuning.max_concurrent_requests = 4;
        let app = router(&config).await;
        fake.set_delay_ms("HEAD", 3_000);

        let slow: Vec<_> = (0..4)
            .map(|_| tokio::spawn(app.clone().oneshot(request("HEAD", "/releases/v1.zip"))))
            .collect();
        // Every slow request has its slot (or its answer) before the probe.
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline
            && fake
                .requests()
                .iter()
                .filter(|r| r.starts_with("HEAD /releases/"))
                .count()
                < 3
        {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let started = std::time::Instant::now();
        let resp = app
            .clone()
            .oneshot(request("GET", "/downloads/notes.txt"))
            .await
            .unwrap();
        let waited = started.elapsed();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        assert!(
            waited < std::time::Duration::from_secs(1),
            "a local-disk GET waited {waited:?} behind a slow backend"
        );
        for h in slow {
            let _ = h.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{head_range_is_partial, is_acl_request};

    #[test]
    fn only_a_head_with_content_range_becomes_206() {
        use axum::http::{header::CONTENT_RANGE, HeaderMap, Method, StatusCode};
        let mut ranged = HeaderMap::new();
        ranged.insert(CONTENT_RANGE, "bytes 1-3/4".parse().unwrap());
        let plain = HeaderMap::new();
        assert!(head_range_is_partial(
            &Method::HEAD,
            StatusCode::OK,
            &ranged
        ));
        assert!(!head_range_is_partial(
            &Method::HEAD,
            StatusCode::OK,
            &plain
        ));
        assert!(!head_range_is_partial(
            &Method::GET,
            StatusCode::OK,
            &ranged
        ));
        assert!(!head_range_is_partial(
            &Method::HEAD,
            StatusCode::NOT_MODIFIED,
            &ranged
        ));
    }

    #[test]
    fn acl_query_is_decoded_like_s3s() {
        let acl = |q: &str| is_acl_request(&format!("/b/k?{q}").parse().unwrap());
        assert!(acl("acl"));
        assert!(acl("acl="));
        assert!(acl("%61cl"));
        assert!(acl("versionId=v&acl"));
        assert!(!acl("aclx"));
        assert!(!acl("x=acl"));
    }
}
