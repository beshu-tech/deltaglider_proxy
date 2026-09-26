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

use crate::common;

use common::{minio_available, minio_client, MINIO_BUCKET};
use deltaglider_proxy::coordination::reference_lock::lock_object_key;
use deltaglider_proxy::coordination::{ReferenceLock, S3ReferenceLock};
use std::time::Duration;

/// Lock TTL for these tests. Expiry is judged by the SERVER clock (the lock
/// object's Last-Modified against the GET's Date, 1 s resolution), so time
/// cannot be simulated through `now`: the tests wait real time instead.
const TTL: i64 = 2;
/// Long enough that the server age is over `TTL` whatever the rounding.
const PAST_TTL: Duration = Duration::from_millis(3_500);

/// A fresh lock object key for an isolated (bucket, deltaspace) each test.
fn unique_key() -> String {
    lock_object_key("race-bucket", &format!("prefix/{}", uuid::Uuid::new_v4()))
}

/// Build an `S3ReferenceLock` over MinIO with a given durable node id and the
/// short test TTL. The coordination bucket is the shared MinIO test bucket.
async fn lock_for(node_id: &str) -> S3ReferenceLock {
    S3ReferenceLock::new(
        minio_client().await,
        MINIO_BUCKET.to_string(),
        node_id.to_string(),
    )
    .with_tunables(TTL, 30)
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// Best-effort teardown: delete the lock object.
async fn cleanup(key: &str) {
    let _ = minio_client()
        .await
        .delete_object()
        .bucket(MINIO_BUCKET)
        .key(key)
        .send()
        .await;
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

    // A acquires the free deltaspace lock (create-if-absent).
    assert!(
        node_a.try_acquire(&key, "ref-a1", now()).await.unwrap(),
        "A should acquire the free deltaspace lock"
    );

    // While A holds it, B is EXCLUDED — this is the guarantee that stops a
    // second node from creating a rival reference.bin.
    assert!(
        !node_b.try_acquire(&key, "ref-b1", now()).await.unwrap(),
        "B must be blocked while A holds the deltaspace lock"
    );

    // A finishes its reference RMW and releases.
    node_a.release(&key, "ref-a1").await.unwrap();

    // Now B can acquire the freed lock.
    assert!(
        node_b.try_acquire(&key, "ref-b1", now()).await.unwrap(),
        "after A releases, B acquires the deltaspace lock"
    );

    node_b.release(&key, "ref-b1").await.unwrap();
    cleanup(&key).await;
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
            lock.try_acquire(&k, &format!("ref{i}"), now())
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

    cleanup(&key).await;
}

#[tokio::test]
async fn reference_lock_steals_after_ttl_expiry() {
    if !minio_available().await {
        eprintln!("Skipping reference_lock_steals_after_ttl_expiry: MinIO not available");
        return;
    }
    // Crash backstop: a holder that dies mid-critical-section never releases, so
    // its lock must become stealable once the TTL lapses on the server clock —
    // otherwise a crashed node would wedge the deltaspace forever.
    let key = unique_key();
    let dead = lock_for("dead-node").await;
    let peer = lock_for("peer-node").await;

    assert!(
        dead.try_acquire(&key, "ref-dead", now()).await.unwrap(),
        "the (soon-dead) holder acquires"
    );
    assert!(
        !peer.try_acquire(&key, "ref-peer", now()).await.unwrap(),
        "peer blocked while the lock is still live"
    );
    tokio::time::sleep(PAST_TTL).await;
    assert!(
        peer.try_acquire(&key, "ref-peer", now()).await.unwrap(),
        "peer steals the lapsed lock after the TTL crash-backstop expires"
    );

    peer.release(&key, "ref-peer").await.unwrap();
    cleanup(&key).await;
}

/// The peer's clock does not matter: a peer that believes it is far in the
/// future (the old "steal with a far-future clock") must still be blocked by
/// a lock that the server says was written a moment ago.
#[tokio::test]
async fn reference_lock_expiry_ignores_the_peer_clock() {
    if !minio_available().await {
        eprintln!("Skipping reference_lock_expiry_ignores_the_peer_clock: MinIO not available");
        return;
    }
    let key = unique_key();
    let a = lock_for("nodeA").await;
    let b = lock_for("nodeB").await;
    assert!(a.try_acquire(&key, "ref-a", now()).await.unwrap());
    assert!(
        !b.try_acquire(&key, "ref-b", now() + 1_000_000)
            .await
            .unwrap(),
        "a peer clock far ahead must not steal a live lock"
    );
    a.release(&key, "ref-a").await.unwrap();
    cleanup(&key).await;
}

#[tokio::test]
async fn reference_lock_live_lock_blocks_the_same_node_id() {
    if !minio_available().await {
        eprintln!("Skipping reference_lock_live_lock_blocks_the_same_node_id: MinIO not available");
        return;
    }
    // Two live replicas can share a HOSTNAME-derived node id, and an engine
    // rebuild runs two engines in one process. A live lock therefore blocks
    // even the "same" node; only expiry frees it.
    let key = unique_key();
    let before = lock_for("nodeC").await;
    assert!(
        before.try_acquire(&key, "ref-c-old", now()).await.unwrap(),
        "node C acquires (still live)"
    );
    let twin = lock_for("nodeC").await; // same node id, fresh owner token
    assert!(
        !twin.try_acquire(&key, "ref-c-new", now()).await.unwrap(),
        "a live lock blocks the same node id"
    );
    tokio::time::sleep(PAST_TTL).await;
    assert!(
        twin.try_acquire(&key, "ref-c-new", now()).await.unwrap(),
        "after the TTL the lock frees"
    );

    twin.release(&key, "ref-c-new").await.unwrap();
    cleanup(&key).await;
}

#[tokio::test]
async fn reference_lock_renew_extends_and_detects_a_steal() {
    if !minio_available().await {
        eprintln!("Skipping reference_lock_renew_extends_and_detects_a_steal: MinIO not available");
        return;
    }
    let key = unique_key();
    let a = lock_for("nodeA").await;
    let b = lock_for("nodeB").await;
    assert!(a.try_acquire(&key, "ref-a", now()).await.unwrap());
    // A renew inside the TTL moves Last-Modified on: B is still blocked.
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert!(a.renew(&key, "ref-a", now()).await.unwrap());
    assert!(!b.try_acquire(&key, "ref-b", now()).await.unwrap());
    // B steals after the renewed lock lapses; A's next renew reports the loss.
    tokio::time::sleep(PAST_TTL).await;
    assert!(b.try_acquire(&key, "ref-b", now()).await.unwrap());
    assert!(!a.renew(&key, "ref-a", now()).await.unwrap());

    b.release(&key, "ref-b").await.unwrap();
    cleanup(&key).await;
}

#[tokio::test]
async fn reference_lock_replaces_a_corrupt_body() {
    if !minio_available().await {
        eprintln!("Skipping reference_lock_replaces_a_corrupt_body: MinIO not available");
        return;
    }
    // A body that does not parse used to read as "absent": the create-if-absent
    // then 412'd on the existing key forever, wedging the deltaspace.
    let key = unique_key();
    minio_client()
        .await
        .put_object()
        .bucket(MINIO_BUCKET)
        .key(&key)
        .body(aws_sdk_s3::primitives::ByteStream::from_static(b"not json"))
        .send()
        .await
        .unwrap();
    let a = lock_for("nodeA").await;
    assert!(
        a.try_acquire(&key, "ref-a", now()).await.unwrap(),
        "a corrupt lock object must be replaced"
    );
    a.release(&key, "ref-a").await.unwrap();
    cleanup(&key).await;
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

    assert!(a.try_acquire(&key, "ref-a", now()).await.unwrap());
    // B steals after expiry (A "crashed").
    tokio::time::sleep(PAST_TTL).await;
    assert!(b.try_acquire(&key, "ref-b", now()).await.unwrap());
    // A's late release (wrong owner) must be a no-op — B still holds it.
    a.release(&key, "ref-a").await.unwrap();
    assert!(
        !a.try_acquire(&key, "ref-a2", now()).await.unwrap(),
        "B's live lock must survive A's stale owner-mismatched release"
    );

    b.release(&key, "ref-b").await.unwrap();
    cleanup(&key).await;
}

/// Fencing (the reference write is conditional on what the lock saw), against
/// a real CAS backend: a fence taken before a peer's write refuses to
/// overwrite that write with a retryable error, and a current fence passes.
#[tokio::test]
async fn s3_reference_writes_are_fenced() {
    use deltaglider_proxy::storage::{RefFence, RefWrite, S3Backend, StorageBackend, StorageError};
    use deltaglider_proxy::types::FileMetadata;
    if !minio_available().await {
        eprintln!("Skipping s3_reference_writes_are_fenced: MinIO not available");
        return;
    }
    let cfg: deltaglider_proxy::config::BackendConfig = serde_yaml::from_str(&format!(
        "type: s3\nendpoint: \"{}\"\nregion: us-east-1\nforce_path_style: true\n\
         access_key_id: {}\nsecret_access_key: {}\nallow_local: true\n",
        common::minio_endpoint_url(),
        common::MINIO_ACCESS_KEY,
        common::MINIO_SECRET_KEY
    ))
    .unwrap();
    let s3 = S3Backend::new(
        &cfg,
        deltaglider_proxy::storage::NativeEncryptionConfig::None,
    )
    .await
    .unwrap();
    let ds = format!("fence/{}", uuid::Uuid::new_v4());
    let meta = FileMetadata::new_reference(
        "reference.bin".into(),
        "a.zip".into(),
        "0".repeat(64),
        "0".repeat(32),
        1,
        None,
    );
    let put = |data: &'static [u8]| RefWrite::Put {
        data,
        metadata: &meta,
    };

    let seen = s3.reference_fence(MINIO_BUCKET, &ds).await.unwrap();
    assert_eq!(seen, RefFence::Absent);
    // A peer creates the baseline after our observation.
    let peer = s3
        .write_reference_fenced(MINIO_BUCKET, &ds, put(b"P"), &RefFence::Absent)
        .await
        .unwrap();
    assert!(matches!(peer, RefFence::ETag(ref e) if !e.is_empty()));
    // Our create (fence: absent) must not overwrite it.
    let err = s3
        .write_reference_fenced(MINIO_BUCKET, &ds, put(b"A"), &seen)
        .await
        .unwrap_err();
    assert!(matches!(err, StorageError::Throttled(_)), "{err:?}");
    assert_eq!(s3.get_reference(MINIO_BUCKET, &ds).await.unwrap(), b"P");
    // A stale ETag fails the same way, for a metadata update and a delete.
    let stale = RefFence::ETag("\"00000000000000000000000000000000\"".into());
    let meta_op = RefWrite::Metadata { metadata: &meta };
    assert!(matches!(
        s3.write_reference_fenced(MINIO_BUCKET, &ds, meta_op, &stale)
            .await,
        Err(StorageError::Throttled(_))
    ));
    assert!(matches!(
        s3.write_reference_fenced(MINIO_BUCKET, &ds, RefWrite::Delete, &stale)
            .await,
        Err(StorageError::Throttled(_))
    ));
    // The current fence passes, and the returned fence is the next one.
    let next = s3
        .write_reference_fenced(MINIO_BUCKET, &ds, meta_op, &peer)
        .await
        .unwrap();
    let gone = s3
        .write_reference_fenced(MINIO_BUCKET, &ds, RefWrite::Delete, &next)
        .await
        .unwrap();
    assert_eq!(gone, RefFence::Absent);
    assert!(!s3.has_reference(MINIO_BUCKET, &ds).await.unwrap());
}
