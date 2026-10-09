// SPDX-License-Identifier: BUSL-1.1

//! Engine paths that must fail closed on a storage fault, and must not
//! touch a live object: each test arms one fault (or one on-disk defect)
//! and checks what the engine did with it.

use super::*;
use crate::config::Config;
use crate::storage::{
    DynStorageBackend, Fault, FaultPoint, Faults, FaultyFs, FilesystemBackend, StorageBackend,
};
use md5::{Digest, Md5};
use sha2::Sha256;
use std::collections::HashMap;

fn noise(seed: u64, n: usize) -> Vec<u8> {
    let mut x = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    (0..n)
        .map(|_| {
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 56) as u8
        })
        .collect()
}

/// `base` with a small edit, so it stores as a delta against `base`.
fn near(base: &[u8], seed: u8) -> Vec<u8> {
    let mut v = base.to_vec();
    for b in &mut v[1000..1100] {
        *b = b.wrapping_add(seed);
    }
    v
}

async fn faulty_engine() -> (tempfile::TempDir, DynEngine, Faults) {
    faulty_engine_with(FaultyFs::new).await
}

async fn faulty_engine_with(
    make: fn(FilesystemBackend) -> FaultyFs,
) -> (tempfile::TempDir, DynEngine, Faults) {
    let dir = tempfile::tempdir().unwrap();
    let faulty = make(
        FilesystemBackend::new(dir.path().to_path_buf())
            .await
            .unwrap(),
    );
    let faults = faulty.faults.clone();
    let backend: Box<DynStorageBackend<'static>> = DynStorageBackend::new_box(faulty);
    let engine = DeltaGliderEngine::new_with_backend(Arc::new(backend), &Config::default(), None);
    engine.create_bucket("b").await.unwrap();
    (dir, engine, faults)
}

/// B013: a delta whose metadata cannot be read is still a delta: reference
/// reclaim must not delete reference.bin under it.
#[tokio::test]
async fn reclaim_keeps_the_reference_of_an_unreadable_delta() {
    let (dir, engine, _) = faulty_engine().await;
    let base = noise(1, 200_000);
    for (key, seed) in [("rel/a.zip", 1u8), ("rel/b.zip", 2), ("rel/c.zip", 3)] {
        engine
            .store("b", key, &near(&base, seed), None, HashMap::new())
            .await
            .unwrap();
    }
    let a = dir.path().join("b/deltaspaces/rel/a.zip.delta");
    assert!(a.exists(), "fixture: a.zip is a delta at {a:?}");
    let saved = xattr::get(&a, crate::storage::xattr_meta::XATTR_NAME).unwrap();
    xattr::set(&a, crate::storage::xattr_meta::XATTR_NAME, b"not json {{{").unwrap();
    engine.delete("b", "rel/b.zip").await.unwrap();
    engine.delete("b", "rel/c.zip").await.unwrap();
    assert!(
        engine.storage().has_reference("b", "rel").await.unwrap(),
        "reference.bin was deleted under an unreadable delta"
    );
    xattr::set(&a, crate::storage::xattr_meta::XATTR_NAME, &saved.unwrap()).unwrap();
    assert_eq!(
        engine.retrieve("b", "rel/a.zip").await.unwrap().0,
        near(&base, 1)
    );
}

/// B060: an I/O error reading an object's metadata is not "absent": HEAD
/// must not answer NotFound, and DELETE must not report success.
#[tokio::test]
async fn an_io_error_on_metadata_is_not_an_absent_object() {
    let (_dir, engine, faults) = faulty_engine().await;
    let base = noise(2, 200_000);
    engine
        .store("b", "v/app-1.zip", &base, None, HashMap::new())
        .await
        .unwrap();
    engine
        .store("b", "v/app-2.zip", &near(&base, 5), None, HashMap::new())
        .await
        .unwrap();
    engine.metadata_cache.invalidate("b", "v/app-2.zip");
    faults.arm(FaultPoint::GetDeltaMetadata, "b/v/app-2.zip", Fault::Io);
    let head = engine.head("b", "v/app-2.zip").await;
    assert!(
        !matches!(head, Err(EngineError::NotFound(_))),
        "HEAD read an I/O error as an absent object: {head:?}"
    );
    let del = engine.delete("b", "v/app-2.zip").await;
    assert!(
        !matches!(del, Ok(_) | Err(EngineError::NotFound(_))),
        "DELETE read an I/O error as an absent object: {del:?}"
    );
    assert!(faults.fired(FaultPoint::GetDeltaMetadata, "b/v/app-2.zip") > 0);
}

