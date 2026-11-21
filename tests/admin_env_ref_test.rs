// SPDX-License-Identifier: BUSL-1.1

//! S7 through the real HTTP surface: an admin cannot learn, move, persist in
//! clear or send elsewhere a secret that the operator injected through the
//! environment. Each refusal has a control that the same surface accepts, and
//! after each refusal the secret is in none of the read paths.

use crate::common;

use common::{admin_http_client, TestServer};
use reqwest::{Client, StatusCode};
use serde_json::{json, Value};

const S3_SECRET: &str = "S3CR3T-VALUE-9f8e7d";
const PUB_TOKEN: &str = "pub-token-1a2b3c4d";
const HOOK_TOKEN: &str = "hook-token-5e6f7a8b";
const DB_KEY: &str = "db-key-secret-00112233445566778899";
const SECRETS: [&str; 4] = [S3_SECRET, PUB_TOKEN, HOOK_TOKEN, DB_KEY];

/// A boot file (the flat shape of the harness) with a whole reference in a
/// credential, an embedded one in a Slack webhook URL (a bearer secret that
/// every read path masks), one in a header value and one in a numeric field.
async fn server() -> TestServer {
    TestServer::builder()
        .auth("AKTESTENV", "${env:AUDIT_S3_SECRET}")
        .env("AUDIT_S3_SECRET", S3_SECRET)
        .env("AUDIT_PUB", PUB_TOKEN)
        .env("AUDIT_CACHE", "256")
        .env("AUDIT_HOOK_TOKEN", HOOK_TOKEN)
        .env("DGP_CONFIG_DB_KEY", DB_KEY)
        .extra_yaml_root(concat!(
            "public_url: http://h.example/\n",
            "cache_size_mb: ${env:AUDIT_CACHE}\n",
            "event_delivery:\n",
            "  enabled: false\n",
            "  format: slack\n",
            "  webhook_url: \"https://hooks.slack.com/services/${env:AUDIT_PUB}\"\n",
            "  webhook_headers:\n",
            "    Authorization: \"Bearer ${env:AUDIT_HOOK_TOKEN}\"\n",
        ))
        .build()
        .await
}

fn url(server: &TestServer, path: &str) -> String {
    format!("{}/_/api/admin/{path}", server.endpoint())
}

async fn text(admin: &Client, server: &TestServer, path: &str) -> String {
    let resp = admin.get(url(server, path)).send().await.unwrap();
    assert!(resp.status().is_success(), "{path}: {}", resp.status());
    resp.text().await.unwrap()
}

/// Every read path an admin has, as `(name, text)`.
async fn read_paths(admin: &Client, server: &TestServer) -> Vec<(String, String)> {
    let mut out = vec![
        (
            "GET /config".to_string(),
            text(admin, server, "config").await,
        ),
        (
            "export".to_string(),
            text(admin, server, "config/export").await,
        ),
    ];
    for section in ["admission", "access", "storage", "advanced"] {
        out.push((
            format!("section {section}"),
            text(admin, server, &format!("config/section/{section}")).await,
        ));
    }
    out.push((
        "GET /backends".to_string(),
        text(admin, server, "backends").await,
    ));
    out.push((
        "the config file".to_string(),
        std::fs::read_to_string(server.config_path()).unwrap_or_default(),
    ));
    out
}

/// The secrets are in no read path.
async fn assert_no_leak(admin: &Client, server: &TestServer, what: &str) {
    for (name, body) in read_paths(admin, server).await {
        for secret in SECRETS {
            assert!(!body.contains(secret), "{what}: {secret} is in {name}");
        }
    }
}

async fn export(admin: &Client, server: &TestServer) -> String {
    text(admin, server, "config/export").await
}

async fn post_doc(
    admin: &Client,
    server: &TestServer,
    route: &str,
    yaml: &str,
) -> (StatusCode, Value) {
    let resp = admin
        .post(url(server, route))
        .json(&json!({ "yaml": yaml }))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// A document of the boot file's content. `secret` is the `access` secret
/// line; `extra` is appended to `advanced`.
fn doc(server: &TestServer, secret: &str, extra: &str) -> String {
    format!(
        "access:\n  access_key_id: AKTESTENV\n  secret_access_key: {secret}\n\
         storage:\n  backend:\n    type: filesystem\n    path: {}\n\
         advanced:\n  listen_addr: 127.0.0.1:{}\n  public_url: http://h.example/\n  cache_size_mb: ${{env:AUDIT_CACHE}}\n\
         {extra}",
        server.data_dir().unwrap().display(),
        server.endpoint().rsplit(':').next().unwrap(),
    )
}

const EVENTS: &str = "  event_delivery:\n    enabled: false\n    format: slack\n    webhook_url: \"https://hooks.slack.com/services/${env:AUDIT_PUB}\"\n    webhook_headers:\n      Authorization: \"Bearer ${env:AUDIT_HOOK_TOKEN}\"\n";

const HOOK_EVENTS: &str = "  event_delivery:\n    enabled: false\n    webhook_url: https://hooks.example.com/a\n    webhook_headers:\n      Authorization: \"Bearer ${env:AUDIT_HOOK_TOKEN}\"\n";

/// A boot file whose webhook URL is plain and whose header holds a token
/// from the environment.
async fn hook_server() -> TestServer {
    TestServer::builder()
        .auth("AKTESTENV", "${env:AUDIT_S3_SECRET}")
        .env("AUDIT_S3_SECRET", S3_SECRET)
        .env("AUDIT_CACHE", "256")
        .env("AUDIT_HOOK_TOKEN", HOOK_TOKEN)
        .extra_yaml_root(concat!(
            "public_url: http://h.example/\n",
            "cache_size_mb: ${env:AUDIT_CACHE}\n",
            "event_delivery:\n",
            "  enabled: false\n",
            "  webhook_url: https://hooks.example.com/a\n",
            "  webhook_headers:\n",
            "    Authorization: \"Bearer ${env:AUDIT_HOOK_TOKEN}\"\n",
        ))
        .build()
        .await
}

/// [`doc`] for the `hook_server` boot file.
fn hook_doc(server: &TestServer, events: &str) -> String {
    doc(server, "${env:AUDIT_S3_SECRET}", events)
}

fn error_of(body: &Value) -> String {
    body["error"].as_str().unwrap_or_default().to_string()
}

/// The audit PoC (`03_envref`): the secret reused inside `public_url`.
/// Control: the boot file's own references, and the export, are accepted.
#[tokio::test]
async fn a_reference_reused_in_another_field_is_refused() {
    let server = server().await;
    let admin = admin_http_client(&server.endpoint()).await;
    assert_no_leak(&admin, &server, "at boot").await;

    // Control: the boot file's references at their own places.
    let good = doc(&server, "${env:AUDIT_S3_SECRET}", EVENTS);
    let (status, body) = post_doc(&admin, &server, "config/validate", &good).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // Control: the export, applied unchanged.
    let exported = export(&admin, &server).await;
    let (status, body) = post_doc(&admin, &server, "config/apply", &exported).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["applied"], true, "{body}");
    assert_no_leak(&admin, &server, "after the unchanged export").await;

    // The attack: the same secret inside public_url; the same whole value;
    // a name that the env has but that no file used.
    let public = "public_url: http://h.example/\n";
    for (attack, path) in [
        (
            good.replace(
                public,
                "public_url: http://h.example/${env:AUDIT_S3_SECRET}\n",
            ),
            "/public_url",
        ),
        (
            good.replace(public, "public_url: ${env:AUDIT_S3_SECRET}\n"),
            "/public_url",
        ),
        (
            good.replace(public, "public_url: http://h.example/${env:HOME}\n"),
            "/public_url",
        ),
        // The embedded Slack token moved into the header value.
        (
            good.replace("Bearer ${env:AUDIT_HOOK_TOKEN}", "${env:AUDIT_PUB}"),
            "/event_delivery/webhook_headers/Authorization",
        ),
    ] {
        for route in ["config/validate", "config/apply"] {
            let (status, body) = post_doc(&admin, &server, route, &attack).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{route}: {body}");
            let error = error_of(&body);
            assert!(error.contains(path) && error.contains("refused"), "{error}");
            for secret in SECRETS {
                assert!(!error.contains(secret), "{error}");
            }
            assert_ne!(body["applied"], true);
        }
        assert_no_leak(&admin, &server, "after a refused document").await;
    }
    // The refusals left the config as it was.
    let config = text(&admin, &server, "config").await;
    assert!(!config.contains(S3_SECRET), "{config}");
}

