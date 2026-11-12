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
    crate::common::expect_server_errors(
        "CreateBucket answers 503 while the coordination bucket is unreachable",
    );
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
    // The endpoint is the data backend too, so the node is not ready
    // because its BACKEND is unreachable (`backend`), not because of the
    // coordination bucket: `coordination` is information. The node has no
    // IAM users, so the unconfirmed IAM database refuses nobody.
    let ready = reqwest::get(format!("{}/_/ready", server.endpoint()))
        .await
        .expect("ready request");
    assert_eq!(ready.status().as_u16(), 503);
    let body: serde_json::Value = ready.json().await.expect("ready body");
    assert_eq!(body["backend"], "unreachable", "{body}");
    assert_eq!(body["coordination"], "unproven", "{body}");
    assert_eq!(body["iam_sync"], "unconfirmed", "{body}");
}

/// A node that restarts while the coordination store is down cannot tell
/// whether its local IAM database is current. It answers `503` to the logins
/// that the database would judge and is not ready; the operator override
/// serves the local copy. No pull ever ended well in the
/// restarted process, so only the rule decides.
#[tokio::test]
async fn a_restart_during_a_sync_outage_refuses_iam_logins_until_the_override() {
    skip_unless_minio!();
    common::expect_server_errors("the coordination store is down during this test");
    let key = format!(".deltaglider/outage-ci-{}.db", uuid::Uuid::new_v4());
    let mut server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .s3_endpoint(&common::minio_endpoint_url())
        .bucket(common::MINIO_BUCKET)
        .config_sync_bucket(common::MINIO_BUCKET)
        .config_sync_object_key(&key)
        .build()
        .await;
    let ep = server.endpoint();
    let admin = common::admin_http_client(&ep).await;
    let user: serde_json::Value = admin
        .post(format!("{ep}/_/api/admin/users"))
        .json(&serde_json::json!({
            "name": "dana",
            "permissions": [{ "actions": ["*"], "resources": ["*"] }]
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let login = serde_json::json!({
        "access_key_id": user["access_key_id"],
        "secret_access_key": user["secret_access_key"],
    });

    // Restart with the store down.
    let down = dropping_endpoint();
    let outage = [
        ("DGP_S3_ENDPOINT", down.as_str()),
        ("DGP_BOOT_BACKEND_PROBE", "warn"),
        ("DGP_BE_AWS_ACCESS_KEY_ID", common::MINIO_ACCESS_KEY),
        ("DGP_BE_AWS_SECRET_ACCESS_KEY", common::MINIO_SECRET_KEY),
    ];
    server.respawn_with_env(&outage).await;
    let ready = reqwest::get(format!("{ep}/_/ready")).await.unwrap();
    assert_eq!(ready.status().as_u16(), 503);
    let body: serde_json::Value = ready.json().await.unwrap();
    // The dropped endpoint is the data backend too: `backend` says so, and
    // `iam_sync` names the refusal of the IAM users beside it. (The unit
    // table `only_a_refusal_of_iam_users_makes_the_node_not_ready` holds the
    // rows where only the IAM refusal makes the node not ready.)
    assert_eq!(body["backend"], "unreachable", "{body}");
    assert_eq!(body["iam_sync"], "unconfirmed", "{body}");
    let fresh = || {
        reqwest::Client::builder()
            .cookie_store(true)
            .no_proxy()
            .build()
            .unwrap()
    };
    let refused = fresh()
        .post(format!("{ep}/_/api/admin/login-as"))
        .json(&login)
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status().as_u16(), 503);
    let refused = fresh()
        .post(format!("{ep}/_/api/admin/session/browser-connect"))
        .json(&serde_json::json!({
            "access_key_id": user["access_key_id"],
            "secret_access_key": user["secret_access_key"],
            "endpoint": ep,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status().as_u16(), 503);

    // The operator accepts the local copy.
    let mut served = outage.to_vec();
    served.push(("DGP_IAM_SERVE_UNCONFIRMED", "true"));
    server.respawn_with_env(&served).await;
    // The node is still not ready, but for its unreachable backend (the
    // dropped endpoint is the data backend too), not for its IAM users.
    let body: serde_json::Value = reqwest::get(format!("{ep}/_/ready"))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["iam_sync"], "unconfirmed_override", "{body}");
    let ok = fresh()
        .post(format!("{ep}/_/api/admin/login-as"))
        .json(&login)
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status().as_u16(), 200);
}

/// A `config_sync_bucket` on a backend that cannot hold one (here: the
/// filesystem) builds no sync client; the error comes from the config, so a
/// retry would repeat it. The sync is off, as when no bucket is set: the
/// local IAM database is the only copy, so IAM users are served and the
/// node is ready.
#[tokio::test]
async fn a_sync_bucket_on_a_filesystem_backend_leaves_the_node_serving_iam_users() {
    let server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .config_sync_bucket("coordination")
        .build()
        .await;
    let ep = server.endpoint();
    let admin = common::admin_http_client(&ep).await;
    let user: serde_json::Value = admin
        .post(format!("{ep}/_/api/admin/users"))
        .json(&serde_json::json!({
            "name": "dana",
            "permissions": [{ "actions": ["*"], "resources": ["*"] }]
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ready = reqwest::get(format!("{ep}/_/ready")).await.unwrap();
    let status = ready.status().as_u16();
    let body: serde_json::Value = ready.json().await.unwrap();
    assert_eq!(status, 200, "{body}");
    let login = reqwest::Client::builder()
        .cookie_store(true)
        .no_proxy()
        .build()
        .unwrap()
        .post(format!("{ep}/_/api/admin/login-as"))
        .json(&serde_json::json!({
            "access_key_id": user["access_key_id"],
            "secret_access_key": user["secret_access_key"],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(login.status().as_u16(), 200);
}

/// A coordination bucket that the backend refuses (here: it does not
/// exist) ends the boot with a message that names the bucket and the
/// setting. Without that, the node would boot, and every delta write would
/// answer 503 for good.
#[tokio::test]
async fn a_missing_coordination_bucket_ends_the_boot() {
    skip_unless_minio!();
    let mut server = TestServer::builder()
        .auth("bootstrap_key", "bootstrap_secret")
        .s3_endpoint(&common::minio_endpoint_url())
        .bucket(common::MINIO_BUCKET)
        .build()
        .await;
    server.kill();
    let bucket = "dgp-no-such-sync-bucket";
    let (status, output) = server
        .spawn_expect_exit(
            &[
                ("DGP_CONFIG_SYNC_BUCKET", bucket),
                ("DGP_CONFIG_DB_KEY", "a-test-key-for-the-sync-bucket"),
                ("DGP_STATE_LOCK_WAIT_SECS", "5"),
            ],
            120,
        )
        .await;
    let status = status.expect("the proxy must exit");
    assert!(!status.success(), "exit status {status:?}\n{output}");
    assert!(
        output.contains(bucket) && output.contains("config_sync_bucket"),
        "the message must name the bucket and the setting:\n{output}"
    );
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

/// A process that waits for the state directory lock stops at once on
/// SIGTERM (as PID 1 in a container it has no default action for it), with
/// a log line and a failing status, not after the whole wait.
#[tokio::test]
async fn a_sigterm_ends_the_wait_for_the_state_lock_at_once() {
    let server = TestServer::filesystem().await;
    let other = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let other_port = other.local_addr().unwrap().port();
    drop(other);
    let mut second = server.spawn_sibling(&[
        ("DGP_LISTEN_ADDR", &format!("127.0.0.1:{other_port}")),
        ("DGP_STATE_LOCK_WAIT_SECS", "60"),
        ("RUST_LOG", "info"),
    ]);
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
    second.sigterm();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let status = loop {
        if let Some(status) = second.exited() {
            break status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the wait ignored SIGTERM:\n{}",
            second.output()
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    assert!(!status.success(), "{status:?}");
    assert!(
        second
            .output()
            .contains("SIGTERM while it waited for the state directory lock"),
        "{}",
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
    // A boot with no provider, on this host and at this load: the time that a
    // boot needs at all. A fixed limit on the boot (8 s) failed on a busy
    // host, where the boot alone takes that long; a boot that waits for
    // discovery takes the 10 s discovery limit more than this one.
    server.terminate();
    let started = std::time::Instant::now();
    server.respawn_with_env(&[]).await;
    let plain_boot = started.elapsed();
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
    // Each client gave up after 500 ms while its save was still running on
    // the server: wait (bounded) until the third provider is saved.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let listed = loop {
        let listed: serde_json::Value = admin
            .get(format!("{ep}/_/api/admin/ext-auth/providers"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if listed.as_array().map(Vec::len) == Some(3) || std::time::Instant::now() > deadline {
            break listed;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    };
    assert_eq!(listed.as_array().map(Vec::len), Some(3), "{listed}");

    server.terminate();
    let started = std::time::Instant::now();
    server.respawn_with_env(&[]).await;
    let boot = started.elapsed();
    assert!(
        boot < plain_boot + std::time::Duration::from_secs(7),
        "the boot waited on identity-provider discovery: {boot:?} with three hanging \
         providers, {plain_boot:?} with none"
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
