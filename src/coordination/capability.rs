// SPDX-License-Identifier: GPL-3.0-only

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum VerifiedVia {
    /// Live two-step `If-None-Match:*` probe ran this boot.
    Probe,
}

/// The capability verdict for one backend.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "verdict", rename_all = "kebab-case")]
pub enum CapabilityVerdict {
    /// Conditional writes enforced (412 on a violated `If-None-Match:*`).
    CasVerified { via: VerifiedVia },
    /// DEFINITIVE: the backend silently ignored the condition. Unsafe for
    /// multi-instance client-writable buckets and for coordination.
    NonCas,
    /// The probe could not run (network error, missing bucket). Not a
    /// verdict — surfaced loudly but never treated as safe OR unsafe.
    Unknown { reason: String },
}

/// Thread-safe backend-name → verdict map, `Arc`-shared into `AppState`.
///
/// Entries carry a FINGERPRINT of the `BackendConfig` they were probed
/// against: a hot-apply that redefines a backend (new endpoint, rotated
/// credentials) under the same name misses the cache and re-probes — a stale
/// `CasVerified` must never vouch for a swapped-out endpoint, and a stale
/// `NonCas` must never lock out a fixed one.
#[derive(Debug, Default)]
pub struct BackendCapabilityCache {
    verdicts: parking_lot::RwLock<HashMap<String, (String, CapabilityVerdict)>>,
}

/// In-memory identity of a backend definition (includes credentials — never
/// persisted or exposed; a credential rotation deliberately re-probes).
fn fingerprint(config: &crate::config::BackendConfig) -> String {
    serde_json::to_string(config).unwrap_or_default()
}

impl BackendCapabilityCache {
    pub fn set(
        &self,
        backend: &str,
        config: &crate::config::BackendConfig,
        verdict: CapabilityVerdict,
    ) {
        self.verdicts
            .write()
            .insert(backend.to_string(), (fingerprint(config), verdict));
    }

    /// Verdict for this backend NAME, only if it was established against this
    /// exact backend DEFINITION. `None` = never probed or definition changed.
    pub fn get(
        &self,
        backend: &str,
        config: &crate::config::BackendConfig,
    ) -> Option<CapabilityVerdict> {
        self.verdicts
            .read()
            .get(backend)
            .filter(|(fp, _)| *fp == fingerprint(config))
            .map(|(_, v)| v.clone())
    }

