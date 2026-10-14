// SPDX-License-Identifier: BUSL-1.1

//! Per-bucket encryption (issue #116, P1): the config rules.

use crate::config::*;

const KA: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const KB: &str = "2222222222222222222222222222222222222222222222222222222222222222";

/// Two named backends; `db-archive` on `local-disk` has its own key.
fn named(db_archive: &str) -> String {
    format!(
        r#"
storage:
  backends:
    - name: hetzner-fsn1
      type: filesystem
      path: /tmp/dgp-bucket-enc-a
    - name: local-disk
      type: filesystem
      path: /tmp/dgp-bucket-enc-b
      encryption:
        mode: aes256-gcm-proxy
        key: "{KA}"
        key_id: local-2026
  buckets:
    releases: {{ backend: hetzner-fsn1 }}
    db-archive: {db_archive}
"#
    )
}

fn fatal(yaml: &str) -> Vec<String> {
    Config::from_yaml_str(yaml).unwrap().check_fatal()
}

#[test]
fn a_bucket_override_is_the_buckets_encryption() {
    let cfg = Config::from_yaml_str(&named(&format!(
        "{{ backend: local-disk, encryption: {{ mode: aes256-gcm-proxy, key: \"{KB}\", key_id: db-archive-2026 }} }}"
    )))
    .unwrap();
    assert!(cfg.check_fatal().is_empty(), "{:?}", cfg.check_fatal());
    let e = cfg.encryption_for_bucket("db-archive").unwrap();
    assert!(e.from_bucket);
    assert_eq!(e.backend, "local-disk");
    assert_eq!(e.config.primary_key(), Some(KB));
    // Another bucket on the same backend inherits the backend's key.
    let mut cfg2 = cfg.clone();
    cfg2.buckets.insert(
        "downloads".into(),
        crate::bucket_policy::BucketPolicyConfig {
            backend: Some("local-disk".into()),
            ..Default::default()
        },
    );
    let e = cfg2.encryption_for_bucket("downloads").unwrap();
    assert!(!e.from_bucket);
    assert_eq!(e.config.primary_key(), Some(KA));
    // The override round-trips through the canonical (persist) YAML.
    let text = cfg.to_canonical_yaml_for_persist_with(&|_| None).unwrap();
    let back = Config::from_yaml_str(&text).unwrap();
    assert_eq!(back.buckets["db-archive"], cfg.buckets["db-archive"]);
}

#[test]
fn a_bucket_override_needs_an_explicit_key_id() {
    let errs = fatal(&named(&format!(
        "{{ backend: local-disk, encryption: {{ mode: aes256-gcm-proxy, key: \"{KB}\" }} }}"
    )));
    assert!(
        errs.iter()
            .any(|e| e.contains("bucket 'db-archive'") && e.contains("key_id is required")),
        "{errs:?}"
    );
    // The legacy key needs its id too.
    let errs = fatal(&named(&format!(
        "{{ backend: local-disk, encryption: {{ mode: none, legacy_key: \"{KB}\" }} }}"
    )));
    assert!(errs.iter().any(|e| e.contains("legacy_key_id")), "{errs:?}");
}

#[test]
fn native_sse_on_a_bucket_is_refused() {
    let errs = fatal(&named(
        "{ backend: local-disk, encryption: { mode: sse-s3 } }",
    ));
    assert!(
        errs.iter()
            .any(|e| e.contains("'sse-s3' is not supported on a bucket")),
        "{errs:?}"
    );
}

#[test]
fn a_bucket_override_needs_a_backend_with_named_backends() {
    let errs = fatal(&named(&format!(
        "{{ encryption: {{ mode: aes256-gcm-proxy, key: \"{KB}\", key_id: db-archive-2026 }} }}"
    )));
    assert!(
        errs.iter().any(|e| e.contains("has no `backend:`")),
        "{errs:?}"
    );
}

