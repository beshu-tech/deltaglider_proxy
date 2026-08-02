// SPDX-License-Identifier: GPL-3.0-only

//! Pure multi-replica preflight: turns the README's "requirements for replicas > 1"
//! into enforced invariants. Pure function over the spec + observed Secret keys, so
//! the whole truth table is unit-tested without a cluster.

use crate::crd::DeltaGliderProxy;

/// What the reconciler observed about `spec.envFromSecret` in the cluster.
#[derive(Debug, Clone, PartialEq)]
pub enum EnvSecret {
    /// spec.envFromSecret not set.
    NotNamed,
    /// Named, but no such Secret exists in the namespace.
    Missing(String),
    /// Named and found; these are its key names (values never leave the cluster).
    Keys(Vec<String>),
    /// The read failed transiently — the truth is unknown this cycle.
    Unknown,
}

/// The replica count to actually deploy. `may_scale_up` is true only when preflight
/// ran and found no problems. Otherwise we HOLD the current fleet size (killing
/// healthy pods remaps the hash ring) and only block growth past it.
pub fn effective_replicas(spec_replicas: i32, may_scale_up: bool, current: Option<i32>) -> i32 {
    if may_scale_up {
        spec_replicas
    } else {
        spec_replicas.min(current.unwrap_or(0).max(1))
    }
}

/// Status phase + message, pure so the truth table is unit-testable.
pub fn phase_and_message(
    problems: &[String],
    ready: i32,
    want: i32,
    router_ready: i32,
) -> (&'static str, String) {
    if !problems.is_empty() {
        (
            "Degraded",
            format!(
                "scale-up blocked — spec violates the multi-replica contract: {}",
                problems.join(" | ")
            ),
        )
    } else if ready >= want && router_ready >= 1 {
        (
            "Ready",
            format!("{ready}/{want} proxy pods, {router_ready} router pods ready"),
        )
    } else {
        (
            "Progressing",
            format!("{ready}/{want} proxy pods, {router_ready} router pods ready"),
        )
    }
}

/// Returns the list of problems that make `replicas > 1` unsafe. Empty = go.
pub fn multi_replica_problems(cr: &DeltaGliderProxy, env_secret: &EnvSecret) -> Vec<String> {
    if cr.replicas() <= 1 {
        return vec![];
    }
    let mut problems = vec![];

    let yaml = match &cr.spec.config_yaml {
        None => {
            problems.push(
                "spec.configYaml is required for replicas > 1 (it must configure an S3 \
                 storage backend and advanced.config_sync_bucket)"
                    .to_string(),
            );
            None
        }
        Some(raw) => match serde_yaml::from_str::<serde_yaml::Value>(raw) {
            Ok(v) => Some(v),
            Err(e) => {
                problems.push(format!("spec.configYaml is not valid YAML: {e}"));
                None
            }
        },
    };

    // Unknown never reaches these checks — the caller skips preflight on a transient
    // read failure, so a control-plane blip can neither invent problems nor assert health.
    let secret_has = |key: &str| match env_secret {
        EnvSecret::Keys(keys) => keys.iter().any(|k| k == key),
        _ => false,
    };

    if let Some(yaml) = &yaml {
        let sync_in_yaml = yaml
            .get("advanced")
            .and_then(|a| a.get("config_sync_bucket"))
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.is_empty());
        if !sync_in_yaml && !secret_has("DGP_CONFIG_SYNC_BUCKET") {
            problems.push(
                "no config sync bucket: set advanced.config_sync_bucket in spec.configYaml \
                 (or DGP_CONFIG_SYNC_BUCKET in the env Secret). Without it the pods cannot \
                 share IAM state or elect replication leaders"
                    .to_string(),
            );
        }
        if uses_filesystem_backend(yaml) {
            problems.push(
                "the filesystem storage backend is per-pod local disk: with more than one \
                 pod, each pod would see different data. Use an S3 backend for replicas > 1"
                    .to_string(),
            );
        }
    }

    match env_secret {
        EnvSecret::Missing(name) => {
            problems.push(format!(
                "spec.envFromSecret names Secret \"{name}\" but it does not exist in this \
                 namespace"
            ));
        }
        EnvSecret::NotNamed if !cr.bootstrap_auto_generate() => {
            problems.push(
                "no bootstrap password hash: every pod must share DGP_BOOTSTRAP_PASSWORD_HASH \
                 (it encrypts the shared IAM database). Set spec.envFromSecret to a Secret \
                 carrying it, or set spec.bootstrapPassword.autoGenerate: true"
                    .to_string(),
            );
        }
        EnvSecret::Keys(_)
            if !secret_has("DGP_BOOTSTRAP_PASSWORD_HASH") && !cr.bootstrap_auto_generate() =>
        {
            problems.push(
                "the env Secret has no DGP_BOOTSTRAP_PASSWORD_HASH key: every pod must share \
                 the same hash (it encrypts the shared IAM database). Add the key, or set \
                 spec.bootstrapPassword.autoGenerate: true"
                    .to_string(),
            );
        }
        _ => {}
    }

    problems
}

