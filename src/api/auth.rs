// SPDX-License-Identifier: GPL-3.0-only

//! SigV4 IDENTITY + authorization-orchestration middleware.
//!
//! This middleware resolves WHO a request is (access-key → `AuthenticatedUser`),
//! runs replay detection, rate-limiting, the auth-gate/config-lock, anonymous
//! public-prefix minting, and stashes the `x-amz-content-sha256` payload hash —
//! but it does NOT verify the SigV4 signature itself.
//!
//! The SIGNATURE is verified downstream by the `s3s` framework
//! (`DeltaGliderS3sAuth` in `startup.rs`), the sole signature authority. s3s
//! rejects a forged or wrong-secret signature (header, presigned, and chunked
//! streaming) before any handler runs — proven in
//! `tests/auth_integration_test.rs::test_forged_*`. Verifying here too was pure
//! redundancy (and the source of a hand-rolled/s3s divergence hazard), so the
//! canonical-request + HMAC machinery was removed.

use super::S3Error;
use crate::iam::{AuthenticatedUser, IamState, Permission, SharedIamState};
use crate::metrics::Metrics;
use crate::rate_limiter::{self, RateLimiter};
use axum::body::Body;
use axum::http::Request;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use dashmap::DashMap;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;
use tracing::{debug, info, warn};

/// Shared replay cache type: signature string -> timestamp of first use.
pub type ReplayCache = Arc<DashMap<String, Instant>>;

const MAX_REPLAY_ENTRIES: usize = 500_000;

/// The single auth-gate decision folded from the config-DB lock flag and the
/// runtime [`IamState`]. Folding both inputs into one exhaustively-matched
/// enum means the middleware can never silently un-lock by reordering a
/// router layer, and there is no `unreachable!()` arm hiding a real state.
///
/// Pure decision at a decision point — mirrors `classify_auth_config`
/// (`config.rs`) / `classify_s3_error` (`storage/s3.rs`).
enum AuthGateDecision<'a> {
    /// Config DB is locked (bootstrap password mismatch) — reject all S3 traffic.
    Locked,
    /// No auth configured — open access, pass through.
    Open,
    /// Legacy single-credential (bootstrap) mode.
    Bootstrap(&'a crate::iam::AuthConfig),
    /// Multi-user IAM mode.
    Iam(&'a crate::iam::IamIndex),
}

/// Fold the config-DB lock flag + [`IamState`] into a single exhaustive auth
/// decision. The lock overrides everything; otherwise the IamState variant
/// selects the auth path.
fn classify_auth_gate(locked: bool, iam_state: &IamState) -> AuthGateDecision<'_> {
    if locked {
        return AuthGateDecision::Locked;
    }
    match iam_state {
        IamState::Disabled => AuthGateDecision::Open,
        IamState::Legacy(auth) => AuthGateDecision::Bootstrap(auth),
        IamState::Iam(index) => AuthGateDecision::Iam(index),
    }
}

fn prune_replay_cache(cache: &ReplayCache, replay_window: Duration, max_entries: usize) {
    // Pass 1: cheap TTL cleanup.
    cache.retain(|_, instant| instant.elapsed() < replay_window);
    let len_after_ttl = cache.len();
    if len_after_ttl <= max_entries {
        return;
    }

    // Pass 2: hard-cap oldest signatures first. We only need to identify the
    // `to_remove` oldest entries, not fully order the cache — quickselect
    // (`select_nth_unstable_by_key`) partitions the oldest prefix in O(n)
    // average time instead of the O(n log n) full sort. The eviction set is
    // identical (the genuinely-oldest signatures); only the partial ordering
    // within that prefix is unspecified, which doesn't matter since they're
    // all removed.
    let to_remove = len_after_ttl - max_entries;
    let mut entries: Vec<(String, Instant)> = cache
        .iter()
        .map(|entry| (entry.key().clone(), *entry.value()))
        .collect();
    // `to_remove` is in `1..len_after_ttl` here (len_after_ttl > max_entries),
    // so the pivot index is always valid.
    entries.select_nth_unstable_by_key(to_remove - 1, |(_, seen_at)| *seen_at);
    for (sig, _) in entries.into_iter().take(to_remove) {
        cache.remove(&sig);
    }
}

/// What to do when a duplicate signature is seen within the replay window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayVerdict {
    /// First time this signature has been seen (or the window expired) — let it through.
    Fresh,
    /// A duplicate of an idempotent read (GET/HEAD). Boto3 emits byte-identical
    /// SigV4 signatures for the same request within one signing second (1s
    /// timestamp granularity), and SDK auto-retries of an idempotent read are
    /// safe by definition — replaying a GET/HEAD just re-reads the same bytes.
    /// Allow the request to proceed; do NOT 400 and do NOT count it as a failure.
    AllowIdempotentReplay,
    /// A duplicate of a mutating method (PUT/POST/DELETE/…) within the window.
    /// Replaying these has real side effects (double-write, double-delete), so
    /// reject. This is the actual replay-attack surface the guard protects.
    Reject,
}

/// Pure replay decision. `is_duplicate` is whether this signature was already
/// present in the cache within the live window; `method` is the HTTP method.
///
/// The split is deliberate: a captured idempotent read replayed within the
/// short window is harmless (it returns the same data), whereas a replayed
/// mutation is not. Keeping this pure lets the full truth table be unit-tested
/// without the HTTP/SigV4 stack (see the codebase testability conventions).
pub fn replay_decision(method: &axum::http::Method, is_duplicate: bool) -> ReplayVerdict {
    use axum::http::Method;
    if !is_duplicate {
        return ReplayVerdict::Fresh;
    }
    match *method {
        Method::GET | Method::HEAD => ReplayVerdict::AllowIdempotentReplay,
        _ => ReplayVerdict::Reject,
    }
}

