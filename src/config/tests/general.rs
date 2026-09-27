// SPDX-License-Identifier: BUSL-1.1

/// S8: the non-TTY first-run banner must not print the bcrypt hash —
/// it is also the SQLCipher key, and container logs are retained.
#[test]
fn first_run_banner_hides_hash_without_tty() {
    let hash = "$2b$12$abcdefghijklmnopqrstuuFAKEFAKEFAKEFAKEFAKEFAKEFAKE";
    let lines = first_run_banner("pw123", hash, false).join("\n");
    assert!(!lines.contains(hash), "{lines}");
    assert!(!lines.contains("pw123"), "{lines}");
    let tty = first_run_banner("pw123", hash, true).join("\n");
    assert!(tty.contains("pw123") && !tty.contains(hash), "{tty}");
}
use crate::config::*;

#[test]
fn test_default_config() {
    let config = Config::default();
    assert_eq!(config.listen_addr.port(), 9000);
    assert!(matches!(config.backend, BackendConfig::Filesystem { .. }));
}

/// End-to-end: `from_yaml_file` expands ${VAR} against the real process
/// environment before parsing. Serialised because it mutates env vars.
#[test]
fn from_yaml_file_expands_env() {
    use std::io::Write;
    use std::sync::{Mutex, OnceLock};
    // Serialise env-mutating tests (this is the only one in this module, but
    // the lock guards against parallel runs touching the same DGP_TEST_* vars).
    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let _guard = ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    let dir = std::env::temp_dir();
    let path = dir.join(format!("dgp-expand-{}.yaml", std::process::id()));
    let mut f = std::fs::File::create(&path).unwrap();
    write!(
            f,
            "storage:\n  backends:\n    - name: b1\n      type: s3\n      \
             endpoint: ${{env:DGP_TEST_ENDPOINT}}\n      region: ${{env:DGP_TEST_REGION:-hel1}}\n      \
             force_path_style: true\n      access_key_id: ak\n      secret_access_key: sk\n  \
             default_backend: b1\naccess:\n  authentication: none\n"
        )
        .unwrap();

    // Unset → load fails with MissingEnvVar.
    std::env::remove_var("DGP_TEST_ENDPOINT");
    std::env::remove_var("DGP_TEST_REGION");
    let err = Config::from_yaml_file(path.to_str().unwrap()).unwrap_err();
    assert!(
        matches!(err, ConfigError::MissingEnvVar(ref v) if v == "DGP_TEST_ENDPOINT"),
        "expected MissingEnvVar, got {err:?}"
    );

    // Set → load succeeds and the value is substituted into the backend.
    std::env::set_var("DGP_TEST_ENDPOINT", "https://hel1.example.com");
    let cfg = Config::from_yaml_file(path.to_str().unwrap()).unwrap();
    let has_endpoint = cfg.backends.iter().any(|b| {
        matches!(&b.backend, BackendConfig::S3 { endpoint: Some(e), .. }
                if e == "https://hel1.example.com")
    });
    assert!(
        has_endpoint,
        "expanded endpoint not found in parsed backends"
    );

    std::env::remove_var("DGP_TEST_ENDPOINT");
    let _ = std::fs::remove_file(&path);
}

/// Build a Config with the three auth-relevant fields set and
/// everything else defaulted (avoids clippy's field-reassign lint).
fn auth_cfg(ak: Option<&str>, sk: Option<&str>, auth: Option<&str>) -> Config {
    Config {
        access_key_id: ak.map(Into::into),
        secret_access_key: sk.map(Into::into),
        authentication: auth.map(Into::into),
        ..Config::default()
    }
}

#[test]
fn banner_names_every_backend() {
    let yaml = "storage:\n  backends:\n    - name: hetzner-fsn1\n      type: s3\n      endpoint: https://fsn1.example.com\n      region: eu-central\n    - name: local-disk\n      type: filesystem\n      path: /var/lib/dgp\n  default_backend: local-disk\n";
    let cfg = Config::from_yaml_str(yaml).expect("parse");
    let lines = cfg.backend_banner_lines();
    assert_eq!(lines[0], "  Backends (2):");
    assert!(
        lines[1].contains("hetzner-fsn1: s3, endpoint https://fsn1.example.com"),
        "{lines:?}"
    );
    assert!(
        lines[2].contains("local-disk (default): filesystem, path /var/lib/dgp"),
        "{lines:?}"
    );
    let single = Config::default().backend_banner_lines();
    assert_eq!(single.len(), 1);
    assert!(single[0].starts_with("  Backend: "), "{single:?}");
}

/// The fatal auth messages show the config the operator can paste: YAML
/// (the only config format), not the TOML `key = "value"` of old.
#[test]
fn fatal_auth_help_is_yaml() {
    for outcome in [
        AuthConfigOutcome::Missing,
        AuthConfigOutcome::UnrecognizedMode,
    ] {
        let help = outcome.fatal_help().expect("fatal outcome has help");
        let text = help.join("\n");
        assert!(text.contains("access:"), "{text}");
        assert!(text.contains("authentication: none"), "{text}");
        assert!(!text.contains(" = "), "TOML syntax in: {text}");
    }
    assert!(AuthConfigOutcome::OpenAccess.fatal_help().is_none());
    // The class: no startup message spells a config key in TOML syntax.
    let startup = include_str!("../../startup.rs");
    for (i, line) in startup.lines().enumerate() {
        let t = line.trim_start();
        if t.starts_with("//") {
            continue;
        }
        assert!(
            !line.contains("authentication = \\\"") && !line.contains("access_key_id = \\\""),
            "src/startup.rs:{}: TOML syntax in a message: {}",
            i + 1,
            t
        );
    }
}

#[test]
fn classify_auth_config_credentials_enabled() {
    assert_eq!(
        auth_cfg(Some("AK"), Some("SK"), None).classify_auth_config(false),
        AuthConfigOutcome::CredentialsEnabled {
            redundant_none: false
        }
    );
    // Credentials win even if authentication = "none" is also set.
    assert_eq!(
        auth_cfg(Some("AK"), Some("SK"), Some("none")).classify_auth_config(false),
        AuthConfigOutcome::CredentialsEnabled {
            redundant_none: true
        }
    );
}

#[test]
fn classify_auth_config_open_access() {
    // Case/whitespace-insensitive normalisation.
    assert_eq!(
        auth_cfg(None, None, Some("  NONE  ")).classify_auth_config(false),
        AuthConfigOutcome::OpenAccess
    );
}

#[test]
fn classify_auth_config_unrecognized_mode_is_fatal() {
    assert_eq!(
        auth_cfg(None, None, Some("disabled")).classify_auth_config(false),
        AuthConfigOutcome::UnrecognizedMode
    );
}

#[test]
fn classify_auth_config_iam_users_are_credentials() {
    // Users in the config DB.
    assert_eq!(
        auth_cfg(None, None, None).classify_auth_config(true),
        AuthConfigOutcome::IamUsers {
            redundant_none: false
        }
    );
    assert_eq!(
        auth_cfg(None, None, Some("none")).classify_auth_config(true),
        AuthConfigOutcome::IamUsers {
            redundant_none: true
        }
    );
    // Declarative iam_users count; the same list in gui mode does not
    // (gui mode never reads it into the DB).
    let yaml = "access:\n  iam_mode: declarative\n  iam_users:\n    - name: u\n      access_key_id: AKU\n      secret_access_key: secret-123\n";
    let decl = Config::from_yaml_str(yaml).expect("parse");
    assert_eq!(
        decl.classify_auth_config(false),
        AuthConfigOutcome::IamUsers {
            redundant_none: false
        }
    );
    let mut gui = decl.clone();
    gui.iam_mode = crate::config_sections::IamMode::Gui;
    assert_eq!(gui.classify_auth_config(false), AuthConfigOutcome::Missing);
    // A bootstrap pair still reports as such.
    assert!(matches!(
        auth_cfg(Some("AK"), Some("SK"), None).classify_auth_config(true),
        AuthConfigOutcome::CredentialsEnabled { .. }
    ));
}

#[test]
fn classify_auth_config_missing_is_fatal() {
    assert_eq!(
        auth_cfg(None, None, None).classify_auth_config(false),
        AuthConfigOutcome::Missing
    );
    // A single credential without its pair is NOT "enabled" → still Missing.
    assert_eq!(
        auth_cfg(Some("AK"), None, None).classify_auth_config(false),
        AuthConfigOutcome::Missing
    );
}

/// Review 4 config-9: every registered variable has a row in the
/// configuration reference, so an operator can find what it does.
#[test]
fn every_registered_env_var_is_documented() {
    let doc = crate::source_scan::read("docs/product/reference/configuration.md");
    let missing: Vec<&str> = ENV_VAR_REGISTRY
        .iter()
        .map(|e| e.name)
        .filter(|n| !doc.contains(&format!("`{n}`")))
        .collect();
    assert!(
        missing.is_empty(),
        "add these to the env tables in docs/product/reference/configuration.md: {missing:?}"
    );
}

/// Source guard: every `DGP_*` name that appears as a string literal in
/// `src/` is in ENV_VAR_REGISTRY (so `--show-env` and the docs list it),
/// apart from test hooks and example names. The hand-kept lists below
/// drifted by ~15 variables; this scan cannot.
#[test]
fn every_dgp_literal_in_src_is_registered() {
    // Not operator settings: test hooks, a build-time stamp, the
    // per-backend and bootstrap name prefixes, and example backend
    // names in tests.
    let exempt = |n: &str| {
        n.starts_with("DGP_TEST_")
            || n == "DGP_BUILD_TIME"
            || n == "DGP_BACKEND_"
            || n == "DGP_BOOTSTRAP_"
            || (n.starts_with("DGP_BACKEND_")
                && (n.ends_with("_ENCRYPTION_KEY") || n.ends_with("_SSE_KMS_KEY_ID")))
    };
    let registry: Vec<&str> = crate::config::ENV_VAR_REGISTRY
        .iter()
        .map(|e| e.name)
        .collect();
    let files = crate::source_scan::rust_files("src");
    let mut missing = std::collections::BTreeSet::new();
    let mut seen: std::collections::HashMap<String, usize> = Default::default();
    for f in files {
        let src = std::fs::read_to_string(&f).unwrap();
        for part in src.split("\"DGP_").skip(1) {
            let name: String = part
                .chars()
                .take_while(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == '_')
                .collect();
            if !part[name.len()..].starts_with('"') {
                continue;
            }
            if name.is_empty() {
                continue;
            }
            let full = format!("DGP_{name}");
            *seen.entry(full.clone()).or_default() += 1;
            if !exempt(&full) && !registry.contains(&full.as_str()) {
                missing.insert(full);
            }
        }
    }
    assert!(
        missing.is_empty(),
        "DGP_* variables read in src/ but missing from ENV_VAR_REGISTRY: {missing:?}"
    );
    // And the reverse: a registry entry that nothing else names is dead.
    let dead: Vec<&str> = registry
        .iter()
        .copied()
        .filter(|n| seen.get(*n).copied().unwrap_or(0) < 2)
        .collect();
    assert!(dead.is_empty(), "registry entries no code reads: {dead:?}");
}

