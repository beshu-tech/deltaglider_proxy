// SPDX-License-Identifier: BUSL-1.1

//! ── Prod-shape regression tests ─────────────────────────────────────────
//!
//! `tests/fixtures/prod_shape_config.yaml` is a SANITIZED, structure-true
//! snapshot of the production configuration (same sections, same feature
//! usage, dummy secrets, docs-cast names). These tests are the canary for
//! "would prod's config still load on this branch?": if the model stops
//! accepting this file — parse, IAM validation, auth classification, or
//! canonical-export round-trip — the real prod config would break on
//! upgrade. Update the fixture deliberately when prod adopts new features.

use crate::config::*;

const PROD_SHAPE: &str = include_str!("../../../tests/fixtures/prod_shape_config.yaml");

fn parsed() -> Config {
    Config::from_yaml_str(PROD_SHAPE).expect("prod-shape config must parse")
}

#[test]
fn parses_and_classifies_auth() {
    let cfg = parsed();
    assert!(matches!(
        cfg.classify_auth_config(false),
        AuthConfigOutcome::CredentialsEnabled {
            redundant_none: false
        }
    ));
    assert_eq!(cfg.iam_mode, crate::config_sections::IamMode::Declarative);
}

#[test]
fn declarative_iam_passes_reconciler_validation() {
    // The exact projection + pure diff the apply path runs. An empty
    // DB snapshot models the first declarative apply after restore.
    let cfg = parsed();
    let snapshot = crate::iam::declarative::snapshot_from_access(
        &cfg.iam_users,
        &cfg.iam_groups,
        &cfg.auth_providers,
        &cfg.group_mapping_rules,
        &[],
    );
    let diff = crate::iam::declarative::diff_iam(
        &snapshot,
        &crate::iam::declarative::CurrentIam::default(),
    )
    .expect("prod-shape IAM must pass reconciler validation");
    assert_eq!(diff.users_to_create.len(), 6, "all six users created");
    assert_eq!(diff.groups_to_create.len(), 3, "all three groups created");
    assert_eq!(diff.providers_to_create.len(), 1, "OIDC provider created");
}

#[test]
fn iam_templates_survive_env_expansion() {
    // ${iam:username} permission templates must pass through the
    // ${env:...} expander untouched (they are substituted at auth
    // time, not at config load).
    let expanded =
        expand_env_with(PROD_SHAPE, |_| None).expect("prod-shape config must pass env expansion");
    assert!(expanded.contains("${iam:username}"));
}

#[test]
fn storage_shape_is_intact() {
    let cfg = parsed();
    let names: Vec<&str> = cfg.backends.iter().map(|b| b.name.as_str()).collect();
    assert_eq!(names, vec!["hetzner-fsn1", "local-disk"]);
    assert_eq!(cfg.default_backend.as_deref(), Some("hetzner-fsn1"));

    // The filesystem backend carries proxy-AES encryption with an
    // inline key (prod's actual setup).
    let local = &cfg.backends[1];
    match &local.encryption {
        BackendEncryptionConfig::Aes256GcmProxy { key, .. } => {
            assert!(key.is_some(), "inline AES key parsed");
        }
        other => panic!("expected aes256-gcm-proxy on local-disk, got {other:?}"),
    }

    assert_eq!(cfg.buckets.len(), 8);
    let routed_local = cfg
        .buckets
        .values()
        .filter(|p| p.backend.as_deref() == Some("local-disk"))
        .count();
    assert_eq!(routed_local, 7, "seven buckets routed to local-disk");
    assert_eq!(
        cfg.buckets["releases"].public_prefixes,
        vec!["firmware/public/".to_string()],
        "public prefix preserved"
    );

    assert!(cfg.replication.enabled);
    assert_eq!(cfg.replication.rules.len(), 1);
    assert_eq!(cfg.replication.rules[0].name, "mirror-releases-to-dr");
    assert!(!cfg.lifecycle.enabled);
    assert_eq!(cfg.lifecycle.rules.len(), 1);

    assert_eq!(cfg.max_object_size, 536_870_912);
}

#[test]
fn canonical_export_round_trips() {
    // What the admin /export endpoint emits must re-load on this
    // branch with the structure intact (secrets are redacted by
    // design — structure, not secrets, is the contract here).
    let cfg = parsed();
    let exported = cfg.to_canonical_yaml().expect("export must serialize");
    let re = Config::from_yaml_str(&exported).expect("exported YAML must re-parse");
    assert_eq!(re.iam_mode, crate::config_sections::IamMode::Declarative);
    assert_eq!(re.iam_users.len(), cfg.iam_users.len());
    assert_eq!(re.iam_groups.len(), cfg.iam_groups.len());
    assert_eq!(re.backends.len(), cfg.backends.len());
    assert_eq!(re.buckets.len(), cfg.buckets.len());
    assert_eq!(re.replication.rules.len(), 1);
    assert_eq!(re.lifecycle.rules.len(), 1);
}
