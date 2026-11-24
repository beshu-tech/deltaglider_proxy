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
    let (_, full) = body(&admin, &ep).await;
    assert_eq!(full["state"], "healthy", "{full}");
    assert!(full["largest_file"].as_u64().unwrap() > 0, "{full}");

    let lift = lift_client(&ep, &admin).await;
    let (text, view) = body(&lift, &ep).await;
    assert_eq!(
        sorted_keys(&view),
        ["largest_file", "paused_above", "uploads_paused"],
        "{text}"
    );
    assert_eq!(view["uploads_paused"], false);
    assert_eq!(view["largest_file"], full["largest_file"]);
}

/// 25% free is level low (the byte floors lift the percent thresholds), 8% high, 3% critical (default thresholds 15/10/5).
#[tokio::test]
async fn an_admin_sees_the_path_and_a_browser_session_does_not() {
    for (percent, state, paused) in [
        ("25", "filling_up", false),
        ("8", "large_paused", true),
        ("3", "backend_paused", true),
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
        let (text, full) = body(&admin, &ep).await;
        assert_eq!(full["state"], state, "{percent}%: {text}");
        match state {
            "filling_up" => assert_eq!(full["disk"]["path"], spool_path.as_str(), "{text}"),
            "large_paused" => {
                assert_eq!(full["paused_above"], 1024, "{text}");
                assert_eq!(full["disk"]["path"], spool_path.as_str(), "{text}");
            }
            _ => {
                let b = &full["backends"][0];
                assert!(b["name"].is_string() && b["path"].is_string(), "{text}");
                assert!(b["free_bytes"].is_u64(), "{text}");
            }
        }

        let lift = lift_client(&ep, &admin).await;
        let (text, view) = body(&lift, &ep).await;
        assert_eq!(
            sorted_keys(&view),
            ["largest_file", "paused_above", "uploads_paused"],
            "{text}"
        );
        assert_eq!(view["uploads_paused"], paused, "{text}");
        assert_eq!(
            view["paused_above"],
            if state == "large_paused" {
                json!(1024)
            } else {
                Value::Null
            },
            "{text}"
        );
        assert!(!text.contains(&spool_path) && !text.contains('/'), "{text}");
    }
}