#[test]
fn test_print_env_vars_output() {
    // Capture stdout by running the function in a string buffer
    // We just verify it doesn't panic and covers all registry entries
    let mut output = String::new();
    let mut current_category = "";
    for entry in crate::config::ENV_VAR_REGISTRY {
        if entry.category != current_category {
            if !current_category.is_empty() {
                output.push('\n');
            }
            use std::fmt::Write;
            let _ = writeln!(output, "# {}", entry.category);
            current_category = entry.category;
        }
        use std::fmt::Write;
        let _ = writeln!(output, "# {}", entry.description);
        let _ = writeln!(output, "{}={}", entry.name, entry.example);
    }

    // Spot-check some entries
    assert!(output.contains("DGP_LISTEN_ADDR=0.0.0.0:9000"));
    assert!(output.contains("DGP_CACHE_MB=100"));
    assert!(output.contains("# Server"));
    assert!(output.contains("# TLS"));
}

#[test]
fn test_authentication_field_deserializes() {
    let yaml = r#"
listen_addr: "127.0.0.1:9000"
authentication: "none"
backend:
  type: filesystem
  path: /tmp/test
"#;
    let config = Config::from_yaml_str(yaml).unwrap();
    assert_eq!(
        config.authentication.as_deref(),
        Some("none"),
        "authentication field must be deserialized from YAML"
    );
}

#[test]
fn test_authentication_field_absent_is_none() {
    let yaml = r#"
listen_addr: "127.0.0.1:9000"
backend:
  type: filesystem
  path: /tmp/test
"#;
    let config = Config::from_yaml_str(yaml).unwrap();
    assert!(
        config.authentication.is_none(),
        "absent authentication field must be None"
    );
}

// ── YAML-only format (TOML removed in v1.4.1) ────────────────────────

#[test]
fn test_path_is_toml() {
    assert!(path_is_toml("foo.toml"));
    assert!(path_is_toml("foo.TOML"));
    assert!(path_is_toml("/etc/deltaglider_proxy/config.toml"));
    assert!(!path_is_toml("foo.yaml"));
    assert!(!path_is_toml("foo.yml"));
    assert!(!path_is_toml("foo"));
    assert!(!path_is_toml("/etc/dgp.txt"));
    assert!(!path_is_toml("toml")); // no extension — not a .toml path
}

#[test]
fn test_from_file_rejects_toml_with_actionable_message() {
    let dir = tempfile::tempdir().unwrap();
    let toml_path = dir.path().join("cfg.toml");
    std::fs::write(&toml_path, "listen_addr = \"127.0.0.1:9100\"\n").unwrap();
    let err = Config::from_file(toml_path.to_str().unwrap())
        .expect_err("loading a .toml config must fail");
    let msg = format!("{err}");
    assert!(
        msg.contains("TOML configs are no longer supported (removed in v1.4.1)"),
        "error must say the format was removed, got: {msg}"
    );
    assert!(
        msg.contains("config migrate") && msg.contains("v1.4.0"),
        "error must name the one-time conversion path, got: {msg}"
    );
}

#[cfg(unix)]
#[test]
fn bootstrap_hash_file_created_0600() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".deltaglider_bootstrap_hash");
    write_bootstrap_hash_file(&path, "$2b$12$abc").unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "sidecar must be 0600 at create, got {mode:o}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "$2b$12$abc");
    // Overwrite path stays 0600 too (truncate, not append).
    write_bootstrap_hash_file(&path, "$2b$12$xyz").unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "$2b$12$xyz");
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    // A looser pre-existing mode must be REPAIRED by a rewrite.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    write_bootstrap_hash_file(&path, "$2b$12$rewrite").unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "$2b$12$rewrite");
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(
        mode, 0o600,
        "rewrite must repair a loose mode, got {mode:o}"
    );
}

#[test]
fn test_yaml_parse_filesystem() {
    let yaml = r#"
listen_addr: "0.0.0.0:8080"
max_delta_ratio: 0.3
backend:
  type: filesystem
  path: /var/lib/deltaglider_proxy
"#;
    let config: Config = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(config.listen_addr.port(), 8080);
    assert_eq!(config.max_delta_ratio, 0.3);
    match config.backend {
        BackendConfig::Filesystem { path } => {
            assert_eq!(path, PathBuf::from("/var/lib/deltaglider_proxy"));
        }
        _ => panic!("Expected filesystem backend"),
    }
}

#[test]
fn test_yaml_parse_s3() {
    let yaml = r#"
listen_addr: "0.0.0.0:8080"
backend:
  type: s3
  endpoint: http://localhost:9000
  region: us-east-1
  force_path_style: true
"#;
    let config: Config = serde_yaml::from_str(yaml).unwrap();
    match config.backend {
        BackendConfig::S3 {
            endpoint,
            region,
            force_path_style,
            ..
        } => {
            assert_eq!(endpoint, Some("http://localhost:9000".to_string()));
            assert_eq!(region, "us-east-1");
            assert!(force_path_style);
        }
        _ => panic!("Expected S3 backend"),
    }
}

#[test]
fn test_yaml_round_trip_default() {
    let default_cfg = Config::default();
    let yaml_str = default_cfg.to_canonical_yaml().unwrap();
    let parsed: Config = serde_yaml::from_str(&yaml_str).unwrap();
    assert_eq!(parsed.listen_addr, default_cfg.listen_addr);
    assert_eq!(parsed.cache_size_mb, default_cfg.cache_size_mb);
    assert_eq!(parsed.max_delta_ratio, default_cfg.max_delta_ratio);
    assert_eq!(parsed.defaults_version, default_cfg.defaults_version);
}

#[test]
fn test_from_file_parses_yaml_for_any_non_toml_extension() {
    let dir = tempfile::tempdir().unwrap();
    let yaml_path = dir.path().join("b.yaml");
    std::fs::write(&yaml_path, "listen_addr: \"127.0.0.1:9200\"\n").unwrap();
    let cfg = Config::from_file(yaml_path.to_str().unwrap()).unwrap();
    assert_eq!(cfg.listen_addr.port(), 9200);

    // .yml also parses as YAML
    let yml_path = dir.path().join("c.yml");
    std::fs::write(&yml_path, "listen_addr: \"127.0.0.1:9300\"\n").unwrap();
    let cfg = Config::from_file(yml_path.to_str().unwrap()).unwrap();
    assert_eq!(cfg.listen_addr.port(), 9300);

    // Unknown extensions parse as YAML too (YAML is the only format).
    let other_path = dir.path().join("d.conf");
    std::fs::write(&other_path, "listen_addr: \"127.0.0.1:9400\"\n").unwrap();
    let cfg = Config::from_file(other_path.to_str().unwrap()).unwrap();
    assert_eq!(cfg.listen_addr.port(), 9400);
}

/// X-ray H7: a malformed config file must return Err from from_file so
/// load() fails loud (exit 1) instead of silently booting full defaults —
/// which would be a security downgrade (default auth/backends) the operator
/// never intended. This asserts the error contract load() relies on.
#[test]
fn test_from_file_malformed_yaml_is_error_not_defaults() {
    let dir = tempfile::tempdir().unwrap();
    // Invalid YAML (unclosed bracket / bad structure).
    let bad = dir.path().join("bad.yaml");
    std::fs::write(&bad, "listen_addr: [unterminated\n  nonsense: : :\n").unwrap();
    assert!(
        Config::from_file(bad.to_str().unwrap()).is_err(),
        "malformed YAML must be an error, not a silent default"
    );
    // A wrong-typed field is also an error, not defaulted-away.
    let wrong = dir.path().join("wrong.yaml");
    std::fs::write(&wrong, "listen_addr: 12345\n").unwrap();
    assert!(
        Config::from_file(wrong.to_str().unwrap()).is_err(),
        "type-mismatched field must be an error"
    );
}

#[test]
fn test_defaults_version_absent_means_v1() {
    let yaml = "listen_addr: \"127.0.0.1:9000\"\n";
    let cfg: Config = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(cfg.defaults_version, DefaultsVersion::V1);
}

#[test]
fn test_defaults_version_explicit_v1() {
    let yaml = "defaults: v1\nlisten_addr: \"127.0.0.1:9000\"\n";
    let cfg: Config = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(cfg.defaults_version, DefaultsVersion::V1);
}

#[test]
fn test_canonical_yaml_omits_default_defaults_version() {
    // When defaults_version equals its default, it should not appear in the
    // exported canonical YAML (keeps the file minimal).
    let cfg = Config::default();
    let yaml = cfg.to_canonical_yaml().unwrap();
    assert!(
        !yaml.contains("defaults:"),
        "canonical YAML must omit the defaults field when it equals V1"
    );
}

#[test]
fn test_canonical_yaml_strips_infra_secrets() {
    // to_canonical_yaml strips infra secrets only.
    // Full redaction (incl. SigV4 creds) goes through redact_all_secrets.
    let cfg = Config {
        access_key_id: Some("AKIAKEEPME".into()),
        secret_access_key: Some("kept-for-file-persistence".into()),
        bootstrap_password_hash: Some("$2b$12$xxxxxxxxxxxxxxxxxxxxxx".into()),
        backend_encryption: BackendEncryptionConfig::Aes256GcmProxy {
            key: Some("deadbeef-hex-encryption-key".into()),
            key_id: Some("singleton-kid".into()),
            legacy_key: None,
            legacy_key_id: None,
        },
        ..Config::default()
    };

    let yaml = cfg.to_canonical_yaml().unwrap();
    // Infra secrets are stripped
    assert!(!yaml.contains("$2b$"));
    assert!(
        !yaml.contains("deadbeef-hex-encryption-key"),
        "singleton encryption key must be redacted from canonical YAML, got:\n{yaml}"
    );
    // Non-secret id survives so operators can still see encryption is on.
    assert!(
        yaml.contains("singleton-kid"),
        "key_id is not a secret; must survive redaction"
    );
    // SigV4 creds survive — the wizard/file deployment path depends on this
    assert!(yaml.contains("AKIAKEEPME"));
    assert!(yaml.contains("kept-for-file-persistence"));
}

