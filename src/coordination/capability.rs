// SPDX-License-Identifier: BUSL-1.1

//! Per-backend conditional-write capability verdicts.
//!
//! Filled at startup by the backend write-capability gate (multi-instance
//! only) and consulted by (a) the hot-apply pre-commit gate — a `/config/apply`
//! must not route a client-writable bucket onto a known-non-CAS backend — and
//! (b) the admin backends API, so the GUI can show a capability banner.
//! Verdicts are per BACKEND NAME (the probe runs once per distinct backend).

use std::collections::HashMap;

/// Doc anchor for every backend-capability / write-boundary enforcement
/// message (403s, FATAL startup lines, apply rejections). The GUI linkifies
/// it and rewrites it to the in-app docs viewer.
pub const CAPABILITY_DOC_URL: &str =
    "https://deltaglider.com/docs/how-to/backend-capability-validation";

/// How a `CasVerified` verdict was established (for operator-facing logs/GUI).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VerifiedVia {
    /// Live `If-None-Match` + `If-Match` probe (`cas_probe`) ran this boot.
    Probe,
    /// A filesystem backend: one node, whose in-process object locks make
    /// every check-then-write one step. No request is sent.
    LocalFilesystem,
}

/// The capability verdict for one backend.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "verdict", rename_all = "kebab-case")]
pub enum CapabilityVerdict {
    /// Conditional writes enforced (412 on a violated `If-None-Match:*`).
    CasVerified {
        via: VerifiedVia,
        /// The form of the ETag in `If-Match` that the backend enforces
        /// (`unquoted` on Ceph RGW: Hetzner). Missing in a verdict from
        /// before the dialect existed = `quoted`.
        #[serde(default)]
        if_match: super::if_match::IfMatchDialect,
    },
    /// DEFINITIVE: the backend silently ignored the condition. Unsafe for
    /// multi-instance client-writable buckets and for coordination.
    NonCas,
    /// The probe could not run (network error, missing bucket). Not a
    /// verdict — surfaced loudly but never treated as safe OR unsafe.
    Unknown { reason: String },
}

/// Thread-safe verdict map, indexed by backend NAME and DEFINITION,
/// `Arc`-shared into `AppState`.
///
/// Entries carry a FINGERPRINT of the `BackendConfig` they were probed
/// against: a hot-apply that redefines a backend (new endpoint, rotated
/// credentials) under the same name misses the cache and re-probes — a stale
/// `CasVerified` must never vouch for a swapped-out endpoint, and a stale
/// `NonCas` must never lock out a fixed one. A name keeps the verdicts of
/// up to `DEFINITIONS_PER_NAME` (4) definitions, so probing a CANDIDATE
/// definition (a validate or a refused apply with rotated credentials)
/// never evicts the verdict of the RUNNING one.
#[derive(Debug, Default)]
pub struct BackendCapabilityCache {
    /// Name → (definition fingerprint, verdict), oldest first.
    verdicts: parking_lot::RwLock<HashMap<String, Vec<(String, CapabilityVerdict)>>>,
    /// Name → fingerprint of the running definition (from the last
    /// [`Self::set_encrypted_plan`]): never evicted.
    running: parking_lot::RwLock<HashMap<String, String>>,
    /// Which buckets encrypt with the proxy, on which backend, by the
    /// running config ([`super::atomicity`]): the read-only rule.
    encrypted: parking_lot::RwLock<super::atomicity::EncryptedPlan>,
    /// Wakes the atomicity watch ([`super::atomicity::spawn_watch`]) after
    /// a config change, and after the verdict of a running definition
    /// changes (the watch logs the new read-only set).
    pub(crate) plan_changed: tokio::sync::Notify,
    /// The storage wrappers' view of the backends without proven atomic
    /// writes ([`crate::storage::AtomicityGate`]), kept current here.
    storage_gate: std::sync::OnceLock<std::sync::Arc<crate::storage::AtomicityGate>>,
}

/// How many definitions of one backend name keep a verdict.
const DEFINITIONS_PER_NAME: usize = 4;

/// In-memory identity of a backend definition (includes credentials — never
/// persisted or exposed; a credential rotation deliberately re-probes).
/// Shared with the sibling health cache (`super::health`).
pub(crate) fn fingerprint(config: &crate::config::BackendConfig) -> String {
    serde_json::to_string(config).unwrap_or_default()
}

