// SPDX-License-Identifier: BUSL-1.1

//! Encryption needs atomic writes (N2 of
//! `docs/plan/encryption-needs-atomic-writes.md`).
//!
//! The proxy converts and re-keys encrypted buckets by rewriting objects
//! in place, each write conditional on the object it read (N1), and it
//! keeps the key claim and the conversion marker with conditional writes.
//! On a backend that does not enforce conditional writes, a rewrite can
//! overwrite a newer client write. So:
//!
//! - **Refusal.** Every action that turns proxy encryption on (or changes
//!   its key, or moves an encrypted bucket) or keeps a conversion going
//!   runs a FRESH probe of the backend first ([`require_atomic_writes`],
//!   [`gate_encryption_change`]); a backend whose probe says non-CAS, or
//!   that could not be verified, is refused with an [`AtomicityRefusal`].
//! - **Read-only.** A bucket that already encrypts with the proxy on such a
//!   backend serves reads only ([`BackendCapabilityCache::read_only_reason`],
//!   asked by the client write gate and the background writers). The
//!   [`spawn_watch`] task probes the backends of encrypted buckets at boot
//!   (in the background) and again later; a backend whose verdict becomes
//!   CAS lifts the read-only state without a restart.
//!
//! Native SSE modes are out of scope: the backend encrypts those objects
//! itself and the proxy never rewrites them for encryption.

use super::capability::{
    establish_backend_verdict_within, forced_noncas_backends, BackendCapabilityCache,
    CapabilityVerdict, ClientWritableGroup, VerifiedVia, CAPABILITY_DOC_URL, PROBE_TIMEOUT,
};
use crate::config::{BackendConfig, Config};
use std::collections::BTreeMap;

/// The machine code of a refusal for lack of atomic writes.
pub const ATOMIC_WRITES_REQUIRED: &str = "atomic_writes_required";

/// Pure: the wire tag of a verdict (`cas-verified`, `non-cas`, `unknown`).
pub fn verdict_tag(verdict: &CapabilityVerdict) -> &'static str {
    match verdict {
        CapabilityVerdict::CasVerified { .. } => "cas-verified",
        CapabilityVerdict::NonCas => "non-cas",
        CapabilityVerdict::Unknown { .. } => "unknown",
    }
}

/// Pure: the probe result in plain words, for messages and the UI. A
/// non-CAS verdict says that the backend does NOT support atomic writes;
/// an unknown one only that the proxy COULD NOT VERIFY them (with the
/// reason: for the admin API and the logs only, never an S3 client).
pub fn verdict_words(verdict: &CapabilityVerdict) -> String {
    match verdict {
        CapabilityVerdict::CasVerified { .. } => {
            "the backend supports atomic writes (it enforced a conditional write)".to_string()
        }
        CapabilityVerdict::NonCas => "the backend does not support atomic writes: it ignored or \
                                      rejected a conditional write, as Backblaze B2 does"
            .to_string(),
        CapabilityVerdict::Unknown { reason } => {
            format!("the proxy could not verify the backend's atomic writes ({reason})")
        }
    }
}

/// Why an encryption action was refused: the backend has no proven atomic
/// writes. HTTP 409 on every admin surface, with [`Self::details`] as
/// the structured part of the body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtomicityRefusal {
    /// The backend that was probed.
    pub backend: String,
    /// What the fresh probe said (never `CasVerified`).
    pub verdict: CapabilityVerdict,
    /// What was refused, for the message ("encryption of bucket 'x'").
    pub what: String,
}

impl AtomicityRefusal {
    /// The operator-facing message: what, the backend, the probe result in
    /// plain words, the way out, and the doc link.
    pub fn message(&self) -> String {
        match &self.verdict {
            CapabilityVerdict::Unknown { .. } => format!(
                "{} refused: encryption needs atomic writes (conditional PUT and DELETE), and \
                 the proxy could not verify them on backend '{}': {}. Retry when the backend \
                 is reachable (Check again on the backend card) — see {CAPABILITY_DOC_URL}",
                self.what,
                self.backend,
                verdict_words(&self.verdict)
            ),
            _ => format!(
                "{} refused: encryption needs atomic writes (conditional PUT and DELETE), and \
                 backend '{}' does not support them: it ignored or rejected a conditional \
                 write, as Backblaze B2 does. Keep encrypted buckets on a backend with atomic \
                 writes (move a bucket there with a migrate job) — see {CAPABILITY_DOC_URL}",
                self.what, self.backend,
            ),
        }
    }

    /// The structured part of a refusal body.
    pub fn details(&self) -> serde_json::Value {
        serde_json::json!({
            "code": ATOMIC_WRITES_REQUIRED,
            "backend": self.backend,
            "probe": verdict_tag(&self.verdict),
            "probe_result": verdict_words(&self.verdict),
            "doc_url": CAPABILITY_DOC_URL,
        })
    }
}

impl std::fmt::Display for AtomicityRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

/// Proof that a fresh probe found atomic writes on a backend. Only
/// [`require_atomic_writes`] makes one: a job creator that holds it does
/// not probe again for the same action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtomicWrites {
    backend: String,
}

impl AtomicWrites {
    /// The backend the proof is for.
    pub fn backend(&self) -> &str {
        &self.backend
    }
}

/// The buckets to probe `backend` on, best first: `hint` (the real name of
/// the bucket the action is about), then the real names of the configured
/// buckets routed there.
fn probe_candidates(config: &Config, backend: &str, hint: Option<&str>) -> Vec<String> {
    let mut out: Vec<String> = hint.map(str::to_string).into_iter().collect();
    for name in config.buckets.keys() {
        if name.starts_with(crate::maintenance::migrate::TRANSIENT_PREFIX) {
            continue;
        }
        if config
            .effective_backend_for_bucket(name)
            .is_some_and(|(b, _)| b == backend)
        {
            let real = config.real_bucket_name(name);
            if !out.contains(&real) {
                out.push(real);
            }
        }
    }
    out
}

/// The bucket that a probe of the S3 backend `def` writes its probe key
/// in: the first candidate that the backend lists, else any bucket it
/// lists. Without a listing (a key restricted to some buckets), the first
/// candidate. `Err`: nothing to probe on.
async fn pick_probe_bucket(
    def: &BackendConfig,
    candidates: &[String],
    budget: std::time::Duration,
) -> Result<String, String> {
    let client = crate::config_db_sync::ConfigDbSync::build_client_labelled(
        def,
        crate::storage::S3Backend::request_timeout(),
        crate::storage::CAPABILITY_PROBE,
    )
    .await
    .map_err(|e| format!("client build failed: {e}"))?;
    let listed = tokio::time::timeout(budget, client.list_buckets().send()).await;
    match listed {
        Ok(Ok(out)) => {
            let names: Vec<String> = out
                .buckets()
                .iter()
                .filter_map(|b| b.name().map(str::to_string))
                .collect();
            candidates
                .iter()
                .find(|c| names.contains(c))
                .or_else(|| names.first())
                .cloned()
                .ok_or_else(|| {
                    "the backend holds no bucket to run the probe in; create a bucket first"
                        .to_string()
                })
        }
        _ => candidates
            .first()
            .cloned()
            .ok_or_else(|| "the backend's buckets could not be listed".to_string()),
    }
}

