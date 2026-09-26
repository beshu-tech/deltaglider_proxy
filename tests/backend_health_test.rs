// SPDX-License-Identifier: BUSL-1.1

//! Backend-health invariant tests (the beshu-b2 incident class).
//!
//! 1. A named backend that is UNREACHABLE at boot must not hide: the proxy
//!    starts DEGRADED and every request to a bucket routed there answers an
//!    honest 503 naming the backend and cause — while buckets on the healthy
//!    default backend keep working.
//! 2. When ALL configured backends fail the boot probe under the default
//!    `enforce` policy, the proxy refuses to start (exit code 1).
//!
//! No MinIO needed: the dead backend is a connection-refused local port.

use crate::common;

use common::TestServer;

/// Degraded boot: default filesystem backend healthy, named S3 backend dead.
/// The routed bucket 503s with the backend named; healthy buckets unaffected.
#[tokio::test]
async fn unhealthy_backend_gates_its_buckets_with_named_503() {
    // Absolute tempdir for the healthy filesystem backend — a relative path
    // would resolve against the spawned proxy's CWD (the repo root) and
    // leave litter behind.
    let good_dir = tempfile::tempdir().expect("tempdir");
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(&format!(
            r#"
backends:
  - name: deadb2
    type: s3
    endpoint: "http://127.0.0.1:1"
    region: us-east-1
    access_key_id: x
    secret_access_key: y
    allow_local: true
  - name: gooddisk
    type: filesystem
    path: {}
buckets:
  doomed:
    backend: deadb2
  alive:
    backend: gooddisk
"#,
            good_dir.path().display()
        ))
        // The harness defaults the boot probe OFF; this test is the gate's
        // coverage, so opt back in. `enforce` won't exit here — the default
        // + gooddisk backends are healthy, so the boot is DEGRADED not dead.
        .env("DGP_BOOT_BACKEND_PROBE", "enforce")
        .build()
        .await;

    let s3 = server.s3_client().await;

    // Healthy path: an EXPLICITLY-ROUTED bucket on a healthy filesystem
    // backend serves normally. (An unrouted create would scan ALL backends
    // for shadow-existence and fail-closed on the dead one — correct, but
    // not this test's subject.)
    s3.create_bucket().bucket("alive").send().await.unwrap();
    s3.put_object()
        .bucket("alive")
        .key("k.txt")
        .body(aws_sdk_s3::primitives::ByteStream::from_static(b"hello"))
        .send()
        .await
        .expect("healthy backend must keep serving");

    // Gated path: ANY verb against the doomed bucket answers 503
    // ServiceUnavailable with the backend name + cause class in the message —
    // never a timeout storm, never a misleading 404/empty-list.
    use aws_sdk_s3::error::ProvideErrorMetadata;
    let err = s3
        .list_objects_v2()
        .bucket("doomed")
        .send()
        .await
        .expect_err("bucket on an unreachable backend must not list");
    assert_eq!(
        err.meta().code().unwrap_or(""),
        "ServiceUnavailable",
        "expected the health gate's 503, got {err:?}"
    );
    let msg = err.meta().message().unwrap_or("").to_string();
    assert!(
        msg.contains("deadb2"),
        "503 message must NAME the unhealthy backend: {msg}"
    );
    assert!(
        msg.contains("unreachable") || msg.contains("unavailable"),
        "503 message must state the cause class: {msg}"
    );

    // The admin backends API surfaces the verdict for the GUI health column.
    let http = reqwest::Client::new();
    let login = http
        .post(format!("{}/_/api/admin/login", server.endpoint()))
        .json(&serde_json::json!({ "password": common::TEST_BOOTSTRAP_PASSWORD }))
        .send()
        .await
        .expect("admin login");
    assert!(login.status().is_success(), "login: {}", login.status());
    let cookie = login
        .headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .split(';')
        .next()
        .unwrap_or_default()
        .to_string();
    let backends: serde_json::Value = http
        .get(format!("{}/_/api/admin/backends", server.endpoint()))
        .header("cookie", &cookie)
        .send()
        .await
        .expect("GET backends")
        .json()
        .await
        .expect("backends json");
    let dead = backends["backends"]
        .as_array()
        .expect("backends array")
        .iter()
        .find(|b| b["name"] == "deadb2")
        .expect("deadb2 listed");
    assert_eq!(
        dead["health"]["status"], "unreachable",
        "health verdict stamped on GET /backends: {dead}"
    );

    // "Test connection" endpoint: re-probe on demand, verdict returned.
    let probe: serde_json::Value = http
        .post(format!(
            "{}/_/api/admin/backends/deadb2/probe",
            server.endpoint()
        ))
        .header("cookie", &cookie)
        .send()
        .await
        .expect("POST probe")
        .json()
        .await
        .expect("probe json");
    assert_eq!(probe["status"], "unreachable", "probe verdict: {probe}");
}

