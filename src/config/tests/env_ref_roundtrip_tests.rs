// SPDX-License-Identifier: BUSL-1.1

use crate::config::env_template::Escape;
use crate::config::*;

const TEMPLATE: &str = r#"
access:
  access_key_id: admin
  secret_access_key: ${env:BOOTSTRAP_SECRET}
  iam_mode: declarative
  iam_users:
  - name: ci-uploader
    access_key_id: ci-uploader
    secret_access_key: ${env:CI_UPLOADER_SECRET}
    enabled: true
    permissions:
    - id: 0
      effect: Allow
      actions: [read, write, list]
      resources: ["releases/*"]
  auth_providers:
  - name: google-sso
    provider_type: oidc
    enabled: true
    priority: 0
    client_id: dummy-client-id
    client_secret: ${env:OIDC_CLIENT_SECRET}
    issuer_url: https://accounts.google.com
    scopes: openid email profile
storage:
  default_backend: remote
  backends:
  - name: remote
    type: s3
    endpoint: https://s3.example.test
    region: x
    access_key_id: ${env:REMOTE_KEY}
    secret_access_key: ${env:REMOTE_SECRET}
  - name: local
    type: filesystem
    path: ./data
    encryption:
      mode: aes256-gcm-proxy
      key: ${env:LOCAL_AES_KEY}
  buckets:
    releases:
      backend: remote
    scratch:
      backend: local
      compression: ${env:UNSET_WITH_DEFAULT:-null}
advanced:
  public_url: https://hooks.example/${env:HOOK_PATH}
"#;

fn lookup(name: &str) -> Option<String> {
    match name {
        "BOOTSTRAP_SECRET" => Some("boot-secret-123".into()),
        "CI_UPLOADER_SECRET" => Some("ci-secret-456".into()),
        "OIDC_CLIENT_SECRET" => Some("GOCSPX-oidc-789".into()),
        "REMOTE_KEY" => Some("AKREMOTE000000000000".into()),
        "REMOTE_SECRET" => Some("remote-secret-abc".into()),
        "HOOK_PATH" => Some("T0123/B456/hook-token".into()),
        "LOCAL_AES_KEY" => {
            Some("00000000000000000000000000000000000000000000000000000000000000ff".into())
        }
        _ => None,
    }
}

fn load_template() -> Config {
    Config::from_template_with(TEMPLATE, &lookup).expect("template loads")
}

fn render(cfg: &Config, path: &str) -> Option<String> {
    cfg.env_refs
        .template_at(path)
        .map(|t| t.render(Escape::Minimal))
}

#[test]
fn load_records_the_value_and_the_template_of_every_reference() {
    let cfg = load_template();
    // Six names resolved from the environment (a value), one more used its
    // default (no value: re-emitting it bare would fail the next load).
    assert_eq!(cfg.env_refs.len(), 7, "{:?}", *cfg.env_refs);
    assert_eq!(cfg.env_refs["CI_UPLOADER_SECRET"], "ci-secret-456");
    assert!(!cfg.env_refs.contains_key("UNSET_WITH_DEFAULT"));
    // Every string with a reference has a template, at its path.
    for (path, text) in [
        ("/secret_access_key", "${env:BOOTSTRAP_SECRET}"),
        (
            "/iam_users/[ci-uploader]/secret_access_key",
            "${env:CI_UPLOADER_SECRET}",
        ),
        (
            "/auth_providers/[google-sso]/client_secret",
            "${env:OIDC_CLIENT_SECRET}",
        ),
        (
            "/backends/[remote]/secret_access_key",
            "${env:REMOTE_SECRET}",
        ),
        ("/backends/[local]/encryption/key", "${env:LOCAL_AES_KEY}"),
        (
            "/buckets/scratch/compression",
            "${env:UNSET_WITH_DEFAULT:-null}",
        ),
        // An embedded reference is one template.
        ("/public_url", "https://hooks.example/${env:HOOK_PATH}"),
    ] {
        assert_eq!(render(&cfg, path).as_deref(), Some(text), "{path}");
    }
}