/// A YAML alias copies an accepted reference: into a header, and into an
/// admission reject message. Each goes through the judge by its own path.
#[tokio::test]
async fn an_alias_cannot_copy_an_accepted_reference_to_another_field() {
    let server = server().await;
    let admin = admin_http_client(&server.endpoint()).await;
    // Control: the anchor alone is the boot file's own field.
    let anchored = doc(&server, "&s ${env:AUDIT_S3_SECRET}", EVENTS);
    let (status, body) = post_doc(&admin, &server, "config/validate", &anchored).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let into_header = doc(
        &server,
        "&s ${env:AUDIT_S3_SECRET}",
        &EVENTS.replace(
            "      Authorization:",
            "      X-Leak: *s\n      Authorization:",
        ),
    );
    let into_reject = format!(
        "{anchored}\
         admission:\n  blocks:\n  - name: leak\n    match: {{}}\n    action:\n      type: reject\n      status: 503\n      message: *s\n"
    );
    for (attack, path) in [
        (into_header, "/event_delivery/webhook_headers/X-Leak"),
        (into_reject, "/admission_blocks/[leak]/action/message"),
    ] {
        for route in ["config/validate", "config/apply"] {
            let (status, body) = post_doc(&admin, &server, route, &attack).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{route}: {body}");
            let error = error_of(&body);
            assert!(error.contains(path), "{error}");
            assert!(!error.contains(S3_SECRET), "{error}");
        }
        assert_no_leak(&admin, &server, "after an alias").await;
    }
}

/// `$${env:DGP_CONFIG_DB_KEY}` in a document and a raw `${env:...}` by PATCH
/// are literal text: after a restart the fields hold the text, not the key.
#[tokio::test]
async fn an_escaped_or_patched_reference_stays_literal_after_a_restart() {
    let mut server = server().await;
    let admin = admin_http_client(&server.endpoint()).await;

    // An escaped reference in a document: accepted, literal.
    let escaped = doc(
        &server,
        "${env:AUDIT_S3_SECRET}",
        &format!("  config_sync_object_key: \"x-$${{env:DGP_CONFIG_DB_KEY}}\"\n{EVENTS}"),
    );
    let (status, body) = post_doc(&admin, &server, "config/apply", &escaped).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["applied"], true, "{body}");

    // A raw reference by PATCH: stored as the text.
    let resp = admin
        .put(url(&server, "config"))
        .json(&json!({ "access_key_id": "${env:DGP_CONFIG_DB_KEY}" }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{}", resp.status());

    let literal = "${env:DGP_CONFIG_DB_KEY}";
    let check = |what: &'static str, server: &TestServer, admin: &Client| {
        let (admin, config, file) = (
            admin.clone(),
            url(server, "config"),
            server.config_path().to_owned(),
        );
        async move {
            let config = admin
                .get(config)
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap();
            assert!(
                config.contains(literal),
                "{what}: the field holds the text: {config}"
            );
            assert!(!config.contains(DB_KEY), "{what}: {config}");
            let file = std::fs::read_to_string(file).unwrap();
            assert!(
                file.contains("$${env:DGP_CONFIG_DB_KEY}"),
                "{what}: escaped in the file:\n{file}"
            );
            assert!(!file.contains(DB_KEY), "{what}: {file}");
        }
    };
    check("before the restart", &server, &admin).await;

    server.respawn_with_env(&[]).await;
    let admin = admin_http_client(&server.endpoint()).await;
    check("after the restart", &server, &admin).await;
    let section = text(&admin, &server, "config/section/advanced").await;
    assert!(
        section.contains("x-$${env:DGP_CONFIG_DB_KEY}")
            || section.contains("x-${env:DGP_CONFIG_DB_KEY}"),
        "{section}"
    );
    assert!(!section.contains(DB_KEY), "{section}");
}

/// A whole reference in a numeric field: refused where no file has it, and
/// accepted at the field where the boot file has it.
#[tokio::test]
async fn a_whole_reference_in_a_numeric_field_follows_the_same_rule() {
    let server = server().await;
    let admin = admin_http_client(&server.endpoint()).await;
    let control = doc(&server, "${env:AUDIT_S3_SECRET}", EVENTS);
    let (status, body) = post_doc(&admin, &server, "config/validate", &control).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let attack = control.replace(
        "cache_size_mb: ${env:AUDIT_CACHE}",
        "cache_size_mb: ${env:AUDIT_S3_SECRET}",
    );
    let (status, body) = post_doc(&admin, &server, "config/apply", &attack).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let error = error_of(&body);
    assert!(
        error.contains("/cache_size_mb") && !error.contains(S3_SECRET),
        "{error}"
    );
    assert_no_leak(&admin, &server, "after the numeric attack").await;
}

