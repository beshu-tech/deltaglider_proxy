// SPDX-License-Identifier: BUSL-1.1

//! `/_/health`, `/_/ready`, `/_/stats` and the `HEAD /` probe: response
//! shapes, the session gate on stats, and readiness against a backend that
//! answers, throttles its LIST, or goes down (a local fake S3 whose mode
//! the test switches).

use crate::common;

use common::{admin_http_client, TestServer};
use serde_json::Value;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

async fn get_json(client: &reqwest::Client, url: &str) -> (u16, Value) {
    let r = client.get(url).send().await.unwrap();
    let code = r.status().as_u16();
    (code, r.json().await.unwrap_or_default())
}

#[tokio::test]
async fn health_and_stats_shapes_and_the_stats_session_gate() {
    let server = TestServer::filesystem().await;
    let ep = server.endpoint();
    let anon = reqwest::Client::new();

    let (code, h) = get_json(&anon, &format!("{ep}/_/health")).await;
    assert_eq!(code, 200, "liveness needs no session: {h}");
    assert_eq!(h["status"], "healthy");
    assert_eq!(h["backend"], "live");
    for f in [
        "peak_rss_bytes",
        "cache_size_bytes",
        "cache_max_bytes",
        "cache_entries",
    ] {
        assert!(h[f].is_u64(), "{f}: {h}");
    }
    assert!(h["peak_rss_bytes"].as_u64().unwrap() > 0, "{h}");
    assert!(h.get("version").is_none(), "no version on health: {h}");

    let (code, _) = get_json(&anon, &format!("{ep}/_/stats")).await;
    assert_eq!(code, 401, "stats reveal sizes: session only");

    let http = server.http();
    for (k, n) in [("s/a.bin", 1000usize), ("s/b.bin", 3000)] {
        common::put_object(
            &http,
            &ep,
            server.bucket(),
            k,
            vec![1u8; n],
            "application/octet-stream",
        )
        .await;
    }
    let admin = admin_http_client(&ep).await;
    let (code, s) = get_json(&admin, &format!("{ep}/_/stats?bucket={}", server.bucket())).await;
    assert_eq!(code, 200, "{s}");
    assert_eq!(s["total_objects"], 2, "{s}");
    assert_eq!(s["total_original_size"], 4000, "{s}");
    assert!(s["total_stored_size"].as_u64().unwrap() > 0, "{s}");
    assert!(s["savings_percentage"].is_number(), "{s}");
    let (_, other) = get_json(&admin, &format!("{ep}/_/stats?bucket=never-used")).await;
    assert_eq!(
        other["total_objects"], 0,
        "an unknown bucket reads zeros: {other}"
    );
    let (code, all) = get_json(&admin, &format!("{ep}/_/stats")).await;
    assert_eq!(code, 200);
    assert!(all["total_objects"].as_u64().unwrap() >= 2, "{all}");

    // Cyberduck-style probe: HEAD on the service root.
    let r = anon.head(format!("{ep}/")).send().await.unwrap();
    assert!(r.status().is_success(), "HEAD / answers: {}", r.status());
}

#[tokio::test]
async fn ready_on_a_healthy_filesystem_backend() {
    let server = TestServer::filesystem().await;
    let (code, r) = get_json(
        &reqwest::Client::new(),
        &format!("{}/_/ready", server.endpoint()),
    )
    .await;
    assert_eq!(code, 200, "{r}");
    assert_eq!(r["status"], "ready");
    assert_eq!(r["backend"], "ready");
    assert_eq!(r["config_db"], "ready");
}