/// How long the bucket listing of a probe may take (part of the probe's
/// one deadline, [`PROBE_TIMEOUT`]).
const PROBE_LISTING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// THE atomicity probe of `backend` under `config`, FRESH (never a cached
/// verdict): the test seam `DGP_TEST_FORCE_NONCAS_BACKEND` → non-CAS; a
/// filesystem backend → CAS (one node, in-process locks, no request); an S3
/// backend → the conditional-write probe (`cas_probe`: one small key,
/// removed with all its versions) on a bucket it holds (`hint`, the real
/// name of the bucket in question, first). The verdict is stored in
/// `cache` (it drives the read-only rule and the admin views).
pub async fn probe_atomic_writes(
    config: &Config,
    cache: &BackendCapabilityCache,
    backend: &str,
    hint: Option<&str>,
) -> CapabilityVerdict {
    let Some(def) = config.backend_by_name(backend) else {
        return CapabilityVerdict::Unknown {
            reason: format!("no backend named '{backend}'"),
        };
    };
    let delay = crate::config::test_seams::test_seams().atomicity_probe_delay_ms;
    if delay > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
    }
    let forced = forced_noncas_backends();
    let verdict = if forced.contains(backend) {
        CapabilityVerdict::NonCas
    } else if matches!(def, BackendConfig::Filesystem { .. }) {
        CapabilityVerdict::CasVerified {
            via: VerifiedVia::LocalFilesystem,
        }
    } else {
        // ONE deadline for the whole probe (the bucket listing and the
        // conditional writes): the pending window of the backend's
        // encrypted buckets is at most PROBE_TIMEOUT.
        let deadline = tokio::time::Instant::now() + PROBE_TIMEOUT;
        let candidates = probe_candidates(config, backend, hint);
        match pick_probe_bucket(def, &candidates, PROBE_LISTING_TIMEOUT).await {
            Err(reason) => CapabilityVerdict::Unknown { reason },
            Ok(probe_bucket) => {
                let group = ClientWritableGroup {
                    backend: def.clone(),
                    buckets: Vec::new(),
                    probe_bucket,
                };
                let left = deadline.saturating_duration_since(tokio::time::Instant::now());
                establish_backend_verdict_within(backend, &group, &forced, left).await
            }
        }
    };
    cache.set(backend, def, verdict.clone());
    verdict
}

/// A fresh probe of `backend` ([`probe_atomic_writes`]); `Err` unless it
/// proves atomic writes. `what` names the refused action.
pub async fn require_atomic_writes(
    config: &Config,
    cache: &BackendCapabilityCache,
    backend: &str,
    hint: Option<&str>,
    what: &str,
) -> Result<AtomicWrites, AtomicityRefusal> {
    match probe_atomic_writes(config, cache, backend, hint).await {
        CapabilityVerdict::CasVerified { .. } => Ok(AtomicWrites {
            backend: backend.to_string(),
        }),
        verdict => Err(AtomicityRefusal {
            backend: backend.to_string(),
            verdict,
            what: what.to_string(),
        }),
    }
}

/// The backend that the writes of `bucket` go to under `config`: the
/// config's answer for a bucket it names, else the router's (a bucket that
/// the config does not name can live on any backend; the router finds it,
/// see [`crate::storage::StorageBackend::bucket_home`]). `Err`: the reason
/// it could not be determined.
pub async fn bucket_backend(
    app: &crate::api::handlers::AppState,
    config: &Config,
    bucket: &str,
) -> Result<String, String> {
    use crate::storage::{BucketHome, StorageBackend};
    let key = bucket.to_ascii_lowercase();
    if config.buckets.contains_key(&key) {
        return config
            .effective_backend_for_bucket(&key)
            .map(|(name, _)| name)
            .ok_or_else(|| format!("bucket '{bucket}' routes to an unknown backend"));
    }
    match app.engine.load().storage().bucket_home(&key).await {
        BucketHome::Backend(name) if config.backend_by_name(&name).is_some() => Ok(name),
        BucketHome::Backend(name) => Err(format!(
            "bucket '{bucket}' is on backend '{name}', which the config does not define"
        )),
        BucketHome::Default => Ok(config.default_backend_name()),
        BucketHome::Undetermined => Err(format!(
            "the backend of bucket '{bucket}' could not be determined (a backend did not answer)"
        )),
    }
}

/// The backend of `bucket` ([`bucket_backend`]), or the refusal of `what`
/// when it cannot be determined.
async fn backend_or_refusal(
    app: &crate::api::handlers::AppState,
    config: &Config,
    bucket: &str,
    what: &str,
) -> Result<String, AtomicityRefusal> {
    bucket_backend(app, config, bucket)
        .await
        .map_err(|reason| AtomicityRefusal {
            backend: "(unknown)".to_string(),
            verdict: CapabilityVerdict::Unknown { reason },
            what: what.to_string(),
        })
}

/// [`require_atomic_writes`] (a FRESH probe) for the backend that the
/// writes of `bucket` really go to ([`bucket_backend`]), with the
/// bucket's real name as the probe hint.
pub async fn require_for_bucket(
    app: &crate::api::handlers::AppState,
    config: &Config,
    bucket: &str,
    what: &str,
) -> Result<AtomicWrites, AtomicityRefusal> {
    let backend = backend_or_refusal(app, config, bucket, what).await?;
    let real = config.real_bucket_name(bucket);
    require_atomic_writes(
        config,
        &app.backend_capabilities,
        &backend,
        Some(&real),
        what,
    )
    .await
}

/// Atomic writes on the backend of `bucket`, proven by the cached CAS
/// verdict of the backend's current definition, else by a fresh probe
/// ([`require_for_bucket`]). For a job's run start: the action that queued
/// it already probed fresh.
pub async fn proven_for_bucket(
    app: &crate::api::handlers::AppState,
    config: &Config,
    bucket: &str,
    what: &str,
) -> Result<AtomicWrites, AtomicityRefusal> {
    let backend = backend_or_refusal(app, config, bucket, what).await?;
    let forced = forced_noncas_backends().contains(&backend);
    let cached_cas = config
        .backend_by_name(&backend)
        .and_then(|def| app.backend_capabilities.get(&backend, def))
        .is_some_and(|v| matches!(v, CapabilityVerdict::CasVerified { .. }));
    if cached_cas && !forced {
        return Ok(AtomicWrites { backend });
    }
    let real = config.real_bucket_name(bucket);
    require_atomic_writes(
        config,
        &app.backend_capabilities,
        &backend,
        Some(&real),
        what,
    )
    .await
}