/// A single backend has one storage: the pin is implicit.
#[test]
fn a_single_backend_pins_the_override_implicitly() {
    let yaml = format!(
        "storage:\n  filesystem: /tmp/dgp-bucket-enc-single\n  buckets:\n    db-archive:\n      encryption: {{ mode: aes256-gcm-proxy, key: \"{KB}\", key_id: db-archive-2026 }}\n"
    );
    let cfg = Config::from_yaml_str(&yaml).unwrap();
    assert!(cfg.check_fatal().is_empty(), "{:?}", cfg.check_fatal());
    let o = cfg.bucket_encryption_overrides();
    assert_eq!(o.len(), 1);
    assert_eq!(
        (o[0].backend.as_str(), o[0].real_bucket.as_str()),
        ("default", "db-archive")
    );
}

/// Two names on one real bucket must agree: the encryption belongs to
/// the storage.
#[test]
fn aliases_that_disagree_are_refused() {
    let yaml = format!(
        "{}    downloads: {{ backend: local-disk, alias: db-archive }}\n",
        named(&format!(
            "{{ backend: local-disk, encryption: {{ mode: aes256-gcm-proxy, key: \"{KB}\", key_id: db-archive-2026 }} }}"
        ))
    );
    let errs = fatal(&yaml);
    assert!(
        errs.iter()
            .any(|e| e.contains("do not agree on its encryption")),
        "{errs:?}"
    );
    // The same block on both names is fine.
    let yaml = format!(
        "{}    downloads: {{ backend: local-disk, alias: db-archive, encryption: {{ mode: aes256-gcm-proxy, key: \"{KB}\", key_id: db-archive-2026 }} }}\n",
        named(&format!(
            "{{ backend: local-disk, encryption: {{ mode: aes256-gcm-proxy, key: \"{KB}\", key_id: db-archive-2026 }} }}"
        ))
    );
    assert!(fatal(&yaml).is_empty(), "{:?}", fatal(&yaml));
}

#[test]
fn a_key_id_names_one_key_across_backends_and_buckets() {
    // The bucket reuses the backend's key id with another key.
    let errs = fatal(&named(&format!(
        "{{ backend: local-disk, encryption: {{ mode: aes256-gcm-proxy, key: \"{KB}\", key_id: local-2026 }} }}"
    )));
    assert!(
        errs.iter()
            .any(|e| e.contains("key id 'local-2026'") && e.contains("DIFFERENT keys")),
        "{errs:?}"
    );
    // The same id with the same key is one key: allowed.
    let ok = fatal(&named(&format!(
        "{{ backend: local-disk, encryption: {{ mode: aes256-gcm-proxy, key: \"{KA}\", key_id: local-2026 }} }}"
    )));
    assert!(ok.is_empty(), "{ok:?}");
}

/// Owner decision 3: no per-bucket encryption while config sync is on.
#[test]
fn a_bucket_override_is_refused_with_a_sync_bucket() {
    let yaml = format!(
        "{}advanced:\n  config_sync_bucket: dgp-sync\n",
        named(&format!(
            "{{ backend: local-disk, encryption: {{ mode: aes256-gcm-proxy, key: \"{KB}\", key_id: db-archive-2026 }} }}"
        ))
    );
    let errs = fatal(&yaml);
    assert!(
        errs.iter()
            .any(|e| e.contains("config_sync_bucket") && e.contains("single-instance")),
        "{errs:?}"
    );
}

fn with_override(enc: &str) -> Config {
    Config::from_yaml_str(&named(&format!(
        "{{ backend: local-disk, encryption: {enc} }}"
    )))
    .unwrap()
}

