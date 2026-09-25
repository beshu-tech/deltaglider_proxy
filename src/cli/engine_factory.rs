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
    /// Session token of temporary (STS) credentials, if any. The engine
    /// cannot sign with it yet (see [`session_token_note`]).
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
    if let Some(note) = session_token_note(&opts) {
        eprintln!("{note}");
    }
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

/// Pure: the warning for a session token the engine verbs cannot send.
/// `BackendConfig::S3` has no token slot, so the engine signs with the
/// key pair alone; temporary credentials then fail with a 403 whose
/// cause is not obvious. Say it up front instead.
pub fn session_token_note(opts: &CliEngineOpts) -> Option<&'static str> {
    opts.session_token.as_ref().map(|_| {
        "warning: a session token is set (AWS_SESSION_TOKEN or aws_session_token), \
         but this command cannot send it yet; temporary (STS) credentials will be \
         rejected. Use long-term access keys for this command."
    })
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
        endpoint,
        region: creds.region.clone().unwrap_or_else(|| "us-east-1".into()),
        force_path_style,
        access_key_id: Some(creds.access_key_id.clone()),
        secret_access_key: Some(creds.secret_access_key.clone()),
        allow_local,
    };
    let client = S3Backend::build_client(&backend).await?;
    let Some(token) = creds.session_token.clone() else {
        return Ok(client);
    };
    // `build_client` signs with a static key pair only; swap in the
    // same pair plus the token and keep every other client setting.
    let with_token = aws_credential_types::Credentials::new(
        &creds.access_key_id,
        &creds.secret_access_key,
        Some(token),
        None,
        "deltaglider_proxy-cli",
    );
    let conf = client
        .config()
        .to_builder()
        .credentials_provider(with_token)
        .build();
    Ok(aws_sdk_s3::Client::from_conf(conf))
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

    /// Send one request from `client` to a local socket and return the
    /// raw request head, so the test sees what goes on the wire.
    async fn request_head(creds: &ResolvedCreds) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let client = build_raw_s3_client(creds, Some(endpoint), true)
            .await
            .unwrap();
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
        let _ = client.list_buckets().send().await;
        server.await.unwrap()
    }

    /// Temporary (STS) credentials only work with their session token.
    #[tokio::test]
    async fn raw_client_sends_the_session_token() {
        let head = request_head(&creds(Some("TOKEN123"))).await;
        assert!(head.contains("x-amz-security-token: token123"), "{head}");
    }

    #[tokio::test]
    async fn raw_client_without_token_sends_none() {
        let head = request_head(&creds(None)).await;
        assert!(head.contains("authorization: aws4-hmac-sha256"), "{head}");
        assert!(!head.contains("x-amz-security-token"), "{head}");
    }

    #[test]
    fn engine_verbs_warn_about_a_session_token_they_cannot_send() {
        let mut opts = CliEngineOpts {
            endpoint: None,
            region: "us-east-1".into(),
            force_path_style: true,
            access_key_id: "AK".into(),
            secret_access_key: "SK".into(),
            session_token: None,
            max_delta_ratio: None,
            max_object_size: None,
            allow_local: false,
        };
        assert!(session_token_note(&opts).is_none());
        opts.session_token = Some("T".into());
        assert!(session_token_note(&opts).unwrap().contains("session token"));
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
