// SPDX-License-Identifier: GPL-3.0-only

//! Per-backend CONNECTIVITY / AUTH health verdicts.
//!
//! The invariant this module enforces: *a backend that cannot be reached or
//! whose credentials are rejected is a FAULT that announces itself* — at boot
//! (probe every configured backend; all dead → refuse to start), at apply
//! (probe changed backends), and at request time (buckets routed to an
//! unhealthy backend answer an honest 503 naming the backend and cause,
//! instead of per-request timeout storms or misleading 404s).
//!
//! Sibling of [`super::capability`]: same name→(fingerprint, verdict) cache
//! idiom (a redefined backend — rotated creds, new endpoint — misses the
//! cache and re-probes), same snapshot→`GET /backends`→GUI surfacing path.
//! Capability answers "does this backend enforce conditional writes?";
//! health answers "can we talk to it at all?".

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::Request;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use super::capability::fingerprint;
use crate::config::BackendConfig;

/// How long one health-probe call may run before it counts as Unreachable.
const HEALTH_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Attempts per probe (transport/5xx failures retry once; an auth rejection
/// is definitive and never retried).
const HEALTH_PROBE_ATTEMPTS: u32 = 2;

/// Monotonic counter bumped on every health-verdict CHANGE — the
/// `IAM_VERSION` pattern: lets tests poll for "the re-probe loop noticed"
/// instead of sleeping, and lets the GUI cheap-poll for transitions.
static BACKEND_HEALTH_VERSION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn bump_backend_health_version() -> u64 {
    BACKEND_HEALTH_VERSION.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
}

pub fn current_backend_health_version() -> u64 {
    BACKEND_HEALTH_VERSION.load(std::sync::atomic::Ordering::SeqCst)
}

/// One backend's probed connection health.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum HealthVerdict {
    /// An authenticated call succeeded.
    Healthy,
    /// The backend answered and rejected our credentials.
    AuthRejected { detail: String },
    /// The endpoint could not be reached (DNS / connect / TLS / timeout).
    Unreachable { detail: String },
    /// Reachable, but answering with server errors (5xx / persistent throttle).
    Erroring { detail: String },
}

impl HealthVerdict {
    pub fn is_healthy(&self) -> bool {
        matches!(self, HealthVerdict::Healthy)
    }

    /// Should this verdict GATE requests (503)? Only definitive
    /// connection-level faults do. `Erroring` (reachable but 5xx/throttling)
    /// does NOT gate — a throttling backend still serves most requests, and
    /// gating would turn partial degradation into a self-inflicted full
    /// outage. Erroring surfaces via badge + logs only.
    pub fn is_gating(&self) -> bool {
        matches!(
            self,
            HealthVerdict::AuthRejected { .. } | HealthVerdict::Unreachable { .. }
        )
    }

    /// One-line operator-facing cause, used verbatim in logs, 503 bodies and
    /// apply rejections so they can never drift.
    pub fn cause(&self) -> String {
        match self {
            HealthVerdict::Healthy => "connection healthy".to_string(),
            HealthVerdict::AuthRejected { detail } => {
                format!("credentials rejected ({detail}) — check access_key_id / secret_access_key")
            }
            HealthVerdict::Unreachable { detail } => {
                format!("endpoint unreachable ({detail}) — check endpoint / network / DNS")
            }
            HealthVerdict::Erroring { detail } => {
                format!("backend erroring ({detail}) — the service is up but failing requests")
            }
        }
    }
}

/// Failure class of one probe call. Pure-classifier target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeFailure {
    AuthRejected,
    Unreachable,
    Erroring,
}

/// PURE: classify a failed probe call from its extracted signal —
/// (transport-level?, HTTP status, AWS error code). Unit-tested truth table;
/// never feed it debug strings (see `sdk_error_signal`'s poisoning warning).
pub fn classify_probe_signal(
    transport: bool,
    status: Option<u16>,
    code: Option<&str>,
) -> ProbeFailure {
    if transport {
        return ProbeFailure::Unreachable;
    }
    if let Some(c) = code {
        if matches!(
            c,
            "InvalidAccessKeyId"
                | "SignatureDoesNotMatch"
                | "AccessDenied"
                | "AccountProblem"
                | "InvalidSecurity"
                | "ExpiredToken"
                | "TokenRefreshRequired"
                | "InvalidClientTokenId"
                | "AuthorizationHeaderMalformed"
        ) {
            return ProbeFailure::AuthRejected;
        }
    }
    match status {
        Some(401) | Some(403) => ProbeFailure::AuthRejected,
        Some(s) if s >= 500 => ProbeFailure::Erroring,
        Some(429) => ProbeFailure::Erroring,
        _ => ProbeFailure::Erroring,
    }
}