/// The apply gate (config apply, validate, section and field writes, CLI
/// apply, backend and bucket create): every backend where the change from
/// `old` to `new` makes the proxy encrypt with a key
/// ([`Config::atomic_write_targets`]) gets a fresh probe, once per
/// backend. The first one without atomic writes refuses the change.
pub async fn gate_encryption_change(
    old: &Config,
    new: &Config,
    cache: &BackendCapabilityCache,
) -> Result<(), AtomicityRefusal> {
    let mut by_backend: BTreeMap<String, Vec<Option<String>>> = BTreeMap::new();
    for t in new.atomic_write_targets(old) {
        by_backend.entry(t.backend).or_default().push(t.bucket);
    }
    for (backend, subjects) in by_backend {
        let bucket = subjects.iter().flatten().next().cloned();
        let what = match &bucket {
            Some(b) => format!("encryption of bucket '{b}'"),
            None => format!("encryption on backend '{backend}'"),
        };
        let hint = bucket.as_ref().map(|b| new.real_bucket_name(b));
        require_atomic_writes(new, cache, &backend, hint.as_deref(), &what).await?;
    }
    Ok(())
}

/// Pure: why proxy encryption is not offered on `backend` under
/// `verdict`, or `None` when the verdict proves atomic writes.
pub fn proxy_unavailable_reason(backend: &str, verdict: &CapabilityVerdict) -> Option<String> {
    match verdict {
        CapabilityVerdict::CasVerified { .. } => None,
        CapabilityVerdict::NonCas => Some(format!(
            "Encryption with a proxy key needs atomic writes, and backend '{backend}' does not \
             support them (it ignored or rejected a conditional write) — see {CAPABILITY_DOC_URL}"
        )),
        CapabilityVerdict::Unknown { .. } => Some(format!(
            "Encryption with a proxy key needs atomic writes, and the proxy could not verify \
             them on backend '{backend}' (the check did not reach it). Check again when the \
             backend is reachable — see {CAPABILITY_DOC_URL}"
        )),
    }
}

/// The atomic-writes verdict of backend `name` for the admin views: the
/// cached verdict of its current definition, else the verdict that needs
/// no request (a filesystem backend). `None`: not checked yet.
pub fn backend_verdict(
    config: &Config,
    cache: &BackendCapabilityCache,
    name: &str,
) -> Option<CapabilityVerdict> {
    let def = config.backend_by_name(name)?;
    cache.get(name, def).or_else(|| static_verdict(name, def))
}

/// Why a write to a bucket that encrypts with the proxy must not pass now
/// ([`BackendCapabilityCache::write_block`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteBlock {
    /// The backend has no CAS or non-CAS verdict for its current
    /// definition: the check has not run yet (boot, a config change) or
    /// could not reach the backend (`unverified`: the reason, for the
    /// admin API and the logs only). Fail closed: S3 `503 SlowDown`, which
    /// clients retry; the watch probes again within seconds.
    Pending {
        backend: String,
        unverified: Option<String>,
    },
    /// The backend does not support atomic writes (non-CAS verdict): the
    /// bucket is read-only (S3 `403 AccessDenied`, not retried).
    ReadOnly { backend: String },
}

impl WriteBlock {
    /// Pure: the block of a bucket on `backend` under `verdict` (`None`:
    /// no verdict yet).
    pub fn of(backend: &str, verdict: Option<CapabilityVerdict>) -> Option<Self> {
        let backend = backend.to_string();
        match verdict {
            None => Some(Self::Pending {
                backend,
                unverified: None,
            }),
            Some(CapabilityVerdict::Unknown { reason }) => Some(Self::Pending {
                backend,
                unverified: Some(reason),
            }),
            Some(CapabilityVerdict::CasVerified { .. }) => None,
            Some(CapabilityVerdict::NonCas) => Some(Self::ReadOnly { backend }),
        }
    }

    /// Is this the read-only case?
    pub fn is_read_only(&self) -> bool {
        matches!(self, Self::ReadOnly { .. })
    }

    /// The message of a write to `bucket` for an S3 client: fixed phrases,
    /// never the probe's error (it can name the backend's address).
    pub fn message(&self, bucket: &str) -> String {
        match self {
            Self::Pending { .. } => format!(
                "Bucket '{bucket}' is encrypted, and the proxy has not yet verified that its \
                 backend supports atomic writes, which encryption needs. Writes wait until the \
                 check succeeds; retry in a few seconds. Reads keep working — see \
                 {CAPABILITY_DOC_URL}"
            ),
            Self::ReadOnly { .. } => format!(
                "Bucket '{bucket}' is read-only: it is encrypted, and its backend does not \
                 support atomic writes, which encryption needs. Reads keep working. To write \
                 to it again, move the bucket to a backend with atomic writes with a migrate \
                 job — see {CAPABILITY_DOC_URL}"
            ),
        }
    }

    /// The message for the admin API and the logs: names the backend and,
    /// for an unverified backend, why the check failed.
    pub fn admin_message(&self, bucket: &str) -> String {
        match self {
            Self::Pending {
                backend,
                unverified: None,
            } => format!(
                "Bucket '{bucket}' is encrypted, and the check that backend '{backend}' supports \
                 atomic writes (which encryption needs) is still running. Writes wait until it \
                 ends; retry in a few seconds. Reads keep working — see {CAPABILITY_DOC_URL}"
            ),
            Self::Pending {
                backend,
                unverified: Some(reason),
            } => format!(
                "Bucket '{bucket}' is encrypted, and the proxy could not verify that backend \
                 '{backend}' supports atomic writes, which encryption needs ({reason}). Writes \
                 wait (503, retried) and the proxy checks again within a minute. Reads keep \
                 working — see {CAPABILITY_DOC_URL}"
            ),
            Self::ReadOnly { backend } => format!(
                "Bucket '{bucket}' is read-only: it is encrypted, and encryption needs atomic \
                 writes, which backend '{backend}' does not support (it ignored or rejected a \
                 conditional write). Reads keep working. To write to it again, move the bucket \
                 to a backend with atomic writes with a migrate job — see {CAPABILITY_DOC_URL}"
            ),
        }
    }

    /// The message of a CreateBucket of `bucket` that would encrypt (S3
    /// client: fixed phrases).
    pub fn create_message(&self, bucket: &str) -> String {
        match self {
            Self::Pending { .. } => self.message(bucket),
            Self::ReadOnly { .. } => format!(
                "Bucket '{bucket}' cannot be created: it would be encrypted by the proxy, and \
                 its backend does not support atomic writes (conditional PUT and DELETE), which \
                 encryption needs. The proxy does not create a bucket that would be read-only. \
                 Create it on a backend with atomic writes — see {CAPABILITY_DOC_URL}"
            ),
        }
    }