/// Request extension carrying the client-claimed payload hash from
/// `x-amz-content-sha256`. Inserted by this middleware after identity
/// resolution (s3s verifies the SIGNATURE, which covers this header value, so
/// the claimed hash is signature-bound). Downstream handlers (the PUT path)
/// compare it against the actual body's SHA-256 to close the H1 integrity gap.
///
/// Sentinel values that disable verification:
/// - `UNSIGNED-PAYLOAD`: client opted out of body-hash signing.
/// - `STREAMING-AWS4-HMAC-SHA256-PAYLOAD` and the other STREAMING-*
///   variants: the chunked-payload protocol authenticates each chunk
///   separately. We don't validate the chunk-signature chain (see
///   `aws_chunked.rs`), so we don't claim end-to-end integrity for
///   those — the value is still recorded for observability but
///   `is_verifiable_hex()` returns false.
#[derive(Debug, Clone)]
pub struct SignedPayloadHash(pub String);

impl SignedPayloadHash {
    /// Returns the inner header value lowercase-trimmed (header
    /// values are sometimes uppercase from older SDKs).
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// Whether this value is a verifiable 64-char hex SHA-256 the
    /// body can be compared to. Returns false for UNSIGNED-PAYLOAD,
    /// STREAMING variants, and anything that's not 64 hex chars.
    pub fn is_verifiable_hex(&self) -> bool {
        let v = self.0.as_str();
        v.len() == 64
            && v.bytes().all(|b| {
                b.is_ascii_digit() || (b'a'..=b'f').contains(&b) || (b'A'..=b'F').contains(&b)
            })
    }

    /// Signed AWS streaming modes require per-chunk signature validation.
    /// The proxy can decode AWS chunked framing but does not verify the
    /// chunk-signature chain, so accepting these modes would advertise
    /// integrity we do not enforce.
    pub fn requires_chunk_signature_verification(&self) -> bool {
        let v = self.0.as_str();
        v.starts_with("STREAMING-AWS4-HMAC-") || v.starts_with("STREAMING-AWS4-ECDSA-")
    }

    /// H1 SigV4 integrity check: confirm the body's actual SHA-256 matches
    /// the value the client signed in `x-amz-content-sha256`. The signature
    /// covers the canonical request which only sees the header value, not
    /// the body bytes — so a credentialed client could otherwise sign hash
    /// A and ship body B unless the receiver verifies downstream.
    ///
    /// Sentinels that disable verification:
    ///   * `UNSIGNED-PAYLOAD` — client opted out (returns Ok).
    ///   * `STREAMING-*` variants — per-chunk signature scheme; we don't
    ///     verify the chunk-signature chain, so accepting them here would
    ///     advertise integrity we do not enforce. Returns `NotImplemented`.
    ///
    /// Comparison uses `subtle::ConstantTimeEq` to deny timing-side-channel
    /// inference of the signed hash.
    pub fn verify_against_body(&self, body: &[u8]) -> Result<(), super::S3Error> {
        if self.requires_chunk_signature_verification() {
            return Err(super::S3Error::NotImplemented(
                "Signed AWS streaming payloads are not supported; use UNSIGNED-PAYLOAD or non-streaming SHA-256 payloads".to_string(),
            ));
        }
        if !self.is_verifiable_hex() {
            return Ok(());
        }
        let actual = hex::encode(Sha256::digest(body));
        let matches: bool =
            ConstantTimeEq::ct_eq(actual.as_bytes(), self.as_str().as_bytes()).into();
        if !matches {
            return Err(super::S3Error::BadDigest);
        }
        Ok(())
    }
}

/// Build an anonymous `AuthenticatedUser` with read+list permissions scoped
/// to the given public prefixes. Used for unauthenticated public access.
fn build_anonymous_user(bucket: &str, public_prefixes: &[String]) -> AuthenticatedUser {
    use crate::iam::permissions::permission_to_iam_policy;

    let mut permissions = Vec::new();
    let mut iam_policies = Vec::new();

    for prefix in public_prefixes {
        // Read permission: scoped to bucket/prefix*
        let read_perm = Permission {
            id: 0,
            effect: "Allow".into(),
            actions: vec!["read".into()],
            resources: vec![format!("{}/{}*", bucket, prefix)],
            conditions: None,
        };
        iam_policies.push(permission_to_iam_policy(&read_perm));
        permissions.push(read_perm);

        // List permission — three shapes:
        //
        // 1. `public_prefixes: [""]` (entire bucket public, `public: true`
        //    shorthand). The middleware doesn't set an `s3:prefix`
        //    context key when the LIST request omits a prefix, so a
        //    StringLike condition evaluates as "key missing" and denies.
        //    We emit an unconditional list Allow in that case —
        //    everything in the bucket is public by definition.
        //
        // 2. `public_prefixes: ["x/"]` (slash-terminated, the canonical
        //    form). Emit `StringLike: { s3:prefix: ["x", "x/*"] }` so
        //    both `aws s3 ls s3://b/x` (no slash) and `aws s3 ls
        //    s3://b/x/` work. False-parent strings like `x-other` are
        //    denied because StringLike is anchored glob matching.
        //
        // 3. `public_prefixes: ["x"]` (non-slash-terminated, loose form).
        //    Preserve the old single-pattern behaviour — the operator
        //    explicitly asked for a loose prefix and splitting would
        //    change semantics.
        let list_perm = if prefix.is_empty() {
            Permission {
                id: 0,
                effect: "Allow".into(),
                actions: vec!["list".into()],
                resources: vec![format!("{}/*", bucket)],
                conditions: None,
            }
        } else {
            let s3_prefix_patterns: Vec<String> = if prefix.ends_with('/') {
                let bare = prefix.trim_end_matches('/').to_string();
                vec![bare, format!("{prefix}*")]
            } else {
                vec![format!("{prefix}*")]
            };
            Permission {
                id: 0,
                effect: "Allow".into(),
                actions: vec!["list".into()],
                resources: vec![format!("{}/*", bucket)],
                conditions: Some(serde_json::json!({
                    "StringLike": { "s3:prefix": s3_prefix_patterns }
                })),
            }
        };
        iam_policies.push(permission_to_iam_policy(&list_perm));
        permissions.push(list_perm);
    }

    AuthenticatedUser {
        name: "$anonymous".into(),
        access_key_id: String::new(),
        permissions,
        iam_policies,
    }
}

/// Common intermediate representation for SigV4 parameters,
/// populated from either Authorization header or presigned URL query params.
/// The SigV4 fields this middleware still needs post-dedup: the ACCESS KEY
/// (identity resolution), the SIGNATURE string (replay-cache key), and the
/// PAYLOAD HASH (`SignedPayloadHash` for the H1 body-integrity check). The
/// canonical-request material (scope, signed-headers, date, canonical query)
/// is gone — s3s reconstructs and verifies the signature itself.
struct SigV4Params {
    access_key: String,
    signature: String,
    payload_hash: String,
}

