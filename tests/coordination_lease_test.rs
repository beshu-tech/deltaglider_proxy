// SPDX-License-Identifier: GPL-3.0-only

//! S3-object leader-lease integration tests against a REAL CAS-enforcing backend
//! (MinIO). The pure acquire/renew/steal DECISION kernels are unit-tested in
//! `src/coordination/s3_lease.rs`; this file proves the actual S3 I/O — the
//! `If-None-Match:*` create, `If-Match` steal/renew, and 412 handling — behaves
//! correctly end-to-end, which is the half a fake store can't validate.
//!
//! Requires MinIO (the CI `deltaglider-test` bucket). Each test uses a unique
//! rule name (UUID) so parallel crates sharing the bucket never collide on a
//! lease object under `_dgp/leases/`.

mod common;

use common::{minio_available, minio_client, MINIO_BUCKET};
use deltaglider_proxy::coordination::{CoordinationLease, LeaseSubsystem, S3Lease};

fn unique_rule() -> String {
    format!("itest-{}", uuid::Uuid::new_v4())
}

const SUB: LeaseSubsystem = LeaseSubsystem::Replication;

/// Build an S3Lease over MinIO with a given durable node id.
async fn lease_for(node_id: &str) -> S3Lease {
    S3Lease::new(
        minio_client().await,
        MINIO_BUCKET.to_string(),
        node_id.to_string(),
    )
}

#[tokio::test]
async fn s3_lease_full_failover_lifecycle() {
    if !minio_available().await {
        eprintln!("Skipping s3_lease_full_failover_lifecycle: MinIO not available");
        return;
    }
    let rule = unique_rule();
    let node_a = lease_for("nodeA").await;
    let node_b = lease_for("nodeB").await;

    // 1. Node A acquires a free lease (create-if-absent). Owner "task-a1", TTL 60.
    assert!(
        node_a
            .try_acquire(SUB, &rule, "task-a1", 1000, 60)
            .await
            .unwrap(),
        "A should acquire a free lease"
    );

    // 2. E2/E3: Node B cannot steal a LIVE lease (expires_at 1060 > now 1030).
    assert!(
        !node_b
            .try_acquire(SUB, &rule, "task-b1", 1030, 60)
            .await
            .unwrap(),
        "B must be blocked while A's lease is live"
    );

    // 3. A renews while live (now 1030, expires 1060 → new 1090).
    assert!(
        node_a.renew(SUB, &rule, "task-a1", 1030, 60).await.unwrap(),
        "A should renew its live lease"
    );

    // 4. E1: A "dies" (stops renewing). Once its lease lapses (now > 1090), B
    //    steals it → automatic failover.
    assert!(
        node_b
            .try_acquire(SUB, &rule, "task-b1", 1091, 60)
            .await
            .unwrap(),
        "B should steal the lapsed lease (failover)"
    );

    // 5. The old owner A can no longer renew (it was stolen) — E3b.
    assert!(
        !node_a.renew(SUB, &rule, "task-a1", 1091, 60).await.unwrap(),
        "A must NOT renew a lease B has stolen"
    );

    // 6. B now holds it and renews normally.
    assert!(
        node_b.renew(SUB, &rule, "task-b1", 1100, 60).await.unwrap(),
        "B should renew the lease it stole"
    );

    // 7. B releases; the lease is now free for anyone.
    node_b.release(SUB, &rule, "task-b1").await.unwrap();
    assert!(
        node_a
            .try_acquire(SUB, &rule, "task-a2", 1200, 60)
            .await
            .unwrap(),
        "after release the lease is free to re-acquire"
    );

    // cleanup
    node_a.release(SUB, &rule, "task-a2").await.unwrap();
}

#[tokio::test]
async fn s3_lease_self_reclaim_after_restart() {
    if !minio_available().await {
        eprintln!("Skipping s3_lease_self_reclaim_after_restart: MinIO not available");
        return;
    }
    // E7: a rebooted NODE (same durable node_id, new task owner) reclaims its own
    // still-live lease immediately instead of waiting a full TTL.
    let rule = unique_rule();
    let before_restart = lease_for("nodeC").await;
    assert!(
        before_restart
            .try_acquire(SUB, &rule, "task-c-old", 1000, 300)
            .await
            .unwrap(),
        "node C acquires (long TTL, still live after 'restart')"
    );

    // Same node_id, fresh task owner (a new process) — the lease is still LIVE
    // (expires 1300) but ours, so we reclaim it now rather than blocking.
    let after_restart = lease_for("nodeC").await;
    assert!(
        after_restart
            .try_acquire(SUB, &rule, "task-c-new", 1050, 300)
            .await
            .unwrap(),
        "same node reclaims its own live lease (E7)"
    );
    // A DIFFERENT node is still blocked by the (now task-c-new-owned) live lease.
    let other = lease_for("nodeD").await;
    assert!(
        !other
            .try_acquire(SUB, &rule, "task-d", 1060, 300)
            .await
            .unwrap(),
        "a different node is still blocked while the lease is live"
    );

    after_restart
        .release(SUB, &rule, "task-c-new")
        .await
        .unwrap();
}

#[tokio::test]
async fn s3_lease_concurrent_acquire_exactly_one_wins() {
    if !minio_available().await {
        eprintln!("Skipping s3_lease_concurrent_acquire_exactly_one_wins: MinIO not available");
        return;
    }
    // E2: N nodes race to acquire a FREE lease concurrently → exactly one wins
    // (the If-None-Match:* CAS), proving the create is atomic under contention.
    let rule = unique_rule();
    let mut handles = Vec::new();
    for i in 0..8 {
        let r = rule.clone();
        handles.push(tokio::spawn(async move {
            let lease = lease_for(&format!("node{i}")).await;
            lease
                .try_acquire(SUB, &r, &format!("task{i}"), 1000, 60)
                .await
                .unwrap_or(false)
        }));
    }
    let mut wins = 0;
    for h in handles {
        if h.await.unwrap() {
            wins += 1;
        }
    }
    assert_eq!(
        wins, 1,
        "exactly one concurrent acquirer must win, got {wins}"
    );

    // cleanup — steal it expired and release.
    let cleanup = lease_for("cleanup").await;
    let _ = cleanup
        .try_acquire(SUB, &rule, "cleanup", 9_999_999_999, 1)
        .await;
    let _ = cleanup.release(SUB, &rule, "cleanup").await;
}
