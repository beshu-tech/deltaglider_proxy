// SPDX-License-Identifier: BUSL-1.1

//! Cross-instance reference-lock integration tests against a REAL CAS-enforcing
//! backend (MinIO). The pure `plan_lock_acquire` kernel and the `acquire_blocking`
//! loop are unit-tested with mocks in `src/coordination/reference_lock.rs`; this
//! file proves the actual S3 I/O — the `If-None-Match:*` create, `If-Match` steal,
//! and delete-if-owner release — enforces MUTUAL EXCLUSION between two instances
//! racing the same deltaspace. That is exactly the race that corrupts
//! `reference.bin`: two nodes both seeing "no reference" and each writing a
//! baseline. If the S3 CAS lock excludes them here, the engine (which holds this
//! lock around its reference read-modify-write) cannot double-baseline.
//!
//! Requires MinIO (the CI `deltaglider-test` bucket). Each test targets a unique
//! deltaspace (UUID) so parallel crates sharing the bucket never collide on a
//! lock object under `_dgp/locks/reference/`.

mod common;

use common::{minio_available, minio_client, MINIO_BUCKET};
use deltaglider_proxy::coordination::reference_lock::lock_object_key;
use deltaglider_proxy::coordination::{ReferenceLock, S3ReferenceLock};

/// A fresh lock object key for an isolated (bucket, deltaspace) each test.
fn unique_key() -> String {
    lock_object_key("race-bucket", &format!("prefix/{}", uuid::Uuid::new_v4()))
}

/// Build an `S3ReferenceLock` over MinIO with a given durable node id. The
/// coordination bucket is the shared MinIO test bucket.
async fn lock_for(node_id: &str) -> S3ReferenceLock {
    S3ReferenceLock::new(
        minio_client().await,
        MINIO_BUCKET.to_string(),
        node_id.to_string(),
    )
}

/// Best-effort teardown: steal with a far-future clock (any live lock is expired
/// relative to it), then release, so a test never leaves a lock object behind.
async fn cleanup(lock: &S3ReferenceLock, key: &str) {
    let _ = lock.try_acquire(key, "cleanup", 9_999_999_999).await;
    let _ = lock.release(key, "cleanup").await;
}

#[tokio::test]
async fn reference_lock_mutual_exclusion_lifecycle() {
    if !minio_available().await {
        eprintln!("Skipping reference_lock_mutual_exclusion_lifecycle: MinIO not available");
        return;
    }
    let key = unique_key();
    let node_a = lock_for("nodeA").await;
    let node_b = lock_for("nodeB").await;

    // A acquires the free deltaspace lock (create-if-absent). TTL default (120).
    assert!(
        node_a.try_acquire(&key, "ref-a1", 1000).await.unwrap(),
        "A should acquire the free deltaspace lock"
    );

    // While A holds it (now 1050 < expires 1120), B is EXCLUDED — this is the
    // guarantee that stops a second node from creating a rival reference.bin.
    assert!(
        !node_b.try_acquire(&key, "ref-b1", 1050).await.unwrap(),
        "B must be blocked while A holds the deltaspace lock"
    );

    // A finishes its reference RMW and releases.
    node_a.release(&key, "ref-a1").await.unwrap();

    // Now B can acquire the freed lock.
    assert!(
        node_b.try_acquire(&key, "ref-b1", 1060).await.unwrap(),
        "after A releases, B acquires the deltaspace lock"
    );

    node_b.release(&key, "ref-b1").await.unwrap();
    cleanup(&node_a, &key).await;
}