impl SigV4Params {
    /// Extract SigV4 parameters from the Authorization header path.
    #[allow(clippy::result_large_err)]
    fn from_headers(request: &Request<Body>) -> Result<Self, Response> {
        let auth_header = match request.headers().get("authorization") {
            Some(v) => match v.to_str() {
                Ok(s) => s.to_string(),
                Err(_) => {
                    warn!("SigV4: invalid Authorization header encoding");
                    return Err(S3Error::InvalidArgument(
                        "Invalid Authorization header encoding".to_string(),
                    )
                    .into_response());
                }
            },
            None => {
                debug!("SigV4: no Authorization header, rejecting");
                return Err(S3Error::AccessDenied.into_response());
            }
        };

        let parsed = match parse_auth_header(&auth_header) {
            Some(p) => p,
            None => {
                warn!("SigV4: failed to parse Authorization header");
                return Err(S3Error::InvalidArgument(
                    "Invalid Authorization header format".to_string(),
                )
                .into_response());
            }
        };

        let payload_hash = request
            .headers()
            .get("x-amz-content-sha256")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("UNSIGNED-PAYLOAD")
            .to_string();

        Ok(SigV4Params {
            access_key: parsed.access_key,
            signature: parsed.signature,
            payload_hash,
        })
    }

    /// Extract SigV4 parameters from presigned URL query params.
    #[allow(clippy::result_large_err)]
    fn from_query(request: &Request<Body>) -> Result<Self, Response> {
        let query_string = request.uri().query().unwrap_or("");

        let params: std::collections::HashMap<String, String> = query_string
            .split('&')
            .filter(|s| !s.is_empty())
            .filter_map(|pair| {
                let (k, v) = pair.split_once('=')?;
                Some((percent_decode(k), percent_decode(v)))
            })
            .collect();

        let credential = params.get("X-Amz-Credential").cloned().unwrap_or_default();
        let signature = params.get("X-Amz-Signature").cloned().unwrap_or_default();
        let amz_date = params.get("X-Amz-Date").cloned().unwrap_or_default();
        let expires = params.get("X-Amz-Expires").cloned().unwrap_or_default();

        if credential.is_empty() || signature.is_empty() {
            debug!("SigV4 presigned: missing credential or signature");
            return Err(S3Error::AccessDenied.into_response());
        }

        // Parse credential: AKID/date/region/service/aws4_request
        let (access_key, credential_scope) = match credential.split_once('/') {
            Some(pair) => pair,
            None => {
                warn!("SigV4 presigned: invalid credential format");
                return Err(S3Error::AccessDenied.into_response());
            }
        };

        // Validate credential scope format: date/region/s3/aws4_request
        let scope_parts: Vec<&str> = credential_scope.split('/').collect();
        if scope_parts.len() != 4 || scope_parts[2] != "s3" || scope_parts[3] != "aws4_request" {
            warn!(
                "SigV4 presigned: malformed credential scope: {}",
                credential_scope
            );
            return Err(
                S3Error::InvalidArgument("Invalid credential scope format".into()).into_response(),
            );
        }

        // Check expiration — hard-fail on parse errors
        // AWS caps presigned URL expiry at 7 days (604,800 seconds).
        const MAX_PRESIGNED_EXPIRY: i64 = 604_800;

        // X-Amz-Expires is REQUIRED for presigned URLs (AWS S3 spec).
        // Without it, the URL would have no time limit — reject immediately.
        if expires.is_empty() {
            warn!("SigV4 presigned: missing X-Amz-Expires (required)");
            return Err(S3Error::InvalidArgument(
                "X-Amz-Expires is required for presigned URLs".into(),
            )
            .into_response());
        }

        let expires_secs: i64 = expires.parse().map_err(|_| {
            warn!("SigV4 presigned: unparseable X-Amz-Expires: {:?}", expires);
            S3Error::InvalidArgument(format!("Invalid X-Amz-Expires: {}", expires)).into_response()
        })?;

        if expires_secs > MAX_PRESIGNED_EXPIRY {
            warn!(
                "SigV4 presigned: X-Amz-Expires={} exceeds 7-day maximum ({})",
                expires_secs, MAX_PRESIGNED_EXPIRY
            );
            return Err(S3Error::InvalidArgument(format!(
                "X-Amz-Expires={} exceeds maximum of {} seconds (7 days)",
                expires_secs, MAX_PRESIGNED_EXPIRY
            ))
            .into_response());
        }

        let request_time = chrono::NaiveDateTime::parse_from_str(&amz_date, "%Y%m%dT%H%M%SZ")
            .map_err(|_| {
                warn!("SigV4 presigned: unparseable X-Amz-Date: {:?}", amz_date);
                S3Error::InvalidArgument(format!("Invalid X-Amz-Date: {}", amz_date))
                    .into_response()
            })?;

        let request_utc = request_time.and_utc();
        let now = chrono::Utc::now();

        // Reject presigned URLs signed far in the future — prevents "permanent" URLs
        // by crafting X-Amz-Date in year 2099. Allow up to MAX_PRESIGNED_EXPIRY in the future.
        let future_limit = chrono::Duration::seconds(MAX_PRESIGNED_EXPIRY);
        if request_utc > now + future_limit {
            warn!(
                "SigV4 presigned: X-Amz-Date {} is too far in the future (limit: {} seconds ahead)",
                amz_date, MAX_PRESIGNED_EXPIRY
            );
            return Err(S3Error::RequestTimeTooSkewed.into_response());
        }

        let expiry = request_utc + chrono::Duration::seconds(expires_secs);
        if now > expiry {
            debug!("SigV4 presigned: URL expired (expired at {})", expiry);
            return Err(S3Error::AccessDenied.into_response());
        }

        Ok(SigV4Params {
            access_key: access_key.to_string(),
            signature,
            payload_hash: "UNSIGNED-PAYLOAD".to_string(),
        })
    }
}

