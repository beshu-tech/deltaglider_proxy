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