/// A fake S3: `mode` 0 answers everything, 1 throttles ListBuckets (503)
/// but answers HEAD, 2 answers every request with 500.
async fn fake_s3() -> (String, Arc<AtomicU8>) {
    use axum::http::{Method, StatusCode};
    let mode = Arc::new(AtomicU8::new(0));
    let m = mode.clone();
    let app = axum::Router::new().fallback(move |method: Method, uri: axum::http::Uri| {
        let m = m.clone();
        async move {
            let mode = m.load(Ordering::SeqCst);
            let list = method == Method::GET && uri.path() == "/";
            if mode == 2 || (mode == 1 && list) {
                let code = if mode == 1 {
                    StatusCode::SERVICE_UNAVAILABLE
                } else {
                    StatusCode::INTERNAL_SERVER_ERROR
                };
                return (code, "<Error><Code>SlowDown</Code></Error>".to_string());
            }
            if list {
                return (
                    StatusCode::OK,
                    "<?xml version=\"1.0\" encoding=\"UTF-8\"?><ListAllMyBucketsResult>\
                     <Owner><ID>o</ID></Owner><Buckets></Buckets></ListAllMyBucketsResult>"
                        .to_string(),
                );
            }
            if method == Method::HEAD {
                return (StatusCode::NOT_FOUND, String::new());
            }
            (StatusCode::OK, String::new())
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), mode)
}

async fn ready(ep: &str) -> (u16, Value) {
    get_json(&reqwest::Client::new(), &format!("{ep}/_/ready")).await
}

/// Strict contract (no last-known-good window): the LIST must succeed.
#[tokio::test]
async fn ready_is_503_when_the_backend_is_down() {
    let (s3, mode) = fake_s3().await;
    let server = TestServer::builder()
        .s3_endpoint(&s3)
        .env("DGP_READY_TIMEOUT_SECS", "5")
        .env("DGP_READY_RETRIES", "0")
        .build()
        .await;
    let ep = server.endpoint();
    let (code, r) = ready(&ep).await;
    assert_eq!(code, 200, "a live backend is ready: {r}");
    assert_eq!(r["backend"], "ready");

    mode.store(2, Ordering::SeqCst);
    let (code, r) = ready(&ep).await;
    assert_eq!(code, 503, "a failing backend is not ready: {r}");
    assert_eq!(r["status"], "not_ready");
    // The SDK retries the 500s inside the per-attempt timeout, so the verdict
    // is `timeout` or `unreachable`; either is a hard failure.
    assert!(
        matches!(r["backend"].as_str(), Some("unreachable" | "timeout")),
        "{r}"
    );
    let (code, _) = get_json(&reqwest::Client::new(), &format!("{ep}/_/health")).await;
    assert_eq!(code, 200, "liveness stays up: it does no I/O");

    mode.store(0, Ordering::SeqCst);
    let (code, r) = ready(&ep).await;
    assert_eq!(code, 200, "ready again once the backend answers: {r}");
}

/// With a last-known-good window: a throttled LIST whose HEAD still answers
/// is `degraded` (ready); nothing answering but a recent success is
/// `cached` (ready).
#[tokio::test]
async fn ready_rides_out_a_list_throttle_and_a_short_outage_with_a_window() {
    let (s3, mode) = fake_s3().await;
    let server = TestServer::builder()
        .s3_endpoint(&s3)
        .env("DGP_READY_TIMEOUT_SECS", "5")
        .env("DGP_READY_RETRIES", "0")
        .env("DGP_READY_CACHE_TTL_SECS", "300")
        .build()
        .await;
    let ep = server.endpoint();
    let (code, r) = ready(&ep).await;
    assert_eq!((code, r["backend"].as_str()), (200, Some("ready")), "{r}");

    mode.store(1, Ordering::SeqCst);
    let (code, r) = ready(&ep).await;
    assert_eq!(code, 200, "{r}");
    assert_eq!(
        r["backend"], "degraded",
        "LIST throttled, HEAD answers: {r}"
    );

    mode.store(2, Ordering::SeqCst);
    let (code, r) = ready(&ep).await;
    assert_eq!(code, 200, "{r}");
    assert_eq!(
        r["backend"], "cached",
        "nothing answers, but a recent success: {r}"
    );
}