/// Pure: the verdict to keep when a probe of the same definition answers
/// `new` over `old`. `Unknown` is not evidence: it never replaces a proven
/// `CasVerified` or `NonCas` (a transient network error must not make a
/// CAS backend's buckets wait, nor lift a non-CAS one's read-only state).
pub fn merged_verdict(
    old: Option<&CapabilityVerdict>,
    new: CapabilityVerdict,
) -> CapabilityVerdict {
    match (old, &new) {
        (
            Some(old @ (CapabilityVerdict::CasVerified { .. } | CapabilityVerdict::NonCas)),
            CapabilityVerdict::Unknown { .. },
        ) => old.clone(),
        _ => new,
    }
}

impl BackendCapabilityCache {
    /// Store the verdict of `backend` under definition `config`
    /// ([`merged_verdict`] with the one it had). Returns the verdict kept.
    pub fn set(
        &self,
        backend: &str,
        config: &crate::config::BackendConfig,
        verdict: CapabilityVerdict,
    ) -> CapabilityVerdict {
        let fp = fingerprint(config);
        let running = self.running.read().get(backend).cloned();
        let (kept, changed) = {
            let mut map = self.verdicts.write();
            let entries = map.entry(backend.to_string()).or_default();
            let old = entries
                .iter()
                .position(|(f, _)| *f == fp)
                .map(|i| entries.remove(i).1);
            let kept = merged_verdict(old.as_ref(), verdict);
            let changed = old.as_ref() != Some(&kept);
            entries.push((fp.clone(), kept.clone()));
            while entries.len() > DEFINITIONS_PER_NAME {
                // Oldest first, never the running definition.
                match entries
                    .iter()
                    .position(|(f, _)| Some(f) != running.as_ref() && *f != fp)
                {
                    Some(i) => {
                        entries.remove(i);
                    }
                    None => break,
                }
            }
            (kept, changed)
        };
        if changed && running.as_deref() == Some(fp.as_str()) {
            self.publish_unproven();
            self.plan_changed.notify_one();
        }
        kept
    }

    /// Hand the storage wrappers' atomic-writes gate to this cache (boot,
    /// once the engine exists): it is kept current from now on.
    pub fn attach_storage_gate(&self, gate: std::sync::Arc<crate::storage::AtomicityGate>) {
        let _ = self.storage_gate.set(gate);
        self.publish_unproven();
    }

    /// Store in the storage gate the backends of the running plan whose
    /// running definition has no CAS verdict.
    fn publish_unproven(&self) {
        let Some(gate) = self.storage_gate.get() else {
            return;
        };
        let plan = self.encrypted.read().clone();
        let unproven = plan
            .backends()
            .into_iter()
            .filter(|(name, def)| {
                !matches!(
                    self.get(name, def),
                    Some(CapabilityVerdict::CasVerified { .. })
                )
            })
            .map(|(name, _)| name)
            .collect();
        gate.set_unproven(unproven);
    }

    /// Verdict for this backend NAME, only if it was established against this
    /// exact backend DEFINITION. `None` = never probed or definition changed.
    pub fn get(
        &self,
        backend: &str,
        config: &crate::config::BackendConfig,
    ) -> Option<CapabilityVerdict> {
        self.get_fp(backend, &fingerprint(config))
    }

    fn get_fp(&self, backend: &str, fp: &str) -> Option<CapabilityVerdict> {
        self.verdicts
            .read()
            .get(backend)?
            .iter()
            .find(|(f, _)| f == fp)
            .map(|(_, v)| v.clone())
    }

    /// Install the running config: its backend definitions (their verdicts
    /// are never evicted) and its encrypted-bucket plan (boot, and every
    /// config transition), and wake the atomicity watch. A backend whose
    /// verdict needs no request gets it here, at once: a filesystem
    /// backend has atomic writes (the test seam
    /// `DGP_TEST_FORCE_NONCAS_BACKEND` makes it non-CAS), so its buckets
    /// never wait for the watch.
    pub fn set_encrypted_plan(&self, config: &crate::config::Config) {
        *self.running.write() = config
            .backends
            .iter()
            .map(|b| (b.name.clone(), fingerprint(&b.backend)))
            .collect();
        let plan = super::atomicity::EncryptedPlan::of(config);
        for (name, def) in plan.backends() {
            if let Some(v) = super::atomicity::static_verdict(&name, &def) {
                self.set(&name, &def, v);
            }
        }
        *self.encrypted.write() = plan;
        self.publish_unproven();
        self.plan_changed.notify_one();
    }

    /// The running encrypted-bucket plan.
    pub(crate) fn encrypted_plan(&self) -> super::atomicity::EncryptedPlan {
        self.encrypted.read().clone()
    }

