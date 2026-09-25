// SPDX-License-Identifier: BUSL-1.1

//! Which admin-GUI fields an environment variable currently controls.
//!
//! `Config::apply_env_overrides_with` makes `DGP_*` variables win over the
//! YAML file (at boot and again on every admin apply). The admin GUI must
//! show that: a field whose value comes from the environment is read-only.
//! [`env_overrides`] reports every such field with its effective value —
//! never the value of a secret.
//!
//! Some variables control a whole block, not one field: an S3 activator
//! (`DGP_S3_ENDPOINT` / `DGP_S3_REGION`) replaces the whole
//! `storage.backend` block, and `DGP_TLS_ENABLED=true` replaces the whole
//! `advanced.tls` block. Members whose own variable is unset are reported
//! too (`set: false`, `activated_by` names the activator), with the default
//! the override puts there.
//!
//! Pure: the environment lookup and the config are injected. A drift test
//! (below) runs the real `apply_env_overrides_with` with a recording lookup
//! and fails when it reads a variable this module does not report.

use super::{backend_encryption_env_names, BackendEncryptionConfig, Config, EnvLookup};
use serde::Serialize;

/// How `apply_env_overrides_with` parses the variable. A value that does not
/// parse is ignored there (with a warning), so it is not an override here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvValueKind {
    Text,
    /// Applies only when not blank (`DGP_CONFIG_SYNC_KEY`).
    NonBlankText,
    Unsigned,
    Float,
    SocketAddr,
    /// A boolean; an unrecognised value falls back to the default.
    Bool {
        default: bool,
    },
}

/// One GUI field that a `DGP_*` variable controls on its own.
#[derive(Debug, Clone, Copy)]
pub struct EnvFieldBinding {
    pub env: &'static str,
    /// YAML path of the field, as the GUI's `FormField yamlPath` names it.
    /// `None` = the setting exists only as an environment variable.
    pub yaml_path: Option<&'static str>,
    pub kind: EnvValueKind,
    pub secret: bool,
}

/// One member of a block that an activator variable replaces as a whole.
#[derive(Debug, Clone, Copy)]
pub struct EnvBlockMember {
    pub env: &'static str,
    pub yaml_path: &'static str,
    pub kind: EnvValueKind,
    pub secret: bool,
    /// What the override puts in the field when `env` is unset.
    pub unset_value: Option<&'static str>,
}

pub const S3_ACTIVATORS: &[&str] = &["DGP_S3_ENDPOINT", "DGP_S3_REGION"];
pub const TLS_FLAG: &str = "DGP_TLS_ENABLED";

const fn bind(env: &'static str, yaml_path: &'static str, kind: EnvValueKind) -> EnvFieldBinding {
    EnvFieldBinding {
        env,
        yaml_path: Some(yaml_path),
        kind,
        secret: false,
    }
}

const fn env_only(env: &'static str) -> EnvFieldBinding {
    EnvFieldBinding {
        env,
        yaml_path: None,
        kind: EnvValueKind::Unsigned,
        secret: false,
    }
}

const fn member(
    env: &'static str,
    yaml_path: &'static str,
    kind: EnvValueKind,
    secret: bool,
    unset_value: Option<&'static str>,
) -> EnvBlockMember {
    EnvBlockMember {
        env,
        yaml_path,
        kind,
        secret,
        unset_value,
    }
}

