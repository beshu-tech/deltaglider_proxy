// SPDX-License-Identifier: BUSL-1.1

//! Per-backend CONNECTIVITY / AUTH health verdicts.
//!
//! The invariant this module enforces: *a backend that cannot be reached or
//! whose credentials are rejected is a FAULT that announces itself* — at boot
//! (probe every configured backend; all dead → refuse to start), at apply
//! (probe changed backends), and at request time (buckets routed to an
//! unhealthy backend answer an honest 503 naming the backend and cause,
//! instead of per-request timeout storms or misleading 404s).
//!
//! The probe measures reachability, not latency: a HeadBucket on a bucket
//! routed to the backend (ListBuckets when none is routed), with the data
//! path's request deadline, on one long-lived client per definition. Only
//! a connect-level fault (DNS, refused connection, TLS, connect timeout) or
//! an auth rejection gates. A backend that answers slowly, or with 5xx, is
//! reachable and stays open.
//!
//! Only an ACTIVE probe sets a verdict that gates. A failed client request
//! (a timeout, a refused connection) is a PASSIVE signal: it makes the
//! backend "suspect" and starts one confirmation probe of that backend
//! ([`report_unavailable`]). The probe decides. When it fails, the backend
//! gates at once, so a dead backend is closed within one probe round-trip
//! of its first failed request. When it answers, the buckets stay open: one
//! slow request among thousands is partial degradation, and gating on it
//! closes every bucket of the backend until the next probe.
//!
//! Sibling of [`super::capability`]: same name→(fingerprint, verdict) cache
//! idiom (a redefined backend — rotated creds, new endpoint — misses the
//! cache and re-probes), same snapshot→`GET /backends`→GUI surfacing path.
//! Capability answers "does this backend enforce conditional writes?";
//! health answers "can we talk to it at all?".

use crate::storage::StorageBackend;
use std::collections::HashMap;
use std::sync::Arc;

use crate::api::errors::S3Error;

use super::capability::fingerprint;
use crate::config::BackendConfig;

/// The probe deadline when `DGP_BACKEND_REQUEST_TIMEOUT_SECS` is off.
const DEFAULT_HEALTH_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The deadline of one probe attempt: the data path's request deadline
/// (`DGP_BACKEND_REQUEST_TIMEOUT_SECS`, 30 s by default). A backend whose
/// requests answer in time answers its probe in time. The 5 s deadline
/// before read a slow backend (Hetzner ListBuckets) as down, and gated it.
fn health_probe_timeout() -> std::time::Duration {
    crate::storage::S3Backend::request_timeout().unwrap_or(DEFAULT_HEALTH_PROBE_TIMEOUT)
}

/// Attempts per probe. A fast failure (refused, 5xx) and an attempt that
/// ran out its deadline are retried once; an auth rejection is definitive.
const HEALTH_PROBE_ATTEMPTS: u32 = 2;

/// At most this many long-lived probe clients (one per definition).
const MAX_PROBE_CLIENTS: usize = 64;

/// After a confirmation probe finds a suspect backend answering, failed
/// requests start no new probe for this long. This bounds the probe rate
/// (and the log rate) of a backend that fails a share of its requests but
/// answers every probe.
const SUSPECT_PROBE_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(5);

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
/// Mirror of `coordination::cas::sdk_error_signal`, kept structured instead of
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
    entries: parking_lot::RwLock<HashMap<String, Stored>>,
    /// Backend name → passive suspicion. Drives the confirmation probe,
    /// never the gate.
    suspects: parking_lot::Mutex<HashMap<String, Suspicion>>,
}

/// One backend's verdict: the definition it is for, and when the probe
/// that established it started (a verdict never replaces a newer one).
#[derive(Debug, Clone)]
struct Stored {
    fp: String,
    entry: HealthEntry,
    started: std::time::Instant,
}

/// State of the confirmation probe of one suspect backend.
#[derive(Debug)]
enum Suspicion {
    /// The confirmation probe runs now; more failures join it.
    Probing,
    /// The last confirmation probe found the backend answering.
    Cleared(std::time::Instant),
}

impl BackendHealthCache {
    /// Record a verdict. Bumps the health version ONLY on change, so pollers
    /// wake on transitions, not on every steady-state re-probe. Returns
    /// whether the verdict changed.
    pub fn set(&self, backend: &str, config: &BackendConfig, verdict: HealthVerdict) -> bool {
        let now = std::time::Instant::now();
        self.record(backend, fingerprint(config), verdict, now, false) == Some(true)
    }

    /// Store `verdict` for definition `fp`, from a probe that started at
    /// `started`. A verdict of a probe that started later stays: a slow
    /// probe that ends last does not overwrite a newer answer. With
    /// `same_definition_only`, an entry of ANOTHER definition stays (a
    /// probe of a definition that an apply replaced while the probe ran
    /// must not paint the new one). Returns `None` when the verdict was not
    /// stored, else whether it changed (and bumped the health version).
    fn record(
        &self,
        backend: &str,
        fp: String,
        verdict: HealthVerdict,
        started: std::time::Instant,
        same_definition_only: bool,
    ) -> Option<bool> {
        let entry = HealthEntry {
            verdict,
            probed_at: chrono::Utc::now().timestamp(),
        };
        let mut map = self.entries.write();
        let changed = match map.get(backend) {
            Some(old) if same_definition_only && old.fp != fp => return None,
            Some(old) if old.started > started => return None,
            Some(old) => old.fp != fp || old.entry.verdict != entry.verdict,
            None => true,
        };
        map.insert(backend.to_string(), Stored { fp, entry, started });
        drop(map);
        if changed {
            bump_backend_health_version();
        }
        Some(changed)
    }

