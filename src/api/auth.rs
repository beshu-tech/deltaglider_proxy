// SPDX-License-Identifier: BUSL-1.1

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

use super::request_target::RequestTarget;
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
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;
use tracing::{debug, info, warn};

/// SigV4 clock-skew tolerance s3s enforces, in seconds (`DGP_CLOCK_SKEW_SECONDS`).
/// The default is s3s's own 900 s, which was the effective value while the
/// variable was documented (as 300 s) but never passed to s3s.
pub fn clock_skew_secs() -> u32 {
    crate::config::env_parse_with_default("DGP_CLOCK_SKEW_SECONDS", 900)
}

/// The SigV4 replay window. Default: the clock-skew window, so a captured
/// mutation cannot be replayed while its signature is still accepted.
/// `DGP_REPLAY_WINDOW_SECS=0` switches replay rejection off.
pub fn replay_window() -> Duration {
    replay_window_from(&crate::config::process_env)
}

/// [`replay_window`] over an injected env lookup (pure, unit-tested).
pub fn replay_window_from(env: crate::config::EnvLookup) -> Duration {
    let skew: u32 = crate::config::lookup_parse(env, "DGP_CLOCK_SKEW_SECONDS").unwrap_or(900);
    Duration::from_secs(
        crate::config::lookup_parse(env, "DGP_REPLAY_WINDOW_SECS").unwrap_or(u64::from(skew)),
    )
}

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
    /// Config DB is locked (no config DB key opens it) — reject all S3 traffic.
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

/// What the replay check did with one signature.
#[derive(Debug, PartialEq, Eq)]
enum ReplayClaim {
    /// The check is off (`DGP_REPLAY_WINDOW_SECS=0`): nothing stored.
    Off,
    /// Seen within the window: a replay.
    Duplicate,
    /// A same-second SDK retry of a PUT/DELETE (`same_second_retry_served`):
    /// served, and the slot stays with the first copy.
    Retry,
    /// Stored at this instant (a failed request gives it back).
    Claimed(Instant),
}

/// Check-and-insert one signature: one DashMap `entry()` call, atomic under
/// the per-key shard lock, so two concurrent duplicates cannot both pass.
/// The timestamp is reset only once the window expired, never on a
/// duplicate hit, so the window is measured from first-seen.
fn claim_replay_slot(
    cache: &ReplayCache,
    sig: &str,
    method: &axum::http::Method,
    replay_window: Duration,
) -> ReplayClaim {
    if replay_window.is_zero() {
        return ReplayClaim::Off;
    }
    let mut verdict = None;
    let claimed_at = Instant::now();
    cache
        .entry(sig.to_string())
        .and_modify(|first_seen: &mut Instant| {
            if same_second_retry_served(method, first_seen.elapsed()) {
                verdict = Some(ReplayClaim::Retry);
            } else if first_seen.elapsed() < replay_window {
                verdict = Some(ReplayClaim::Duplicate);
            } else {
                *first_seen = claimed_at;
            }
        })
        .or_insert(claimed_at);
    verdict.unwrap_or(ReplayClaim::Claimed(claimed_at))
}

