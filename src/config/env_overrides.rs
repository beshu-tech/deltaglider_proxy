// SPDX-License-Identifier: BUSL-1.1

//! Which admin-GUI fields an environment variable currently controls.
//!
//! `Config::apply_env_overrides` makes `DGP_*` variables win over the YAML
//! file. The admin GUI must show that: a field whose value comes from the
//! environment is read-only in practice (an edit is persisted to YAML and then
//! overridden again at the next start). [`env_overrides`] reports, for each
//! GUI-visible field, whether its variable is set and applied, and the
//! effective value — never the value of a secret.
//!
//! Pure: the environment lookup is injected, so the truth table is unit-tested
//! without touching the process environment.

use serde::Serialize;

/// How `apply_env_overrides` parses the variable. A value that does not parse
/// is ignored there (with a warning), so it is not an override here either.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvValueKind {
    Text,
    Unsigned,
    Float,
    SocketAddr,
    /// Only a truthy value applies (`DGP_TLS_ENABLED=false` changes nothing).
    TruthyFlag,
}

/// Extra condition from `apply_env_overrides` for the variable to apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvGuard {
    Always,
    /// Applies only when the named flag variable is truthy.
    FlagSet(&'static str),
    /// Applies only when at least one of these variables is set.
    AnySet(&'static [&'static str]),
    /// Applies only when none of these variables is set.
    NoneSet(&'static [&'static str]),
}

/// One GUI field that a `DGP_*` variable can control.
#[derive(Debug, Clone, Copy)]
pub struct EnvFieldBinding {
    pub env: &'static str,
    /// YAML path of the field, as the GUI's `FormField yamlPath` names it.
    /// `None` = the setting exists only as an environment variable.
    pub yaml_path: Option<&'static str>,
    pub kind: EnvValueKind,
    pub secret: bool,
    pub guard: EnvGuard,
}

const S3_ACTIVATORS: &[&str] = &["DGP_S3_ENDPOINT", "DGP_S3_REGION"];

const fn bind(env: &'static str, yaml_path: &'static str, kind: EnvValueKind) -> EnvFieldBinding {
    EnvFieldBinding {
        env,
        yaml_path: Some(yaml_path),
        kind,
        secret: false,
        guard: EnvGuard::Always,
    }
}

const fn env_only(env: &'static str) -> EnvFieldBinding {
    EnvFieldBinding {
        env,
        yaml_path: None,
        kind: EnvValueKind::Unsigned,
        secret: false,
        guard: EnvGuard::Always,
    }
}

/// Mirrors `Config::apply_env_overrides` for every field the GUI edits, plus
/// the env-only request limits the System page shows.
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
        "DGP_TLS_ENABLED",
        "advanced.tls.enabled",
        EnvValueKind::TruthyFlag,
    ),
    EnvFieldBinding {
        guard: EnvGuard::FlagSet("DGP_TLS_ENABLED"),
        ..bind("DGP_TLS_CERT", "advanced.tls.cert_path", EnvValueKind::Text)
    },
    EnvFieldBinding {
        guard: EnvGuard::FlagSet("DGP_TLS_ENABLED"),
        ..bind("DGP_TLS_KEY", "advanced.tls.key_path", EnvValueKind::Text)
    },
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
    // Legacy single-backend shape (`storage.backend`).
    bind(
        "DGP_S3_ENDPOINT",
        "storage.backend.endpoint",
        EnvValueKind::Text,
    ),
    bind(
        "DGP_S3_REGION",
        "storage.backend.region",
        EnvValueKind::Text,
    ),
    EnvFieldBinding {
        guard: EnvGuard::AnySet(S3_ACTIVATORS),
        ..bind(
            "DGP_BE_AWS_ACCESS_KEY_ID",
            "storage.backend.access_key_id",
            EnvValueKind::Text,
        )
    },
    EnvFieldBinding {
        guard: EnvGuard::AnySet(S3_ACTIVATORS),
        secret: true,
        ..bind(
            "DGP_BE_AWS_SECRET_ACCESS_KEY",
            "storage.backend.secret_access_key",
            EnvValueKind::Text,
        )
    },
    EnvFieldBinding {
        guard: EnvGuard::NoneSet(S3_ACTIVATORS),
        ..bind("DGP_DATA_DIR", "storage.backend.path", EnvValueKind::Text)
    },
    env_only("DGP_REQUEST_TIMEOUT_SECS"),
    env_only("DGP_MAX_CONCURRENT_REQUESTS"),
    env_only("DGP_MAX_MULTIPART_UPLOADS"),
];

/// One field whose value currently comes from the environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EnvOverride {
    pub env: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub yaml_path: Option<&'static str>,
    pub secret: bool,
    /// The effective value. `None` for secrets: the GUI shows only that the
    /// value is set from the environment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

