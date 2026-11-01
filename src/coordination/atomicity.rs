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
    establish_backend_verdict, forced_noncas_backends, BackendCapabilityCache, CapabilityVerdict,
    ClientWritableGroup, VerifiedVia, CAPABILITY_DOC_URL, PROBE_TIMEOUT,
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

/// Pure: the probe result in plain words, for messages and the UI.
pub fn verdict_words(verdict: &CapabilityVerdict) -> String {
    match verdict {
        CapabilityVerdict::CasVerified { .. } => {
            "the backend enforces conditional writes".to_string()
        }
        CapabilityVerdict::NonCas => "the backend ignored or rejected a conditional write, as \
                                      Backblaze B2 does"
            .to_string(),
        CapabilityVerdict::Unknown { reason } => {
            format!("could not reach the backend to verify ({reason})")
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
        format!(
            "{} refused: encryption needs atomic writes (conditional PUT and DELETE), and \
             backend '{}' does not provide them: {}. Keep encrypted buckets on a backend \
             with atomic writes (move a bucket there with a migrate job) — see {CAPABILITY_DOC_URL}",
            self.what,
            self.backend,
            verdict_words(&self.verdict)
        )
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
    for (name, policy) in &config.buckets {
        if name.starts_with(crate::maintenance::migrate::TRANSIENT_PREFIX) {
            continue;
        }
        if config
            .effective_backend_for_bucket(name)
            .is_some_and(|(b, _)| b == backend)
        {
            let real = policy.alias.clone().unwrap_or_else(|| name.clone());
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
async fn pick_probe_bucket(def: &BackendConfig, candidates: &[String]) -> Result<String, String> {
    let client = crate::config_db_sync::ConfigDbSync::build_client_labelled(
        def,
        crate::storage::S3Backend::request_timeout(),
        crate::storage::CAPABILITY_PROBE,
    )
    .await
    .map_err(|e| format!("client build failed: {e}"))?;
    let listed = tokio::time::timeout(PROBE_TIMEOUT, client.list_buckets().send()).await;
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
        match pick_probe_bucket(def, &probe_candidates(config, backend, hint)).await {
            Err(reason) => CapabilityVerdict::Unknown { reason },
            Ok(probe_bucket) => {
                let group = ClientWritableGroup {
                    backend: def.clone(),
                    buckets: Vec::new(),
                    probe_bucket,
                };
                establish_backend_verdict(backend, &group, &forced).await
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

/// [`require_atomic_writes`] for the backend that `bucket` routes to, with
/// the bucket's real name as the probe hint.
pub async fn require_for_bucket(
    config: &Config,
    cache: &BackendCapabilityCache,
    bucket: &str,
    what: &str,
) -> Result<AtomicWrites, AtomicityRefusal> {
    let key = bucket.to_ascii_lowercase();
    let Some((backend, _)) = config.effective_backend_for_bucket(&key) else {
        return Err(AtomicityRefusal {
            backend: "(none)".to_string(),
            verdict: CapabilityVerdict::Unknown {
                reason: format!("bucket '{bucket}' routes to an unknown backend"),
            },
            what: what.to_string(),
        });
    };
    let real = config
        .buckets
        .get(&key)
        .and_then(|p| p.alias.clone())
        .unwrap_or(key);
    require_atomic_writes(config, cache, &backend, Some(&real), what).await
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
        let hint = bucket.as_ref().map(|b| {
            new.buckets
                .get(b)
                .and_then(|p| p.alias.clone())
                .unwrap_or_else(|| b.clone())
        });
        require_atomic_writes(new, cache, &backend, hint.as_deref(), &what).await?;
    }
    Ok(())
}

/// Pure: the read-only message of `bucket` on `backend` under `verdict`,
/// or `None` when the verdict proves atomic writes.
pub fn read_only_message(
    bucket: &str,
    backend: &str,
    verdict: &CapabilityVerdict,
) -> Option<String> {
    if matches!(verdict, CapabilityVerdict::CasVerified { .. }) {
        return None;
    }
    Some(format!(
        "Bucket '{bucket}' is read-only: it is encrypted, and encryption needs atomic writes, \
         which backend '{backend}' does not provide ({}). Reads keep working. To write to it \
         again, move the bucket to a backend with atomic writes with a migrate job — see \
         {CAPABILITY_DOC_URL}",
        verdict_words(verdict)
    ))
}

/// Pure: why proxy encryption is not offered on `backend` under
/// `verdict`, or `None` when the verdict proves atomic writes.
pub fn proxy_unavailable_reason(backend: &str, verdict: &CapabilityVerdict) -> Option<String> {
    if matches!(verdict, CapabilityVerdict::CasVerified { .. }) {
        return None;
    }
    Some(format!(
        "Encryption with a proxy key needs atomic writes, which backend '{backend}' does not \
         provide ({}) — see {CAPABILITY_DOC_URL}",
        verdict_words(verdict)
    ))
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
    /// The backend has no verdict yet for its current definition (boot, or
    /// a config change; the watch probes it now). Fail closed: S3 `503
    /// SlowDown`, which clients retry.
    Pending { backend: String },
    /// The backend has no proven atomic writes: the bucket is read-only
    /// (S3 `403 AccessDenied`).
    ReadOnly {
        backend: String,
        verdict: CapabilityVerdict,
    },
}

impl WriteBlock {
    /// Pure: the block of a bucket on `backend` under `verdict` (`None`:
    /// no verdict yet).
    pub fn of(backend: &str, verdict: Option<CapabilityVerdict>) -> Option<Self> {
        match verdict {
            None => Some(Self::Pending {
                backend: backend.to_string(),
            }),
            Some(CapabilityVerdict::CasVerified { .. }) => None,
            Some(verdict) => Some(Self::ReadOnly {
                backend: backend.to_string(),
                verdict,
            }),
        }
    }

    /// Is this the read-only case?
    pub fn is_read_only(&self) -> bool {
        matches!(self, Self::ReadOnly { .. })
    }

    /// The operator-facing message of a write to `bucket`.
    pub fn message(&self, bucket: &str) -> String {
        match self {
            Self::Pending { backend } => format!(
                "Bucket '{bucket}' is encrypted, and the check that backend '{backend}' has \
                 atomic writes (which encryption needs) is still running. Writes wait until it \
                 ends; retry in a few seconds. Reads keep working — see {CAPABILITY_DOC_URL}"
            ),
            Self::ReadOnly { backend, verdict } => {
                read_only_message(bucket, backend, verdict).unwrap_or_default()
            }
        }
    }

    /// The message of a CreateBucket of `bucket` that would encrypt.
    pub fn create_message(&self, bucket: &str) -> String {
        match self {
            Self::Pending { .. } => self.message(bucket),
            Self::ReadOnly { backend, verdict } => format!(
                "Bucket '{bucket}' cannot be created: it would encrypt with the proxy on backend \
                 '{backend}', and encryption needs atomic writes (conditional PUT and DELETE), \
                 which the backend does not provide ({}). The proxy does not create a bucket \
                 that would be read-only. Create it on a backend with atomic writes — see \
                 {CAPABILITY_DOC_URL} [atomic_writes: backend={backend}, probe={}]",
                verdict_words(verdict),
                verdict_tag(verdict)
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
    /// [`BackendCapabilityCache::write_block`] of `bucket`, with the
    /// backend that the router resolved for it.
    pub fn atomic_write_block(&self, bucket: &str) -> Option<WriteBlock> {
        use crate::storage::StorageBackend;
        let held = self.engine.load().storage().resolved_backend_name(bucket);
        self.backend_capabilities
            .write_block(bucket, held.as_deref())
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
        if let Some(WriteBlock::ReadOnly { backend, .. }) =
            cache.write_block(&bucket, held.as_deref())
        {
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
            .map(|(name, _)| {
                config
                    .buckets
                    .get(name)
                    .and_then(|p| p.alias.clone())
                    .unwrap_or_else(|| name.clone())
            })
    }
}

/// How often the watch probes a backend of encrypted buckets again whose
/// verdict does not prove atomic writes.
const RECHECK: std::time::Duration = std::time::Duration::from_secs(600);

/// The atomicity watch: probes every backend that stores a bucket that
/// encrypts with the proxy, at once when the backend has no verdict for
/// its current definition (boot, a config change; writes to its encrypted
/// buckets wait with 503 meanwhile), and every ten minutes while its
/// verdict does not prove atomic writes, so a fixed backend lifts its
/// buckets' read-only state without a restart. The backends are probed
/// concurrently. Logs each change of the read-only set (a warning per
/// backend). The returned receiver fires after the first round: the boot
/// waits for it a short, bounded time.
pub fn spawn_watch(
    app: std::sync::Arc<crate::api::handlers::AppState>,
    config: crate::config::SharedConfig,
) -> tokio::sync::oneshot::Receiver<()> {
    let (first_round, done) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let mut first_round = Some(first_round);
        let mut announced: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut last_probe: BTreeMap<String, std::time::Instant> = BTreeMap::new();
        loop {
            let cfg = config.read().await.clone();
            let cache = &app.backend_capabilities;
            let plan = cache.encrypted_plan();
            let mut probes = Vec::new();
            for (name, def) in plan.backends() {
                let due = match cache.get(&name, &def) {
                    None => true,
                    Some(CapabilityVerdict::CasVerified { .. }) => false,
                    Some(_) => last_probe.get(&name).is_none_or(|t| t.elapsed() >= RECHECK),
                };
                if due {
                    let hint = plan.hint(&cfg, &name);
                    probes.push((name, hint));
                }
            }
            let cfg_ref = &cfg;
            futures::future::join_all(probes.iter().map(|(name, hint)| async move {
                probe_atomic_writes(cfg_ref, cache, name, hint.as_deref()).await
            }))
            .await;
            for (name, _) in probes {
                last_probe.insert(name, std::time::Instant::now());
            }
            if let Some(tx) = first_round.take() {
                let _ = tx.send(());
            }
            let now = read_only_buckets(&app).await;
            if now != announced {
                for (backend, buckets) in &now {
                    let verdict = cfg
                        .backend_by_name(backend)
                        .and_then(|d| cache.get(backend, d));
                    tracing::warn!(
                        "encryption: bucket(s) {buckets:?} on backend '{backend}' are READ-ONLY: \
                         encryption needs atomic writes, which the backend does not provide \
                         ({}). Reads keep working; writes are refused. Move them to a backend \
                         with atomic writes with a migrate job — see {CAPABILITY_DOC_URL}",
                        verdict.as_ref().map_or_else(String::new, verdict_words)
                    );
                }
                for backend in announced.keys().filter(|b| !now.contains_key(*b)) {
                    tracing::info!(
                        "encryption: the encrypted buckets on backend '{backend}' are writable \
                         again (atomic writes verified)"
                    );
                }
                announced = now;
            }
            tokio::select! {
                _ = tokio::time::sleep(RECHECK) => {}
                _ = cache.plan_changed.notified() => {}
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
        assert!(m.contains(CAPABILITY_DOC_URL), "{m}");
        assert_eq!(r.details()["probe"], "non-cas");
        assert_eq!(r.details()["backend"], "b2");
        let unknown = CapabilityVerdict::Unknown {
            reason: "timeout".into(),
        };
        assert!(verdict_words(&unknown).contains("could not reach the backend to verify"));
        let ro = read_only_message("releases", "b2", &unknown).unwrap();
        assert!(ro.contains("read-only") && ro.contains("'b2'"), "{ro}");
        assert_eq!(
            read_only_message(
                "releases",
                "b2",
                &CapabilityVerdict::CasVerified {
                    via: VerifiedVia::Probe
                }
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

    /// The write rule follows the verdict of the backend's CURRENT
    /// definition: no verdict yet → pending (fail closed), non-CAS and
    /// unknown → read-only, CAS → writable. A bucket without a policy that
    /// the router found on the encrypting backend follows the same rule; on
    /// the plaintext default it is never blocked.
    #[test]
    fn the_write_rule_follows_the_verdict() {
        let c = s3_b2_encrypting();
        let cache = BackendCapabilityCache::default();
        cache.set_encrypted_plan(&c);
        let pending = Some(WriteBlock::Pending {
            backend: "b2".into(),
        });
        assert_eq!(cache.write_block("releases", None), pending, "no verdict");
        assert_eq!(cache.write_block("photos", Some("b2")), pending, "unnamed");
        assert_eq!(cache.write_block("photos", None), None, "plain default");
        assert_eq!(cache.read_only_reason("releases", None), None);
        let def = c.backend_by_name("b2").unwrap();
        cache.set("b2", def, CapabilityVerdict::NonCas);
        assert!(cache.read_only_reason("releases", None).is_some());
        assert!(cache.read_only_reason("photos", Some("b2")).is_some());
        assert!(cache.read_only_reason("photos", None).is_none());
        cache.set("b2", def, CapabilityVerdict::Unknown { reason: "x".into() });
        assert!(cache.read_only_reason("releases", None).is_some());
        cache.set(
            "b2",
            def,
            CapabilityVerdict::CasVerified {
                via: VerifiedVia::Probe,
            },
        );
        assert_eq!(cache.write_block("releases", None), None, "lifted");
        assert_eq!(cache.write_block("photos", Some("b2")), None);
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
        assert_eq!(cache.write_block("releases", None), None);
    }

    #[test]
    fn the_messages_of_a_block_say_what_happens() {
        let p = WriteBlock::Pending {
            backend: "b2".into(),
        };
        assert!(p.message("releases").contains("still running"));
        assert!(matches!(
            p.to_s3_error("releases", false),
            crate::api::errors::S3Error::SlowDown(_)
        ));
        let r = WriteBlock::ReadOnly {
            backend: "b2".into(),
            verdict: CapabilityVerdict::NonCas,
        };
        let m = r.create_message("photos");
        assert!(
            m.contains("cannot be created")
                && m.contains("backend=b2")
                && m.contains("probe=non-cas"),
            "{m}"
        );
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