/// HARD auth codes: unambiguous "these credentials are wrong". A bare 403 /
/// `AccessDenied` is NOT hard — Ceph-family backends answer 403 for buckets
/// that don't exist (anti-enumeration), and bucket-scoped keys legally get
/// AccessDenied on out-of-scope calls.
fn is_hard_auth_code(code: Option<&str>) -> bool {
    matches!(
        code,
        Some(
            "InvalidAccessKeyId"
                | "SignatureDoesNotMatch"
                | "ExpiredToken"
                | "InvalidClientTokenId"
                | "AuthorizationHeaderMalformed"
                | "InvalidSecurity"
                | "TokenRefreshRequired"
        )
    )
}

/// Structured signal from a typed SDK error: (transport?, status, code).
/// Mirror of `config_db_sync::sdk_error_signal`, kept structured instead of
/// stringified so the classifier match can't be poisoned by endpoint text.
fn sdk_probe_signal<E>(e: &aws_sdk_s3::error::SdkError<E>) -> (bool, Option<u16>, Option<String>)
where
    E: aws_sdk_s3::error::ProvideErrorMetadata,
{
    use aws_sdk_s3::error::ProvideErrorMetadata;
    let code = e.code().map(str::to_string);
    match e {
        aws_sdk_s3::error::SdkError::ServiceError(svc) => {
            (false, Some(svc.raw().status().as_u16()), code)
        }
        _ => (true, None, code),
    }
}

/// A health verdict plus when it was established (unix seconds) — the GUI's
/// "last probed 2m ago".
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct HealthEntry {
    #[serde(flatten)]
    pub verdict: HealthVerdict,
    pub probed_at: i64,
}

/// Thread-safe backend-name → health map, `Arc`-shared into `AppState`.
/// Fingerprint-keyed like [`super::BackendCapabilityCache`]: a redefined
/// backend under the same name misses and re-probes.
#[derive(Debug, Default)]
pub struct BackendHealthCache {
    entries: parking_lot::RwLock<HashMap<String, (String, HealthEntry)>>,
}

impl BackendHealthCache {
    /// Record a verdict. Bumps the health version ONLY on change, so pollers
    /// wake on transitions, not on every steady-state re-probe.
    pub fn set(&self, backend: &str, config: &BackendConfig, verdict: HealthVerdict) {
        let entry = HealthEntry {
            verdict,
            probed_at: chrono::Utc::now().timestamp(),
        };
        let fp = fingerprint(config);
        let mut map = self.entries.write();
        let changed = map
            .get(backend)
            .map(|(old_fp, old)| *old_fp != fp || old.verdict != entry.verdict)
            .unwrap_or(true);
        map.insert(backend.to_string(), (fp, entry));
        drop(map);
        if changed {
            bump_backend_health_version();
        }
    }

    /// Verdict for this backend NAME, only if established against this exact
    /// backend DEFINITION. `None` = never probed or definition changed.
    pub fn get(&self, backend: &str, config: &BackendConfig) -> Option<HealthVerdict> {
        self.entries
            .read()
            .get(backend)
            .filter(|(fp, _)| *fp == fingerprint(config))
            .map(|(_, e)| e.verdict.clone())
    }

    /// Snapshot for the admin backends API (name → entry).
    pub fn snapshot(&self) -> HashMap<String, HealthEntry> {
        self.entries
            .read()
            .iter()
            .map(|(k, (_, e))| (k.clone(), e.clone()))
            .collect()
    }

