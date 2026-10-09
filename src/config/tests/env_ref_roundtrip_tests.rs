// SPDX-License-Identifier: BUSL-1.1

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
"#;

fn lookup(name: &str) -> Option<String> {
    match name {
        "BOOTSTRAP_SECRET" => Some("boot-secret-123".into()),
        "CI_UPLOADER_SECRET" => Some("ci-secret-456".into()),
        "OIDC_CLIENT_SECRET" => Some("GOCSPX-oidc-789".into()),
        "REMOTE_KEY" => Some("AKREMOTE000000000000".into()),
        "REMOTE_SECRET" => Some("remote-secret-abc".into()),
        "LOCAL_AES_KEY" => {
            Some("00000000000000000000000000000000000000000000000000000000000000ff".into())
        }
        _ => None,
    }
}

fn load_template() -> Config {
    let (expanded, refs) = expand_env_with_recording(TEMPLATE, lookup).expect("template expands");
    let mut cfg = Config::from_yaml_str(&expanded).expect("expanded template parses");
    cfg.env_refs = refs;
    cfg.record_env_ref_paths();
    cfg
}

#[test]
fn recording_captures_resolved_refs_but_not_defaults() {
    let (_, refs) = expand_env_with_recording(TEMPLATE, lookup).unwrap();
    assert_eq!(refs.len(), 6, "six refs resolved from env: {refs:?}");
    assert_eq!(refs["CI_UPLOADER_SECRET"], "ci-secret-456");
    // UNSET_WITH_DEFAULT fell back to its default — not recorded
    // (re-emitting it as a bare ref would fail the next load).
    assert!(!refs.contains_key("UNSET_WITH_DEFAULT"));
}

#[test]
fn export_reemits_refs_and_never_the_secrets() {
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
    ] {
        assert!(exported.contains(r), "export must carry {r}:\n{exported}");
    }
    for s in [
        "boot-secret-123",
        "ci-secret-456",
        "GOCSPX-oidc-789",
        "remote-secret-abc",
        "00000000000000000000000000000000000000000000000000000000000000ff",
    ] {
        assert!(!exported.contains(s), "export must not leak {s}");
    }
    // And the export must still parse as a config document.
    Config::from_yaml_str(&exported.replace("${env:", "${noexpand:"))
        .expect("exported template re-parses");
}