/// The webhook URL stands next to an env-sourced header. Every step of the
/// "literal, then move, then put the reference back" sequence is refused, and
/// the same fields of a config with no env credential stay free.
#[tokio::test]
async fn the_frozen_header_and_its_url_never_change_in_any_sequence() {
    let server = hook_server().await;
    let admin = admin_http_client(&server.endpoint()).await;
    let control = hook_doc(&server, HOOK_EVENTS);
    let (status, body) = post_doc(&admin, &server, "config/validate", &control).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let moved_url = HOOK_EVENTS.replace(
        "https://hooks.example.com/a",
        "https://attacker.example.com/b",
    );
    let literal_header = HOOK_EVENTS.replace("Bearer ${env:AUDIT_HOOK_TOKEN}", "Bearer typed");
    let steps = [
        // 1: the header becomes a literal and the URL moves.
        (
            literal_header.replace(
                "https://hooks.example.com/a",
                "https://attacker.example.com/b",
            ),
            "/event_delivery/webhook_headers/Authorization",
        ),
        // 2: the reference is put back and the URL stays moved.
        (moved_url.clone(), "/event_delivery/webhook_url"),
        // 3: the header alone becomes a literal.
        (
            literal_header,
            "/event_delivery/webhook_headers/Authorization",
        ),
        // 4: the header is removed.
        (
            HOOK_EVENTS.replace(
                "      Authorization: \"Bearer ${env:AUDIT_HOOK_TOKEN}\"\n",
                "",
            ),
            "/event_delivery/webhook_headers/Authorization",
        ),
    ];
    for (events, path) in steps {
        for route in ["config/validate", "config/apply"] {
            let (status, body) =
                post_doc(&admin, &server, route, &hook_doc(&server, &events)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{route}: {body}");
            let error = error_of(&body);
            assert!(error.contains(path), "{error}");
            assert!(
                error.contains("comes from the environment")
                    && error.contains("config file and restart"),
                "{error}"
            );
            assert!(!error.contains(HOOK_TOKEN), "{error}");
        }
        assert_no_leak(&admin, &server, "after a refused step").await;
    }
    // After the refused sequence the file still names the variable.
    let file = std::fs::read_to_string(server.config_path()).unwrap();
    assert!(file.contains("${env:AUDIT_HOOK_TOKEN}"), "{file}");
    assert!(file.contains("https://hooks.example.com/a"), "{file}");

    // Control: an unrelated change next to the header is accepted.
    let other = HOOK_EVENTS.replace("enabled: false", "enabled: false\n    batch_size: 7");
    let (status, body) =
        post_doc(&admin, &server, "config/apply", &hook_doc(&server, &other)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_no_leak(&admin, &server, "after the control").await;
}

/// Control: with no env credential next to it, a webhook URL moves freely
/// and a header value is the admin's to change.
#[tokio::test]
async fn a_webhook_url_with_no_env_credential_beside_it_moves_freely() {
    let server = TestServer::builder()
        .env("AUDIT_CACHE", "256")
        .extra_yaml_root(concat!(
            "cache_size_mb: ${env:AUDIT_CACHE}\n",
            "event_delivery:\n",
            "  enabled: false\n",
            "  webhook_url: https://hooks.example.com/a\n",
            "  webhook_headers:\n",
            "    Authorization: Bearer typed-by-the-operator\n",
        ))
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;
    let events = "  event_delivery:\n    enabled: false\n    webhook_url: https://elsewhere.example.com/b\n    webhook_headers:\n      Authorization: Bearer typed-by-the-admin\n";
    let yaml = format!(
        "access:\n  access_key_id: test\n  secret_access_key: test\n\
         storage:\n  backend:\n    type: filesystem\n    path: {}\n\
         advanced:\n  listen_addr: 127.0.0.1:{}\n  cache_size_mb: ${{env:AUDIT_CACHE}}\n{events}",
        server.data_dir().unwrap().display(),
        server.endpoint().rsplit(':').next().unwrap(),
    );
    let (status, body) = post_doc(&admin, &server, "config/apply", &yaml).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let file = std::fs::read_to_string(server.config_path()).unwrap();
    assert!(file.contains("elsewhere.example.com"), "{file}");
    assert!(file.contains("${env:AUDIT_CACHE}"), "{file}");
}

/// A PATCH that writes a field of the frozen set is refused whatever it
/// writes: the right guess and a wrong one get the same answer.
#[tokio::test]
async fn a_patch_of_a_frozen_field_has_no_equality_oracle() {
    let server = server().await;
    let admin = admin_http_client(&server.endpoint()).await;
    let mut answers = Vec::new();
    for guess in [S3_SECRET, "a-wrong-guess-0123456789"] {
        let resp = admin
            .put(url(&server, "config"))
            .json(&json!({ "secret_access_key": guess }))
            .send()
            .await
            .unwrap();
        let status = resp.status();
        let body: Value = resp.json().await.unwrap();
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        let error = error_of(&body);
        assert!(
            error.contains("/secret_access_key") && error.contains("comes from the environment"),
            "{error}"
        );
        assert!(!error.contains(S3_SECRET), "{error}");
        answers.push((status, body));
    }
    assert_eq!(answers[0], answers[1], "the answer tells the guesses apart");
    // Control: a field that the file does not give to the environment.
    let resp = admin
        .put(url(&server, "config"))
        .json(&json!({ "max_delta_ratio": 0.4 }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{}", resp.status());
    assert_no_leak(&admin, &server, "after the patches").await;
}

const CORP_SECRET: &str = "corp-oidc-secret-77aa88bb99cc";

/// A declarative boot file whose OIDC provider has an env-sourced secret.
async fn provider_server() -> TestServer {
    TestServer::builder()
        .env("CORP_OIDC_SECRET", CORP_SECRET)
        .extra_yaml_root(concat!(
            "iam_mode: declarative\n",
            "iam_users:\n",
            "  - name: dana\n",
            "    access_key_id: AKDANA0001\n",
            "    secret_access_key: dana-secret-literal-0001\n",
            "    enabled: true\n",
            "    permissions:\n",
            "      - effect: Allow\n",
            "        actions: [read]\n",
            "        resources: [\"*\"]\n",
            "auth_providers:\n",
            "  - name: corp\n",
            "    provider_type: oidc\n",
            "    client_id: dgp\n",
            "    client_secret: ${env:CORP_OIDC_SECRET}\n",
            "    issuer_url: https://idp.example.com\n",
        ))
        .build()
        .await
}

async fn backup_text(admin: &Client, server: &TestServer) -> String {
    use std::io::Read;
    let zip = admin
        .get(url(server, "backup"))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip.to_vec())).unwrap();
    let mut all = String::new();
    for i in 0..archive.len() {
        let mut f = archive.by_index(i).unwrap();
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).unwrap();
        all.push_str(&String::from_utf8_lossy(&buf));
    }
    all
}

/// The provider's secret and its issuer are frozen together: the literal,
/// then the move, then the reference put back, are each refused.
#[tokio::test]
async fn a_provider_secret_from_the_environment_and_its_issuer_stay_frozen() {
    let server = provider_server().await;
    let admin = admin_http_client(&server.endpoint()).await;
    let exported = export(&admin, &server).await;
    assert!(exported.contains("${env:CORP_OIDC_SECRET}"), "{exported}");
    assert!(!exported.contains(CORP_SECRET), "{exported}");
    let (status, body) = post_doc(&admin, &server, "config/validate", &exported).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let moved = exported.replace("https://idp.example.com", "https://evil.example.com");
    assert_ne!(moved, exported, "the export names the issuer");
    let literal = exported.replace("${env:CORP_OIDC_SECRET}", "typed-by-the-admin");
    for (attack, path) in [
        (
            literal.replace("https://idp.example.com", "https://evil.example.com"),
            "/auth_providers/[corp]/client_secret",
        ),
        (moved, "/auth_providers/[corp]/issuer_url"),
        (literal, "/auth_providers/[corp]/client_secret"),
    ] {
        for route in ["config/validate", "config/apply"] {
            let (status, body) = post_doc(&admin, &server, route, &attack).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{route}: {body}");
            let error = error_of(&body);
            assert!(error.contains(path), "{error}");
            assert!(error.contains("comes from the environment"), "{error}");
            assert!(!error.contains(CORP_SECRET), "{error}");
        }
    }
    let file = std::fs::read_to_string(server.config_path()).unwrap();
    assert!(file.contains("${env:CORP_OIDC_SECRET}"), "{file}");
    assert!(file.contains("https://idp.example.com"), "{file}");
    assert!(!file.contains(CORP_SECRET), "{file}");
}

/// The flip to GUI mode (which would leave the secret in the IAM database
/// with an editable issuer) and the removal of the provider are refused; the
/// secret is in no backup file.
#[tokio::test]
async fn the_gui_flip_cannot_detach_a_provider_whose_secret_comes_from_the_environment() {
    let server = provider_server().await;
    let admin = admin_http_client(&server.endpoint()).await;
    for body in [
        json!({ "iam_mode": "gui", "auth_providers": [] }),
        json!({ "iam_mode": "gui" }),
        json!({ "auth_providers": [] }),
    ] {
        for validate in [true, false] {
            let route = if validate {
                "config/section/access/validate"
            } else {
                "config/section/access"
            };
            let request = if validate {
                admin.post(url(&server, route))
            } else {
                admin.put(url(&server, route))
            };
            let resp = request.json(&body).send().await.unwrap();
            let status = resp.status();
            let reply: Value = resp.json().await.unwrap();
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body} {route}: {reply}");
            let error = error_of(&reply);
            assert!(error.contains("comes from the environment"), "{error}");
            assert!(!error.contains(CORP_SECRET), "{error}");
        }
    }
    let backup = backup_text(&admin, &server).await;
    assert!(!backup.contains(CORP_SECRET), "the backup holds the secret");
    let full_export = text(
        &admin,
        &server,
        "config/declarative-iam-export?include_secrets=true",
    )
    .await;
    assert!(
        !full_export.contains(CORP_SECRET),
        "the full IAM export holds the secret"
    );
    // Control: a secret that the file holds as a literal is still exported.
    assert!(
        full_export.contains("dana-secret-literal-0001"),
        "{full_export}"
    );
    // The provider API refuses an issuer change too.
    let providers: Value = admin
        .get(url(&server, "ext-auth/providers"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = providers[0]["id"].as_i64().expect("the provider row");
    let resp = admin
        .put(url(&server, &format!("ext-auth/providers/{id}")))
        .json(&json!({ "issuer_url": "https://evil.example.com" }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_client_error(), "{}", resp.status());
    let after: Value = admin
        .get(url(&server, "ext-auth/providers"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(after[0]["issuer_url"], "https://idp.example.com");
}

/// A backend whose path comes from the environment is not deleted: the
/// name would come back with another definition.
#[tokio::test]
async fn a_backend_with_an_env_field_is_not_deleted_and_recreated() {
    let dir_a = tempfile::tempdir().unwrap();
    let dir_b = tempfile::tempdir().unwrap();
    let server = TestServer::builder()
        .env("AUDIT_BK_PATH", dir_b.path().to_str().unwrap())
        .extra_yaml_storage_section(&format!(
            "backends:\n  - name: bk-a\n    type: filesystem\n    path: \"{}\"\n  - name: bk-b\n    type: filesystem\n    path: \"${{env:AUDIT_BK_PATH}}\"\ndefault_backend: bk-a\n",
            dir_a.path().display()
        ))
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;
    let other = tempfile::tempdir().unwrap();
    let resp = admin
        .delete(url(&server, "backends/bk-b"))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body = resp.text().await.unwrap();
    assert!(status.is_client_error(), "delete bk-b: {status} {body}");
    assert!(body.contains("comes from the environment"), "{body}");
    let resp = admin
        .post(url(&server, "backends"))
        .json(&json!({ "name": "bk-b", "type": "filesystem", "path": other.path() }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_client_error(), "{}", resp.status());
    let listed = text(&admin, &server, "backends").await;
    assert!(!listed.contains(other.path().to_str().unwrap()), "{listed}");
    // Control: a backend whose fields the file gives to nobody can go.
    let resp = admin
        .post(url(&server, "backends"))
        .json(&json!({ "name": "bk-c", "type": "filesystem", "path": other.path() }))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{}", resp.status());
    let resp = admin
        .delete(url(&server, "backends/bk-c"))
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "{}", resp.status());
}

/// A reserved private-use character in a free string is plain text: the
/// next boot reads the file.
#[tokio::test]
async fn a_private_use_character_in_a_string_does_not_stop_the_next_boot() {
    let mut server = server().await;
    let admin = admin_http_client(&server.endpoint()).await;
    let message = "busy \u{E000}0\u{E001} now";
    let resp = admin
        .put(url(&server, "config/section/admission"))
        .json(&json!({ "blocks": [{
            "name": "maintenance",
            "match": {},
            "action": { "type": "reject", "status": 503, "message": message },
        }]}))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body = resp.text().await.unwrap();
    assert!(status.is_success(), "{status}: {body}");
    server.respawn_with_env(&[]).await;
    let admin = admin_http_client(&server.endpoint()).await;
    let section = text(&admin, &server, "config/section/admission").await;
    assert!(section.contains("busy"), "{section}");
}

/// The allowlist admits a name only in a field that no read path returns
/// and that the proxy never sends: the backend secret, not the key id, a
/// header value or a provider secret.
#[tokio::test]
async fn an_allowlisted_name_is_accepted_only_in_a_secret_that_nothing_reads_back() {
    const ALLOWED_ID: &str = "AKIAALLOWLISTED0001";
    const ALLOWED_SECRET: &str = "allowlisted-backend-secret-4455";
    let server = TestServer::builder()
        .env("AUDIT_CACHE", "256")
        .env("DGP_CONFIG_ENV_ALLOWLIST", "AUDIT_ALLOWED_*")
        .env("AUDIT_ALLOWED_ID", ALLOWED_ID)
        .env("AUDIT_ALLOWED_SECRET", ALLOWED_SECRET)
        .extra_yaml_root("cache_size_mb: ${env:AUDIT_CACHE}\n")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;
    let s3_doc = |id: &str, secret: &str, header: &str| {
        format!(
            "access:\n  access_key_id: test\n  secret_access_key: test\n\
             storage:\n  backend:\n    type: s3\n    endpoint: http://127.0.0.1:1\n    region: us-east-1\n    access_key_id: {id}\n    secret_access_key: {secret}\n\
             advanced:\n  listen_addr: 127.0.0.1:{}\n  cache_size_mb: ${{env:AUDIT_CACHE}}\n{header}",
            server.endpoint().rsplit(':').next().unwrap(),
        )
    };
    // The secret slot takes the name.
    let ok = s3_doc("literal-id", "${env:AUDIT_ALLOWED_SECRET}", "");
    let (status, body) = post_doc(&admin, &server, "config/validate", &ok).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // The key id (shown by GET /backends) and a header value (sent to a URL
    // that the admin sets) do not.
    for (doc, path) in [
        (
            s3_doc("${env:AUDIT_ALLOWED_ID}", "literal-secret", ""),
            "/backend/access_key_id",
        ),
        (
            s3_doc(
                "literal-id",
                "literal-secret",
                "  event_delivery:\n    enabled: false\n    webhook_url: https://hooks.example.com/a\n    webhook_headers:\n      X-Token: \"${env:AUDIT_ALLOWED_SECRET}\"\n",
            ),
            "/event_delivery/webhook_headers/X-Token",
        ),
    ] {
        let (status, body) = post_doc(&admin, &server, "config/validate", &doc).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}: {body}");
        let error = error_of(&body);
        assert!(error.contains(path), "{error}");
        assert!(!error.contains(ALLOWED_ID) && !error.contains(ALLOWED_SECRET), "{error}");
    }
    let backends = text(&admin, &server, "backends").await;
    assert!(
        !backends.contains(ALLOWED_ID) && !backends.contains(ALLOWED_SECRET),
        "{backends}"
    );
}

/// The GUI flow: section GET, edit one unrelated field, PUT. The frozen
/// fields stay references in the file, before and after a restart.
#[tokio::test]
async fn an_unrelated_edit_in_a_section_with_frozen_fields_keeps_the_references() {
    let mut server = server().await;
    let admin = admin_http_client(&server.endpoint()).await;
    let route = "config/section/advanced";
    let got = admin.get(url(&server, route)).send().await.unwrap();
    let etag = got.headers().get("etag").cloned();
    let mut body: Value = got.json().await.unwrap();
    body["log_level"] = json!("deltaglider_proxy=debug");
    let mut put = admin.put(url(&server, route)).json(&body);
    if let Some(etag) = etag {
        put = put.header("if-match", etag);
    }
    let resp = put.send().await.unwrap();
    let status = resp.status();
    let reply = resp.text().await.unwrap();
    assert!(status.is_success(), "{status}: {reply}");
    for when in ["before the restart", "after the restart"] {
        if when == "after the restart" {
            server.respawn_with_env(&[]).await;
        }
        let admin = admin_http_client(&server.endpoint()).await;
        let file = std::fs::read_to_string(server.config_path()).unwrap();
        for template in [
            "${env:AUDIT_S3_SECRET}",
            "${env:AUDIT_CACHE}",
            "${env:AUDIT_PUB}",
            "${env:AUDIT_HOOK_TOKEN}",
        ] {
            assert!(file.contains(template), "{when}: {template} in\n{file}");
        }
        assert!(file.contains("deltaglider_proxy=debug"), "{when}: {file}");
        assert_no_leak(&admin, &server, when).await;
    }
}

/// A backup restore on the same node keeps the references.
#[tokio::test]
async fn a_backup_restore_on_the_same_node_keeps_the_references() {
    let server = server().await;
    let admin = admin_http_client(&server.endpoint()).await;
    let zip = admin
        .get(url(&server, "backup"))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let resp = admin
        .post(url(&server, "backup"))
        .header("content-type", "application/zip")
        .body(zip.to_vec())
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let reply = resp.text().await.unwrap();
    assert!(status.is_success(), "restore: {status}: {reply}");
    let file = std::fs::read_to_string(server.config_path()).unwrap();
    assert!(file.contains("${env:AUDIT_S3_SECRET}"), "{file}");
    assert_no_leak(&admin, &server, "after the restore").await;
}

/// Section GET, then PUT of the unchanged body, is accepted and lossless;
/// a reference moved into another field of the same body is refused.
#[tokio::test]
async fn a_section_round_trip_is_lossless_and_a_moved_reference_is_refused() {
    let server = server().await;
    let admin = admin_http_client(&server.endpoint()).await;
    for section in ["advanced", "access"] {
        let route = format!("config/section/{section}");
        let got = admin.get(url(&server, &route)).send().await.unwrap();
        let etag = got.headers().get("etag").cloned();
        let body: Value = got.json().await.unwrap();
        let mut put = admin.put(url(&server, &route)).json(&body);
        if let Some(etag) = etag {
            put = put.header("if-match", etag);
        }
        let resp = put.send().await.unwrap();
        assert!(resp.status().is_success(), "{section}: {}", resp.status());
        let again: Value = admin
            .get(url(&server, &route))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(again, body, "{section}: the round trip changed the section");
        assert_no_leak(&admin, &server, "after a section round trip").await;
    }

    // The attack: the reference of the secret in `public_url`.
    let mut body: Value = admin
        .get(url(&server, "config/section/advanced"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    body["public_url"] = json!("http://h.example/${env:AUDIT_S3_SECRET}");
    for route in [
        "config/section/advanced",
        "config/section/advanced/validate",
    ] {
        let request = if route.ends_with("validate") {
            admin.post(url(&server, route))
        } else {
            admin.put(url(&server, route))
        };
        let resp = request.json(&body).send().await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{route}");
        let error = error_of(&resp.json::<Value>().await.unwrap());
        assert!(
            error.contains("/public_url") && !error.contains(S3_SECRET),
            "{error}"
        );
    }
    assert_no_leak(&admin, &server, "after a moved reference").await;
}

/// The CLI path: `config apply` expands on the operator's side and escapes
/// `$`, so the server judges a document with no reference in it. The secret
/// stays a reference in the file.
#[tokio::test]
async fn the_cli_apply_keeps_working_and_keeps_the_reference_in_the_file() {
    let server = server().await;
    let admin = admin_http_client(&server.endpoint()).await;
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), doc(&server, "${env:AUDIT_S3_SECRET}", EVENTS)).unwrap();
    let output = common::proxy_binary()
        .current_dir(std::env::temp_dir())
        .args([
            "config",
            "apply",
            file.path().to_str().unwrap(),
            "--server",
            &server.endpoint(),
        ])
        .env("DGP_BOOTSTRAP_PASSWORD", common::TEST_BOOTSTRAP_PASSWORD)
        .env("AUDIT_S3_SECRET", S3_SECRET)
        .env("AUDIT_PUB", PUB_TOKEN)
        .env("AUDIT_CACHE", "256")
        .env("AUDIT_HOOK_TOKEN", HOOK_TOKEN)
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let config_file = std::fs::read_to_string(server.config_path()).unwrap();
    assert!(
        config_file.contains("${env:AUDIT_S3_SECRET}"),
        "{config_file}"
    );
    assert_no_leak(&admin, &server, "after the CLI apply").await;
}

// ── Server-side copies of a secret ───────────────────────────────────────

const BUCKET_KEY: &str = "c1c2c3c4c5c6c7c8c9cacbcccdcecfd0d1d2d3d4d5d6d7d8d9dadbdcdddedfe0";

async fn job_is_active(admin: &Client, server: &TestServer, bucket: &str) -> bool {
    let v: Value = admin
        .get(url(server, &format!("jobs/bucket/{bucket}")))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    !v["active"].is_null()
}

/// The migrate stage copies the bucket's encryption block to a transient
/// route. The copy keeps the reference: the file and the backup never hold the
/// key while the job runs, and no copy is left when it ends.
#[tokio::test]
async fn a_migrate_never_writes_a_bucket_key_from_the_environment_in_clear() {
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let bucket = "migenc";
    let server = TestServer::builder()
        .bucket(bucket)
        .env("AUDIT_BUCKET_KEY", BUCKET_KEY)
        .env("DGP_TEST_MAINTENANCE_OBJECT_STALL_MS", "250")
        .bucket_policy(
            bucket,
            "backend: src\nencryption: {mode: aes256-gcm-proxy, key: \"${env:AUDIT_BUCKET_KEY}\", key_id: mig-e}",
        )
        .extra_yaml_storage_section(&format!(
            "backends:\n  - name: src\n    type: filesystem\n    path: \"{}\"\n  - name: dst\n    type: filesystem\n    path: \"{}\"\ndefault_backend: src\n",
            dir_a.path().display(),
            dir_b.path().display()
        ))
        .build()
        .await;
    let (http, admin) = (server.http(), admin_http_client(&server.endpoint()).await);
    for i in 0..8 {
        common::put_object(
            &http,
            &server.endpoint(),
            bucket,
            &format!("o{i}.json"),
            format!("object {i}").into_bytes(),
            "application/json",
        )
        .await;
    }
    let resp = admin
        .post(url(&server, &format!("buckets/{bucket}/migrate")))
        .json(&json!({ "target_backend": "dst", "delete_source": false }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::ACCEPTED,
        "{:?}",
        resp.text().await
    );

    let mut saw_route = false;
    while job_is_active(&admin, &server, bucket).await {
        let file = std::fs::read_to_string(server.config_path()).unwrap_or_default();
        saw_route |= file.contains("__dgmigrate_");
        assert!(!file.contains(BUCKET_KEY), "the key is in the file: {file}");
        let backup = backup_text(&admin, &server).await;
        assert!(!backup.contains(BUCKET_KEY), "the key is in the backup");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(saw_route, "premise: the staging route was in the file");

    let file = std::fs::read_to_string(server.config_path()).unwrap();
    assert!(!file.contains("__dgmigrate_"), "a copy is left: {file}");
    assert!(file.contains("${env:AUDIT_BUCKET_KEY}"), "{file}");
    assert!(!backup_text(&admin, &server).await.contains(BUCKET_KEY));
    let jobs: Value = admin
        .get(url(&server, "jobs"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(jobs["jobs"][0]["status"], "succeeded", "{jobs}");
}

/// An allowlisted reference at a bucket key, then a rotation: the old key
/// moves to `legacy_key` as the same reference, never as its value.
#[tokio::test]
async fn a_rotation_keeps_an_allowlisted_old_key_as_a_reference() {
    const OLD: &str = "d1d2d3d4d5d6d7d8d9dadbdcdddedfe0e1e2e3e4e5e6e7e8e9eaebecedeeeff0";
    const NEW: &str = "e1e2e3e4e5e6e7e8e9eaebecedeeeff0f1f2f3f4f5f6f7f8f9fafbfcfdfeff00";
    let server = TestServer::builder()
        .env("DGP_CONFIG_ENV_ALLOWLIST", "AUDIT_ALLOWED_*")
        .env("AUDIT_ALLOWED_KEY", OLD)
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;
    let with_key = |key: &str, id: &str| {
        format!(
            "{}storage:\n  backend:\n    type: filesystem\n    path: {}\n  buckets:\n    keyed:\n      encryption:\n        mode: aes256-gcm-proxy\n        key: {key}\n        key_id: {id}\n",
            "access:\n  access_key_id: test\n  secret_access_key: test\n",
            server.data_dir().unwrap().display(),
        ) + &format!(
            "advanced:\n  listen_addr: 127.0.0.1:{}\n",
            server.endpoint().rsplit(':').next().unwrap()
        )
    };
    let (status, body) = post_doc(
        &admin,
        &server,
        "config/apply",
        &with_key("${env:AUDIT_ALLOWED_KEY}", "k1"),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = post_doc(&admin, &server, "config/apply", &with_key(NEW, "k2")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let file = std::fs::read_to_string(server.config_path()).unwrap();
    assert!(!file.contains(OLD), "the old key is in the file: {file}");
    assert!(
        file.contains("legacy_key: ${env:AUDIT_ALLOWED_KEY}"),
        "{file}"
    );
    for (name, body) in read_paths(&admin, &server).await {
        assert!(!body.contains(OLD), "the old key is in {name}");
    }
    assert!(!backup_text(&admin, &server).await.contains(OLD));
}

// ── The singleton backend becomes the named backend `default` ────────────

async fn singleton_server(
    backend_yaml: &str,
    env: Vec<(String, String)>,
) -> (TestServer, tempfile::TempDir) {
    let data = tempfile::TempDir::new().unwrap();
    let doc = format!(
        "access:\n  access_key_id: AKSINGLE\n  secret_access_key: single-secret-0001\n\
         storage:\n{backend_yaml}\
         advanced:\n  listen_addr: {}\n  bootstrap_password_hash: \"{}\"\n",
        common::LISTEN_ADDR_PLACEHOLDER,
        common::TEST_BOOTSTRAP_PASSWORD_HASH
    );
    let extra = tempfile::TempDir::new().unwrap();
    let server =
        TestServer::from_config_document(&doc, data, ("AKSINGLE", "single-secret-0001"), "b", env)
            .await;
    (server, extra)
}

/// Add a backend and check that no read path or file holds the mask text or
/// a secret; the new backend `default` keeps the references.
async fn add_backend_and_check(server: &TestServer, extra: &std::path::Path, secrets: &[&str]) {
    let admin = admin_http_client(&server.endpoint()).await;
    let resp = admin
        .post(url(server, "backends"))
        .json(&json!({ "name": "second", "type": "filesystem", "path": extra }))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body = resp.text().await.unwrap();
    assert!(status.is_success(), "POST /backends: {status} {body}");
    for (name, text) in read_paths(&admin, server).await {
        assert!(!text.contains("frozen-"), "the mask is in {name}: {text}");
        for secret in secrets {
            assert!(!text.contains(secret), "{secret} is in {name}");
        }
    }
    let listed: Value = serde_json::from_str(&text(&admin, server, "backends").await).unwrap();
    let names: Vec<&str> = listed["backends"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|b| b["name"].as_str())
        .collect();
    assert!(
        names.contains(&"default") && names.contains(&"second"),
        "{names:?}"
    );
}

const SINGLE_SECRET: &str = "SINGLETON-S3-SECRET-4f5e6d7c";
const SINGLE_KEY: &str = "f1f2f3f4f5f6f7f8f9fafbfcfdfeff00f1f2f3f4f5f6f7f8f9fafbfcfdfeff00";

#[tokio::test]
async fn adding_a_backend_keeps_the_env_secret_of_an_s3_singleton_as_a_reference() {
    let (server, extra) = singleton_server(
        "  backend:\n    type: s3\n    endpoint: http://127.0.0.1:1\n    region: us-east-1\n    force_path_style: true\n    access_key_id: AKSINGLETON\n    secret_access_key: ${env:SINGLE_SK}\n",
        vec![
            ("SINGLE_SK".into(), SINGLE_SECRET.into()),
            ("DGP_BOOT_BACKEND_PROBE".into(), "off".into()),
        ],
    )
    .await;
    add_backend_and_check(&server, extra.path(), &[SINGLE_SECRET]).await;
    let file = std::fs::read_to_string(server.config_path()).unwrap();
    assert_eq!(file.matches("${env:SINGLE_SK}").count(), 2, "{file}");
}

#[tokio::test]
async fn adding_a_backend_keeps_the_env_path_of_a_filesystem_singleton() {
    let dir = tempfile::TempDir::new().unwrap();
    let (server, extra) = singleton_server(
        "  backend:\n    type: filesystem\n    path: ${env:SINGLE_DATA}\n",
        vec![("SINGLE_DATA".into(), dir.path().display().to_string())],
    )
    .await;
    add_backend_and_check(&server, extra.path(), &[]).await;
    let file = std::fs::read_to_string(server.config_path()).unwrap();
    assert_eq!(file.matches("${env:SINGLE_DATA}").count(), 2, "{file}");
}

#[tokio::test]
async fn adding_a_backend_keeps_the_env_key_of_a_singleton_encryption() {
    let dir = tempfile::TempDir::new().unwrap();
    let (server, extra) = singleton_server(
        &format!(
            "  backend:\n    type: filesystem\n    path: {}\n  backend_encryption:\n    mode: aes256-gcm-proxy\n    key: ${{env:SINGLE_KEY}}\n    key_id: single\n",
            dir.path().display()
        ),
        vec![("SINGLE_KEY".into(), SINGLE_KEY.into())],
    )
    .await;
    add_backend_and_check(&server, extra.path(), &[SINGLE_KEY]).await;
    let file = std::fs::read_to_string(server.config_path()).unwrap();
    assert_eq!(file.matches("${env:SINGLE_KEY}").count(), 2, "{file}");
}

// ── A failed restore puts the config back ────────────────────────────────

/// `zip` with `edit` applied to the named parts and the manifest hashes made
/// again: a backup that Phase A accepts.
fn edited_backup(zip: &[u8], edit: impl Fn(&str, Vec<u8>) -> Vec<u8>) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    use std::io::{Read, Write};
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip.to_vec())).unwrap();
    let mut parts = Vec::new();
    for i in 0..archive.len() {
        let mut f = archive.by_index(i).unwrap();
        let name = f.name().unwrap().to_string();
        let mut bytes = Vec::new();
        f.read_to_end(&mut bytes).unwrap();
        let bytes = edit(&name, bytes);
        parts.push((name, bytes));
    }
    let hashes: std::collections::BTreeMap<String, String> = parts
        .iter()
        .map(|(n, b)| (n.clone(), hex::encode(Sha256::digest(b))))
        .collect();
    let mut out = Vec::new();
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut out));
    let opts = zip::write::SimpleFileOptions::default();
    for (name, bytes) in parts {
        let bytes = if name == "manifest.json" {
            let mut m: Value = serde_json::from_slice(&bytes).unwrap();
            for entry in m["files"].as_array_mut().unwrap() {
                let n = entry["name"].as_str().unwrap().to_string();
                entry["sha256"] = json!(hashes[&n]);
            }
            serde_json::to_vec(&m).unwrap()
        } else {
            bytes
        };
        writer.start_file(name, opts).unwrap();
        writer.write_all(&bytes).unwrap();
    }
    writer.finish().unwrap();
    out
}

/// The config part of a restore is live when its IAM part fails (two groups
/// of one name). The config has frozen fields, so the rollback must run
/// outside the mask: the running config is the one from before the restore.
#[tokio::test]
async fn a_failed_restore_puts_the_config_back_while_fields_are_frozen() {
    crate::common::expect_server_errors("the IAM part of the restore fails on purpose");
    let server = server().await;
    let admin = admin_http_client(&server.endpoint()).await;
    let zip = admin
        .get(url(&server, "backup"))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let broken = edited_backup(&zip, |name, bytes| match name {
        "config.yaml" => {
            let text = String::from_utf8(bytes).unwrap();
            assert!(text.contains("http://h.example/"), "{text}");
            text.replace("http://h.example/", "http://changed.example/")
                .into_bytes()
        }
        "iam.json" => serde_json::to_vec(&json!({
            "version": 2,
            "users": [],
            "groups": [
                { "id": 1, "name": "dup", "description": "", "permissions": [], "member_ids": [] },
                { "id": 2, "name": "dup", "description": "", "permissions": [], "member_ids": [] },
            ],
            "auth_providers": [], "mapping_rules": [], "external_identities": [],
        }))
        .unwrap(),
        _ => bytes,
    });
    let resp = admin
        .post(url(&server, "backup"))
        .header("content-type", "application/zip")
        .body(broken)
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let reply = resp.text().await.unwrap();
    assert!(!status.is_success(), "the restore must fail: {reply}");
    assert!(reply.contains("rolled back"), "{reply}");
    assert!(
        !reply.contains("rollback of the earlier phases failed"),
        "{reply}"
    );
    let live = export(&admin, &server).await;
    assert!(live.contains("http://h.example/"), "{live}");
    assert!(!live.contains("changed.example"), "{live}");
    let file = std::fs::read_to_string(server.config_path()).unwrap();
    assert!(!file.contains("changed.example"), "{file}");
    assert!(file.contains("${env:AUDIT_S3_SECRET}"), "{file}");
    // The rollback of the config is audited, as every config write is.
    let audit: Value = admin
        .get(url(&server, "audit?limit=200"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let rows = audit["entries"]
        .as_array()
        .or(audit.as_array())
        .expect("audit rows");
    assert!(
        rows.iter()
            .any(|r| r["action"] == "restore_rollback" && r["target"] == "config"),
        "no audit entry for the config rollback: {audit}"
    );
}

// ── A bucket policy edit with a frozen bucket key ────────────────────────

#[tokio::test]
async fn an_unrelated_bucket_policy_edit_is_accepted_while_a_bucket_key_is_frozen() {
    let server = TestServer::builder()
        .bucket("kb")
        .env("AUDIT_KB_KEY", BUCKET_KEY)
        .bucket_policy(
            "kb",
            "encryption: {mode: aes256-gcm-proxy, key: \"${env:AUDIT_KB_KEY}\", key_id: kb1}",
        )
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;
    let cfg: Value = admin
        .get(url(&server, "config"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut policies = cfg["bucket_policies"].clone();
    policies["other"] = json!({ "compression": false });
    let resp = admin
        .put(url(&server, "config"))
        .json(&json!({ "bucket_policies": policies }))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body = resp.text().await.unwrap();
    assert!(
        status.is_success(),
        "PATCH bucket_policies: {status} {body}"
    );
    let file = std::fs::read_to_string(server.config_path()).unwrap();
    assert!(file.contains("${env:AUDIT_KB_KEY}"), "{file}");
    assert!(
        !file.contains(BUCKET_KEY) && !file.contains("frozen-"),
        "{file}"
    );
    assert!(file.contains("other"), "{file}");
    assert_eq!(
        cfg["bucket_policies"]["kb"]["encryption"]["mode"],
        "aes256-gcm-proxy"
    );
}

// ── A migrate that cannot flip is refused at the start ───────────────────

#[tokio::test]
async fn a_migrate_of_a_bucket_with_a_frozen_route_is_refused_before_any_copy() {
    let (dir_a, dir_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let bucket = "migref";
    let server = TestServer::builder()
        .bucket(bucket)
        .env("AUDIT_MIG_BACKEND", "src")
        .bucket_policy(bucket, "backend: ${env:AUDIT_MIG_BACKEND}")
        .extra_yaml_storage_section(&format!(
            "backends:\n  - name: src\n    type: filesystem\n    path: \"{}\"\n  - name: dst\n    type: filesystem\n    path: \"{}\"\ndefault_backend: src\n",
            dir_a.path().display(),
            dir_b.path().display()
        ))
        .build()
        .await;
    let http = server.http();
    common::put_object(
        &http,
        &server.endpoint(),
        bucket,
        "o.json",
        b"object".to_vec(),
        "application/json",
    )
    .await;
    let admin = admin_http_client(&server.endpoint()).await;
    let resp = admin
        .post(url(&server, &format!("buckets/{bucket}/migrate")))
        .json(&json!({ "target_backend": "dst", "delete_source": false }))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body = resp.text().await.unwrap();
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body.contains(&format!("/buckets/{bucket}/backend")),
        "{body}"
    );
    assert!(!dir_b.path().join(bucket).exists(), "something was copied");
    let jobs: Value = admin
        .get(url(&server, "jobs"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(jobs["jobs"].as_array().unwrap().is_empty(), "{jobs}");
}

// ── No decision depends on the value of a secret ─────────────────────────

/// The shape of production: one secret value stands at two paths, by
/// reference in the bootstrap field and as the literal of an IAM user. Every
/// save works, and the fields keep what the file holds.
#[tokio::test]
async fn one_secret_value_at_two_paths_blocks_no_save() {
    let server = TestServer::builder()
        .auth("AKTESTENV", "${env:AUDIT_S3_SECRET}")
        .env("AUDIT_S3_SECRET", S3_SECRET)
        .extra_yaml_root(&format!(
            "iam_mode: declarative\niam_users:\n  - name: dana\n    access_key_id: AKDANA0001\n    secret_access_key: {S3_SECRET}\n    enabled: true\n    permissions:\n      - effect: Allow\n        actions: [read]\n        resources: [\"*\"]\n"
        ))
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;
    let secret_lines = |file: &str| {
        file.lines()
            .filter(|l| l.contains("secret_access_key"))
            .map(|l| l.trim().replace('"', ""))
            .collect::<Vec<_>>()
    };
    let before = secret_lines(&std::fs::read_to_string(server.config_path()).unwrap());
    assert!(before.iter().any(|l| l.contains("${env:AUDIT_S3_SECRET}")));

    let resp = admin
        .put(url(&server, "config/section/advanced"))
        .json(&json!({ "public_url": "http://second.example/" }))
        .send()
        .await
        .unwrap();
    let (status, body) = (resp.status(), resp.text().await.unwrap());
    assert!(status.is_success(), "section PUT: {status} {body}");
    let body: Value = serde_json::from_str(&body).unwrap();
    assert!(body["persisted_path"].is_string(), "{body}");

    let extra = tempfile::TempDir::new().unwrap();
    let resp = admin
        .post(url(&server, "backends"))
        .json(&json!({ "name": "second", "type": "filesystem", "path": extra.path() }))
        .send()
        .await
        .unwrap();
    let (status, body) = (resp.status(), resp.text().await.unwrap());
    assert!(status.is_success(), "POST /backends: {status} {body}");

    let file = std::fs::read_to_string(server.config_path()).unwrap();
    assert!(file.contains("http://second.example/"), "{file}");
    assert_eq!(secret_lines(&file), before, "{file}");
    assert_eq!(
        file.matches(S3_SECRET).count(),
        1,
        "the literal stays a literal: {file}"
    );
}

/// A free-text field set to the exact value of an environment secret gets the
/// answer, the persisted state and every read of a wrong guess (the guess
/// itself is the only text that differs).
#[tokio::test]
async fn a_free_text_field_holding_a_secret_value_is_no_oracle() {
    let server = server().await;
    let admin = admin_http_client(&server.endpoint()).await;
    let mut seen = Vec::new();
    for guess in [S3_SECRET, "a-wrong-guess-0123456789"] {
        // The same state before each guess.
        let reset = admin
            .put(url(&server, "config/section/advanced"))
            .json(&json!({ "public_url": "http://h.example/" }))
            .send()
            .await
            .unwrap();
        assert!(reset.status().is_success());
        let mut answers = Vec::new();
        for (method, route, body) in [
            (
                "PUT",
                "config/section/advanced",
                json!({ "public_url": guess }),
            ),
            (
                "POST",
                "config/section/advanced/validate",
                json!({ "public_url": guess }),
            ),
            ("PUT", "config", json!({ "public_url": guess })),
            // A refusal that may echo the text it refuses.
            (
                "PUT",
                "config/section/advanced",
                json!({ "log_level": guess }),
            ),
            ("PUT", "config", json!({ "log_level": guess })),
            // A text that the log filter refuses: its warning must not repeat it.
            (
                "PUT",
                "config",
                json!({ "log_level": format!("{guess}=x") }),
            ),
        ] {
            let req = match method {
                "PUT" => admin.put(url(&server, route)),
                _ => admin.post(url(&server, route)),
            };
            let resp = req.json(&body).send().await.unwrap();
            let status = resp.status();
            answers.push((route, status, resp.text().await.unwrap()));
        }
        let mut reads = read_paths(&admin, &server).await;
        reads.push(("diff".into(), String::new()));
        let all: Vec<(String, String)> = answers
            .into_iter()
            .map(|(r, s, b)| (format!("{r} {s}"), b))
            .chain(reads)
            .map(|(n, t)| (n, t.replace(guess, "<GUESS>")))
            .collect();
        seen.push(all);
    }
    for (a, b) in seen[0].iter().zip(&seen[1]) {
        assert_eq!(a, b, "{}: the answer tells the guesses apart", a.0);
    }
    assert!(seen[0][0].0.contains("200"), "{:?}", seen[0][0]);
}

// ── The routes of a migrate job ──────────────────────────────────────────

/// No admin surface creates, changes or removes a `__dgmigrate_*` route, and
/// a save that leaves the routes alone is accepted.
#[tokio::test]
async fn no_admin_request_makes_changes_or_removes_a_migrate_route() {
    let server = server().await;
    let admin = admin_http_client(&server.endpoint()).await;
    let file_before = std::fs::read_to_string(server.config_path()).unwrap();
    let route = "__dgmigrate_releases_0";
    let section = json!({ "buckets": { route: { "compression": false } } });
    for (method, path, body) in [
        ("PUT", "config/section/storage", section.clone()),
        ("POST", "config/section/storage/validate", section),
        (
            "PUT",
            "config",
            json!({ "bucket_policies": { route: { "compression": false } } }),
        ),
    ] {
        let req = match method {
            "PUT" => admin.put(url(&server, path)),
            _ => admin.post(url(&server, path)),
        };
        let resp = req.json(&body).send().await.unwrap();
        let status = resp.status();
        let text = resp.text().await.unwrap();
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}: {text}");
        assert!(
            text.contains(route) && text.contains("migrate job"),
            "{text}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(server.config_path()).unwrap(),
        file_before
    );
    // Control: an unrelated bucket policy is accepted.
    let ok = admin
        .put(url(&server, "config/section/storage"))
        .json(&json!({ "buckets": { "plain": { "compression": false } } }))
        .send()
        .await
        .unwrap();
    assert!(ok.status().is_success(), "{}", ok.status());
}
