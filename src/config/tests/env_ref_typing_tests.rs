// SPDX-License-Identifier: BUSL-1.1

//! ── Env-ref round-trip tests ────────────────────────────────────────────
//!
//! The IaC workflow this guards: provision a secret-free template with
//! `${env:NAME}` refs → boot (refs expand, provenance recorded) → tweak
//! via the GUI (persist re-emits refs, not secrets) → export (refs
//! survive redaction) → put the export back into IaC.
//! An env value that YAML would re-type (all digits, `true`, `null`,
//! `1e5`, `0x…`) must reach a string field as a string, from a hand-written
//! unquoted ref and after persist → reload. A bool field fed by a ref keeps
//! working.

use crate::config::*;

const AES: &str = "1234567890123456789012345678901234567890123456789012345678901234";

fn lookup(name: &str) -> Option<String> {
    match name {
        "AES" => Some(AES.into()),
        "TRUE_SECRET" => Some("true".into()),
        "NUMLIKE" => Some("1e5".into()),
        "QUOTEY" => Some(r#"a"b\c 'd' # e"#.into()),
        "PATH_STYLE" => Some("true".into()),
        _ => None,
    }
}

const FILE: &str = r#"
access:
  access_key_id: admin
  secret_access_key: ${env:TRUE_SECRET}
storage:
  default_backend: remote
  backends:
  - name: remote
    type: s3
    endpoint: "${env:QUOTEY}"
    region: '${env:NUMLIKE}'
    force_path_style: ${env:PATH_STYLE:-false}   # a bool field
    access_key_id: ${env:NULLISH:-null}
    secret_access_key: ${env:AES}
  - name: local
    type: filesystem
    path: ./data
    encryption:
      mode: aes256-gcm-proxy
      key: ${env:AES}
"#;

fn load(text: &str) -> Config {
    let (expanded, refs) = expand_env_with_recording(text, lookup).unwrap();
    let mut cfg = Config::from_yaml_str(&expanded)
        .unwrap_or_else(|e| panic!("{e}\n--- expanded:\n{expanded}"));
    cfg.env_refs = refs;
    cfg
}

fn check(cfg: &Config) {
    assert_eq!(cfg.secret_access_key.as_deref(), Some("true"));
    let remote = cfg.backends.iter().find(|b| b.name == "remote").unwrap();
    match &remote.backend {
        BackendConfig::S3 {
            endpoint,
            region,
            force_path_style,
            access_key_id,
            secret_access_key,
            ..
        } => {
            assert_eq!(endpoint.as_deref(), Some(r#"a"b\c 'd' # e"#));
            assert_eq!(region, "1e5");
            assert!(*force_path_style);
            assert_eq!(access_key_id, &None);
            assert_eq!(secret_access_key.as_deref(), Some(AES));
        }
        other => panic!("{other:?}"),
    }
    let local = cfg.backends.iter().find(|b| b.name == "local").unwrap();
    assert_eq!(local.encryption.primary_key(), Some(AES));
}

#[test]
fn a_hand_written_unquoted_ref_keeps_its_string_type() {
    // The old plain splice: the all-digit key read as a number.
    let raw = expand_env_with(FILE, lookup).unwrap();
    assert!(Config::from_yaml_str(&raw).is_err(), "precondition");
    check(&load(FILE));
}

#[test]
fn persisted_refs_reload_with_their_string_type() {
    let cfg = load(FILE);
    let persisted = cfg.to_canonical_yaml_for_persist_with(&|_| None).unwrap();
    assert!(persisted.contains("${env:AES}"), "{persisted}");
    assert!(!persisted.contains(AES), "{persisted}");
    check(&load(&persisted));
}