/// v2 stored as a delta (the newest variant), plus an older v1 passthrough
/// left at the same name (a PUT whose cleanup failed).
async fn object_with_a_stale_sibling() -> (tempfile::TempDir, DynEngine, Faults, Vec<u8>) {
    let (dir, engine, faults, v2) = delta_object(FaultyFs::new).await;
    put_passthrough_sibling(&engine, b"old-passthrough-bytes", -1).await;
    (dir, engine, faults, v2)
}

/// v2 at `p/k.zip`, stored as a delta.
async fn delta_object(
    make: fn(FilesystemBackend) -> FaultyFs,
) -> (tempfile::TempDir, DynEngine, Faults, Vec<u8>) {
    let (dir, engine, faults) = faulty_engine_with(make).await;
    let v2 = noise(3, 200_000);
    engine
        .store("b", "p/k.zip", &v2, None, HashMap::new())
        .await
        .unwrap();
    let head = engine.head("b", "p/k.zip").await.unwrap();
    assert!(
        matches!(head.storage_info, StorageInfo::Delta { .. }),
        "fixture: v2 is a delta"
    );
    (dir, engine, faults, v2)
}

/// A passthrough variant of `p/k.zip`, `hours` from now (negative: older
/// than the delta), written past the engine like a failed cleanup or a
/// peer instance.
async fn put_passthrough_sibling(engine: &DynEngine, bytes: &[u8], hours: i64) {
    write_passthrough(engine.storage(), bytes, hours).await;
    engine.metadata_cache.invalidate("b", "p/k.zip");
}

async fn write_passthrough<S: StorageBackend>(storage: &S, bytes: &[u8], hours: i64) {
    let mut meta = crate::types::FileMetadata::new_passthrough(
        "k.zip".into(),
        hex::encode(Sha256::digest(bytes)),
        hex::encode(Md5::digest(bytes)),
        bytes.len() as u64,
        None,
    );
    meta.created_at += chrono::Duration::hours(hours);
    storage
        .put_passthrough("b", "p", "k.zip", bytes, &meta)
        .await
        .unwrap();
}

/// A peer instance that writes a newer passthrough `p/k.zip` straight to
/// the data directory: its PUT takes no lock of ours.
fn peer_write(dir: &std::path::Path, bytes: &'static [u8]) -> crate::storage::Hook {
    let dir = dir.to_path_buf();
    Arc::new(move || {
        let dir = dir.clone();
        Box::pin(async move {
            let peer = FilesystemBackend::new(dir).await.unwrap();
            write_passthrough(&peer, bytes, 1).await;
        })
    })
}

/// Review A6, S3 twin: a conditional delete pins the sibling it saw. No
/// sibling at the check, and a peer's PUT of the passthrough kind lands
/// right after it: that is a new object, and the delete must not remove it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_conditional_delete_keeps_a_sibling_written_after_its_check() {
    let (dir, engine, _faults, _v2) = delta_object(FaultyFs::versioned).await;
    let peer = peer_write(dir.path(), b"peer-bytes");
    // The check is the last step before the deletes: the peer writes there.
    let still_ours = move |_: &FileMetadata| {
        tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(peer()));
        true
    };
    let out = engine.delete_if("b", "p/k.zip", &still_ours).await.unwrap();
    assert!(matches!(out, ConditionalDelete::Deleted(_)), "{out:?}");
    engine.metadata_cache.invalidate("b", "p/k.zip");
    let read = engine.retrieve("b", "p/k.zip").await;
    assert!(
        matches!(&read, Ok(r) if r.0 == b"peer-bytes"),
        "the peer's new object is gone: {:?}",
        read.map(|r| r.0.len())
    );
}

/// The sibling the check saw is overwritten by a peer before its delete:
/// `Changed`, and the peer's object stays.
#[tokio::test]
async fn a_conditional_delete_stops_when_its_sibling_changes() {
    let (dir, engine, faults, _v2) = delta_object(FaultyFs::versioned).await;
    put_passthrough_sibling(&engine, b"old-passthrough-bytes", -1).await;
    faults.hook(
        FaultPoint::DeleteVariantIf,
        "b/p/k.zip",
        peer_write(dir.path(), b"peer-bytes"),
    );
    let out = engine.delete_if("b", "p/k.zip", &|_| true).await.unwrap();
    assert_eq!(faults.fired(FaultPoint::DeleteVariantIf, "b/p/k.zip"), 1);
    assert!(matches!(out, ConditionalDelete::Changed), "{out:?}");
    engine.metadata_cache.invalidate("b", "p/k.zip");
    let read = engine.retrieve("b", "p/k.zip").await;
    assert!(
        matches!(&read, Ok(r) if r.0 == b"peer-bytes"),
        "the peer's new object is gone: {:?}",
        read.map(|r| r.0.len())
    );
}

