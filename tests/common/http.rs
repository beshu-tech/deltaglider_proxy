// SPDX-License-Identifier: BUSL-1.1

//! HTTP helpers: the logged-in admin client, raw S3 requests, quick setup,
//! and `/_/metrics` scraping.

use super::*;

/// Create a reqwest client that is logged in to the admin API.
/// Uses the known [`TEST_BOOTSTRAP_PASSWORD`] to authenticate.
pub async fn admin_http_client(endpoint: &str) -> reqwest::Client {
    admin_http_client_with_password(endpoint, TEST_BOOTSTRAP_PASSWORD).await
}

/// Like [`admin_http_client`] but with an explicit bootstrap password.
/// Used by HA-sync tests that spawn a replica with a non-default
/// password via [`TestServerBuilder::bootstrap_password`].
pub async fn admin_http_client_with_password(endpoint: &str, password: &str) -> reqwest::Client {
    let jar = std::sync::Arc::new(reqwest::cookie::Jar::default());
    let client = reqwest::Client::builder()
        .cookie_provider(jar)
        .build()
        .unwrap();

    let resp = client
        .post(format!("{}/_/api/admin/login", endpoint))
        .json(&serde_json::json!({ "password": password }))
        .send()
        .await
        .expect("Admin login request failed");
    assert!(
        resp.status().is_success(),
        "Admin login failed: {}",
        resp.status()
    );
    client
}

// === Shared HTTP helpers (raw S3 requests) ===
//
// Each takes `&impl S3Requests`: `server.http()` (signed when auth is on) or
// a plain reqwest client (unsigned, open-access servers only).

/// Build an S3 object URL from endpoint, bucket, and key.
fn object_url(endpoint: &str, bucket: &str, key: &str) -> String {
    format!("{}/{}/{}", endpoint, bucket, key)
}

/// PUT an object via reqwest and return the response.
pub async fn put_object(
    client: &impl S3Requests,
    endpoint: &str,
    bucket: &str,
    key: &str,
    data: Vec<u8>,
    content_type: &str,
) -> reqwest::Response {
    let url = object_url(endpoint, bucket, key);
    let resp = client
        .s3_request(reqwest::Method::PUT, &url)
        .header("content-type", content_type)
        .body(data)
        .send()
        .await
        .expect("PUT failed");
    if !resp.status().is_success() {
        let st = resp.status();
        let body = resp.text().await.unwrap_or_default();
        panic!("PUT {} failed: {} body={}", key, st, body);
    }
    resp
}