fn prune_replay_cache(cache: &ReplayCache, replay_window: Duration, max_entries: usize) {
    // Pass 1: cheap TTL cleanup.
    cache.retain(|_, instant| instant.elapsed() < replay_window);
    let len_after_ttl = cache.len();
    if len_after_ttl <= max_entries {
        return;
    }
    // Down to a low-water mark, not to the cap: pruned to exactly the cap,
    // the next insert is over it again, and under a steady load every later
    // mutation ran this O(cache) prune on the request path.
    let max_entries = max_entries - max_entries / 10;

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

/// Whether the identity this middleware resolved may act, given the access
/// key whose signature s3s actually verified (`None` = s3s saw no usable
/// credentials and verified nothing).
///
/// This middleware resolves identity from the request text; s3s is the only
/// signature authority. The two parse the request independently, so they can
/// disagree: with two `Authorization` headers s3s sees none (`get_unique`),
/// and a SigV2 query is verified by s3s while we read the v4 header. Either
/// way the resolved user is honoured only when s3s verified THAT user's key.
///
/// - No resolved user: nothing is granted here (open mode, `HEAD /` probe,
///   CORS preflight, form-POST policy deferral) — allow.
/// - `$anonymous` (admission allow-anonymous): carries only public-prefix
///   rights, needs no signature — allow.
/// - Any other user: s3s must have verified the same access key.
pub fn resolved_identity_is_verified(
    resolved: Option<&AuthenticatedUser>,
    verified_access_key: Option<&str>,
) -> bool {
    match resolved {
        None => true,
        Some(user) if user.is_anonymous() => true,
        Some(user) => verified_access_key.is_some_and(|verified| {
            crate::security::secret_eq(verified.as_bytes(), user.access_key_id.as_bytes())
        }),
    }
}

/// Per-request record of how far authentication got, shared (one `Arc`)
/// between this middleware, the IAM authorization middleware and the s3s
/// access hook. This middleware only RESOLVES identity; s3s verifies the
/// signature later. So the brute-force limiter can only learn the outcome
/// after the inner layers ran: success is a signature s3s verified, failure
/// is a 403 that never reached that point.
#[derive(Clone, Default)]
pub struct AuthOutcome(Arc<AtomicU8>);

// 0 (the `Default`) is pending: nothing verified or denied yet.
const OUTCOME_VERIFIED: u8 = 1;
const OUTCOME_AUTHZ_DENIED: u8 = 2;

impl AuthOutcome {
    /// s3s verified the signature of the resolved identity.
    pub fn mark_verified(&self) {
        self.0.store(OUTCOME_VERIFIED, Ordering::Release);
    }

    /// IAM authorization refused the resolved identity before s3s ran. Not
    /// a credential failure: the signature was never checked.
    pub fn mark_authz_denied(&self) {
        self.0.store(OUTCOME_AUTHZ_DENIED, Ordering::Release);
    }

    fn state(&self) -> u8 {
        self.0.load(Ordering::Acquire)
    }
}

/// What the brute-force limiter records once the response is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimiterVerdict {
    /// s3s verified the signature: reset the IP's failure counter.
    Success,
    /// A 403 before verification: bad signature, expired or skewed
    /// request, or an identity s3s did not verify.
    Failure,
    /// Nothing learned about the credential (authz denial, gate 503, ...).
    Neither,
}

/// Pure decision for [`LimiterVerdict`] from the request outcome and status.
///
/// A verified PRESIGNED request is not a success: it proves that the signer
/// knew the secret, not the caller. Anyone holding a link could otherwise
/// reset the IP's counter between wrong-secret guesses.
pub fn limiter_verdict(
    outcome: &AuthOutcome,
    status: axum::http::StatusCode,
    presigned: bool,
) -> LimiterVerdict {
    match outcome.state() {
        OUTCOME_VERIFIED if presigned => LimiterVerdict::Neither,
        OUTCOME_VERIFIED => LimiterVerdict::Success,
        OUTCOME_AUTHZ_DENIED => LimiterVerdict::Neither,
        _ if status == axum::http::StatusCode::FORBIDDEN => LimiterVerdict::Failure,
        _ => LimiterVerdict::Neither,
    }
}

/// Pure: whether a request takes part in replay detection. Only mutating
/// requests do. A replayed GET/HEAD re-reads the same bytes, and SDKs emit
/// byte-identical same-second signatures for reads and their retries
/// (beshu-tech/deltaglider_proxy#24), so caching read signatures costs
/// memory for the whole window and protects nothing.
pub fn replay_tracked(method: &axum::http::Method) -> bool {
    use axum::http::Method;
    !matches!(*method, Method::GET | Method::HEAD)
}

/// Pure: whether a duplicate signature seen `since_first` after its first
/// copy is served as a retry instead of refused as a replay. Only PUT and
/// DELETE, and only inside the signing second: SigV4 timestamps have
/// one-second resolution, so an SDK that retries in the second it signed
/// (after a lost response or a gateway 5xx) sends the same signature. A
/// retry signed in a later second carries a new signature anyway. The
/// second is measured on the server clock from the first copy, not from
/// `x-amz-date`, so client clock skew cannot stretch it. PUT and DELETE
/// repeat the same effect; other mutations (POST) stay strict.
pub fn same_second_retry_served(method: &axum::http::Method, since_first: Duration) -> bool {
    use axum::http::Method;
    matches!(*method, Method::PUT | Method::DELETE) && since_first < Duration::from_secs(1)
}

