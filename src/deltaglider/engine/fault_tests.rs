// SPDX-License-Identifier: BUSL-1.1

//! Engine paths that must fail closed on a storage fault, and must not
//! touch a live object: each test arms one fault (or one on-disk defect)
//! and checks what the engine did with it.

use super::*;
use crate::config::Config;
use crate::storage::StorageBackendExt;
use crate::storage::{
    fail_io, DynStorageBackend, Fault, FaultPoint, Faults, FaultyFs, FilesystemBackend,
    StorageBackend,
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
        .put_passthrough(
            "b",
            "p",
            "k.zip",
            bytes::Bytes::copy_from_slice(bytes),
            &meta,
        )
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
        String::from_utf8_lossy(&read.iter().take(32).copied().collect::<Vec<u8>>())
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
            bytes::Bytes::copy_from_slice(&v1),
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

/// Review B11: a read error on reference.bin is not "no reference". Read as
/// absent, the PUT wrote a new baseline over the live one, and the deltas
/// against it stopped decoding. The PUT reads the reference once (its
/// metadata, no separate stat): that read fails here.
#[tokio::test]
async fn a_reference_stat_error_is_not_absent() {
    let (dir, engine, _) = faulty_engine().await;
    let base = noise(5, 200_000);
    for (key, seed) in [("rel/a.zip", 1u8), ("rel/b.zip", 2)] {
        engine
            .store("b", key, &near(&base, seed), None, HashMap::new())
            .await
            .unwrap();
    }
    let sha = engine
        .reference_metadata_raw("b", "rel")
        .await
        .unwrap()
        .file_sha256;
    let path = dir.path().join("b/deltaspaces/rel/reference.bin");
    let fault = fail_io(&path, libc::EIO);
    assert!(engine.storage().has_reference("b", "rel").await.is_err());
    drop(fault);
    // An unreadable metadata attribute fails the one read of the PUT.
    let name = "user.dg.metadata";
    let saved = xattr::get(&path, name).unwrap().unwrap();
    xattr::set(&path, name, b"{unreadable").unwrap();
    let put = engine
        .store("b", "rel/c.zip", &noise(6, 200_000), None, HashMap::new())
        .await;
    xattr::set(&path, name, &saved).unwrap();
    assert!(put.is_err(), "the PUT went on past a failed reference read");
    let fresh = DeltaGliderEngine::new_with_backend(
        Arc::new(
            FilesystemBackend::new(dir.path().to_path_buf())
                .await
                .unwrap(),
        ),
        &Config::default(),
        None,
    );
    assert_eq!(
        fresh
            .reference_metadata_raw("b", "rel")
            .await
            .unwrap()
            .file_sha256,
        sha,
        "the reference was replaced"
    );
    assert!(fresh.retrieve("b", "rel/b.zip").await.unwrap().0 == near(&base, 2));
}

/// Review B11: a stat error on the deltaspace directory is not an empty
/// deltaspace. Read as empty, a DELETE reclaimed reference.bin under the
/// remaining deltas.
#[tokio::test]
async fn an_unreadable_deltaspace_dir_keeps_its_reference() {
    let (dir, engine, _) = faulty_engine().await;
    let base = noise(7, 200_000);
    for (key, seed) in [("rel/a.zip", 1u8), ("rel/b.zip", 2)] {
        engine
            .store("b", key, &near(&base, seed), None, HashMap::new())
            .await
            .unwrap();
    }
    let fault = fail_io(&dir.path().join("b/deltaspaces/rel"), libc::EIO);
    engine.delete("b", "rel/a.zip").await.unwrap();
    assert!(fault.fired() > 0);
    drop(fault);
    assert!(engine.storage().has_reference("b", "rel").await.unwrap());
    engine.metadata_cache.invalidate("b", "rel/b.zip");
    assert!(engine.retrieve("b", "rel/b.zip").await.unwrap().0 == near(&base, 2));
}

/// Review B11: a failing remove of the stale sibling is not "already
/// gone". The pre-check read the error as absent, the DELETE answered
/// success, and GET served the stale sibling.
#[tokio::test]
async fn a_sibling_disk_error_does_not_pass_for_deleted() {
    let (dir, engine, _faults, v2) = object_with_a_stale_sibling().await;
    let fault = fail_io(&dir.path().join("b/deltaspaces/p/k.zip"), libc::EIO);
    let del = engine.delete("b", "p/k.zip").await;
    assert!(fault.fired() > 0);
    drop(fault);
    assert!(del.is_err(), "DELETE answered success: {del:?}");
    engine.metadata_cache.invalidate("b", "p/k.zip");
    assert!(engine.retrieve("b", "p/k.zip").await.unwrap().0 == v2);
}

/// P7.4: passthrough writes into one folder run at once; they hold their
/// key's write lock, not the folder's deltaspace lock. Writes of ONE key
/// still go one at a time. Measured as the most backend writes in flight
/// (each write waits 40 ms in the backend), not as wall time.
#[tokio::test]
async fn passthrough_writes_into_one_folder_run_at_once() {
    let (_dir, engine, faults) = faulty_engine().await;
    let engine = Arc::new(engine);
    // The folder exists and has a reference.bin (a delta baseline).
    engine
        .store(
            "b",
            "d/base.zip",
            &noise(1, 64 * 1024),
            None,
            HashMap::new(),
        )
        .await
        .unwrap();
    faults.slow_passthrough_writes(std::time::Duration::from_millis(40));
    let writes = (0..8).map(|i| {
        let e = engine.clone();
        tokio::spawn(async move {
            e.store(
                "b",
                &format!("d/p{i}.jpg"),
                &noise(i, 1024),
                None,
                HashMap::new(),
            )
            .await
            .unwrap();
        })
    });
    futures::future::join_all(writes).await;
    let distinct = faults.max_passthrough_in_flight();
    assert!(distinct > 1, "8 keys of one folder wrote one at a time");

    faults.slow_passthrough_writes(std::time::Duration::from_millis(40));
    let same = (0..4).map(|i| {
        let e = engine.clone();
        tokio::spawn(async move {
            e.store("b", "d/same.jpg", &noise(i, 1024), None, HashMap::new())
                .await
                .unwrap();
        })
    });
    futures::future::join_all(same).await;
    assert_eq!(
        faults.max_passthrough_in_flight(),
        1,
        "two writes of one key ran at once"
    );
    eprintln!("passthrough writes in flight in one folder: {distinct} (one key: 1)");
}

/// A bucket whose only object left is an orphan reference.bin (a delete
/// cut before its reclaim) deletes: a client cannot see or delete that
/// reference, so it must not keep the bucket.
#[tokio::test]
async fn delete_bucket_reclaims_an_orphan_reference() {
    let (dir, engine, _) = faulty_engine().await;
    let base = noise(7, 200_000);
    for (key, seed) in [("rel/a.zip", 1u8), ("rel/b.zip", 2)] {
        engine
            .store("b", key, &near(&base, seed), None, HashMap::new())
            .await
            .unwrap();
    }
    engine.delete_in_sweep("b", "rel/a.zip").await.unwrap();
    engine.delete_in_sweep("b", "rel/b.zip").await.unwrap();
    assert!(
        engine.storage().has_reference("b", "rel").await.unwrap(),
        "fixture: the folder keeps its reference.bin alone"
    );
    engine.delete_bucket("b").await.unwrap();
    assert!(
        !dir.path().join("b").exists(),
        "the bucket is still on disk"
    );
}

/// A DeleteBucket that meets a PUT in flight (the PUT holds the deltaspace
/// lock, encoded against reference.bin, and has not yet written its delta)
/// must not delete that reference: it answers BucketNotEmpty after the
/// PUT, and the new object reads back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_bucket_keeps_the_reference_of_a_put_in_flight() {
    let (_dir, engine, faults) = faulty_engine().await;
    let engine = Arc::new(engine);
    let base = noise(8, 200_000);
    engine
        .store("b", "rel/a.zip", &near(&base, 1), None, HashMap::new())
        .await
        .unwrap();
    engine.delete_in_sweep("b", "rel/a.zip").await.unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let tx = Arc::new(std::sync::Mutex::new(Some(tx)));
    let e = engine.clone();
    faults.hook(
        FaultPoint::PutDelta,
        "b/rel/b.zip",
        Arc::new(move || {
            let (e, tx) = (e.clone(), tx.clone());
            Box::pin(async move {
                let task = tokio::spawn(async move { e.delete_bucket("b").await });
                // Room for a DeleteBucket that does not wait for the lock
                // to finish before the PUT writes its delta.
                tokio::time::sleep(Duration::from_millis(300)).await;
                let _ = tx.lock().unwrap().take().unwrap().send(task);
            })
        }),
    );
    let v2 = near(&base, 2);
    let put = engine
        .store("b", "rel/b.zip", &v2, None, HashMap::new())
        .await;
    assert_eq!(faults.fired(FaultPoint::PutDelta, "b/rel/b.zip"), 1);
    let deleted = rx.await.unwrap().await.unwrap();
    assert!(
        matches!(
            deleted,
            Err(EngineError::Storage(StorageError::BucketNotEmpty(_)))
        ),
        "DeleteBucket during a PUT: {deleted:?}"
    );
    put.unwrap();
    engine.metadata_cache.invalidate("b", "rel/b.zip");
    assert_eq!(engine.retrieve("b", "rel/b.zip").await.unwrap().0, v2);
}
