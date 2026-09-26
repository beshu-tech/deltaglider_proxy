// SPDX-License-Identifier: BUSL-1.1

//! S3-object leader-lease integration tests against a REAL CAS-enforcing backend
//! (MinIO). The pure acquire/renew/steal DECISION kernels are unit-tested in
//! `src/coordination/s3_lease.rs`; this file proves the actual S3 I/O — the
//! `If-None-Match:*` create, `If-Match` steal/renew, and 412 handling — behaves
//! correctly end-to-end, which is the half a fake store can't validate.
//!
//! Requires MinIO (the CI `deltaglider-test` bucket). Each test uses a unique
//! rule name (UUID) so parallel crates sharing the bucket never collide on a
//! lease object under `_dgp/leases/`.

use crate::common;

use common::{minio_available, minio_client, MINIO_BUCKET};
use deltaglider_proxy::coordination::{CoordinationLease, LeaseSubsystem, S3Lease};

fn unique_rule() -> String {
    format!("itest-{}", uuid::Uuid::new_v4())
}

const SUB: LeaseSubsystem = LeaseSubsystem::Replication;

/// Lease TTL for the expiry steps. Expiry is judged by the SERVER clock (the
/// lease object's Last-Modified against the GET's Date, 1 s resolution), so
/// the tests wait real time instead of passing a simulated `now`.
const TTL: i64 = 2;
const PAST_TTL: std::time::Duration = std::time::Duration::from_millis(3_500);

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// Build an S3Lease over MinIO with a given durable node id.
async fn lease_for(node_id: &str) -> S3Lease {
    S3Lease::new(
        minio_client().await,
        MINIO_BUCKET.to_string(),
        node_id.to_string(),
    )
}

async fn delete_lease(rule: &str) {
    let key = format!("_dgp/leases/{}/{}.json", SUB.slug(), rule);
    let _ = minio_client()
        .await
        .delete_object()
        .bucket(MINIO_BUCKET)
        .key(&key)
        .send()
        .await;
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

    // 1. Node A acquires a free lease (create-if-absent).
    assert!(
        node_a
            .try_acquire(SUB, &rule, "task-a1", now(), TTL)
            .await
            .unwrap(),
        "A should acquire a free lease"
    );

    // 2. E2/E3: Node B cannot steal a LIVE lease — not even with a clock far
    //    ahead: the server clock judges expiry.
    assert!(
        !node_b
            .try_acquire(SUB, &rule, "task-b1", now() + 1_000_000, TTL)
            .await
            .unwrap(),
        "B must be blocked while A's lease is live"
    );

    // 3. A renews while live.
    tokio::time::sleep(std::time::Duration::from_millis(1_500)).await;
    assert!(
        node_a
            .renew(SUB, &rule, "task-a1", now(), TTL)
            .await
            .unwrap(),
        "A should renew its live lease"
    );

    // 4. E1: A "dies" (stops renewing). Once its lease lapses, B steals it →
    //    automatic failover.
    tokio::time::sleep(PAST_TTL).await;
    assert!(
        node_b
            .try_acquire(SUB, &rule, "task-b1", now(), 60)
            .await
            .unwrap(),
        "B should steal the lapsed lease (failover)"
    );

    // 5. The old owner A can no longer renew (it was stolen) — E3b.
    assert!(
        !node_a
            .renew(SUB, &rule, "task-a1", now(), 60)
            .await
            .unwrap(),
        "A must NOT renew a lease B has stolen"
    );

    // 6. B now holds it and renews normally.
    assert!(
        node_b
            .renew(SUB, &rule, "task-b1", now(), 60)
            .await
            .unwrap(),
        "B should renew the lease it stole"
    );

    // 7. B releases; the lease is now free for anyone.
    node_b.release(SUB, &rule, "task-b1").await.unwrap();
    assert!(
        node_a
            .try_acquire(SUB, &rule, "task-a2", now(), 60)
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
            .try_acquire(SUB, &rule, "task-c-old", now(), 300)
            .await
            .unwrap(),
        "node C acquires (long TTL, still live after 'restart')"
    );

    // Same node_id, fresh task owner (a new process) — the lease is still LIVE
    // but ours, so we reclaim it now rather than blocking.
    let after_restart = lease_for("nodeC").await;
    assert!(
        after_restart
            .try_acquire(SUB, &rule, "task-c-new", now(), 300)
            .await
            .unwrap(),
        "same node reclaims its own live lease (E7)"
    );
    // A DIFFERENT node is still blocked by the (now task-c-new-owned) live lease.
    let other = lease_for("nodeD").await;
    assert!(
        !other
            .try_acquire(SUB, &rule, "task-d", now(), 300)
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
                .try_acquire(SUB, &r, &format!("task{i}"), now(), 60)
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

    delete_lease(&rule).await;
}

#[tokio::test]
async fn s3_lease_replaces_a_corrupt_body() {
    if !minio_available().await {
        eprintln!("Skipping s3_lease_replaces_a_corrupt_body: MinIO not available");
        return;
    }
    // An unparsable lease body used to read as "absent": create-if-absent
    // then 412'd on the existing key on every tick, so the rule never ran.
    let rule = unique_rule();
    let key = format!("_dgp/leases/{}/{}.json", SUB.slug(), rule);
    minio_client()
        .await
        .put_object()
        .bucket(MINIO_BUCKET)
        .key(&key)
        .body(aws_sdk_s3::primitives::ByteStream::from_static(
            b"{truncated",
        ))
        .send()
        .await
        .unwrap();
    let node_a = lease_for("nodeA").await;
    assert!(
        node_a
            .try_acquire(SUB, &rule, "task-a1", now(), 60)
            .await
            .unwrap(),
        "a corrupt lease object must be replaced"
    );
    node_a.release(SUB, &rule, "task-a1").await.unwrap();
}
