// SPDX-License-Identifier: BUSL-1.1

//! OIDC sign-in end to end, against an identity provider on a private
//! address behind a private CA — the shape of an in-house Keycloak/Dex.
//!
//! The test runs its own minimal IdP over HTTPS (discovery, JWKS, an
//! authorize endpoint that approves at once, a token endpoint that signs an
//! ES256 ID token). The proxy must:
//! - refuse the private issuer at save time without `allow_local`;
//! - with `allow_local` but without the CA, report the TLS cause;
//! - with `ca_cert_path`, complete the login and mint a session;
//! - never echo the client secret.

use crate::common::{admin_http_client, TestServer};
use axum::extract::{Form, Query, State};
use axum::response::{IntoResponse, Redirect};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const CLIENT_ID: &str = "dgp-test-client";
const CLIENT_SECRET: &str = "dgp-test-client-secret-value";

struct Idp {
    issuer: String,
    signing_pem: String,
    jwk: Value,
    /// Authorization code → nonce.
    codes: Mutex<HashMap<String, String>>,
}

async fn discovery(State(idp): State<Arc<Idp>>) -> Json<Value> {
    let i = &idp.issuer;
    Json(json!({
        "issuer": i,
        "authorization_endpoint": format!("{i}/authorize"),
        "token_endpoint": format!("{i}/token"),
        "jwks_uri": format!("{i}/jwks"),
    }))
}

async fn jwks(State(idp): State<Arc<Idp>>) -> Json<Value> {
    Json(json!({ "keys": [idp.jwk] }))
}

/// Approves at once: redirects back with a code bound to the nonce.
async fn authorize(
    State(idp): State<Arc<Idp>>,
    Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    assert_eq!(q.get("client_id").map(String::as_str), Some(CLIENT_ID));
    let code = format!("code-{}", idp.codes.lock().unwrap().len());
    idp.codes
        .lock()
        .unwrap()
        .insert(code.clone(), q.get("nonce").cloned().unwrap_or_default());
    Redirect::to(&format!(
        "{}?code={code}&state={}",
        q["redirect_uri"],
        urlencoding::encode(&q["state"])
    ))
}

async fn token(
    State(idp): State<Arc<Idp>>,
    Form(f): Form<HashMap<String, String>>,
) -> axum::response::Response {
    if f.get("client_secret").map(String::as_str) != Some(CLIENT_SECRET) {
        return (axum::http::StatusCode::UNAUTHORIZED, "bad client").into_response();
    }
    let Some(nonce) = f
        .get("code")
        .and_then(|c| idp.codes.lock().unwrap().remove(c))
    else {
        return (axum::http::StatusCode::BAD_REQUEST, "bad code").into_response();
    };
    let now = chrono::Utc::now().timestamp();
    let claims = json!({
        "iss": idp.issuer, "sub": "u-alice", "aud": CLIENT_ID,
        "iat": now, "exp": now + 300, "nonce": nonce,
        "email": "alice@acme.example", "email_verified": true, "name": "Alice",
    });
    let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::ES256);
    header.kid = Some("k1".into());
    let key = jsonwebtoken::EncodingKey::from_ec_pem(idp.signing_pem.as_bytes()).unwrap();
    let id_token = jsonwebtoken::encode(&header, &claims, &key).unwrap();
    Json(json!({ "access_token": "at", "token_type": "Bearer", "id_token": id_token }))
        .into_response()
}

/// Starts the IdP on 127.0.0.1 with a leaf certificate from a fresh CA.
/// Returns the issuer URL and the CA certificate (PEM).
async fn start_idp() -> (String, String) {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let ca_key = rcgen::KeyPair::generate().unwrap();
    let mut ca_params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "dgp test CA");
    let ca = ca_params.self_signed(&ca_key).unwrap();
    let leaf_key = rcgen::KeyPair::generate().unwrap();
    let leaf = rcgen::CertificateParams::new(vec!["127.0.0.1".to_string()])
        .unwrap()
        .signed_by(&leaf_key, &ca, &ca_key)
        .unwrap();

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let issuer = format!(
        "https://127.0.0.1:{}",
        listener.local_addr().unwrap().port()
    );

    let signing = rcgen::KeyPair::generate().unwrap(); // P-256
    let raw = signing.public_key_raw(); // 0x04 || X || Y
    let jwk = json!({
        "kty": "EC", "crv": "P-256", "kid": "k1", "alg": "ES256", "use": "sig",
        "x": URL_SAFE_NO_PAD.encode(&raw[1..33]),
        "y": URL_SAFE_NO_PAD.encode(&raw[33..65]),
    });
    let idp = Arc::new(Idp {
        issuer: issuer.clone(),
        signing_pem: signing.serialize_pem(),
        jwk,
        codes: Mutex::new(HashMap::new()),
    });
    let app = Router::new()
        .route("/.well-known/openid-configuration", get(discovery))
        .route("/jwks", get(jwks))
        .route("/authorize", get(authorize))
        .route("/token", post(token))
        .with_state(idp);
    let tls = axum_server::tls_rustls::RustlsConfig::from_pem(
        leaf.pem().into_bytes(),
        leaf_key.serialize_pem().into_bytes(),
    )
    .await
    .unwrap();
    tokio::spawn(async move {
        axum_server::from_tcp_rustls(listener, tls)
            .serve(app.into_make_service())
            .await
            .unwrap();
    });
    (issuer, ca.pem())
}