/// Whether a request that claimed a replay-cache slot keeps it once its
/// response is known: only on success (2xx/3xx). A failed mutation had no
/// effect, so a byte-identical retry of it is not a replay.
pub fn replay_slot_kept(status: axum::http::StatusCode) -> bool {
    status.is_success() || status.is_redirection()
}

/// Request extension carrying the client-claimed payload hash from
/// `x-amz-content-sha256`. Inserted by this middleware after identity
/// resolution (s3s verifies the SIGNATURE, which covers this header value, so
/// the claimed hash is signature-bound). Downstream handlers (the PUT path)
/// compare it against the actual body's SHA-256 to close the H1 integrity gap.
///
/// Sentinel values that disable the body-hash comparison here:
/// - `UNSIGNED-PAYLOAD`: client opted out of body-hash signing.
/// - `STREAMING-AWS4-HMAC-SHA256-PAYLOAD` and the other STREAMING-*
///   variants: the chunked-payload protocol authenticates each chunk with a
///   signature chain, which s3s verifies UPSTREAM (the sole SigV4 authority)
///   before the handler runs, handing us the already de-chunked body. There
///   is no single body-hash to compare, so `is_verifiable_hex()` returns
///   false and the payload is accepted (integrity already enforced). Rejecting
///   these here 501'd authenticated streaming PUTs (X-ray H15).
#[derive(Debug, Clone)]
pub struct SignedPayloadHash(pub String);

/// The policy client IP for this request (`extract_trusted_client_ip`: the
/// peer, or the XFF client behind a `DGP_TRUSTED_PROXY_CIDRS` proxy), injected into request extensions so handlers that run a SECOND
/// authorization the middleware never saw — e.g. CopyObject's source-read
/// check — can build a policy context with `aws:SourceIp` and honor IP-scoped
/// conditions. Without it those checks silently ignore IP conditions.
#[derive(Debug, Clone)]
pub struct RequestClientIp(pub std::net::IpAddr);

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

    /// H1 SigV4 integrity check: confirm the body's actual SHA-256 matches
    /// the value the client signed in `x-amz-content-sha256`. The signature
    /// covers the canonical request which only sees the header value, not
    /// the body bytes — so a credentialed client could otherwise sign hash
    /// A and ship body B unless the receiver verifies downstream.
    ///
    /// Sentinels that disable body-hash comparison here:
    ///   * `UNSIGNED-PAYLOAD` — client opted out (returns Ok).
    ///   * `STREAMING-*` variants — the per-chunk signature chain is verified
    ///     UPSTREAM by s3s (`aws_chunked_stream::check_signature`, the sole
    ///     SigV4 authority since the dedup), and s3s hands us the already
    ///     de-chunked body. There is no single body-hash to compare, exactly
    ///     like UNSIGNED. Returning NotImplemented here rejected valid
    ///     authenticated streaming PUTs with 501 (X-ray H15) while the
    ///     anonymous path — no SignedPayloadHash injected — worked; the
    ///     asymmetry was the bug.
    ///
    /// Comparison uses `subtle::ConstantTimeEq` to deny timing-side-channel
    /// inference of the signed hash.
    pub fn verify_against_body(&self, body: &[u8]) -> Result<(), super::S3Error> {
        // STREAMING and UNSIGNED both have no verifiable single hash here; the
        // streaming chunk signatures are enforced by s3s upstream.
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
pub(crate) fn build_anonymous_user(bucket: &str, public_prefixes: &[String]) -> AuthenticatedUser {
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
        name: crate::iam::types::ANONYMOUS_USER_NAME.into(),
        access_key_id: String::new(),
        permissions,
        iam_policies,
    }
}