    /// Snapshot for the admin backends API (name → verdict).
    pub fn snapshot(&self) -> HashMap<String, CapabilityVerdict> {
        self.verdicts
            .read()
            .iter()
            .map(|(k, (_, v))| (k.clone(), v.clone()))
            .collect()
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

/// Pure projection: which NAMED S3 backends host at least one client-writable
/// routed bucket, and therefore need a CAS verdict under multi-instance.
///
/// Skipped by design: `replication_target_only` buckets (no client writers),
/// filesystem backends (per-node local, single-writer by nature), and the
/// DEFAULT backend — it hosts the coordination bucket (`ConfigDbSync` builds
/// its client from `config.backend`), so the coordination gate already
/// crash-validates it. Compression policy is deliberately IGNORED: it is
/// hot-flippable, so a `compression: false` exemption would be unsound.
pub fn client_writable_s3_backends(
    config: &crate::config::Config,
) -> std::collections::BTreeMap<String, ClientWritableGroup> {
    let mut groups: std::collections::BTreeMap<String, ClientWritableGroup> = Default::default();
    for (bucket, policy) in &config.buckets {
        if policy.replication_target_only {
            continue;
        }
        let Some(backend_name) = policy.backend.as_deref() else {
            continue; // default backend: covered by the coordination gate
        };
        let Some(named) = config.backends.iter().find(|b| b.name == backend_name) else {
            continue; // unknown backend: already warned by Config::check()
        };
        if matches!(
            named.backend,
            crate::config::BackendConfig::Filesystem { .. }
        ) {
            continue;
        }
        let real = policy.alias.clone().unwrap_or_else(|| bucket.clone());
        groups
            .entry(backend_name.to_string())
            .or_insert_with(|| ClientWritableGroup {
                backend: named.backend.clone(),
                buckets: Vec::new(),
                probe_bucket: real,
            })
            .buckets
            .push(bucket.clone());
    }
    groups
}

/// Test seam: backends listed in `DGP_TEST_FORCE_NONCAS_BACKEND` (comma-
/// separated) get a forced NonCas verdict without probing — the only way to
/// exercise the fail-fast path against a MinIO-only test harness.
pub fn forced_noncas_backends() -> std::collections::BTreeSet<String> {
    crate::config::env_parse::<String>("DGP_TEST_FORCE_NONCAS_BACKEND")
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// How long a backend capability probe may run before we give up with an
/// `Unknown` verdict (bounds both the startup gate and the hot-apply gate).
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// Establish a CAS verdict for one backend group: forced-hook → live probe.
/// Shared by the startup gate and the hot-apply pre-commit gate; the CALLER
/// decides what each verdict means (exit(1) vs reject-apply vs warn).
///
/// NO witness object in the data bucket: a witness there is client-visible,
/// blocks DeleteBucket with a ghost key, and lands unencrypted on encrypting
/// backends. The probe is 3 requests once per (backend definition, boot) —
/// the in-memory fingerprint cache absorbs repeats within a process.
pub async fn establish_backend_verdict(
    name: &str,
    group: &ClientWritableGroup,
    forced_noncas: &std::collections::BTreeSet<String>,
) -> CapabilityVerdict {
    if forced_noncas.contains(name) {
        return CapabilityVerdict::NonCas;
    }
    let client = match crate::config_db_sync::ConfigDbSync::build_client(&group.backend).await {
        Ok(c) => c,
        Err(e) => {
            return CapabilityVerdict::Unknown {
                reason: format!("client build failed: {e}"),
            }
        }
    };
    let probe_key = format!(".deltaglider/_cwprobe/{}", uuid::Uuid::new_v4());
    match tokio::time::timeout(
        PROBE_TIMEOUT,
        crate::config_db_sync::probe_conditional_write(&client, &group.probe_bucket, &probe_key),
    )
    .await
    {
        Err(_) => {
            // The timed-out future was cancelled mid-flight — possibly between
            // its PUT and its cleanup delete. Sweep the probe object so a
            // client-visible plaintext key is never leaked into a data bucket.
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                client
                    .delete_object()
                    .bucket(&group.probe_bucket)
                    .key(&probe_key)
                    .send(),
            )
            .await;
            CapabilityVerdict::Unknown {
                reason: format!("capability probe timed out ({}s)", PROBE_TIMEOUT.as_secs()),
            }
        }
        Ok(Ok(true)) => CapabilityVerdict::CasVerified {
            via: VerifiedVia::Probe,
        },
        Ok(Ok(false)) => CapabilityVerdict::NonCas,
        Ok(Err(reason)) => CapabilityVerdict::Unknown { reason },
    }
}

/// The single source of the non-CAS enforcement message — used verbatim by the
/// boot FATAL and the hot-apply refusal so the two can never drift.
pub fn noncas_enforcement_message(name: &str, buckets: &[String]) -> String {
    format!(
        "backend '{name}' does not support conditional writes, but client-writable \
         bucket(s) {buckets:?} route to it and multi-instance mode is active \
         (config_sync_bucket is set). Concurrent writes from two instances can corrupt \
         delta references. Fix: move these buckets to a CAS-capable backend, or mark \
         each as replication_target_only. — see {CAPABILITY_DOC_URL}"
    )
}

/// Hot-apply pre-commit gate: refuse a config transition that would route a
/// client-writable bucket onto a known- (or freshly-probed-) non-CAS backend
/// while multi-instance. Runs AFTER `Config::check()` passes and BEFORE the
/// transition commits; only active when a coordination bucket is configured.
///
/// Verdicts are trusted only for the exact backend DEFINITION they were
/// probed against (fingerprint match); a redefined backend re-probes once,
/// bounded by [`PROBE_TIMEOUT`]. A fingerprint-matched `Unknown` IS re-probed
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
    for (name, group) in client_writable_s3_backends(new_config) {
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
    let Some(named) = config.backends.iter().find(|b| b.name == target_backend) else {
        return Ok(()); // unknown/default backend: validated elsewhere
    };
    if matches!(
        named.backend,
        crate::config::BackendConfig::Filesystem { .. }
    ) {
        return Ok(());
    }
    // Prefer probing a bucket already routed to the target (it exists there);
    // fall back to the migrated bucket's real name.
    let probe_bucket = config
        .buckets
        .iter()
        .find(|(_, p)| p.backend.as_deref() == Some(target_backend))
        .map(|(b, p)| p.alias.clone().unwrap_or_else(|| b.clone()))
        .unwrap_or_else(|| {
            policy
                .and_then(|p| p.alias.clone())
                .unwrap_or_else(|| bucket.to_string())
        });
    let group = ClientWritableGroup {
        backend: named.backend.clone(),
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
            },
        );
        assert_eq!(cache.get("b2", &b2), Some(CapabilityVerdict::NonCas));
        assert_eq!(cache.snapshot().len(), 2);
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
            },
        );
        assert_eq!(cache.get("remote", &new_def), None, "redefinition → miss");
        assert!(cache.get("remote", &old_def).is_some(), "same def → hit");
    }

    #[test]
    fn projection_skips_marked_default_backend_and_filesystem_buckets() {
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
        assert_eq!(buckets, vec!["also-writable", "writable"]);
        // Probe bucket is the alias-resolved real name of a routed bucket
        // (BTreeMap iteration → first entry, "also-writable", no alias).
        assert!(
            g.probe_bucket == "also-writable" || g.probe_bucket == "real-writable",
            "probe bucket must be a real routed bucket, got {}",
            g.probe_bucket
        );
    }

    #[test]
    fn verdict_serializes_kebab_case_for_the_admin_api() {
        let v = serde_json::to_value(CapabilityVerdict::CasVerified {
            via: VerifiedVia::Probe,
        })
        .unwrap();
        assert_eq!(v["verdict"], "cas-verified");
        assert_eq!(v["via"], "probe");
    }
}
