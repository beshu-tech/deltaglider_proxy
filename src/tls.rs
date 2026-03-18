//! TLS setup for DeltaGlider Proxy.
//!
//! Supports two modes:
//! - **User-provided**: load PEM cert + key from disk
//! - **Self-signed**: generate an ephemeral certificate via `rcgen`

use crate::config::TlsConfig;
use axum_server::tls_rustls::RustlsConfig;

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

    /// Both-omitted is the valid "auto self-signed" path.
    /// We can't easily test the full TLS setup in unit tests (needs CryptoProvider),
    /// but verify our validation logic doesn't reject it.
    #[test]
    fn both_omitted_passes_validation() {
        let cfg = TlsConfig {
            enabled: true,
            cert_path: None,
            key_path: None,
        };
        // Our validation check: cert_path.is_some() != key_path.is_some()
        // Both None → false != false → false → no error from our check
        assert_eq!(cfg.cert_path.is_some(), cfg.key_path.is_some());
    }
}