    /// THE write rule of encrypted buckets (N2): why a write to `bucket`
    /// must not pass now, or `None`. A bucket that encrypts with the proxy
    /// needs atomic writes on the backend that holds it (`home`: where the
    /// router sends the bucket's writes, see
    /// [`crate::storage::BucketHome`]). For the backend's CURRENT
    /// definition: no verdict yet or `Unknown` →
    /// [`super::atomicity::WriteBlock::Pending`] (fail closed, retried);
    /// non-CAS → [`super::atomicity::WriteBlock::ReadOnly`]; CAS → `None`.
    /// A home that the router could not determine is judged against every
    /// backend that could hold the bucket and encrypts: pending unless all
    /// of them are CAS. Memory only.
    pub fn write_block(
        &self,
        bucket: &str,
        home: &crate::storage::BucketHome,
    ) -> Option<super::atomicity::WriteBlock> {
        let plan = self.encrypted.read();
        let verdict = |backend: &str, fp: &str| self.get_fp(backend, fp);
        plan.write_block(bucket, home, verdict)
    }

    /// Why `bucket` is read-only now ([`Self::write_block`], the
    /// read-only case only), or `None`.
    pub fn read_only_reason(
        &self,
        bucket: &str,
        home: &crate::storage::BucketHome,
    ) -> Option<String> {
        self.write_block(bucket, home)
            .filter(super::atomicity::WriteBlock::is_read_only)
            .map(|b| b.admin_message(bucket))
    }
}

/// One named S3 backend that hosts client-writable routed buckets, plus the
/// bucket to probe on it (alias-resolved real name of the first routed bucket).
#[derive(Debug, Clone)]
pub struct ClientWritableGroup {
    pub backend: crate::config::BackendConfig,
    /// Virtual bucket names routed here (for the operator-facing error).
    pub buckets: Vec<String>,
    /// Alias-resolved real bucket the probe writes into.
    pub probe_bucket: String,
}

/// Pure projection: which S3 backends host at least one client-writable
/// bucket (by [`crate::config::Config::effective_backend_for_bucket`], so an
/// unrouted bucket counts for the default backend), and therefore need a CAS
/// verdict under multi-instance.
///
/// Skipped by design: `replication_target_only` buckets (no client writers)
/// and filesystem backends (per-node local, single-writer by nature).
/// Compression policy is deliberately IGNORED: it is hot-flippable, so a
/// `compression: false` exemption would be unsound.
pub fn client_writable_s3_backends(
    config: &crate::config::Config,
) -> std::collections::BTreeMap<String, ClientWritableGroup> {
    let mut groups: std::collections::BTreeMap<String, ClientWritableGroup> = Default::default();
    for (bucket, policy) in &config.buckets {
        if policy.replication_target_only {
            continue;
        }
        let Some((backend_name, backend)) = config.effective_backend_for_bucket(bucket) else {
            continue; // undefined backend: refused by Config::check_fatal()
        };
        if matches!(backend, crate::config::BackendConfig::Filesystem { .. }) {
            continue;
        }
        let real = config.real_bucket_name(bucket);
        groups
            .entry(backend_name)
            .or_insert_with(|| ClientWritableGroup {
                backend: backend.clone(),
                buckets: Vec::new(),
                probe_bucket: real,
            })
            .buckets
            .push(bucket.clone());
    }
    groups
}

/// Pure: which bucket to probe the DEFAULT backend on, given the real
/// buckets it lists. Buckets with a policy are skipped: they are covered by
/// [`client_writable_s3_backends`] or exempt. `None` = no unrouted bucket.
pub fn pick_default_probe_bucket(
    config: &crate::config::Config,
    listed: &[String],
) -> Option<String> {
    listed
        .iter()
        .find(|b| !config.buckets.contains_key(&b.to_ascii_lowercase()))
        .cloned()
}

/// The DEFAULT backend as a probe group when it is S3 and hosts buckets
/// with no policy (clients write those, and config cannot name them), and
/// no policy bucket already put it in [`client_writable_s3_backends`].
/// Lists the backend's buckets to find one to probe on (bounded by
/// `PROBE_TIMEOUT`). The forced-non-CAS seam skips the listing.
pub async fn default_backend_group(
    config: &crate::config::Config,
    groups: &std::collections::BTreeMap<String, ClientWritableGroup>,
    forced_noncas: &std::collections::BTreeSet<String>,
) -> Option<(String, ClientWritableGroup)> {
    let name = config.default_backend_name();
    if groups.contains_key(&name) {
        return None;
    }
    let def = config.backend_by_name(&name)?;
    if !matches!(def, crate::config::BackendConfig::S3 { .. }) {
        return None;
    }
    let group = |probe_bucket: String| ClientWritableGroup {
        backend: def.clone(),
        buckets: vec![format!("(buckets without a policy on '{name}')")],
        probe_bucket,
    };
    if forced_noncas.contains(&name) {
        return Some((name.clone(), group(String::new())));
    }
    let client = crate::config_db_sync::ConfigDbSync::build_client_labelled(
        def,
        crate::storage::S3Backend::request_timeout(),
        crate::storage::CAPABILITY_PROBE,
    )
    .await
    .ok()?;
    let listed = tokio::time::timeout(PROBE_TIMEOUT, client.list_buckets().send())
        .await
        .ok()?
        .ok()?;
    let names: Vec<String> = listed
        .buckets()
        .iter()
        .filter_map(|b| b.name().map(str::to_string))
        .collect();
    let bucket = pick_default_probe_bucket(config, &names)?;
    Some((name.clone(), group(bucket)))
}

