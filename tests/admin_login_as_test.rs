// SPDX-License-Identifier: BUSL-1.1

//! Integration tests for admin login-as (IAM user impersonation).

use crate::common;

use common::{admin_http_client, TestServer};
use reqwest::StatusCode;
use serde_json::json;

/// Create an IAM user via the admin API and return (access_key_id, secret_access_key).
async fn create_user(
    admin: &reqwest::Client,
    endpoint: &str,
    name: &str,
    permissions: serde_json::Value,
) -> (String, String) {
    let resp = admin
        .post(format!("{}/_/api/admin/users", endpoint))
        .json(&json!({ "name": name, "permissions": permissions }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body: serde_json::Value = resp.json().await.unwrap();
    (
        body["access_key_id"].as_str().unwrap().to_string(),
        body["secret_access_key"].as_str().unwrap().to_string(),
    )
}

fn admin_perms() -> serde_json::Value {
    json!([{ "actions": ["*"], "resources": ["*"] }])
}

fn readonly_perms() -> serde_json::Value {
    json!([{ "actions": ["read", "list"], "resources": ["*"] }])
}

#[tokio::test]
async fn test_login_as_admin_succeeds() {
    let server = TestServer::builder()
        .auth("BOOTSTRAP", "BOOTSTRAPSECRET")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    // Create an IAM admin user
    let (ak, sk) = create_user(&admin, &server.endpoint(), "test-admin", admin_perms()).await;

    // Login-as that user
    let login_client = reqwest::Client::builder()
        .cookie_store(true)
        .no_proxy()
        .build()
        .unwrap();
    let resp = login_client
        .post(format!("{}/_/api/admin/login-as", server.endpoint()))
        .json(&json!({ "access_key_id": ak, "secret_access_key": sk }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Verify admin session works — can access config
    let resp = login_client
        .get(format!("{}/_/api/admin/config", server.endpoint()))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_login_as_non_admin_rejected() {
    let server = TestServer::builder()
        .auth("BOOTSTRAP2", "BOOTSTRAPSECRET2")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    // Create a read-only user (not admin)
    let (ak, sk) = create_user(&admin, &server.endpoint(), "reader", readonly_perms()).await;

    // Login-as should be rejected (not admin)
    let resp = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("{}/_/api/admin/login-as", server.endpoint()))
        .json(&json!({ "access_key_id": ak, "secret_access_key": sk }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_login_as_wrong_secret_rejected() {
    let server = TestServer::builder()
        .auth("BOOTSTRAP3", "BOOTSTRAPSECRET3")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    let (ak, _sk) = create_user(&admin, &server.endpoint(), "admin2", admin_perms()).await;

    let resp = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("{}/_/api/admin/login-as", server.endpoint()))
        .json(&json!({ "access_key_id": ak, "secret_access_key": "wrong-secret" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_login_as_unknown_key_rejected() {
    let server = TestServer::builder()
        .auth("BOOTSTRAP4", "BOOTSTRAPSECRET4")
        .build()
        .await;

    let resp = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("{}/_/api/admin/login-as", server.endpoint()))
        .json(&json!({ "access_key_id": "nonexistent", "secret_access_key": "anything" }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_login_as_disabled_user_rejected() {
    let server = TestServer::builder()
        .auth("BOOTSTRAP5", "BOOTSTRAPSECRET5")
        .build()
        .await;
    let admin = admin_http_client(&server.endpoint()).await;

    // Create admin user
    let (ak, sk) = create_user(&admin, &server.endpoint(), "to-disable", admin_perms()).await;

    // Get user ID from list
    let resp = admin
        .get(format!("{}/_/api/admin/users", server.endpoint()))
        .send()
        .await
        .unwrap();
    let users: Vec<serde_json::Value> = resp.json().await.unwrap();
    let user_id = users
        .iter()
        .find(|u| u["access_key_id"].as_str() == Some(&ak))
        .unwrap()["id"]
        .as_i64()
        .unwrap();

    // Disable the user
    admin
        .put(format!(
            "{}/_/api/admin/users/{}",
            server.endpoint(),
            user_id
        ))
        .json(&json!({ "enabled": false }))
        .send()
        .await
        .unwrap();

    // Login-as should fail
    let resp = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("{}/_/api/admin/login-as", server.endpoint()))
        .json(&json!({ "access_key_id": ak, "secret_access_key": sk }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
}

/// S23: admin audit entries carried the literal actor "admin" whoever made
/// the change. An IAM admin's mutation must be attributed to that user.
#[tokio::test]
async fn test_audit_entry_names_the_iam_admin() {
    let server = TestServer::builder()
        .auth("BOOTSTRAP3", "BOOTSTRAPSECRET3")
        .build()
        .await;
    let ep = server.endpoint();
    let admin = admin_http_client(&ep).await;
    let (ak, sk) = create_user(&admin, &ep, "dana", admin_perms()).await;
    let dana = reqwest::Client::builder()
        .cookie_store(true)
        .no_proxy()
        .build()
        .unwrap();
    let resp = dana
        .post(format!("{ep}/_/api/admin/login-as"))
        .json(&json!({ "access_key_id": ak, "secret_access_key": sk }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = dana
        .post(format!("{ep}/_/api/admin/groups"))
        .json(&json!({ "name": "Engineering", "permissions": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let audit: serde_json::Value = admin
        .get(format!("{ep}/_/api/admin/audit?limit=50"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let entries = audit
        .as_array()
        .or_else(|| audit["entries"].as_array())
        .expect("audit list");
    let e = entries
        .iter()
        .find(|e| e["action"] == "create_group")
        .expect("create_group entry");
    assert_eq!(e["user"], "dana", "{e}");
}

/// A full backup and a full-IAM export with secrets hand out every
/// credential in plain text, and neither left an audit entry. Both must be
/// audited and attributed to the admin who took them.
#[tokio::test]
async fn test_secret_exports_are_audited() {
    let server = TestServer::builder()
        .auth("BOOTSTRAP4", "BOOTSTRAPSECRET4")
        .build()
        .await;
    let ep = server.endpoint();
    let admin = admin_http_client(&ep).await;
    let (ak, sk) = create_user(&admin, &ep, "dana", admin_perms()).await;
    let dana = reqwest::Client::builder()
        .cookie_store(true)
        .no_proxy()
        .build()
        .unwrap();
    let resp = dana
        .post(format!("{ep}/_/api/admin/login-as"))
        .json(&json!({ "access_key_id": ak, "secret_access_key": sk }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    for path in [
        "backup",
        "config/declarative-iam-export?include_secrets=true",
    ] {
        let resp = dana
            .get(format!("{ep}/_/api/admin/{path}"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{path}");
    }
    let audit: serde_json::Value = admin
        .get(format!("{ep}/_/api/admin/audit?limit=50"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let entries = audit
        .as_array()
        .or_else(|| audit["entries"].as_array())
        .expect("audit list");
    for action in ["export_backup", "export_iam_with_secrets"] {
        let e = entries
            .iter()
            .find(|e| e["action"] == action)
            .unwrap_or_else(|| panic!("no {action} entry in {audit}"));
        assert_eq!(e["user"], "dana", "{e}");
    }
}

/// IAM audit entries named deleted users, groups, members, providers and
/// mapping rules by their numeric id only, which says nothing once the row
/// is gone. Every target must carry the name (and the id).
#[tokio::test]
async fn test_iam_audit_targets_carry_names() {
    let server = TestServer::builder()
        .auth("BOOTSTRAP5", "BOOTSTRAPSECRET5")
        .build()
        .await;
    let ep = server.endpoint();
    let admin = admin_http_client(&ep).await;
    let users: serde_json::Value = {
        create_user(&admin, &ep, "dana", readonly_perms()).await;
        admin
            .get(format!("{ep}/_/api/admin/users"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    };
    let dana_id = users
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["name"] == "dana")
        .unwrap()["id"]
        .as_i64()
        .unwrap();
    let group: serde_json::Value = admin
        .post(format!("{ep}/_/api/admin/groups"))
        .json(&json!({ "name": "Engineering", "permissions": [] }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let gid = group["id"].as_i64().unwrap();
    let ok = |r: reqwest::Response| assert!(r.status().is_success(), "{}", r.status());
    ok(admin
        .post(format!("{ep}/_/api/admin/groups/{gid}/members"))
        .json(&json!({ "user_id": dana_id }))
        .send()
        .await
        .unwrap());
    ok(admin
        .delete(format!("{ep}/_/api/admin/groups/{gid}/members/{dana_id}"))
        .send()
        .await
        .unwrap());
    let rule: serde_json::Value = admin
        .post(format!("{ep}/_/api/admin/ext-auth/mappings"))
        .json(
            &json!({ "match_type": "email_domain", "match_value": "example.com", "group_id": gid }),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let rid = rule["id"].as_i64().unwrap();
    let provider: serde_json::Value = admin
        .post(format!("{ep}/_/api/admin/ext-auth/providers"))
        .json(&json!({
            "name": "corp-sso",
            "provider_type": "oidc",
            "enabled": false,
            "client_id": "client",
            "client_secret": "secret",
            "issuer_url": "https://accounts.google.com",
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let pid = provider["id"].as_i64().unwrap();
    for path in [
        format!("ext-auth/providers/{pid}"),
        format!("ext-auth/mappings/{rid}"),
        format!("groups/{gid}"),
        format!("users/{dana_id}"),
    ] {
        ok(admin
            .delete(format!("{ep}/_/api/admin/{path}"))
            .send()
            .await
            .unwrap());
    }
    let audit: serde_json::Value = admin
        .get(format!("{ep}/_/api/admin/audit?limit=50"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let target = |action: &str| {
        audit["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["action"] == action)
            .unwrap_or_else(|| panic!("no {action} in {audit}"))["target"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(
        target("add_member"),
        format!("user dana (id {dana_id}) to group Engineering (id {gid})")
    );
    assert_eq!(
        target("remove_member"),
        format!("user dana (id {dana_id}) from group Engineering (id {gid})")
    );
    assert_eq!(
        target("delete_mapping_rule"),
        format!("rule {rid}: email_domain example.com -> group Engineering (id {gid})")
    );
    assert_eq!(target("delete_group"), format!("Engineering (id {gid})"));
    assert_eq!(
        target("delete_auth_provider"),
        format!("corp-sso (id {pid})")
    );
    assert_eq!(target("delete_user"), format!("dana (id {dana_id})"));
}

/// A restore that carries another bootstrap password changes it only for a
/// session that logged in with the bootstrap password. An IAM admin keeps it.
#[tokio::test]
async fn an_iam_admin_restore_keeps_the_bootstrap_password() {
    let src_password = "source-instance-password-1";
    let source = TestServer::builder()
        .auth("BKSRC2", "BKSRC2SECRET")
        .bootstrap_password(src_password)
        .build()
        .await;
    let zip = common::admin_http_client_with_password(&source.endpoint(), src_password)
        .await
        .get(format!("{}/_/api/admin/backup", source.endpoint()))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();

    let target = TestServer::builder()
        .auth("BKDST2", "BKDST2SECRET")
        .build()
        .await;
    let ep = target.endpoint();
    let boot = admin_http_client(&ep).await;
    let (ak, sk) = create_user(&boot, &ep, "iam-root", admin_perms()).await;
    let iam_admin = reqwest::Client::builder()
        .cookie_store(true)
        .no_proxy()
        .build()
        .unwrap();
    let r = iam_admin
        .post(format!("{ep}/_/api/admin/login-as"))
        .json(&json!({ "access_key_id": ak, "secret_access_key": sk }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);

    let resp = iam_admin
        .post(format!("{ep}/_/api/admin/backup?mode=full"))
        .header("content-type", "application/zip")
        .body(zip.to_vec())
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(status, StatusCode::OK, "{body}");
    let note = body["bootstrap_password"].as_str().unwrap_or_default();
    assert!(
        note.contains("kept") && note.contains("bootstrap password"),
        "the response must say that the password was kept and why: {body}"
    );

    let login = |pw: &'static str| {
        let ep = ep.clone();
        async move {
            reqwest::Client::new()
                .post(format!("{ep}/_/api/admin/login"))
                .json(&json!({ "password": pw }))
                .send()
                .await
                .unwrap()
                .status()
        }
    };
    assert!(login(common::TEST_BOOTSTRAP_PASSWORD).await.is_success());
    assert!(!login("source-instance-password-1").await.is_success());

    // The bootstrap session still adopts it (disaster recovery).
    let resp = boot
        .post(format!("{ep}/_/api/admin/backup?mode=full"))
        .header("content-type", "application/zip")
        .body(zip.to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(login("source-instance-password-1").await.is_success());
}

// ── A delete job follows the live admin rule of the admin who started it ──

/// Objects under `big/`: more than one page (1,000 keys) of a delete job.
const JOB_OBJECTS: usize = 1_100;

async fn put_job_objects(server: &TestServer) {
    let http = server.http();
    let (ep, bucket) = (server.endpoint(), server.bucket().to_string());
    let puts = (0..JOB_OBJECTS).map(|i| {
        http.put(format!("{ep}/{bucket}/big/f{i:05}.txt"))
            .body(b"x".to_vec())
            .send()
    });
    for r in futures::future::join_all(puts).await {
        assert!(r.unwrap().status().is_success());
    }
}

async fn login_as(ep: &str, ak: &str, sk: &str) -> reqwest::Client {
    let client = reqwest::Client::builder()
        .cookie_store(true)
        .no_proxy()
        .build()
        .unwrap();
    let resp = client
        .post(format!("{ep}/_/api/admin/login-as"))
        .json(&json!({ "access_key_id": ak, "secret_access_key": sk }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    client
}

/// Start a delete job of `big/` as `who`; returns the job's row id.
async fn start_delete_job(who: &reqwest::Client, server: &TestServer) -> String {
    let resp = who
        .post(format!(
            "{}/_/api/admin/objects/delete-jobs",
            server.endpoint()
        ))
        .json(&json!({
            "bucket": server.bucket(),
            "prefixes": ["big/"],
            "expected": JOB_OBJECTS,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 202);
    let id = resp.json::<serde_json::Value>().await.unwrap()["job_id"]
        .as_i64()
        .unwrap();
    format!("maintenance:{id}")
}

async fn job_row(admin: &reqwest::Client, ep: &str, id: &str) -> serde_json::Value {
    let all: serde_json::Value = admin
        .get(format!("{ep}/_/api/admin/jobs"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    all["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|j| j["id"] == id)
        .unwrap_or_else(|| panic!("job {id} not listed: {all}"))
        .clone()
}

/// Poll the row until `done(row)`; panics after 60 s.
async fn wait_for_job(
    admin: &reqwest::Client,
    ep: &str,
    id: &str,
    done: impl Fn(&serde_json::Value) -> bool,
) -> serde_json::Value {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let row = job_row(admin, ep, id).await;
        if done(&row) {
            return row;
        }
        assert!(std::time::Instant::now() < deadline, "job stuck: {row}");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

async fn user_id(admin: &reqwest::Client, ep: &str, ak: &str) -> i64 {
    let users: Vec<serde_json::Value> = admin
        .get(format!("{ep}/_/api/admin/users"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    users
        .iter()
        .find(|u| u["access_key_id"].as_str() == Some(ak))
        .unwrap()["id"]
        .as_i64()
        .unwrap()
}

/// An IAM admin starts a delete job of more than one page. During the pause
/// after the first page the admin is disabled (case 1) or demoted (case 2):
/// the job settles failed with the admin reason and keeps the rest of the
/// objects. Fails if the job stored the bootstrap marker for every admin.
#[tokio::test]
async fn a_delete_job_stops_when_its_admin_is_disabled_or_demoted() {
    for (case, change) in [
        ("disabled", json!({ "enabled": false })),
        (
            "demoted",
            json!({ "permissions": [{ "actions": ["read", "list"], "resources": ["*"] }] }),
        ),
    ] {
        let server = TestServer::builder()
            .auth("BOOTSTRAPJ", "BOOTSTRAPJSECRET")
            .env("DGP_TEST_MAINTENANCE_OBJECT_STALL_MS", "3000")
            .build()
            .await;
        let ep = server.endpoint();
        let admin = admin_http_client(&ep).await;
        put_job_objects(&server).await;
        let (ak, sk) = create_user(&admin, &ep, "dana", admin_perms()).await;
        let dana = login_as(&ep, &ak, &sk).await;
        let id = start_delete_job(&dana, &server).await;
        // The first page is deleted; the job now pauses before page two.
        wait_for_job(&admin, &ep, &id, |r| {
            r["progress"]["processed"].as_i64().unwrap_or(0) >= 1_000
        })
        .await;
        let resp = admin
            .put(format!(
                "{ep}/_/api/admin/users/{}",
                user_id(&admin, &ep, &ak).await
            ))
            .json(&change)
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "{case}: {}", resp.status());
        let row = wait_for_job(&admin, &ep, &id, |r| r["status"] == "failed").await;
        let why = row["last_error"].as_str().unwrap_or_default();
        assert!(
            why.contains("no longer an enabled admin"),
            "{case}: the reason names the admin: {row}"
        );
        let left: serde_json::Value = admin
            .get(format!(
                "{ep}/_/api/admin/objects/list?bucket={}&prefix=big/",
                server.bucket()
            ))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(
            left["keys"].as_array().map(Vec::len),
            Some(JOB_OBJECTS - 1_000),
            "{case}: the rest stays"
        );
    }
}

/// Controls: the job of an admin who stays an enabled admin, and the job of
/// a bootstrap session, both run to the end across pages.
#[tokio::test]
async fn a_delete_job_of_a_live_admin_and_of_the_bootstrap_session_completes() {
    let server = TestServer::builder()
        .auth("BOOTSTRAPK", "BOOTSTRAPKSECRET")
        .build()
        .await;
    let ep = server.endpoint();
    let admin = admin_http_client(&ep).await;
    let (ak, sk) = create_user(&admin, &ep, "dana", admin_perms()).await;
    let dana = login_as(&ep, &ak, &sk).await;
    for (who, label) in [(&dana, "iam admin"), (&admin, "bootstrap")] {
        put_job_objects(&server).await;
        let id = start_delete_job(who, &server).await;
        let row = wait_for_job(&admin, &ep, &id, |r| {
            matches!(r["status"].as_str(), Some("succeeded" | "failed"))
        })
        .await;
        assert_eq!(row["status"], "succeeded", "{label}: {row}");
        assert_eq!(row["progress"]["processed"], JOB_OBJECTS, "{label}: {row}");
    }
}