/// B065 + review A6: a DELETE whose sibling cleanup fails reports the
/// failure AND changes nothing visible. It deleted the newest variant
/// first, so the stale sibling came back as the object.
#[tokio::test]
async fn a_failed_sibling_cleanup_fails_the_delete() {
    let (_dir, engine, faults, v2) = object_with_a_stale_sibling().await;
    faults.arm(FaultPoint::DeletePassthrough, "b/p/k.zip", Fault::Throttled);
    let del = engine.delete("b", "p/k.zip").await;
    faults.disarm_all();
    assert_eq!(
        faults.fired(FaultPoint::DeletePassthrough, "b/p/k.zip"),
        1,
        "the sibling delete never met the fault"
    );
    assert!(del.is_err(), "DELETE answered success: {del:?}");
    engine.metadata_cache.invalidate("b", "p/k.zip");
    let read = engine.retrieve("b", "p/k.zip").await.unwrap().0;
    assert!(
        read == v2,
        "the failed DELETE left the stale sibling as the object: {:?}",
        String::from_utf8_lossy(&read[..read.len().min(32)])
    );
    assert!(
        engine
            .storage()
            .get_passthrough_metadata("b", "p", "k.zip")
            .await
            .is_ok(),
        "the sibling is still stored, so a retry deletes it"
    );
}

/// The newest variant's delete fails after the sibling went: the object is
/// still v2, and a retry converges.
#[tokio::test]
async fn a_failed_delete_of_the_newest_variant_keeps_the_object() {
    let (_dir, engine, faults, v2) = object_with_a_stale_sibling().await;
    faults.arm(FaultPoint::DeleteDelta, "b/p/k.zip", Fault::Io);
    let del = engine.delete("b", "p/k.zip").await;
    faults.disarm_all();
    assert_eq!(faults.fired(FaultPoint::DeleteDelta, "b/p/k.zip"), 1);
    assert!(del.is_err(), "DELETE answered success: {del:?}");
    engine.metadata_cache.invalidate("b", "p/k.zip");
    assert!(engine.retrieve("b", "p/k.zip").await.unwrap().0 == v2);
    engine.delete("b", "p/k.zip").await.unwrap();
    engine.metadata_cache.invalidate("b", "p/k.zip");
    assert!(matches!(
        engine.retrieve("b", "p/k.zip").await,
        Err(EngineError::NotFound(_))
    ));
}

/// B012: the legacy-reference migration must not overwrite a live object
/// that a client stored at the reference's original name.
#[tokio::test]
async fn legacy_reference_migration_keeps_a_live_object() {
    let tmp = tempfile::tempdir().unwrap();
    let backend = Arc::new(
        FilesystemBackend::new(tmp.path().to_path_buf())
            .await
            .unwrap(),
    );
    backend.create_bucket("b").await.unwrap();
    let engine = DeltaGliderEngine::new_with_backend(backend.clone(), &Config::default(), None);
    let v1 = noise(4, 200_000);
    let legacy = crate::types::FileMetadata::new_reference(
        "a.zip".into(),
        "a.zip".into(),
        hex::encode(Sha256::digest(&v1)),
        hex::encode(Md5::digest(&v1)),
        v1.len() as u64,
        None,
    );
    backend
        .put_reference(
            "b",
            "rel",
            &v1,
            &legacy,
            crate::deltaglider::RefWriteProof::for_tests(),
        )
        .await
        .unwrap();
    let v2 = near(&v1, 9);
    engine
        .store("b", "rel/a.zip", &v2, None, HashMap::new())
        .await
        .unwrap();
    assert_eq!(engine.retrieve("b", "rel/a.zip").await.unwrap().0, v2);
    engine.migrate_legacy_references("b").await.unwrap();
    engine.metadata_cache.invalidate("b", "rel/a.zip");
    assert_eq!(
        engine.retrieve("b", "rel/a.zip").await.unwrap().0,
        v2,
        "the migration put the legacy bytes back over the live object"
    );
}