    /// Names of currently-unhealthy backends (the request-gate fast path:
    /// empty = zero per-request overhead beyond one read-lock).
    pub fn unhealthy_names(&self) -> Vec<String> {
        self.entries
            .read()
            .iter()
            .filter(|(_, (_, e))| !e.verdict.is_healthy())
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// The unhealthy verdict for a backend name, if any (request gate lookup).
    pub fn unhealthy_verdict(&self, backend: &str) -> Option<HealthVerdict> {
        self.entries
            .read()
            .get(backend)
            .map(|(_, e)| e.verdict.clone())
            .filter(|v| !v.is_healthy())
    }

    /// Drop entries for backends no longer in the config (post-apply hygiene).
    pub fn retain_backends(&self, names: &std::collections::BTreeSet<String>) {
        self.entries.write().retain(|k, _| names.contains(k));
    }
}

/// Probe one backend definition's connectivity + auth.
///
/// S3: an authenticated `ListBuckets` under [`HEALTH_PROBE_TIMEOUT`]. If it is
/// DENIED, fall back to `HeadBucket` on `fallback_bucket` — bucket-scoped
/// application keys (Backblaze B2) legitimately cannot ListBuckets, and a 404
/// there still proves the credentials work (authenticated + bucket absent).
/// Filesystem: the root path must exist and be a directory.
/// ponytail: fs probe is exists+is_dir; add a write test if silent read-only
/// mounts ever bite.
pub async fn probe_backend_health(
    config: &BackendConfig,
    fallback_bucket: Option<&str>,
) -> HealthVerdict {
    match config {
        // Mirror FilesystemBackend::new: the engine CREATES the root dir on
        // build, so a not-yet-existing path is a healthy backend-to-be — the
        // probe must create it too, or adding a fresh filesystem backend via
        // apply would always be rejected as "unreachable".
        BackendConfig::Filesystem { path, .. } => match tokio::fs::create_dir_all(path).await {
            Ok(()) => HealthVerdict::Healthy,
            Err(e) => HealthVerdict::Unreachable {
                detail: format!("{}: {e}", path.display()),
            },
        },
        BackendConfig::S3 { .. } => {
            let client = match crate::config_db_sync::ConfigDbSync::build_client(config).await {
                Ok(c) => c,
                Err(e) => {
                    return HealthVerdict::Unreachable {
                        detail: format!("client build failed: {e}"),
                    }
                }
            };
            let mut last = HealthVerdict::Unreachable {
                detail: "probe never ran".to_string(),
            };
            for attempt in 0..HEALTH_PROBE_ATTEMPTS {
                if attempt > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
                last = probe_s3_once(&client, fallback_bucket).await;
                match &last {
                    // Definitive either way — no retry.
                    HealthVerdict::Healthy | HealthVerdict::AuthRejected { .. } => break,
                    // Transport/5xx: retry once — a single blip must not
                    // gate a backend for the next 30s.
                    _ => {}
                }
            }
            last
        }
    }
}

async fn probe_s3_once(
    client: &aws_sdk_s3::Client,
    fallback_bucket: Option<&str>,
) -> HealthVerdict {
    match tokio::time::timeout(HEALTH_PROBE_TIMEOUT, client.list_buckets().send()).await {
        Err(_) => HealthVerdict::Unreachable {
            detail: format!("probe timed out ({}s)", HEALTH_PROBE_TIMEOUT.as_secs()),
        },
        Ok(Ok(_)) => HealthVerdict::Healthy,
        Ok(Err(e)) => {
            let (transport, status, code) = sdk_probe_signal(&e);
            let failure = classify_probe_signal(transport, status, code.as_deref());
            let detail = format!(
                "status={} code={}",
                status.map(|s| s.to_string()).unwrap_or_else(|| "-".into()),
                code.as_deref().unwrap_or("-")
            );
            if failure == ProbeFailure::AuthRejected {
                // A HARD auth code on ListBuckets is definitive — no fallback.
                if is_hard_auth_code(code.as_deref()) {
                    return failure_verdict(failure, detail);
                }
                // Soft denial (AccessDenied / bare 403): could be a
                // bucket-SCOPED key (B2 app keys can't ListBuckets). HeadBucket
                // on a routed bucket disambiguates.
                if let Some(bucket) = fallback_bucket {
                    match tokio::time::timeout(
                        HEALTH_PROBE_TIMEOUT,
                        client.head_bucket().bucket(bucket).send(),
                    )
                    .await
                    {
                        // Answered (even 404 = authenticated, bucket absent).
                        Ok(Ok(_)) => return HealthVerdict::Healthy,
                        Ok(Err(he)) => {
                            let (t, s, c) = sdk_probe_signal(&he);
                            if t {
                                return HealthVerdict::Unreachable {
                                    detail: "transport error on fallback probe".into(),
                                };
                            }
                            if is_hard_auth_code(c.as_deref()) {
                                return failure_verdict(
                                    ProbeFailure::AuthRejected,
                                    format!(
                                        "status={} code={}",
                                        s.map(|x| x.to_string()).unwrap_or_else(|| "-".into()),
                                        c.as_deref().unwrap_or("-")
                                    ),
                                );
                            }
                            if matches!(s, Some(x) if x >= 500) {
                                return HealthVerdict::Erroring {
                                    detail: format!("status={} on fallback probe", s.unwrap()),
                                };
                            }
                            // 404 = authenticated + bucket absent. A soft 403
                            // is ALSO not proof of broken creds: Ceph-family
                            // backends answer 403 for buckets that simply
                            // don't exist (anti-enumeration), and scoped keys
                            // legally get AccessDenied out of scope. FAIL OPEN
                            // — never 503 a backend on ambiguous evidence.
                            return HealthVerdict::Healthy;
                        }
                        Err(_) => {
                            return HealthVerdict::Unreachable {
                                detail: format!(
                                    "probe timed out ({}s)",
                                    HEALTH_PROBE_TIMEOUT.as_secs()
                                ),
                            }
                        }
                    }
                }
                // No routed bucket to disambiguate with — a soft denial is
                // not proof of broken creds. Fail open.
                return HealthVerdict::Healthy;
            }
            failure_verdict(failure, detail)
        }
    }
}

fn failure_verdict(failure: ProbeFailure, detail: String) -> HealthVerdict {
    match failure {
        ProbeFailure::AuthRejected => HealthVerdict::AuthRejected { detail },
        ProbeFailure::Unreachable => HealthVerdict::Unreachable { detail },
        ProbeFailure::Erroring => HealthVerdict::Erroring { detail },
    }
}

/// All backends to health-probe: the default backend under its synthesized
/// name `"default"` (matching the admin backends API) + every named backend.
/// For each, a fallback HeadBucket target: the alias-resolved real name of the
/// first bucket routed to it (scoped-key disambiguation).
pub fn probe_targets(
    config: &crate::config::Config,
) -> Vec<(String, BackendConfig, Option<String>)> {
    let mut out = Vec::new();
    let fallback_for = |name: Option<&str>| {
        config
            .buckets
            .iter()
            .find(|(_, p)| p.backend.as_deref() == name)
            .map(|(b, p)| p.alias.clone().unwrap_or_else(|| b.clone()))
    };
    out.push((
        "default".to_string(),
        config.backend.clone(),
        fallback_for(None),
    ));
    for named in &config.backends {
        out.push((
            named.name.clone(),
            named.backend.clone(),
            fallback_for(Some(named.name.as_str())),
        ));
    }
    out
}

/// Boot policy for the health gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootProbeMode {
    /// Probe; ALL backends unhealthy → exit(1). (Default.)
    Enforce,
    /// Probe + log, never exit.
    Warn,
    /// Skip probing entirely.
    Off,
}

