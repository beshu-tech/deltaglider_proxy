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

/// Two named backends, one of them S3, with `db-archive` on `s3-bucket`.
fn on_s3(db_archive: &str, backend_enc: &str) -> String {
    format!(
        r#"
storage:
  backends:
    - name: hetzner-fsn1
      type: filesystem
      path: /tmp/dgp-bucket-enc-a
    - name: aws-dr
      type: s3
      endpoint: http://127.0.0.1:9
      access_key_id: k
      secret_access_key: s
      region: us-east-1
{backend_enc}
  buckets:
    db-archive: {db_archive}
"#
    )
}

/// Issue #116 P3: a bucket on an S3 backend takes native SSE; on a
/// filesystem backend it is refused (the bytes would be stored as they are).
#[test]
fn native_sse_on_a_bucket_needs_an_s3_backend() {
    let ok = fatal(&on_s3(
        "{ backend: aws-dr, encryption: { mode: sse-kms, kms_key_id: \"alias/db-archive\" } }",
        "",
    ));
    assert!(ok.is_empty(), "{ok:?}");
    let ok = fatal(&on_s3(
        "{ backend: aws-dr, encryption: { mode: sse-s3 } }",
        "",
    ));
    assert!(ok.is_empty(), "{ok:?}");
    let errs = fatal(&named(
        "{ backend: local-disk, encryption: { mode: sse-s3 } }",
    ));
    assert!(
        errs.iter().any(|e| e.contains("bucket 'db-archive'")
            && e.contains("'sse-s3' is native S3 encryption")
            && e.contains("filesystem")),
        "{errs:?}"
    );
}

/// A `kms_key_id` that no KMS accepts is fatal, on a bucket AND on a backend.
#[test]
fn a_malformed_kms_key_id_is_fatal_on_buckets_and_backends() {
    let errs = fatal(&on_s3(
        "{ backend: aws-dr, encryption: { mode: sse-kms, kms_key_id: \"my key\" } }",
        "",
    ));
    assert!(
        errs.iter()
            .any(|e| e.starts_with("bucket 'db-archive': kms_key_id must be")),
        "{errs:?}"
    );
    let errs = fatal(&on_s3(
        "{ backend: aws-dr }",
        "      encryption:\n        mode: sse-kms\n        kms_key_id: \"\"\n",
    ));
    assert!(
        errs.iter()
            .any(|e| e.starts_with("backend 'aws-dr': kms_key_id is empty")),
        "{errs:?}"
    );
}

#[test]
fn kms_key_id_shapes() {
    for ok in [
        "arn:aws:kms:eu-central-1:123456789012:key/1234abcd-12ab-34cd-56ef-1234567890ab",
        "arn:aws:kms:eu-central-1:123456789012:alias/db-archive",
        "arn:aws-us-gov:kms:us-gov-west-1:123456789012:key/mrk-1234abcd12ab34cd56ef1234567890ab",
        "arn:aws:kms:dgp-test-key",
        "alias/db-archive",
        "alias/aws/s3",
        "1234abcd-12ab-34cd-56ef-1234567890ab",
        "mrk-1234abcd12ab34cd56ef1234567890ab",
    ] {
        assert_eq!(kms_key_id_problem(ok), None, "{ok}");
    }
    for bad in [
        "",
        "  ",
        "my-key",
        "alias/",
        "alias/has space",
        "arn:aws:s3:::bucket",
        "arn:aws:kms:",
        "arn:aws:kms:eu-central-1:123456789012:key/",
        "1234abcd-12ab-34cd-56ef",
        "mrk-123",
        "arn:aws:kms:eu-central-1:1:key/a\nb",
    ] {
        assert!(kms_key_id_problem(bad).is_some(), "{bad:?}");
    }
}

