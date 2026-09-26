// SPDX-License-Identifier: BUSL-1.1

//! The TLS listener, end to end: the proxy boots with `tls.enabled: true`
//! (self-signed and user PEM) and answers HTTPS requests. Regression: with
//! two rustls crypto providers in the dependency tree, the first TLS
//! handshake panicked because no process-default provider was installed.

use crate::common::{TestServer, TestTls, TEST_BOOTSTRAP_PASSWORD};

/// Signs in to the admin API over HTTPS and returns the `Set-Cookie` value.
async fn login_cookie(client: &reqwest::Client, endpoint: &str) -> String {
    let resp = client
        .post(format!("{endpoint}/_/api/admin/login"))
        .json(&serde_json::json!({ "password": TEST_BOOTSTRAP_PASSWORD }))
        .send()
        .await
        .expect("HTTPS admin login");
    assert!(resp.status().is_success(), "login: {}", resp.status());
    resp.headers()
        .get("set-cookie")
        .expect("login sets a session cookie")
        .to_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn self_signed_listener_serves_https() {
    let server = TestServer::builder().tls(TestTls::SelfSigned).build().await;
    let endpoint = server.endpoint();
    assert!(endpoint.starts_with("https://"));
    let client = reqwest::Client::builder()
        .no_proxy()
        .danger_accept_invalid_certs(true)
        .build()
        .unwrap();

    let health = client
        .get(format!("{endpoint}/_/health"))
        .send()
        .await
        .expect("HTTPS health request");
    assert_eq!(health.status(), 200);

    // The listener has TLS from the YAML only (no DGP_TLS_ENABLED), so the
    // session cookie must still be Secure.
    let cookie = login_cookie(&client, &endpoint).await;
    assert!(
        cookie.contains("; Secure"),
        "session cookie over TLS lacks Secure: {cookie}"
    );
}

#[tokio::test]
async fn user_pem_listener_serves_https_with_a_verified_certificate() {
    let dir = tempfile::tempdir().unwrap();
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = rcgen::CertificateParams::new(vec!["127.0.0.1".to_string()])
        .unwrap()
        .self_signed(&key)
        .unwrap();
    let cert_path = dir.path().join("cert.pem");
    let key_path = dir.path().join("key.pem");
    std::fs::write(&cert_path, cert.pem()).unwrap();
    std::fs::write(&key_path, key.serialize_pem()).unwrap();

    let server = TestServer::builder()
        .tls(TestTls::Pem {
            cert_path: cert_path.display().to_string(),
            key_path: key_path.display().to_string(),
        })
        .build()
        .await;
    let endpoint = server.endpoint();

    // Full verification: the client trusts only this certificate.
    let client = reqwest::Client::builder()
        .no_proxy()
        .tls_built_in_root_certs(false)
        .add_root_certificate(reqwest::Certificate::from_pem(cert.pem().as_bytes()).unwrap())
        .build()
        .unwrap();
    let health = client
        .get(format!("{endpoint}/_/health"))
        .send()
        .await
        .expect("verified HTTPS health request");
    assert_eq!(health.status(), 200);

    let cookie = login_cookie(&client, &endpoint).await;
    assert!(cookie.contains("; Secure"), "cookie: {cookie}");

    // Plain HTTP to the TLS port gets no HTTP answer.
    let plain = format!("http://{}", endpoint.trim_start_matches("https://"));
    let r = reqwest::Client::new()
        .get(format!("{plain}/_/health"))
        .send()
        .await;
    assert!(
        r.map(|r| !r.status().is_success()).unwrap_or(true),
        "plain HTTP must not be served on the TLS port"
    );
}