/// Variables that control exactly one field.
pub const ENV_FIELD_BINDINGS: &[EnvFieldBinding] = &[
    bind(
        "DGP_LISTEN_ADDR",
        "advanced.listen_addr",
        EnvValueKind::SocketAddr,
    ),
    bind(
        "DGP_MAX_DELTA_RATIO",
        "advanced.max_delta_ratio",
        EnvValueKind::Float,
    ),
    bind(
        "DGP_MAX_OBJECT_SIZE",
        "advanced.max_object_size",
        EnvValueKind::Unsigned,
    ),
    bind(
        "DGP_MAX_PASSTHROUGH_OBJECT_SIZE",
        "advanced.max_passthrough_object_size",
        EnvValueKind::Unsigned,
    ),
    bind(
        "DGP_CACHE_MB",
        "advanced.cache_size_mb",
        EnvValueKind::Unsigned,
    ),
    bind(
        "DGP_METADATA_CACHE_MB",
        "advanced.metadata_cache_mb",
        EnvValueKind::Unsigned,
    ),
    bind(
        "DGP_CODEC_CONCURRENCY",
        "advanced.codec_concurrency",
        EnvValueKind::Unsigned,
    ),
    bind(
        "DGP_BLOCKING_THREADS",
        "advanced.blocking_threads",
        EnvValueKind::Unsigned,
    ),
    bind("DGP_LOG_LEVEL", "advanced.log_level", EnvValueKind::Text),
    bind(
        "DGP_CONFIG_SYNC_BUCKET",
        "advanced.config_sync_bucket",
        EnvValueKind::Text,
    ),
    bind(
        "DGP_CONFIG_SYNC_KEY",
        "advanced.config_sync_object_key",
        EnvValueKind::NonBlankText,
    ),
    bind(
        "DGP_AUTHENTICATION",
        "access.authentication",
        EnvValueKind::Text,
    ),
    bind(
        "DGP_ACCESS_KEY_ID",
        "access.access_key_id",
        EnvValueKind::Text,
    ),
    EnvFieldBinding {
        secret: true,
        ..bind(
            "DGP_SECRET_ACCESS_KEY",
            "access.secret_access_key",
            EnvValueKind::Text,
        )
    },
    env_only("DGP_REQUEST_TIMEOUT_SECS"),
    env_only("DGP_MAX_CONCURRENT_REQUESTS"),
    env_only("DGP_MAX_MULTIPART_UPLOADS"),
];

/// The legacy singleton backend when an S3 activator is set: the WHOLE
/// `storage.backend` block comes from the environment.
pub const S3_BLOCK: &[EnvBlockMember] = &[
    member(
        "DGP_S3_ENDPOINT",
        "storage.backend.endpoint",
        EnvValueKind::Text,
        false,
        None,
    ),
    member(
        "DGP_S3_REGION",
        "storage.backend.region",
        EnvValueKind::Text,
        false,
        Some("us-east-1"),
    ),
    member(
        "DGP_S3_PATH_STYLE",
        "storage.backend.force_path_style",
        EnvValueKind::Bool { default: true },
        false,
        Some("true"),
    ),
    member(
        "DGP_BE_AWS_ACCESS_KEY_ID",
        "storage.backend.access_key_id",
        EnvValueKind::Text,
        false,
        None,
    ),
    member(
        "DGP_BE_AWS_SECRET_ACCESS_KEY",
        "storage.backend.secret_access_key",
        EnvValueKind::Text,
        true,
        None,
    ),
    member(
        "DGP_BACKEND_ALLOW_LOCAL",
        "storage.backend.allow_local",
        EnvValueKind::Bool { default: false },
        false,
        Some("false"),
    ),
];

/// `DGP_TLS_ENABLED=true` replaces the WHOLE `advanced.tls` block.
pub const TLS_BLOCK: &[EnvBlockMember] = &[
    member(
        "DGP_TLS_CERT",
        "advanced.tls.cert_path",
        EnvValueKind::Text,
        false,
        None,
    ),
    member(
        "DGP_TLS_KEY",
        "advanced.tls.key_path",
        EnvValueKind::Text,
        false,
        None,
    ),
];

pub const BACKEND_TYPE_PATH: &str = "storage.backend.type";
pub const BACKEND_PATH_PATH: &str = "storage.backend.path";
pub const DATA_DIR: &str = "DGP_DATA_DIR";

/// Variables `apply_env_overrides_with` reads that the GUI never shows,
/// with the reason. The drift test accepts these and nothing else unbound.
pub const NOT_GUI_VISIBLE: &[(&str, &str)] = &[
    (
        "DGP_BOOTSTRAP_PASSWORD_HASH",
        "infra secret; never persisted, rotated with PUT /api/admin/password",
    ),
    (
        "DGP_ADMIN_PASSWORD_HASH",
        "legacy alias of DGP_BOOTSTRAP_PASSWORD_HASH",
    ),
];

