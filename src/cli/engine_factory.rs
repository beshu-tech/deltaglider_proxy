// SPDX-License-Identifier: GPL-3.0-only

//! Construct an ephemeral `DeltaGliderEngine` for CLI subcommands.
//!
//! The proxy server hot-reloads a full `Config` from disk; the CLI
//! has only flag-supplied bits. This factory takes the CLI bits,
//! starts from `Config::default()`, overrides `backend` (and any
//! optional knobs), and hands the result to the same `DynEngine::new`
//! the server uses. No new engine surface.

use crate::config::{BackendConfig, Config};
use crate::deltaglider::DynEngine;
use crate::storage::StorageError;

/// Inputs the CLI gathers from its flags + the credential resolver.
#[derive(Debug, Clone)]
pub struct CliEngineOpts {
    pub endpoint: Option<String>,
    pub region: String,
    pub force_path_style: bool,
    pub access_key_id: String,
    pub secret_access_key: String,
    /// Override `Config::max_delta_ratio` when set.
    pub max_delta_ratio: Option<f32>,
    /// Override `Config::max_object_size` (in bytes) when set. The
    /// engine's default is 100 MB — the proxy's defensive memory
    /// ceiling because xdelta3 holds reference + delta + result in
    /// RAM simultaneously. CLI invocations against large artifacts
    /// (release ZIPs, OS images) need to raise this. Surfaced via
    /// `--max-object-size-mb` on the CLI subcommands that ingest
    /// data (`cp`, `sync`, `migrate`); reading-only verbs (`ls`,
    /// `stats`, `verify`, `purge`, `rm`) ignore it.
    pub max_object_size: Option<u64>,
    /// When the operator hands us a private-IP / localhost endpoint
    /// (typical MinIO / dev pattern), set `DGP_BACKEND_ALLOW_LOCAL=true`
    /// in the CLI process so the SSRF guard at `src/storage/s3.rs`
    /// doesn't reject the connection. The server's equivalent stays
    /// config-driven; this is the documented CLI divergence.
    pub allow_local: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("engine init failed: {0}")]
    Engine(#[from] StorageError),
}

/// Build a one-shot engine pointed at the supplied S3 endpoint.
pub async fn build_cli_engine(opts: CliEngineOpts) -> Result<DynEngine, BuildError> {
    // `allow_local` flows through the typed `BackendConfig::S3` field
    // instead of via the `DGP_BACKEND_ALLOW_LOCAL` env var. The legacy
    // env path still works for backward compat (handled inside
    // `S3Backend::build_client`), but new CLI invocations don't need
    // to mutate process env — eliminates the `unsafe { set_var }`
    // hazard at startup and makes the engine testable without env
    // munging.
    let backend = BackendConfig::S3 {
        endpoint: opts.endpoint,
        region: opts.region,
        force_path_style: opts.force_path_style,
        access_key_id: Some(opts.access_key_id),
        secret_access_key: Some(opts.secret_access_key),
        allow_local: opts.allow_local,
    };
    let mut cfg = Config {
        backend,
        max_delta_ratio: opts.max_delta_ratio.unwrap_or_else(default_max_delta_ratio),
        ..Config::default()
    };
    if let Some(size) = opts.max_object_size {
        cfg.max_object_size = size;
    }

    let engine = DynEngine::new(&cfg, None).await?;
    Ok(engine)
}

fn default_max_delta_ratio() -> f32 {
    Config::default().max_delta_ratio
}

/// Render an engine error for the operator. For `TooLarge` we surface
/// the actionable knob (`--max-object-size-mb`) so users don't have
/// to dig through docs after their multi-GB release upload fails 100
/// MiB in. Other errors fall through to the existing Display impl.
///
/// Kept tiny on purpose — the CLI ingest verbs (`cp`, `sync`,
/// `migrate`) all want the same hint, but the read-only verbs never
/// hit `TooLarge` so they don't need this helper.
pub fn render_store_error(e: &crate::deltaglider::EngineError) -> String {
    use crate::deltaglider::EngineError;
    match e {
        EngineError::TooLarge { size, max } => {
            let size_mb = *size as f64 / (1024.0 * 1024.0);
            let max_mb = *max as f64 / (1024.0 * 1024.0);
            format!(
                "object exceeds engine size cap ({size_mb:.1} MiB > {max_mb:.1} MiB). \
                 Raise the cap for this invocation with --max-object-size-mb <MIB>, \
                 or set `max_object_size` in the proxy config for server-side raises. \
                 Note: xdelta3 memory scales with object size; values >1 GiB may OOM \
                 small hosts."
            )
        }
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: `Config::default()` is overridable into an S3
    /// shape without leaving stale fields behind. We don't actually
    /// build the engine here (no MinIO assumed) — just verify the
    /// overrides land.
    #[test]
    fn cli_opts_override_default_backend() {
        let backend = BackendConfig::S3 {
            endpoint: Some("https://s3.amazonaws.com".into()),
            region: "eu-central-1".into(),
            force_path_style: false,
            access_key_id: Some("AK".into()),
            secret_access_key: Some("SK".into()),
            allow_local: false,
        };
        let cfg = Config {
            backend,
            ..Config::default()
        };
        match &cfg.backend {
            BackendConfig::S3 {
                region,
                access_key_id,
                ..
            } => {
                assert_eq!(region, "eu-central-1");
                assert_eq!(access_key_id.as_deref(), Some("AK"));
            }
            _ => panic!("expected S3 backend after override"),
        }
    }

    #[test]
    fn max_delta_ratio_override_lands() {
        let opts = CliEngineOpts {
            endpoint: None,
            region: "us-east-1".into(),
            force_path_style: true,
            access_key_id: "AK".into(),
            secret_access_key: "SK".into(),
            max_delta_ratio: Some(0.5),
            max_object_size: None,
            allow_local: false,
        };
        let cfg = Config {
            max_delta_ratio: opts.max_delta_ratio.unwrap_or(0.0),
            ..Config::default()
        };
        assert!((cfg.max_delta_ratio - 0.5).abs() < 1e-6);
    }
}