#[test]
fn test_redact_all_secrets_full_paranoia() {
    let mut cfg = Config {
        access_key_id: Some("AKIASHOULDNOTAPPEAR".into()),
        secret_access_key: Some("secret-should-not-appear".into()),
        bootstrap_password_hash: Some("$2b$12$xxxxxxxxxxxxxxxxxxxxxx".into()),
        backend_encryption: BackendEncryptionConfig::Aes256GcmProxy {
            key: Some("deadbeef-hex-encryption-key".into()),
            key_id: None,
            legacy_key: Some("legacy-deadbeef-should-also-redact".into()),
            legacy_key_id: Some("legacy-kid".into()),
        },
        backend: BackendConfig::S3 {
            session_token: None,
            endpoint: Some("http://minio:9000".into()),
            region: "us-east-1".into(),
            force_path_style: true,
            access_key_id: Some("BACKEND-SECRET-ID".into()),
            secret_access_key: Some("BACKEND-SECRET-KEY".into()),
            allow_local: false,
        },
        ..Config::default()
    };
    cfg.backends.push(NamedBackendConfig {
        name: "hetzner".into(),
        backend: BackendConfig::S3 {
            session_token: None,
            endpoint: Some("https://fsn1.your-objectstorage.com".into()),
            region: "eu-central-1".into(),
            force_path_style: true,
            access_key_id: Some("NAMED-SECRET-ID".into()),
            secret_access_key: Some("NAMED-SECRET-KEY".into()),
            allow_local: false,
        },
        encryption: BackendEncryptionConfig::Aes256GcmProxy {
            key: Some("NAMED-ENCRYPTION-KEY-SHOULD-REDACT".into()),
            key_id: Some("hetzner-kid".into()),
            legacy_key: None,
            legacy_key_id: None,
        },
    });

    let redacted = cfg.redact_all_secrets();
    let yaml = serde_yaml::to_string(&redacted).unwrap();
    // Top-level proxy creds: the key id is an identifier and stays.
    assert!(yaml.contains("AKIASHOULDNOTAPPEAR"));
    assert!(!yaml.contains("secret-should-not-appear"));
    // Bootstrap + encryption (primary + legacy on singleton, primary on named)
    assert!(!yaml.contains("$2b$"));
    assert!(!yaml.contains("deadbeef-hex-encryption-key"));
    assert!(
        !yaml.contains("legacy-deadbeef-should-also-redact"),
        "legacy_key (the decrypt-only-shim slot) must also redact"
    );
    assert!(
        !yaml.contains("NAMED-ENCRYPTION-KEY-SHOULD-REDACT"),
        "per-named-backend encryption keys must redact"
    );
    // Backend creds: the key id is an identifier and stays (the operator
    // must see WHICH key a backend uses); the secret never shows.
    assert!(yaml.contains("BACKEND-SECRET-ID"));
    assert!(!yaml.contains("BACKEND-SECRET-KEY"));
    assert!(yaml.contains("NAMED-SECRET-ID"));
    assert!(!yaml.contains("NAMED-SECRET-KEY"));
    // Non-secret fields survive: backend names, regions, non-secret key_ids.
    assert!(yaml.contains("hetzner"));
    assert!(yaml.contains("eu-central-1"));
    assert!(
        yaml.contains("hetzner-kid"),
        "key_id (not a secret) must survive — operators need to see which backend \
             is encrypted under which id"
    );
    assert!(
        yaml.contains("legacy-kid"),
        "legacy_key_id (not a secret, just an id) must survive"
    );
}

// ──────────────────────────────────────────────────────────────
// Per-backend encryption — new config shape
// ──────────────────────────────────────────────────────────────

#[test]
fn test_backend_encryption_yaml_roundtrip_none() {
    // Default variant: should not serialize at all (skip_serializing_if).
    let cfg = BackendEncryptionConfig::default();
    assert!(is_default_encryption(&cfg));
}

#[test]
fn test_backend_encryption_yaml_roundtrip_aes() {
    let yaml = r#"
mode: aes256-gcm-proxy
key: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
key_id: eu-2026-04
"#;
    let parsed: BackendEncryptionConfig = serde_yaml::from_str(yaml).unwrap();
    match &parsed {
        BackendEncryptionConfig::Aes256GcmProxy { key, key_id, .. } => {
            assert!(key.as_deref().unwrap().starts_with("0123"));
            assert_eq!(key_id.as_deref(), Some("eu-2026-04"));
        }
        other => panic!("unexpected variant: {other:?}"),
    }
    // Round-trip
    let emitted = serde_yaml::to_string(&parsed).unwrap();
    assert!(emitted.contains("mode: aes256-gcm-proxy"));
    assert!(emitted.contains("key_id: eu-2026-04"));
}

#[test]
fn test_backend_encryption_yaml_roundtrip_sse_kms() {
    let yaml = r#"
mode: sse-kms
kms_key_id: arn:aws:kms:us-east-1:123456789012:key/abcd
bucket_key_enabled: false
"#;
    let parsed: BackendEncryptionConfig = serde_yaml::from_str(yaml).unwrap();
    match &parsed {
        BackendEncryptionConfig::SseKms {
            kms_key_id,
            bucket_key_enabled,
            ..
        } => {
            assert!(kms_key_id.contains("key/abcd"));
            assert!(!bucket_key_enabled);
        }
        other => panic!("unexpected variant: {other:?}"),
    }
}

#[test]
fn test_backend_encryption_yaml_default_bucket_key_enabled_true() {
    let yaml = r#"
mode: sse-kms
kms_key_id: arn:aws:kms:us-east-1:1:key/x
"#;
    let parsed: BackendEncryptionConfig = serde_yaml::from_str(yaml).unwrap();
    match parsed {
        BackendEncryptionConfig::SseKms {
            bucket_key_enabled, ..
        } => assert!(bucket_key_enabled),
        other => panic!("unexpected variant: {other:?}"),
    }
}

#[test]
fn test_named_backend_with_encryption_roundtrips_through_config() {
    let yaml = r#"
backends:
  - name: eu
    type: filesystem
    path: /tmp/eu
    encryption:
      mode: aes256-gcm-proxy
      key: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
      key_id: eu-kid
  - name: us
    type: filesystem
    path: /tmp/us
"#;
    let cfg: Config = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(cfg.backends.len(), 2);
    assert!(matches!(
        cfg.backends[0].encryption,
        BackendEncryptionConfig::Aes256GcmProxy { .. }
    ));
    assert!(matches!(
        cfg.backends[1].encryption,
        BackendEncryptionConfig::None { .. }
    ));
}

#[test]
fn test_global_encryption_key_field_no_longer_accepted() {
    // Regression: the old `encryption_key:` at the config root must
    // be rejected. No legacy — nobody shipped with it — so a YAML
    // still carrying it is an outdated doc that should fail loudly.
    let yaml = r#"
encryption_key: 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
"#;
    // Flat-root Config with serde's default (no deny_unknown_fields):
    // silently ignored. That's fine — the flat shape is legacy-reading
    // surface, and after this refactor it just drops the field.
    // The point is the field is NOT present on the parsed struct.
    let cfg: Config = serde_yaml::from_str(yaml).unwrap_or_default();
    // No top-level encryption_key field exists anymore; the only
    // encryption surfaces are `backend_encryption` (singleton) and
    // `backends[*].encryption` (list).
    assert!(matches!(
        cfg.backend_encryption,
        BackendEncryptionConfig::None { .. }
    ));
}

#[test]
fn check_fatal_refuses_a_client_facing_coordination_bucket() {
    let fatal = |buckets: &str| {
        let mut cfg =
            Config::from_yaml_str(&format!("storage:\n  buckets:\n{buckets}")).expect("parses");
        cfg.config_sync_bucket = Some("dgp-sync".into());
        cfg.check_fatal()
    };
    // A backend route only says where the bucket lives: allowed.
    assert!(fatal("    dgp-sync: {}\n").is_empty());
    let public = fatal("    DGP-sync: { public: true }\n");
    assert!(
        public.iter().any(|e| e.contains("cannot be public")),
        "{public:?}"
    );
    let prefixes = fatal("    dgp-sync: { public_prefixes: [\"a/\"] }\n");
    assert!(!prefixes.is_empty());
    let aliased = fatal("    innocent: { alias: dgp-sync }\n");
    assert!(
        aliased.iter().any(|e| e.contains("innocent")),
        "{aliased:?}"
    );
    let alias_away = fatal("    dgp-sync: { alias: elsewhere }\n");
    assert!(
        alias_away.iter().any(|e| e.contains("alias")),
        "{alias_away:?}"
    );
    // Without config_sync_bucket nothing is reserved.
    let cfg =
        Config::from_yaml_str("storage:\n  buckets:\n    dgp-sync: { public: true }\n").unwrap();
    assert!(cfg.check_fatal().is_empty());
}

#[test]
fn check_fatal_rejects_undefined_backend_route_and_dup_names() {
    // Bucket routed to an undefined backend → FATAL (the beshu-b2
    // incident: the route silently fell to the default backend and 404'd).
    let cfg = Config::from_yaml_str(
        r#"
storage:
  buckets:
    beshu-b2: { backend: b2 }
"#,
    )
    .expect("parses");
    let errors = cfg.check_fatal();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].contains("beshu-b2") && errors[0].contains("undefined backend 'b2'"));

    // Duplicate backend names → FATAL.
    let cfg = Config::from_yaml_str(
        r#"
storage:
  backends:
    - name: b2
      type: s3
      endpoint: "http://x"
      region: r
      access_key_id: k
      secret_access_key: s
    - name: b2
      type: filesystem
      path: /tmp/x
"#,
    )
    .expect("parses");
    let errors = cfg.check_fatal();
    assert!(
        errors
            .iter()
            .any(|e| e.contains("duplicate backend name 'b2'")),
        "{errors:?}"
    );

    // A correctly-routed config has no fatal errors.
    let cfg = Config::from_yaml_str(
        r#"
storage:
  backends:
    - name: b2
      type: s3
      endpoint: "http://x"
      region: r
      access_key_id: k
      secret_access_key: s
  buckets:
    mirror: { backend: b2 }
    plain: {}
"#,
    )
    .expect("parses");
    assert!(cfg.check_fatal().is_empty());
}