/// PUT an object and return the x-amz-storage-type header value.
pub async fn put_and_get_storage_type(
    client: &impl S3Requests,
    endpoint: &str,
    bucket: &str,
    key: &str,
    data: Vec<u8>,
    content_type: &str,
) -> String {
    let resp = put_object(client, endpoint, bucket, key, data, content_type).await;
    resp.headers()
        .get("x-amz-storage-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string()
}

/// GET an object and return the body bytes.
pub async fn get_bytes(
    client: &impl S3Requests,
    endpoint: &str,
    bucket: &str,
    key: &str,
) -> Vec<u8> {
    let url = object_url(endpoint, bucket, key);
    let resp = client
        .s3_request(reqwest::Method::GET, &url)
        .send()
        .await
        .expect("GET failed");
    assert!(
        resp.status().is_success(),
        "GET {} failed: {}",
        key,
        resp.status()
    );
    resp.bytes().await.unwrap().to_vec()
}

/// HEAD an object and return response headers.
pub async fn head_headers(
    client: &impl S3Requests,
    endpoint: &str,
    bucket: &str,
    key: &str,
) -> reqwest::header::HeaderMap {
    let url = object_url(endpoint, bucket, key);
    let resp = client
        .s3_request(reqwest::Method::HEAD, &url)
        .send()
        .await
        .expect("HEAD failed");
    assert!(
        resp.status().is_success(),
        "HEAD {} failed: {}",
        key,
        resp.status()
    );
    resp.headers().clone()
}

/// DELETE an object via reqwest (tolerates 204 and 404).
pub async fn delete_object(client: &impl S3Requests, endpoint: &str, bucket: &str, key: &str) {
    let url = object_url(endpoint, bucket, key);
    let resp = client
        .s3_request(reqwest::Method::DELETE, &url)
        .send()
        .await
        .expect("DELETE failed");
    assert!(
        resp.status().is_success()
            || resp.status().as_u16() == 204
            || resp.status().as_u16() == 404,
        "DELETE {} failed: {}",
        key,
        resp.status()
    );
}

/// Make a raw ListObjectsV2 request and return the XML body.
pub async fn list_objects_raw(
    client: &impl S3Requests,
    endpoint: &str,
    bucket: &str,
    params: &str,
) -> String {
    let url = format!("{}/{}?list-type=2&{}", endpoint, bucket, params);
    let resp = client
        .s3_request(reqwest::Method::GET, &url)
        .send()
        .await
        .unwrap();
    assert!(
        resp.status().is_success(),
        "ListObjects failed: {}",
        resp.status()
    );
    resp.text().await.unwrap()
}

// === Quick-setup helpers (reduce test boilerplate) ===

/// Quick setup: filesystem server (auth on) + a client that signs with its
/// credentials.
pub async fn signed_setup() -> (TestServer, S3Http) {
    let server = TestServer::filesystem().await;
    let http = server.http();
    (server, http)
}

/// Upload a simple test file, return its bytes
pub async fn upload_test_data(
    http: &impl S3Requests,
    endpoint: &str,
    bucket: &str,
    key: &str,
    size: usize,
) -> Vec<u8> {
    let data = generate_binary(size, 42);
    put_object(
        http,
        endpoint,
        bucket,
        key,
        data.clone(),
        "application/octet-stream",
    )
    .await;
    data
}

/// Parsed snapshot of the un-labelled streaming-copy metrics scraped from
/// `GET /_/metrics`. Fields map 1:1 to the `deltaglider_*` series; absent
/// lines default to 0.
#[derive(Debug, Clone, Default)]
pub struct MetricsSnapshot {
    pub part_bytes_resident: u64,
    pub part_bytes_resident_peak: u64,
    pub parts_inflight: u64,
    pub parts_inflight_peak: u64,
    pub objects_inflight: u64,
    pub objects_inflight_peak: u64,
    pub multipart_parts_total: u64,
    pub part_retries_total: u64,
    pub bytes_streamed_total: u64,
    pub delta_bytes_saved_total: u64,
    pub delta_passthrough_bytes_saved_total: u64,
    pub list_calls_total: u64,
    pub head_calls_total: u64,
    pub dirs_completed_total: u64,
    pub process_peak_rss_bytes: u64,
}

/// Scrape `GET /_/metrics` and parse the un-labelled Prometheus lines
/// (`name value`) into a [`MetricsSnapshot`]. Snapshot ONCE after a
/// synchronous run-now returns 200 (peaks have settled).
pub async fn metrics_snapshot(endpoint: &str) -> MetricsSnapshot {
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("metrics client");
    let url = format!("{}/_/metrics", endpoint);
    let body = client
        .get(&url)
        .send()
        .await
        .expect("GET /_/metrics failed")
        .text()
        .await
        .expect("read /_/metrics body");

    let mut snap = MetricsSnapshot::default();
    for line in body.lines() {
        if line.starts_with('#') {
            continue;
        }
        // Un-labelled series: `name value`. Skip labelled lines (name{..}).
        let Some((name, value)) = line.rsplit_once(' ') else {
            continue;
        };
        if name.contains('{') {
            continue;
        }
        let parsed = value.trim().parse::<f64>().unwrap_or(0.0) as u64;
        match name {
            "deltaglider_replication_list_calls_total" => snap.list_calls_total = parsed,
            "deltaglider_replication_head_calls_total" => snap.head_calls_total = parsed,
            "deltaglider_replication_dirs_completed_total" => snap.dirs_completed_total = parsed,
            "deltaglider_replication_part_bytes_resident" => snap.part_bytes_resident = parsed,
            "deltaglider_replication_part_bytes_resident_peak" => {
                snap.part_bytes_resident_peak = parsed
            }
            "deltaglider_replication_parts_inflight" => snap.parts_inflight = parsed,
            "deltaglider_replication_parts_inflight_peak" => snap.parts_inflight_peak = parsed,
            "deltaglider_replication_objects_inflight" => snap.objects_inflight = parsed,
            "deltaglider_replication_objects_inflight_peak" => snap.objects_inflight_peak = parsed,
            "deltaglider_replication_multipart_parts_total" => snap.multipart_parts_total = parsed,
            "deltaglider_replication_part_retries_total" => snap.part_retries_total = parsed,
            "deltaglider_replication_bytes_streamed_total" => snap.bytes_streamed_total = parsed,
            "deltaglider_delta_bytes_saved_total" => snap.delta_bytes_saved_total = parsed,
            "deltaglider_replication_delta_passthrough_bytes_saved_total" => {
                snap.delta_passthrough_bytes_saved_total = parsed
            }
            "process_peak_rss_bytes" => snap.process_peak_rss_bytes = parsed,
            _ => {}
        }
    }
    snap
}

pub async fn metrics_text(endpoint: &str) -> String {
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("metrics client");
    let url = format!("{}/_/metrics", endpoint);
    client
        .get(&url)
        .send()
        .await
        .expect("GET /_/metrics failed")
        .text()
        .await
        .expect("read /_/metrics body")
}

pub fn prometheus_counter_has_labels(metrics: &str, name: &str, labels: &[&str]) -> bool {
    metrics.lines().any(|line| {
        if !line.starts_with(name) || labels.iter().any(|label| !line.contains(label)) {
            return false;
        }
        line.rsplit_once(' ')
            .and_then(|(_, value)| value.trim().parse::<f64>().ok())
            .map(|value| value > 0.0)
            .unwrap_or(false)
    })
}