    /// Verdict for this backend NAME, only if established against this exact
    /// backend DEFINITION. `None` = never probed or definition changed.
    pub fn get(&self, backend: &str, config: &BackendConfig) -> Option<HealthVerdict> {
        self.entries
            .read()
            .get(backend)
            .filter(|s| s.fp == fingerprint(config))
            .map(|s| s.entry.verdict.clone())
    }

    /// Snapshot for the admin backends API (name → entry).
    pub fn snapshot(&self) -> HashMap<String, HealthEntry> {
        self.entries
            .read()
            .iter()
            .map(|(k, s)| (k.clone(), s.entry.clone()))
            .collect()
    }

    /// Names of currently-unhealthy backends (the request-gate fast path:
    /// empty = zero per-request overhead beyond one read-lock).
    pub fn unhealthy_names(&self) -> Vec<String> {
        self.entries
            .read()
            .iter()
            .filter(|(_, s)| !s.entry.verdict.is_healthy())
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// The unhealthy verdict for a backend name, if any (request gate lookup).
    pub fn unhealthy_verdict(&self, backend: &str) -> Option<HealthVerdict> {
        self.entries
            .read()
            .get(backend)
            .map(|s| s.entry.verdict.clone())
            .filter(|v| !v.is_healthy())
    }

    /// Passive signal: a request found the backend with definition
    /// fingerprint `fp` unavailable. The verdict does NOT change, so the
    /// signal never gates on its own; it makes the backend suspect.
    /// Returns `true` when the caller must run the confirmation probe
    /// ([`confirm_suspect`]). Returns `false` when the signal is absorbed:
    /// the entry belongs to ANOTHER definition (an engine still running a
    /// replaced definition must not drive the current one), the backend
    /// already gates (the re-probe loop owns its recovery), a confirmation
    /// probe already runs, or one cleared the backend less than
    /// `SUSPECT_PROBE_COOLDOWN` ago.
    #[must_use]
    pub fn mark_unavailable(&self, backend: &str, fp: &str, detail: &str) -> bool {
        if let Some(old) = self.entries.read().get(backend) {
            if old.fp != fp || old.entry.verdict.is_gating() {
                return false;
            }
        }
        let mut suspects = self.suspects.lock();
        match suspects.get(backend) {
            Some(Suspicion::Probing) => return false,
            Some(Suspicion::Cleared(at)) if at.elapsed() < SUSPECT_PROBE_COOLDOWN => return false,
            _ => {}
        }
        suspects.insert(backend.to_string(), Suspicion::Probing);
        drop(suspects);
        tracing::debug!("backend health: '{backend}' failed a request ({detail}) — probing it");
        true
    }

    /// End the suspicion of `backend` with the confirmation probe's
    /// `outcome`: the probed definition's fingerprint, its verdict and when
    /// the probe started. `None` = no probe ran, because the definition was
    /// replaced or removed. A gating verdict closes the buckets at once. A
    /// verdict that does not gate leaves them open and starts the cooldown.
    fn settle_suspect(
        &self,
        backend: &str,
        outcome: Option<(String, HealthVerdict, std::time::Instant)>,
        detail: &str,
    ) {
        let mut cooldown = false;
        if let Some((fp, verdict, started)) = outcome {
            let cause = verdict.cause();
            let (healthy, gating) = (verdict.is_healthy(), verdict.is_gating());
            // Not recorded = an apply replaced the definition while the
            // probe ran (the verdict says nothing about the new one), or a
            // newer probe answered first.
            if self.record(backend, fp, verdict, started, true).is_some() {
                cooldown = !gating;
                if healthy {
                    tracing::warn!(
                        "backend health: '{backend}' failed a request ({detail}), but it \
                         answered its health probe — its buckets stay open"
                    );
                } else if gating {
                    tracing::warn!(
                        "backend health: '{backend}' did not answer a request ({detail}), and \
                         its health probe failed: {cause} — its buckets answer 503 until a \
                         health probe succeeds"
                    );
                } else {
                    tracing::warn!(
                        "backend health: '{backend}' failed a request ({detail}); its health \
                         probe: {cause} — requests are not blocked"
                    );
                }
            }
        }
        let mut suspects = self.suspects.lock();
        if cooldown {
            let now = std::time::Instant::now();
            suspects.insert(backend.to_string(), Suspicion::Cleared(now));
        } else {
            suspects.remove(backend);
        }
    }

    /// Drop entries for backends no longer in the config (post-apply hygiene).
    pub fn retain_backends(&self, names: &std::collections::BTreeSet<String>) {
        self.entries.write().retain(|k, _| names.contains(k));
    }
}

/// The process's health cache and config, for passive signals from the
/// storage layer (an `S3Backend` has no handle on `AppState`). Installed
/// once at startup.
struct PassiveSink {
    health: Arc<BackendHealthCache>,
    config: crate::config::SharedConfig,
}

static PASSIVE_SINK: std::sync::OnceLock<PassiveSink> = std::sync::OnceLock::new();

/// Install the cache and config that [`note_unavailable`] uses (startup,
/// once). Install it only while the re-probe loop runs: the loop is what
/// reopens a backend that a confirmation probe gated.
pub fn install_passive_sink(health: Arc<BackendHealthCache>, config: crate::config::SharedConfig) {
    let _ = PASSIVE_SINK.set(PassiveSink { health, config });
}

/// A request found backend `name` (definition fingerprint `fp`) unavailable.
pub fn note_unavailable(name: &str, fp: &str, detail: &str) {
    if let Some(sink) = PASSIVE_SINK.get() {
        let _ = report_unavailable(&sink.health, &sink.config, name, fp, detail);
    }
}

/// [`note_unavailable`] with its sink passed in (the test seam). Marks the
/// backend suspect and, when this call claims the confirmation probe, runs
/// [`confirm_suspect`] on a task. At most one confirmation probe runs per
/// backend, so a burst of failed requests starts one probe, not one per
/// request. The handle lets a test wait for the verdict.
pub fn report_unavailable(
    health: &Arc<BackendHealthCache>,
    config: &crate::config::SharedConfig,
    name: &str,
    fp: &str,
    detail: &str,
) -> Option<tokio::task::JoinHandle<()>> {
    if !health.mark_unavailable(name, fp, detail) {
        return None;
    }
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        health.settle_suspect(name, None, detail); // no runtime, no probe
        return None;
    };
    let (health, config) = (health.clone(), config.clone());
    let (name, fp, detail) = (name.to_string(), fp.to_string(), detail.to_string());
    Some(runtime.spawn(async move {
        confirm_suspect(&health, &config, &name, &fp, &detail).await;
    }))
}

