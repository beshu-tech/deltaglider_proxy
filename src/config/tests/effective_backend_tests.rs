// SPDX-License-Identifier: BUSL-1.1

use crate::config::*;

const NAMED: &str = r#"
storage:
  backends:
    - name: hetzner-fsn1
      type: s3
      endpoint: "http://127.0.0.1:1"
      region: fsn1
      access_key_id: x
      secret_access_key: y
    - name: local-disk
      type: filesystem
      path: /tmp/dgp-effective-backend
  buckets:
    releases: {}
    db-archive: { backend: local-disk }
"#;

#[test]
fn unrouted_bucket_goes_to_first_named_backend() {
    let cfg = Config::from_yaml_str(NAMED).unwrap();
    assert_eq!(cfg.default_backend_name(), "hetzner-fsn1");
    let (name, def) = cfg.effective_backend_for_bucket("releases").unwrap();
    assert_eq!(name, "hetzner-fsn1");
    assert!(matches!(def, BackendConfig::S3 { .. }));
    // A bucket with no policy at all resolves the same way.
    assert_eq!(
        cfg.effective_backend_for_bucket("downloads").unwrap().0,
        "hetzner-fsn1"
    );
}

#[test]
fn explicit_route_and_default_backend_win() {
    let mut cfg = Config::from_yaml_str(NAMED).unwrap();
    assert_eq!(
        cfg.effective_backend_for_bucket("DB-Archive").unwrap().0,
        "local-disk",
        "bucket keys are lowercase"
    );
    cfg.default_backend = Some("local-disk".into());
    assert_eq!(
        cfg.effective_backend_for_bucket("releases").unwrap().0,
        "local-disk"
    );
}

#[test]
fn singleton_is_default_and_only_while_no_named_backends() {
    let cfg = Config::from_yaml_str("storage:\n  buckets:\n    releases: {}\n").unwrap();
    assert_eq!(cfg.default_backend_name(), "default");
    assert_eq!(
        cfg.effective_backend_for_bucket("releases").unwrap().0,
        "default"
    );
    let named = Config::from_yaml_str(NAMED).unwrap();
    assert!(named.backend_by_name("default").is_none());
}

#[test]
fn undefined_route_is_none() {
    let mut cfg = Config::from_yaml_str(NAMED).unwrap();
    cfg.buckets.get_mut("releases").unwrap().backend = Some("aws-dr".into());
    assert!(cfg.effective_backend_for_bucket("releases").is_none());
}

/// Production shape: named backends only, no singleton. The sync client
/// must target the named S3 default, not the unused filesystem default.
#[test]
fn coordination_backend_follows_named_default() {
    let mut cfg = Config::from_yaml_str(NAMED).unwrap();
    cfg.config_sync_bucket = Some("dgp-sync".into());
    assert!(matches!(
        cfg.coordination_backend(),
        Some(BackendConfig::S3 { .. })
    ));
    // A sync bucket routed to the filesystem backend follows the route.
    cfg.buckets.insert(
        "dgp-sync".into(),
        crate::bucket_policy::BucketPolicyConfig {
            backend: Some("local-disk".into()),
            ..Default::default()
        },
    );
    assert!(matches!(
        cfg.coordination_backend(),
        Some(BackendConfig::Filesystem { .. })
    ));
}

#[test]
fn coordination_backend_keeps_an_s3_singleton() {
    let mut cfg = Config::from_yaml_str(NAMED).unwrap();
    cfg.backend = BackendConfig::S3 {
        endpoint: Some("http://127.0.0.1:2".into()),
        region: "us-east-1".into(),
        force_path_style: true,
        access_key_id: None,
        secret_access_key: None,
        session_token: None,
        allow_local: true,
    };
    cfg.config_sync_bucket = Some("dgp-sync".into());
    assert_eq!(cfg.coordination_backend(), Some(&cfg.backend));
}