#[test]
fn an_active_bucket_key_never_leaves_in_one_step() {
    let old = with_override(&format!(
        "{{ mode: aes256-gcm-proxy, key: \"{KB}\", key_id: db-archive-2026 }}"
    ));
    // The whole block removed: refused.
    let mut gone = old.clone();
    gone.buckets.get_mut("db-archive").unwrap().encryption = None;
    let errs = gone.dropped_bucket_keys(&old);
    assert!(
        errs.iter().any(|e| e.contains("key id 'db-archive-2026'")),
        "{errs:?}"
    );
    // mode none with the key kept as legacy: allowed.
    let kept = with_override(&format!(
        "{{ mode: none, legacy_key: \"{KB}\", legacy_key_id: db-archive-2026 }}"
    ));
    assert!(kept.dropped_bucket_keys(&old).is_empty());
    // Removing a decrypt-only key is the operator's step (after the scan).
    let mut no_legacy = kept.clone();
    no_legacy.buckets.get_mut("db-archive").unwrap().encryption = None;
    assert!(no_legacy.dropped_bucket_keys(&kept).is_empty());
    // A bucket that moves to another backend keeps its key (migrate flip).
    let mut moved = old.clone();
    moved.buckets.get_mut("db-archive").unwrap().backend = Some("hetzner-fsn1".into());
    assert!(moved.dropped_bucket_keys(&old).is_empty());
}

#[test]
fn a_changed_bucket_encryption_names_the_reencrypt_job() {
    let old = Config::from_yaml_str(&named("{ backend: local-disk }")).unwrap();
    let new = with_override(&format!(
        "{{ mode: aes256-gcm-proxy, key: \"{KB}\", key_id: db-archive-2026 }}"
    ));
    let notes = new.bucket_encryption_notes(&old);
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].contains("bucket 'db-archive'"), "{}", notes[0]);
    assert!(notes[0].contains("db-archive-2026"), "{}", notes[0]);
    assert!(notes[0].contains("local-2026"), "{}", notes[0]);
    assert!(notes[0].contains("/jobs/reencrypt"), "{}", notes[0]);
    assert!(new.bucket_encryption_notes(&new).is_empty());
}

/// Owner decision 4: encrypted → plaintext replication is a warning.
#[test]
fn replication_from_encrypted_into_plaintext_warns() {
    let yaml = format!(
        "{}  replication:\n    rules:\n      - name: archive-to-releases\n        source: {{ bucket: db-archive }}\n        destination: {{ bucket: releases }}\n",
        named(&format!(
            "{{ backend: local-disk, encryption: {{ mode: aes256-gcm-proxy, key: \"{KB}\", key_id: db-archive-2026 }} }}"
        ))
    );
    let mut cfg = Config::from_yaml_str(&yaml).unwrap();
    let warnings = cfg.check();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("archive-to-releases") && w.contains("not encrypted at rest")),
        "{warnings:?}"
    );
}

/// The bucket key never appears in GET /config or the export.
#[test]
fn redaction_strips_a_bucket_key() {
    let cfg = with_override(&format!(
        "{{ mode: aes256-gcm-proxy, key: \"{KB}\", key_id: db-archive-2026, legacy_key: \"{KA}\", legacy_key_id: local-2026 }}"
    ));
    for text in [
        serde_json::to_string(&cfg.redact_all_secrets()).unwrap(),
        cfg.to_canonical_yaml_with(&|_| None).unwrap(),
    ] {
        assert!(!text.contains(KB), "bucket key leaked: {text}");
        assert!(!text.contains(KA), "legacy key leaked: {text}");
        assert!(text.contains("db-archive-2026"), "the key id stays visible");
    }
}

/// P0 behaviour on a bucket: proxy mode with no key refuses the apply that
/// makes it, and `check` warns about it.
#[test]
fn a_keyless_bucket_override_is_refused_on_apply() {
    let old = Config::from_yaml_str(&named("{ backend: local-disk }")).unwrap();
    let mut new = with_override("{ mode: aes256-gcm-proxy, key_id: db-archive-2026 }");
    let errs = new.newly_keyless_proxy_backends(&old);
    assert!(
        errs.iter()
            .any(|e| e.contains("bucket 'db-archive'") && e.contains("no key")),
        "{errs:?}"
    );
    assert!(new.newly_keyless_proxy_backends(&new.clone()).is_empty());
    assert!(new
        .check()
        .iter()
        .any(|w| w.contains("bucket 'db-archive'") && w.contains("refuses every write")),);
}