/// [`client_writable_s3_backends`] plus the default backend's group.
pub async fn client_writable_groups_with_default(
    config: &crate::config::Config,
    forced_noncas: &std::collections::BTreeSet<String>,
) -> std::collections::BTreeMap<String, ClientWritableGroup> {
    let mut groups = client_writable_s3_backends(config);
    if let Some((name, g)) = default_backend_group(config, &groups, forced_noncas).await {
        groups.insert(name, g);
    }
    groups
}

/// Test seam: backends listed in `DGP_TEST_FORCE_NONCAS_BACKEND` (comma-
/// separated) get a forced NonCas verdict without probing — the only way to
/// exercise the fail-fast path against a MinIO-only test harness.
pub fn forced_noncas_backends() -> std::collections::BTreeSet<String> {
    let seams = crate::config::test_seams::test_seams();
    if seams
        .force_noncas_while_file
        .as_ref()
        .is_some_and(|f| !f.exists())
    {
        return Default::default();
    }
    seams.force_noncas_backends.clone()
}

/// How long a backend capability probe may run before we give up with an
/// `Unknown` verdict (bounds both the startup gate and the hot-apply gate).
pub(crate) const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Establish a CAS verdict for one backend group: forced-hook → live probe.
/// Shared by the startup gate and the hot-apply pre-commit gate; the CALLER
/// decides what each verdict means (exit(1) vs reject-apply vs warn).
///
/// NO witness object in the data bucket: a witness there is client-visible,
/// blocks DeleteBucket with a ghost key, and lands unencrypted on encrypting
/// backends. The probe is about 7 requests for the conditional PUT plus
/// about 15 for the conditional CompleteMultipartUpload (three tiny
/// uploads), once per (backend definition, boot), both within ONE deadline
/// (`PROBE_TIMEOUT`, 15 s) — the in-memory fingerprint cache absorbs repeats
/// within a process.
pub async fn establish_backend_verdict(
    name: &str,
    group: &ClientWritableGroup,
    forced_noncas: &std::collections::BTreeSet<String>,
) -> CapabilityVerdict {
    establish_backend_verdict_within(name, group, forced_noncas, PROBE_TIMEOUT).await
}

/// [`establish_backend_verdict`] bounded by `budget` (the rest of a
/// caller's deadline).
pub async fn establish_backend_verdict_within(
    name: &str,
    group: &ClientWritableGroup,
    forced_noncas: &std::collections::BTreeSet<String>,
    budget: std::time::Duration,
) -> CapabilityVerdict {
    if forced_noncas.contains(name) {
        return CapabilityVerdict::NonCas;
    }
    let client = match crate::config_db_sync::ConfigDbSync::build_client_labelled(
        &group.backend,
        crate::storage::S3Backend::request_timeout(),
        crate::storage::CAPABILITY_PROBE,
    )
    .await
    {
        Ok(c) => c,
        Err(e) => {
            return CapabilityVerdict::Unknown {
                reason: format!("client build failed: {e}"),
            }
        }
    };
    let probe_key = super::cas_probe::new_probe_key();
    // ONE deadline for both stages: the callers (the boot gate, the hot-apply
    // gate, the encrypted-bucket atomic-writes check that answers 503 while
    // it runs) rely on a probe that ends within `budget`.
    let deadline = tokio::time::Instant::now() + budget;
    // Litter of an earlier probe that never ended goes first, within a third
    // of the budget, so the probe itself keeps the rest.
    super::cas_probe::sweep_stale_probes(
        &client,
        &group.probe_bucket,
        chrono::Utc::now().timestamp(),
        tokio::time::Instant::now() + budget / 3,
    )
    .await;
    // A future that the deadline cuts is dropped, and its probe key is then
    // removed by the probe's own drop guard (`cas_probe::SweepOnDrop`, with
    // ALL its versions: a versioned bucket, as every B2 bucket is, keeps a
    // noncurrent version of an unversioned DELETE), so no client-visible key
    // stays in a data bucket.
    match tokio::time::timeout_at(
        deadline,
        super::cas_probe::probe_cas(&client, &group.probe_bucket, &probe_key),
    )
    .await
    {
        Err(_) => CapabilityVerdict::Unknown {
            reason: format!("capability probe timed out ({}s)", budget.as_secs()),
        },
        Ok(Ok(p)) => {
            // A backend that enforces the conditional PUT is asked about the
            // conditional CompleteMultipartUpload too: an object above 5 GiB
            // is written with a multipart upload, and a write with a
            // condition then needs the proof. It gets what is left of the
            // deadline; no time left or a cut stage is "not proven".
            let mut profile = p.profile();
            if p.cas {
                let key = format!("{probe_key}-mpu");
                profile.conditional_complete = tokio::time::timeout_at(
                    deadline,
                    super::cas_probe::probe_conditional_complete(
                        &client,
                        &group.probe_bucket,
                        &key,
                        p.if_match,
                    ),
                )
                .await
                .unwrap_or(None);
            }
            // Every conditional request of the backends built from this
            // definition uses what the probe found.
            super::if_match::record_for(&group.backend, profile);
            if p.cas {
                CapabilityVerdict::CasVerified {
                    via: VerifiedVia::Probe,
                    if_match: p.if_match,
                }
            } else {
                CapabilityVerdict::NonCas
            }
        }
        Ok(Err(reason)) => CapabilityVerdict::Unknown {
            reason: reason.to_string(),
        },
    }
}