/// `DGP_BOOT_BACKEND_PROBE` = enforce (default) | warn | off.
pub fn boot_probe_mode() -> BootProbeMode {
    match crate::config::env_parse::<String>("DGP_BOOT_BACKEND_PROBE")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "off" => BootProbeMode::Off,
        "warn" => BootProbeMode::Warn,
        _ => BootProbeMode::Enforce,
    }
}

// ── Request gate ─────────────────────────────────────────────────────────────

/// Extension carried by the S3 router: the health cache + the config handle
/// used to resolve bucket → backend name. Bucket resolution takes the config
/// read lock ONLY while at least one backend is unhealthy (the healthy fast
/// path is a single RwLock read of an empty predicate).
#[derive(Clone)]
pub struct BackendHealthGate {
    pub health: Arc<BackendHealthCache>,
    pub config: crate::config::SharedConfig,
}

/// Axum middleware on the S3 router: requests to a bucket whose backend is
/// UNHEALTHY answer a fast honest 503 naming the backend and cause — all
/// verbs (a read against a dead backend fails anyway; this replaces the
/// per-request timeout storm with an actionable error). Recovery lag is
/// bounded by the re-probe loop (~30s). Buckets on healthy or never-probed
/// backends always pass (fail-open — only a definitive verdict gates).
pub async fn backend_health_gate_middleware(request: Request<Body>, next: Next) -> Response {
    let Some(gate) = request.extensions().get::<BackendHealthGate>().cloned() else {
        return next.run(request).await;
    };
    // Fast path: nothing unhealthy → pass without touching the config lock.
    let unhealthy = gate.health.unhealthy_names();
    if unhealthy.is_empty() {
        return next.run(request).await;
    }
    let Some(bucket) = crate::maintenance::gate::bucket_from_path(request.uri().path()) else {
        return next.run(request).await;
    };
    // Resolve the bucket's backend NAME + DEFINITION, then consult the cache
    // fingerprint-checked: a verdict established against a DIFFERENT
    // definition (e.g. a rejected apply's probe, or a pre-rotation entry)
    // must never gate the currently-running one. Miss = fail-open.
    //
    // try_read (never await the lock): a config APPLY holds the write lock —
    // and an apply is most likely exactly while a backend is unhealthy (the
    // operator fixing it). Queueing every S3 request behind that writer
    // would be a self-inflicted global stall; failing open for the apply's
    // duration just restores pre-gate behavior for a few seconds.
    let resolved = match gate.config.try_read() {
        Err(_) => return next.run(request).await,
        Ok(cfg) => match cfg.buckets.get(&bucket).and_then(|p| p.backend.clone()) {
            // The literal "default" is the synthesized singleton name
            // (accepted by check_fatal + the admin API) — same target as an
            // unrouted bucket.
            Some(name) if name == "default" => Some(("default".to_string(), cfg.backend.clone())),
            Some(name) => cfg
                .backends
                .iter()
                .find(|b| b.name == name)
                .map(|b| (name.clone(), b.backend.clone())),
            None => Some(("default".to_string(), cfg.backend.clone())),
        },
    };
    let Some((backend_name, backend_cfg)) = resolved else {
        // Route to an undefined backend: unreachable in practice (check_fatal
        // blocks it at boot + apply) — fail-open rather than double-enforce.
        return next.run(request).await;
    };
    if let Some(verdict) = gate.health.get(&backend_name, &backend_cfg) {
        if verdict.is_gating() {
            return crate::api::errors::S3Error::ServiceUnavailable(format!(
                "bucket '{bucket}' is on backend '{backend_name}', which is currently \
                 unavailable: {}. Requests are blocked until the backend recovers \
                 (re-checked every 30s); see Storage → Backends for live status",
                verdict.cause()
            ))
            .into_response();
        }
    }
    next.run(request).await
}