/// Request-extension marker: this request was authenticated through a
/// presigned URL (query-string SigV4), i.e. by the link holder, not by a
/// caller who holds the signer's credentials.
#[derive(Debug, Clone, Copy)]
pub struct PresignedRequest;

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
        let target = RequestTarget::from_uri(request.uri()).map_err(|_| {
            S3Error::InvalidArgument("Invalid URI encoding".to_string()).into_response()
        })?;
        let param = |name: &str| target.query_value(name).unwrap_or_default().to_string();

        let credential = param("X-Amz-Credential");
        let signature = param("X-Amz-Signature");
        let amz_date = param("X-Amz-Date");
        let expires = param("X-Amz-Expires");

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

        // AWS accepts 1..=604800. A negative value is no URL, and a huge one
        // overflowed the expiry arithmetic below (a pre-auth panic).
        if expires_secs < 1 {
            warn!("SigV4 presigned: X-Amz-Expires={expires_secs} is not positive");
            return Err(S3Error::InvalidArgument(format!(
                "X-Amz-Expires={expires_secs} must be at least 1 second"
            ))
            .into_response());
        }
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
        if request_utc > now.checked_add_signed(future_limit).unwrap_or(now) {
            warn!(
                "SigV4 presigned: X-Amz-Date {} is too far in the future (limit: {} seconds ahead)",
                amz_date, MAX_PRESIGNED_EXPIRY
            );
            return Err(S3Error::RequestTimeTooSkewed.into_response());
        }

        // In range by the checks above; `checked_` so no date near chrono's
        // limits can panic here.
        let Some(expiry) = request_utc.checked_add_signed(chrono::Duration::seconds(expires_secs))
        else {
            return Err(S3Error::AccessDenied.into_response());
        };
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
    RequestTarget::parse("/", Some(query)).is_ok_and(|t| t.is_presigned_v4())
}

/// Fuzz entry (`fuzz_entry::sigv4`): the identity parse this middleware runs
/// before s3s, `(access key, signature)`.
#[doc(hidden)]
pub fn fuzz_sigv4_identity(request: &Request<Body>) -> Option<(String, String)> {
    let params = if has_presigned_query_params(request.uri().query().unwrap_or("")) {
        SigV4Params::from_query(request)
    } else {
        SigV4Params::from_headers(request)
    }
    .ok()?;
    Some((params.access_key, params.signature))
}