/// YAML path of a backend's encryption field. The singleton backend
/// ("default" with no named list) lives under `storage.backend_encryption`.
pub fn backend_encryption_path(backend_name: Option<&str>, field: &str) -> String {
    match backend_name {
        None => format!("storage.backend_encryption.{field}"),
        Some(name) => format!("storage.backends[{name}].encryption.{field}"),
    }
}

/// One field whose value currently comes from the environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EnvOverride {
    pub env: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub yaml_path: Option<String>,
    pub secret: bool,
    /// The effective value. `None` for secrets (the GUI shows only that the
    /// value comes from the environment) and for block members left unset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// `false` when `env` itself is unset and the field is env-controlled
    /// only because `activated_by` replaced its whole block.
    pub set: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activated_by: Option<String>,
}

/// Same truth table as `env_bool`.
fn parse_bool(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// The value the override applies, or `None` when it ignores the variable.
fn applied_value(kind: EnvValueKind, raw: &str) -> Option<String> {
    match kind {
        EnvValueKind::Text => Some(raw.to_string()),
        EnvValueKind::NonBlankText => (!raw.trim().is_empty()).then(|| raw.to_string()),
        EnvValueKind::Unsigned => raw.parse::<u64>().ok().map(|v| v.to_string()),
        EnvValueKind::Float => raw.parse::<f32>().ok().map(|v| v.to_string()),
        EnvValueKind::SocketAddr => raw
            .parse::<std::net::SocketAddr>()
            .ok()
            .map(|v| v.to_string()),
        EnvValueKind::Bool { default } => Some(parse_bool(raw).unwrap_or(default).to_string()),
    }
}

fn shown(secret: bool, value: String) -> Option<String> {
    (!secret).then_some(value)
}

fn block(out: &mut Vec<EnvOverride>, members: &[EnvBlockMember], activator: &str, env: EnvLookup) {
    for m in members {
        let entry = match env(m.env) {
            Some(raw) => EnvOverride {
                env: m.env.to_string(),
                yaml_path: Some(m.yaml_path.to_string()),
                secret: m.secret,
                value: applied_value(m.kind, &raw).and_then(|v| shown(m.secret, v)),
                set: true,
                activated_by: (m.env != activator).then(|| activator.to_string()),
            },
            None => EnvOverride {
                env: m.env.to_string(),
                yaml_path: Some(m.yaml_path.to_string()),
                secret: m.secret,
                value: m.unset_value.map(str::to_string),
                set: false,
                activated_by: Some(activator.to_string()),
            },
        };
        out.push(entry);
    }
}

/// Every GUI field whose value the environment currently controls, for the
/// running config `cfg` (its backend names and encryption modes decide which
/// per-backend encryption variables apply).
pub fn env_overrides(cfg: &Config, env: EnvLookup) -> Vec<EnvOverride> {
    let mut out = Vec::new();
    // First, so a lookup by YAML path finds it before DGP_LOG_LEVEL, which
    // it beats (see apply_env_overrides_with).
    if let Some(value) = env(super::RUST_LOG) {
        out.push(EnvOverride {
            env: super::RUST_LOG.to_string(),
            yaml_path: Some("advanced.log_level".to_string()),
            secret: false,
            value: Some(value),
            set: true,
            activated_by: None,
        });
    }
    for b in ENV_FIELD_BINDINGS {
        if let Some(value) = env(b.env).and_then(|raw| applied_value(b.kind, &raw)) {
            out.push(EnvOverride {
                env: b.env.to_string(),
                yaml_path: b.yaml_path.map(str::to_string),
                secret: b.secret,
                value: shown(b.secret, value),
                set: true,
                activated_by: None,
            });
        }
    }

    // Legacy singleton backend block.
    if let Some(activator) = S3_ACTIVATORS.iter().find(|v| env(v).is_some()) {
        block(&mut out, S3_BLOCK, activator, env);
        out.push(EnvOverride {
            env: activator.to_string(),
            yaml_path: Some(BACKEND_TYPE_PATH.to_string()),
            secret: false,
            value: Some("s3".into()),
            set: true,
            activated_by: None,
        });
        // The filesystem path is not used: the block is an S3 backend now.
        out.push(EnvOverride {
            env: DATA_DIR.to_string(),
            yaml_path: Some(BACKEND_PATH_PATH.to_string()),
            secret: false,
            value: None,
            set: false,
            activated_by: Some(activator.to_string()),
        });
    } else if let Some(dir) = env(DATA_DIR) {
        for (path, value) in [
            (BACKEND_TYPE_PATH, "filesystem"),
            (BACKEND_PATH_PATH, dir.as_str()),
        ] {
            out.push(EnvOverride {
                env: DATA_DIR.to_string(),
                yaml_path: Some(path.to_string()),
                secret: false,
                value: Some(value.to_string()),
                set: true,
                activated_by: None,
            });
        }
    }

    // TLS block.
    if env(TLS_FLAG).and_then(|v| parse_bool(&v)) == Some(true) {
        out.push(EnvOverride {
            env: TLS_FLAG.to_string(),
            yaml_path: Some("advanced.tls.enabled".into()),
            secret: false,
            value: Some("true".into()),
            set: true,
            activated_by: None,
        });
        block(&mut out, TLS_BLOCK, TLS_FLAG, env);
    }

    // Per-backend encryption: the variable applies only in the matching mode.
    let singleton = std::iter::once((None, "default", &cfg.backend_encryption));
    let named = cfg
        .backends
        .iter()
        .map(|b| (Some(b.name.as_str()), b.name.as_str(), &b.encryption));
    for (path_name, env_name, enc) in singleton.chain(named) {
        let (key_env, kms_env) = backend_encryption_env_names(env_name);
        let (var, field, secret) = match enc {
            BackendEncryptionConfig::Aes256GcmProxy { .. } => (key_env, "key", true),
            BackendEncryptionConfig::SseKms { .. } => (kms_env, "kms_key_id", false),
            _ => continue,
        };
        if let Some(raw) = env(&var).filter(|v| !v.is_empty()) {
            out.push(EnvOverride {
                env: var,
                yaml_path: Some(backend_encryption_path(path_name, field)),
                secret,
                value: shown(secret, raw),
                set: true,
                activated_by: None,
            });
        }
    }
    out
}

/// `(variable, value)` for every SECRET variable that is currently set and
/// applies to `cfg` (plus the bootstrap password hash, which the GUI never
/// shows). Persist and export refuse to write any of these values.
pub fn secret_env_values(cfg: &Config, env: EnvLookup) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = env_overrides(cfg, env)
        .into_iter()
        .filter(|o| o.secret && o.set)
        .filter_map(|o| env(&o.env).map(|v| (o.env, v)))
        .collect();
    for (name, _) in NOT_GUI_VISIBLE {
        if let Some(v) = env(name) {
            out.push((name.to_string(), v));
        }
    }
    out.retain(|(_, v)| !v.is_empty());
    out.sort();
    out.dedup();
    out
}

