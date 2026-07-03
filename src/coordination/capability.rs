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
    /// A fresh witness object from a prior boot let us skip the probe.
    Witness,
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
#[derive(Debug, Default)]
pub struct BackendCapabilityCache {
    verdicts: parking_lot::RwLock<HashMap<String, CapabilityVerdict>>,
}

impl BackendCapabilityCache {
    pub fn set(&self, backend: &str, verdict: CapabilityVerdict) {
        self.verdicts.write().insert(backend.to_string(), verdict);
    }

    pub fn get(&self, backend: &str) -> Option<CapabilityVerdict> {
        self.verdicts.read().get(backend).cloned()
    }

    /// Snapshot for the admin backends API.
    pub fn snapshot(&self) -> HashMap<String, CapabilityVerdict> {
        self.verdicts.read().clone()
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
    std::env::var("DGP_TEST_FORCE_NONCAS_BACKEND")
        .map(|v| {
            v.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// Establish a CAS verdict for one backend group: forced-hook → witness
/// fast-path → live probe (via [`crate::config_db_sync::validate_cas_bucket`]).
/// Shared by the startup gate and the hot-apply pre-commit gate; the CALLER
/// decides what each verdict means (exit(1) vs reject-apply vs warn).
pub async fn establish_backend_verdict(
    name: &str,
    group: &ClientWritableGroup,
    forced_noncas: &std::collections::BTreeSet<String>,
) -> CapabilityVerdict {
    use crate::config_db_sync::{
        validate_cas_bucket, CasValidationFailure, CoordinationValidation,
        BACKEND_CAPABILITY_WITNESS_KEY,
    };
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
    match validate_cas_bucket(&client, &group.probe_bucket, BACKEND_CAPABILITY_WITNESS_KEY).await {
        Ok(CoordinationValidation::Probed) => CapabilityVerdict::CasVerified {
            via: VerifiedVia::Probe,
        },
        Ok(CoordinationValidation::CachedWitness { .. }) => CapabilityVerdict::CasVerified {
            via: VerifiedVia::Witness,
        },
        Err(CasValidationFailure::NonCas) => CapabilityVerdict::NonCas,
        Err(CasValidationFailure::Indeterminate(reason)) => CapabilityVerdict::Unknown { reason },
    }
}

/// Hot-apply pre-commit gate: refuse a config transition that would route a
/// client-writable bucket onto a known- (or freshly-probed-) non-CAS backend
/// while multi-instance. Runs AFTER `Config::check()` passes and BEFORE the
/// transition commits; only active when a coordination bucket is configured.
/// `Unknown`/unprobed backends are probed inline with a bounded timeout so a
/// brand-new backend added via the GUI still gets a verdict before commit.
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
        let verdict = match cache.get(&name) {
            Some(v @ CapabilityVerdict::CasVerified { .. })
            | Some(v @ CapabilityVerdict::NonCas) => v,
            // Unknown or never probed: probe inline, bounded, and record.
            _ => {
                let v = match tokio::time::timeout(
                    std::time::Duration::from_secs(15),
                    establish_backend_verdict(&name, &group, &forced),
                )
                .await
                {
                    Ok(v) => v,
                    Err(_) => CapabilityVerdict::Unknown {
                        reason: "capability probe timed out (15s)".to_string(),
                    },
                };
                cache.set(&name, v.clone());
                v
            }
        };
        if verdict == CapabilityVerdict::NonCas {
            return Err(format!(
                "config refused: it would route client-writable bucket(s) {:?} to backend \
                 '{name}', which does NOT support conditional writes, while multi-instance \
                 mode is active (config_sync_bucket is set). Concurrent writes from two \
                 instances can corrupt delta references. Fix: move these buckets to a \
                 CAS-capable backend, or mark each as replication_target_only. — see \
                 {CAPABILITY_DOC_URL}",
                group.buckets
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_set_get_snapshot() {
        let cache = BackendCapabilityCache::default();
        assert_eq!(cache.get("b2"), None);
        cache.set("b2", CapabilityVerdict::NonCas);
        cache.set(
            "hetzner",
            CapabilityVerdict::CasVerified {
                via: VerifiedVia::Probe,
            },
        );
        assert_eq!(cache.get("b2"), Some(CapabilityVerdict::NonCas));
        assert_eq!(cache.snapshot().len(), 2);
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
            via: VerifiedVia::Witness,
        })
        .unwrap();
        assert_eq!(v["verdict"], "cas-verified");
        assert_eq!(v["via"], "witness");
    }
}