proptest::proptest! {
    /// Anything with whitespace is never a key id.
    #[test]
    fn kms_key_id_with_whitespace_is_refused(a in "[a-z0-9/:-]{0,20}", b in "[a-z0-9/:-]{0,20}") {
        let with_space = format!("{a} {b}");
        proptest::prop_assert!(kms_key_id_problem(&with_space).is_some());
    }
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
fn a_bucket_override_is_allowed_with_a_sync_bucket() {
    // Issue #116 P3: the encryption witness replaces P1's refusal.
    let yaml = format!(
        "{}advanced:\n  config_sync_bucket: dgp-sync\n",
        named(&format!(
            "{{ backend: local-disk, encryption: {{ mode: aes256-gcm-proxy, key: \"{KB}\", key_id: db-archive-2026 }} }}"
        ))
    );
    let errs = fatal(&yaml);
    assert!(
        !errs.iter().any(|e| e.contains("config_sync_bucket")),
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
    let errs = gone.dropped_bucket_keys(&old, &[], &[]);
    assert!(
        errs.iter().any(|e| e.contains("key id 'db-archive-2026'")),
        "{errs:?}"
    );
    // mode none with the key kept as legacy: allowed.
    let kept = with_override(&format!(
        "{{ mode: none, legacy_key: \"{KB}\", legacy_key_id: db-archive-2026 }}"
    ));
    assert!(kept.dropped_bucket_keys(&old, &[], &[]).is_empty());
    // Removing a decrypt-only key needs the confirmation of its id
    // (review A: before, it had no gate at all).
    let mut no_legacy = kept.clone();
    no_legacy.buckets.get_mut("db-archive").unwrap().encryption = None;
    let errs = no_legacy.dropped_bucket_keys(&kept, &[], &[]);
    assert!(
        errs.iter()
            .any(|e| e.contains("legacy")
                && e.contains("X-DGP-Confirm-Key-Removal: db-archive-2026")),
        "{errs:?}"
    );
    assert!(no_legacy
        .dropped_bucket_keys(&kept, &["db-archive-2026".to_string()], &[])
        .is_empty());
    // A bucket that moves to another backend keeps its key (migrate flip).
    let mut moved = old.clone();
    moved.buckets.get_mut("db-archive").unwrap().backend = Some("hetzner-fsn1".into());
    assert!(moved.dropped_bucket_keys(&old, &[], &[]).is_empty());
}

/// Review A, the executed probe: `releases` and `downloads` share the key
/// id `shared-a`. Before, each step passed because the OTHER bucket still
/// held the id: `releases` dropped its block, then `downloads` rotated and
/// dropped its legacy key, and the objects of `releases` under
/// `shared-a` became unreadable. A bucket's keys are now judged on that
/// bucket alone (its block and its backend's), never on a neighbour.
#[test]
fn a_bucket_never_relies_on_a_neighbours_key() {
    let a = format!("{{ mode: aes256-gcm-proxy, key: \"{KB}\", key_id: shared-a }}");
    let yaml = |releases: &str, downloads: &str| {
        format!(
            "{}    downloads: {{ backend: local-disk, encryption: {downloads} }}\n",
            named("{ backend: local-disk }").replace(
                "releases: { backend: hetzner-fsn1 }",
                &format!("releases: {{ backend: local-disk, encryption: {releases} }}")
            )
        )
    };
    let old = Config::from_yaml_str(&yaml(&a, &a)).unwrap();
    let mut step1 = old.clone();
    step1.buckets.get_mut("releases").unwrap().encryption = None;
    let errs = step1.dropped_bucket_keys(&old, &[], &[]);
    assert!(
        errs.iter()
            .any(|e| e.contains("bucket 'releases'") && e.contains("shared-a")),
        "releases drops shared-a while downloads still holds it: {errs:?}"
    );
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

/// Review E: the override belongs to the stored bucket, so a client that
/// names the REAL bucket `real-archive` of `db-archive: { alias:
/// real-archive }` gets the override from the resolver, as the engine
/// wrapper (indexed by real bucket) writes it. Before, the resolver
/// looked up the policy NAME and answered with the backend's encryption.
#[test]
fn the_resolver_finds_the_override_by_real_bucket() {
    let mut cfg = Config::from_yaml_str(&named(&format!(
        "{{ backend: local-disk, alias: real-archive, encryption: {{ mode: aes256-gcm-proxy, key: \"{KB}\", key_id: db-archive-b }} }}"
    )))
    .unwrap();
    // `real-archive` has no policy; it routes to the default `local-disk`.
    cfg.default_backend = Some("local-disk".into());
    let e = cfg.encryption_for_bucket("real-archive").unwrap();
    assert!(e.from_bucket, "the real name resolves to the override");
    assert_eq!(e.owner.as_deref(), Some("db-archive"));
    assert_eq!(e.site_keys().primary_id(), Some("db-archive-b"));
    assert_eq!(
        crate::maintenance::resolve_desired(&cfg, "real-archive").unwrap(),
        crate::maintenance::DesiredEncryption::Proxy {
            key_id: "db-archive-b".into()
        }
    );
}

/// Review of P3: `storage.backend.encryption:` (the singleton's encryption
/// is `storage.backend_encryption`) was dropped without a word, so the
/// backend stored plaintext. An unknown field in a backend definition or
/// an encryption block is a load error that names it, also in a named
/// backend (where the definition is flattened) and in the flat shape.
#[test]
fn an_unknown_backend_or_encryption_field_is_a_load_error() {
    let cases = [
        format!(
            "storage:\n  backend:\n    type: filesystem\n    path: /tmp/x\n    encryption:\n      mode: aes256-gcm-proxy\n      key: \"{KA}\"\n"
        ),
        "storage:\n  backends:\n    - name: local-disk\n      type: filesystem\n      path: /tmp/x\n      encrypton: { mode: none }\n".to_string(),
        "storage:\n  backends:\n    - name: aws-dr\n      type: s3\n      regoin: eu-west-1\n".to_string(),
        "storage:\n  backend_encryption:\n    mode: sse-kms\n    kms_key: alias/x\n".to_string(),
        "backend:\n  type: filesystem\n  path: /tmp/x\n  encryption: { mode: none }\n".to_string(),
    ];
    for yaml in &cases {
        let err = Config::from_yaml_str(yaml)
            .err()
            .unwrap_or_else(|| panic!("loaded: {yaml}"));
        assert!(err.to_string().contains("unknown field"), "{yaml}: {err}");
    }
    // The documented shapes still load.
    for ok in [
        format!(
            "storage:\n  backends:\n    - name: local-disk\n      type: filesystem\n      path: /tmp/x\n      encryption: {{ mode: aes256-gcm-proxy, key: \"{KA}\", key_id: k }}\n"
        ),
        "storage:\n  backend:\n    type: s3\n    region: eu-west-1\n    force_path_style: \"true\"\n  backend_encryption: { mode: sse-s3 }\n".to_string(),
    ] {
        Config::from_yaml_str(&ok).unwrap_or_else(|e| panic!("{ok}: {e}"));
    }
}

/// The modes that the admin UI offers on a bucket (`BUCKET_ENCRYPTION_MODES`)
/// are exactly the modes that `check_fatal` accepts on a bucket override.
#[test]
fn bucket_modes_match_the_fatal_rules() {
    let blocks = [
        ("none", "{ mode: none }".to_string()),
        (
            "aes256-gcm-proxy",
            format!("{{ mode: aes256-gcm-proxy, key: \"{KB}\", key_id: db-archive-2026 }}"),
        ),
        (
            "sse-kms",
            "{ mode: sse-kms, kms_key_id: \"arn:aws:kms:eu-central-1:123456789012:key/1234abcd-12ab-34cd-56ef-1234567890ab\" }".to_string(),
        ),
        ("sse-s3", "{ mode: sse-s3 }".to_string()),
    ];
    // A filesystem backend (local-disk) and an S3 backend (aws-dr): native
    // SSE is offered, and accepted, only on S3.
    let doc = |bucket: &str| {
        format!(
            r#"
storage:
  backends:
    - name: local-disk
      type: filesystem
      path: /tmp/dgp-bucket-modes
    - name: aws-dr
      type: s3
      region: eu-central-1
      access_key_id: AKIAEXAMPLE
      secret_access_key: example-secret
  buckets:
    db-archive: {bucket}
"#
        )
    };
    let cfg = Config::from_yaml_str(&doc("{ backend: local-disk }")).unwrap();
    for backend in ["local-disk", "aws-dr"] {
        let offered = cfg.bucket_encryption_modes(backend);
        for (mode, block) in &blocks {
            let errs = fatal(&doc(&format!(
                "{{ backend: {backend}, encryption: {block} }}"
            )));
            let accepted = !errs.iter().any(|e| e.contains("bucket 'db-archive'"));
            assert_eq!(
                accepted,
                offered.contains(mode),
                "{backend}, mode {mode}: the server accepts it = {accepted}, the UI offers it = {}: {errs:?}",
                offered.contains(mode)
            );
        }
    }
}

/// A bucket without a fixed storage that the listing finds on a
/// non-default backend is written with that backend's encryption.
#[test]
fn a_listed_bucket_takes_the_encryption_of_the_backend_that_holds_it() {
    let cfg = Config::from_yaml_str(&named("{ backend: local-disk }")).unwrap();
    // `stray` has no route: by config it is on the default (hetzner-fsn1).
    let e = cfg.encryption_for_bucket("stray").unwrap();
    assert_eq!(
        (e.backend.as_str(), e.config.mode_tag()),
        ("hetzner-fsn1", "none")
    );
    let e = cfg
        .encryption_for_listed_bucket("stray", Some("local-disk"))
        .unwrap();
    assert_eq!(e.backend, "local-disk");
    assert_eq!(e.config.mode_tag(), "aes256-gcm-proxy");
    assert!(!e.from_bucket);
    // A routed bucket ignores the listing.
    let e = cfg
        .encryption_for_listed_bucket("releases", Some("local-disk"))
        .unwrap();
    assert_eq!(e.backend, "hetzner-fsn1");
    // An unknown backend name changes nothing.
    let e = cfg
        .encryption_for_listed_bucket("stray", Some("gone"))
        .unwrap();
    assert_eq!(e.backend, "hetzner-fsn1");
}

/// Review P2 (defects 1 and 2): a bucket that does not exist on its
/// storage holds no object, so its keys can leave without a scan or a
/// confirmation: the rollback of a failed create, and the removal of a
/// policy whose bucket was never created.
#[test]
fn the_keys_of_a_bucket_that_does_not_exist_can_leave() {
    let old = with_override(&format!(
        "{{ mode: aes256-gcm-proxy, key: \"{KB}\", key_id: db-archive-2026 }}"
    ));
    let mut gone = old.clone();
    gone.buckets.remove("db-archive");
    assert!(!gone.dropped_bucket_keys(&old, &[], &[]).is_empty());
    assert!(gone
        .dropped_bucket_keys(&old, &[], &["db-archive".to_string()])
        .is_empty());
    // Another absent bucket does not excuse this one.
    assert!(!gone
        .dropped_bucket_keys(&old, &[], &["releases".to_string()])
        .is_empty());
}