/// Same truth table as `env_bool`.
fn parse_bool(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// The value `apply_env_overrides` would apply, or `None` when it would
/// ignore the variable.
fn applied_value(kind: EnvValueKind, raw: &str) -> Option<String> {
    match kind {
        EnvValueKind::Text => Some(raw.to_string()),
        EnvValueKind::Unsigned => raw.parse::<u64>().ok().map(|v| v.to_string()),
        EnvValueKind::Float => raw.parse::<f32>().ok().map(|v| v.to_string()),
        EnvValueKind::SocketAddr => raw
            .parse::<std::net::SocketAddr>()
            .ok()
            .map(|v| v.to_string()),
        EnvValueKind::TruthyFlag => (parse_bool(raw) == Some(true)).then(|| "true".to_string()),
    }
}

fn guard_holds(guard: EnvGuard, lookup: &dyn Fn(&str) -> Option<String>) -> bool {
    match guard {
        EnvGuard::Always => true,
        EnvGuard::FlagSet(flag) => lookup(flag).and_then(|v| parse_bool(&v)) == Some(true),
        EnvGuard::AnySet(vars) => vars.iter().any(|v| lookup(v).is_some()),
        EnvGuard::NoneSet(vars) => vars.iter().all(|v| lookup(v).is_none()),
    }
}

/// Every GUI field whose value the environment currently controls.
pub fn env_overrides(lookup: &dyn Fn(&str) -> Option<String>) -> Vec<EnvOverride> {
    ENV_FIELD_BINDINGS
        .iter()
        .filter(|b| guard_holds(b.guard, lookup))
        .filter_map(|b| {
            let value = applied_value(b.kind, &lookup(b.env)?)?;
            Some(EnvOverride {
                env: b.env,
                yaml_path: b.yaml_path,
                secret: b.secret,
                value: (!b.secret).then_some(value),
            })
        })
        .collect()
}

/// [`env_overrides`] against the real process environment.
pub fn process_env_overrides() -> Vec<EnvOverride> {
    env_overrides(&|name| std::env::var(name).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn run(vars: &[(&str, &str)]) -> Vec<EnvOverride> {
        let map: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        env_overrides(&move |name| map.get(name).cloned())
    }

    fn find<'a>(out: &'a [EnvOverride], env: &str) -> Option<&'a EnvOverride> {
        out.iter().find(|o| o.env == env)
    }

    #[test]
    fn empty_environment_reports_nothing() {
        assert!(run(&[]).is_empty());
    }

    #[test]
    fn plain_field_reports_value_and_yaml_path() {
        let out = run(&[("DGP_CACHE_MB", "2048")]);
        assert_eq!(
            out,
            vec![EnvOverride {
                env: "DGP_CACHE_MB",
                yaml_path: Some("advanced.cache_size_mb"),
                secret: false,
                value: Some("2048".into()),
            }]
        );
    }

    #[test]
    fn secrets_never_carry_their_value() {
        let out = run(&[
            ("DGP_ACCESS_KEY_ID", "AKIAENV"),
            ("DGP_SECRET_ACCESS_KEY", "super-secret"),
        ]);
        assert_eq!(
            find(&out, "DGP_ACCESS_KEY_ID").unwrap().value.as_deref(),
            Some("AKIAENV")
        );
        let secret = find(&out, "DGP_SECRET_ACCESS_KEY").unwrap();
        assert!(secret.secret);
        assert_eq!(secret.value, None);
        let json = serde_json::to_string(&out).unwrap();
        assert!(!json.contains("super-secret"), "secret leaked: {json}");
    }

    #[test]
    fn unparseable_values_are_not_overrides() {
        // apply_env_overrides ignores these, so the YAML value still wins.
        let out = run(&[
            ("DGP_CACHE_MB", "lots"),
            ("DGP_LISTEN_ADDR", "not-an-addr"),
            ("DGP_MAX_DELTA_RATIO", "x"),
        ]);
        assert!(out.is_empty(), "{out:?}");
    }

    #[test]
    fn tls_flag_applies_only_when_truthy_and_gates_paths() {
        let off = run(&[("DGP_TLS_ENABLED", "false"), ("DGP_TLS_CERT", "/c.pem")]);
        assert!(off.is_empty(), "{off:?}");
        let on = run(&[("DGP_TLS_ENABLED", "yes"), ("DGP_TLS_CERT", "/c.pem")]);
        assert_eq!(
            find(&on, "DGP_TLS_ENABLED").unwrap().value.as_deref(),
            Some("true")
        );
        assert_eq!(
            find(&on, "DGP_TLS_CERT").unwrap().yaml_path,
            Some("advanced.tls.cert_path")
        );
    }

    #[test]
    fn backend_vars_follow_the_s3_versus_filesystem_switch() {
        // Backend credentials apply only when an S3 activator is set.
        let no_s3 = run(&[("DGP_BE_AWS_ACCESS_KEY_ID", "k")]);
        assert!(no_s3.is_empty());
        // DGP_DATA_DIR loses to the S3 activators.
        let both = run(&[("DGP_S3_REGION", "eu"), ("DGP_DATA_DIR", "/d")]);
        assert!(find(&both, "DGP_DATA_DIR").is_none());
        assert!(find(&both, "DGP_S3_REGION").is_some());
        let fs = run(&[("DGP_DATA_DIR", "/d")]);
        assert_eq!(
            find(&fs, "DGP_DATA_DIR").unwrap().yaml_path,
            Some("storage.backend.path")
        );
    }

    #[test]
    fn env_only_limits_have_no_yaml_path() {
        let out = run(&[("DGP_REQUEST_TIMEOUT_SECS", "60")]);
        let o = find(&out, "DGP_REQUEST_TIMEOUT_SECS").unwrap();
        assert_eq!(o.yaml_path, None);
        assert_eq!(o.value.as_deref(), Some("60"));
    }

    #[test]
    fn every_binding_is_a_registered_env_var() {
        let registry: Vec<&str> = super::super::ENV_VAR_REGISTRY
            .iter()
            .map(|e| e.name)
            .collect();
        for b in ENV_FIELD_BINDINGS {
            assert!(
                registry.contains(&b.env),
                "{} not in ENV_VAR_REGISTRY",
                b.env
            );
        }
    }

    #[test]
    fn every_secret_binding_is_a_secret_named_var() {
        for b in ENV_FIELD_BINDINGS {
            let looks_secret = b.env.contains("SECRET");
            assert_eq!(b.secret, looks_secret, "{}", b.env);
        }
    }
}