/// Source guard: the "which backend is this bucket on" rule lives only
/// in `Config::effective_backend_for_bucket`. Four hand-rolled copies
/// once disagreed (the CAS gate, the health gate, migrate, the sync
/// client) and HA went silently off with named backends.
#[test]
fn effective_backend_rule_has_one_home() {
    let patterns = [
        regex_lite::Regex::new(r"backends\[0\]\.name").unwrap(),
        regex_lite::Regex::new(r"default_backend\.clone\(\)\.(unwrap_or|or_else)").unwrap(),
        regex_lite::Regex::new(r"or_else\(\|\|\w+\.default_backend\.clone\(\)\)").unwrap(),
        regex_lite::Regex::new(r#"unwrap_or_else\(\|\|"default"\.to_string\(\)\)"#).unwrap(),
        // The coordination client must come from the resolver, not the
        // singleton (unused once named backends exist).
        regex_lite::Regex::new(r"build_client\(&(config|cfg)\.backend\)").unwrap(),
        regex_lite::Regex::new(r"ConfigDbSync::new\(&(config|cfg)\.backend,").unwrap(),
    ];
    let allowed = ["src/config/mod.rs"];
    let mut offenders = Vec::new();
    for path in crate::source_scan::rust_files("src") {
        let rel = crate::source_scan::rel(&path);
        if allowed.contains(&rel.as_str()) || rel.starts_with("src/cli/") {
            continue;
        }
        let text: String = std::fs::read_to_string(&path)
            .unwrap()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        for p in &patterns {
            if p.is_match(&text) {
                offenders.push(format!("{rel}: {}", p.as_str()));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "resolve the bucket's backend with Config::effective_backend_for_bucket: {offenders:?}"
    );
}

/// Issue #116 P0.4: the encryption of a BUCKET has one home,
/// `Config::encryption_for_bucket`. A call site that reads a backend's
/// encryption by name to decide a bucket's would miss a per-bucket
/// override later. Only the backend-level views listed here may ask
/// about a backend by name, each the given number of times.
#[test]
fn bucket_encryption_rule_has_one_home() {
    let needle = concat!("backend_encryption", "_by_name(");
    let allowed: &[(&str, usize, &str)] = &[
        // The definition and the resolver.
        ("src/config/mod.rs", 2, "home"),
        // GET /backends/:name/legacy-key-usage: the backend's own legacy key.
        ("src/api/admin/backends.rs", 1, "backend-level view"),
        // destination_count_on_backend: the raw client of a target backend.
        ("src/maintenance/migrate.rs", 1, "backend-level client"),
    ];
    let mut offenders = Vec::new();
    for path in crate::source_scan::rust_files("src") {
        let rel = crate::source_scan::rel(&path);
        let text = std::fs::read_to_string(&path).unwrap();
        let n = text.matches(needle).count();
        let limit = allowed
            .iter()
            .find(|(f, _, _)| *f == rel)
            .map_or(0, |(_, n, _)| *n);
        if n > limit {
            offenders.push(format!("{rel}: {n} (allowed {limit})"));
        }
    }
    assert!(
        offenders.is_empty(),
        "decide a bucket's encryption with Config::encryption_for_bucket: {offenders:?}"
    );
}

/// `encryption_for_bucket` answers with the effective backend's
/// encryption (no bucket override exists yet), and `None` for a route to
/// an undefined backend.
#[test]
fn encryption_for_bucket_follows_the_effective_backend() {
    let mut cfg = Config::from_yaml_str(NAMED).unwrap();
    cfg.backends[1].encryption = BackendEncryptionConfig::Aes256GcmProxy {
        key: Some("ab".repeat(32)),
        key_id: Some("local-2026".into()),
        legacy_key: None,
        legacy_key_id: None,
        plaintext_reads: Default::default(),
    };
    let e = cfg.encryption_for_bucket("db-archive").unwrap();
    assert_eq!(e.backend, "local-disk");
    assert_eq!(e.config, &cfg.backends[1].encryption);
    let e = cfg.encryption_for_bucket("releases").unwrap();
    assert_eq!(e.backend, "hetzner-fsn1");
    assert_eq!(e.config.mode_tag(), "none");
    cfg.buckets.get_mut("releases").unwrap().backend = Some("gone".into());
    assert!(cfg.encryption_for_bucket("releases").is_none());
}