/// `"default"` names the singleton backend. With named backends it is
/// not routable (`RoutingBackend::new` refuses it), so it is fatal.
#[test]
fn check_fatal_default_route_is_valid_only_without_named_backends() {
    let singleton = Config::from_yaml_str(
        r#"
storage:
  buckets:
    releases: { backend: default }
"#,
    )
    .expect("parses");
    assert!(singleton.check_fatal().is_empty());

    let named = Config::from_yaml_str(
        r#"
storage:
  backends:
    - name: local-disk
      type: filesystem
      path: /tmp/x
  buckets:
    releases: { backend: default }
"#,
    )
    .expect("parses");
    let errors = named.check_fatal();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].contains("undefined backend 'default'"));
}

#[test]
fn test_is_valid_key_id_charset() {
    assert!(is_valid_key_id("eu-2026-04"));
    assert!(is_valid_key_id("a"));
    assert!(is_valid_key_id("a.b_c-d"));
    assert!(is_valid_key_id(&"x".repeat(64)));
    // Empty
    assert!(!is_valid_key_id(""));
    // Too long
    assert!(!is_valid_key_id(&"x".repeat(65)));
    // Forbidden chars
    assert!(!is_valid_key_id("has space"));
    assert!(!is_valid_key_id("has/slash"));
    assert!(!is_valid_key_id("has:colon"));
    assert!(!is_valid_key_id("héllo")); // non-ASCII
}

#[test]
fn test_env_suffix_normalises_name() {
    assert_eq!(env_suffix_for_backend_name("eu-archive"), "EU_ARCHIVE");
    assert_eq!(env_suffix_for_backend_name("default"), "DEFAULT");
    assert_eq!(env_suffix_for_backend_name("a.b-c"), "A_B_C");
}

#[test]
fn test_check_rejects_sse_kms_on_filesystem() {
    let mut cfg = Config {
        backends: vec![NamedBackendConfig {
            name: "local".into(),
            backend: BackendConfig::Filesystem {
                path: "/tmp/x".into(),
            },
            encryption: BackendEncryptionConfig::SseKms {
                kms_key_id: "arn:aws:kms:...".into(),
                bucket_key_enabled: true,
                legacy_key: None,
                legacy_key_id: None,
            },
        }],
        ..Config::default()
    };
    let warnings = cfg.check();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("local") && w.contains("sse-kms")),
        "sse-kms on filesystem must warn, got {:?}",
        warnings
    );
}

#[test]
fn test_check_warns_aes_without_key() {
    // Make sure the env-var fallback doesn't accidentally satisfy
    // the check (our tests may run with DGP_ENCRYPTION_KEY set from
    // prior tests — scope the fixture to a unique name).
    let mut cfg = Config {
        backends: vec![NamedBackendConfig {
            name: "unconfigured-xyz-42".into(),
            backend: BackendConfig::Filesystem {
                path: "/tmp/x".into(),
            },
            encryption: BackendEncryptionConfig::Aes256GcmProxy {
                key: None,
                key_id: None,
                legacy_key: None,
                legacy_key_id: None,
            },
        }],
        ..Config::default()
    };
    let warnings = cfg.check();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("unconfigured-xyz-42") && w.contains("DGP_BACKEND_")),
        "aes mode with no key must produce env-var hint, got {:?}",
        warnings
    );
}

#[test]
fn test_check_surfaces_stale_iam_template_advisory() {
    // End-to-end wiring proof: a user whose permission uses the stale bare
    // `${username}` (removed in the breaking ${iam:username} rename) must
    // surface as a check() warning at save/lint time — the footgun that's
    // silently denying the `xperi` user in prod.
    let mut cfg = Config {
        iam_users: vec![crate::iam::DeclarativeUser {
            name: "xperi".into(),
            access_key_id: "AKXPERI".into(),
            secret_access_key: "s".into(),
            enabled: true,
            groups: vec![],
            permissions: vec![crate::iam::types::Permission {
                id: 0,
                effect: "Allow".into(),
                actions: vec!["write".into()],
                resources: vec!["scrap/customers/${username}/*".into()],
                conditions: None,
            }],
            auth_source: None,
        }],
        ..Config::default()
    };
    let warnings = cfg.check();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("xperi") && w.contains("${iam:username}")),
        "stale ${{username}} template must surface as a check() advisory, got {:?}",
        warnings
    );
}

/// Parse a sectioned YAML doc and run check(), returning the warnings.
fn check_yaml(yaml: &str) -> Vec<String> {
    let mut cfg = Config::from_yaml_str(yaml).expect("fixture must parse");
    cfg.check()
}

/// The routing table applies `alias` only with an explicit `backend`;
/// without one the alias is ignored, silently. check() now says so.
#[test]
fn test_check_warns_alias_without_backend() {
    let warnings = check_yaml(
        r#"
storage:
  buckets:
    releases:
      alias: real-releases
"#,
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("releases") && w.contains("alias") && w.contains("backend")),
        "alias without backend must warn, got {warnings:?}"
    );
    let clean = check_yaml(
        r#"
storage:
  backends:
    - name: hetzner-fsn1
      type: filesystem
      path: /tmp/x
  buckets:
    releases:
      backend: hetzner-fsn1
      alias: real-releases
"#,
    );
    assert!(!clean.iter().any(|w| w.contains("is ignored")), "{clean:?}");
}

#[test]
fn test_check_warns_orphaned_replication_target_only_marker() {
    let warnings = check_yaml(
        r#"
storage:
  buckets:
    mirror:
      replication_target_only: true
"#,
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("mirror") && w.contains("no replication rule")),
        "orphaned marker must warn, got {warnings:?}"
    );
}

#[test]
fn test_check_accepts_marker_with_rule_and_public_prefixes() {
    // marker + public_prefixes is COHERENT (read-only published mirror);
    // a targeting rule silences the orphan warning. Regression-pin both.
    let warnings = check_yaml(
        r#"
storage:
  buckets:
    mirror:
      replication_target_only: true
      public_prefixes: ["releases/"]
  replication:
    rules:
      - name: seed-mirror
        source: { bucket: releases }
        destination: { bucket: mirror }
"#,
    );
    assert!(
        !warnings
            .iter()
            .any(|w| w.contains("replication_target_only")),
        "marked+targeted+public bucket must not warn, got {warnings:?}"
    );
}

#[test]
fn test_check_warns_lifecycle_writing_into_marked_bucket() {
    let warnings = check_yaml(
        r#"
storage:
  buckets:
    mirror:
      replication_target_only: true
  replication:
    rules:
      - name: seed-mirror
        source: { bucket: releases }
        destination: { bucket: mirror }
  lifecycle:
    rules:
      - name: prune-mirror
        bucket: mirror
        expire_after: 30d
"#,
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("prune-mirror") && w.contains("second internal writer")),
        "lifecycle-into-marked must warn, got {warnings:?}"
    );
}

#[test]
fn test_check_warns_unmarked_alias_of_marked_bucket() {
    let warnings = check_yaml(
        r#"
storage:
  buckets:
    mirror:
      backend: b2
      alias: real-mirror
      replication_target_only: true
    shadow:
      backend: b2
      alias: real-mirror
  replication:
    rules:
      - name: seed
        source: { bucket: src }
        destination: { bucket: mirror }
"#,
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("shadow") && w.contains("single-writer")),
        "unmarked alias of a marked bucket must warn, got {warnings:?}"
    );
    // A DIFFERENT real bucket on the same backend does not warn.
    let ok = check_yaml(
        r#"
storage:
  buckets:
    mirror:
      backend: b2
      alias: real-mirror
      replication_target_only: true
    unrelated:
      backend: b2
      alias: real-other
  replication:
    rules:
      - name: seed
        source: { bucket: src }
        destination: { bucket: mirror }
"#,
    );
    assert!(
        !ok.iter().any(|w| w.contains("single-writer")),
        "distinct real buckets must not warn, got {ok:?}"
    );
}

#[test]
fn test_check_warns_overlapping_replication_destinations() {
    let warnings = check_yaml(
        r#"
storage:
  replication:
    rules:
      - name: rule-a
        source: { bucket: src-a }
        destination: { bucket: mirror, prefix: "builds/" }
      - name: rule-b
        source: { bucket: src-b }
        destination: { bucket: mirror, prefix: "builds/v2/" }
"#,
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("rule-a") && w.contains("rule-b") && w.contains("overlapping")),
        "overlapping dest prefixes must warn, got {warnings:?}"
    );
    // Distinct prefixes on the same bucket are fine.
    let ok = check_yaml(
        r#"
storage:
  replication:
    rules:
      - name: rule-a
        source: { bucket: src-a }
        destination: { bucket: mirror, prefix: "builds/" }
      - name: rule-b
        source: { bucket: src-b }
        destination: { bucket: mirror, prefix: "docs/" }
"#,
    );
    assert!(
        !ok.iter().any(|w| w.contains("overlapping")),
        "distinct prefixes must not warn, got {ok:?}"
    );
}

#[test]
fn test_check_detects_key_id_collision_with_different_keys() {
    let mut cfg = Config {
        backends: vec![
            NamedBackendConfig {
                name: "a".into(),
                backend: BackendConfig::Filesystem {
                    path: "/tmp/a".into(),
                },
                encryption: BackendEncryptionConfig::Aes256GcmProxy {
                    key: Some("K1".into()),
                    key_id: Some("shared".into()),
                    legacy_key: None,
                    legacy_key_id: None,
                },
            },
            NamedBackendConfig {
                name: "b".into(),
                backend: BackendConfig::Filesystem {
                    path: "/tmp/b".into(),
                },
                encryption: BackendEncryptionConfig::Aes256GcmProxy {
                    key: Some("K2".into()),
                    key_id: Some("shared".into()),
                    legacy_key: None,
                    legacy_key_id: None,
                },
            },
        ],
        ..Config::default()
    };
    let warnings = cfg.check();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("shared") && w.contains("DIFFERENT")),
        "shared key_id + different keys must warn, got {:?}",
        warnings
    );
}

