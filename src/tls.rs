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
