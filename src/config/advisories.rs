// SPDX-License-Identifier: GPL-3.0-only

//! Cross-field config ADVISORIES — "this combination is suspicious" checks that
//! surface to the admin at SAVE time (and `config lint`).
//!
//! `Config::check()` already does per-section structural validation. This module
//! adds the *cross-field* rules that catch real operational footguns a single
//! field can't reveal — seeded from incidents:
//!
//! - **shared-rate-limit-bucket**: rate limiting on + `trust_proxy_headers` off
//!   behind a reverse proxy → every client collapses onto ONE bucket (the proxy
//!   IP) and one client can lock out the fleet. (The prod CI-403 incident.)
//! - **stale-iam-template**: a permission using a bare `${username}` (the
//!   pre-`${iam:username}` form) silently DENIES ALL of that user's permissions.
//!   (Denying the `xperi` user in prod right now.)
//!
//! Rules are PURE `fn(&Config, &EnvView) -> Option<Advisory>` so they unit-test
//! against a truth table without a server. They fold into the existing
//! `Vec<String>` warning channel (rendered by the admin ApplyDialog + `config
//! lint`) via `Advisory::to_string`. Advisories are NON-fatal — they never block
//! a save.

use super::Config;

/// Advisory severity — `Info` (FYI / probably-intentional) vs `Warn` (likely a
/// real problem). Both are non-blocking; severity only colors the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Info,
    Warn,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advisory {
    pub severity: Severity,
    pub message: String,
}

impl Advisory {
    fn warn(message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warn,
            message: message.into(),
        }
    }
    fn info(message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Info,
            message: message.into(),
        }
    }
    /// Render into the existing `Vec<String>` warning channel. Info-severity
    /// advisories are tagged `(info)` so they read as advisory-not-alarm; Warn
    /// advisories carry no prefix because every consumer of this channel already
    /// frames them as warnings (`config validate` lists them under "warnings",
    /// `Config::validate` prepends "Warning:", the admin ApplyDialog shows a
    /// warning Alert). Double-prefixing ("Warning: Warning:") is the bug this
    /// avoids.
    pub fn render(&self) -> String {
        match self.severity {
            Severity::Warn => self.message.clone(),
            Severity::Info => format!("(info) {}", self.message),
        }
    }
}

/// The few ENV-only settings cross-field rules need — rate limiting and
/// `trust_proxy_headers` live in env, NOT on `Config`, so a `fn(&Config)` rule
/// can't see them. `from_env()` snapshots the real environment; tests inject a
/// literal, keeping the rules pure.
#[derive(Debug, Clone, Copy)]
pub struct EnvView {
    pub trust_proxy_headers: bool,
    /// Whether per-IP auth rate limiting is active. It's on unless explicitly
    /// disabled — mirror `RateLimiter::default_auth()`'s enablement.
    pub rate_limit_enabled: bool,
}

impl EnvView {
    pub fn from_env() -> Self {
        Self {
            trust_proxy_headers: crate::config::env_bool("DGP_TRUST_PROXY_HEADERS", false),
            // Rate limiting is on by default; only an explicit 0-attempts disables it.
            rate_limit_enabled: crate::config::env_parse_with_default(
                "DGP_RATE_LIMIT_MAX_ATTEMPTS",
                100u32,
            ) > 0,
        }
    }
}

/// Run every advisory rule, collecting the ones that fire.
pub fn advisories(cfg: &Config, env: &EnvView) -> Vec<Advisory> {
    [
        rule_shared_rate_limit_bucket(cfg, env),
        rule_stale_iam_template(cfg, env),
        rule_frozen_bucket_quota(cfg, env),
        rule_public_prefix_redundant_with_open_auth(cfg, env),
    ]
    .into_iter()
    .flatten()
    .collect()
}

// ── rules ───────────────────────────────────────────────────────────────────