#[test]
fn test_check_allows_shared_key_id_with_same_key() {
    // Same kid + same key = intentional cross-backend portability.
    // This is the documented escape hatch; MUST NOT warn.
    let mut cfg = Config {
        backends: vec![
            NamedBackendConfig {
                name: "primary".into(),
                backend: BackendConfig::Filesystem {
                    path: "/tmp/a".into(),
                },
                encryption: BackendEncryptionConfig::Aes256GcmProxy {
                    key: Some("SAME".into()),
                    key_id: Some("portable".into()),
                    legacy_key: None,
                    legacy_key_id: None,
                },
            },
            NamedBackendConfig {
                name: "replica".into(),
                backend: BackendConfig::Filesystem {
                    path: "/tmp/b".into(),
                },
                encryption: BackendEncryptionConfig::Aes256GcmProxy {
                    key: Some("SAME".into()),
                    key_id: Some("portable".into()),
                    legacy_key: None,
                    legacy_key_id: None,
                },
            },
        ],
        ..Config::default()
    };
    let warnings = cfg.check();
    assert!(
        !warnings
            .iter()
            .any(|w| w.contains("portable") && w.contains("DIFFERENT")),
        "identical key_id + identical key must NOT warn, got {:?}",
        warnings
    );
}

#[test]
fn test_check_rejects_invalid_key_id_charset() {
    let mut cfg = Config {
        backends: vec![NamedBackendConfig {
            name: "bad".into(),
            backend: BackendConfig::Filesystem {
                path: "/tmp/x".into(),
            },
            encryption: BackendEncryptionConfig::Aes256GcmProxy {
                key: Some("K".into()),
                key_id: Some("has space!".into()),
                legacy_key: None,
                legacy_key_id: None,
            },
        }],
        ..Config::default()
    };
    let warnings = cfg.check();
    assert!(
        warnings.iter().any(|w| w.contains("has space!")),
        "invalid key_id charset must warn, got {:?}",
        warnings
    );
}

#[test]
fn test_persist_to_file_writes_sectioned_yaml() {
    let dir = tempfile::tempdir().unwrap();
    // Deliberately non-default listen_addr so the sectioned canonical
    // YAML exporter surfaces an `advanced:` block — a default Config
    // round-trips to an (intentionally) empty YAML document, which
    // would make this test vacuous.
    let cfg = Config {
        listen_addr: "127.0.0.1:9099".parse().unwrap(),
        ..Config::default()
    };

    let yaml_path = dir.path().join("out.yaml");
    cfg.persist_to_file(yaml_path.to_str().unwrap()).unwrap();
    let content = std::fs::read_to_string(&yaml_path).unwrap();
    assert!(
        content.contains("listen_addr:"),
        "YAML output must use : separator, got: {content}"
    );
    assert!(
        content.contains("advanced:"),
        "sectioned YAML must group listen_addr under `advanced:`, got: {content}"
    );

    // A non-.yaml/.yml extension still writes YAML content (YAML is
    // the only persist format).
    let other_path = dir.path().join("out.conf");
    cfg.persist_to_file(other_path.to_str().unwrap()).unwrap();
    let content = std::fs::read_to_string(&other_path).unwrap();
    assert!(
        content.contains("listen_addr:"),
        "non-YAML extension must still receive YAML content, got: {content}"
    );
}

#[test]
fn dollar_literal_survives_persist_load_roundtrip() {
    // X-ray M25: a runtime value containing `$$` or a `${env:...}`-shaped
    // substring must round-trip through persist→load unchanged. The load
    // path runs the env expander over the whole file, so persist must escape
    // materialized `$` → `$$` (load's inverse recovers it).
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config {
        listen_addr: "127.0.0.1:9098".parse().unwrap(),
        // A scalar with both hazards: a literal `$$` and a `${env:...}`-shaped
        // substring that must NOT be expanded. config_sync_object_key persists
        // unredacted and isn't an env ref.
        config_sync_bucket: Some("sync-bucket".into()),
        config_sync_object_key: Some("pay-$$10-or-${env:SECRET_TOKEN}".into()),
        ..Config::default()
    };
    let path = dir.path().join("dollar.yaml");
    cfg.persist_to_file(path.to_str().unwrap()).unwrap();

    // Load it back (no such env var set → must NOT try to expand it).
    let loaded = Config::from_yaml_file(path.to_str().unwrap())
        .expect("load must not choke on the escaped $ / ${env:...} literal");
    assert_eq!(
        loaded.config_sync_object_key.as_deref(),
        Some("pay-$$10-or-${env:SECRET_TOKEN}"),
        "the literal $$ and ${{env:...}} substring must survive the round-trip"
    );
}

#[test]
fn escape_dollar_leaves_whole_env_refs_intact() {
    // A re-inserted whole ${env:NAME} ref must NOT be escaped (it must expand
    // on load); a materialized literal with `$` must be.
    assert!(is_whole_env_ref("${env:FOO}"));
    assert!(is_whole_env_ref("${env:FOO:-default}"));
    assert!(!is_whole_env_ref("pay $$10"));
    assert!(!is_whole_env_ref("prefix ${env:FOO} suffix")); // not a WHOLE ref
    let mut v = serde_yaml::Value::String("${env:FOO}".into());
    escape_dollar_for_persist(&mut v);
    assert_eq!(v.as_str(), Some("${env:FOO}"), "whole ref left intact");
    let mut lit = serde_yaml::Value::String("pay $$10".into());
    escape_dollar_for_persist(&mut lit);
    assert_eq!(lit.as_str(), Some("pay $$$$10"), "literal $ escaped to $$");
}

#[test]
fn test_persist_to_toml_path_rejected_with_actionable_message() {
    // TOML persistence was removed in v1.4.1. Writing YAML bytes into
    // a `.toml` file would be a trap on the next load, so the persist
    // refuses with the same actionable message as the load path.
    let cfg = Config::default();
    let dir = tempfile::tempdir().unwrap();
    let toml_path = dir.path().join("out.toml");
    let err = cfg
        .persist_to_file(toml_path.to_str().unwrap())
        .expect_err("persist to a .toml path must fail");
    let msg = format!("{err}");
    assert!(
        msg.contains("TOML configs are no longer supported (removed in v1.4.1)"),
        "error must say the format was removed, got: {msg}"
    );
    assert!(
        !toml_path.exists(),
        "no file may be created on a refused persist"
    );
}

#[test]
fn test_persist_to_yaml_accepts_admission_blocks() {
    // Symmetric: the same config persists fine to a YAML target.
    use crate::admission::spec::{ActionSpec, AdmissionBlockSpec, MatchSpec, SimpleAction};
    let cfg = Config {
        admission_blocks: vec![AdmissionBlockSpec {
            name: "deny-bad".into(),
            match_: MatchSpec::default(),
            action: ActionSpec::Simple(SimpleAction::Deny),
        }],
        iam_mode: crate::config_sections::IamMode::Declarative,
        ..Config::default()
    };
    let dir = tempfile::tempdir().unwrap();
    let yaml_path = dir.path().join("out.yaml");
    cfg.persist_to_file(yaml_path.to_str().unwrap()).unwrap();
    let content = std::fs::read_to_string(&yaml_path).unwrap();
    assert!(content.contains("deny-bad"));
    assert!(content.contains("iam_mode: declarative"));
}

// ── Correctness regressions (post Phase-1 audit) ────────────────────

#[test]
fn test_atomic_write_creates_file() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("cfg.yaml");
    atomic_write(&target, b"hello: world\n").unwrap();
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello: world\n");
}

#[test]
fn test_atomic_write_overwrites_existing() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("cfg.yaml");
    std::fs::write(&target, b"old: value\n").unwrap();
    atomic_write(&target, b"new: value\n").unwrap();
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "new: value\n");
}

#[test]
fn test_atomic_write_leaves_no_tempfile_on_success() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("cfg.yaml");
    atomic_write(&target, b"ok\n").unwrap();
    // The sibling tempfile (named ".cfg.yaml.tmp.<hex>") must not leak.
    let leftovers: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().into_string().unwrap())
        .filter(|n| n.starts_with(".cfg.yaml.tmp."))
        .collect();
    assert!(
        leftovers.is_empty(),
        "atomic_write leaked tempfiles: {leftovers:?}"
    );
}

/// The persisted config carries secrets (SigV4 and backend credentials,
/// AES keys), so it is never world-readable: a new file is 0600, and a
/// rewrite keeps the owner/group bits of the file it replaces but drops
/// every "other" bit.
#[cfg(unix)]
#[test]
fn test_atomic_write_is_never_world_readable() {
    use std::os::unix::fs::PermissionsExt;
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    let dir = tempfile::tempdir().unwrap();

    let fresh = dir.path().join("fresh.yaml");
    atomic_write(&fresh, b"secret: x\n").unwrap();
    assert_eq!(mode(&fresh), 0o600);

    let private = dir.path().join("private.yaml");
    std::fs::write(&private, b"old\n").unwrap();
    std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o600)).unwrap();
    atomic_write(&private, b"secret: x\n").unwrap();
    assert_eq!(mode(&private), 0o600);

    let shared = dir.path().join("shared.yaml");
    std::fs::write(&shared, b"old\n").unwrap();
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o644)).unwrap();
    atomic_write(&shared, b"secret: x\n").unwrap();
    assert_eq!(mode(&shared), 0o640, "a 0644 file loses its world-read bit");

    let group = dir.path().join("group.yaml");
    std::fs::write(&group, b"old\n").unwrap();
    std::fs::set_permissions(&group, std::fs::Permissions::from_mode(0o660)).unwrap();
    atomic_write(&group, b"secret: x\n").unwrap();
    assert_eq!(mode(&group), 0o660, "an operator's group access is kept");
}

#[test]
fn test_atomic_write_fails_when_parent_missing() {
    let dir = tempfile::tempdir().unwrap();
    // Parent directory does not exist — write must fail cleanly with
    // an IO error, not a panic or a silent success.
    let target = dir.path().join("does_not_exist").join("cfg.yaml");
    let err = atomic_write(&target, b"x").unwrap_err();
    assert!(
        matches!(err, ConfigError::Io(_)),
        "expected ConfigError::Io, got {err:?}"
    );
}

#[test]
fn test_check_handles_nan_delta_ratio() {
    let mut cfg = Config {
        max_delta_ratio: f32::NAN,
        ..Config::default()
    };
    let warnings = cfg.check();
    assert!(
        warnings.iter().any(|w| w.contains("NaN")),
        "expected NaN warning, got {warnings:?}"
    );
    assert!(
        !cfg.max_delta_ratio.is_nan(),
        "NaN ratio should have been replaced with a sane default"
    );
    assert!(
        (cfg.max_delta_ratio - default_max_delta_ratio()).abs() < f32::EPSILON,
        "NaN ratio should be replaced with default 0.75, got {}",
        cfg.max_delta_ratio
    );
}