#[test]
fn export_reemits_templates_and_never_the_secrets() {
    let cfg = load_template();
    // The export-endpoint chain: redact_all_secrets → to_canonical_yaml.
    let exported = cfg.redact_all_secrets().to_canonical_yaml().unwrap();
    for r in [
        "${env:BOOTSTRAP_SECRET}",
        "${env:CI_UPLOADER_SECRET}",
        "${env:OIDC_CLIENT_SECRET}",
        "${env:REMOTE_KEY}",
        "${env:REMOTE_SECRET}",
        "${env:LOCAL_AES_KEY}",
        "https://hooks.example/${env:HOOK_PATH}",
    ] {
        assert!(exported.contains(r), "export must carry {r}:\n{exported}");
    }
    for s in [
        "boot-secret-123",
        "ci-secret-456",
        "GOCSPX-oidc-789",
        "remote-secret-abc",
        "hook-token",
        "00000000000000000000000000000000000000000000000000000000000000ff",
    ] {
        assert!(!exported.contains(s), "export must not leak {s}");
    }
}

/// The export of a boot file, applied unchanged as an admin document, is
/// accepted and gives back the same values and the same templates.
#[test]
fn an_unchanged_export_is_admitted_and_lossless() {
    let cfg = load_template();
    let exported = cfg.redact_all_secrets().to_canonical_yaml().unwrap();
    let (tree, added) =
        env_template::admit_document(&exported, &cfg.env_refs, &|_| false, &|_| None)
            .expect("the unchanged export is admitted");
    assert!(added.templates.is_empty() && added.is_empty());
    let text = serde_yaml::to_string(&tree).unwrap();
    let back = Config::from_yaml_str(&text).unwrap();
    assert_eq!(back.secret_access_key.as_deref(), Some("boot-secret-123"));
    assert_eq!(
        back.public_url.as_deref(),
        Some("https://hooks.example/T0123/B456/hook-token")
    );
    assert_eq!(back.iam_users[0].secret_access_key, "ci-secret-456");
    assert!(back.backends.iter().any(|b| b.name == "remote"));
}

