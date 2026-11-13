// SPDX-License-Identifier: BUSL-1.1

//! The TestServer harness keeps concurrent test processes apart: a port is
//! reserved machine-wide for a server's life (`common::PortLease`).

use crate::common::{self, TestServer};

/// A leased port is locked for every other holder. `flock` locks belong to
/// the open file description, so a second handle in this process sees what
/// a second test process would see.
#[test]
fn a_leased_port_is_refused_to_other_processes() {
    let lease = common::lease_free_port();
    let other = std::fs::OpenOptions::new()
        .write(true)
        .open(common::port_lock_path(lease.port()))
        .unwrap();
    assert!(
        other.try_lock().is_err(),
        "port {} is leased, yet another holder could lock it",
        lease.port()
    );
    let port = lease.port();
    drop(lease);
    assert!(
        other.try_lock().is_ok(),
        "port {port} stays locked after drop"
    );
}

/// Servers started at the same time get distinct ports, and each endpoint
/// is that server's own proxy (it lists only its own bucket).
#[tokio::test]
async fn parallel_test_servers_never_share_a_port() {
    let names: Vec<String> = (0..6)
        .map(|i| common::unique_bucket(&format!("port{i}")))
        .collect();
    let servers = futures::future::join_all(
        names
            .iter()
            .map(|n| TestServer::builder().bucket(n).build()),
    )
    .await;
    let mut ports: Vec<String> = servers.iter().map(|s| s.endpoint()).collect();
    ports.sort();
    ports.dedup();
    assert_eq!(ports.len(), servers.len(), "two servers share a port");
    for (server, name) in servers.iter().zip(&names) {
        let listed = server
            .s3_client()
            .await
            .list_buckets()
            .send()
            .await
            .unwrap();
        let buckets: Vec<&str> = listed.buckets().iter().filter_map(|b| b.name()).collect();
        assert_eq!(
            buckets,
            vec![name.as_str()],
            "{} is not its own proxy",
            server.endpoint()
        );
    }
}

/// Set in the helper process of
/// [`a_test_proxy_ends_with_the_test_process_that_started_it`]: the file that
/// receives the proxy's process id.
#[cfg(target_os = "linux")]
const ORPHAN_PID_FILE_ENV: &str = "HARNESS_ORPHAN_PID_FILE";

/// Is `pid` a live process? A zombie is not: the kernel already ended it and
/// only waits for its new parent to collect the status.
#[cfg(target_os = "linux")]
fn process_is_running(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    // `pid (name) S ...`: the state follows the last `)`.
    let state = stat.rsplit(')').next().unwrap_or("").trim_start();
    !matches!(state.chars().next(), Some('Z' | 'X') | None)
}

/// A test proxy ends with the test process that started it, also when that
/// process is killed with SIGKILL and runs no `Drop`. A proxy that survived
/// its `cargo test` ran for 31 hours and held a build slot of the host.
///
/// The test starts itself again as a helper process. The helper starts a
/// `TestServer`, reports the proxy's process id and waits. The test kills the
/// helper with SIGKILL and waits (with a limit) until the proxy is gone.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_test_proxy_ends_with_the_test_process_that_started_it() {
    // The helper role.
    if let Some(pid_file) = std::env::var_os(ORPHAN_PID_FILE_ENV) {
        let server = TestServer::filesystem().await;
        let pid_file = std::path::PathBuf::from(pid_file);
        let partial = pid_file.with_extension("partial");
        std::fs::write(&partial, server.pid().to_string()).unwrap();
        std::fs::rename(&partial, &pid_file).unwrap();
        // The outer test kills this process; nothing else ends it.
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        }
    }

    // The test role.
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("proxy.pid");
    let mut helper = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "harness_port_test::a_test_proxy_ends_with_the_test_process_that_started_it",
            "--test-threads",
            "1",
        ])
        .env(ORPHAN_PID_FILE_ENV, &pid_file)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let kill = |pid: u32| unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    };

    // The proxy runs while its test process lives.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    let proxy = loop {
        if let Ok(text) = std::fs::read_to_string(&pid_file) {
            break text.trim().parse::<u32>().unwrap();
        }
        if std::time::Instant::now() > deadline || helper.try_wait().unwrap().is_some() {
            let _ = helper.kill();
            panic!("the helper process reported no proxy");
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };
    assert!(
        process_is_running(proxy),
        "the proxy {proxy} is not running"
    );

    // SIGKILL: no `Drop`, no signal handler runs in the helper.
    kill(helper.id());
    helper.wait().unwrap();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while process_is_running(proxy) && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let survived = process_is_running(proxy);
    if survived {
        kill(proxy);
    }
    assert!(
        !survived,
        "the proxy {proxy} kept running after its test process was killed"
    );
}
