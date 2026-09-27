// SPDX-License-Identifier: BUSL-1.1

use crate::config::*;
use std::collections::HashMap;

fn lookup(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: HashMap<String, String> = vars
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |name: &str| map.get(name).cloned()
}

const FILE: &str = r#"
access:
  access_key_id: FILEKEY
  secret_access_key: file-secret
storage:
  backend:
    type: filesystem
    path: /srv/file-data
  backends:
    - name: eu-archive
      type: filesystem
      path: /srv/eu
      encryption:
        mode: aes256-gcm-proxy
advanced:
  cache_size_mb: 100
"#;

fn loaded(env: &[(&str, &str)]) -> Config {
    let mut cfg = Config::from_yaml_str(FILE).unwrap();
    cfg.apply_env_overrides_at_load(&lookup(env));
    cfg
}

const ENV: &[(&str, &str)] = &[
    ("DGP_SECRET_ACCESS_KEY", "env-secret"),
    ("DGP_CACHE_MB", "777"),
    ("DGP_S3_ENDPOINT", "http://minio:9000"),
    ("DGP_BE_AWS_SECRET_ACCESS_KEY", "env-be-secret"),
    ("DGP_TLS_ENABLED", "true"),
    ("DGP_BACKEND_EU_ARCHIVE_ENCRYPTION_KEY", "env-enc-key"),
    ("DGP_BOOTSTRAP_PASSWORD_HASH", "env-hash"),
];

fn assert_no_env_value(text: &str) {
    for leak in [
        "env-secret",
        "777",
        "minio",
        "env-be-secret",
        "env-enc-key",
        "env-hash",
    ] {
        assert!(!text.contains(leak), "{leak} leaked:\n{text}");
    }
}

#[test]
fn runtime_holds_env_values_and_the_file_view_holds_file_values() {
    let cfg = loaded(ENV);
    assert_eq!(cfg.secret_access_key.as_deref(), Some("env-secret"));
    assert_eq!(cfg.cache_size_mb, 777);
    assert!(matches!(cfg.backend, BackendConfig::S3 { .. }));
    assert!(cfg.tls.is_some());

    let file = cfg.file_view().unwrap();
    assert_eq!(file.secret_access_key.as_deref(), Some("file-secret"));
    assert_eq!(file.access_key_id.as_deref(), Some("FILEKEY"));
    assert_eq!(file.cache_size_mb, 100);
    assert_eq!(
        file.backend,
        BackendConfig::Filesystem {
            path: "/srv/file-data".into()
        }
    );
    assert!(file.tls.is_none());
    assert!(file.env_shadow.is_empty());
    match &file.backends[0].encryption {
        BackendEncryptionConfig::Aes256GcmProxy { key, .. } => assert_eq!(*key, None),
        other => panic!("{other:?}"),
    }
}

#[test]
fn persist_and_every_export_write_the_file_view() {
    let cfg = loaded(ENV);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.yaml");
    cfg.persist_to_file(path.to_str().unwrap()).unwrap();
    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert_no_env_value(&on_disk);
    assert!(on_disk.contains("file-secret"), "{on_disk}");
    assert_no_env_value(&cfg.to_canonical_yaml().unwrap());
    assert_no_env_value(&cfg.redact_all_secrets().to_canonical_yaml().unwrap());
    // Reloading the persisted file and re-applying the env reproduces
    // the runtime exactly.
    let mut again = Config::from_yaml_str(&on_disk).unwrap();
    again.apply_env_overrides_tracked(&lookup(ENV));
    assert_eq!(again.secret_access_key, cfg.secret_access_key);
    assert_eq!(again.backend, cfg.backend);
}

#[test]
fn reapply_keeps_env_winning_and_the_file_view_stable() {
    let running = loaded(ENV);
    // An edit built from the running config: env values echoed back,
    // one unrelated field changed, one env-controlled field edited.
    let mut edit = running.clone();
    edit.env_shadow = Default::default();
    edit.max_delta_ratio = 0.5;
    edit.cache_size_mb = 50;
    let edited = edit.reapply_env_overrides(&running, &lookup(ENV)).unwrap();
    assert_eq!(edited.edited, vec!["cache_size_mb".to_string()]);

    // Runtime: env still wins, the unrelated edit applies.
    assert_eq!(edit.cache_size_mb, 777);
    assert_eq!(edit.secret_access_key.as_deref(), Some("env-secret"));
    assert_eq!(edit.max_delta_ratio, 0.5);
    // File: echoes keep the file's values, the edit to the env-controlled
    // field is what the operator authored for the file.
    let file = edit.file_view().unwrap();
    assert_eq!(file.secret_access_key.as_deref(), Some("file-secret"));
    assert_eq!(file.cache_size_mb, 50);
    assert_eq!(file.max_delta_ratio, 0.5);
    assert!(matches!(file.backend, BackendConfig::Filesystem { .. }));
}

#[test]
fn no_environment_means_no_shadow_and_identity_file_view() {
    let cfg = loaded(&[]);
    assert!(cfg.env_shadow.is_empty());
    assert_eq!(cfg.file_view().unwrap(), cfg);
}