#[test]
fn persist_reemits_templates_and_reloads_the_same_config() {
    let mut cfg = load_template();
    // A GUI tweak of a non-secret field.
    cfg.max_object_size = (42 * 1024 * 1024).into();
    let dir = std::env::temp_dir().join(format!("dgp-envref-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("cfg.yaml");
    cfg.persist_to_file(path.to_str().unwrap()).unwrap();
    let persisted = std::fs::read_to_string(&path).unwrap();
    std::fs::remove_dir_all(&dir).ok();
    assert!(persisted.contains("max_object_size: 44040192"));
    for r in [
        "${env:CI_UPLOADER_SECRET}",
        "${env:LOCAL_AES_KEY}",
        "https://hooks.example/${env:HOOK_PATH}",
    ] {
        assert!(persisted.contains(r), "the file keeps {r}:\n{persisted}");
    }
    for s in ["ci-secret-456", "hook-token"] {
        assert!(!persisted.contains(s), "{s} reached the file");
    }
    // The persisted file loads against the same env (boot path), with the
    // same values and the same templates.
    let re = Config::from_template_with(&persisted, &lookup).unwrap();
    assert_eq!(re.max_object_size_bytes(), 44040192);
    assert_eq!(re.iam_users[0].secret_access_key, "ci-secret-456");
    // Every template comes back, a typed field's too, but one whose value
    // is null: the field is not in the text, so there is nothing to write.
    let mut want = cfg.env_refs.clone();
    let mut frozen = (*want.frozen).clone();
    frozen.templates.remove("/buckets/scratch/compression");
    want.frozen = std::sync::Arc::new(frozen);
    assert_eq!(re.env_refs, want);
}

/// S9, by path: a secret that the file wrote in two fields gets its
/// template back in both, and plaintext never reaches the file.
#[test]
fn a_secret_used_in_two_fields_is_a_template_in_both() {
    let tpl = "access:\n  access_key_id: ${env:SHARED}\n  secret_access_key: ${env:SHARED}\n";
    let env = |n: &str| (n == "SHARED").then(|| "AKIA-shared-secret-1".to_string());
    let cfg = Config::from_template_with(tpl, &env).unwrap();
    let out = cfg.with_file_text(Escape::Minimal);
    assert_eq!(out.access_key_id.as_deref(), Some("${env:SHARED}"));
    assert_eq!(out.secret_access_key.as_deref(), Some("${env:SHARED}"));
    let yaml = cfg.to_canonical_yaml_for_persist_with(&|_| None).unwrap();
    assert!(!yaml.contains("AKIA-shared-secret-1"), "{yaml}");
}

/// A field with no template never turns into a reference, whatever its
/// value is: the match is by path, not by value.
#[test]
fn a_value_equal_to_a_recorded_value_is_not_turned_into_a_reference() {
    let tpl = "access:\n  secret_access_key: ${env:S}\n";
    let env = |n: &str| (n == "S").then(|| "hunter2-hunter2-hunter2".to_string());
    let mut cfg = Config::from_template_with(tpl, &env).unwrap();
    cfg.access_key_id = Some("hunter2-hunter2-hunter2".into());
    let out = cfg.with_file_text(Escape::Minimal);
    assert_eq!(out.secret_access_key.as_deref(), Some("${env:S}"));
    assert_eq!(
        out.access_key_id.as_deref(),
        Some("hunter2-hunter2-hunter2")
    );
}

/// A field that an edit changed is written as what it holds now.
#[test]
fn an_edited_field_is_written_as_its_new_value() {
    let mut cfg = load_template();
    cfg.public_url = Some("https://elsewhere.example".into());
    cfg.secret_access_key = Some("typed-by-hand".into());
    let out = cfg.with_file_text(Escape::Minimal);
    assert_eq!(out.public_url.as_deref(), Some("https://elsewhere.example"));
    assert_eq!(out.secret_access_key.as_deref(), Some("typed-by-hand"));
    assert_eq!(
        out.iam_users[0].secret_access_key, "${env:CI_UPLOADER_SECRET}",
        "the other templates stay"
    );
}

/// A renamed backend does not keep the template of its old name: the file
/// holds what the field holds. (The admin API refuses the reference at the
/// new name, see `env_template::tests`.)
#[test]
fn a_renamed_field_loses_its_template() {
    let mut cfg = load_template();
    cfg.backends
        .iter_mut()
        .find(|b| b.name == "remote")
        .unwrap()
        .name = "remote-2".into();
    let out = cfg.with_file_text(Escape::Minimal);
    let renamed = out.backends.iter().find(|b| b.name == "remote-2").unwrap();
    match &renamed.backend {
        BackendConfig::S3 {
            secret_access_key, ..
        } => assert_eq!(secret_access_key.as_deref(), Some("remote-secret-abc")),
        other => panic!("{other:?}"),
    }
}

/// B037: a `${env:NAME:-default}` ref keeps its default through persist,
/// so the next boot without NAME still loads.
#[test]
fn a_defaulted_ref_keeps_its_default_through_persist() {
    let tpl = "advanced:\n  log_level: ${env:LOG_LEVEL:-info}\n";
    let set = |n: &str| (n == "LOG_LEVEL").then(|| "debug".to_string());
    let cfg = Config::from_template_with(tpl, &set).unwrap();
    let out = cfg.to_canonical_yaml_for_persist_with(&|_| None).unwrap();
    assert!(out.contains("${env:LOG_LEVEL:-info}"), "{out}");
    Config::from_template_with(&out, &|_| None)
        .expect("the persisted file loads without LOG_LEVEL");
}

/// The text of a name with two different defaults is each site's own.
#[test]
fn each_site_keeps_its_own_default() {
    let tpl = "advanced:\n  log_level: ${env:L:-info}\n  config_sync_object_key: ${env:L:-other}\n";
    let cfg = Config::from_template_with(tpl, &|_| Some("debug".into())).unwrap();
    assert_eq!(
        render(&cfg, "/log_level").as_deref(),
        Some("${env:L:-info}")
    );
    assert_eq!(
        render(&cfg, "/config_sync_object_key").as_deref(),
        Some("${env:L:-other}")
    );
}

/// A literal that looks like a reference is escaped on persist and on
/// export, so a restart never expands it.
#[test]
fn a_literal_that_looks_like_a_reference_is_written_escaped() {
    let cfg = Config {
        config_sync_object_key: Some("${env:HOME}".into()),
        public_url: Some("https://h/${env:HOME}".into()),
        ..Config::default()
    };
    let persisted = cfg.to_canonical_yaml_for_persist_with(&|_| None).unwrap();
    assert!(persisted.contains("$${env:HOME}"), "{persisted}");
    let exported = cfg.to_canonical_yaml().unwrap();
    assert!(exported.contains("$${env:HOME}"), "{exported}");
    for text in [&persisted, &exported] {
        let env = |n: &str| (n == "HOME").then(|| "/home/leak".to_string());
        let back = Config::from_template_with(text, &env).unwrap();
        assert_eq!(back.config_sync_object_key.as_deref(), Some("${env:HOME}"));
        assert_eq!(back.public_url.as_deref(), Some("https://h/${env:HOME}"));
        assert!(back.env_refs.frozen.is_empty());
    }
}

/// A map key that holds `${env:` is escaped too.
#[test]
fn a_map_key_that_looks_like_a_reference_is_written_escaped() {
    let mut cfg = Config::default();
    cfg.event_delivery
        .webhook_headers
        .insert("X-${env:HOME}".into(), "v".into());
    let persisted = cfg.to_canonical_yaml_for_persist_with(&|_| None).unwrap();
    assert!(persisted.contains("X-$${env:HOME}"), "{persisted}");
    let env = |n: &str| (n == "HOME").then(|| "/home/leak".to_string());
    let back = Config::from_template_with(&persisted, &env).unwrap();
    assert!(back
        .event_delivery
        .webhook_headers
        .contains_key("X-${env:HOME}"));
}

/// A reference in a typed (non-string) field is recorded and loads; the
/// field is written as its value.
#[test]
fn a_reference_in_a_numeric_field_loads() {
    let tpl = "advanced:\n  cache_size_mb: ${env:CACHE}\n";
    let env = |n: &str| (n == "CACHE").then(|| "512".to_string());
    let cfg = Config::from_template_with(tpl, &env).unwrap();
    assert_eq!(cfg.cache_size_mb, 512);
    assert_eq!(
        render(&cfg, "/cache_size_mb").as_deref(),
        Some("${env:CACHE}")
    );
}

/// A flat boot file, a sectioned export and a storage shorthand name one
/// field by one path.
#[test]
fn the_flat_shape_and_a_shorthand_record_the_flat_paths() {
    let env = |n: &str| match n {
        "AK" => Some("AKFLAT0000000000001".to_string()),
        "SK" => Some("flat-secret-000000000000001".to_string()),
        _ => None,
    };
    let flat = Config::from_template_with(
        "access_key_id: ${env:AK}\nsecret_access_key: ${env:SK}\n",
        &env,
    )
    .unwrap();
    let sectioned = Config::from_template_with(
        "access:\n  access_key_id: ${env:AK}\n  secret_access_key: ${env:SK}\n",
        &env,
    )
    .unwrap();
    assert_eq!(flat.env_refs, sectioned.env_refs);
    let shorthand = Config::from_template_with(
        "storage:\n  s3: https://s3.example.test\n  access_key_id: ${env:AK}\n  secret_access_key: ${env:SK}\n",
        &env,
    )
    .unwrap();
    assert!(shorthand
        .env_refs
        .frozen
        .templates
        .contains_key("/backend/secret_access_key"));
}

/// The boot file has a whole reference in a typed field (an enum). A map key
/// that an admin stored with `${env:SECRET}` in it is still written escaped:
/// the typed-field path escapes keys like every other path.
#[test]
fn a_key_with_reference_text_is_escaped_when_a_typed_field_holds_a_reference() {
    let env = |n: &str| match n {
        "IAM_MODE" => Some("declarative".to_string()),
        "SECRET" => Some("hunter2-hunter2-hunter2".to_string()),
        _ => None,
    };
    let mut cfg = Config::from_template_with(
        "access:\n  iam_mode: ${env:IAM_MODE}\nadvanced:\n  event_delivery:\n    webhook_url: https://hooks.example/in\n",
        &env,
    )
    .unwrap();
    cfg.event_delivery
        .webhook_headers
        .insert("${env:SECRET}".into(), "v".into());
    let persisted = cfg.to_canonical_yaml_for_persist_with(&|_| None).unwrap();
    assert!(persisted.contains("$${env:SECRET}"), "{persisted}");
    assert!(persisted.contains("${env:IAM_MODE}"), "{persisted}");
    assert!(!persisted.contains("hunter2"), "{persisted}");
    let back = Config::from_template_with(&persisted, &env).unwrap();
    assert!(
        back.event_delivery
            .webhook_headers
            .contains_key("${env:SECRET}"),
        "the key is the literal text: {:?}",
        back.event_delivery.webhook_headers
    );
    // The export is escaped the same way.
    let exported = cfg.to_canonical_yaml().unwrap();
    assert!(exported.contains("$${env:SECRET}"), "{exported}");
}

/// The last gate: a config whose text would hold a reference that the file
/// does not have there is not written.
#[test]
fn the_persist_gate_fails_closed_on_an_unrecorded_reference() {
    let mut cfg =
        Config::from_template_with("advanced:\n  public_url: https://h/${env:P}\n", &|n| {
            (n == "P").then(|| "token-0000".to_string())
        })
        .unwrap();
    // A string that holds the reference text, put in the config the way a
    // writer that skipped the escape would.
    cfg.env_refs.rendered = true;
    cfg.config_sync_object_key = Some("x-${env:P}".into());
    let err = cfg
        .to_canonical_yaml_for_persist_with(&|_| None)
        .expect_err("refused");
    assert!(
        err.to_string().contains("refusing to write the config"),
        "{err}"
    );
}
