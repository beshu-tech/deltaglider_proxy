// SPDX-License-Identifier: BUSL-1.1

//! `GET /_/api/admin/storage/status`: two views behind one admin check.

use crate::common;

use common::{admin_http_client, TestServer};
use reqwest::StatusCode;
use serde_json::{json, Value};

const URL: &str = "/_/api/admin/storage/status";

/// A lift session of a read-only IAM user.
async fn lift_client(ep: &str, admin: &reqwest::Client) -> reqwest::Client {
    let resp = admin
        .post(format!("{ep}/_/api/admin/users"))
        .json(&json!({
            "name": "status-reader",
            "permissions": [{ "actions": ["read", "list"], "resources": ["*"] }],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let user: Value = resp.json().await.unwrap();
    let lift = reqwest::Client::builder()
        .cookie_store(true)
        .no_proxy()
        .build()
        .unwrap();
    let resp = lift
        .post(format!("{ep}/_/api/admin/session/browser-connect"))
        .json(&json!({
            "access_key_id": user["access_key_id"],
            "secret_access_key": user["secret_access_key"],
            "endpoint": ep,
            "bucket": "",
            "region": "us-east-1",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    lift
}

async fn body(client: &reqwest::Client, ep: &str) -> (String, Value) {
    let resp = client.get(format!("{ep}{URL}")).send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let text = resp.text().await.unwrap();
    let v = serde_json::from_str(&text).unwrap();
    (text, v)
}

/// Poll until the first disk tick has run (`state != "checking"`).
async fn settled(client: &reqwest::Client, ep: &str) -> (String, Value) {
    for _ in 0..100 {
        let (text, v) = body(client, ep).await;
        if v["state"] != "checking" {
            return (text, v);
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!("the disk monitor never ticked");
}

const BROWSER_KEYS: [&str; 2] = ["largest_file", "uploads_paused"];

fn sorted_keys(v: &Value) -> Vec<String> {
    let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
    k.sort();
    k
}

#[tokio::test]
async fn no_session_gets_401_and_a_healthy_disk_shows_two_views() {
    let server = TestServer::builder()
        .auth("SST1", "SST1SECRET")
        .build()
        .await;
    let ep = server.endpoint();
    let anon = reqwest::Client::builder().no_proxy().build().unwrap();
    let resp = anon.get(format!("{ep}{URL}")).send().await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let admin = admin_http_client(&ep).await;
    let (_, full) = settled(&admin, &ep).await;
    assert_eq!(full["state"], "healthy", "{full}");
    assert!(full["largest_file"].as_u64().unwrap() > 0, "{full}");

    let lift = lift_client(&ep, &admin).await;
    let (text, view) = body(&lift, &ep).await;
    assert_eq!(sorted_keys(&view), BROWSER_KEYS, "{text}");
    assert_eq!(view["uploads_paused"], false);
    assert_eq!(view["largest_file"], full["largest_file"]);
}

/// 25% free is level low (the byte floors lift the percent thresholds), 8% high, 3% critical (default thresholds 15/10/5).
#[tokio::test]
async fn an_admin_sees_the_path_and_a_browser_session_does_not() {
    for (percent, state, paused) in [
        ("25", "filling_up", false),
        ("8", "large_paused", true),
        // A full backend disk alone never pauses the browser view.
        ("3", "backend_paused", false),
    ] {
        let spool = tempfile::TempDir::new().unwrap();
        let spool_path = spool.path().display().to_string();
        let server = TestServer::builder()
            .auth("SST2", "SST2SECRET")
            .env("DGP_SPOOL_DIR", &spool_path)
            .env("DGP_SPOOL_THRESHOLD_BYTES", "1024")
            .env("DGP_TEST_DISK_MONITOR_FREE_PERCENT", percent)
            .build()
            .await;
        let ep = server.endpoint();
        let admin = admin_http_client(&ep).await;
        let (text, full) = settled(&admin, &ep).await;
        assert_eq!(full["state"], state, "{percent}%: {text}");
        match state {
            "filling_up" => assert_eq!(full["disk"]["path"], spool_path.as_str(), "{text}"),
            "large_paused" => {
                assert!(full.get("paused_above").is_none(), "{text}");
                assert_eq!(full["disk"]["path"], spool_path.as_str(), "{text}");
            }
            _ => {
                let b = &full["backends"][0];
                assert!(b["name"].is_string() && b["path"].is_string(), "{text}");
                assert!(b["free_bytes"].is_u64(), "{text}");
                assert_eq!(full["large_uploads_paused"], true, "{text}");
            }
        }

        let lift = lift_client(&ep, &admin).await;
        let (text, view) = body(&lift, &ep).await;
        assert_eq!(sorted_keys(&view), BROWSER_KEYS, "{text}");
        // backend_paused here also has the spool refusing (same disk), so the view is paused.
        let want = paused || state == "backend_paused";
        assert_eq!(view["uploads_paused"], want, "{text}");
        assert!(!text.contains(&spool_path) && !text.contains('/'), "{text}");
    }
}

async fn create_admin_user(
    ep: &str,
    admin: &reqwest::Client,
    name: &str,
) -> (Value, reqwest::Client) {
    let resp = admin
        .post(format!("{ep}/_/api/admin/users"))
        .json(&json!({
            "name": name,
            "permissions": [{ "actions": ["*"], "resources": ["*"] }],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let user: Value = resp.json().await.unwrap();
    let c = reqwest::Client::builder()
        .cookie_store(true)
        .no_proxy()
        .build()
        .unwrap();
    let resp = c
        .post(format!("{ep}/_/api/admin/login-as"))
        .json(&json!({
            "access_key_id": user["access_key_id"],
            "secret_access_key": user["secret_access_key"],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    (user, c)
}

/// An admin who is demoted or disabled after login drops to the browser view.
#[tokio::test]
async fn an_admin_demoted_or_disabled_after_login_gets_the_browser_view() {
    let server = TestServer::builder()
        .auth("SST3", "SST3SECRET")
        .build()
        .await;
    let ep = server.endpoint();
    let admin = admin_http_client(&ep).await;
    for (name, change) in [
        (
            "demoted",
            json!({ "permissions": [{ "actions": ["read", "list"], "resources": ["*"] }] }),
        ),
        ("disabled", json!({ "enabled": false })),
    ] {
        let (user, c) = create_admin_user(&ep, &admin, name).await;
        let (_, full) = settled(&c, &ep).await;
        assert_eq!(full["state"], "healthy", "{name}: {full}");
        let resp = admin
            .put(format!("{ep}/_/api/admin/users/{}", user["id"]))
            .json(&change)
            .send()
            .await
            .unwrap();
        assert!(resp.status().is_success(), "{name}: {}", resp.status());
        let resp = c.get(format!("{ep}{URL}")).send().await.unwrap();
        // A disabled user may lose the session; any answer must be the browser view or a refusal.
        if resp.status() == StatusCode::OK {
            let v: Value = resp.json().await.unwrap();
            assert_eq!(sorted_keys(&v), BROWSER_KEYS, "{name}: {v}");
        } else {
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{name}");
        }
    }
}

/// An open-mode browser session never gets the admin view.
#[tokio::test]
async fn an_open_mode_browser_session_gets_the_browser_view() {
    let server = TestServer::builder().open_access().build().await;
    let ep = server.endpoint();
    let c = reqwest::Client::builder()
        .cookie_store(true)
        .no_proxy()
        .build()
        .unwrap();
    let r = c
        .post(format!("{ep}/_/api/admin/session/open-browser-connect"))
        .json(&json!({ "endpoint": ep, "bucket": "" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let (text, v) = settled(&c, &ep).await;
    assert_eq!(sorted_keys(&v), BROWSER_KEYS, "{text}");
    assert!(!text.contains('/'), "{text}");
}
