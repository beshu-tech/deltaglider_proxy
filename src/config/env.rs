// SPDX-License-Identifier: BUSL-1.1

//! Typed environment-variable parsing helpers.
//!
//! These are the DRY convention for reading `DGP_*` env vars — parse through
//! `env_parse` / `env_bool` / `env_parse_with_default` rather than hand-rolling
//! `std::env::var(...).ok().and_then(...)` at call sites.

/// Parse an env var into a typed value, warning on invalid input.
pub fn env_parse<T: std::str::FromStr>(var: &str) -> Option<T>
where
    T::Err: std::fmt::Display,
{
    std::env::var(var).ok().and_then(|raw| {
        raw.parse()
            .map_err(|e| eprintln!("Warning: ignoring invalid {var}=\"{raw}\": {e}"))
            .ok()
    })
}

/// Parse an env var into a typed value, returning `default` if absent or invalid.
/// Logs a warning on invalid input (same as `env_parse`).
pub fn env_parse_with_default<T: std::str::FromStr>(var: &str, default: T) -> T
where
    T::Err: std::fmt::Display,
{
    env_parse(var).unwrap_or(default)
}

/// Parse a boolean env var, returning `default` if absent or
/// unrecognised. Accepts (case-insensitive, trimmed): `true`, `1`,
/// `yes`, `on` as true; `false`, `0`, `no`, `off` as false.
///
/// Unrecognised values log a warning and fall back to `default` so
/// operator typos don't silently flip behaviour in either direction.
pub fn env_bool(var: &str, default: bool) -> bool {
    let Ok(raw) = std::env::var(var) else {
        return default;
    };
    let trimmed = raw.trim();
    match trimmed.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => true,
        "false" | "0" | "no" | "off" => false,
        other => {
            tracing::warn!(
                target: "deltaglider_proxy::config",
                var = %var,
                value = %other,
                "env var `{}={}` is not a recognised boolean (expected true/1/yes/on or \
                 false/0/no/off); falling back to default={}",
                var,
                other,
                default
            );
            default
        }
    }
}
