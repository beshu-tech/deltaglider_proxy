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

/// Build the S3-compatible router with all routes and middleware layers.
///
/// Backed by the `s3s` crate, which translates wire-level S3 protocol
/// onto our [`crate::s3_adapter_s3s::DeltaGliderS3Service`].
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
///
/// The production S3 router: [`build_s3_router_with`] over the
/// [`DeltaGliderS3Service`](crate::s3_adapter_s3s::DeltaGliderS3Service)
/// and the [`VerifiedIdentityS3sAccess`](crate::api::s3s_hooks::VerifiedIdentityS3sAccess)
/// hook.
#[allow(clippy::too_many_arguments)]
pub fn build_s3_router(
    state: &Arc<AppState>,
    iam_state: &SharedIamState,
    metrics: &Arc<Metrics>,
    rate_limiter: &RateLimiter,
    replay_cache: &crate::api::auth::ReplayCache,
    config: &Config,
    config_db_mismatch: bool,
    public_prefix_snapshot: &crate::bucket_policy::SharedPublicPrefixSnapshot,
    admission_chain: &crate::admission::SharedAdmissionChain,
    shared_config: &crate::config::SharedConfig,
) -> Router {
    build_s3_router_with(
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
        crate::s3_adapter_s3s::DeltaGliderS3Service::new(state.clone()),
        crate::api::s3s_hooks::VerifiedIdentityS3sAccess,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn build_s3_router_with<S, A>(
    state: &Arc<AppState>,
    iam_state: &SharedIamState,
    metrics: &Arc<Metrics>,
    rate_limiter: &RateLimiter,
    replay_cache: &crate::api::auth::ReplayCache,
    config: &Config,
    config_db_mismatch: bool,
    public_prefix_snapshot: &crate::bucket_policy::SharedPublicPrefixSnapshot,
    admission_chain: &crate::admission::SharedAdmissionChain,
    shared_config: &crate::config::SharedConfig,
    s3: S,
    access: A,
) -> Router
where
    S: s3s::S3,
    A: S3Access,
{
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
    builder.set_config(crate::api::s3s_hooks::s3s_config());
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
        // feeds the SAME per-IP brute-force lockout as a failed SigV4 header
        // request. The SigV4 middleware defers form-POSTs to this handler, so
        // WITHOUT this the form-POST endpoint would be the one auth surface with
        // no rate limiting. Extracted here (before the body is consumed) because
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
                // An auth-class rejection (bad signature, unknown/denied
                // credential, expired/violated policy) feeds the per-IP
                // brute-force limiter + auth-failure metric, so this surface
                // is throttled and observable like the SigV4 path.
                if matches!(
                    s3_err,
                    crate::api::S3Error::AccessDenied
                        | crate::api::S3Error::AccessDeniedReason(_)
                        | crate::api::S3Error::SignatureDoesNotMatch
                ) {
                    if let Some(m) = &metrics {
                        m.auth_attempts_total.with_label_values(&["failure"]).inc();
                        m.auth_failures_total
                            .with_label_values(&["form_post_denied"])
                            .inc();
                    }
                    if let (Some(rl), Some(ip)) = (&rate_limiter, &peer_ip) {
                        let ip = crate::rate_limiter::extract_client_ip_with_peer(
                            &parts.headers,
                            Some(*ip),
                        );
                        if let Some(ip) = ip {
                            let locked = rl.record_failure(&ip);
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
        .layer(axum::Extension(rate_limiter.clone()))
        .layer(axum::Extension(metrics.clone()))
        .layer(DefaultBodyLimit::max(config.max_object_size as usize))
        .layer(tower_http::timeout::TimeoutLayer::with_status_code(
            axum::http::StatusCode::GATEWAY_TIMEOUT,
            std::time::Duration::from_secs(crate::config::env_parse_with_default(
                "DGP_REQUEST_TIMEOUT_SECS",
                300u64,
            )),
        ))
        .layer(tower::limit::ConcurrencyLimitLayer::new(
            crate::config::env_parse_with_default("DGP_MAX_CONCURRENT_REQUESTS", 1024usize),
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
            let permissive = crate::config::env_bool("DGP_CORS_PERMISSIVE", false);
            crate::cors::cors_layer_for(permissive)
        })
        .with_state(state.clone())
}

#[cfg(test)]
mod tests {
    use super::is_acl_request;

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
