// SPDX-License-Identifier: BUSL-1.1

//! Boot resilience (epic #145, E7): a transient outage of something the proxy
//! only COORDINATES through must not stop the boot. No MinIO needed: the
//! "outage" is a local port that accepts connections and drops them.

use crate::common;

use common::TestServer;

/// A listener that accepts every connection and closes it at once: the shape
/// of an S3 endpoint that is up at the TCP level and dead above it.
fn dropping_endpoint() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            drop(stream);
        }
    });
    format!("http://{addr}")
}

/// F12: the coordination bucket cannot be probed (transport error). The
/// verdict is unknown, not non-CAS: the proxy must boot and answer
/// `/_/health`, with the boot probe on `warn` as the operator asked.
#[tokio::test]
async fn an_unreachable_coordination_bucket_does_not_stop_the_boot() {
    let endpoint = dropping_endpoint();
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .s3_endpoint(&endpoint)
        .config_sync_bucket("coordination")
        .env("DGP_BOOT_BACKEND_PROBE", "warn")
        .build()
        .await;
    let health = reqwest::get(format!("{}/_/health", server.endpoint()))
        .await
        .expect("health request");
    assert_eq!(health.status().as_u16(), 200);
}

/// Log lines that only a running boot after the listener step writes.
const WORKER_LOG_MARKERS: &[&str] = &[
    "scheduler started",
    "Job-plane lease",
    "Backend health gate",
    "External auth:",
];

/// F13: an occupied port fails the boot BEFORE any worker starts, and the
/// error names the address (human text, not `Os { code: 98 .. }`).
#[tokio::test]
async fn an_occupied_port_fails_the_boot_before_any_worker_starts() {
    let mut server = TestServer::filesystem().await;
    server.kill();
    let port: u16 = server
        .endpoint()
        .rsplit(':')
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let addr = format!("127.0.0.1:{port}");
    let _holder = {
        // The killed child frees the port a moment later.
        let mut bound = None;
        for _ in 0..40 {
            match std::net::TcpListener::bind(&addr) {
                Ok(l) => {
                    bound = Some(l);
                    break;
                }
                Err(_) => tokio::time::sleep(std::time::Duration::from_millis(50)).await,
            }
        }
        bound.expect("take the port")
    };
    let (status, output) = server.spawn_expect_exit(&[("RUST_LOG", "info")], 30).await;
    let status = status.expect("the proxy must exit on an occupied port");
    assert!(!status.success(), "exit status {status:?}\n{output}");
    assert!(
        output.contains(&addr),
        "the error must name the listen address {addr}:\n{output}"
    );
    assert!(
        !output.contains("Os {"),
        "raw Debug error in the output:\n{output}"
    );
    for worker in WORKER_LOG_MARKERS {
        assert!(
            !output.contains(worker),
            "worker line {worker:?} logged before the bind failed:\n{output}"
        );
    }
}

/// F13: a second process on the same state directory is refused with a
/// message that names the directory; the first one keeps serving.
#[tokio::test]
async fn a_second_process_on_the_same_state_directory_is_refused() {
    let mut server = TestServer::filesystem().await;
    // Another port, the same config file (so the same state directory).
    let other = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let other_port = other.local_addr().unwrap().port().to_string();
    drop(other);
    let (status, output) = server
        .spawn_expect_exit(
            &[("DGP_LISTEN_ADDR", &format!("127.0.0.1:{other_port}"))],
            30,
        )
        .await;
    let status = status.expect("the second process must exit");
    assert!(!status.success(), "{output}");
    assert!(
        output.contains("another DeltaGlider Proxy process already uses the state directory"),
        "{output}"
    );
    let health = reqwest::get(format!("{}/_/health", server.endpoint()))
        .await
        .expect("health");
    assert_eq!(
        health.status().as_u16(),
        200,
        "the first process must serve"
    );
}

/// F13: an unusable `DGP_SPOOL_DIR` is a clean exit that names the variable
/// and the path (it was a panic without either).
#[tokio::test]
async fn an_unusable_spool_dir_names_the_variable_and_the_path() {
    let mut server = TestServer::filesystem().await;
    server.kill();
    let file = tempfile::NamedTempFile::new().unwrap();
    // A path below a regular file cannot be a directory.
    let bad = file.path().join("spool");
    let (status, output) = server
        .spawn_expect_exit(&[("DGP_SPOOL_DIR", bad.to_str().unwrap())], 30)
        .await;
    assert!(!status.expect("must exit").success(), "{output}");
    assert!(output.contains("DGP_SPOOL_DIR"), "{output}");
    assert!(output.contains(bad.to_str().unwrap()), "{output}");
    assert!(!output.contains("panicked"), "{output}");
}

/// F14: three identity providers that never answer discovery must not delay
/// the first `/_/health`. Discovery runs concurrently and off the boot path.
#[tokio::test]
async fn hanging_identity_providers_do_not_delay_the_boot() {
    // An IdP that accepts connections and never answers.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let issuer = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming().flatten() {
            held.push(stream);
        }
    });

    let mut server = TestServer::filesystem().await;
    let ep = server.endpoint();
    let admin = common::admin_http_client(&ep).await;
    for name in ["idp-a", "idp-b", "idp-c"] {
        // The request waits for discovery; the client gives up early, and
        // the mutation completes anyway (the provider is saved).
        let _ = admin
            .post(format!("{ep}/_/api/admin/ext-auth/providers"))
            .timeout(std::time::Duration::from_millis(500))
            .json(&serde_json::json!({
                "name": name, "provider_type": "oidc", "client_id": "c",
                "client_secret": "s", "issuer_url": issuer,
                "scopes": "openid email profile",
                "extra_config": { "allow_local": true },
            }))
            .send()
            .await;
    }
    let listed: serde_json::Value = admin
        .get(format!("{ep}/_/api/admin/ext-auth/providers"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listed.as_array().map(Vec::len), Some(3), "{listed}");

    server.terminate();
    let started = std::time::Instant::now();
    server.respawn_with_env(&[]).await;
    let boot = started.elapsed();
    assert!(
        boot < std::time::Duration::from_secs(8),
        "the boot waited on identity-provider discovery: {boot:?}"
    );
}