/// The single source of the non-CAS enforcement message — used verbatim by the
/// boot FATAL and the hot-apply refusal so the two can never drift.
pub fn noncas_enforcement_message(name: &str, buckets: &[String]) -> String {
    format!(
        "backend '{name}' does not support conditional writes, but client-writable \
         bucket(s) {buckets:?} route to it and multi-instance mode is active. \
         Concurrent writes from two instances can corrupt delta references. \
         Fix: move these buckets to a backend that supports conditional writes, or \
         mark each as configured for replication targets only. — see {CAPABILITY_DOC_URL}"
    )
}

/// Hot-apply pre-commit gate: refuse a config transition that would route a
/// client-writable bucket onto a known- (or freshly-probed-) non-CAS backend
/// while multi-instance. Runs AFTER `Config::check()` passes and BEFORE the
/// transition commits; only active when a coordination bucket is configured.
///
/// Verdicts are trusted only for the exact backend DEFINITION they were
/// probed against (fingerprint match); a redefined backend re-probes once,
/// bounded by `PROBE_TIMEOUT`. A fingerprint-matched `Unknown` IS re-probed
/// here — it's a non-verdict whose cause (missing bucket, network) may since
/// be fixed; staying sticky would leave the corruption window open until a
/// restart. Accepted cost: while a client-writable backend is unreachable AND
/// multi-instance is on, every apply pays up to 15s under the config lock.
pub async fn hot_apply_capability_gate(
    new_config: &crate::config::Config,
    cache: &BackendCapabilityCache,
) -> Result<(), String> {
    if new_config
        .config_sync_bucket
        .as_deref()
        .is_none_or(|b| b.is_empty())
    {
        return Ok(());
    }
    let forced = forced_noncas_backends();
    let mut groups = client_writable_s3_backends(new_config);
    // The default backend's group needs a bucket listing: only when its
    // current definition has no real verdict yet.
    let default_name = new_config.default_backend_name();
    let default_known = new_config
        .backend_by_name(&default_name)
        .and_then(|def| cache.get(&default_name, def))
        .is_some_and(|v| !matches!(v, CapabilityVerdict::Unknown { .. }));
    if !default_known {
        if let Some((name, g)) = default_backend_group(new_config, &groups, &forced).await {
            groups.insert(name, g);
        }
    }
    for (name, group) in groups {
        let verdict = match cache.get(&name, &group.backend) {
            // A cached Unknown is NOT a verdict — the blocker (missing bucket,
            // network) may since be fixed; re-probe instead of staying sticky.
            Some(v) if !matches!(v, CapabilityVerdict::Unknown { .. }) => v,
            _ => {
                let v = establish_backend_verdict(&name, &group, &forced).await;
                cache.set(&name, &group.backend, v.clone());
                v
            }
        };
        if verdict == CapabilityVerdict::NonCas {
            return Err(format!(
                "config refused: {}",
                noncas_enforcement_message(&name, &group.buckets)
            ));
        }
    }
    Ok(())
}

