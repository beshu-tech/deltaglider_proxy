// SPDX-License-Identifier: BUSL-1.1

//! Boot resilience (epic #145, E7): a transient outage of something the proxy
//! only COORDINATES through must not stop the boot. No MinIO needed: the
//! "outage" is a local port that accepts connections and drops them.

use crate::common;

use common::{S3Requests, TestServer};

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
/// message that names the directory; the first one keeps serving. The wait
/// for the lock is one second here (the default is 60).
#[tokio::test]
async fn a_second_process_on_the_same_state_directory_is_refused() {
    let mut server = TestServer::filesystem().await;
    // Another port, the same config file (so the same state directory).
    let other = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let other_port = other.local_addr().unwrap().port().to_string();
    drop(other);
    let (status, output) = server
        .spawn_expect_exit(
            &[
                ("DGP_LISTEN_ADDR", &format!("127.0.0.1:{other_port}")),
                ("DGP_STATE_LOCK_WAIT_SECS", "1"),
            ],
            30,
        )
        .await;
    let status = status.expect("the second process must exit");
    assert!(!status.success(), "{output}");
    assert!(output.contains("did not stop within 1 s"), "{output}");
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

/// A rolling deploy: the new process starts while the old one still stops.
/// It waits for the state directory lock, says so once, and takes over when
/// the old process exits inside the wait.
#[tokio::test]
async fn a_second_process_takes_over_when_the_first_one_exits_in_time() {
    let mut server = TestServer::filesystem().await;
    let other = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let other_port = other.local_addr().unwrap().port();
    drop(other);
    let mut second = server.spawn_sibling(&[
        ("DGP_LISTEN_ADDR", &format!("127.0.0.1:{other_port}")),
        ("DGP_STATE_LOCK_WAIT_SECS", "60"),
        ("RUST_LOG", "info"),
    ]);
    // It must wait, not exit, and say that it waits.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !second.output().contains("waiting up to 60 s") {
        assert!(second.exited().is_none(), "{}", second.output());
        assert!(
            std::time::Instant::now() < deadline,
            "no wait message:\n{}",
            second.output()
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(second.exited().is_none(), "{}", second.output());
    server.terminate();
    let url = format!("http://127.0.0.1:{other_port}/_/health");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if let Ok(r) = reqwest::get(&url).await {
            if r.status().as_u16() == 200 {
                break;
            }
        }
        assert!(second.exited().is_none(), "{}", second.output());
        assert!(
            std::time::Instant::now() < deadline,
            "the second process never became healthy:\n{}",
            second.output()
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(
        second.output().matches("waiting up to").count(),
        1,
        "the wait is logged once:\n{}",
        second.output()
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

/// Every `.dg-tmp.*` file below `dir`.
fn temp_files_below(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                stack.push(path);
            } else if entry.file_name().to_string_lossy().starts_with(".dg-tmp.") {
                found.push(path);
            }
        }
    }
    found
}

/// The boot sweep removes old `.dg-tmp.*` and `.tmpXXXXXX` files. A client
/// must not be able to store such a key, or the next boot would delete it.
#[tokio::test]
async fn a_client_cannot_store_a_key_that_the_temp_sweep_would_delete() {
    let server = TestServer::filesystem().await;
    let ep = server.endpoint();
    let bucket = server.bucket().to_string();
    let data = server.data_dir().expect("data dir").to_path_buf();
    let http = server.http();
    // Passthrough names and a delta-eligible one.
    for key in ["a/.dg-tmp.x", "a/.tmpAb12Cd", "a/.dg-tmp.y.zip"] {
        let r = http
            .s3_request(reqwest::Method::PUT, &format!("{ep}/{bucket}/{key}"))
            .body(b"payload".to_vec())
            .send()
            .await
            .unwrap();
        assert!(
            r.status().is_client_error(),
            "{key}: {} {}",
            r.status(),
            r.text().await.unwrap_or_default()
        );
    }
    assert!(
        temp_files_below(&data).is_empty(),
        "a refused key left a file"
    );
}

/// F22: a proxy killed with SIGKILL in the middle of a storage write leaves
/// its `.dg-tmp.*` file. The next boot on the same data directory removes it
/// when it is old, keeps a fresh one (a live writer may hold it), and LIST
/// never shows either.
///
/// The test does not race a live write for its temp file (that depends on
/// timing). It kills the proxy with SIGKILL, then plants the leftovers a
/// killed write leaves, with known ages, and restarts.
#[tokio::test]
async fn a_restart_after_sigkill_sweeps_the_stale_temp_file() {
    let mut server = TestServer::filesystem().await;
    let ep = server.endpoint();
    let bucket = server.bucket().to_string();
    let data = server.data_dir().expect("data dir").to_path_buf();

    server.kill(); // SIGKILL: no destructor runs.

    let folder = data.join(&bucket).join("big");
    std::fs::create_dir_all(&folder).unwrap();
    let plant = |name: &str, age_secs: u64| {
        let path = folder.join(name);
        std::fs::write(&path, b"leftover").unwrap();
        let t = std::time::SystemTime::now() - std::time::Duration::from_secs(age_secs);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(t)
            .unwrap();
        path
    };
    let stale = plant(".dg-tmp.killed0", 7200);
    let stale_old_name = plant(".tmpAb12Cd", 7200);
    let fresh = plant(".dg-tmp.fresh0", 0);
    let kept = plant("photo.jpg", 7200);

    server.respawn_with_env(&[]).await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while stale.exists() || stale_old_name.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "a stale temp file is still there after the restart: {} {}",
            stale.exists(),
            stale_old_name.exists()
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(fresh.exists(), "a fresh temp file must be kept");
    assert!(kept.exists(), "an old file with a normal name must be kept");

    let listing = common::list_objects_raw(&server.http(), &ep, &bucket, "").await;
    assert!(
        !listing.contains(".dg-tmp.") && !listing.contains(".tmpAb12Cd"),
        "LIST shows a temp file: {listing}"
    );
}