    /// The S3 error of a write to `bucket` (`create`: a CreateBucket).
    pub fn to_s3_error(&self, bucket: &str, create: bool) -> crate::api::errors::S3Error {
        let message = if create {
            self.create_message(bucket)
        } else {
            self.message(bucket)
        };
        match self {
            Self::Pending { .. } => crate::api::errors::S3Error::SlowDown(message),
            Self::ReadOnly { .. } => crate::api::errors::S3Error::AccessDeniedReason(message),
        }
    }
}

impl crate::api::handlers::AppState {
    /// THE write rule of the request path:
    /// [`BackendCapabilityCache::write_block`] of `bucket` on the backend
    /// that its writes really go to. The router resolves a bucket that the
    /// config does not name (a cached answer, or one HeadBucket round shared
    /// with the write that follows), so a cold resolution after a boot, a
    /// config apply or a CreateBucket never judges the default backend in
    /// place of the real one.
    pub async fn atomic_write_block(&self, bucket: &str) -> Option<WriteBlock> {
        let cache = &self.backend_capabilities;
        if !cache.encrypted_plan().needs_home(bucket) {
            return cache.write_block(bucket, &crate::storage::BucketHome::Default);
        }
        use crate::storage::StorageBackend;
        let engine = self.engine.load();
        let home = engine.storage().bucket_home(bucket).await;
        cache.write_block(bucket, &home)
    }

    /// [`Self::atomic_write_block`] from memory only, for the background
    /// writers' synchronous gates: a bucket whose home the router does not
    /// know without a request is undetermined (fail closed: pending unless
    /// every encrypting backend is CAS), and the router resolves it in the
    /// background, so the next check knows.
    pub fn atomic_write_block_now(&self, bucket: &str) -> Option<WriteBlock> {
        let cache = &self.backend_capabilities;
        if !cache.encrypted_plan().needs_home(bucket) {
            return cache.write_block(bucket, &crate::storage::BucketHome::Default);
        }
        use crate::storage::StorageBackend;
        let home = self.engine.load().storage().bucket_home_now(bucket);
        cache.write_block(bucket, &home)
    }
}

/// The verdict of a backend that needs no request: a filesystem backend
/// has atomic writes (one node, in-process locks); the test seam
/// `DGP_TEST_FORCE_NONCAS_BACKEND` makes it non-CAS. `None` for S3, and
/// under `DGP_TEST_ATOMICITY_PROBE_DELAY_MS` (the watch then probes it).
pub fn static_verdict(name: &str, def: &BackendConfig) -> Option<CapabilityVerdict> {
    if !matches!(def, BackendConfig::Filesystem { .. })
        || crate::config::test_seams::test_seams().atomicity_probe_delay_ms > 0
    {
        return None;
    }
    Some(if forced_noncas_backends().contains(name) {
        CapabilityVerdict::NonCas
    } else {
        CapabilityVerdict::CasVerified {
            via: VerifiedVia::LocalFilesystem,
        }
    })
}

/// Every read-only bucket now, by backend: the configured buckets that
/// encrypt with the proxy, and every bucket that the storage listing shows
/// (also those without a policy that inherit the encryption of their
/// backend). A failed listing lists the configured ones only.
pub async fn read_only_buckets(
    app: &crate::api::handlers::AppState,
) -> BTreeMap<String, Vec<String>> {
    let cache = &app.backend_capabilities;
    let plan = cache.encrypted_plan();
    let mut names: BTreeMap<String, Option<String>> =
        plan.buckets().into_iter().map(|(b, _)| (b, None)).collect();
    if let Ok(listed) = app.engine.load().list_bucket_origins().await {
        for b in listed {
            names
                .entry(b.name.to_ascii_lowercase())
                .or_insert(b.backend_name);
        }
    }
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (bucket, held) in names {
        let home = held.map_or(crate::storage::BucketHome::Default, |b| {
            crate::storage::BucketHome::Backend(b)
        });
        if let Some(WriteBlock::ReadOnly { backend }) = cache.write_block(&bucket, &home) {
            out.entry(backend).or_default().push(bucket);
        }
    }
    out
}

/// The buckets that encrypt with the proxy and the definitions of their
/// backends, by the running config: what the write rule reads on every
/// write ([`BackendCapabilityCache::write_block`]).
#[derive(Debug, Clone, Default)]
pub struct EncryptedPlan {
    storage: crate::config::ProxyEncryptedStorage,
    /// Backend name → (definition, its cache fingerprint, computed once:
    /// the rule runs on every client write).
    defs: BTreeMap<String, (BackendConfig, String)>,
}

impl EncryptedPlan {
    /// The plan of `config`.
    pub fn of(config: &Config) -> Self {
        let storage = config.proxy_encrypted_storage();
        let defs = storage
            .backends()
            .into_iter()
            .filter_map(|b| {
                let def = config.backend_by_name(b)?.clone();
                let fp = super::capability::fingerprint(&def);
                Some((b.to_string(), (def, fp)))
            })
            .collect();
        Self { storage, defs }
    }

    /// The backend (name and definition fingerprint) whose atomic writes
    /// `bucket` needs, or `None` (`held_by`: see
    /// [`crate::config::ProxyEncryptedStorage::backend_of`]).
    pub fn backend_of(&self, bucket: &str, held_by: Option<&str>) -> Option<(&str, &str)> {
        let name = self.storage.backend_of(bucket, held_by)?;
        Some((name, self.defs.get(name)?.1.as_str()))
    }

    /// Does the write rule of `bucket` depend on the backend the router
    /// finds it on? Only for a bucket that the config does not name, while
    /// some backend's own site encrypts with the proxy.
    pub fn needs_home(&self, bucket: &str) -> bool {
        !self.storage.sites.is_empty()
            && !self
                .storage
                .configured
                .contains(&bucket.to_ascii_lowercase())
    }

    /// Pure: the write rule of `bucket` at `home`, with `verdict(name,
    /// fingerprint)` the cached verdict of a definition
    /// ([`BackendCapabilityCache::write_block`]). An undetermined home of
    /// a bucket that needs one is pending unless every encrypting site is
    /// CAS (any of them could hold it).
    pub fn write_block(
        &self,
        bucket: &str,
        home: &crate::storage::BucketHome,
        verdict: impl Fn(&str, &str) -> Option<CapabilityVerdict>,
    ) -> Option<WriteBlock> {
        use crate::storage::BucketHome;
        let held_by = match home {
            BucketHome::Backend(b) => Some(b.as_str()),
            BucketHome::Default => None,
            BucketHome::Undetermined if self.needs_home(bucket) => {
                return self.storage.sites.iter().find_map(|site| {
                    let fp = self.defs.get(site.as_str())?.1.as_str();
                    match WriteBlock::of(site, verdict(site, fp))? {
                        WriteBlock::ReadOnly { backend } => Some(WriteBlock::Pending {
                            backend,
                            unverified: None,
                        }),
                        pending => Some(pending),
                    }
                });
            }
            BucketHome::Undetermined => None,
        };
        let (backend, fp) = self.backend_of(bucket, held_by)?;
        WriteBlock::of(backend, verdict(backend, fp))
    }