/// The confirmation probe of suspect backend `name`: the same probe as the
/// re-probe loop ([`probe_backend_health`]: `HEALTH_PROBE_ATTEMPTS`
/// attempts of at most `HEALTH_PROBE_TIMEOUT` each), on that one backend.
/// Its verdict is recorded like a loop round's, so a failed probe gates
/// and an answered probe leaves the buckets open. A backend whose
/// definition is no longer `fp` is not probed: the failed request ran
/// against a definition that an apply replaced.
pub async fn confirm_suspect(
    health: &BackendHealthCache,
    config: &crate::config::SharedConfig,
    name: &str,
    fp: &str,
    detail: &str,
) {
    let target = probe_targets(&*config.read().await)
        .into_iter()
        .find(|(n, backend, _)| n == name && fingerprint(backend) == fp);
    let outcome = match target {
        Some((_, backend, fallback)) => {
            let started = std::time::Instant::now();
            let verdict = probe_backend_health(&backend, fallback.as_deref()).await;
            Some((fp.to_string(), verdict, started))
        }
        None => None,
    };
    health.settle_suspect(name, outcome, detail);
}

/// `DGP_BACKEND_HEALTH_INTERVAL_SECS` (default 30; 0 turns the loop off):
/// how often every backend is health-probed. `DGP_BOOT_BACKEND_PROBE=off`
/// ("skip probing entirely") turns it off too.
pub fn health_probe_interval() -> Option<std::time::Duration> {
    if boot_probe_mode() == BootProbeMode::Off {
        return None;
    }
    match crate::config::env_parse_with_default("DGP_BACKEND_HEALTH_INTERVAL_SECS", 30u64) {
        0 => None,
        secs => Some(std::time::Duration::from_secs(secs)),
    }
}

/// One round of the health loop: probe EVERY configured backend, at the
/// same time, and record each verdict as its probe ends (a slow backend
/// does not hold back the others' verdicts). A healthy backend that hangs
/// or goes down turns unhealthy within one interval, with no request
/// needed; an unhealthy one recovers the same way.
///
/// A verdict is recorded only for the definition that is current when its
/// probe ends (an apply may replace or remove a backend while the round
/// runs), and never over a verdict of a probe that started later.
pub async fn reprobe_all(health: &BackendHealthCache, config: &crate::config::SharedConfig) {
    use futures::StreamExt;
    // Hygiene FIRST: drop entries for backends no longer in the config. A
    // stale unhealthy entry would pin the gate's slow path forever. Under
    // the read lock: an apply cannot record a new backend in between.
    let targets = {
        let cfg = config.read().await;
        let targets = probe_targets(&cfg);
        let current: std::collections::BTreeSet<String> =
            targets.iter().map(|(n, _, _)| n.clone()).collect();
        health.retain_backends(&current);
        targets
    };
    let mut probes: futures::stream::FuturesUnordered<_> = targets
        .into_iter()
        .map(|(name, backend, fallback)| async move {
            let started = std::time::Instant::now();
            let verdict = probe_backend_health(&backend, fallback.as_deref()).await;
            (name, fingerprint(&backend), verdict, started)
        })
        .collect();
    while let Some((name, fp, verdict, started)) = probes.next().await {
        let current = probe_targets(&*config.read().await)
            .into_iter()
            .find(|(n, _, _)| *n == name)
            .map(|(_, backend, _)| fingerprint(&backend));
        if current.as_deref() != Some(fp.as_str()) {
            continue; // replaced or removed while the probe ran
        }
        let was_unhealthy = health.unhealthy_verdict(&name).is_some();
        let (healthy, cause) = (verdict.is_healthy(), verdict.cause());
        if health.record(&name, fp, verdict, started, false).is_none() {
            continue; // a newer verdict is in place
        }
        if was_unhealthy && healthy {
            tracing::info!("backend health: '{name}' RECOVERED — gated buckets reopen");
        } else if !was_unhealthy && !healthy {
            tracing::warn!("backend health: '{name}' is unhealthy: {cause}");
        }
    }
}

/// Probe one backend definition's connectivity + auth.
///
/// S3: an authenticated `HeadBucket` on `fallback_bucket` (a bucket routed
/// to the backend), or `ListBuckets` when no bucket is routed, under
/// `health_probe_timeout`, on the definition's long-lived probe client
/// (`probe_client`). A 404 proves the credentials work (authenticated,
/// bucket absent). A HEAD answer carries no error body, so a 403 is
/// ambiguous (a scoped key, or a Ceph-family backend hiding the bucket): a
/// `ListBuckets` then tells a rejected credential (a hard auth code) from a
/// soft denial (fail open). Only a connect-level fault, a rejected
/// credential, or no answer within the request deadline on both attempts
/// (a hung backend) gates; a slow answer within the deadline or a 5xx is
/// `Erroring` or healthy, which does not.
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
            let client = match probe_client(config).await {
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
                let outcome = match fallback_bucket {
                    Some(bucket) => probe_head_bucket(&client, bucket).await,
                    None => probe_list_buckets(&client).await,
                };
                last = outcome.verdict;
                // A fast failure (refused, 5xx) is retried once — a single
                // blip must not gate a backend for the next 30s.
                if !outcome.retry {
                    break;
                }
            }
            last
        }
    }
}

