// SPDX-License-Identifier: BUSL-1.1

//! Construct an ephemeral `DeltaGliderEngine` for CLI subcommands.
//!
//! The proxy server hot-reloads a full `Config` from disk; the CLI
//! has only flag-supplied bits. This factory takes the CLI bits,
//! starts from `Config::default()`, overrides `backend` (and any
//! optional knobs), and hands the result to the same `DynEngine::new`
//! the server uses. No new engine surface.

use crate::cli::aws_creds::ResolvedCreds;
use crate::config::{BackendConfig, Config};
use crate::deltaglider::DynEngine;
use crate::storage::{S3Backend, StorageError};

/// Inputs the CLI gathers from its flags + the credential resolver.
#[derive(Debug, Clone)]
pub struct CliEngineOpts {
    pub endpoint: Option<String>,
    pub region: String,
    pub force_path_style: bool,
    pub access_key_id: String,
    pub secret_access_key: String,
    /// Session token of temporary (STS) credentials, if any.
    pub session_token: Option<String>,
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
        session_token: opts.session_token,
        endpoint: opts.endpoint,
        region: opts.region,
        force_path_style: opts.force_path_style,
        access_key_id: Some(opts.access_key_id),
        secret_access_key: Some(opts.secret_access_key),
        allow_local: opts.allow_local,
    };
    let mut cfg = Config {
        backend,
        max_delta_ratio: opts
            .max_delta_ratio
            .unwrap_or_else(crate::config::default_max_delta_ratio),
        ..Config::default()
    };
    if let Some(size) = opts.max_object_size {
        cfg.max_object_size = size;
    }

    let engine = DynEngine::new(&cfg, None).await?;
    Ok(engine)
}

/// Build a raw SDK client for the verbs that talk to S3 directly
/// (`purge`, `bucket-acl`). The one place that turns resolved
/// credentials into a client, so the session token cannot be dropped
/// at a call site.
pub async fn build_raw_s3_client(
    creds: &ResolvedCreds,
    endpoint: Option<String>,
    force_path_style: bool,
) -> Result<aws_sdk_s3::Client, StorageError> {
    let allow_local = crate::cli::ls::should_allow_local(endpoint.as_deref());
    let backend = BackendConfig::S3 {
        session_token: creds.session_token.clone(),
        endpoint,
        region: creds.region.clone().unwrap_or_else(|| "us-east-1".into()),
        force_path_style,
        access_key_id: Some(creds.access_key_id.clone()),
        secret_access_key: Some(creds.secret_access_key.clone()),
        allow_local,
    };
    S3Backend::build_client(&backend).await
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
            session_token: None,
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

    use crate::cli::aws_creds::CredsSource;

    fn creds(token: Option<&str>) -> ResolvedCreds {
        ResolvedCreds {
            access_key_id: "AK".into(),
            secret_access_key: "SK".into(),
            session_token: token.map(str::to_string),
            region: Some("eu-central-1".into()),
            source: CredsSource::Env,
        }
    }

    /// Run `send` against a local socket and return the head of the
    /// first request it makes, so the test sees what goes on the wire.
    async fn capture_head<F, Fut>(send: F) -> String
    where
        F: FnOnce(String) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 16 * 1024];
            let n = sock.read(&mut buf).await.unwrap();
            let _ = sock
                .write_all(
                    b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
            String::from_utf8_lossy(&buf[..n]).to_lowercase()
        });
        send(endpoint).await;
        server.await.unwrap()
    }

    async fn raw_head(creds: &ResolvedCreds) -> String {
        capture_head(|endpoint| async move {
            let client = build_raw_s3_client(creds, Some(endpoint), true)
                .await
                .unwrap();
            let _ = client.list_buckets().send().await;
        })
        .await
    }

    /// Temporary (STS) credentials only work with their session token.
    #[tokio::test]
    async fn raw_client_sends_the_session_token() {
        let head = raw_head(&creds(Some("TOKEN123"))).await;
        assert!(head.contains("x-amz-security-token: token123"), "{head}");
    }

    #[tokio::test]
    async fn raw_client_without_token_sends_none() {
        let head = raw_head(&creds(None)).await;
        assert!(head.contains("authorization: aws4-hmac-sha256"), "{head}");
        assert!(!head.contains("x-amz-security-token"), "{head}");
    }

    /// The engine verbs (cp, sync, rm, ls, stats, verify, migrate) sign
    /// through `DynEngine`, so the token must reach `BackendConfig::S3`.
    #[tokio::test]
    async fn engine_sends_the_session_token() {
        let head = capture_head(|endpoint| async move {
            let engine = build_cli_engine(CliEngineOpts {
                endpoint: Some(endpoint),
                region: "us-east-1".into(),
                force_path_style: true,
                access_key_id: "AK".into(),
                secret_access_key: "SK".into(),
                session_token: Some("ENGTOKEN".into()),
                max_delta_ratio: None,
                max_object_size: None,
                allow_local: true,
            })
            .await
            .unwrap();
            let _ = engine
                .list_objects("releases", "", None, 1, None, false)
                .await;
        })
        .await;
        assert!(head.contains("x-amz-security-token: engtoken"), "{head}");
    }

    /// The token is runtime-only: it must never reach exported or
    /// persisted config, and config cannot set it.
    #[test]
    fn session_token_is_never_serialized() {
        let b = BackendConfig::S3 {
            endpoint: None,
            region: "us-east-1".into(),
            force_path_style: false,
            access_key_id: Some("AK".into()),
            secret_access_key: Some("SK".into()),
            allow_local: false,
            session_token: Some("SECRET-TOKEN".into()),
        };
        let yaml = serde_yaml::to_string(&b).unwrap();
        assert!(!yaml.contains("SECRET-TOKEN"), "{yaml}");
        let back: BackendConfig = serde_yaml::from_str(&yaml).unwrap();
        assert!(matches!(back, BackendConfig::S3 { session_token: None, .. }));
    }

    #[test]
    fn max_delta_ratio_override_lands() {
        let opts = CliEngineOpts {
            endpoint: None,
            region: "us-east-1".into(),
            force_path_style: true,
            access_key_id: "AK".into(),
            secret_access_key: "SK".into(),
            session_token: None,
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