/// THE CI-403 incident: rate limiting on + trust-proxy off → all clients behind
/// a reverse proxy share one bucket (the proxy IP) and one can lock out everyone.
fn rule_shared_rate_limit_bucket(_cfg: &Config, env: &EnvView) -> Option<Advisory> {
    (env.rate_limit_enabled && !env.trust_proxy_headers).then(|| {
        Advisory::warn(
            "Auth rate limiting is enabled but DGP_TRUST_PROXY_HEADERS is off. Behind a \
             reverse proxy (Coolify/Traefik/nginx/ALB) every client collapses onto ONE \
             rate-limit bucket — the proxy's IP — so one client's failures can lock out \
             everyone. Set DGP_TRUST_PROXY_HEADERS=true if (and only if) DGP sits behind a \
             trusted proxy that sets X-Forwarded-For.",
        )
    })
}

/// A permission using a bare `${username}` / `${access_key_id}` (the pre-`iam:`
/// form removed in the breaking template rename) no longer substitutes — DGP
/// fails closed and DENIES ALL of that user's permissions. Reuses the canonical
/// `validate_permissions` so this advisory and the runtime check agree exactly.
fn rule_stale_iam_template(cfg: &Config, _env: &EnvView) -> Option<Advisory> {
    let mut offenders: Vec<String> = cfg
        .iam_users
        .iter()
        .filter(|u| crate::iam::permissions::validate_permissions(&u.permissions).is_err())
        .map(|u| u.name.clone())
        .collect();
    if offenders.is_empty() {
        return None;
    }
    offenders.sort();
    offenders.dedup();
    Some(Advisory::warn(format!(
        "IAM user(s) {:?} have permissions with an invalid/stale template variable (e.g. a \
         bare ${{username}} — the supported forms are ${{iam:username}} and \
         ${{iam:access_key_id}}). DGP fails closed on these and DENIES ALL of the user's \
         permissions until fixed.",
        offenders
    )))
}

/// A bucket with `quota_bytes: 0` is FROZEN (rejects every write). Easy to set by
/// accident; surface it as Info so an intentional freeze isn't noisy.
fn rule_frozen_bucket_quota(cfg: &Config, _env: &EnvView) -> Option<Advisory> {
    let mut frozen: Vec<String> = cfg
        .buckets
        .iter()
        .filter(|(_, p)| p.quota_bytes == Some(0))
        .map(|(name, _)| name.clone())
        .collect();
    if frozen.is_empty() {
        return None;
    }
    frozen.sort();
    Some(Advisory::info(format!(
        "Bucket(s) {:?} have quota_bytes=0, which FREEZES them (every write is rejected). \
         Remove the quota or set a positive limit if that wasn't intended.",
        frozen
    )))
}

