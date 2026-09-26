// SPDX-License-Identifier: BUSL-1.1

//! TLS setup for DeltaGlider Proxy.
//!
//! Supports two modes:
//! - **User-provided**: load PEM cert + key from disk
//! - **Self-signed**: generate an ephemeral certificate via `rcgen`

use crate::config::TlsConfig;
use axum_server::tls_rustls::RustlsConfig;

/// Install aws-lc-rs as the process-default rustls crypto provider.
///
/// The dependency tree enables both `ring` (reqwest) and `aws-lc-rs` (the
/// AWS SDK), so rustls cannot pick a default and panics on the first TLS
/// config built without an explicit provider — the HTTPS listener did that
/// on its first handshake. With a default installed, the listener and
/// reqwest (which prefers the process default) use aws-lc-rs, the same
/// provider the AWS SDK selects explicitly. Call it first thing in `main`.
/// Idempotent: a provider that is already installed stays.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

/// Whether this process serves HTTPS on its listener. Set once at startup
/// (`startup::init_tls`); the session cookies take `Secure` from it, so
/// TLS from the YAML (`advanced.tls.enabled`) counts, not only the env var.
static LISTENER_TLS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_listener_tls(on: bool) {
    LISTENER_TLS.store(on, std::sync::atomic::Ordering::Relaxed);
}

pub fn listener_tls() -> bool {
    LISTENER_TLS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Build a [`RustlsConfig`] from the given [`TlsConfig`].
///
/// When `cert_path` and `key_path` are both set, loads user-provided PEM files.
/// Otherwise generates a self-signed certificate for `localhost` / `127.0.0.1`.
pub async fn build_rustls_config(
    tls: &TlsConfig,
) -> Result<RustlsConfig, Box<dyn std::error::Error>> {
    // Reject partial TLS config: setting only cert or only key is almost certainly a mistake.
    if tls.cert_path.is_some() != tls.key_path.is_some() {
        return Err(
            "TLS misconfiguration: cert_path and key_path must both be set, or both omitted. \
             Set both for a user-provided certificate, or omit both for auto-generated self-signed."
                .into(),
        );
    }

    if let (Some(cert), Some(key)) = (&tls.cert_path, &tls.key_path) {
        Ok(RustlsConfig::from_pem_file(cert, key).await?)
    } else {
        let subject_alt_names = vec!["localhost".to_string(), "127.0.0.1".to_string()];
        let cert_params = rcgen::CertificateParams::new(subject_alt_names)?;
        let key_pair = rcgen::KeyPair::generate()?;
        let cert = cert_params.self_signed(&key_pair)?;
        let cert_pem = cert.pem();
        let key_pem = key_pair.serialize_pem();
        Ok(RustlsConfig::from_pem(cert_pem.into(), key_pem.into()).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: Setting cert_path without key_path (or vice versa) must error,
    /// not silently fall back to a self-signed certificate.
    #[tokio::test]
    async fn partial_tls_config_is_rejected() {
        let cert_only = TlsConfig {
            enabled: true,
            cert_path: Some("/tmp/cert.pem".to_string()),
            key_path: None,
        };
        assert!(build_rustls_config(&cert_only).await.is_err());

        let key_only = TlsConfig {
            enabled: true,
            cert_path: None,
            key_path: Some("/tmp/key.pem".to_string()),
        };
        assert!(build_rustls_config(&key_only).await.is_err());
    }

    /// Both-omitted is the valid "auto self-signed" path: with the process
    /// provider installed, it builds a server config.
    #[tokio::test]
    async fn self_signed_config_builds_with_the_installed_provider() {
        install_crypto_provider();
        install_crypto_provider(); // idempotent
        let cfg = TlsConfig {
            enabled: true,
            cert_path: None,
            key_path: None,
        };
        build_rustls_config(&cfg).await.expect("self-signed config");
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
    }
}