/// Migrate-time capability gate: the migrate flip persists routing through
/// `ConfigMutator` WITHOUT passing the hot-apply gate, so a client-writable
/// bucket must be checked here before it is flipped onto a non-CAS backend
/// under multi-instance — otherwise the NEXT boot's startup gate exit(1)s on
/// the persisted config (a crash loop needing manual YAML surgery).
///
/// Probes against a bucket that already EXISTS on the target backend when one
/// is routed there (the migrated bucket itself doesn't exist on the target
/// yet); with nothing to probe, an `Unknown` verdict warn-allows, consistent
/// with the startup gate's fail-open stance.
pub async fn migrate_target_capability_gate(
    config: &crate::config::Config,
    cache: &BackendCapabilityCache,
    bucket: &str,
    target_backend: &str,
) -> Result<(), String> {
    if config
        .config_sync_bucket
        .as_deref()
        .is_none_or(|b| b.is_empty())
    {
        return Ok(()); // single-instance: in-process lock is sufficient
    }
    let policy = config.buckets.get(bucket);
    if policy.is_some_and(|p| p.replication_target_only) {
        return Ok(()); // no client writers → any backend is safe
    }
    let Some(target_def) = config.backend_by_name(target_backend) else {
        return Ok(()); // unknown backend: the caller already refused it
    };
    if matches!(target_def, crate::config::BackendConfig::Filesystem { .. }) {
        return Ok(());
    }
    // Prefer probing a bucket already on the target (it exists there); fall
    // back to the migrated bucket's real name.
    let probe_bucket = config
        .buckets
        .iter()
        .find(|(b, _)| {
            config
                .effective_backend_for_bucket(b)
                .is_some_and(|(n, _)| n == target_backend)
        })
        .map(|(b, _)| config.real_bucket_name(b))
        .unwrap_or_else(|| config.real_bucket_name(bucket));
    let group = ClientWritableGroup {
        backend: target_def.clone(),
        buckets: vec![bucket.to_string()],
        probe_bucket,
    };
    let verdict = match cache.get(target_backend, &group.backend) {
        Some(v) if !matches!(v, CapabilityVerdict::Unknown { .. }) => v,
        _ => {
            let v =
                establish_backend_verdict(target_backend, &group, &forced_noncas_backends()).await;
            cache.set(target_backend, &group.backend, v.clone());
            v
        }
    };
    match verdict {
        CapabilityVerdict::NonCas => Err(format!(
            "migrate refused: {}",
            noncas_enforcement_message(target_backend, &group.buckets)
        )),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s3_backend(endpoint: &str) -> crate::config::BackendConfig {
        crate::config::BackendConfig::S3 {
            session_token: None,
            endpoint: Some(endpoint.to_string()),
            region: "us-east-1".into(),
            force_path_style: true,
            access_key_id: Some("k".into()),
            secret_access_key: Some("s".into()),
            allow_local: true,
        }
    }

    #[test]
    fn cache_set_get_snapshot() {
        let cache = BackendCapabilityCache::default();
        let b2 = s3_backend("https://b2.example");
        assert_eq!(cache.get("b2", &b2), None);
        cache.set("b2", &b2, CapabilityVerdict::NonCas);
        cache.set(
            "hetzner",
            &s3_backend("https://hetzner.example"),
            CapabilityVerdict::CasVerified {
                via: VerifiedVia::Probe,
                if_match: Default::default(),
            },
        );
        assert_eq!(cache.get("b2", &b2), Some(CapabilityVerdict::NonCas));
    }

    /// C6: a probe that its deadline cuts short still removes EVERY
    /// version of its key (B2 buckets are versioned: one plain DELETE
    /// leaves noncurrent versions).
    #[tokio::test]
    async fn a_probe_cut_by_its_deadline_removes_every_version_of_its_key() {
        use std::sync::atomic::Ordering;
        let (endpoint, counts) = super::super::cas_probe::b2_like::start(true).await;
        let group = ClientWritableGroup {
            backend: s3_backend(&endpoint),
            buckets: Vec::new(),
            probe_bucket: "coord".into(),
        };
        let v = establish_backend_verdict_within(
            "b2",
            &group,
            &Default::default(),
            std::time::Duration::from_millis(1),
        )
        .await;
        assert!(matches!(v, CapabilityVerdict::Unknown { .. }), "{v:?}");
        // The cleanup runs on a task of its own (the probe's drop guard).
        for _ in 0..100 {
            if counts.version_lists.load(Ordering::SeqCst) >= 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(
            counts.version_lists.load(Ordering::SeqCst) >= 1,
            "the cleanup lists and deletes the key's versions"
        );
    }

    /// The probe is ONE deadline, both stages: the callers (the boot gate,
    /// the hot-apply gate, the atomic-writes check of an encrypted bucket,
    /// which answers 503 meanwhile) rely on a probe that ends within its
    /// budget. Stage 1 takes about 1.3 s here (a PUT takes 250 ms), stage 2
    /// would take 10 s: with a fresh budget for it the call took 3.3 s.
    #[tokio::test]
    async fn the_whole_probe_ends_within_its_budget() {
        let (endpoint, fake) = crate::storage::fake_s3::start().await;
        fake.set_delay_ms("PUT", 250);
        fake.set_delay_ms("POST", 10_000);
        let group = ClientWritableGroup {
            backend: s3_backend(&endpoint),
            buckets: Vec::new(),
            probe_bucket: "b".into(),
        };
        let started = std::time::Instant::now();
        let v = establish_backend_verdict_within(
            "fake",
            &group,
            &Default::default(),
            std::time::Duration::from_secs(2),
        )
        .await;
        let took = started.elapsed();
        assert!(
            matches!(v, CapabilityVerdict::CasVerified { .. }),
            "stage 1 finished: {v:?}"
        );
        assert!(
            took < std::time::Duration::from_millis(2_600),
            "the probe took {took:?} of a 2 s budget"
        );
        // Stage 2 was cut: not proven.
        let profile = super::super::if_match::lookup_for(&group.backend).unwrap();
        assert_eq!(profile.conditional_complete, None);
    }

    /// `Unknown` never replaces a proven verdict of the same definition.
    #[test]
    fn unknown_never_replaces_a_proven_verdict() {
        let cache = BackendCapabilityCache::default();
        let d = s3_backend("https://b2.example");
        let unknown = CapabilityVerdict::Unknown {
            reason: "timeout".into(),
        };
        for proven in [
            CapabilityVerdict::NonCas,
            CapabilityVerdict::CasVerified {
                via: VerifiedVia::Probe,
                if_match: Default::default(),
            },
        ] {
            cache.set("b2", &d, proven.clone());
            assert_eq!(cache.set("b2", &d, unknown.clone()), proven);
            assert_eq!(cache.get("b2", &d), Some(proven));
        }
        let fresh = s3_backend("https://new.example");
        assert_eq!(
            cache.set("b2", &fresh, unknown.clone()),
            unknown,
            "no proof yet"
        );
    }

    /// The storage wrappers learn which plan backends lack proven atomic
    /// writes, and follow each verdict change of the running definition.
    #[test]
    fn the_storage_gate_follows_the_running_verdicts() {
        let cfg = crate::config::Config::from_yaml_str(
            "storage:\n  backends:\n    - name: b2\n      type: s3\n      \
             endpoint: \"https://b2.example\"\n      region: us-east-1\n      \
             access_key_id: k\n      secret_access_key: s\n      encryption: { mode: \
             aes256-gcm-proxy, key: \"1111111111111111111111111111111111111111111111111111111111111111\", \
             key_id: a }\n",
        )
        .unwrap();
        let cache = BackendCapabilityCache::default();
        let gate = std::sync::Arc::new(crate::storage::AtomicityGate::default());
        cache.set_encrypted_plan(&cfg);
        cache.attach_storage_gate(gate.clone());
        assert!(gate.is_unproven("b2"), "no verdict yet");
        let def = cfg.backend_by_name("b2").unwrap();
        cache.set(
            "b2",
            def,
            CapabilityVerdict::CasVerified {
                via: VerifiedVia::Probe,
                if_match: Default::default(),
            },
        );
        assert!(!gate.is_unproven("b2"));
        cache.set("b2", def, CapabilityVerdict::NonCas);
        assert!(gate.is_unproven("b2"));
    }

    /// Probes of many candidate definitions never evict the running one.
    #[test]
    fn candidate_definitions_never_evict_the_running_verdict() {
        let cfg = crate::config::Config::from_yaml_str(
            "storage:\n  backends:\n    - name: b2\n      type: s3\n      \
             endpoint: \"https://b2.example\"\n      region: us-east-1\n      \
             access_key_id: k\n      secret_access_key: s\n",
        )
        .unwrap();
        let cache = BackendCapabilityCache::default();
        cache.set_encrypted_plan(&cfg);
        let running = cfg.backend_by_name("b2").unwrap().clone();
        let cas = CapabilityVerdict::CasVerified {
            via: VerifiedVia::Probe,
            if_match: Default::default(),
        };
        cache.set("b2", &running, cas.clone());
        for i in 0..10 {
            cache.set(
                "b2",
                &s3_backend(&format!("https://candidate-{i}.example")),
                CapabilityVerdict::NonCas,
            );
        }
        assert_eq!(cache.get("b2", &running), Some(cas));
        assert!(cache.verdicts.read()["b2"].len() <= DEFINITIONS_PER_NAME);
    }

    #[test]
    fn cache_misses_on_redefined_backend() {
        // A hot-apply that swaps the endpoint under the same NAME must miss
        // the cache: a stale CasVerified must not vouch for the new endpoint,
        // and a stale NonCas must not lock out a fixed one.
        let cache = BackendCapabilityCache::default();
        let old_def = s3_backend("https://hetzner.example");
        let new_def = s3_backend("https://b2.example");
        cache.set(
            "remote",
            &old_def,
            CapabilityVerdict::CasVerified {
                via: VerifiedVia::Probe,
                if_match: Default::default(),
            },
        );
        assert_eq!(cache.get("remote", &new_def), None, "redefinition → miss");
        assert!(cache.get("remote", &old_def).is_some(), "same def → hit");
    }

    #[test]
    fn projection_skips_marked_and_filesystem_buckets() {
        let cfg = crate::config::Config::from_yaml_str(
            r#"
storage:
  backends:
    - name: remote
      type: s3
      endpoint: "http://127.0.0.1:1"
      region: us-east-1
      access_key_id: x
      secret_access_key: y
    - name: localdisk
      type: filesystem
      path: /tmp/x
  buckets:
    writable: { backend: remote, alias: real-writable }
    also-writable: { backend: remote }
    mirror: { backend: remote, replication_target_only: true }
    on-default: {}
    on-disk: { backend: localdisk }
"#,
        )
        .expect("fixture parses");
        let groups = client_writable_s3_backends(&cfg);
        assert_eq!(
            groups.len(),
            1,
            "only the named S3 backend group: {groups:?}"
        );
        let g = &groups["remote"];
        let mut buckets = g.buckets.clone();
        buckets.sort();
        // `on-default` routes to `remote` (the first named backend).
        assert_eq!(buckets, vec!["also-writable", "on-default", "writable"]);
        // Probe bucket is the alias-resolved real name of a routed bucket
        // (BTreeMap iteration → first entry, "also-writable", no alias).
        assert!(
            g.probe_bucket == "also-writable" || g.probe_bucket == "real-writable",
            "probe bucket must be a real routed bucket, got {}",
            g.probe_bucket
        );
    }

    /// An unrouted bucket lands on the NAMED default backend. The gate once
    /// skipped it as "the coordination gate covers the default", which is
    /// true only for the legacy singleton.
    #[test]
    fn projection_includes_unrouted_buckets_on_the_named_default() {
        let cfg = crate::config::Config::from_yaml_str(
            r#"
storage:
  backends:
    - name: remote
      type: s3
      endpoint: "http://127.0.0.1:1"
      region: us-east-1
      access_key_id: x
      secret_access_key: y
  buckets:
    on-default: {}
"#,
        )
        .expect("fixture parses");
        let groups = client_writable_s3_backends(&cfg);
        assert_eq!(groups["remote"].buckets, vec!["on-default"]);
        assert_eq!(groups["remote"].probe_bucket, "on-default");
    }

    #[test]
    fn default_probe_bucket_skips_policy_buckets() {
        let cfg = crate::config::Config::from_yaml_str("storage:\n  buckets:\n    releases: {}\n")
            .unwrap();
        let listed = vec!["releases".to_string(), "downloads".to_string()];
        assert_eq!(
            pick_default_probe_bucket(&cfg, &listed).as_deref(),
            Some("downloads")
        );
        assert_eq!(pick_default_probe_bucket(&cfg, &listed[..1]), None);
    }

    #[test]
    fn verdict_serializes_kebab_case_for_the_admin_api() {
        let v = serde_json::to_value(CapabilityVerdict::CasVerified {
            via: VerifiedVia::Probe,
            if_match: Default::default(),
        })
        .unwrap();
        assert_eq!(v["verdict"], "cas-verified");
        assert_eq!(v["via"], "probe");
        assert_eq!(v["if_match"], "quoted");
    }

    /// A verdict without the dialect (from before it existed) reads as
    /// the quoted form; the unquoted one round-trips.
    #[test]
    fn a_verdict_without_a_dialect_is_quoted() {
        let old: CapabilityVerdict =
            serde_json::from_str(r#"{"verdict":"cas-verified","via":"probe"}"#).unwrap();
        assert_eq!(
            old,
            CapabilityVerdict::CasVerified {
                via: VerifiedVia::Probe,
                if_match: super::super::if_match::IfMatchDialect::Quoted,
            }
        );
        let rgw = CapabilityVerdict::CasVerified {
            via: VerifiedVia::Probe,
            if_match: super::super::if_match::IfMatchDialect::Unquoted,
        };
        let json = serde_json::to_string(&rgw).unwrap();
        assert!(json.contains(r#""if_match":"unquoted""#), "{json}");
        assert_eq!(
            serde_json::from_str::<CapabilityVerdict>(&json).unwrap(),
            rgw
        );
    }
}