/// Public prefixes / `public: true` while `authentication=none` is redundant —
/// the whole proxy is already unauthenticated, so every object is world-readable
/// regardless. Info, not Warn (it's harmless, just confusing).
fn rule_public_prefix_redundant_with_open_auth(cfg: &Config, _env: &EnvView) -> Option<Advisory> {
    let auth_none = cfg
        .authentication
        .as_deref()
        .map(|a| a.eq_ignore_ascii_case("none"))
        .unwrap_or(false);
    if !auth_none {
        return None;
    }
    let any_public = cfg
        .buckets
        .values()
        .any(|p| p.public == Some(true) || !p.public_prefixes.is_empty());
    any_public.then(|| {
        Advisory::info(
            "authentication is 'none' (open access) AND some buckets declare public \
             prefixes / public:true. With auth disabled every object is already \
             world-readable, so the public-prefix config is redundant.",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iam::types::Permission;
    use crate::iam::DeclarativeUser;

    fn base_cfg() -> Config {
        Config::default()
    }
    fn env(trust: bool, rl: bool) -> EnvView {
        EnvView {
            trust_proxy_headers: trust,
            rate_limit_enabled: rl,
        }
    }
    fn perm(resources: Vec<&str>) -> Permission {
        Permission {
            id: 0,
            effect: "Allow".into(),
            actions: vec!["read".into()],
            resources: resources.into_iter().map(String::from).collect(),
            conditions: None,
        }
    }
    fn user(name: &str, resources: Vec<&str>) -> DeclarativeUser {
        DeclarativeUser {
            name: name.into(),
            access_key_id: format!("AK{name}"),
            secret_access_key: "s".into(),
            enabled: true,
            groups: vec![],
            permissions: vec![perm(resources)],
        }
    }

    // ── shared-rate-limit-bucket truth table ──────────────────────────
    #[test]
    fn rate_limit_bucket_fires_only_when_on_and_untrusted() {
        let c = base_cfg();
        assert!(rule_shared_rate_limit_bucket(&c, &env(false, true)).is_some()); // rl on, untrusted → WARN
        assert!(rule_shared_rate_limit_bucket(&c, &env(true, true)).is_none()); // trusted → ok
        assert!(rule_shared_rate_limit_bucket(&c, &env(false, false)).is_none()); // rl off → ok
        assert!(rule_shared_rate_limit_bucket(&c, &env(true, false)).is_none());
    }

    // ── stale-iam-template ────────────────────────────────────────────
    #[test]
    fn stale_template_flags_bare_username_only() {
        let mut c = base_cfg();
        c.iam_users = vec![
            user("good", vec!["scrap/${iam:username}/*"]),
            user("plain", vec!["bucket/*"]),
        ];
        assert!(
            rule_stale_iam_template(&c, &env(true, true)).is_none(),
            "valid templates + plain resources → no advisory"
        );

        c.iam_users.push(user("xperi", vec!["scrap/${username}/*"]));
        let a = rule_stale_iam_template(&c, &env(true, true)).expect("bare ${username} must fire");
        assert!(
            a.message.contains("xperi"),
            "names the offender: {}",
            a.message
        );
        assert_eq!(a.severity, Severity::Warn);
    }

    // ── frozen quota ──────────────────────────────────────────────────
    #[test]
    fn frozen_quota_only_on_zero() {
        let mut c = base_cfg();
        c.buckets.insert(
            "frozen".into(),
            crate::bucket_policy::BucketPolicyConfig {
                quota_bytes: Some(0),
                ..Default::default()
            },
        );
        c.buckets.insert(
            "ok".into(),
            crate::bucket_policy::BucketPolicyConfig {
                quota_bytes: Some(1024),
                ..Default::default()
            },
        );
        let a = rule_frozen_bucket_quota(&c, &env(true, true)).expect("quota=0 fires");
        assert!(a.message.contains("frozen") && !a.message.contains("\"ok\""));
        assert_eq!(a.severity, Severity::Info);
    }

    // ── public + open auth ────────────────────────────────────────────
    #[test]
    fn public_redundant_only_when_auth_none_and_public() {
        let mut c = base_cfg();
        c.authentication = Some("none".into());
        c.buckets.insert(
            "pub".into(),
            crate::bucket_policy::BucketPolicyConfig {
                public: Some(true),
                ..Default::default()
            },
        );
        assert!(rule_public_prefix_redundant_with_open_auth(&c, &env(true, true)).is_some());

        c.authentication = Some("sigv4".into());
        assert!(
            rule_public_prefix_redundant_with_open_auth(&c, &env(true, true)).is_none(),
            "auth on → public prefixes are meaningful, no advisory"
        );
    }

    #[test]
    fn render_warn_is_bare_info_is_tagged() {
        // Warn carries no prefix (consumers already frame it as a warning — avoids
        // the "Warning: Warning:" double-prefix); Info is tagged so it reads as FYI.
        assert_eq!(Advisory::warn("x").render(), "x".to_string());
        assert_eq!(Advisory::info("y").render(), "(info) y".to_string());
    }
}