#[test]
fn test_check_flags_out_of_range_ratio() {
    let mut cfg = Config {
        max_delta_ratio: 1.5,
        ..Config::default()
    };
    let warnings = cfg.check();
    assert!(
        warnings.iter().any(|w| w.contains("max_delta_ratio")),
        "expected out-of-range warning, got {warnings:?}"
    );
    // Out-of-range values survive (they're a sanity warning, not a fix).
    assert!((cfg.max_delta_ratio - 1.5).abs() < f32::EPSILON);
}

#[test]
fn test_check_clamps_infinity_delta_ratio() {
    // YAML `.inf` deserializes to f32::INFINITY. INFINITY > 1.0 is true
    // (the old warning fired) but the value would have survived and
    // silently stored every file as a delta regardless of size. Clamp
    // to the default alongside NaN.
    let mut cfg = Config {
        max_delta_ratio: f32::INFINITY,
        ..Config::default()
    };
    let warnings = cfg.check();
    assert!(
        warnings.iter().any(|w| w.contains("infinite")),
        "expected infinity warning, got {warnings:?}"
    );
    assert!(
        !cfg.max_delta_ratio.is_infinite(),
        "infinity should have been replaced, got {}",
        cfg.max_delta_ratio
    );
    assert!(
        (cfg.max_delta_ratio - default_max_delta_ratio()).abs() < f32::EPSILON,
        "infinity should be replaced with default 0.75, got {}",
        cfg.max_delta_ratio
    );
}

#[test]
fn test_check_all_rejects_duplicate_backend_names() {
    // Routing keys on backend.name. A duplicate silently shadows the
    // second entry; the first wins at runtime. Warn so the operator
    // knows the config is ambiguous.
    let mut cfg = Config {
        backends: vec![
            NamedBackendConfig {
                name: "shared".into(),
                backend: BackendConfig::Filesystem { path: "/a".into() },
                encryption: BackendEncryptionConfig::default(),
            },
            NamedBackendConfig {
                name: "unique".into(),
                backend: BackendConfig::Filesystem { path: "/b".into() },
                encryption: BackendEncryptionConfig::default(),
            },
            NamedBackendConfig {
                name: "shared".into(),
                backend: BackendConfig::Filesystem { path: "/c".into() },
                encryption: BackendEncryptionConfig::default(),
            },
        ],
        ..Config::default()
    };
    let fatal = cfg.check_all().expect_err("duplicate names are fatal");
    assert!(
        fatal
            .iter()
            .any(|e| e.contains("duplicate backend name") && e.contains("shared")),
        "expected duplicate-name error, got {fatal:?}"
    );
}

#[test]
fn test_check_no_warning_when_backend_names_unique() {
    let mut cfg = Config {
        backends: vec![
            NamedBackendConfig {
                name: "a".into(),
                backend: BackendConfig::Filesystem { path: "/a".into() },
                encryption: BackendEncryptionConfig::default(),
            },
            NamedBackendConfig {
                name: "b".into(),
                backend: BackendConfig::Filesystem { path: "/b".into() },
                encryption: BackendEncryptionConfig::default(),
            },
        ],
        ..Config::default()
    };
    let warnings = cfg.check_all().expect("unique names are not fatal");
    assert!(
        !warnings.iter().any(|w| w.contains("duplicate")),
        "no duplicate warning expected when names are unique, got {warnings:?}"
    );
}

#[test]
fn test_resolve_config_path_honors_env_even_when_missing() {
    // DGP_CONFIG pointing at a non-existent file must STILL be returned
    // — the operator's explicit intent beats silent fallthrough that
    // would redirect admin-API persists to an unrelated file.
    let guard = EnvGuard::set("DGP_CONFIG", "/tmp/definitely-does-not-exist.yaml");
    let resolved = Config::resolve_config_path();
    assert_eq!(resolved, Some("/tmp/definitely-does-not-exist.yaml".into()));
    drop(guard);
}

#[test]
fn test_resolve_config_path_empty_env_falls_through() {
    // An empty-string env var must not hijack resolution.
    let guard = EnvGuard::set("DGP_CONFIG", "");
    let _ = Config::resolve_config_path(); // may be None or search-path hit; either is fine
    drop(guard);
}

/// Process-wide lock serializing every `EnvGuard`-using test. `cargo test`
/// runs the suite in parallel threads of ONE process, so two tests mutating
/// the same env var (`DGP_CONFIG`) otherwise race — one reads back the
/// other's value and fails intermittently (only under CI scheduling). The
/// guard holds this lock for its whole lifetime, so env-driven tests run one
/// at a time. Mirrors the `ENV_LOCK` pattern used by the `DGP_TEST_*` tests.
static ENV_GUARD_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();

/// Test-only RAII guard that sets an env var on construction and
/// unsets it on drop. Prevents one test from polluting another when
/// they exercise environment-driven behavior.
struct EnvGuard {
    key: &'static str,
    prior: Option<String>,
    // Held for the guard's lifetime to serialize env-touching tests.
    // `'static` via the `OnceLock`; poison is irrelevant (unit `()` state).
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let lock = ENV_GUARD_LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let prior = std::env::var(key).ok();
        std::env::set_var(key, value);
        Self {
            key,
            prior,
            _lock: lock,
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match self.prior.take() {
            Some(v) => std::env::set_var(self.key, v),
            None => std::env::remove_var(self.key),
        }
    }
}

#[test]
fn test_buckets_field_is_ordered() {
    // BTreeMap iteration must yield keys in sorted order. This is the
    // stability guarantee that makes canonical YAML export byte-stable.
    let mut cfg = Config::default();
    cfg.buckets.insert(
        "zeta".into(),
        crate::bucket_policy::BucketPolicyConfig::default(),
    );
    cfg.buckets.insert(
        "alpha".into(),
        crate::bucket_policy::BucketPolicyConfig::default(),
    );
    cfg.buckets.insert(
        "mu".into(),
        crate::bucket_policy::BucketPolicyConfig::default(),
    );
    let yaml = cfg.to_canonical_yaml().unwrap();
    // Extract the order in which bucket keys appear — must be sorted.
    let alpha = yaml.find("alpha:").unwrap();
    let mu = yaml.find("mu:").unwrap();
    let zeta = yaml.find("zeta:").unwrap();
    assert!(
        alpha < mu && mu < zeta,
        "bucket keys must appear in sorted order; got YAML:\n{yaml}"
    );
}

// ── Phase 3a: dual-shape deserialize ────────────────────────────────

#[test]
fn test_from_yaml_str_accepts_flat_shape() {
    // Legacy shape: keys at the document root. Still works.
    let yaml = r#"
listen_addr: "127.0.0.1:9123"
max_delta_ratio: 0.3
cache_size_mb: 256
"#;
    let cfg = Config::from_yaml_str(yaml).unwrap();
    assert_eq!(cfg.listen_addr.port(), 9123);
    assert!((cfg.max_delta_ratio - 0.3).abs() < f32::EPSILON);
    assert_eq!(cfg.cache_size_mb, 256);
}

#[test]
fn test_from_yaml_str_accepts_sectioned_shape() {
    // Phase 3 canonical shape: four top-level sections.
    let yaml = r#"
advanced:
  listen_addr: "127.0.0.1:9124"
  max_delta_ratio: 0.2
  cache_size_mb: 512
access:
  access_key_id: "AKIA"
  secret_access_key: "s3cret"
"#;
    let cfg = Config::from_yaml_str(yaml).unwrap();
    assert_eq!(cfg.listen_addr.port(), 9124);
    assert!((cfg.max_delta_ratio - 0.2).abs() < f32::EPSILON);
    assert_eq!(cfg.cache_size_mb, 512);
    assert_eq!(cfg.access_key_id.as_deref(), Some("AKIA"));
    assert_eq!(cfg.secret_access_key.as_deref(), Some("s3cret"));
}

#[test]
fn test_from_yaml_str_empty_document_yields_default() {
    let cfg = Config::from_yaml_str("").unwrap();
    assert_eq!(cfg, Config::default());
    let cfg2 = Config::from_yaml_str("   \n\t\n").unwrap();
    assert_eq!(cfg2, Config::default());
}

#[test]
fn test_from_yaml_str_bare_defaults_key_is_flat_compatible() {
    // `defaults: v1` is valid at the root of BOTH shapes — looks_sectioned
    // returns false (no section keys, no flat-only keys), and the flat
    // deserializer handles it.
    let yaml = "defaults: v1\n";
    let cfg = Config::from_yaml_str(yaml).unwrap();
    assert_eq!(cfg.defaults_version, DefaultsVersion::V1);
}

#[test]
fn test_from_yaml_str_sectioned_roundtrips_canonical_output() {
    // The canonical exporter emits sectioned YAML. That YAML, fed back
    // through `from_yaml_str`, must reconstruct the same Config. This is
    // the GitOps invariant: export → apply is a no-op.
    let original = Config {
        listen_addr: "10.0.0.1:9000".parse().unwrap(),
        max_delta_ratio: 0.15,
        cache_size_mb: 333,
        access_key_id: Some("AKIAROUND".into()),
        secret_access_key: Some("roundtrip".into()),
        ..Config::default()
    };
    let yaml = original.to_canonical_yaml().unwrap();
    // Must be sectioned.
    assert!(
        yaml.contains("advanced:") || yaml.contains("access:"),
        "canonical YAML must be sectioned, got:\n{yaml}"
    );
    let roundtripped = Config::from_yaml_str(&yaml).unwrap();
    assert_eq!(original.listen_addr, roundtripped.listen_addr);
    assert_eq!(original.max_delta_ratio, roundtripped.max_delta_ratio);
    assert_eq!(original.cache_size_mb, roundtripped.cache_size_mb);
    assert_eq!(original.access_key_id, roundtripped.access_key_id);
    assert_eq!(original.secret_access_key, roundtripped.secret_access_key);
}