/// Check whether the query string contains presigned URL parameters.
/// Uses proper key-level parsing instead of substring matching.
fn has_presigned_query_params(query: &str) -> bool {
    query.split('&').filter(|s| !s.is_empty()).any(|pair| {
        let key = pair.split_once('=').map(|(k, _)| k).unwrap_or(pair);
        percent_decode(key) == "X-Amz-Algorithm"
    })
}

/// Bucket-level HTML form upload candidates (`POST /bucket` with multipart form-data)
/// are authenticated via SigV4 POST policy fields in the body, not via the
/// Authorization header or presigned query params.
fn is_form_post_policy_candidate(request: &Request<Body>) -> bool {
    if request.method() != axum::http::Method::POST {
        return false;
    }
    if request.uri().query().is_some() {
        return false;
    }
    if request.uri().path().trim_matches('/').split('/').count() != 1 {
        return false;
    }
    request
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.to_ascii_lowercase().starts_with("multipart/form-data"))
        .unwrap_or(false)
}

/// Axum middleware that verifies SigV4 signatures when auth is configured.
///
/// Inserted as a layer around the router. If `auth` is `None` (no credentials
/// configured), all requests pass through unchanged.
pub async fn sigv4_auth_middleware(
    mut request: Request<Body>,
    next: Next,
) -> Result<Response, Response> {
    // IAM state is read from an ArcSwap so admin API user management
    // updates take effect immediately without restart.
    let iam_snapshot = request
        .extensions()
        .get::<SharedIamState>()
        .map(|swap| swap.load_full());

    let metrics = request.extensions().get::<Arc<Metrics>>().cloned();
    let rate_limiter = request.extensions().get::<RateLimiter>().cloned();
    let replay_cache = request.extensions().get::<ReplayCache>().cloned();

    // Extract client IP for rate limiting/session security.
    let peer_ip = request
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0.ip());
    let client_ip = rate_limiter::extract_client_ip_with_peer(request.headers(), peer_ip);

    // Extract audit fields before the closure captures them. Use the PEER-aware
    // resolver so audit lines agree with the rate limiter on the client IP for
    // this request (else audit logs "unknown" while the limiter keys on the peer).
    let (audit_ip, audit_ua) =
        crate::audit::extract_client_info_with_peer(request.headers(), peer_ip);

    let record_auth_failure = {
        let metrics = metrics.clone();
        let rate_limiter = rate_limiter.clone();
        let audit_ip = audit_ip.clone();
        let audit_ua = audit_ua.clone();
        move |reason: &str| {
            if let Some(m) = &metrics {
                m.auth_attempts_total.with_label_values(&["failure"]).inc();
                m.auth_failures_total.with_label_values(&[reason]).inc();
            }
            // Record failure in rate limiter + security logging. `bucket_key` is
            // the IP the rate limiter buckets on; `trust_proxy` reveals whether
            // that's the real client or a shared proxy IP — the field that makes
            // "all clients collapsed onto one bucket" diagnosable at a glance.
            if let (Some(rl), Some(ip)) = (&rate_limiter, &client_ip) {
                let locked = rl.record_failure(ip);
                let count = rl.failure_count(ip);
                let trust_proxy = crate::rate_limiter::trust_proxy_headers();
                if locked {
                    warn!(
                        "SECURITY | event=brute_force_lockout | ip={} | bucket_key={} | trust_proxy={} | attempts={} | reason={} | ua={}",
                        ip, ip, trust_proxy, count, reason, audit_ua
                    );
                } else if count >= 3 {
                    warn!(
                        "SECURITY | event=repeated_auth_failure | ip={} | bucket_key={} | trust_proxy={} | attempts={} | reason={} | ua={}",
                        ip, ip, trust_proxy, count, reason, audit_ua
                    );
                }
            }
            info!(
                "AUDIT | action=login_failed | user= | target={} | ip={} | ua={} | bucket= | path=",
                reason, audit_ip, audit_ua
            );
        }
    };

    // Replay rejection is NOT a credential failure: the signature is
    // cryptographically valid, the request is just a duplicate within the
    // window. We record it for observability (metrics + a distinct audit
    // action) but deliberately do NOT feed the per-IP brute-force lockout —
    // otherwise a retry-happy client holding a valid key could self-DoS its
    // own production key. See beshu-tech/deltaglider_proxy#24.
    let record_replay_rejection = {
        let metrics = metrics.clone();
        let audit_ip = audit_ip.clone();
        let audit_ua = audit_ua.clone();
        move || {
            if let Some(m) = &metrics {
                m.auth_failures_total.with_label_values(&["replay"]).inc();
            }
            info!(
                "AUDIT | action=replay_rejected | user= | target=replay | ip={} | ua={} | bucket= | path=",
                audit_ip, audit_ua
            );
        }
    };

    // Fold the config-DB lock flag + IamState into ONE exhaustive auth
    // decision. The lock is a first-class match arm (not a pre-match `if`),
    // so a router-layer reorder can never silently un-lock the server. The
    // `ConfigDbMismatchGuard` marker is injected by `build_s3_router` when the
    // bootstrap password fails to decrypt the config DB. Absence of the IAM
    // extension is treated as `Disabled` (open access).
    let config_db_locked = request
        .extensions()
        .get::<crate::api::ConfigDbMismatchGuard>()
        .is_some();
    // No IAM extension == open access; model it as `Disabled` for the fold.
    let disabled_fallback = IamState::Disabled;
    let iam_state = iam_snapshot.as_deref().unwrap_or(&disabled_fallback);

    let auth_config = match classify_auth_gate(config_db_locked, iam_state) {
        AuthGateDecision::Locked => {
            // Clear 503 with a recovery hint — NOT a misleading 500.
            // The proxy must not serve data without working authentication.
            return Err(crate::api::S3Error::ServiceUnavailable(
                "Config database locked — recover via admin GUI (/_/).".into(),
            )
            .into_response());
        }
        // Open access (no auth configured / no IAM extension): pass through.
        AuthGateDecision::Open => return Ok(next.run(request).await),
        // Auth required — fall through to signature verification below.
        decision => decision,
    };

    // Check rate limit before processing auth
    if let (Some(rl), Some(ip)) = (&rate_limiter, &client_ip) {
        if rl.is_limited(ip) {
            let count = rl.failure_count(ip);
            warn!(
                "SECURITY | event=brute_force_blocked | ip={} | bucket_key={} | trust_proxy={} | attempts={} | action=blocked",
                ip, ip, crate::rate_limiter::trust_proxy_headers(), count
            );
            return Err(
                S3Error::SlowDown("Rate limited due to repeated auth failures".into())
                    .into_response(),
            );
        }
        // Progressive delay: slow down responses proportional to failure count.
        // Makes brute force expensive even before lockout threshold.
        let delay = rl.progressive_delay(ip);
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
    }

    // Log every incoming request before auth check for debugging
    debug!(
        "Incoming request: {} {} (has auth header: {})",
        request.method(),
        request.uri(),
        request.headers().contains_key("authorization")
    );

    // Let CORS preflight requests through — browsers send OPTIONS without credentials
    if request.method() == axum::http::Method::OPTIONS {
        return Ok(next.run(request).await);
    }

    // Let HEAD / through unauthenticated — S3 clients (Cyberduck, etc.) use this as
    // a connection probe before sending real requests. Real S3 returns 200 for HEAD /.
    if request.method() == axum::http::Method::HEAD && request.uri().path() == "/" {
        debug!("SigV4: allowing unauthenticated HEAD / (connection probe)");
        return Ok(next.run(request).await);
    }

    // NOTE: Status endpoints (health, stats, metrics) live under /_/ and are served
    // by the admin router, NOT the S3 router. Do NOT bypass auth for bare /health,
    // /stats, /metrics here — those are valid S3 bucket names and bypassing auth
    // would expose any bucket named "health" etc. without credentials.

    // ── Admission-chain anonymous pre-admit ──
    // The admission middleware (runs before this one) has already decided
    // whether this request should proceed as anonymous. If it planted an
    // `AdmissionAllowAnonymous` marker in request extensions, we mint the
    // scoped `$anonymous` user here and skip signature verification.
    // Public-prefix matching logic lives in `crate::admission`; this
    // middleware is now responsible only for materialising the principal.
    if let Some(admit) = request
        .extensions()
        .get::<crate::admission::AdmissionAllowAnonymous>()
        .cloned()
    {
        let snapshot = request
            .extensions()
            .get::<crate::bucket_policy::SharedPublicPrefixSnapshot>()
            .map(|s| s.load_full());
        if let Some(snapshot) = snapshot {
            let public_prefixes = snapshot.public_prefixes_for_bucket(&admit.bucket);
            let anon_user = build_anonymous_user(&admit.bucket, public_prefixes);

            info!(
                "AUDIT | action=public_read | user=$anonymous | bucket={} | matched_block={} | ip={} | method={}",
                admit.bucket,
                admit.matched_block,
                audit_ip,
                request.method()
            );

            request.extensions_mut().insert(anon_user);
            return Ok(next.run(request).await);
        }
    }
    let query_string = request.uri().query().unwrap_or("");
    if !request.headers().contains_key("authorization")
        && !has_presigned_query_params(query_string)
        && is_form_post_policy_candidate(&request)
    {
        debug!("SigV4: deferring POST form policy auth to object handler");
        return Ok(next.run(request).await);
    }
    let is_presigned = has_presigned_query_params(query_string);
    let params = if is_presigned {
        SigV4Params::from_query(&request).inspect_err(|_| {
            record_auth_failure("invalid_presigned");
        })?
    } else {
        SigV4Params::from_headers(&request).inspect_err(|_| {
            record_auth_failure("missing_header");
        })?
    };

    // Look up the user's secret key and build the authenticated identity.
    // `auth_config` is already narrowed to Bootstrap | Iam by the gate match
    // above (Locked/Open returned early). The Locked/Open arm here is dead by
    // construction; it fails SAFE (deny) rather than panicking on `unreachable!()`.
    //
    // NOTE: this resolves IDENTITY only. The SIGNATURE is verified downstream by
    // s3s (`DeltaGliderS3sAuth`, startup.rs) — the sole signature authority. s3s
    // rejects a forged/absent-secret signature before any handler runs (proven:
    // tests/auth_integration_test.rs::test_forged_*). This middleware no longer
    // re-derives the signature; it produces the AuthenticatedUser + payload-hash
    // that the authz middleware and handlers consume.
    let authenticated_user = match auth_config {
        AuthGateDecision::Locked | AuthGateDecision::Open => {
            return Err(S3Error::AccessDenied.into_response());
        }
        AuthGateDecision::Bootstrap(auth) => {
            // Constant-time compare via fixed-length hashes. ct_eq
            // requires equal-length inputs; hashing first lets us
            // feed two `[u8; 32]` arrays into ct_eq regardless of
            // the AKID strings' lengths, so we don't leak length /
            // existence via timing. The IAM map path is inherently
            // leaky on existence (DashMap shards), but the bootstrap
            // path is hot enough to be a measurable oracle without
            // this guard.
            use sha2::{Digest, Sha256};
            use subtle::ConstantTimeEq;
            let provided_hash = Sha256::digest(params.access_key.as_bytes());
            let configured_hash = Sha256::digest(auth.access_key_id.as_bytes());
            let matches: bool = provided_hash.ct_eq(&configured_hash).into();
            if !matches {
                debug!("SigV4: access key mismatch (legacy mode)");
                record_auth_failure("invalid_access_key");
                return Err(S3Error::AccessDenied.into_response());
            }
            // Legacy user gets full access via wildcard permissions
            let bootstrap_perms = vec![Permission {
                id: 0,
                effect: "Allow".to_string(),
                actions: vec!["*".to_string()],
                resources: vec!["*".to_string()],
                conditions: None,
            }];
            let bootstrap_policies: Vec<iam_rs::IAMPolicy> = bootstrap_perms
                .iter()
                .map(crate::iam::permissions::permission_to_iam_policy)
                .collect();
            let auth_user = AuthenticatedUser {
                name: "$bootstrap".to_string(),
                access_key_id: auth.access_key_id.clone(),
                permissions: bootstrap_perms,
                iam_policies: bootstrap_policies,
            };
            Some(auth_user)
        }
        AuthGateDecision::Iam(index) => {
            let user = match index.get(&params.access_key) {
                Some(u) => u,
                None => {
                    debug!("SigV4: unknown access key '{}'", &params.access_key);
                    record_auth_failure("invalid_access_key");
                    return Err(S3Error::AccessDenied.into_response());
                }
            };
            if !user.enabled {
                debug!("SigV4: user '{}' is disabled", user.name);
                record_auth_failure("user_disabled");
                return Err(S3Error::AccessDenied.into_response());
            }
            let auth_user = AuthenticatedUser {
                name: user.name.clone(),
                access_key_id: user.access_key_id.clone(),
                permissions: user.permissions.clone(),
                iam_policies: user.iam_policies.clone(),
            };
            Some(auth_user)
        }
    };

    // Identity resolved (a known, enabled access key). s3s verifies the
    // signature downstream; a forged one is rejected there before any handler.
    if let Some(m) = &metrics {
        m.auth_attempts_total.with_label_values(&["success"]).inc();
    }
    // Reset the brute-force limiter for this IP now that a valid identity was
    // presented (access-key-guessing failures above still record + lock out).
    if let (Some(rl), Some(ip)) = (&rate_limiter, &client_ip) {
        rl.record_success(ip);
    }

    // Replay attack detection: reject duplicate signatures within the clock-skew window.
    //
    // Previously this used get() + insert() on two separate DashMap operations, which
    // is not atomic: two concurrent requests with the same signature could both pass
    // the get() check before either inserted. Fixed by using the entry() API which
    // acquires the per-key shard lock for the entire check-and-insert sequence.
    //
    // Skip replay detection for:
    // - Presigned URLs: designed to be reused (same signature for entire expiry window)
    //
    // Method matters here. A replayed *mutation* (PUT/POST/DELETE) has real
    // side effects, so it is rejected. A replayed *idempotent read* (GET/HEAD)
    // is harmless — it re-reads the same bytes — and is the one pattern boto3
    // produces unavoidably: SigV4 timestamps have 1-second granularity, so the
    // SDK emits byte-identical signatures for the same request issued twice (or
    // auto-retried) within one signing second. We keep GET/HEAD in the cache
    // (so a captured signature can't be replayed for the full clock-skew window
    // as it once could) but let same-window duplicates *pass through* instead
    // of 400-ing. See `replay_decision` and beshu-tech/deltaglider_proxy#24.
    let is_presigned = has_presigned_query_params(request.uri().query().unwrap_or(""));
    if let Some(ref cache) = replay_cache {
        if is_presigned {
            // No replay detection for presigned URLs (designed to be reused).
        } else {
            // Cap replay cache size to prevent memory exhaustion under attack.
            // First drop expired entries, then enforce a hard oldest-first cap.
            let replay_window = Duration::from_secs(crate::config::env_parse_with_default(
                "DGP_REPLAY_WINDOW_SECS",
                2,
            ));
            prune_replay_cache(cache, replay_window, MAX_REPLAY_ENTRIES);
            if cache.len() > MAX_REPLAY_ENTRIES {
                warn!(
                    "SECURITY | Replay cache still at {} entries after hard-cap eviction — possible flood attack",
                    cache.len()
                );
            }

            let sig = &params.signature;

            // Atomic check-and-insert under the per-key shard lock: decide
            // whether this signature is a live duplicate. Only RESET the
            // timestamp once the window has expired — never on a duplicate hit,
            // so the window is measured from first-seen and a tight retry loop
            // can't keep an idempotent read's slot alive indefinitely.
            let mut is_duplicate = false;
            cache
                .entry(sig.clone())
                .and_modify(|first_seen: &mut Instant| {
                    if first_seen.elapsed() < replay_window {
                        is_duplicate = true;
                    } else {
                        // Window expired — reset so the slot can be reused.
                        *first_seen = Instant::now();
                    }
                })
                .or_insert_with(Instant::now);

            match replay_decision(request.method(), is_duplicate) {
                ReplayVerdict::Fresh => {}
                ReplayVerdict::AllowIdempotentReplay => {
                    // Boto3 same-second signature on an idempotent read. Safe to
                    // serve; log at debug only (not a security event) and do not
                    // touch the lockout.
                    debug!(
                        "SigV4: idempotent-read replay tolerated — {} {} sig={}… (duplicate within {:?})",
                        request.method(),
                        request.uri().path(),
                        &params.signature[..params.signature.len().min(12)],
                        replay_window
                    );
                }
                ReplayVerdict::Reject => {
                    warn!(
                        "SigV4: replay detected — {} {} sig={}… (duplicate within {:?})",
                        request.method(),
                        request.uri().path(),
                        &params.signature[..params.signature.len().min(12)],
                        replay_window
                    );
                    // Distinct from a credential failure: observability only, no lockout.
                    record_replay_rejection();
                    return Err(
                        S3Error::InvalidArgument("Request replay detected".to_string())
                            .into_response(),
                    );
                }
            }
        } // else (not presigned)
    }

    // Insert authenticated user into request extensions (for authorization middleware)
    if let Some(user) = authenticated_user {
        debug!("SigV4: authenticated user '{}'", user.name);
        request.extensions_mut().insert(user);
    }

    // H1 SigV4 fix: stash the verified `x-amz-content-sha256` so the
    // PUT handler can compare it against the actual body's SHA-256.
    // Without this, a credentialed client could sign hash A and ship
    // body B — middleware would still accept the signature (it's
    // computed over the canonical-request, which only sees the header
    // value, not the body bytes) and the proxy would store body B.
    request
        .extensions_mut()
        .insert(SignedPayloadHash(params.payload_hash.clone()));

    Ok(next.run(request).await)
}