/// Hot-apply pre-commit HEALTH gate: probe backends whose DEFINITION changed
/// (fingerprint miss against the cache) and refuse the transition when the
/// probe fails — "Test connection" semantics built into every apply. Backends
/// with an unchanged definition are never re-probed here, and an EXISTING
/// unhealthy backend does not block unrelated applies (only a changed
/// definition must prove itself).
pub async fn hot_apply_health_gate(
    new_config: &crate::config::Config,
    cache: &BackendHealthCache,
) -> Result<(), String> {
    if boot_probe_mode() == BootProbeMode::Off {
        return Ok(());
    }
    for (name, backend, fallback) in probe_targets(new_config) {
        if cache.get(&name, &backend).is_some() {
            continue; // same definition, verdict already established
        }
        let verdict = probe_backend_health(&backend, fallback.as_deref()).await;
        if !verdict.is_healthy() {
            // Do NOT cache: the apply is rejected, so this definition never
            // goes live — caching it would overwrite the RUNNING definition's
            // entry under the same name and paint a healthy backend red in
            // the GUI (the snapshot is not fingerprint-checked).
            return Err(format!(
                "config refused: backend '{name}' failed its connection probe — {}. \
                 Fix the endpoint/credentials and re-apply (nothing was changed)",
                verdict.cause()
            ));
        }
        cache.set(&name, &backend, verdict);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_probe_signal_truth_table() {
        use ProbeFailure::*;
        // Transport always wins — even with an auth-looking code attached.
        assert_eq!(classify_probe_signal(true, None, None), Unreachable);
        assert_eq!(
            classify_probe_signal(true, None, Some("InvalidAccessKeyId")),
            Unreachable
        );
        // Auth codes are definitive regardless of status.
        for code in [
            "InvalidAccessKeyId",
            "SignatureDoesNotMatch",
            "AccessDenied",
            "ExpiredToken",
        ] {
            assert_eq!(
                classify_probe_signal(false, Some(400), Some(code)),
                AuthRejected,
                "{code}"
            );
        }
        // Bare 401/403 without a code → auth.
        assert_eq!(classify_probe_signal(false, Some(403), None), AuthRejected);
        assert_eq!(classify_probe_signal(false, Some(401), None), AuthRejected);
        // 5xx / 429 / anything else service-side → erroring.
        assert_eq!(classify_probe_signal(false, Some(503), None), Erroring);
        assert_eq!(classify_probe_signal(false, Some(500), None), Erroring);
        assert_eq!(classify_probe_signal(false, Some(429), None), Erroring);
        assert_eq!(
            classify_probe_signal(false, Some(400), Some("MalformedXML")),
            Erroring
        );
    }

    #[test]
    fn health_cache_fingerprint_and_version_semantics() {
        let cache = BackendHealthCache::default();
        let cfg = BackendConfig::S3 {
            endpoint: Some("https://b2.example".into()),
            region: "eu-central-003".into(),
            force_path_style: true,
            access_key_id: Some("k".into()),
            secret_access_key: Some("s".into()),
            allow_local: true,
        };
        assert_eq!(cache.get("b2", &cfg), None);
        let v0 = current_backend_health_version();
        cache.set("b2", &cfg, HealthVerdict::Healthy);
        assert!(current_backend_health_version() > v0, "first set bumps");
        let v1 = current_backend_health_version();
        // Steady-state re-probe with the same verdict does NOT bump.
        cache.set("b2", &cfg, HealthVerdict::Healthy);
        assert_eq!(current_backend_health_version(), v1);
        // Transition bumps.
        cache.set(
            "b2",
            &cfg,
            HealthVerdict::AuthRejected { detail: "x".into() },
        );
        assert!(current_backend_health_version() > v1);
        assert_eq!(cache.unhealthy_names(), vec!["b2".to_string()]);
        assert!(cache.unhealthy_verdict("b2").is_some());
        // A redefined backend (rotated secret) misses the cache.
        let rotated = BackendConfig::S3 {
            endpoint: Some("https://b2.example".into()),
            region: "eu-central-003".into(),
            force_path_style: true,
            access_key_id: Some("k".into()),
            secret_access_key: Some("s2".into()),
            allow_local: true,
        };
        assert_eq!(cache.get("b2", &rotated), None, "rotation → miss");
    }

    #[test]
    fn probe_targets_covers_default_and_named_with_fallback_buckets() {
        let cfg = crate::config::Config::from_yaml_str(
            r#"
storage:
  backends:
    - name: b2
      type: s3
      endpoint: "http://127.0.0.1:1"
      region: eu-central-003
      access_key_id: x
      secret_access_key: y
  buckets:
    mirror: { backend: b2, alias: real-mirror }
    plain: {}
"#,
        )
        .expect("fixture parses");
        let targets = probe_targets(&cfg);
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].0, "default");
        assert_eq!(
            targets[0].2.as_deref(),
            Some("plain"),
            "default's fallback = first bucket routed to it"
        );
        assert_eq!(targets[1].0, "b2");
        assert_eq!(
            targets[1].2.as_deref(),
            Some("real-mirror"),
            "alias-resolved real bucket"
        );
    }

    #[test]
    fn boot_probe_mode_parses() {
        // No env manipulation (cross-test contamination) — just the default.
        assert_eq!(boot_probe_mode(), BootProbeMode::Enforce);
    }

    #[test]
    fn cause_lines_name_the_fix() {
        let v = HealthVerdict::AuthRejected {
            detail: "status=403 code=InvalidAccessKeyId".into(),
        };
        assert!(v.cause().contains("credentials rejected"));
        assert!(v.cause().contains("secret_access_key"));
        let v = HealthVerdict::Unreachable {
            detail: "dns".into(),
        };
        assert!(v.cause().contains("endpoint unreachable"));
    }
}