/// ALL backends dead + `enforce` (the default) → the proxy refuses to start.
#[tokio::test]
async fn all_backends_dead_exits_on_boot_under_enforce() {
    let dir = tempfile::tempdir().expect("tempdir");
    let config_path = dir.path().join("cfg.yaml");
    std::fs::write(
        &config_path,
        r#"
access:
  authentication: none
storage:
  backend:
    type: s3
    endpoint: "http://127.0.0.1:1"
    region: us-east-1
    access_key_id: x
    secret_access_key: y
    allow_local: true
"#,
    )
    .expect("write config");

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_deltaglider_proxy"))
        .current_dir(dir.path())
        .env("DGP_CONFIG", &config_path)
        .env("DGP_BOOT_BACKEND_PROBE", "enforce")
        .env("RUST_LOG", "deltaglider_proxy=error")
        .env("DGP_LISTEN_ADDR", "127.0.0.1:0")
        .env_remove("DGP_BOOTSTRAP_PASSWORD_HASH")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn proxy");

    // Connection-refused probes fail fast (2 attempts + 500ms backoff), so
    // exit(1) lands well within this window. Poll instead of a blind wait.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status;
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            panic!(
                "proxy with ALL backends dead must exit under enforce — still running after 30s"
            );
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    };
    assert_eq!(status.code(), Some(1), "expected exit(1), got {status:?}");
}

/// The gate must judge a bucket by the backend the ROUTER sends it to. An
/// unrouted bucket that lives on a healthy named backend was gated by the
/// default backend's health (the config-only resolution), so an outage of
/// the default 503'd buckets it does not host.
#[tokio::test]
async fn unrouted_bucket_on_a_healthy_backend_is_served_while_the_default_is_down() {
    let primary_dir = tempfile::tempdir().expect("tempdir");
    let primary = primary_dir.path().join("primary");
    let disk_dir = tempfile::tempdir().expect("tempdir");
    // `downloads` exists on local-disk only and has no bucket policy.
    std::fs::create_dir_all(disk_dir.path().join("downloads")).unwrap();
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(&format!(
            r#"
backends:
  - name: primary
    type: filesystem
    path: {}
  - name: local-disk
    type: filesystem
    path: {}
"#,
            primary.display(),
            disk_dir.path().display()
        ))
        .env("DGP_BOOT_BACKEND_PROBE", "enforce")
        .build()
        .await;
    let s3 = server.s3_client().await;
    s3.list_objects_v2()
        .bucket("downloads")
        .send()
        .await
        .expect("healthy: the router finds downloads on local-disk");

    // Take the default backend down (its root becomes a file) and re-probe.
    std::fs::remove_dir_all(&primary).unwrap();
    std::fs::write(&primary, b"not a directory").unwrap();
    let admin = common::admin_http_client(&server.endpoint()).await;
    let probe: serde_json::Value = admin
        .post(format!(
            "{}/_/api/admin/backends/primary/probe",
            server.endpoint()
        ))
        .send()
        .await
        .expect("probe")
        .json()
        .await
        .unwrap();
    assert_ne!(probe["verdict"], "healthy", "default must be down: {probe}");

    s3.list_objects_v2()
        .bucket("downloads")
        .send()
        .await
        .expect("a bucket on the healthy named backend must still be served");
}

/// A fake S3 endpoint that answers every request with an empty
/// ListAllMyBuckets result, until `hang` is set: then it reads requests and
/// never answers (a hung backend, like a SIGSTOPped process).
async fn fake_s3(hang: std::sync::Arc<std::sync::atomic::AtomicBool>) -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let hang = hang.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let n = match sock.read(&mut chunk).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    buf.extend_from_slice(&chunk[..n]);
                    // One request per header block; the fake ignores bodies.
                    while let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        buf.drain(..end + 4);
                        if hang.load(std::sync::atomic::Ordering::SeqCst) {
                            std::future::pending::<()>().await;
                        }
                        let body = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
                            <ListAllMyBucketsResult><Owner><ID>x</ID></Owner>\
                            <Buckets></Buckets></ListAllMyBucketsResult>";
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: application/xml\r\n\
                             content-length: {}\r\n\r\n{body}",
                            body.len()
                        );
                        if sock.write_all(resp.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                }
            });
        }
    });
    port
}

async fn ready(ep: &str) -> (u16, serde_json::Value) {
    let resp = reqwest::get(format!("{ep}/_/ready")).await.unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap())
}