#[test]
fn persist_reemits_refs_even_after_runtime_mutation() {
    let mut cfg = load_template();
    // Simulate a GUI tweak: a non-secret change via the admin API.
    cfg.max_object_size = 42 * 1024 * 1024;
    let dir = std::env::temp_dir().join(format!("dgp-envref-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("cfg.yaml");
    cfg.persist_to_file(path.to_str().unwrap()).unwrap();
    let persisted = std::fs::read_to_string(&path).unwrap();
    std::fs::remove_dir_all(&dir).ok();
    assert!(persisted.contains("max_object_size: 44040192"));
    assert!(
        persisted.contains("${env:CI_UPLOADER_SECRET}"),
        "persisted file keeps the user-secret ref:\n{persisted}"
    );
    assert!(persisted.contains("${env:LOCAL_AES_KEY}"));
    assert!(!persisted.contains("ci-secret-456"));
    // The persisted file must re-load against the same env (boot path).
    let (re_expanded, _) = expand_env_with_recording(&persisted, lookup).unwrap();
    let re = Config::from_yaml_str(&re_expanded).unwrap();
    assert_eq!(re.max_object_size, 44040192);
    assert_eq!(re.iam_users[0].secret_access_key, "ci-secret-456");
}

#[test]
fn collision_resolves_deterministically() {
    let mut cfg = Config {
        access_key_id: Some("same-value".into()),
        ..Default::default()
    };
    cfg.env_refs.insert("ZED".into(), "same-value".into());
    cfg.env_refs.insert("ALPHA".into(), "same-value".into());
    cfg.record_env_ref_paths();
    let out = cfg.with_env_refs_reinserted();
    assert_eq!(out.access_key_id.as_deref(), Some("${env:ALPHA}"));
}

#[test]
fn no_refs_is_a_plain_clone() {
    let cfg = Config::default();
    assert_eq!(cfg.with_env_refs_reinserted(), cfg);
}

/// S9: a secret-length ref value that the file uses in two fields must
/// not land on disk in plaintext. Both fields get the ref back.
#[test]
fn shared_secret_ref_is_reinserted_everywhere() {
    let mut cfg = Config {
        access_key_id: Some("AKIA-shared-secret-1".into()),
        secret_access_key: Some("AKIA-shared-secret-1".into()),
        ..Default::default()
    };
    cfg.env_refs
        .insert("SHARED".into(), "AKIA-shared-secret-1".into());
    let out = cfg.with_env_refs_reinserted();
    assert_eq!(out.access_key_id.as_deref(), Some("${env:SHARED}"));
    assert_eq!(out.secret_access_key.as_deref(), Some("${env:SHARED}"));
    let yaml = cfg.to_canonical_yaml_for_persist_with(&|_| None).unwrap();
    assert!(!yaml.contains("AKIA-shared-secret-1"), "{yaml}");
}

#[test]
fn ambiguous_value_in_two_fields_is_not_reinserted() {
    // A value that appears in TWO distinct fields is ambiguous — rewriting
    // either into ${env:NAME} would couple the unrelated field to that env
    // var. Both must stay materialized. (X-ray M55.)
    let mut cfg = Config {
        access_key_id: Some("hunter2".into()),
        secret_access_key: Some("hunter2".into()),
        ..Default::default()
    };
    cfg.env_refs.insert("SHARED".into(), "hunter2".into());
    cfg.record_env_ref_paths();
    let out = cfg.with_env_refs_reinserted();
    assert_eq!(
        out.access_key_id.as_deref(),
        Some("hunter2"),
        "ambiguous value must NOT be rewritten"
    );
    assert_eq!(out.secret_access_key.as_deref(), Some("hunter2"));

    // But a value appearing exactly ONCE is still re-inserted.
    let mut cfg2 = Config {
        access_key_id: Some("only-here".into()),
        ..Default::default()
    };
    cfg2.env_refs.insert("UNIQ".into(), "only-here".into());
    cfg2.record_env_ref_paths();
    assert_eq!(
        cfg2.with_env_refs_reinserted().access_key_id.as_deref(),
        Some("${env:UNIQ}")
    );
}

#[test]
fn section_echo_back_resolves_instead_of_clobbering() {
    // A GUI round-trip echoes the ref string a section GET emitted.
    // resolve_env_ref_scalars must restore the real secret from
    // provenance — NOT store the literal "${env:...}" string.
    let mut cfg = load_template();
    cfg.iam_users[0].secret_access_key = "${env:CI_UPLOADER_SECRET}".into();
    cfg.resolve_env_ref_scalars().unwrap();
    assert_eq!(cfg.iam_users[0].secret_access_key, "ci-secret-456");
    // And an operator-typed ref with a default resolves via the default
    // when neither provenance nor process env has it.
    cfg.access_key_id = Some("${env:DGP_DOES_NOT_EXIST_ANYWHERE:-fallback}".into());
    cfg.resolve_env_ref_scalars().unwrap();
    assert_eq!(cfg.access_key_id.as_deref(), Some("fallback"));
    // An unresolvable ref fails loudly.
    cfg.access_key_id = Some("${env:DGP_DOES_NOT_EXIST_ANYWHERE}".into());
    assert!(cfg.resolve_env_ref_scalars().is_err());
    // S7: a server env var the file never referenced does not resolve.
    cfg.access_key_id = Some("${env:HOME}".into());
    assert!(cfg.resolve_env_ref_scalars().is_err());
}

/// B037: a `${env:NAME:-default}` ref keeps its default through persist,
/// so the next boot without NAME still loads.
#[test]
fn a_defaulted_ref_keeps_its_default_through_persist() {
    let tpl = "advanced:\n  log_level: ${env:LOG_LEVEL:-info}\n";
    let set = |n: &str| (n == "LOG_LEVEL").then(|| "debug".to_string());
    let (exp, refs) = crate::config::expansion::expand_env_with_recording(tpl, set).unwrap();
    let mut cfg = Config::from_yaml_str(&exp).unwrap();
    cfg.env_refs = refs;
    cfg.record_env_ref_paths();
    let out = cfg.to_canonical_yaml_for_persist_with(&|_| None).unwrap();
    assert!(out.contains("${env:LOG_LEVEL:-info}"), "{out}");
    crate::config::expansion::expand_env_with_recording(&out, |_| None)
        .expect("the persisted file needs LOG_LEVEL at the next boot");
}

/// B084: a short env value is re-emitted only where the ref was. After the
/// GUI changed aws-dr's region (the field the ref filled), the value left
/// in env_refs must not turn the minio backend's own literal region into
/// `${env:DR_REGION}`.
#[test]
fn a_short_ref_is_not_reinserted_into_an_unrelated_field() {
    let tpl = "storage:\n  backends:\n  - name: aws-dr\n    type: s3\n    region: ${env:DR_REGION}\n  - name: minio\n    type: s3\n    endpoint: http://minio:9000\n    region: eu-central-1\n";
    let set = |n: &str| (n == "DR_REGION").then(|| "us-east-1".to_string());
    let (exp, refs) = crate::config::expansion::expand_env_with_recording(tpl, set).unwrap();
    let mut cfg = Config::from_yaml_str(&exp).unwrap();
    cfg.env_refs = refs;
    cfg.record_env_ref_paths();
    // The GUI edits: aws-dr moves to eu-west-1, minio to us-east-1.
    for b in &mut cfg.backends {
        if let BackendConfig::S3 { region, .. } = &mut b.backend {
            *region = if b.name == "aws-dr" {
                "eu-west-1"
            } else {
                "us-east-1"
            }
            .into();
        }
    }
    let out = cfg.with_env_refs_reinserted();
    for b in &out.backends {
        if let BackendConfig::S3 { region, .. } = &b.backend {
            assert!(!region.contains("${env:"), "{}: {region}", b.name);
        }
    }
    // Unedited, the ref goes back where it was.
    let mut cfg = Config::from_yaml_str(&exp).unwrap();
    cfg.env_refs = crate::config::expansion::expand_env_with_recording(tpl, set)
        .unwrap()
        .1;
    cfg.record_env_ref_paths();
    let out = cfg.with_env_refs_reinserted();
    let aws = out.backends.iter().find(|b| b.name == "aws-dr").unwrap();
    assert!(
        matches!(&aws.backend, BackendConfig::S3 { region, .. } if region == "${env:DR_REGION}")
    );
}

/// Review A2: a ref follows its field through a section write. The body
/// carries the ref text at the field's new place (here a renamed backend);
/// the path recorded at load went stale, so the persist wrote the literal.
#[test]
fn a_ref_follows_its_renamed_field_through_a_section_resolve() {
    let tpl = "storage:\n  backends:\n  - name: aws-dr\n    type: s3\n    region: ${env:DR_REGION}\n  - name: minio\n    type: s3\n    endpoint: http://minio:9000\n    region: eu-central-1\n";
    let set = |n: &str| (n == "DR_REGION").then(|| "us-east-1".to_string());
    let (exp, refs) = crate::config::expansion::expand_env_with_recording(tpl, set).unwrap();
    let mut old = Config::from_yaml_str(&exp).unwrap();
    old.env_refs = refs;
    old.record_env_ref_paths();
    // The body as the GUI sends it back: the section GET showed the ref
    // text, and the operator renamed the backend.
    let mut incoming = old.with_env_refs_reinserted();
    for b in &mut incoming.backends {
        if b.name == "aws-dr" {
            b.name = "aws-east".into();
        }
    }
    incoming.env_refs = old.env_refs.clone();
    incoming.resolve_env_ref_scalars().unwrap();
    let out = incoming.with_env_refs_reinserted();
    let region = |name: &str| match &out
        .backends
        .iter()
        .find(|b| b.name == name)
        .unwrap()
        .backend
    {
        BackendConfig::S3 { region, .. } => region.clone(),
        _ => unreachable!(),
    };
    assert_eq!(region("aws-east"), "${env:DR_REGION}");
    assert_eq!(region("minio"), "eu-central-1");
}

/// Review A3: a name written with different defaults at different sites
/// (or with one and without one) keeps no default. The first default of a
/// name went back at every site, so a secret written `${env:S}` came back
/// as `${env:S:-}` and loaded EMPTY when S was unset.
#[test]
fn a_name_with_two_defaults_keeps_neither() {
    let secret = "  secret_access_key: ${env:S}\n";
    let region = "    region: ${env:S:-}\n";
    let backends = |region: &str| {
        format!("storage:\n  backends:\n  - name: remote\n    type: s3\n    endpoint: http://minio:9000\n{region}")
    };
    let access = |secret: &str| format!("access:\n  access_key_id: admin\n{secret}");
    // Both orders: the defaulted site first, and the secret first.
    for tpl in [
        format!("{}{}", backends(region), access(secret)),
        format!("{}{}", access(secret), backends(region)),
    ] {
        let set = |n: &str| (n == "S").then(|| "sekret-value-000000000000001".to_string());
        let (exp, refs) = crate::config::expansion::expand_env_with_recording(&tpl, set).unwrap();
        let mut cfg = Config::from_yaml_str(&exp).unwrap();
        cfg.env_refs = refs;
        cfg.record_env_ref_paths();
        let out = cfg.to_canonical_yaml_for_persist_with(&|_| None).unwrap();
        assert!(
            !out.contains("${env:S:-"),
            "a site got another site's default:\n{out}"
        );
        assert!(
            crate::config::expansion::expand_env_with_recording(&out, |_| None).is_err(),
            "the secret must fail loud without S:\n{out}"
        );
    }
    // One default everywhere is kept (B037).
    let tpl = "advanced:\n  log_level: ${env:L:-info}\nconfig_sync_object_key: ${env:L:-info}\n";
    let (_, refs) =
        crate::config::expansion::expand_env_with_recording(tpl, |_| Some("debug".into())).unwrap();
    assert_eq!(refs.ref_text("L"), "${env:L:-info}");
}