/// The long-lived probe clients, one per backend definition (fingerprint).
/// A client per probe paid a new TCP and TLS handshake on every probe.
static PROBE_CLIENTS: std::sync::LazyLock<parking_lot::Mutex<HashMap<String, aws_sdk_s3::Client>>> =
    std::sync::LazyLock::new(Default::default);

/// The probe client of `config`'s definition, built on first use: the
/// SSRF-guarded client of `ConfigDbSync::build_client`, with one SDK
/// attempt (the probe makes its own), the data path's connect timeout, the
/// probe deadline, and a retry partition of its own (the probe never
/// spends a data path's retry quota, and never feeds its rate limiter).
async fn probe_client(config: &BackendConfig) -> Result<aws_sdk_s3::Client, String> {
    use aws_sdk_s3::config::retry::{RetryConfig, RetryPartition};
    use sha2::Digest;
    let fp = fingerprint(config);
    if let Some(client) = PROBE_CLIENTS.lock().get(&fp) {
        return Ok(client.clone());
    }
    let built = crate::config_db_sync::ConfigDbSync::build_client(config).await?;
    let connect = std::time::Duration::from_secs(crate::config::env_parse_with_default(
        "DGP_S3_CONNECT_TIMEOUT_SECS",
        10u64,
    ));
    // Named by a hash: the fingerprint holds the secret.
    let hash = hex::encode(sha2::Sha256::digest(fp.as_bytes()));
    let conf = built
        .config()
        .to_builder()
        .retry_config(RetryConfig::standard().with_max_attempts(1))
        .retry_partition(RetryPartition::custom(format!("dgp-health-{}", &hash[..12])).build())
        .timeout_config(
            aws_sdk_s3::config::timeout::TimeoutConfig::builder()
                .connect_timeout(connect)
                .disable_read_timeout()
                .operation_timeout(health_probe_timeout())
                .build(),
        )
        .build();
    let client = aws_sdk_s3::Client::from_conf(conf);
    let mut clients = PROBE_CLIENTS.lock();
    if clients.len() >= MAX_PROBE_CLIENTS {
        clients.clear();
    }
    clients.insert(fp, client.clone());
    Ok(client)
}

/// One probe attempt: its verdict, and whether a second attempt may
/// change it (a fast failure; not an answer, not a run-out deadline).
struct ProbeAttempt {
    verdict: HealthVerdict,
    retry: bool,
}

impl ProbeAttempt {
    fn done(verdict: HealthVerdict) -> Self {
        Self {
            verdict,
            retry: false,
        }
    }

    /// No answer within the deadline, which is the data path's own request
    /// deadline: a request to this backend would time out too. One such
    /// attempt can be a stall of a slow backend, so it is retried; when the
    /// second attempt gets no answer either, the backend is hung and gates
    /// (clients get a fast 503 instead of a 30 s wait each).
    fn slow() -> Self {
        Self {
            verdict: HealthVerdict::Unreachable {
                detail: format!(
                    "no answer within {}s on {HEALTH_PROBE_ATTEMPTS} attempts: the backend hangs",
                    health_probe_timeout().as_secs()
                ),
            },
            retry: true,
        }
    }
}

/// The attempt of a request that got no HTTP answer; `None` when it got
/// one. Only a dispatch failure (DNS, refused connection, TLS, the connect
/// timeout: the probe client sets no read timeout) is connect-level.
fn unanswered<E>(e: &aws_sdk_s3::error::SdkError<E>) -> Option<ProbeAttempt> {
    use aws_sdk_s3::error::SdkError;
    match e {
        SdkError::ServiceError(_) => None,
        SdkError::TimeoutError(_) => Some(ProbeAttempt::slow()),
        SdkError::ResponseError(_) => Some(ProbeAttempt {
            verdict: HealthVerdict::Erroring {
                detail: "the response broke off".into(),
            },
            retry: true,
        }),
        _ => Some(ProbeAttempt {
            verdict: HealthVerdict::Unreachable {
                detail: format!("no connection: {}", e),
            },
            retry: true,
        }),
    }
}

/// `status=… code=…` of a service error, for the verdict detail.
fn signal_detail(status: Option<u16>, code: Option<&str>) -> String {
    format!(
        "status={} code={}",
        status.map(|s| s.to_string()).unwrap_or_else(|| "-".into()),
        code.unwrap_or("-")
    )
}

/// The HeadBucket probe of a backend with a routed bucket.
async fn probe_head_bucket(client: &aws_sdk_s3::Client, bucket: &str) -> ProbeAttempt {
    let request = client.head_bucket().bucket(bucket).send();
    let Ok(result) = tokio::time::timeout(health_probe_timeout(), request).await else {
        return ProbeAttempt::slow();
    };
    let e = match result {
        Ok(_) => return ProbeAttempt::done(HealthVerdict::Healthy),
        Err(e) => e,
    };
    if let Some(attempt) = unanswered(&e) {
        return attempt;
    }
    let (_, status, code) = sdk_probe_signal(&e);
    if is_hard_auth_code(code.as_deref()) {
        return ProbeAttempt::done(HealthVerdict::AuthRejected {
            detail: signal_detail(status, code.as_deref()),
        });
    }
    match status {
        // A HEAD carries no error body: ask ListBuckets why.
        Some(401 | 403) => ProbeAttempt::done(list_buckets_names_bad_credentials(client).await),
        Some(s) if s >= 500 || s == 429 => ProbeAttempt {
            verdict: HealthVerdict::Erroring {
                detail: signal_detail(status, code.as_deref()),
            },
            retry: true,
        },
        // The bucket lives behind another endpoint or region.
        Some(301) => ProbeAttempt::done(HealthVerdict::Erroring {
            detail: signal_detail(status, code.as_deref()),
        }),
        // 404 = authenticated, bucket absent; any other answer proves the
        // backend is reachable.
        _ => ProbeAttempt::done(HealthVerdict::Healthy),
    }
}