/// Parsed components of an AWS SigV4 Authorization header.
struct ParsedAuthHeader {
    access_key: String,
    signature: String,
}

/// Parse the Authorization header for the fields this middleware still needs:
/// the ACCESS KEY (identity) and the SIGNATURE string (replay-cache key). s3s
/// re-parses + verifies the full header downstream; this is a shape gate that
/// rejects a malformed header early with a clear 400.
///
/// Format: `AWS4-HMAC-SHA256 Credential=AKID/20260101/us-east-1/s3/aws4_request, SignedHeaders=..., Signature=abcdef...`
fn parse_auth_header(header: &str) -> Option<ParsedAuthHeader> {
    let header = header.trim();
    if !header.starts_with("AWS4-HMAC-SHA256") {
        return None;
    }

    let parts = header.strip_prefix("AWS4-HMAC-SHA256")?.trim();

    let mut credential = None;
    let mut signature = None;

    for part in parts.split(',') {
        let part = part.trim();
        if let Some(val) = part.strip_prefix("Credential=") {
            credential = Some(val.trim().to_string());
        } else if let Some(val) = part.strip_prefix("Signature=") {
            signature = Some(val.trim().to_string());
        }
    }

    let credential = credential?;
    let signature = signature?;

    // Parse credential: AKID/date/region/service/aws4_request; validate scope
    // shape so a garbled header is a clean 400 rather than a downstream surprise.
    let (access_key, credential_scope) = credential.split_once('/')?;
    let scope_parts: Vec<&str> = credential_scope.split('/').collect();
    if scope_parts.len() != 4 || scope_parts[2] != "s3" || scope_parts[3] != "aws4_request" {
        return None;
    }

    Some(ParsedAuthHeader {
        access_key: access_key.to_string(),
        signature,
    })
}