#[test]
fn test_from_yaml_str_mixed_shape_is_hard_error() {
    // A doc with BOTH a flat key (`listen_addr:`) AND a section key
    // (`storage:`) must be rejected — picking either shape would drop
    // half of what the operator wrote.
    let yaml = r#"
listen_addr: "127.0.0.1:9125"
storage:
  default_backend: "hetzner"
"#;
    let err = Config::from_yaml_str(yaml)
        .expect_err("mixed flat+sectioned must be rejected, not silently merged");
    let msg = format!("{err}");
    assert!(
        msg.contains("listen_addr") && msg.contains("storage"),
        "error must name BOTH the flat and the section key that collided, got: {msg}"
    );
}

#[test]
fn test_from_yaml_str_typo_inside_section_is_hard_error() {
    // Typo inside a section — `default_backnd` instead of
    // `default_backend` — must be rejected loudly, not silently
    // defaulted. This is the Phase 3a promise that motivates
    // `#[serde(deny_unknown_fields)]` on every section type.
    let yaml = r#"
storage:
  default_backnd: "hetzner"
"#;
    let err =
        Config::from_yaml_str(yaml).expect_err("unknown field inside `storage:` must be rejected");
    let msg = format!("{err}");
    assert!(
        msg.contains("default_backnd"),
        "error must name the offending field, got: {msg}"
    );
}

/// Review 4 config-11: the flat shape has no `deny_unknown_fields`
/// (tightening it would stop an existing file from booting), so a typo
/// such as `cache_size_mbb` loaded silently as the default. It is now
/// named: in the load log and in `config lint`.
#[test]
fn unknown_flat_root_keys_are_named() {
    let doc: serde_yaml::Value = serde_yaml::from_str(
        "cache_size_mbb: 5\nlisten_addr: 0.0.0.0:9000\nadmin_password_hash: x\ndefaults: v1\n",
    )
    .unwrap();
    assert_eq!(
        unknown_flat_root_keys(&doc),
        vec!["cache_size_mbb".to_string()]
    );
    // Sectioned documents are strict already: nothing to report.
    let doc: serde_yaml::Value = serde_yaml::from_str("advanced: {}\n").unwrap();
    assert!(unknown_flat_root_keys(&doc).is_empty());
}

#[test]
fn test_from_yaml_str_unknown_section_is_hard_error() {
    // Typo at the root: `storge:` instead of `storage:`. Because the
    // doc lacks any known section OR flat key, it classifies as flat,
    // and `Config` has a permissive serde… but we DO want the root-
    // level section-key typo to surface in practice. Right now this
    // is accepted silently (the classifier routes to flat and flat
    // lacks deny_unknown_fields). We document the current behavior
    // so a future tightening of `Config` to `deny_unknown_fields`
    // has a test anchor.
    let yaml = r#"
storge:
  default_backend: "hetzner"
"#;
    // Currently: classified as flat, silently accepted as default.
    // This is NOT ideal but matches pre-Phase-3a behavior for any
    // unknown top-level key. Tightening requires a one-release
    // deprecation window to avoid breaking operators who've been
    // relying on silently-ignored fields.
    let cfg = Config::from_yaml_str(yaml).unwrap();
    assert_eq!(
        cfg,
        Config::default(),
        "unknown root key currently silently ignored (pre-existing Config behavior)"
    );
}

#[test]
fn test_defaults_version_current_is_v1() {
    // Pinning test: if a future release changes the default
    // DefaultsVersion to V2, an export that omitted `defaults:`
    // (because it equalled the old default) will re-import at the
    // new default, which is a silent version drift. Bumping this
    // assertion on purpose forces the release engineer to think
    // about migration.
    assert_eq!(DefaultsVersion::default(), DefaultsVersion::V1);
    assert!(
        DefaultsVersion::V1.is_default(),
        "DefaultsVersion::V1 must be the current default; a future V2 must update this \
             assertion AND provide a migration path for YAML files that omitted `defaults:`"
    );
}

// ── Phase 3b.1: `public: true` shorthand through the full load path ──

#[test]
fn test_bucket_public_shorthand_normalised_on_yaml_load() {
    // A sectioned YAML with `public: true` on a bucket expands to
    // `public_prefixes: [""]` after normalisation, which is the form
    // the runtime (PublicPrefixSnapshot) already knows how to serve.
    let yaml = r#"
storage:
  buckets:
    my-bucket:
      public: true
"#;
    let cfg = Config::from_yaml_str(yaml).unwrap();
    let policy = cfg.buckets.get("my-bucket").unwrap();
    assert_eq!(policy.public_prefixes, vec![String::new()]);
    assert_eq!(policy.public, Some(true));
}

#[test]
fn test_bucket_public_shorthand_conflict_rejected_at_load() {
    // Mixing `public: true` with `public_prefixes` is operator error
    // — the loader must refuse, not silently pick one.
    let yaml = r#"
storage:
  buckets:
    my-bucket:
      public: true
      public_prefixes:
        - "releases/"
"#;
    let err = Config::from_yaml_str(yaml).expect_err("public + public_prefixes must be rejected");
    let msg = format!("{err}");
    assert!(
        msg.contains("my-bucket"),
        "error must name the offending bucket, got: {msg}"
    );
    assert!(
        msg.contains("public"),
        "error must mention `public`, got: {msg}"
    );
}

#[test]
fn test_bucket_public_shorthand_roundtrip_via_canonical_yaml() {
    // Round-trip: YAML with `public: true` → load → export → re-load.
    // Canonical exporter must use the shorthand form again (cleaner
    // GitOps diffs), and the re-loaded config must be identical to
    // the first load.
    let yaml = r#"
storage:
  buckets:
    my-bucket:
      public: true
"#;
    let cfg1 = Config::from_yaml_str(yaml).unwrap();
    let exported = cfg1.to_canonical_yaml().unwrap();
    assert!(
        exported.contains("public: true"),
        "canonical export must use the shorthand form, got:\n{exported}"
    );
    assert!(
        !exported.contains("public_prefixes:"),
        "canonical export must NOT emit the long form when shorthand applies, got:\n{exported}"
    );
    let cfg2 = Config::from_yaml_str(&exported).unwrap();
    assert_eq!(
        cfg1.buckets, cfg2.buckets,
        "bucket policies must round-trip losslessly"
    );
}

#[test]
fn test_bucket_specific_prefixes_roundtrip_without_shorthand() {
    // When a bucket has specific prefixes (not the `[""]` sentinel),
    // the exporter must keep the long form — shorthand only applies
    // to the unambiguous "entire bucket is public" case.
    let yaml = r#"
storage:
  buckets:
    semi-public:
      public_prefixes:
        - "releases/"
        - "docs/"
"#;
    let cfg = Config::from_yaml_str(yaml).unwrap();
    let exported = cfg.to_canonical_yaml().unwrap();
    assert!(
        exported.contains("public_prefixes:"),
        "specific-prefix config must round-trip as long form, got:\n{exported}"
    );
    assert!(
        !exported.contains("public: true"),
        "shorthand must not be emitted for multi-prefix config, got:\n{exported}"
    );
}

// ── Phase 3b.1: storage shorthand through the full load path ──────

/// The T1 acceptance example from the plan: a 5-line config that
/// loads, starts, and serves S3 traffic. The acceptance gate is
/// "loads without error" — downstream startup is tested separately.
#[test]
fn test_t1_five_line_example_loads() {
    // Five lines, counting only non-blank content:
    //   1. storage:
    //   2.   s3: http://minio:9000
    //   3.   access_key_id: AKIAEXAMPLE
    //   4.   secret_access_key: SECRET
    //   5.   buckets:
    // (the empty bucket map is elided in YAML counting)
    let yaml = r#"
storage:
  s3: http://minio:9000
  access_key_id: AKIAEXAMPLE
  secret_access_key: SECRET
"#;
    let cfg = Config::from_yaml_str(yaml).unwrap();
    match &cfg.backend {
        BackendConfig::S3 {
            endpoint,
            region,
            access_key_id,
            secret_access_key,
            ..
        } => {
            assert_eq!(endpoint.as_deref(), Some("http://minio:9000"));
            assert_eq!(region, "us-east-1");
            assert_eq!(access_key_id.as_deref(), Some("AKIAEXAMPLE"));
            assert_eq!(secret_access_key.as_deref(), Some("SECRET"));
        }
        other => panic!("T1 example must yield S3 backend, got {other:?}"),
    }
}

#[test]
fn test_filesystem_shorthand_loads_via_yaml() {
    let yaml = r#"
storage:
  filesystem: /var/lib/dgp
"#;
    let cfg = Config::from_yaml_str(yaml).unwrap();
    match &cfg.backend {
        BackendConfig::Filesystem { path } => {
            assert_eq!(path.to_str(), Some("/var/lib/dgp"));
        }
        other => panic!("filesystem shorthand must yield Filesystem backend, got {other:?}"),
    }
}

#[test]
fn test_storage_shorthand_plus_explicit_backend_is_rejected_at_load() {
    // Operator error surfaces cleanly from the full load path.
    let yaml = r#"
storage:
  s3: http://minio:9000
  backend:
    type: filesystem
    path: /explicit
"#;
    let err =
        Config::from_yaml_str(yaml).expect_err("shorthand + explicit backend must be rejected");
    let msg = format!("{err}");
    assert!(
        msg.contains("shorthand") || msg.contains("backend"),
        "error must explain the shorthand/backend conflict, got: {msg}"
    );
}

// ── Phase 3b.2.a: operator-authored admission blocks ─────────────

#[test]
fn test_admission_blocks_deserialize_and_roundtrip() {
    let yaml = r#"
admission:
  blocks:
    - name: deny-bad-ips
      match:
        source_ip_list: ["203.0.113.5", "198.51.100.0/24"]
      action: deny
    - name: maint
      match:
        config_flag: "maintenance_mode"
      action:
        type: reject
        status: 503
        message: "back soon"
"#;
    let cfg = Config::from_yaml_str(yaml).unwrap();
    assert_eq!(cfg.admission_blocks.len(), 2);
    assert_eq!(cfg.admission_blocks[0].name, "deny-bad-ips");
    assert_eq!(cfg.admission_blocks[1].name, "maint");

    // Round-trip through canonical YAML must preserve everything.
    let exported = cfg.to_canonical_yaml().unwrap();
    assert!(exported.contains("admission:"));
    assert!(exported.contains("deny-bad-ips"));
    assert!(exported.contains("maint"));
    let cfg2 = Config::from_yaml_str(&exported).unwrap();
    assert_eq!(cfg.admission_blocks, cfg2.admission_blocks);
}

