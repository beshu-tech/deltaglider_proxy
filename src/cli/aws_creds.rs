// SPDX-License-Identifier: GPL-3.0-only

//! AWS credential resolution for the `cp`/`ls`/`rm`/`stats`/`verify`
//! subcommands. Hand-rolled to keep the binary free of `aws-config`
//! (the Cargo.toml comment explicitly bans it for SDK-bloat reasons).
//!
//! Precedence per dimension:
//!   1. CLI flag (`--access-key-id`, `--secret-access-key`, `--region`)
//!   2. Env var (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`,
//!      `AWS_SESSION_TOKEN`, `AWS_REGION` / `AWS_DEFAULT_REGION`)
//!   3. INI section from `~/.aws/credentials`, selected by
//!      `--profile` → `AWS_PROFILE` → `default`. File path overridable
//!      via `AWS_SHARED_CREDENTIALS_FILE`.
//!
//! No IMDS, no SSO, no STS / role assumption.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Resolved credentials + a tag recording where they came from (for
/// the optional `--verbose` trace and for the "which file did we
/// read?" diagnostics).
#[derive(Debug, Clone)]
pub struct ResolvedCreds {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
    pub region: Option<String>,
    pub source: CredsSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredsSource {
    /// Both keys came from CLI flags.
    Flag,
    /// Both keys came from environment variables.
    Env,
    /// Both keys came from the listed file's named profile section.
    ProfileFile { path: PathBuf, profile: String },
    /// Mixed sources (e.g. key id from flag, secret from env). The
    /// underlying source-per-field is opaque on purpose — the CLI
    /// only ever needs the resolved pair.
    Mixed,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CredsError {
    #[error("no AWS access key ID available (set --access-key-id, AWS_ACCESS_KEY_ID, or ~/.aws/credentials)")]
    MissingAccessKey,
    #[error("no AWS secret access key available (set --secret-access-key, AWS_SECRET_ACCESS_KEY, or ~/.aws/credentials)")]
    MissingSecretKey,
    #[error("profile '{profile}' not found in {path}")]
    ProfileNotFound { path: PathBuf, profile: String },
    #[error("could not read {path}: {msg}")]
    Io { path: PathBuf, msg: String },
    #[error("could not parse INI at {path}:{line}: {msg}")]
    IniParse {
        path: PathBuf,
        line: usize,
        msg: String,
    },
    #[error("home directory not found — set AWS_SHARED_CREDENTIALS_FILE to point at the file explicitly")]
    HomeDirUnknown,
}

/// Inputs supplied by the CLI — all optional; the resolver fills in
/// missing pieces from the env / profile chain.
#[derive(Debug, Default, Clone, Copy)]
pub struct CredsInputs<'a> {
    pub access_key_flag: Option<&'a str>,
    pub secret_key_flag: Option<&'a str>,
    pub session_token_flag: Option<&'a str>,
    pub profile_flag: Option<&'a str>,
    pub region_flag: Option<&'a str>,
}

/// Resolve credentials using the documented precedence chain. Reads
/// env vars + the shared-credentials file; otherwise pure.
pub fn resolve(inputs: CredsInputs<'_>) -> Result<ResolvedCreds, CredsError> {
    // 1. Flag pair short-circuits everything else for the key half.
    let (mut access_key, from_flag_ak) = match inputs.access_key_flag {
        Some(s) if !s.is_empty() => (Some(s.to_string()), true),
        _ => (None, false),
    };
    let (mut secret_key, from_flag_sk) = match inputs.secret_key_flag {
        Some(s) if !s.is_empty() => (Some(s.to_string()), true),
        _ => (None, false),
    };
    let mut session_token = inputs.session_token_flag.map(str::to_string);
    let from_flag_st = session_token.is_some();
    let mut region = inputs.region_flag.map(str::to_string);
    let from_flag_region = region.is_some();

    // 2. Env vars fill any unfilled slot.
    let mut from_env_any = false;
    if access_key.is_none() {
        if let Ok(v) = std::env::var("AWS_ACCESS_KEY_ID") {
            if !v.is_empty() {
                access_key = Some(v);
                from_env_any = true;
            }
        }
    }
    if secret_key.is_none() {
        if let Ok(v) = std::env::var("AWS_SECRET_ACCESS_KEY") {
            if !v.is_empty() {
                secret_key = Some(v);
                from_env_any = true;
            }
        }
    }
    if session_token.is_none() {
        if let Ok(v) = std::env::var("AWS_SESSION_TOKEN") {
            if !v.is_empty() {
                session_token = Some(v);
                from_env_any = true;
            }
        }
    }
    if region.is_none() {
        for var in ["AWS_REGION", "AWS_DEFAULT_REGION"] {
            if let Ok(v) = std::env::var(var) {
                if !v.is_empty() {
                    region = Some(v);
                    from_env_any = true;
                    break;
                }
            }
        }
    }

    // 3. Profile file fills any still-unfilled slot.
    let mut from_profile: Option<(PathBuf, String)> = None;
    if access_key.is_none() || secret_key.is_none() || region.is_none() {
        let profile_name = inputs
            .profile_flag
            .map(str::to_string)
            .or_else(|| std::env::var("AWS_PROFILE").ok())
            .unwrap_or_else(|| "default".to_string());
        let path = credentials_file_path()?;
        if path.exists() {
            let section = read_profile_section(&path, &profile_name)?;
            if access_key.is_none() {
                if let Some(v) = section.get("aws_access_key_id") {
                    access_key = Some(v.clone());
                }
            }
            if secret_key.is_none() {
                if let Some(v) = section.get("aws_secret_access_key") {
                    secret_key = Some(v.clone());
                }
            }
            if session_token.is_none() {
                if let Some(v) = section.get("aws_session_token") {
                    session_token = Some(v.clone());
                }
            }
            if region.is_none() {
                if let Some(v) = section.get("region") {
                    region = Some(v.clone());
                }
            }
            from_profile = Some((path, profile_name));
        }
    }

    let access_key_id = access_key.ok_or(CredsError::MissingAccessKey)?;
    let secret_access_key = secret_key.ok_or(CredsError::MissingSecretKey)?;

    // Tag the source for diagnostics. "All from flags" only when BOTH
    // keys originated as flags; same for env / profile. Anything else
    // is `Mixed`.
    let _ = (from_flag_st, from_flag_region);
    let source = match (from_flag_ak, from_flag_sk, from_profile, from_env_any) {
        (true, true, _, _) => CredsSource::Flag,
        (false, false, Some((p, n)), _) => CredsSource::ProfileFile {
            path: p,
            profile: n,
        },
        (false, false, None, true) => CredsSource::Env,
        // Mixed: e.g. AK from flag, SK from env.
        _ => CredsSource::Mixed,
    };

    Ok(ResolvedCreds {
        access_key_id,
        secret_access_key,
        session_token,
        region,
        source,
    })
}

/// Path to the AWS shared-credentials file. Honours
/// `AWS_SHARED_CREDENTIALS_FILE` then falls back to
/// `$HOME/.aws/credentials`.
fn credentials_file_path() -> Result<PathBuf, CredsError> {
    if let Ok(p) = std::env::var("AWS_SHARED_CREDENTIALS_FILE") {
        if !p.is_empty() {
            return Ok(PathBuf::from(p));
        }
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or(CredsError::HomeDirUnknown)?;
    Ok(home.join(".aws").join("credentials"))
}

/// Read one section of the credentials file. Returns the section's
/// key/value pairs (lowercased keys). Missing-file produces
/// `ProfileNotFound` only if the caller observed the file existed.
fn read_profile_section(
    path: &Path,
    profile: &str,
) -> Result<std::collections::HashMap<String, String>, CredsError> {
    let content = std::fs::read_to_string(path).map_err(|e| CredsError::Io {
        path: path.to_path_buf(),
        msg: e.to_string(),
    })?;
    let parsed = parse_ini(&content).map_err(|(line, msg)| CredsError::IniParse {
        path: path.to_path_buf(),
        line,
        msg,
    })?;
    parsed
        .get(profile)
        .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .ok_or_else(|| CredsError::ProfileNotFound {
            path: path.to_path_buf(),
            profile: profile.to_string(),
        })
}

/// Pure INI parser. Lines may be blank, comment (`#` or `;`),
/// `[section]`, or `key = value`. Keys are lowercased. Values are
/// trimmed of surrounding whitespace and **not** further unescaped
/// (matches AWS credentials-file conventions).
pub(crate) fn parse_ini(
    content: &str,
) -> Result<BTreeMap<String, BTreeMap<String, String>>, (usize, String)> {
    let mut out: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    let mut current: Option<String> = None;
    for (i, raw) in content.lines().enumerate() {
        let line_no = i + 1;
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(stripped) = line.strip_prefix('[') {
            let section = stripped
                .strip_suffix(']')
                .ok_or((line_no, "section header missing closing ']'".to_string()))?;
            let section = section.trim().to_string();
            if section.is_empty() {
                return Err((line_no, "empty section header".to_string()));
            }
            current = Some(section.clone());
            out.entry(section).or_default();
            continue;
        }
        let (k, v) = line.split_once('=').ok_or((
            line_no,
            "expected `key = value`, `[section]`, or a comment".to_string(),
        ))?;
        let key = k.trim().to_ascii_lowercase();
        let value = v.trim().to_string();
        if key.is_empty() {
            return Err((line_no, "empty key".to_string()));
        }
        let section = current.as_deref().ok_or((
            line_no,
            "key/value pair before any [section] header".to_string(),
        ))?;
        out.entry(section.to_string())
            .or_default()
            .insert(key, value);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shared lock so env-var-mutating tests don't trip over each
    /// other under `cargo test`'s parallel scheduler.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Snapshot every AWS_* env var, clear them, then restore on drop.
    struct EnvGuard {
        snapshot: Vec<(String, Option<String>)>,
    }
    impl EnvGuard {
        fn vars() -> &'static [&'static str] {
            &[
                "AWS_ACCESS_KEY_ID",
                "AWS_SECRET_ACCESS_KEY",
                "AWS_SESSION_TOKEN",
                "AWS_REGION",
                "AWS_DEFAULT_REGION",
                "AWS_PROFILE",
                "AWS_SHARED_CREDENTIALS_FILE",
            ]
        }
        fn capture_and_clear() -> Self {
            let snapshot = Self::vars()
                .iter()
                .map(|k| (k.to_string(), std::env::var(k).ok()))
                .collect();
            for k in Self::vars() {
                std::env::remove_var(k);
            }
            Self { snapshot }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for (k, v) in &self.snapshot {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    #[test]
    fn ini_parser_handles_empty_blank_comments() {
        let parsed = parse_ini(
            "\
# a comment
; another comment
[default]

aws_access_key_id = AK
aws_secret_access_key = SK

[other]
aws_access_key_id=AK2
",
        )
        .unwrap();
        let default = parsed.get("default").unwrap();
        assert_eq!(
            default.get("aws_access_key_id").map(String::as_str),
            Some("AK")
        );
        assert_eq!(
            default.get("aws_secret_access_key").map(String::as_str),
            Some("SK")
        );
        let other = parsed.get("other").unwrap();
        assert_eq!(
            other.get("aws_access_key_id").map(String::as_str),
            Some("AK2")
        );
    }

    #[test]
    fn ini_parser_rejects_unclosed_section() {
        let err = parse_ini("[default\nkey=val").expect_err("must reject");
        assert_eq!(err.0, 1);
        assert!(err.1.contains("closing"));
    }

    #[test]
    fn ini_parser_rejects_key_before_section() {
        let err = parse_ini("orphan = bad").expect_err("must reject");
        assert_eq!(err.0, 1);
        assert!(err.1.contains("section"));
    }

    #[test]
    fn ini_parser_keys_are_lowercased() {
        let parsed = parse_ini("[default]\nAWS_ACCESS_KEY_ID = X\n").unwrap();
        let d = parsed.get("default").unwrap();
        assert!(d.contains_key("aws_access_key_id"));
    }

    #[test]
    fn ini_parser_value_with_equals_keeps_rest() {
        // The first `=` splits; the rest of the value is preserved.
        let parsed = parse_ini("[default]\nkey = a=b=c\n").unwrap();
        let d = parsed.get("default").unwrap();
        assert_eq!(d.get("key").map(String::as_str), Some("a=b=c"));
    }

    #[test]
    fn resolve_flag_wins_over_env_and_file() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::capture_and_clear();
        std::env::set_var("AWS_ACCESS_KEY_ID", "from-env");
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "from-env-secret");

        let r = resolve(CredsInputs {
            access_key_flag: Some("from-flag"),
            secret_key_flag: Some("from-flag-secret"),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(r.access_key_id, "from-flag");
        assert_eq!(r.secret_access_key, "from-flag-secret");
        assert!(matches!(r.source, CredsSource::Flag));
    }

    #[test]
    fn resolve_env_when_no_flag() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::capture_and_clear();
        std::env::set_var("AWS_ACCESS_KEY_ID", "ak");
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "sk");
        std::env::set_var("AWS_REGION", "eu-west-2");

        let r = resolve(CredsInputs::default()).unwrap();
        assert_eq!(r.access_key_id, "ak");
        assert_eq!(r.secret_access_key, "sk");
        assert_eq!(r.region.as_deref(), Some("eu-west-2"));
        assert!(matches!(r.source, CredsSource::Env));
    }

    #[test]
    fn resolve_profile_file_when_no_flag_no_env() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::capture_and_clear();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds");
        std::fs::write(
            &path,
            "\
[default]
aws_access_key_id = default-ak
aws_secret_access_key = default-sk

[prod]
aws_access_key_id = prod-ak
aws_secret_access_key = prod-sk
region = us-east-2
",
        )
        .unwrap();
        std::env::set_var("AWS_SHARED_CREDENTIALS_FILE", &path);

        let r = resolve(CredsInputs {
            profile_flag: Some("prod"),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(r.access_key_id, "prod-ak");
        assert_eq!(r.secret_access_key, "prod-sk");
        assert_eq!(r.region.as_deref(), Some("us-east-2"));
        match &r.source {
            CredsSource::ProfileFile { profile, .. } => assert_eq!(profile, "prod"),
            other => panic!("expected ProfileFile, got {other:?}"),
        }
    }

    #[test]
    fn resolve_returns_missing_access_key_when_nothing_set() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::capture_and_clear();
        // Make sure the resolver doesn't accidentally find a real
        // ~/.aws/credentials on the test host.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("never-exists");
        std::env::set_var("AWS_SHARED_CREDENTIALS_FILE", &path);

        let err = resolve(CredsInputs::default()).expect_err("must fail");
        assert_eq!(err, CredsError::MissingAccessKey);
    }

    #[test]
    fn resolve_returns_profile_not_found_when_profile_missing_from_file() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _env = EnvGuard::capture_and_clear();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds");
        std::fs::write(
            &path,
            "[default]\naws_access_key_id=x\naws_secret_access_key=y\n",
        )
        .unwrap();
        std::env::set_var("AWS_SHARED_CREDENTIALS_FILE", &path);

        let err = resolve(CredsInputs {
            profile_flag: Some("nope"),
            ..Default::default()
        })
        .expect_err("must fail");
        match err {
            CredsError::ProfileNotFound { profile, .. } => assert_eq!(profile, "nope"),
            other => panic!("expected ProfileNotFound, got {other:?}"),
        }
    }
}