/// Percent-decode a URI component (e.g. `%2F` → `/`).
///
/// Lossy on invalid UTF-8 sequences (substitutes `U+FFFD`). This function
/// is the canonical decoder used across the request path — SigV4 query
/// parsing, admission middleware, and the admin trace endpoint all call
/// it so their decoding semantics are identical by construction.
pub fn percent_decode(input: &str) -> String {
    let mut result = Vec::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&input[i + 1..i + 3], 16) {
                result.push(byte);
                i += 3;
                continue;
            }
        }
        result.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&result).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Discriminant for asserting `classify_auth_gate` outcomes without
    /// constructing/comparing the borrowed payloads.
    fn gate_kind(d: &AuthGateDecision) -> &'static str {
        match d {
            AuthGateDecision::Locked => "locked",
            AuthGateDecision::Open => "open",
            AuthGateDecision::Bootstrap(_) => "bootstrap",
            AuthGateDecision::Iam(_) => "iam",
        }
    }

    #[test]
    fn classify_auth_gate_truth_table() {
        let auth = crate::iam::AuthConfig {
            access_key_id: "AKIA".into(),
            secret_access_key: "secret".into(),
        };
        let legacy = IamState::Legacy(auth.clone());
        let iam = IamState::Iam(crate::iam::IamIndex::from_users(vec![]));
        let disabled = IamState::Disabled;

        // locked=true overrides EVERY IamState variant.
        for state in [&disabled, &legacy, &iam] {
            assert_eq!(gate_kind(&classify_auth_gate(true, state)), "locked");
        }

        // locked=false: the IamState variant decides the path.
        assert_eq!(gate_kind(&classify_auth_gate(false, &disabled)), "open");
        assert_eq!(gate_kind(&classify_auth_gate(false, &legacy)), "bootstrap");
        assert_eq!(gate_kind(&classify_auth_gate(false, &iam)), "iam");
    }

    #[test]
    fn test_parse_auth_header() {
        let header = "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, Signature=fe5f80f77d5fa3beca038a248ff027d0445342fe2855ddc963176630326f1024";
        let parsed = parse_auth_header(header).unwrap();
        assert_eq!(parsed.access_key, "AKIAIOSFODNN7EXAMPLE");
        assert_eq!(
            parsed.signature,
            "fe5f80f77d5fa3beca038a248ff027d0445342fe2855ddc963176630326f1024"
        );
        // A malformed credential scope is rejected (shape gate).
        assert!(
            parse_auth_header("AWS4-HMAC-SHA256 Credential=AK/bad/scope, Signature=abc").is_none()
        );
    }

    #[test]
    fn test_parse_auth_header_invalid() {
        assert!(parse_auth_header("Basic dXNlcjpwYXNz").is_none());
        assert!(parse_auth_header("").is_none());
    }

    #[test]
    fn signed_payload_hash_classification() {
        let hex = SignedPayloadHash("a".repeat(64));
        assert!(hex.is_verifiable_hex());
        assert!(!hex.requires_chunk_signature_verification());

        let unsigned = SignedPayloadHash("UNSIGNED-PAYLOAD".into());
        assert!(!unsigned.is_verifiable_hex());
        assert!(!unsigned.requires_chunk_signature_verification());

        let signed_stream = SignedPayloadHash("STREAMING-AWS4-HMAC-SHA256-PAYLOAD".into());
        assert!(!signed_stream.is_verifiable_hex());
        assert!(signed_stream.requires_chunk_signature_verification());
    }
    #[test]
    fn test_has_presigned_query_params() {
        assert!(has_presigned_query_params(
            "X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=foo"
        ));
        assert!(!has_presigned_query_params("list-type=2&prefix=test"));
        assert!(!has_presigned_query_params(""));
        // Should not match substring (e.g. a value containing "X-Amz-Algorithm=")
        assert!(!has_presigned_query_params("foo=X-Amz-Algorithm%3Dbar"));
    }
    #[test]
    fn replay_cache_pruning_enforces_hard_cap() {
        let cache: ReplayCache = Arc::new(DashMap::new());
        for i in 0..10 {
            cache.insert(format!("sig-{i}"), Instant::now());
        }
        prune_replay_cache(&cache, Duration::from_secs(60), 3);
        assert!(cache.len() <= 3);
    }

    #[test]
    fn replay_cache_pruning_removes_expired_entries_before_size_eviction() {
        let cache: ReplayCache = Arc::new(DashMap::new());
        cache.insert("expired".into(), Instant::now() - Duration::from_secs(20));
        cache.insert("fresh-1".into(), Instant::now());
        cache.insert("fresh-2".into(), Instant::now());
        prune_replay_cache(&cache, Duration::from_secs(5), 10);
        assert!(!cache.contains_key("expired"));
        assert!(cache.contains_key("fresh-1"));
        assert!(cache.contains_key("fresh-2"));
    }

    // ── replay_decision truth table (beshu-tech/deltaglider_proxy#24) ──
    //
    // A non-duplicate of ANY method is always Fresh. A duplicate of an
    // idempotent read (GET/HEAD) is tolerated; a duplicate of any mutating
    // method is rejected. This is the whole product decision — keep it pure.

    #[test]
    fn replay_first_sighting_is_fresh_for_every_method() {
        use axum::http::Method;
        for m in [
            Method::GET,
            Method::HEAD,
            Method::PUT,
            Method::POST,
            Method::DELETE,
            Method::PATCH,
        ] {
            assert_eq!(
                replay_decision(&m, false),
                ReplayVerdict::Fresh,
                "first sighting of {m} should be Fresh"
            );
        }
    }

    #[test]
    fn replay_duplicate_idempotent_reads_are_tolerated() {
        use axum::http::Method;
        assert_eq!(
            replay_decision(&Method::GET, true),
            ReplayVerdict::AllowIdempotentReplay
        );
        assert_eq!(
            replay_decision(&Method::HEAD, true),
            ReplayVerdict::AllowIdempotentReplay
        );
    }

    #[test]
    fn replay_duplicate_mutations_are_rejected() {
        use axum::http::Method;
        for m in [
            Method::PUT,
            Method::POST,
            Method::DELETE,
            Method::PATCH,
            Method::OPTIONS,
        ] {
            assert_eq!(
                replay_decision(&m, true),
                ReplayVerdict::Reject,
                "duplicate {m} must be rejected"
            );
        }
    }

    // ── AWS-parity regression tests for anonymous LIST authz ──
    //
    // `aws s3 ls s3://bucket/ror/libs` (no trailing slash) sends
    // ListObjectsV2 with prefix="ror/libs". Before the fix, this
    // returned AccessDenied because the anonymous user's StringLike
    // condition was `ror/libs/*`, which doesn't match the bare
    // `ror/libs` string. After the fix the condition is a 2-element
    // array — the bare parent + the trailing-slash glob — so both
    // forms are allowed. A false parent like `ror/libsomething` must
    // STILL deny; these tests lock that in.

    fn allow_anon_list(public_prefix: &str, requested_prefix: &str) -> bool {
        use iam_rs::Context;
        let user = build_anonymous_user("beshu", &[public_prefix.to_string()]);
        let ctx = Context::new().with_string("s3:prefix", requested_prefix);
        crate::iam::permissions::evaluate_iam(
            &user.iam_policies,
            crate::iam::types::S3Action::List,
            "beshu",
            "",
            &ctx,
        )
    }

    /// Variant that mimics an incoming request with NO `prefix=` query
    /// parameter — the IAM middleware (iam/middleware.rs) does not set
    /// an `s3:prefix` context key in that case. For full-bucket-public
    /// configs this must still allow LIST.
    fn allow_anon_list_no_prefix(public_prefix: &str) -> bool {
        use iam_rs::Context;
        let user = build_anonymous_user("beshu", &[public_prefix.to_string()]);
        let ctx = Context::new(); // no s3:prefix
        crate::iam::permissions::evaluate_iam(
            &user.iam_policies,
            crate::iam::types::S3Action::List,
            "beshu",
            "",
            &ctx,
        )
    }

    #[test]
    fn anonymous_list_allows_exact_parent_prefix() {
        // `aws s3 ls s3://beshu/ror/libs` — CLI-convenience form.
        assert!(
            allow_anon_list("ror/libs/", "ror/libs"),
            "expected Allow for prefix=ror/libs against public=ror/libs/"
        );
    }

    #[test]
    fn anonymous_list_allows_trailing_slash_form() {
        // `aws s3 ls s3://beshu/ror/libs/`
        assert!(allow_anon_list("ror/libs/", "ror/libs/"));
    }

    #[test]
    fn anonymous_list_allows_deeper_prefix() {
        // `aws s3 ls s3://beshu/ror/libs/org/`
        assert!(allow_anon_list("ror/libs/", "ror/libs/org/"));
    }

    #[test]
    fn anonymous_list_denies_false_parent_sibling() {
        // `ror/libsomething` must NOT sneak under `ror/libs/`.
        // StringLike is anchored so `ror/libs` matches only the exact
        // string and `ror/libs/*` matches only strings starting with
        // `ror/libs/`. `ror/libsomething` fails both.
        assert!(
            !allow_anon_list("ror/libs/", "ror/libsomething"),
            "false-parent prefix `ror/libsomething` must be denied against public=ror/libs/"
        );
    }

    #[test]
    fn anonymous_list_denies_unrelated_prefix() {
        assert!(!allow_anon_list("ror/libs/", "secret/"));
    }

    #[test]
    fn anonymous_list_honors_non_slash_terminated_public_prefix() {
        // Operator configured `public_prefixes: ["archive"]` without a
        // trailing slash. The condition stays `archive*` — the loose-
        // prefix behaviour the existing code had. Documented tradeoff;
        // the fix for slash-terminated prefixes doesn't change this.
        assert!(allow_anon_list("archive", "archive/foo"));
        assert!(allow_anon_list("archive", "archiver/foo"));
    }

    #[test]
    fn anonymous_list_handles_empty_prefix_entire_bucket_public() {
        // `public: true` expands to `public_prefixes: [""]`. The
        // generated permission has NO condition, so every LIST is
        // allowed regardless of the request's s3:prefix value.
        assert!(allow_anon_list("", "anything/at/all"));
        assert!(allow_anon_list("", ""));
    }

    #[test]
    fn anonymous_list_fully_public_bucket_with_no_prefix_query() {
        // Real AWS S3 shape: client sends `GET /bucket/?list-type=2`
        // with no `prefix=` query param. The IAM middleware doesn't
        // set an `s3:prefix` context key. A StringLike condition
        // would evaluate as "key missing" → false → deny. The
        // empty-prefix public config therefore emits an unconditional
        // list Allow (see build_anonymous_user). Regression guard for
        // the public_prefixes: [""] + no-prefix-query case.
        assert!(
            allow_anon_list_no_prefix(""),
            "anonymous LIST with no prefix query param must succeed \
             when the entire bucket is public"
        );
    }

    #[test]
    fn anonymous_list_partial_public_without_prefix_query_denied() {
        // Opposite case: only a specific prefix is public, and the
        // client asks for a LIST with no prefix (= whole bucket).
        // Must deny, otherwise we'd leak keys outside the public
        // subtree.
        assert!(
            !allow_anon_list_no_prefix("ror/libs/"),
            "bucket-root LIST with no prefix query must be denied \
             when only a sub-prefix is public"
        );
    }
}
