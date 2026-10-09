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
    /// How long the discovery document takes to answer.
    discovery_delay: std::time::Duration,
}

async fn discovery(State(idp): State<Arc<Idp>>) -> Json<Value> {
    tokio::time::sleep(idp.discovery_delay).await;
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
    start_idp_with_delay(std::time::Duration::ZERO).await
}

async fn start_idp_with_delay(discovery_delay: std::time::Duration) -> (String, String) {
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
        discovery_delay,
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

    // 2b. H9: Test Connection runs on the unsaved form. The body's fields
    //     override the saved ones (here: the CA), a blank secret keeps the
    //     saved one, and the saved provider does not change.
    let with_ca = json!({ "extra_config": {
        "allow_local": true, "ca_cert_path": ca_path.display().to_string()
    }, "client_secret": "" });
    let r = admin
        .post(format!("{providers}/{id}/test"))
        .json(&with_ca)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let test: Value = r.json().await.unwrap();
    assert_eq!(test["success"], true, "{test}");
    let test: Value = admin
        .post(format!("{providers}/{id}/test"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        test["success"], false,
        "the saved provider is unchanged: {test}"
    );
    // A provider that is not saved yet (the create form).
    let mut unsaved = provider(json!({
        "allow_local": true, "ca_cert_path": ca_path.display().to_string()
    }));
    unsaved["client_secret"] = json!("");
    let r = admin
        .post(format!("{providers}/test"))
        .json(&unsaved)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let test: Value = r.json().await.unwrap();
    assert_eq!(test["success"], true, "{test}");
    // A form without a client ID fails the test, as a 200 with the cause.
    unsaved["client_id"] = json!("");
    let r = admin
        .post(format!("{providers}/test"))
        .json(&unsaved)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let test: Value = r.json().await.unwrap();
    assert_eq!(test["success"], false, "{test}");
    assert!(
        test["error"].as_str().unwrap_or("").contains("client_id"),
        "{test}"
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

    // auth-5: the OAuth non-admin's browser session reaches the bulk object
    // endpoints, which judge each request by that user's own IAM policy
    // (Alice has none, so the list is refused by policy, not by session kind).
    let r = browser
        .get(format!(
            "{ep}/_/api/admin/objects/list?bucket={}&prefix=x/",
            server.bucket()
        ))
        .send()
        .await
        .unwrap();
    let status = r.status();
    let body = r.text().await.unwrap_or_default();
    assert!(
        !body.contains("admin_session_required"),
        "an OAuth browser session was refused by kind: {status} {body}"
    );
    assert_eq!(status, 403, "{body}");
    assert!(body.contains("may not list"), "{body}");

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

/// B040: starting an OAuth login proves no credential, so it must not clear
/// the per-IP failure counter that guards secret guessing.
#[tokio::test]
async fn oauth_authorize_does_not_reset_the_brute_force_counter() {
    let (issuer, ca_pem) = start_idp().await;
    let dir = tempfile::tempdir().unwrap();
    let ca_path = dir.path().join("idp-ca.pem");
    std::fs::write(&ca_path, &ca_pem).unwrap();
    let server = TestServer::builder()
        .env("DGP_RATE_LIMIT_MAX_ATTEMPTS", "3")
        .env("DGP_RATE_LIMIT_WINDOW_SECS", "60")
        .env("DGP_RATE_LIMIT_LOCKOUT_SECS", "60")
        .build()
        .await;
    let ep = server.endpoint();
    let admin = admin_http_client(&ep).await;
    let r = admin
        .post(format!("{ep}/_/api/admin/ext-auth/providers"))
        .json(&json!({
            "name": "corp", "provider_type": "oidc", "client_id": CLIENT_ID,
            "client_secret": CLIENT_SECRET, "issuer_url": issuer,
            "scopes": "openid email profile",
            "extra_config": { "allow_local": true, "ca_cert_path": ca_path.display().to_string() },
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);

    let anon = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let mut statuses = Vec::new();
    for _ in 0..3 {
        for _ in 0..2 {
            let r = anon
                .post(format!("{ep}/_/api/iam/identity"))
                .json(&json!({ "access_key_id": "AKGUESS", "secret_access_key": "wrong" }))
                .send()
                .await
                .unwrap();
            statuses.push(r.status().as_u16());
        }
        let r = anon
            .get(format!("{ep}/_/api/admin/oauth/authorize/corp"))
            .send()
            .await
            .unwrap();
        // 307 starts the flow; 429 once the lockout covers this IP.
        assert!(
            [307, 429].contains(&r.status().as_u16()),
            "authorize: {}",
            r.status()
        );
    }
    assert!(
        statuses.contains(&429),
        "secret guessing never hit the lockout: {statuses:?}"
    );
}

/// B041/B090/B092: a client that disconnects drops the handler future. A
/// mutating admin request must still run to its end. The provider set is
/// rebuilt before the slow discovery, so a dropped request lost the steps
/// after it: the version bump that this test waits for, and the push to the
/// other nodes.
#[tokio::test]
async fn a_dropped_admin_request_still_completes_its_mutation() {
    let (issuer, ca_pem) = start_idp_with_delay(std::time::Duration::from_secs(2)).await;
    let dir = tempfile::tempdir().unwrap();
    let ca_path = dir.path().join("idp-ca.pem");
    std::fs::write(&ca_path, &ca_pem).unwrap();
    let server = TestServer::builder().build().await;
    let ep = server.endpoint();
    let admin = admin_http_client(&ep).await;
    let before = crate::common::get_ext_auth_version(&admin, &ep).await;
    let impatient = admin
        .post(format!("{ep}/_/api/admin/ext-auth/providers"))
        .timeout(std::time::Duration::from_millis(500))
        .json(&json!({
            "name": "corp", "provider_type": "oidc", "client_id": CLIENT_ID,
            "client_secret": CLIENT_SECRET, "issuer_url": issuer,
            "scopes": "openid email profile",
            "extra_config": { "allow_local": true, "ca_cert_path": ca_path.display().to_string() },
        }))
        .send()
        .await;
    assert!(impatient.is_err(), "the client was meant to give up first");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    loop {
        if crate::common::get_ext_auth_version(&admin, &ep).await > before {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the dropped request never finished its mutation (provider set not rebuilt)"
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}