/// Axum middleware that verifies SigV4 signatures when auth is configured.
///
/// Inserted as a layer around the router. If `auth` is `None` (no credentials
/// configured), all requests pass through unchanged.
// `Err` is the early-response short-circuit axum middleware idiom; boxing an
// `http::Response` on the per-request hot path to please `result_large_err`
// (clippy ≥ 1.98) buys nothing.
#[allow(clippy::result_large_err)]
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
    // `ConfigDbMismatchGuard` marker is injected by `build_s3_router` when no
    // config DB key decrypts the config DB. Absence of the IAM
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
        if let Some(left) = rl.lockout_remaining(ip) {
            let count = rl.failure_count(ip);
            warn!(
                "SECURITY | event=brute_force_blocked | ip={} | bucket_key={} | trust_proxy={} | attempts={} | action=blocked",
                ip, ip, crate::rate_limiter::trust_proxy_headers(), count
            );
            // S3 clients understand SlowDown; the message and Retry-After
            // say how long the lockout lasts.
            let (secs, message) = crate::rate_limiter::lockout_message(left);
            let mut resp = S3Error::SlowDown(format!(
                "Rate limited due to repeated auth failures. {message}"
            ))
            .into_response();
            if let Ok(v) = axum::http::HeaderValue::from_str(&secs.to_string()) {
                resp.headers_mut()
                    .insert(axum::http::header::RETRY_AFTER, v);
            }
            return Err(resp);
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
            let mut anon_user = build_anonymous_user(&admit.bucket, public_prefixes);
            // An operator block grants exactly this read-class request.
            if let Some(perm) = admit.grant.as_ref().and_then(|g| g.permission()) {
                anon_user
                    .iam_policies
                    .push(crate::iam::permissions::permission_to_iam_policy(&perm));
                anon_user.permissions.push(perm);
            }

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
    // A browser form POST carries its signature in the policy fields; the
    // form handler checks it. One predicate decides both this deferral and
    // the router's interception (`form_post_bucket`).
    if crate::api::handlers::form_post::form_post_bucket(
        request.method(),
        request.uri(),
        request.headers(),
    )
    .is_some()
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
    // that the authz middleware and handlers consume. s3s parses the request on
    // its own, so the s3s access hook (`resolved_identity_is_verified`) refuses
    // the request unless s3s verified the SAME access key resolved here.
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
            let matches = crate::security::secret_eq(
                params.access_key.as_bytes(),
                auth.access_key_id.as_bytes(),
            );
            if !matches {
                debug!("SigV4: access key mismatch (legacy mode)");
                record_auth_failure("invalid_access_key");
                return Err(S3Error::AccessDenied.into_response());
            }
            // Legacy user gets full access via wildcard permissions
            Some(AuthenticatedUser::bootstrap(&auth.access_key_id))
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
            Some(AuthenticatedUser::from(user))
        }
    };

    // Identity resolved (a known, enabled access key). s3s verifies the
    // signature downstream; a forged one is rejected there before any handler.
    // Success and failure are recorded only after that (see `AuthOutcome`):
    // resetting the limiter here, before verification, meant a wrong-secret
    // loop on a known key was never counted (S19).
    let outcome = AuthOutcome::default();
    request.extensions_mut().insert(outcome.clone());

    // Replay attack detection: reject a duplicate signature of a MUTATING
    // request within DGP_REPLAY_WINDOW_SECS. The default is the clock-skew
    // window (DGP_CLOCK_SKEW_SECONDS, 900 s): a signature outside the skew
    // fails verification anyway, so a captured mutation is refused for its
    // whole valid life. Memory is capped at MAX_REPLAY_ENTRIES. The cache is
    // per instance: behind a load balancer, a replay sent to another node is
    // not detected.
    //
    // The check-and-insert is one DashMap `entry()` call, atomic under the
    // per-key shard lock, so two concurrent duplicates cannot both pass.
    //
    // Not tracked: presigned URLs (designed to be reused) and GET/HEAD (see
    // `replay_tracked`).
    let is_presigned = has_presigned_query_params(request.uri().query().unwrap_or(""));
    // The cache slot this request claimed (signature + the instant it
    // stored), so a failed request can give it back below.
    let mut replay_claim: Option<(ReplayCache, String, Instant)> = None;
    if let Some(ref cache) = replay_cache {
        if !is_presigned && replay_tracked(request.method()) {
            let replay_window = replay_window();
            // Expired entries go in the periodic sweep (`init_replay_cache`),
            // not here: a full retain per request is O(cache) with a 900 s
            // window. Only an over-cap cache is pruned inline.
            if cache.len() > MAX_REPLAY_ENTRIES {
                prune_replay_cache(cache, replay_window, MAX_REPLAY_ENTRIES);
                if cache.len() > MAX_REPLAY_ENTRIES {
                    warn!(
                        "SECURITY | Replay cache still at {} entries after hard-cap eviction — possible flood attack",
                        cache.len()
                    );
                }
            }

            let sig = &params.signature;
            let claim = claim_replay_slot(cache, sig, request.method(), replay_window);

            if claim == ReplayClaim::Duplicate {
                warn!(
                    "SigV4: replay detected — {} {} sig={}… (duplicate within {:?})",
                    request.method(),
                    request.uri().path(),
                    crate::security::str_prefix(&params.signature, 12),
                    replay_window
                );
                // Distinct from a credential failure: observability only, no lockout.
                record_replay_rejection();
                return Err(
                    S3Error::InvalidArgument("Request replay detected".to_string()).into_response(),
                );
            }
            // A served retry (`Retry`) does not own the slot: the first copy does.
            if let ReplayClaim::Claimed(claimed_at) = claim {
                replay_claim = Some((cache.clone(), sig.clone(), claimed_at));
            }
        }
    }

    // Insert authenticated user into request extensions (for authorization middleware)
    if let Some(user) = authenticated_user {
        debug!("SigV4: authenticated user '{}'", user.name);
        request.extensions_mut().insert(user);
    }
    // A presigned URL authenticates as its SIGNER, but whoever holds the link
    // is an anonymous party (the docs recommend presigned links over public
    // prefixes for third parties). Handlers that hide deployment provenance
    // from anonymous readers check this marker alongside `$anonymous`.
    if is_presigned {
        request.extensions_mut().insert(PresignedRequest);
    }

    // Stash the policy IP so handlers running a SECONDARY authz the
    // authorization middleware never sees (CopyObject source-read) can build a
    // policy context with aws:SourceIp and honor IP-scoped conditions. It is
    // the TRUSTED IP, not the limiter's `client_ip`: without a CIDR list the
    // XFF header is client-written and must not satisfy a condition.
    if let Some(policy_ip) = rate_limiter::extract_trusted_client_ip(request.headers(), peer_ip) {
        request.extensions_mut().insert(RequestClientIp(policy_ip));
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

    let response = next.run(request).await;
    // Only a request that succeeded keeps its signature in the replay cache.
    // An SDK retries a 503 (gate SlowDown, backend outage) within the same
    // signing second with a byte-identical signature; refusing that retry
    // as a replay would turn a retryable error into a hard 400. The slot is
    // held while the request runs, so a concurrent duplicate still fails.
    if let Some((cache, sig, claimed_at)) = replay_claim {
        if !replay_slot_kept(response.status()) {
            cache.remove_if(&sig, |_, seen| *seen == claimed_at);
        }
    }
    match limiter_verdict(&outcome, response.status(), is_presigned) {
        LimiterVerdict::Success => {
            if let Some(m) = &metrics {
                m.auth_attempts_total.with_label_values(&["success"]).inc();
            }
            if let (Some(rl), Some(ip)) = (&rate_limiter, &client_ip) {
                rl.record_success(ip);
            }
        }
        LimiterVerdict::Failure => record_auth_failure("signature_rejected"),
        LimiterVerdict::Neither => {}
    }
    Ok(response)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// X-Amz-Expires is client text. A huge negative value made
    /// `request_time + expires` overflow chrono and panic, pre-auth, on any
    /// S3 request with a presigned query. AWS accepts 1..=604800 only.
    /// Found by the `sigv4` fuzz target.
    #[test]
    fn presigned_expiry_outside_one_second_to_seven_days_is_refused() {
        let date = chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
        for expires in [
            "-600110010600106",
            "-9223372036854775808",
            "0",
            "-1",
            "604801",
        ] {
            let uri = format!(
                "/b/k?X-Amz-Algorithm=AWS4-HMAC-SHA256\
                 &X-Amz-Credential=AK%2F20260101%2Fus-east-1%2Fs3%2Faws4_request\
                 &X-Amz-Date={date}&X-Amz-Expires={expires}&X-Amz-SignedHeaders=host\
                 &X-Amz-Signature=ab"
            );
            let request = Request::builder().uri(uri).body(Body::empty()).unwrap();
            assert!(
                SigV4Params::from_query(&request).is_err(),
                "X-Amz-Expires={expires} accepted"
            );
        }
        let uri = format!(
            "/b/k?X-Amz-Credential=AK%2F20260101%2Fus-east-1%2Fs3%2Faws4_request\
             &X-Amz-Date={date}&X-Amz-Expires=1&X-Amz-Signature=ab"
        );
        let request = Request::builder().uri(uri).body(Body::empty()).unwrap();
        assert!(SigV4Params::from_query(&request).is_ok());
    }

    /// Only an s3s-verified signature resets the limiter; a 403 before
    /// verification is a failure; an authz denial or other status teaches
    /// nothing about the credential (S19).
    #[test]
    fn limiter_verdict_truth_table() {
        use axum::http::StatusCode;
        let pending = AuthOutcome::default();
        assert_eq!(
            limiter_verdict(&pending, StatusCode::FORBIDDEN, false),
            LimiterVerdict::Failure
        );
        assert_eq!(
            limiter_verdict(&pending, StatusCode::OK, false),
            LimiterVerdict::Neither
        );
        assert_eq!(
            limiter_verdict(&pending, StatusCode::SERVICE_UNAVAILABLE, false),
            LimiterVerdict::Neither
        );
        let verified = AuthOutcome::default();
        verified.mark_verified();
        assert_eq!(
            limiter_verdict(&verified, StatusCode::OK, false),
            LimiterVerdict::Success
        );
        // Verified, then a handler-level 403 (per-key deny): still a good key.
        assert_eq!(
            limiter_verdict(&verified, StatusCode::FORBIDDEN, false),
            LimiterVerdict::Success
        );
        let denied = AuthOutcome::default();
        denied.mark_authz_denied();
        assert_eq!(
            limiter_verdict(&denied, StatusCode::FORBIDDEN, false),
            LimiterVerdict::Neither
        );
        // A verified presigned link resets nothing; a forged one still counts.
        assert_eq!(
            limiter_verdict(&verified, StatusCode::OK, true),
            LimiterVerdict::Neither
        );
        assert_eq!(
            limiter_verdict(&pending, StatusCode::FORBIDDEN, true),
            LimiterVerdict::Failure
        );
        // Clones share one state: the s3s hook marks what the middleware reads.
        let shared = AuthOutcome::default();
        shared.clone().mark_verified();
        assert_eq!(
            limiter_verdict(&shared, StatusCode::OK, false),
            LimiterVerdict::Success
        );
    }

    /// Truth table for binding the resolved identity to the s3s-verified key.
    #[test]
    fn resolved_identity_must_match_verified_key() {
        let alice = AuthenticatedUser::bootstrap("AKALICE");
        let anon = build_anonymous_user("b", &["pub/".to_string()]);

        // Nothing resolved: nothing granted, allow.
        assert!(resolved_identity_is_verified(None, None));
        assert!(resolved_identity_is_verified(None, Some("AKALICE")));
        // Anonymous needs no signature.
        assert!(resolved_identity_is_verified(Some(&anon), None));
        // A real user needs s3s to have verified that same key.
        assert!(resolved_identity_is_verified(Some(&alice), Some("AKALICE")));
        // Duplicate Authorization headers: s3s verified nothing.
        assert!(!resolved_identity_is_verified(Some(&alice), None));
        // A stored user NAMED `$anonymous` is not the anonymous principal.
        let named_anon = AuthenticatedUser {
            name: crate::iam::types::ANONYMOUS_USER_NAME.into(),
            access_key_id: "AKREAL".into(),
            permissions: vec![],
            iam_policies: vec![],
        };
        assert!(!resolved_identity_is_verified(Some(&named_anon), None));
        // SigV2 query signed by another key.
        assert!(!resolved_identity_is_verified(
            Some(&alice),
            Some("AKMALLORY")
        ));
        assert!(!resolved_identity_is_verified(Some(&alice), Some("")));
        assert!(!resolved_identity_is_verified(
            Some(&alice),
            Some("AKALICE ")
        ));
    }

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
        // Only a 64-char hex is a verifiable single body hash. UNSIGNED and
        // STREAMING variants are NOT verifiable here — streaming chunk sigs are
        // enforced by s3s upstream, so they must NOT be rejected (X-ray H15).
        let hex = SignedPayloadHash("a".repeat(64));
        assert!(hex.is_verifiable_hex());

        let unsigned = SignedPayloadHash("UNSIGNED-PAYLOAD".into());
        assert!(!unsigned.is_verifiable_hex());
        assert!(unsigned.verify_against_body(b"anything").is_ok());

        let signed_stream = SignedPayloadHash("STREAMING-AWS4-HMAC-SHA256-PAYLOAD".into());
        assert!(!signed_stream.is_verifiable_hex());
        // The former bug: this returned NotImplemented (→ 501) under auth.
        assert!(
            signed_stream
                .verify_against_body(b"chunk-decoded-body")
                .is_ok(),
            "signed streaming payload must be accepted (verified upstream by s3s)"
        );
    }
    #[test]
    fn test_has_presigned_query_params() {
        assert!(has_presigned_query_params(
            "X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=foo&X-Amz-Signature=ab"
        ));
        assert!(!has_presigned_query_params("list-type=2&prefix=test"));
        assert!(!has_presigned_query_params(""));
        // Should not match substring (e.g. a value containing "X-Amz-Signature=")
        assert!(!has_presigned_query_params("foo=X-Amz-Signature%3Dbar"));
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

    // ── replay tracking (beshu-tech/deltaglider_proxy#24, S23) ──
    //
    // Only mutating requests take part; GET/HEAD never touch the cache.

    #[test]
    fn replay_tracks_mutations_only() {
        use axum::http::Method;
        assert!(!replay_tracked(&Method::GET));
        assert!(!replay_tracked(&Method::HEAD));
        for m in [
            Method::PUT,
            Method::POST,
            Method::DELETE,
            Method::PATCH,
            Method::OPTIONS,
        ] {
            assert!(replay_tracked(&m), "{m} must be replay-tracked");
        }
    }

    #[test]
    fn replay_window_defaults_to_the_clock_skew() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |n: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == n)
                    .map(|(_, v)| v.to_string())
            }
        };
        let secs = |f: &dyn Fn(&str) -> Option<String>| replay_window_from(f).as_secs();
        assert_eq!(secs(&env(&[])), 900);
        assert_eq!(secs(&env(&[("DGP_CLOCK_SKEW_SECONDS", "300")])), 300);
        assert_eq!(secs(&env(&[("DGP_REPLAY_WINDOW_SECS", "0")])), 0);
        assert_eq!(
            secs(&env(&[
                ("DGP_REPLAY_WINDOW_SECS", "60"),
                ("DGP_CLOCK_SKEW_SECONDS", "300")
            ])),
            60
        );
        // Blank or invalid = unset.
        assert_eq!(secs(&env(&[("DGP_REPLAY_WINDOW_SECS", "")])), 900);
    }

    #[test]
    fn same_second_retry_is_served_for_put_and_delete_only() {
        use axum::http::Method;
        let now = Duration::from_millis(0);
        let in_second = Duration::from_millis(999);
        let after = Duration::from_millis(1000);
        for m in [Method::PUT, Method::DELETE] {
            assert!(same_second_retry_served(&m, now), "{m}");
            assert!(same_second_retry_served(&m, in_second), "{m}");
            assert!(
                !same_second_retry_served(&m, after),
                "{m} replayed after the second"
            );
        }
        // POST (CompleteMultipartUpload, DeleteObjects, form upload) and
        // the rest stay strict: any duplicate is a replay.
        for m in [Method::POST, Method::PATCH] {
            assert!(!same_second_retry_served(&m, now), "{m}");
        }
    }

    #[test]
    fn replay_slot_is_kept_only_on_success() {
        use axum::http::StatusCode;
        assert!(replay_slot_kept(StatusCode::OK));
        assert!(replay_slot_kept(StatusCode::NO_CONTENT));
        assert!(replay_slot_kept(StatusCode::NOT_MODIFIED));
        assert!(!replay_slot_kept(StatusCode::SERVICE_UNAVAILABLE));
        assert!(!replay_slot_kept(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(!replay_slot_kept(StatusCode::FORBIDDEN));
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

#[cfg(test)]
mod review3_tests {
    use super::*;

    /// With the 900 s window, legit traffic above ~555 mutations/s keeps the
    /// cache over the cap. The inline prune cuts to EXACTLY the cap, so the
    /// next insert is over it again: every later mutation runs an O(500k)
    /// retain + clone + select on the request path.
    #[test]
    fn review3_hard_cap_prune_leaves_headroom() {
        let cache: ReplayCache = Arc::new(DashMap::new());
        for i in 0..101 {
            cache.insert(format!("sig-{i}"), Instant::now());
        }
        prune_replay_cache(&cache, Duration::from_secs(900), 100);
        cache.insert("next".into(), Instant::now());
        assert!(
            cache.len() <= 100,
            "one insert after a prune is over the cap again ({}), so the next request prunes again",
            cache.len()
        );
    }

    /// S23: `DGP_REPLAY_WINDOW_SECS=0` is the off switch, but every
    /// mutation still stored its signature: the cache filled up to the cap
    /// and the inline prune ran on the request path, for a check that never
    /// fires.
    #[test]
    fn review3_a_zero_window_stores_nothing() {
        let cache: ReplayCache = Arc::new(DashMap::new());
        for i in 0..3 {
            assert_ne!(
                claim_replay_slot(
                    &cache,
                    &format!("sig-{i}"),
                    &axum::http::Method::PUT,
                    Duration::ZERO
                ),
                ReplayClaim::Duplicate
            );
        }
        assert_eq!(cache.len(), 0, "the off switch must store no signature");
    }
}
