// SPDX-License-Identifier: GPL-3.0-only

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

mod common;

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