/// [`env_overrides`] against the real process environment.
pub fn process_env_overrides(cfg: &Config) -> Vec<EnvOverride> {
    env_overrides(cfg, &super::process_env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::{BTreeSet, HashMap};

    fn lookup(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name: &str| map.get(name).cloned()
    }

    fn run(vars: &[(&str, &str)]) -> Vec<EnvOverride> {
        env_overrides(&Config::default(), &lookup(vars))
    }

    fn at<'a>(out: &'a [EnvOverride], path: &str) -> Option<&'a EnvOverride> {
        out.iter().find(|o| o.yaml_path.as_deref() == Some(path))
    }

    fn by_env<'a>(out: &'a [EnvOverride], env: &str) -> Option<&'a EnvOverride> {
        out.iter().find(|o| o.env == env)
    }

    /// A config with every encryption mode the per-backend variables act on.
    fn encrypted_config() -> Config {
        let yaml = r#"
storage:
  backend_encryption:
    mode: aes256-gcm-proxy
  backends:
    - name: eu-archive
      type: filesystem
      path: /tmp/a
      encryption:
        mode: aes256-gcm-proxy
    - name: kms-one
      type: filesystem
      path: /tmp/b
      encryption:
        mode: sse-kms
        kms_key_id: arn:file
"#;
        Config::from_yaml_str(yaml).unwrap()
    }

    #[test]
    fn empty_environment_reports_nothing() {
        assert!(run(&[]).is_empty());
    }

    /// The GUI must show `RUST_LOG` as the source of the log level (read-only),
    /// ahead of `DGP_LOG_LEVEL`, which `RUST_LOG` beats.
    #[test]
    fn rust_log_controls_the_log_level_field() {
        let out = run(&[("RUST_LOG", "info"), ("DGP_LOG_LEVEL", "warn")]);
        let o = at(&out, "advanced.log_level").expect("log level reported");
        assert_eq!(o.env, "RUST_LOG");
        assert_eq!(o.value.as_deref(), Some("info"));
        let out = run(&[("DGP_LOG_LEVEL", "warn")]);
        assert_eq!(at(&out, "advanced.log_level").unwrap().env, "DGP_LOG_LEVEL");
    }

    #[test]
    fn plain_field_reports_value_and_yaml_path() {
        let out = run(&[("DGP_CACHE_MB", "2048")]);
        assert_eq!(
            out,
            vec![EnvOverride {
                env: "DGP_CACHE_MB".into(),
                yaml_path: Some("advanced.cache_size_mb".into()),
                secret: false,
                value: Some("2048".into()),
                set: true,
                activated_by: None,
            }]
        );
    }

    #[test]
    fn secrets_never_carry_their_value() {
        let out = env_overrides(
            &encrypted_config(),
            &lookup(&[
                ("DGP_ACCESS_KEY_ID", "AKIAENV"),
                ("DGP_SECRET_ACCESS_KEY", "super-secret"),
                ("DGP_S3_ENDPOINT", "http://s3"),
                ("DGP_BE_AWS_SECRET_ACCESS_KEY", "be-secret"),
                ("DGP_ENCRYPTION_KEY", "enc-secret"),
                ("DGP_BACKEND_EU_ARCHIVE_ENCRYPTION_KEY", "enc2-secret"),
            ]),
        );
        assert_eq!(
            by_env(&out, "DGP_ACCESS_KEY_ID").unwrap().value.as_deref(),
            Some("AKIAENV")
        );
        for o in out.iter().filter(|o| o.secret) {
            assert_eq!(o.value, None, "{o:?}");
        }
        let json = serde_json::to_string(&out).unwrap();
        for leak in ["super-secret", "be-secret", "enc-secret", "enc2-secret"] {
            assert!(!json.contains(leak), "{leak} leaked: {json}");
        }
    }

    #[test]
    fn unparseable_values_are_not_overrides() {
        let out = run(&[
            ("DGP_CACHE_MB", "lots"),
            ("DGP_LISTEN_ADDR", "not-an-addr"),
            ("DGP_MAX_DELTA_RATIO", "x"),
            ("DGP_CONFIG_SYNC_KEY", "  "),
        ]);
        assert!(out.is_empty(), "{out:?}");
    }

    #[test]
    fn s3_activator_puts_the_whole_backend_block_under_env_control() {
        let out = run(&[("DGP_S3_ENDPOINT", "http://minio:9000")]);
        let region = at(&out, "storage.backend.region").unwrap();
        assert_eq!(region.value.as_deref(), Some("us-east-1"));
        assert!(!region.set);
        assert_eq!(region.activated_by.as_deref(), Some("DGP_S3_ENDPOINT"));
        let secret = at(&out, "storage.backend.secret_access_key").unwrap();
        assert!(secret.secret && !secret.set && secret.value.is_none());
        assert_eq!(
            at(&out, "storage.backend.force_path_style")
                .unwrap()
                .value
                .as_deref(),
            Some("true")
        );
        assert_eq!(
            at(&out, "storage.backend.allow_local")
                .unwrap()
                .value
                .as_deref(),
            Some("false")
        );
        assert_eq!(
            at(&out, BACKEND_TYPE_PATH).unwrap().value.as_deref(),
            Some("s3")
        );
        assert!(at(&out, BACKEND_PATH_PATH).is_some());
        let endpoint = at(&out, "storage.backend.endpoint").unwrap();
        assert!(endpoint.set && endpoint.activated_by.is_none());
    }

    #[test]
    fn data_dir_loses_to_the_s3_activators() {
        let both = run(&[("DGP_S3_REGION", "eu"), ("DGP_DATA_DIR", "/d")]);
        assert_eq!(
            at(&both, BACKEND_TYPE_PATH).unwrap().value.as_deref(),
            Some("s3")
        );
        assert!(!at(&both, BACKEND_PATH_PATH).unwrap().set);
        let fs = run(&[("DGP_DATA_DIR", "/d")]);
        assert_eq!(
            at(&fs, BACKEND_PATH_PATH).unwrap().value.as_deref(),
            Some("/d")
        );
        assert_eq!(
            at(&fs, BACKEND_TYPE_PATH).unwrap().value.as_deref(),
            Some("filesystem")
        );
        // Backend credentials alone change nothing.
        assert!(run(&[("DGP_BE_AWS_ACCESS_KEY_ID", "k")]).is_empty());
    }

    #[test]
    fn tls_flag_controls_the_whole_block() {
        assert!(run(&[("DGP_TLS_ENABLED", "false"), ("DGP_TLS_CERT", "/c.pem")]).is_empty());
        let on = run(&[("DGP_TLS_ENABLED", "yes")]);
        assert_eq!(
            at(&on, "advanced.tls.enabled").unwrap().value.as_deref(),
            Some("true")
        );
        let cert = at(&on, "advanced.tls.cert_path").unwrap();
        assert!(!cert.set && cert.value.is_none());
        assert_eq!(cert.activated_by.as_deref(), Some("DGP_TLS_ENABLED"));
    }

    #[test]
    fn per_backend_encryption_follows_the_mode() {
        let cfg = encrypted_config();
        let out = env_overrides(
            &cfg,
            &lookup(&[
                ("DGP_ENCRYPTION_KEY", "k0"),
                ("DGP_BACKEND_EU_ARCHIVE_ENCRYPTION_KEY", "k1"),
                ("DGP_BACKEND_KMS_ONE_SSE_KMS_KEY_ID", "arn:env"),
                // Wrong mode for this backend: ignored.
                ("DGP_BACKEND_KMS_ONE_ENCRYPTION_KEY", "k2"),
            ]),
        );
        assert!(at(&out, "storage.backend_encryption.key").unwrap().secret);
        assert!(at(&out, "storage.backends[eu-archive].encryption.key").is_some());
        assert_eq!(
            at(&out, "storage.backends[kms-one].encryption.kms_key_id")
                .unwrap()
                .value
                .as_deref(),
            Some("arn:env")
        );
        assert!(by_env(&out, "DGP_BACKEND_KMS_ONE_ENCRYPTION_KEY").is_none());
    }

    #[test]
    fn env_only_limits_have_no_yaml_path() {
        let out = run(&[("DGP_REQUEST_TIMEOUT_SECS", "60")]);
        let o = by_env(&out, "DGP_REQUEST_TIMEOUT_SECS").unwrap();
        assert_eq!(o.yaml_path, None);
        assert_eq!(o.value.as_deref(), Some("60"));
    }

    /// A value every override accepts.
    fn plausible(name: &str) -> String {
        match name {
            "DGP_LISTEN_ADDR" => "127.0.0.1:1".into(),
            "DGP_TLS_ENABLED" => "true".into(),
            _ => "1".into(),
        }
    }

    /// Every variable the real override code reads, in both backend branches.
    fn variables_read_by_apply() -> BTreeSet<String> {
        let read = RefCell::new(BTreeSet::new());
        for with_s3 in [true, false] {
            let env = |name: &str| {
                read.borrow_mut().insert(name.to_string());
                if !with_s3 && S3_ACTIVATORS.contains(&name) {
                    return None;
                }
                Some(plausible(name))
            };
            encrypted_config().apply_env_overrides_with(&env);
        }
        read.into_inner()
    }

    /// Every variable `env_overrides` can report, in both backend branches.
    fn variables_reported() -> BTreeSet<String> {
        let mut names = BTreeSet::new();
        for with_s3 in [true, false] {
            let env = |name: &str| {
                if !with_s3 && S3_ACTIVATORS.contains(&name) {
                    return None;
                }
                Some(plausible(name))
            };
            for o in env_overrides(&encrypted_config(), &env) {
                names.insert(o.env);
            }
        }
        names
    }

    /// Drift guard: a variable the overrides read must be shown to the GUI or
    /// be listed in NOT_GUI_VISIBLE with a reason.
    #[test]
    fn every_variable_the_overrides_read_is_reported_or_exempt() {
        let reported = variables_reported();
        let exempt: BTreeSet<&str> = NOT_GUI_VISIBLE.iter().map(|(n, _)| *n).collect();
        for name in variables_read_by_apply() {
            assert!(
                reported.contains(&name) || exempt.contains(name.as_str()),
                "{name} is read by apply_env_overrides_with but neither reported by \
                 env_overrides nor listed in NOT_GUI_VISIBLE"
            );
        }
    }

    /// Deny-list: a variable whose name says it holds a secret must be marked
    /// secret, so its value never reaches the GUI. Names ending in `_KEY_ID`
    /// are identifiers; the two listed below name a file path and an object key.
    #[test]
    fn secret_looking_names_are_marked_secret() {
        const IDENTIFIERS: &[&str] = &["DGP_TLS_KEY", "DGP_CONFIG_SYNC_KEY"];
        let env = |name: &str| Some(plausible(name));
        let mut all = env_overrides(&encrypted_config(), &env);
        all.extend(env_overrides(&encrypted_config(), &|n: &str| {
            (!S3_ACTIVATORS.contains(&n)).then(|| plausible(n))
        }));
        assert!(!all.is_empty());
        for o in &all {
            let n = o.env.as_str();
            let looks_secret = ["HASH", "TOKEN", "PASSWORD", "SECRET", "KEY"]
                .iter()
                .any(|w| n.contains(w))
                && !n.ends_with("_KEY_ID")
                && !IDENTIFIERS.contains(&n);
            if looks_secret {
                assert!(o.secret, "{n} looks secret but is not marked secret");
            }
        }
    }

    #[test]
    fn every_static_binding_is_a_registered_env_var() {
        let registry: Vec<&str> = super::super::ENV_VAR_REGISTRY
            .iter()
            .map(|e| e.name)
            .collect();
        let names = ENV_FIELD_BINDINGS
            .iter()
            .map(|b| b.env)
            .chain(S3_BLOCK.iter().map(|m| m.env))
            .chain(TLS_BLOCK.iter().map(|m| m.env))
            .chain([DATA_DIR, TLS_FLAG]);
        for n in names {
            assert!(registry.contains(&n), "{n} not in ENV_VAR_REGISTRY");
        }
    }

    /// Source guard: the override code must read the environment ONLY
    /// through its injected lookup, or the drift test and the leak probe
    /// would not see the variable.
    #[test]
    fn override_code_reads_the_environment_only_through_the_lookup() {
        let src = include_str!("mod.rs");
        for (start, close) in [
            ("pub(crate) fn apply_env_overrides_with(", "\n    }\n"),
            ("pub(crate) fn apply_backend_encryption_env(", "\n}\n"),
        ] {
            let at = src
                .find(start)
                .unwrap_or_else(|| panic!("{start} not found"));
            let body = &src[at..];
            let end = body.find(close).expect("end of function");
            let body = &body[..end];
            for banned in [
                "std::env::var",
                "env_parse",
                "env_bool(",
                "env_parse_with_default",
            ] {
                assert!(
                    !body.contains(banned),
                    "{start} calls {banned}: read the environment through the injected lookup"
                );
            }
        }
    }
}