/// True when the config YAML declares any filesystem storage backend — the root
/// `storage.filesystem` shorthand, `storage.backend.type: filesystem`, or any entry
/// of `storage.backends` with `type: filesystem`.
fn uses_filesystem_backend(yaml: &serde_yaml::Value) -> bool {
    let Some(storage) = yaml.get("storage") else {
        return false;
    };
    if storage.get("filesystem").is_some() {
        return true;
    }
    let is_fs =
        |v: &serde_yaml::Value| v.get("type").and_then(|t| t.as_str()) == Some("filesystem");
    if storage.get("backend").is_some_and(is_fs) {
        return true;
    }
    storage
        .get("backends")
        .and_then(|b| b.as_sequence())
        .is_some_and(|seq| seq.iter().any(is_fs))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::{BootstrapPasswordSpec, DeltaGliderProxySpec};

    fn cr(replicas: i32, config_yaml: Option<&str>, auto_gen: bool) -> DeltaGliderProxy {
        DeltaGliderProxy::new(
            "dgp",
            DeltaGliderProxySpec {
                replicas: Some(replicas),
                image: None,
                config_yaml: config_yaml.map(String::from),
                env_from_secret: Some("dgp-env".into()),
                storage: None,
                router: None,
                service: None,
                resources: None,
                bootstrap_password: auto_gen.then_some(BootstrapPasswordSpec {
                    auto_generate: Some(true),
                }),
            },
        )
    }

    const GOOD_HA_YAML: &str = "storage:\n  s3: https://s3.example.com\n  region: r1\nadvanced:\n  config_sync_bucket: dgp-iam-sync\n";
    fn keys(ks: &[&str]) -> EnvSecret {
        EnvSecret::Keys(ks.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn single_replica_never_blocks() {
        let c = cr(1, None, false);
        assert!(multi_replica_problems(&c, &EnvSecret::NotNamed).is_empty());
    }

    #[test]
    fn good_ha_config_passes() {
        let c = cr(3, Some(GOOD_HA_YAML), false);
        assert!(multi_replica_problems(&c, &keys(&["DGP_BOOTSTRAP_PASSWORD_HASH"])).is_empty());
    }

    #[test]
    fn missing_sync_bucket_blocks_unless_in_secret() {
        let yaml = "storage:\n  s3: https://s3.example.com\n";
        let c = cr(3, Some(yaml), false);
        let hash_only = keys(&["DGP_BOOTSTRAP_PASSWORD_HASH"]);
        assert!(multi_replica_problems(&c, &hash_only)
            .iter()
            .any(|p| p.contains("config sync bucket")));
        let both = keys(&["DGP_BOOTSTRAP_PASSWORD_HASH", "DGP_CONFIG_SYNC_BUCKET"]);
        assert!(multi_replica_problems(&c, &both).is_empty());
    }

    #[test]
    fn filesystem_backends_block_in_every_shape() {
        for yaml in [
            "storage:\n  filesystem: /data/storage\nadvanced:\n  config_sync_bucket: b\n",
            "storage:\n  backend:\n    type: filesystem\n    path: /d\nadvanced:\n  config_sync_bucket: b\n",
            "storage:\n  backends:\n  - name: x\n    type: filesystem\n    path: /d\nadvanced:\n  config_sync_bucket: b\n",
        ] {
            let c = cr(2, Some(yaml), false);
            assert!(
                multi_replica_problems(&c, &keys(&["DGP_BOOTSTRAP_PASSWORD_HASH"]))
                    .iter()
                    .any(|p| p.contains("filesystem")),
                "should block: {yaml}"
            );
        }
    }

    #[test]
    fn missing_bootstrap_hash_blocks_unless_auto_generated() {
        let c = cr(3, Some(GOOD_HA_YAML), false);
        assert!(multi_replica_problems(&c, &keys(&["DGP_ACCESS_KEY_ID"]))
            .iter()
            .any(|p| p.contains("DGP_BOOTSTRAP_PASSWORD_HASH")));
        let auto = cr(3, Some(GOOD_HA_YAML), true);
        assert!(multi_replica_problems(&auto, &keys(&["DGP_ACCESS_KEY_ID"])).is_empty());
    }

    #[test]
    fn named_but_absent_secret_blocks() {
        let c = cr(2, Some(GOOD_HA_YAML), false);
        let problems = multi_replica_problems(&c, &EnvSecret::Missing("dgp-env".into()));
        assert!(problems.iter().any(|p| p.contains("does not exist")));
    }

    #[test]
    fn effective_replicas_holds_fleet_never_scales_down_on_problems() {
        // Clean preflight: spec wins.
        assert_eq!(effective_replicas(3, true, Some(2)), 3);
        // Problems on a running 3-fleet: HOLD 3, never kill healthy pods.
        assert_eq!(effective_replicas(3, false, Some(3)), 3);
        // Problems while asking to grow 2→5: hold at 2.
        assert_eq!(effective_replicas(5, false, Some(2)), 2);
        // Problems on a fresh deploy (no StatefulSet yet): come up at 1.
        assert_eq!(effective_replicas(3, false, None), 1);
        assert_eq!(effective_replicas(3, false, Some(0)), 1);
        // Spec below current: shrink is honored even with problems (user asked).
        assert_eq!(effective_replicas(1, false, Some(3)), 1);
    }

    #[test]
    fn phase_message_truth_table() {
        let (p, m) = phase_and_message(&["x".into()], 3, 3, 2);
        assert_eq!(p, "Degraded");
        assert!(m.contains("scale-up blocked"));
        let (p, _) = phase_and_message(&[], 3, 3, 1);
        assert_eq!(p, "Ready");
        let (p, _) = phase_and_message(&[], 1, 3, 2);
        assert_eq!(p, "Progressing");
        let (p, _) = phase_and_message(&[], 3, 3, 0);
        assert_eq!(p, "Progressing", "no router ready means not Ready");
    }

    #[test]
    fn invalid_yaml_blocks_with_parse_error() {
        let c = cr(2, Some(": not yaml : ["), false);
        assert!(
            multi_replica_problems(&c, &keys(&["DGP_BOOTSTRAP_PASSWORD_HASH"]))
                .iter()
                .any(|p| p.contains("not valid YAML"))
        );
    }
}