    /// The configured buckets that encrypt with the proxy: (bucket,
    /// backend).
    pub fn buckets(&self) -> Vec<(String, String)> {
        self.storage
            .buckets
            .iter()
            .map(|(b, n)| (b.clone(), n.clone()))
            .collect()
    }

    /// Every backend that stores a proxy-encrypted bucket.
    pub fn backends(&self) -> Vec<(String, BackendConfig)> {
        self.defs
            .iter()
            .map(|(n, (d, _))| (n.clone(), d.clone()))
            .collect()
    }

    /// The real name of a bucket on `backend` to probe it on first.
    fn hint(&self, config: &Config, backend: &str) -> Option<String> {
        self.storage
            .buckets
            .iter()
            .find(|(_, b)| b.as_str() == backend)
            .map(|(name, _)| config.real_bucket_name(name))
    }
}

/// Pure: when the watch probes a backend of encrypted buckets again,
/// given its cached verdict and how many probes in a row could not verify
/// it (`failures`, counted from 1 after the first one). `None`: never on a
/// timer. No verdict → now. Unknown → 5 s, doubling to 60 s (its buckets
/// answer 503 meanwhile, so the next try comes soon). CAS and non-CAS are
/// verdicts: they are probed again only on Check again, a config change or
/// a restart. A timer on non-CAS would write a probe key into the
/// customer's bucket every few minutes forever (a versioned bucket keeps
/// every version) to detect a backend that rarely changes.
pub fn recheck_after(
    verdict: Option<&CapabilityVerdict>,
    failures: u32,
) -> Option<std::time::Duration> {
    match verdict {
        None => Some(std::time::Duration::ZERO),
        Some(CapabilityVerdict::Unknown { .. }) => {
            let exp = failures.saturating_sub(1).min(4);
            Some(
                UNKNOWN_RECHECK_FIRST
                    .saturating_mul(1 << exp)
                    .min(UNKNOWN_RECHECK_MAX),
            )
        }
        Some(_) => None,
    }
}

/// The first re-probe of a backend that could not be verified.
const UNKNOWN_RECHECK_FIRST: std::time::Duration = std::time::Duration::from_secs(5);
/// The longest wait between two probes of a backend that could not be
/// verified.
const UNKNOWN_RECHECK_MAX: std::time::Duration = std::time::Duration::from_secs(60);

/// Probe every backend of the encrypted plan that has no verdict yet, all
/// at once (boot: this runs in parallel with the boot health check, so the
/// pending window of the encrypted buckets is one probe, at most
/// 15 s (`PROBE_TIMEOUT`), not the health check plus a probe).
pub fn spawn_boot_probes(
    config: Config,
    cache: std::sync::Arc<BackendCapabilityCache>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let plan = cache.encrypted_plan();
        let due: Vec<(String, Option<String>)> = plan
            .backends()
            .into_iter()
            .filter(|(name, def)| cache.get(name, def).is_none())
            .map(|(name, _)| {
                let hint = plan.hint(&config, &name);
                (name, hint)
            })
            .collect();
        futures::future::join_all(
            due.iter()
                .map(|(name, hint)| probe_atomic_writes(&config, &cache, name, hint.as_deref())),
        )
        .await;
    })
}