#[tokio::test]
async fn reference_lock_concurrent_acquire_exactly_one_wins() {
    if !minio_available().await {
        eprintln!(
            "Skipping reference_lock_concurrent_acquire_exactly_one_wins: MinIO not available"
        );
        return;
    }
    // THE corruption race: N instances concurrently attempt to create the SAME
    // deltaspace's baseline. Exactly one must win the `If-None-Match:*` CAS; the
    // rest are blocked. That is what prevents two reference.bin baselines.
    let key = unique_key();
    let mut handles = Vec::new();
    for i in 0..8 {
        let k = key.clone();
        handles.push(tokio::spawn(async move {
            let lock = lock_for(&format!("node{i}")).await;
            lock.try_acquire(&k, &format!("ref{i}"), 1000)
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
        "exactly one concurrent acquirer must win the deltaspace lock, got {wins}"
    );

    let cleaner = lock_for("cleanup").await;
    cleanup(&cleaner, &key).await;
}

#[tokio::test]
async fn reference_lock_steals_after_ttl_expiry() {
    if !minio_available().await {
        eprintln!("Skipping reference_lock_steals_after_ttl_expiry: MinIO not available");
        return;
    }
    // Crash backstop: a holder that dies mid-critical-section never releases, so
    // its lock must become stealable once the TTL lapses — otherwise a crashed
    // node would wedge the deltaspace forever. Default TTL 120: acquire at 1000
    // (expires 1120), a peer at 1050 is blocked, a peer past 1120 steals.
    let key = unique_key();
    let dead = lock_for("dead-node").await;
    let peer = lock_for("peer-node").await;

    assert!(
        dead.try_acquire(&key, "ref-dead", 1000).await.unwrap(),
        "the (soon-dead) holder acquires"
    );
    assert!(
        !peer.try_acquire(&key, "ref-peer", 1050).await.unwrap(),
        "peer blocked while the lock is still live"
    );
    assert!(
        peer.try_acquire(&key, "ref-peer", 1200).await.unwrap(),
        "peer steals the lapsed lock after the TTL crash-backstop expires"
    );

    peer.release(&key, "ref-peer").await.unwrap();
    cleanup(&dead, &key).await;
}

#[tokio::test]
async fn reference_lock_self_reclaims_same_node() {
    if !minio_available().await {
        eprintln!("Skipping reference_lock_self_reclaims_same_node: MinIO not available");
        return;
    }
    // A same-node re-acquire (re-entrancy, or a crash-restart with the same
    // durable node id) must reclaim its own still-live lock immediately rather
    // than deadlock on itself; a DIFFERENT node stays blocked.
    let key = unique_key();
    let before = lock_for("nodeC").await;
    assert!(
        before.try_acquire(&key, "ref-c-old", 1000).await.unwrap(),
        "node C acquires (still live)"
    );
    let after = lock_for("nodeC").await; // same node id, fresh owner token
    assert!(
        after.try_acquire(&key, "ref-c-new", 1050).await.unwrap(),
        "same node reclaims its own live lock"
    );
    let other = lock_for("nodeD").await;
    assert!(
        !other.try_acquire(&key, "ref-d", 1060).await.unwrap(),
        "a different node is still blocked by the live lock"
    );

    after.release(&key, "ref-c-new").await.unwrap();
    cleanup(&other, &key).await;
}

#[tokio::test]
async fn reference_lock_release_is_owner_scoped() {
    if !minio_available().await {
        eprintln!("Skipping reference_lock_release_is_owner_scoped: MinIO not available");
        return;
    }
    // A stale release from a previous owner must NOT delete a lock a peer now
    // holds — otherwise a late best-effort release (spawned on guard drop) could
    // free a lock the new holder is relying on, re-opening the race.
    let key = unique_key();
    let a = lock_for("nodeA").await;
    let b = lock_for("nodeB").await;

    assert!(a.try_acquire(&key, "ref-a", 1000).await.unwrap());
    // B steals after expiry (A "crashed").
    assert!(b.try_acquire(&key, "ref-b", 1200).await.unwrap());
    // A's late release (wrong owner) must be a no-op — B still holds it.
    a.release(&key, "ref-a").await.unwrap();
    assert!(
        !a.try_acquire(&key, "ref-a2", 1250).await.unwrap(),
        "B's live lock must survive A's stale owner-mismatched release"
    );

    b.release(&key, "ref-b").await.unwrap();
    cleanup(&a, &key).await;
}