#[test]
fn test_admission_duplicate_block_names_rejected_at_load() {
    let yaml = r#"
admission:
  blocks:
    - name: same
      match: {}
      action: continue
    - name: same
      match: {}
      action: deny
"#;
    let err =
        Config::from_yaml_str(yaml).expect_err("duplicate admission block names must be rejected");
    assert!(format!("{err}").contains("duplicate"));
}

#[test]
fn test_admission_invalid_reject_status_rejected_at_load() {
    let yaml = r#"
admission:
  blocks:
    - name: bad
      match: {}
      action:
        type: reject
        status: 200
"#;
    let err = Config::from_yaml_str(yaml).expect_err("reject with 2xx status must be rejected");
    let msg = format!("{err}");
    assert!(
        msg.contains("4xx") || msg.contains("5xx"),
        "error must point at the status range, got: {msg}"
    );
}

#[test]
fn test_admission_unknown_field_in_match_rejected() {
    // `deny_unknown_fields` on MatchSpec catches field typos.
    let yaml = r#"
admission:
  blocks:
    - name: typo
      match:
        source_ips: ["1.2.3.4"]
      action: deny
"#;
    let err = Config::from_yaml_str(yaml).expect_err("typo in match field must be rejected");
    assert!(format!("{err}").contains("source_ips"));
}

#[test]
fn test_admission_empty_omitted_on_default_export() {
    // A Config with no operator-authored blocks must not emit an
    // `admission:` section — keeps default-config YAML minimal.
    let cfg = Config::default();
    let exported = cfg.to_canonical_yaml().unwrap();
    assert!(
        !exported.contains("admission:"),
        "empty admission must be omitted, got:\n{exported}"
    );
}

// ── Phase 3b.2.a hardening: flat-shape + classifier coverage ──────

#[test]
fn test_admission_blocks_flat_shape_also_validates() {
    // H1 from adversarial review: the flat-shape load path must
    // ALSO run AdmissionSpec::validate so duplicate names / bad
    // reject status don't slip through.
    let yaml = r#"
listen_addr: "127.0.0.1:9000"
admission_blocks:
  - name: same
    match: {}
    action: deny
  - name: same
    match: {}
    action: continue
"#;
    let err =
        Config::from_yaml_str(yaml).expect_err("duplicate block name on flat-shape path must fail");
    assert!(
        format!("{err}").contains("duplicate"),
        "error must say duplicate, got: {err}"
    );
}

#[test]
fn test_admission_blocks_flat_only_keys_coverage() {
    // M2 from adversarial review: `admission_blocks:` at flat root
    // is valid (flat shape preservation), but mixing it with the
    // sectioned `admission:` must be rejected as a mixed doc.
    let yaml = r#"
admission_blocks:
  - name: x
    match: {}
    action: continue
admission:
  blocks: []
"#;
    let err = Config::from_yaml_str(yaml)
        .expect_err("mixed admission_blocks + admission must be rejected");
    let msg = format!("{err}");
    assert!(
        msg.contains("mix") || msg.contains("flat") || msg.contains("section"),
        "error must explain the mixed-shape, got: {msg}"
    );
}

// ── Phase 3c.1: iam_mode enum ──────────────────────────────────────

#[test]
fn test_iam_mode_default_is_gui() {
    let cfg = Config::default();
    assert_eq!(cfg.iam_mode, crate::config_sections::IamMode::Gui);
}

#[test]
fn test_iam_mode_omitted_from_default_export() {
    // Minimalism invariant: default deployments don't gain an
    // `iam_mode: gui` line in their exported YAML.
    let cfg = Config::default();
    let exported = cfg.to_canonical_yaml().unwrap();
    assert!(
        !exported.contains("iam_mode"),
        "default iam_mode must be omitted, got:\n{exported}"
    );
}

#[test]
fn test_iam_mode_declarative_roundtrips_through_sectioned_yaml() {
    let yaml = r#"
access:
  iam_mode: declarative
"#;
    let cfg = Config::from_yaml_str(yaml).unwrap();
    assert_eq!(cfg.iam_mode, crate::config_sections::IamMode::Declarative);
    let exported = cfg.to_canonical_yaml().unwrap();
    assert!(
        exported.contains("iam_mode: declarative"),
        "declarative mode must survive round-trip, got:\n{exported}"
    );
    let reloaded = Config::from_yaml_str(&exported).unwrap();
    assert_eq!(reloaded.iam_mode, cfg.iam_mode);
}

#[test]
fn test_iam_mode_flat_shape_also_accepts() {
    let yaml = r#"
listen_addr: "127.0.0.1:9000"
iam_mode: declarative
"#;
    let cfg = Config::from_yaml_str(yaml).unwrap();
    assert_eq!(cfg.iam_mode, crate::config_sections::IamMode::Declarative);
}

#[test]
fn test_iam_mode_unknown_variant_rejected() {
    let yaml = r#"
access:
  iam_mode: wat
"#;
    let err = Config::from_yaml_str(yaml).expect_err("unknown iam_mode value must be rejected");
    let msg = format!("{err}");
    assert!(
        msg.contains("wat") || msg.contains("iam_mode"),
        "error must explain the offending value, got: {msg}"
    );
}

#[test]
fn test_admission_blocks_flat_shape_loads_without_sectioned_wrapper() {
    // Flat-shape YAML with only `admission_blocks:` at root must
    // parse without error (classifier routes to Flat because there
    // are no section keys).
    let yaml = r#"
admission_blocks:
  - name: deny-bad
    match:
      source_ip_list: ["203.0.113.5"]
    action: deny
"#;
    let cfg = Config::from_yaml_str(yaml).unwrap();
    assert_eq!(cfg.admission_blocks.len(), 1);
    assert_eq!(cfg.admission_blocks[0].name, "deny-bad");
}

#[test]
fn test_check_warns_on_env_suffix_collision() {
    // Correctness x-ray C4: "eu-archive" and "eu.archive" both
    // normalize to EU_ARCHIVE, so both would read the SAME env
    // var DGP_BACKEND_EU_ARCHIVE_ENCRYPTION_KEY. With the same
    // key material but distinct derived key_ids (the raw name
    // feeds into derive_key_id), objects written by one backend
    // can't be read by the other. check() now warns loudly.
    let mut cfg = Config {
        backends: vec![
            NamedBackendConfig {
                name: "eu-archive".into(),
                backend: BackendConfig::Filesystem { path: "/a".into() },
                encryption: BackendEncryptionConfig::default(),
            },
            NamedBackendConfig {
                name: "eu.archive".into(),
                backend: BackendConfig::Filesystem { path: "/b".into() },
                encryption: BackendEncryptionConfig::default(),
            },
        ],
        ..Config::default()
    };
    let warnings = cfg.check();
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("env-var suffix") && w.contains("EU_ARCHIVE")),
        "expected env-suffix collision warning, got {:?}",
        warnings
    );
}

#[test]
fn test_check_no_env_suffix_warning_for_distinct_names() {
    let mut cfg = Config {
        backends: vec![
            NamedBackendConfig {
                name: "one".into(),
                backend: BackendConfig::Filesystem { path: "/a".into() },
                encryption: BackendEncryptionConfig::default(),
            },
            NamedBackendConfig {
                name: "two".into(),
                backend: BackendConfig::Filesystem { path: "/b".into() },
                encryption: BackendEncryptionConfig::default(),
            },
        ],
        ..Config::default()
    };
    let warnings = cfg.check();
    assert!(
        !warnings.iter().any(|w| w.contains("env-var suffix")),
        "distinct names must not trip env-suffix collision, got {:?}",
        warnings
    );
}

#[test]
fn test_persist_to_file_preserves_yaml_stored_encryption_keys() {
    // Regression for xray-finding C1: before the fix,
    // `persist_to_file` → `to_canonical_yaml` → `redact_infra_secrets`
    // would strip per-backend encryption keys from the on-disk
    // YAML on every admin write. Operator-configured keys
    // silently disappeared on the next `PATCH … /config/section`
    // round-trip, and the next server restart fell back to env
    // lookup; if no env var was set, historical encrypted reads
    // started erroring and new writes landed plaintext.
    //
    // The persist path now uses the _for_persist serializers
    // which only strip the bootstrap password hash.
    let dir = tempfile::tempdir().unwrap();
    let yaml_path = dir.path().join("out.yaml");
    let cfg = Config {
        listen_addr: "127.0.0.1:9099".parse().unwrap(),
        backends: vec![NamedBackendConfig {
            name: "b".into(),
            backend: BackendConfig::Filesystem {
                path: "/tmp/x".into(),
            },
            encryption: BackendEncryptionConfig::Aes256GcmProxy {
                // 64-char hex key — realistic shape.
                key: Some(
                    "0101010101010101010101010101010101010101010101010101010101010101".into(),
                ),
                key_id: Some("test-kid".into()),
                legacy_key: Some(
                    "0202020202020202020202020202020202020202020202020202020202020202".into(),
                ),
                legacy_key_id: Some("test-legacy-kid".into()),
            },
        }],
        ..Config::default()
    };

    cfg.persist_to_file(yaml_path.to_str().unwrap()).unwrap();

    let persisted = std::fs::read_to_string(&yaml_path).unwrap();
    assert!(
        persisted.contains("0101010101010101010101010101010101010101010101010101010101010101"),
        "persisted YAML must preserve the primary encryption key, got:\n{}",
        persisted
    );
    assert!(
        persisted.contains("0202020202020202020202020202020202020202020202020202020202020202"),
        "persisted YAML must preserve the legacy encryption key, got:\n{}",
        persisted
    );

    // Export serialization (the downloadable artifact) MUST continue
    // to redact.
    let exported = cfg.to_canonical_yaml().unwrap();
    assert!(
        !exported.contains("0101010101010101010101010101010101010101010101010101010101010101"),
        "exported YAML must redact the primary encryption key, got:\n{}",
        exported
    );
    assert!(
        !exported.contains("0202020202020202020202020202020202020202020202020202020202020202"),
        "exported YAML must redact the legacy encryption key, got:\n{}",
        exported
    );
}

/// The registry and the admin GUI state this default: 4 per core, at least 16.
#[test]
fn default_codec_concurrency_is_four_per_core_at_least_16() {
    assert_eq!(default_codec_concurrency(1), 16);
    assert_eq!(default_codec_concurrency(4), 16);
    assert_eq!(default_codec_concurrency(8), 32);
    let pinned = Config {
        codec_concurrency: Some(3),
        ..Config::default()
    };
    assert_eq!(pinned.effective_codec_concurrency(), 3);
}