/// The atomicity watch: probes every backend that stores a bucket that
/// encrypts with the proxy and has no verdict for its current definition
/// (boot, a config change: writes to its encrypted buckets wait with 503
/// meanwhile), and probes again, with a short backoff
/// ([`recheck_after`]), every backend that it could not verify; at once
/// when the health loop sees a backend recover. The backends are probed
/// concurrently. Logs each change of the read-only set (a warning per
/// backend). `boot`: the boot probes ([`spawn_boot_probes`]), awaited
/// before the first round. The returned receiver fires after the first
/// round: the boot waits for it a short, bounded time.
pub fn spawn_watch(
    app: std::sync::Arc<crate::api::handlers::AppState>,
    config: crate::config::SharedConfig,
    boot: Option<tokio::task::JoinHandle<()>>,
) -> tokio::sync::oneshot::Receiver<()> {
    let (first_round, done) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        if let Some(boot) = boot {
            let _ = boot.await;
        }
        let mut first_round = Some(first_round);
        let mut announced: BTreeMap<String, Vec<String>> = BTreeMap::new();
        // Backend → (when probed last, probes in a row without a verdict).
        let mut tries: BTreeMap<String, (std::time::Instant, u32)> = BTreeMap::new();
        let mut recovered = false;
        let mut unverified: BTreeMap<String, String> = BTreeMap::new();
        loop {
            let cfg = config.read().await.clone();
            let cache = &app.backend_capabilities;
            let plan = cache.encrypted_plan();
            let mut probes = Vec::new();
            let mut next_wake: Option<std::time::Duration> = None;
            for (name, def) in plan.backends() {
                let verdict = cache.get(&name, &def);
                let failures = tries.get(&name).map_or(0, |(_, n)| *n);
                let Some(after) = recheck_after(verdict.as_ref(), failures) else {
                    tries.remove(&name);
                    continue;
                };
                let since = tries
                    .get(&name)
                    .map_or(std::time::Duration::MAX, |(t, _)| t.elapsed());
                if verdict.is_none() || recovered || since >= after {
                    let hint = plan.hint(&cfg, &name);
                    probes.push((name, hint));
                } else {
                    let left = after - since;
                    next_wake = Some(next_wake.map_or(left, |w| w.min(left)));
                }
            }
            recovered = false;
            let cfg_ref = &cfg;
            let verdicts =
                futures::future::join_all(probes.iter().map(|(name, hint)| async move {
                    probe_atomic_writes(cfg_ref, cache, name, hint.as_deref()).await
                }))
                .await;
            for ((name, _), fresh) in probes.into_iter().zip(verdicts) {
                let kept = cfg_ref
                    .backend_by_name(&name)
                    .and_then(|d| cache.get(&name, d))
                    .unwrap_or(fresh);
                let entry = tries.entry(name).or_insert((std::time::Instant::now(), 0));
                entry.0 = std::time::Instant::now();
                entry.1 = match kept {
                    CapabilityVerdict::Unknown { .. } => entry.1 + 1,
                    _ => 0,
                };
                if let Some(after) = recheck_after(Some(&kept), entry.1) {
                    next_wake = Some(next_wake.map_or(after, |w| w.min(after)));
                }
            }
            let now_unverified: BTreeMap<String, String> = plan
                .backends()
                .into_iter()
                .filter_map(|(name, def)| match cache.get(&name, &def) {
                    Some(CapabilityVerdict::Unknown { reason }) => Some((name, reason)),
                    _ => None,
                })
                .collect();
            for (backend, reason) in &now_unverified {
                if !unverified.contains_key(backend) {
                    tracing::warn!(
                        "encryption: could not verify the atomic writes of backend '{backend}' \
                         ({reason}); writes to its encrypted buckets answer 503 (retried) until \
                         a check succeeds — see {CAPABILITY_DOC_URL}"
                    );
                }
            }
            unverified = now_unverified;
            if let Some(tx) = first_round.take() {
                let _ = tx.send(());
            }
            let now = read_only_buckets(&app).await;
            if now != announced {
                for (backend, buckets) in &now {
                    tracing::warn!(
                        "encryption: bucket(s) {buckets:?} on backend '{backend}' are READ-ONLY: \
                         encryption needs atomic writes, which the backend does not support \
                         ({}). Reads keep working; writes are refused. Move them to a backend \
                         with atomic writes with a migrate job — see {CAPABILITY_DOC_URL}",
                        verdict_words(&CapabilityVerdict::NonCas)
                    );
                }
                for backend in announced.keys().filter(|b| !now.contains_key(*b)) {
                    tracing::info!(
                        "encryption: the encrypted buckets on backend '{backend}' are no longer \
                         read-only"
                    );
                }
                announced = now;
            }
            let sleep = next_wake.unwrap_or(std::time::Duration::from_secs(3600));
            tokio::select! {
                _ = tokio::time::sleep(sleep) => {}
                _ = cache.plan_changed.notified() => {}
                _ = app.backend_health.recovered.notified() => { recovered = true; }
            }
        }
    });
    done
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(yaml: &str) -> Config {
        Config::from_yaml_str(yaml).expect("fixture parses")
    }

    const KEY: &str = "1111111111111111111111111111111111111111111111111111111111111111";

    fn two_backends(b2_enc: &str, buckets: &str) -> Config {
        cfg(&format!(
            "storage:\n  backends:\n    - name: local-disk\n      type: filesystem\n      \
             path: /tmp/dgp-atomicity-a\n    - name: b2\n      type: filesystem\n      \
             path: /tmp/dgp-atomicity-b\n{b2_enc}  default_backend: local-disk\n  buckets:\n{buckets}"
        ))
    }

    fn aes(id: &str) -> String {
        format!("{{ mode: aes256-gcm-proxy, key: \"{KEY}\", key_id: {id} }}")
    }

    #[test]
    fn the_messages_name_the_backend_the_probe_result_and_the_doc() {
        let r = AtomicityRefusal {
            backend: "b2".into(),
            verdict: CapabilityVerdict::NonCas,
            what: "encryption of bucket 'releases'".into(),
        };
        let m = r.message();
        assert!(m.contains("backend 'b2'"), "{m}");
        assert!(m.contains("as Backblaze B2 does"), "{m}");
        assert_eq!(m.matches("does not support").count(), 1, "said once: {m}");
        assert!(m.contains(CAPABILITY_DOC_URL), "{m}");
        assert_eq!(r.details()["probe"], "non-cas");
        assert_eq!(r.details()["backend"], "b2");
        let unknown = CapabilityVerdict::Unknown {
            reason: "timeout".into(),
        };
        assert!(verdict_words(&unknown).contains("could not verify"));
        assert!(verdict_words(&CapabilityVerdict::NonCas).contains("does not support"));
        let u = AtomicityRefusal {
            verdict: unknown.clone(),
            ..r.clone()
        };
        assert!(
            u.message().contains("could not verify") && !u.message().contains("does not support"),
            "{u}"
        );
        let ro = WriteBlock::ReadOnly {
            backend: "b2".into(),
        };
        assert!(ro.admin_message("releases").contains("'b2'"));
        assert!(proxy_unavailable_reason("b2", &unknown)
            .unwrap()
            .contains("could not verify"));
        assert!(proxy_unavailable_reason("b2", &CapabilityVerdict::NonCas)
            .unwrap()
            .contains("does not support"));
        assert_eq!(
            proxy_unavailable_reason(
                "b2",
                &CapabilityVerdict::CasVerified {
                    via: VerifiedVia::Probe
                }
            ),
            None
        );
    }

    /// C3: an S3 client never gets the probe's error (it can name the
    /// backend's address); the admin message keeps it.
    #[test]
    fn s3_clients_get_fixed_phrases_without_the_probe_error() {
        let secret = "DispatchFailure ConnectError 10.0.0.7:9000";
        let pending = WriteBlock::of(
            "b2",
            Some(CapabilityVerdict::Unknown {
                reason: secret.into(),
            }),
        )
        .unwrap();
        for create in [false, true] {
            let crate::api::errors::S3Error::SlowDown(m) = pending.to_s3_error("photos", create)
            else {
                panic!("pending is a 503")
            };
            assert!(!m.contains(secret) && !m.contains("b2"), "{m}");
            assert!(m.contains(CAPABILITY_DOC_URL), "{m}");
        }
        assert!(pending.admin_message("photos").contains(secret));
        let ro = WriteBlock::of("b2", Some(CapabilityVerdict::NonCas)).unwrap();
        for create in [false, true] {
            let crate::api::errors::S3Error::AccessDeniedReason(m) =
                ro.to_s3_error("photos", create)
            else {
                panic!("read-only is a 403")
            };
            assert!(!m.contains("[atomic_writes"), "no machine tag: {m}");
            assert!(m.contains(CAPABILITY_DOC_URL), "{m}");
        }
    }

    /// The watch re-probes an unverified backend soon (5 s doubling to
    /// 60 s) and never a verdict on a timer (C6: no probe keys pile up in
    /// a non-CAS customer bucket).
    #[test]
    fn the_watch_rechecks_only_what_it_could_not_verify() {
        use std::time::Duration;
        let unknown = CapabilityVerdict::Unknown { reason: "x".into() };
        assert_eq!(recheck_after(None, 0), Some(Duration::ZERO));
        assert_eq!(
            recheck_after(Some(&unknown), 1),
            Some(Duration::from_secs(5))
        );
        assert_eq!(
            recheck_after(Some(&unknown), 2),
            Some(Duration::from_secs(10))
        );
        assert_eq!(
            recheck_after(Some(&unknown), 30),
            Some(Duration::from_secs(60))
        );
        assert_eq!(recheck_after(Some(&CapabilityVerdict::NonCas), 0), None);
        assert_eq!(
            recheck_after(
                Some(&CapabilityVerdict::CasVerified {
                    via: VerifiedVia::Probe
                }),
                0
            ),
            None
        );
    }

    #[test]
    fn targets_are_new_or_changed_proxy_encryption_only() {
        let plain = two_backends("", "    releases: { backend: b2 }\n");
        let bucket = two_backends(
            "",
            &format!(
                "    releases: {{ backend: b2, encryption: {} }}\n",
                aes("a")
            ),
        );
        let t = bucket.atomic_write_targets(&plain);
        assert_eq!(
            t,
            vec![crate::config::AtomicWriteTarget {
                backend: "b2".into(),
                bucket: Some("releases".into())
            }]
        );
        assert!(bucket.atomic_write_targets(&bucket).is_empty(), "no change");
        assert!(
            plain.atomic_write_targets(&bucket).is_empty(),
            "a downgrade needs no atomic writes"
        );
        let site = two_backends(
            &format!("      encryption: {}\n", aes("b")),
            "    releases: { backend: b2 }\n",
        );
        let backends: Vec<_> = site
            .atomic_write_targets(&plain)
            .into_iter()
            .map(|t| (t.backend, t.bucket))
            .collect();
        assert!(backends.contains(&("b2".into(), None)), "{backends:?}");
        let sse = two_backends(
            "",
            "    releases: { backend: b2, encryption: { mode: sse-s3 } }\n",
        );
        assert!(
            sse.atomic_write_targets(&plain).is_empty(),
            "native SSE: the proxy rewrites nothing"
        );
        // A move of an encrypted bucket to another backend is a target.
        let moved = two_backends(
            "",
            &format!(
                "    releases: {{ backend: local-disk, encryption: {} }}\n",
                aes("a")
            ),
        );
        assert_eq!(moved.atomic_write_targets(&bucket)[0].backend, "local-disk");
    }

    #[test]
    fn the_plan_maps_encrypted_buckets_to_their_backend() {
        let c = two_backends(
            "",
            &format!(
                "    releases: {{ backend: b2, encryption: {} }}\n    downloads: {{ backend: b2 }}\n",
                aes("a")
            ),
        );
        let plan = EncryptedPlan::of(&c);
        assert_eq!(
            plan.backend_of("Releases", None).map(|(n, _)| n),
            Some("b2")
        );
        assert_eq!(
            plan.backend_of("releases", None)
                .map(|(_, fp)| fp.to_string()),
            Some(super::super::capability::fingerprint(
                c.backend_by_name("b2").unwrap()
            ))
        );
        assert_eq!(plan.backend_of("downloads", None), None);
        assert_eq!(
            plan.backend_of("unnamed", None),
            None,
            "the default site is plain"
        );
    }

    /// An S3 backend `b2` (its verdict needs a probe) whose own site
    /// encrypts, a plaintext filesystem default, and one configured bucket
    /// on `b2` without a block of its own.
    fn s3_b2_encrypting() -> Config {
        cfg(&format!(
            "storage:\n  backends:\n    - name: local-disk\n      type: filesystem\n      \
             path: /tmp/dgp-atomicity-c\n    - name: b2\n      type: s3\n      \
             endpoint: \"http://127.0.0.1:1\"\n      region: us-east-1\n      \
             access_key_id: k\n      secret_access_key: s\n      encryption: {}\n  \
             default_backend: local-disk\n  buckets:\n    releases: {{ backend: b2 }}\n",
            aes("site")
        ))
    }

    fn home(b: Option<&str>) -> crate::storage::BucketHome {
        b.map_or(crate::storage::BucketHome::Default, |b| {
            crate::storage::BucketHome::Backend(b.into())
        })
    }

    /// The write rule follows the verdict of the backend's CURRENT
    /// definition: no verdict yet or unknown → pending (fail closed, 503,
    /// retried), non-CAS → read-only, CAS → writable. A bucket without a
    /// policy that the router found on the encrypting backend follows the
    /// same rule; on the plaintext default it is never blocked.
    #[test]
    fn the_write_rule_follows_the_verdict() {
        let c = s3_b2_encrypting();
        let cache = BackendCapabilityCache::default();
        cache.set_encrypted_plan(&c);
        let pending = Some(WriteBlock::Pending {
            backend: "b2".into(),
            unverified: None,
        });
        assert_eq!(
            cache.write_block("releases", &home(None)),
            pending,
            "no verdict"
        );
        assert_eq!(
            cache.write_block("photos", &home(Some("b2"))),
            pending,
            "unnamed"
        );
        assert_eq!(
            cache.write_block("photos", &home(None)),
            None,
            "plain default"
        );
        assert_eq!(cache.read_only_reason("releases", &home(None)), None);
        let def = c.backend_by_name("b2").unwrap();
        cache.set("b2", def, CapabilityVerdict::Unknown { reason: "x".into() });
        assert_eq!(
            cache.write_block("releases", &home(None)),
            Some(WriteBlock::Pending {
                backend: "b2".into(),
                unverified: Some("x".into())
            }),
            "unknown is pending, not read-only"
        );
        assert!(cache.read_only_reason("releases", &home(None)).is_none());
        cache.set("b2", def, CapabilityVerdict::NonCas);
        assert!(cache.read_only_reason("releases", &home(None)).is_some());
        assert!(cache
            .read_only_reason("photos", &home(Some("b2")))
            .is_some());
        assert!(cache.read_only_reason("photos", &home(None)).is_none());
        cache.set(
            "b2",
            def,
            CapabilityVerdict::CasVerified {
                via: VerifiedVia::Probe,
            },
        );
        assert_eq!(cache.write_block("releases", &home(None)), None, "lifted");
        assert_eq!(cache.write_block("photos", &home(Some("b2"))), None);
    }

    /// A bucket that the config does not name, whose home the router could
    /// not determine, is pending while any encrypting site lacks a CAS
    /// verdict (it could be there), never judged against the default.
    #[test]
    fn an_undetermined_home_fails_closed() {
        let c = s3_b2_encrypting();
        let cache = BackendCapabilityCache::default();
        cache.set_encrypted_plan(&c);
        let undetermined = crate::storage::BucketHome::Undetermined;
        let def = c.backend_by_name("b2").unwrap();
        cache.set("b2", def, CapabilityVerdict::NonCas);
        assert!(
            matches!(
                cache.write_block("photos", &undetermined),
                Some(WriteBlock::Pending { .. })
            ),
            "may be on the non-CAS site"
        );
        assert_eq!(
            cache.write_block("releases", &undetermined),
            Some(WriteBlock::ReadOnly {
                backend: "b2".into()
            }),
            "a configured bucket needs no home"
        );
        cache.set(
            "b2",
            def,
            CapabilityVerdict::CasVerified {
                via: VerifiedVia::Probe,
            },
        );
        assert_eq!(cache.write_block("photos", &undetermined), None);
    }

    /// Source guard (finding 1 of the N2 review): the write rule judges
    /// the backend a bucket's writes REALLY go to. Only this module asks
    /// `write_block`, always with a home from the router
    /// (`bucket_home`/`bucket_home_now`, or the backend a storage listing
    /// found the bucket on). Nobody judges by the router's no-I/O
    /// `resolved_backend_name` (empty after a boot, an apply or a
    /// CreateBucket, so a cold bucket looked like the default's); every
    /// other caller goes through `AppState::atomic_write_block{,_now}`.
    #[test]
    fn no_caller_judges_an_unresolved_bucket() {
        let mut offenders = Vec::new();
        for (rel, text) in crate::source_scan::prod_sources("src") {
            let code = crate::source_scan::prod_text(&text);
            let here =
                rel == "src/coordination/atomicity.rs" || rel == "src/coordination/capability.rs";
            if !here && code.contains(".write_block(") {
                offenders.push(format!("{rel}: write_block outside the atomicity module"));
            }
            if rel == "src/coordination/atomicity.rs" && code.contains("resolved_backend_name") {
                offenders.push(format!("{rel}: judges by resolved_backend_name"));
            }
            if !here && code.contains("read_only_reason(") && !code.contains("BucketHome") {
                offenders.push(format!("{rel}: read_only_reason without a BucketHome"));
            }
        }
        assert!(offenders.is_empty(), "{offenders:#?}");
    }

    /// A filesystem backend gets its verdict with the plan: its encrypted
    /// buckets never wait for a probe.
    #[test]
    fn a_filesystem_backend_never_waits() {
        let c = two_backends(
            "",
            &format!(
                "    releases: {{ backend: b2, encryption: {} }}\n",
                aes("a")
            ),
        );
        let cache = BackendCapabilityCache::default();
        cache.set_encrypted_plan(&c);
        assert_eq!(cache.write_block("releases", &home(None)), None);
    }

    #[test]
    fn the_messages_of_a_block_say_what_happens() {
        let p = WriteBlock::Pending {
            backend: "b2".into(),
            unverified: None,
        };
        assert!(p.admin_message("releases").contains("still running"));
        assert!(p.message("releases").contains("retry in a few seconds"));
        assert!(matches!(
            p.to_s3_error("releases", false),
            crate::api::errors::S3Error::SlowDown(_)
        ));
        let r = WriteBlock::ReadOnly {
            backend: "b2".into(),
        };
        let m = r.create_message("photos");
        assert!(m.contains("cannot be created"), "{m}");
        assert!(matches!(
            r.to_s3_error("photos", true),
            crate::api::errors::S3Error::AccessDeniedReason(_)
        ));
    }

    /// A probe that meets a B2-like backend (it drops the connection on a
    /// conditional PUT) refuses the action with the plain-words verdict.
    #[tokio::test]
    async fn a_dropped_connection_refuses_encryption() {
        let (endpoint, _) = super::super::cas_probe::b2_like::start(true).await;
        let c = cfg(&format!(
            "storage:\n  backends:\n    - name: b2\n      type: s3\n      endpoint: \"{endpoint}\"\n      \
             region: us-east-1\n      force_path_style: true\n      access_key_id: k\n      \
             secret_access_key: s\n      allow_local: true\n  buckets:\n    coord: {{ backend: b2 }}\n"
        ));
        let cache = BackendCapabilityCache::default();
        let r = require_atomic_writes(
            &c,
            &cache,
            "b2",
            Some("coord"),
            "encryption of bucket 'coord'",
        )
        .await
        .expect_err("refused");
        assert_eq!(r.verdict, CapabilityVerdict::NonCas, "{r}");
        assert!(r.message().contains("as Backblaze B2 does"), "{r}");
    }

    /// A proven CAS verdict, then ONE fresh probe that
    /// fails for a transient reason (unreachable endpoint). Expected: the
    /// running buckets stay writable.
    #[tokio::test]
    async fn a_transient_probe_failure_does_not_make_cas_buckets_read_only() {
        let c = cfg(&format!(
            "storage:\n  backends:\n    - name: local-disk\n      type: filesystem\n      \
             path: /tmp/dgp-atomicity-c\n    - name: b2\n      type: s3\n      \
             endpoint: \"http://127.0.0.1:1\"\n      region: us-east-1\n      allow_local: true\n      \
             access_key_id: k\n      secret_access_key: s\n      encryption: {}\n  \
             default_backend: local-disk\n  buckets:\n    releases: {{ backend: b2 }}\n",
            aes("site")
        ));
        let cache = BackendCapabilityCache::default();
        cache.set_encrypted_plan(&c);
        let def = c.backend_by_name("b2").unwrap();
        cache.set(
            "b2",
            def,
            CapabilityVerdict::CasVerified {
                via: VerifiedVia::Probe,
            },
        );
        assert_eq!(cache.write_block("releases", &home(None)), None, "writable");
        let v = probe_atomic_writes(&c, &cache, "b2", Some("releases")).await;
        eprintln!("fresh probe verdict: {v:?}");
        assert_eq!(
            cache.write_block("releases", &home(None)),
            None,
            "after a transient probe failure"
        );
    }

    /// A probe of a NOT-running definition of the same
    /// backend name (validate / refused apply with rotated credentials)
    /// must not evict the running definition's verdict.
    #[tokio::test]
    async fn a_probe_of_another_definition_keeps_the_running_verdict() {
        let c = s3_b2_encrypting();
        let cache = BackendCapabilityCache::default();
        cache.set_encrypted_plan(&c);
        let def = c.backend_by_name("b2").unwrap();
        cache.set(
            "b2",
            def,
            CapabilityVerdict::CasVerified {
                via: VerifiedVia::Probe,
            },
        );
        let mut candidate = c.clone();
        for b in candidate.backends.iter_mut() {
            if b.name == "b2" {
                if let BackendConfig::S3 {
                    secret_access_key, ..
                } = &mut b.backend
                {
                    *secret_access_key = Some("rotated".into());
                }
            }
        }
        let _ = probe_atomic_writes(&candidate, &cache, "b2", Some("releases")).await;
        assert_eq!(
            cache.write_block("releases", &home(None)),
            None,
            "running definition still CAS"
        );
    }

    #[tokio::test]
    async fn a_filesystem_backend_has_atomic_writes_without_a_request() {
        let c = two_backends("", "    releases: { backend: b2 }\n");
        let cache = BackendCapabilityCache::default();
        let proof = require_atomic_writes(&c, &cache, "b2", None, "x")
            .await
            .expect("filesystem");
        assert_eq!(proof.backend(), "b2");
    }
}