/// After an ambiguous 403: `AuthRejected` when ListBuckets names a hard
/// auth code, else `Healthy`. A soft 403 is not proof of broken creds:
/// Ceph-family backends answer 403 for buckets that do not exist
/// (anti-enumeration), and scoped keys legally get AccessDenied out of
/// scope. FAIL OPEN — never 503 a backend on ambiguous evidence.
async fn list_buckets_names_bad_credentials(client: &aws_sdk_s3::Client) -> HealthVerdict {
    let request = client.list_buckets().send();
    if let Ok(Err(e)) = tokio::time::timeout(health_probe_timeout(), request).await {
        let (transport, status, code) = sdk_probe_signal(&e);
        if !transport && is_hard_auth_code(code.as_deref()) {
            return HealthVerdict::AuthRejected {
                detail: signal_detail(status, code.as_deref()),
            };
        }
    }
    HealthVerdict::Healthy
}

/// The ListBuckets probe of a backend with no routed bucket.
async fn probe_list_buckets(client: &aws_sdk_s3::Client) -> ProbeAttempt {
    let request = client.list_buckets().send();
    let Ok(result) = tokio::time::timeout(health_probe_timeout(), request).await else {
        return ProbeAttempt::slow();
    };
    let e = match result {
        Ok(_) => return ProbeAttempt::done(HealthVerdict::Healthy),
        Err(e) => e,
    };
    if let Some(attempt) = unanswered(&e) {
        return attempt;
    }
    let (_, status, code) = sdk_probe_signal(&e);
    let detail = signal_detail(status, code.as_deref());
    match classify_probe_signal(false, status, code.as_deref()) {
        // A HARD auth code is definitive.
        ProbeFailure::AuthRejected if is_hard_auth_code(code.as_deref()) => {
            ProbeAttempt::done(HealthVerdict::AuthRejected { detail })
        }
        // A soft denial (AccessDenied / bare 403) with no routed bucket to
        // disambiguate: a bucket-SCOPED key (B2 app keys) cannot
        // ListBuckets. Fail open.
        ProbeFailure::AuthRejected => ProbeAttempt::done(HealthVerdict::Healthy),
        failure => ProbeAttempt {
            retry: matches!(status, Some(s) if s >= 500 || s == 429),
            verdict: failure_verdict(failure, detail),
        },
    }
}

fn failure_verdict(failure: ProbeFailure, detail: String) -> HealthVerdict {
    match failure {
        ProbeFailure::AuthRejected => HealthVerdict::AuthRejected { detail },
        ProbeFailure::Unreachable => HealthVerdict::Unreachable { detail },
        ProbeFailure::Erroring => HealthVerdict::Erroring { detail },
    }
}

/// All backends to health-probe: the singleton under its synthesized name
/// `"default"` (matching the admin backends API) while no named backends
/// exist, else every named backend (the singleton is then unused). For each,
/// a fallback HeadBucket target: the alias-resolved real name of the first
/// bucket that routes to it (scoped-key disambiguation).
pub fn probe_targets(
    config: &crate::config::Config,
) -> Vec<(String, BackendConfig, Option<String>)> {
    let fallback_for = |name: &str| {
        config
            .buckets
            .iter()
            .find(|(b, _)| {
                config
                    .effective_backend_for_bucket(b)
                    .is_some_and(|(n, _)| n == name)
            })
            .map(|(b, p)| p.alias.clone().unwrap_or_else(|| b.clone()))
    };
    if config.backends.is_empty() {
        return vec![(
            "default".to_string(),
            config.backend.clone(),
            fallback_for("default"),
        )];
    }
    config
        .backends
        .iter()
        .map(|named| {
            (
                named.name.clone(),
                named.backend.clone(),
                fallback_for(&named.name),
            )
        })
        .collect()
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
    /// The router knows where an unrouted bucket actually lives.
    pub app: Arc<crate::api::handlers::AppState>,
}

/// The reserved-bucket gate: every verified S3 request to the coordination
/// bucket (`config_sync_bucket`) gets 403, whatever the identity. Runs with
/// the other post-verification gates (`check_verified_request`), so the
/// s3s access hook and the form-POST handler share it.
pub fn reserved_bucket_refusal(gate: &BackendHealthGate, path: &str) -> Option<S3Error> {
    let bucket = crate::maintenance::gate::bucket_from_path(path)?;
    gate.app
        .engine
        .load()
        .bucket_policy_registry()
        .reserved_bucket_reason(&bucket)
        .map(S3Error::AccessDeniedReason)
}

/// The S3 request gate: a request to a bucket whose backend is UNHEALTHY
/// gets a fast honest 503 naming the backend and cause, for all verbs (a
/// read against a dead backend fails anyway; this replaces the per-request
/// timeout storm with an actionable error). Recovery lag is bounded by the
/// re-probe loop (~30s). Buckets on healthy or never-probed backends always
/// pass (fail-open: only a definitive verdict gates).
///
/// Called only for requests whose credentials were VERIFIED (the s3s access
/// hook, the form-POST handler after its policy check): the 503 body names
/// internal backend topology and credential state.
/// See `maintenance::gate::check_verified_request`.
pub fn health_gate_refusal(gate: &BackendHealthGate, path: &str) -> Option<S3Error> {
    // Fast path: nothing unhealthy → pass without touching the config lock.
    if gate.health.unhealthy_names().is_empty() {
        return None;
    }
    let bucket = crate::maintenance::gate::bucket_from_path(path)?;
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
    let (backend_name, backend_cfg) = {
        let cfg = gate.config.try_read().ok()?;
        let routed = gate
            .app
            .engine
            .load()
            .storage()
            .resolved_backend_name(&bucket);
        // Route to an undefined backend: unreachable in practice (check_fatal
        // blocks it at boot + apply) — fail-open rather than double-enforce.
        gated_backend(&cfg, &bucket, routed)?
    };
    let verdict = gate.health.get(&backend_name, &backend_cfg)?;
    verdict.is_gating().then(|| {
        S3Error::ServiceUnavailable(format!(
            "bucket '{bucket}' is on backend '{backend_name}', which is currently \
             unavailable: {}. Requests are blocked until the backend recovers \
             (re-checked every {}s); see Storage → Backends for live status",
            verdict.cause(),
            health_probe_interval().map_or(30, |d| d.as_secs())
        ))
    })
}

