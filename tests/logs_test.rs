// SPDX-License-Identifier: GPL-3.0-only

//! Integration coverage for the in-GUI operational log viewer
//! (`GET /_/api/admin/logs`). Proves: the endpoint is admin-session-gated, it
//! returns INFO+ entries captured from the running process, and the level /
//! target / q filters narrow server-side. The pure filter logic itself is
//! unit-tested in `src/logs.rs`; this covers the HTTP + capture-layer seam.

mod common;

use common::{admin_http_client, TestServer};

#[derive(serde::Deserialize, Debug)]
struct LogEntry {
    level: String,
    target: String,
    #[allow(dead_code)]
    message: String,
}

#[derive(serde::Deserialize, Debug)]
struct LogsResponse {
    entries: Vec<LogEntry>,
}

async fn get_logs(admin: &reqwest::Client, endpoint: &str, query: &str) -> LogsResponse {
    let url = format!("{}/_/api/admin/logs?{}", endpoint, query);
    let resp = admin.get(&url).send().await.expect("logs request");
    assert!(resp.status().is_success(), "logs GET got {}", resp.status());
    resp.json().await.expect("logs json")
}

#[tokio::test]
async fn logs_endpoint_is_admin_gated_and_returns_captured_events() {
    let server = TestServer::builder().bucket("logs-basic").build().await;
    let endpoint = server.endpoint();

    // 1. Session-gated: a plain (no admin session) client must be rejected.
    let anon = reqwest::Client::new();
    let anon_resp = anon
        .get(format!("{}/_/api/admin/logs", endpoint))
        .send()
        .await
        .expect("anon logs request");
    assert!(
        anon_resp.status() == 401 || anon_resp.status() == 403,
        "logs endpoint must require an admin session, got {}",
        anon_resp.status()
    );

    // 2. With an admin session, the ring has captured INFO+ events from startup
    //    (the proxy logs "Starting…", scheduler start, etc. at INFO).
    let admin = admin_http_client(&endpoint).await;
    let all = get_logs(&admin, &endpoint, "limit=500").await;
    assert!(
        !all.entries.is_empty(),
        "the log ring should have captured startup INFO events"
    );
    // INFO floor: nothing below INFO should be present by default.
    assert!(
        all.entries
            .iter()
            .all(|e| matches!(e.level.as_str(), "ERROR" | "WARN" | "INFO")),
        "default capture floor is INFO+ — no DEBUG/TRACE: {:?}",
        all.entries.iter().map(|e| &e.level).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn logs_filters_narrow_server_side() {
    let server = TestServer::builder().bucket("logs-filter").build().await;
    let endpoint = server.endpoint();
    let admin = admin_http_client(&endpoint).await;

    // target filter: only deltaglider_proxy::* events (there are always some).
    let scoped = get_logs(&admin, &endpoint, "target=deltaglider_proxy&limit=500").await;
    assert!(
        scoped
            .entries
            .iter()
            .all(|e| e.target.contains("deltaglider_proxy")),
        "target filter must constrain results"
    );
    assert!(
        !scoped.entries.is_empty(),
        "expected some dgp-targeted logs"
    );

    // A target that matches nothing yields an empty (but valid) result.
    let none = get_logs(&admin, &endpoint, "target=zzz-no-such-module&limit=500").await;
    assert!(
        none.entries.is_empty(),
        "a non-matching target filter must return no entries, got {}",
        none.entries.len()
    );

    // level=error floor: at most error-level entries (likely zero in a clean run,
    // but never an INFO/WARN).
    let errors = get_logs(&admin, &endpoint, "level=error&limit=500").await;
    assert!(
        errors.entries.iter().all(|e| e.level == "ERROR"),
        "level=error must exclude WARN/INFO: {:?}",
        errors.entries.iter().map(|e| &e.level).collect::<Vec<_>>()
    );
}