fn hung_backend_yaml(port: u16, disk: &std::path::Path) -> String {
    format!(
        r#"
backends:
  - name: local-disk
    type: filesystem
    path: {}
  - name: hetzner-fsn1
    type: s3
    endpoint: "http://127.0.0.1:{port}"
    region: us-east-1
    access_key_id: x
    secret_access_key: y
    allow_local: true
default_backend: local-disk
buckets:
  releases:
    backend: hetzner-fsn1
"#,
        disk.display()
    )
}

/// Browser review #5: a backend that HANGS (answers nothing) must not hang
/// requests. The request times out after DGP_BACKEND_REQUEST_TIMEOUT_SECS
/// with a 503 that names the backend; the timeout marks the backend
/// unhealthy at once, so the next request gets the gate's fast 503; and
/// `/_/ready` lists the backend as unreachable.
#[tokio::test]
async fn hung_backend_times_out_fast_and_is_marked_unhealthy() {
    let hang = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let port = fake_s3(hang.clone()).await;
    let disk = tempfile::tempdir().unwrap();
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(&hung_backend_yaml(port, disk.path()))
        .env("DGP_BOOT_BACKEND_PROBE", "enforce")
        // The loop must not be what finds the hang in this test.
        .env("DGP_BACKEND_HEALTH_INTERVAL_SECS", "3600")
        .env("DGP_BACKEND_REQUEST_TIMEOUT_SECS", "2")
        .build()
        .await;
    let ep = server.endpoint();
    let (_, body) = ready(&ep).await;
    assert_eq!(body["backends"]["hetzner-fsn1"], "healthy", "{body}");

    hang.store(true, std::sync::atomic::Ordering::SeqCst);
    // No client retries: the test times the proxy, not the SDK's backoff.
    let s3 = aws_sdk_s3::Client::from_conf(
        server
            .s3_client()
            .await
            .config()
            .to_builder()
            .retry_config(aws_sdk_s3::config::retry::RetryConfig::disabled())
            .build(),
    );
    use aws_sdk_s3::error::ProvideErrorMetadata;
    let t0 = std::time::Instant::now();
    let err = s3
        .head_object()
        .bucket("releases")
        .key("app-1.0.0.tar")
        .send()
        .await
        .expect_err("a hung backend cannot answer");
    let first = t0.elapsed();
    assert!(
        first < std::time::Duration::from_secs(15),
        "the request must end near the 2s timeout, took {first:?}"
    );
    // HEAD has no body, so check the status; GET carries the message.
    assert_eq!(
        err.raw_response().map(|r| r.status().as_u16()),
        Some(503),
        "{err:?}"
    );

    let t1 = std::time::Instant::now();
    let err = s3
        .get_object()
        .bucket("releases")
        .key("app-1.0.0.tar")
        .send()
        .await
        .expect_err("gated");
    assert!(
        t1.elapsed() < std::time::Duration::from_secs(1),
        "after the timeout the gate answers at once, took {:?}",
        t1.elapsed()
    );
    assert_eq!(err.meta().code(), Some("ServiceUnavailable"), "{err:?}");
    assert!(
        err.meta().message().unwrap_or("").contains("hetzner-fsn1"),
        "{err:?}"
    );

    let (status, body) = ready(&ep).await;
    assert_eq!(body["backends"]["hetzner-fsn1"], "unreachable", "{body}");
    assert_eq!(body["backends"]["local-disk"], "healthy", "{body}");
    assert_eq!(
        status, 200,
        "one healthy backend keeps the node ready: {body}"
    );
}

/// The health loop probes HEALTHY backends too: a backend that hangs with no
/// traffic turns unreachable, and it recovers on its own when it answers.
#[tokio::test]
async fn health_loop_finds_a_hang_and_the_recovery() {
    let hang = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let port = fake_s3(hang.clone()).await;
    let disk = tempfile::tempdir().unwrap();
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .extra_yaml_storage_section(&hung_backend_yaml(port, disk.path()))
        .env("DGP_BOOT_BACKEND_PROBE", "enforce")
        .env("DGP_BACKEND_HEALTH_INTERVAL_SECS", "1")
        .build()
        .await;
    let ep = server.endpoint();
    let wait_for = |want: &'static str| {
        let ep = ep.clone();
        async move {
            // A hung probe takes two 5s attempts before it reports.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(40);
            loop {
                let (_, body) = ready(&ep).await;
                if body["backends"]["hetzner-fsn1"] == want {
                    return;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "hetzner-fsn1 never became {want}: {body}"
                );
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
    };
    wait_for("healthy").await;
    hang.store(true, std::sync::atomic::Ordering::SeqCst);
    wait_for("unreachable").await;
    hang.store(false, std::sync::atomic::Ordering::SeqCst);
    wait_for("healthy").await;
}