/// The backend whose health gates `bucket`: where the router sends it
/// (`routed`, from `resolved_backend_name`), else the config resolver.
/// Judging an unrouted bucket by the config alone gated it by the DEFAULT
/// backend even when the router serves it from another one.
pub fn gated_backend(
    cfg: &crate::config::Config,
    bucket: &str,
    routed: Option<String>,
) -> Option<(String, BackendConfig)> {
    match routed {
        Some(name) => cfg.backend_by_name(&name).map(|def| (name, def.clone())),
        None => cfg
            .effective_backend_for_bucket(bucket)
            .map(|(name, def)| (name, def.clone())),
    }
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

    /// Prod ('HetznerHelsinki1'): one HEAD among thousands timed out, the
    /// passive mark turned the backend Unreachable, and every bucket on it
    /// answered 503 until the next probe. A failed request alone must never
    /// gate, whatever definition it ran against.
    #[test]
    fn a_failed_request_alone_never_gates() {
        let cache = BackendHealthCache::default();
        let cfg = BackendConfig::Filesystem {
            path: "/tmp/dgp-passive".into(),
        };
        let fp = fingerprint(&cfg);
        cache.set("hetzner-fsn1", &cfg, HealthVerdict::Healthy);
        let _ = cache.mark_unavailable("hetzner-fsn1", "other-fp", "timed out");
        let _ = cache.mark_unavailable(
            "hetzner-fsn1",
            &fp,
            "head_object on bucket 'releases': request has timed out",
        );
        let v = cache.get("hetzner-fsn1", &cfg).unwrap();
        assert!(
            !v.is_gating(),
            "one failed request gated the backend: {v:?}"
        );
        assert_eq!(v, HealthVerdict::Healthy);
        assert!(cache.unhealthy_names().is_empty());
    }

    /// One named filesystem backend at `path`: its config handle and its
    /// definition. A filesystem probe fails when `path` is a regular file.
    fn fs_backend(
        name: &str,
        path: &std::path::Path,
    ) -> (crate::config::SharedConfig, BackendConfig) {
        let cfg = crate::config::Config::from_yaml_str(&format!(
            "storage:\n  backends:\n    - name: {name}\n      type: filesystem\n      path: {}\n",
            path.display()
        ))
        .expect("fixture parses");
        let backend = cfg.backend_by_name(name).expect("named backend").clone();
        (cfg.into_shared(), backend)
    }

    const TIMED_OUT: &str = "head_object on bucket 'releases': request has timed out";

    #[tokio::test]
    async fn a_failed_request_on_a_backend_that_answers_its_probe_stays_open() {
        let dir = tempfile::tempdir().unwrap();
        let (config, cfg) = fs_backend("hetzner-fsn1", dir.path());
        let fp = fingerprint(&cfg);
        let cache = Arc::new(BackendHealthCache::default());
        cache.set("hetzner-fsn1", &cfg, HealthVerdict::Healthy);

        let probe = report_unavailable(&cache, &config, "hetzner-fsn1", &fp, TIMED_OUT)
            .expect("the first failed request starts a confirmation probe");
        // The test runtime is single-threaded: the probe has not run yet.
        assert!(
            report_unavailable(&cache, &config, "hetzner-fsn1", &fp, TIMED_OUT).is_none(),
            "a second failure joins the running probe"
        );
        assert_eq!(
            cache.get("hetzner-fsn1", &cfg),
            Some(HealthVerdict::Healthy),
            "no gate before the probe answers"
        );
        probe.await.unwrap();
        assert_eq!(
            cache.get("hetzner-fsn1", &cfg),
            Some(HealthVerdict::Healthy),
            "the probe answered, so the buckets stay open"
        );
        assert!(cache.unhealthy_names().is_empty());
        assert!(
            report_unavailable(&cache, &config, "hetzner-fsn1", &fp, TIMED_OUT).is_none(),
            "no new probe within the cooldown"
        );
    }

    #[tokio::test]
    async fn a_failed_request_on_a_dead_backend_gates_after_its_probe() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::write(&root, b"not a directory").unwrap();
        let (config, cfg) = fs_backend("hetzner-fsn1", &root);
        let fp = fingerprint(&cfg);
        let cache = Arc::new(BackendHealthCache::default());
        cache.set("hetzner-fsn1", &cfg, HealthVerdict::Healthy);

        let v0 = current_backend_health_version();
        report_unavailable(&cache, &config, "hetzner-fsn1", &fp, TIMED_OUT)
            .expect("confirmation probe")
            .await
            .unwrap();
        let v = cache.get("hetzner-fsn1", &cfg).unwrap();
        assert!(v.is_gating(), "the failed probe gates the backend: {v:?}");
        assert!(current_backend_health_version() > v0, "a real change bumps");
        assert!(
            report_unavailable(&cache, &config, "hetzner-fsn1", &fp, TIMED_OUT).is_none(),
            "a gated backend starts no probe: the re-probe loop reopens it"
        );
    }

    /// The fingerprint guard: a failed request that ran against a replaced
    /// definition neither probes nor paints the current one.
    #[tokio::test]
    async fn a_failed_request_of_a_replaced_definition_paints_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::write(&root, b"not a directory").unwrap();
        // The running config holds a DEAD definition; the request ran
        // against the old, healthy one.
        let (config, current) = fs_backend("hetzner-fsn1", &root);
        let old = BackendConfig::Filesystem {
            path: dir.path().to_path_buf(),
        };
        let cache = Arc::new(BackendHealthCache::default());
        cache.set("hetzner-fsn1", &current, HealthVerdict::Healthy);
        assert!(
            report_unavailable(
                &cache,
                &config,
                "hetzner-fsn1",
                &fingerprint(&old),
                TIMED_OUT
            )
            .is_none(),
            "another definition's failure starts no probe"
        );

        // The cache still holds the old definition (the apply has not
        // recorded the new one yet): the probe finds the definition
        // replaced, and records nothing.
        let cache = Arc::new(BackendHealthCache::default());
        cache.set("hetzner-fsn1", &old, HealthVerdict::Healthy);
        report_unavailable(
            &cache,
            &config,
            "hetzner-fsn1",
            &fingerprint(&old),
            TIMED_OUT,
        )
        .expect("same definition as the cache: probe")
        .await
        .unwrap();
        assert_eq!(
            cache.get("hetzner-fsn1", &old),
            Some(HealthVerdict::Healthy)
        );
        assert_eq!(cache.get("hetzner-fsn1", &current), None);
        assert!(cache.unhealthy_names().is_empty());
    }

    #[test]
    fn health_cache_fingerprint_and_version_semantics() {
        let cache = BackendHealthCache::default();
        let cfg = BackendConfig::S3 {
            session_token: None,
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
        // Steady-state re-probe with the same verdict does NOT bump. (Other
        // tests bump the global counter in parallel, so the return value
        // proves it, not an equality on the counter.)
        assert!(!cache.set("b2", &cfg, HealthVerdict::Healthy));
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
            session_token: None,
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
    fn probe_targets_skips_unused_singleton_with_named_backends() {
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
        // Named backends exist → the unused legacy singleton is NOT probed,
        // and the unrouted `plain` bucket belongs to the named default (b2).
        assert_eq!(targets.len(), 1, "{targets:?}");
        assert_eq!(targets[0].0, "b2");
        assert_eq!(
            targets[0].2.as_deref(),
            Some("real-mirror"),
            "alias-resolved real bucket (BTreeMap order: mirror first)"
        );
    }

    #[test]
    fn probe_targets_singleton_is_default() {
        let cfg = crate::config::Config::from_yaml_str(
            r#"
storage:
  buckets:
    plain: {}
"#,
        )
        .expect("fixture parses");
        let targets = probe_targets(&cfg);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].0, "default");
        assert_eq!(targets[0].2.as_deref(), Some("plain"));
    }

    #[test]
    fn gated_backend_prefers_the_routers_answer() {
        let cfg = crate::config::Config::from_yaml_str(
            r#"
storage:
  backends:
    - name: primary
      type: filesystem
      path: /tmp/dgp-gate-a
    - name: local-disk
      type: filesystem
      path: /tmp/dgp-gate-b
"#,
        )
        .unwrap();
        // Unrouted, router unaware → the config default.
        assert_eq!(gated_backend(&cfg, "downloads", None).unwrap().0, "primary");
        // The router found it on local-disk → that backend gates it.
        assert_eq!(
            gated_backend(&cfg, "downloads", Some("local-disk".into()))
                .unwrap()
                .0,
            "local-disk"
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

#[cfg(test)]
mod slow_backend_tests {
    //! A backend that answers slowly is reachable: it never gates. And a
    //! probe round never overwrites a newer verdict, or the verdict of a
    //! definition that replaced the one it probed.

    use super::*;

    const TIMED_OUT: &str = "head_object on bucket 'releases': request has timed out";

    /// `hetzner-fsn1` on the fake S3 at `endpoint`, with `secret`; the
    /// bucket `releases` routes to it when `routed`.
    fn fake_backend(
        endpoint: &str,
        secret: &str,
        routed: bool,
    ) -> (crate::config::SharedConfig, BackendConfig) {
        let buckets = if routed {
            "  buckets:\n    releases: { backend: hetzner-fsn1 }\n"
        } else {
            ""
        };
        let cfg = crate::config::Config::from_yaml_str(&format!(
            "storage:\n  backends:\n    - name: hetzner-fsn1\n      type: s3\n      \
             endpoint: \"{endpoint}\"\n      region: eu-central\n      force_path_style: true\n      \
             access_key_id: k\n      secret_access_key: {secret}\n      allow_local: true\n{buckets}"
        ))
        .expect("fixture parses");
        let backend = cfg.backend_by_name("hetzner-fsn1").unwrap().clone();
        (cfg.into_shared(), backend)
    }

    /// Prod (Hetzner hel1): ListBuckets and HeadBucket answer in more than
    /// 5 s now and then, and the probe gave up at 5 s (twice), so a backend
    /// whose data requests still worked answered 503 for every bucket.
    #[tokio::test]
    async fn a_slow_backend_that_answers_is_never_gated() {
        let (endpoint, fake) = crate::storage::fake_s3::start().await;
        fake.set_delay_ms("LIST_BUCKETS", 6_000);
        fake.set_delay_ms("HEAD_BUCKET", 6_000);
        let (config, cfg) = fake_backend(&endpoint, "s", true);
        let fp = fingerprint(&cfg);
        let cache = Arc::new(BackendHealthCache::default());
        cache.set("hetzner-fsn1", &cfg, HealthVerdict::Healthy);

        report_unavailable(&cache, &config, "hetzner-fsn1", &fp, TIMED_OUT)
            .expect("a confirmation probe")
            .await
            .unwrap();
        let v = cache.get("hetzner-fsn1", &cfg).unwrap();
        assert!(!v.is_gating(), "the confirmation probe gated: {v:?}");

        reprobe_all(&cache, &config).await;
        let v = cache.get("hetzner-fsn1", &cfg).unwrap();
        assert!(!v.is_gating(), "the probe round gated: {v:?}");
    }

    /// Interleaving A of the blast scan: a round probes definition D1, an
    /// apply rotates the secret to D2 and records D2 healthy, then the
    /// round's D1 verdict lands on top. The GUI and `/_/ready` showed the
    /// stale red verdict, and passive signals for D2 were absorbed.
    #[tokio::test]
    async fn a_round_never_overwrites_a_newer_definitions_verdict() {
        let (endpoint, fake) = crate::storage::fake_s3::start().await;
        fake.set_delay_ms("LIST_BUCKETS", 1_000);
        fake.fail("LIST_BUCKETS", "/", 403, "InvalidAccessKeyId", u32::MAX);
        let (config, _d1) = fake_backend(&endpoint, "s", false);
        let cache = Arc::new(BackendHealthCache::default());
        let round = tokio::spawn({
            let (cache, config) = (cache.clone(), config.clone());
            async move { reprobe_all(&cache, &config).await }
        });
        assert!(
            fake.wait_for(|r| r == "GET /" || r.starts_with("GET /?"))
                .await,
            "the round probes"
        );

        // The apply: a rotated secret, probed healthy, then the swap.
        let (_, d2) = fake_backend(&endpoint, "s2", false);
        cache.set("hetzner-fsn1", &d2, HealthVerdict::Healthy);
        config.write().await.backends[0].backend = d2.clone();
        round.await.unwrap();

        assert_eq!(
            cache.snapshot()["hetzner-fsn1"].verdict,
            HealthVerdict::Healthy,
            "the old definition's verdict replaced the new one's"
        );
        assert!(
            cache.mark_unavailable("hetzner-fsn1", &fingerprint(&d2), TIMED_OUT),
            "a failed request of the new definition was absorbed"
        );
    }

    /// Interleaving C: a verdict recorded while a round's probe ran (a
    /// confirmation probe, or Probe now) is newer than the round's; the
    /// round must not replace it when its slow probe ends.
    #[tokio::test]
    async fn a_round_never_overwrites_a_verdict_newer_than_its_probe() {
        let (endpoint, fake) = crate::storage::fake_s3::start().await;
        fake.set_delay_ms("LIST_BUCKETS", 1_000);
        fake.fail("LIST_BUCKETS", "/", 403, "InvalidAccessKeyId", u32::MAX);
        let (config, d1) = fake_backend(&endpoint, "s", false);
        let cache = Arc::new(BackendHealthCache::default());
        let round = tokio::spawn({
            let (cache, config) = (cache.clone(), config.clone());
            async move { reprobe_all(&cache, &config).await }
        });
        assert!(
            fake.wait_for(|r| r == "GET /" || r.starts_with("GET /?"))
                .await,
            "the round probes"
        );
        cache.set("hetzner-fsn1", &d1, HealthVerdict::Healthy);
        round.await.unwrap();
        assert_eq!(
            cache.get("hetzner-fsn1", &d1),
            Some(HealthVerdict::Healthy),
            "an older probe replaced a newer verdict"
        );
    }
}

#[cfg(test)]
mod review2_tests {
    use super::*;

    /// Puts `DGP_BACKEND_ALLOW_LOCAL` back on drop (also on a failed assert).
    struct RestoreEnv(Option<std::ffi::OsString>);
    impl Drop for RestoreEnv {
        fn drop(&mut self) {
            if let Some(v) = self.0.take() {
                // SAFETY: still under SSRF_ENV_LOCK (dropped after this guard).
                unsafe { std::env::set_var("DGP_BACKEND_ALLOW_LOCAL", v) };
            }
        }
    }

    /// Review-2 (S18 incomplete): the pre-commit health probe (and every
    /// client built by `ConfigDbSync::build_client`: config sync, S3 leases,
    /// the reference lock, the capability probe) never runs the outbound-URL
    /// check or the SSRF resolver. The engine refuses the endpoint; the
    /// probe still sends a signed request to it.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn review2_probe_never_contacts_an_endpoint_the_backend_validator_refuses() {
        // The env override lets every builder accept a local endpoint, so the
        // premise (the engine refuses it) needs it unset, whatever the runner
        // exports. The lock serialises this with the other SSRF env tests.
        let _g = crate::storage::SSRF_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var_os("DGP_BACKEND_ALLOW_LOCAL");
        // SAFETY: the SSRF env tests are serialised on SSRF_ENV_LOCK.
        unsafe { std::env::remove_var("DGP_BACKEND_ALLOW_LOCAL") };
        let _restore = RestoreEnv(prev);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let accepted = tokio::spawn(async move {
            tokio::time::timeout(std::time::Duration::from_secs(5), listener.accept())
                .await
                .is_ok()
        });
        let backend = BackendConfig::S3 {
            endpoint: Some(format!("http://127.0.0.1:{port}")),
            region: "us-east-1".into(),
            force_path_style: true,
            access_key_id: Some("k".into()),
            secret_access_key: Some("s".into()),
            allow_local: false,
            session_token: None,
        };
        assert!(
            crate::storage::S3Backend::build_client(&backend)
                .await
                .is_err(),
            "precondition: the engine's builder refuses this endpoint"
        );
        let _ = probe_backend_health(&backend, None).await;
        assert!(
            !accepted.await.unwrap(),
            "the health probe reached an SSRF-refused endpoint"
        );
    }
}