#[tokio::test]
async fn oidc_login_against_a_private_idp_with_a_private_ca() {
    let (issuer, ca_pem) = start_idp().await;
    let dir = tempfile::tempdir().unwrap();
    let ca_path = dir.path().join("idp-ca.pem");
    std::fs::write(&ca_path, &ca_pem).unwrap();

    let server = TestServer::builder().build().await;
    let ep = server.endpoint();
    let admin = admin_http_client(&ep).await;
    let providers = format!("{ep}/_/api/admin/ext-auth/providers");
    let provider = |extra: Value| {
        json!({
            "name": "corp", "provider_type": "oidc", "client_id": CLIENT_ID,
            "client_secret": CLIENT_SECRET, "issuer_url": issuer,
            "scopes": "openid email profile", "extra_config": extra,
        })
    };

    // 1. A private issuer without allow_local is refused at save time.
    let r = admin
        .post(&providers)
        .json(&provider(json!({})))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 422);
    let body: Value = r.json().await.unwrap();
    assert!(
        body["error"].as_str().unwrap_or("").contains("allow_local"),
        "{body}"
    );

    // 2. allow_local without the CA: saved (secret not echoed); the test
    //    action names the TLS cause, not only "error sending request".
    let r = admin
        .post(&providers)
        .json(&provider(json!({ "allow_local": true })))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let created: Value = r.json().await.unwrap();
    assert_eq!(created["client_secret"], "****", "{created}");
    let id = created["id"].as_i64().unwrap();
    let test: Value = admin
        .post(format!("{providers}/{id}/test"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(test["success"], false, "{test}");
    let err = test["error"].as_str().unwrap_or("");
    assert!(
        err.to_ascii_lowercase().contains("certificate") || err.contains("UnknownIssuer"),
        "the TLS cause is missing: {err}"
    );

    // 3. A CA path that does not exist is refused at save time.
    let r = admin
        .put(format!("{providers}/{id}"))
        .json(&json!({ "extra_config": { "allow_local": true, "ca_cert_path": "/nonexistent/ca.pem" } }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 422);

    // 4. With the CA, the provider works.
    let r = admin
        .put(format!("{providers}/{id}"))
        .json(&json!({ "extra_config": {
            "allow_local": true, "ca_cert_path": ca_path.display().to_string()
        } }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let updated: Value = r.json().await.unwrap();
    assert_eq!(updated["client_secret"], "****", "{updated}");
    let test: Value = admin
        .post(format!("{providers}/{id}/test"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(test["success"], true, "{test}");

    // 5. The browser flow: authorize → IdP → callback → session.
    let browser = reqwest::Client::builder()
        .cookie_store(true)
        .redirect(reqwest::redirect::Policy::none())
        .add_root_certificate(reqwest::Certificate::from_pem(ca_pem.as_bytes()).unwrap())
        .build()
        .unwrap();
    let r = browser
        .get(format!("{ep}/_/api/admin/oauth/authorize/corp"))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_redirection(), "authorize: {}", r.status());
    let to_idp = r.headers()["location"].to_str().unwrap().to_string();
    assert!(
        to_idp.starts_with(&format!("{issuer}/authorize?")),
        "{to_idp}"
    );
    let r = browser.get(&to_idp).send().await.unwrap();
    assert!(r.status().is_redirection(), "IdP authorize: {}", r.status());
    let callback = r.headers()["location"].to_str().unwrap().to_string();
    assert!(
        callback.starts_with(&format!("{ep}/_/api/admin/oauth/callback?")),
        "{callback}"
    );
    let r = browser.get(&callback).send().await.unwrap();
    let status = r.status();
    let cookies: Vec<String> = r
        .headers()
        .get_all("set-cookie")
        .iter()
        .map(|v| v.to_str().unwrap().to_string())
        .collect();
    let body = r.text().await.unwrap_or_default();
    assert_eq!(status, 302, "callback: {body}");
    assert!(
        cookies
            .iter()
            .any(|c| c.starts_with("dgp_session=") && !c.starts_with("dgp_session=;")),
        "no session cookie: {cookies:?}"
    );
    let session: Value = browser
        .get(format!("{ep}/_/api/admin/session"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(session["valid"], true, "{session}");

    // The login provisioned an external user.
    let users: Value = admin
        .get(format!("{ep}/_/api/admin/users"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        users
            .as_array()
            .unwrap()
            .iter()
            .any(|u| u["name"] == "Alice" && u["auth_source"] == "external"),
        "{users}"
    );

    // The list endpoint never shows the secret either.
    let list = admin
        .get(&providers)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(!list.contains(CLIENT_SECRET), "{list}");
}
