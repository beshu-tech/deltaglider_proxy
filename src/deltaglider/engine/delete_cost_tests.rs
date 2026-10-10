// SPDX-License-Identifier: BUSL-1.1

//! What a delete costs, and what it reclaims. After each delete the engine
//! asks "does the deltaspace hold anything but reference.bin now?". On S3
//! that check HEADed every remaining `.delta` (to read original sizes it
//! does not need), so a folder of N deltas deleted key by key cost about
//! N²/2 HEADs.

use super::*;
use crate::bucket_usage::BucketUsage;
use crate::storage::{DynStorageBackend, FakeS3, FilesystemBackend};

/// An engine on a fake S3 with bucket `b`.
pub(crate) async fn s3_engine() -> (DynEngine, Arc<FakeS3>) {
    let (s3, fake) = crate::storage::fake_s3_backend().await;
    let backend: Box<DynStorageBackend<'static>> = DynStorageBackend::new_box(s3);
    let engine = DeltaGliderEngine::new_with_backend(Arc::new(backend), &Config::default(), None);
    engine.create_bucket("b").await.unwrap();
    (engine, fake)
}

/// Store `n` close versions of one file under `dir/`, each as a delta
/// against the folder's reference.bin. Returns their keys.
pub(crate) async fn store_deltas(engine: &DynEngine, dir: &str, n: usize) -> Vec<String> {
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let base: Vec<u8> = (0..64 * 1024)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect();
    let mut keys = Vec::with_capacity(n);
    for i in 0..n {
        let mut data = base.clone();
        data[1000 + i * 16..1016 + i * 16].fill(i as u8);
        let key = format!("{dir}/build-{i:03}.zip");
        let stored = engine
            .store("b", &key, &data, None, Default::default())
            .await
            .unwrap();
        assert_eq!(stored.metadata.storage_info.label(), "delta", "{key}");
        keys.push(key);
    }
    keys
}

fn count(fake: &FakeS3, method: &str) -> usize {
    fake.requests()
        .iter()
        .filter(|r| r.starts_with(method))
        .count()
}

/// LIST requests for object folders (not the background listing-facts
/// upkeep under `.dg/`).
pub(crate) fn folder_lists(fake: &FakeS3) -> Vec<String> {
    fake.requests()
        .into_iter()
        .filter(|r| r.contains("list-type=2") && !r.contains("prefix=.dg"))
        .collect()
}

/// Store `n` one-byte passthrough objects under `dir/` (`""`: the bucket
/// root), 32 at a time.
pub(crate) async fn fill(engine: &DynEngine, dir: &str, n: usize) {
    use futures::StreamExt;
    futures::stream::iter(0..n)
        .for_each_concurrent(32, |i| async move {
            let key = match dir {
                "" => format!("img-{i:05}.jpg"),
                dir => format!("{dir}/img-{i:05}.jpg"),
            };
            engine
                .store("b", &key, b"x", None, Default::default())
                .await
                .unwrap();
        })
        .await;
}

/// The reclaim check after a delete asks one question: does the folder
/// still hold an object besides reference.bin? It lists the folder's own
/// level (`delimiter=/`) and stops at the first object. It listed the
/// whole subtree, every page of it, and for a root key the whole bucket:
/// 3 pages for a key next to 2,500 objects in a sub-folder.
#[tokio::test]
async fn the_reclaim_check_reads_one_page_of_its_own_folder() {
    let (engine, fake) = s3_engine().await;
    let keys = store_deltas(&engine, "run", 3).await;
    fill(&engine, "run/sub", 2_500).await;
    fill(&engine, "big", 2_500).await;
    fill(&engine, "", 2).await;

    for (key, folder) in [
        (keys[0].as_str(), "run/"),
        ("big/img-00000.jpg", "big/"),
        ("img-00000.jpg", ""),
    ] {
        fake.clear();
        engine.delete("b", key).await.unwrap();
        let lists = folder_lists(&fake);
        assert_eq!(lists.len(), 1, "{key}: {lists:#?}");
        assert!(
            lists[0].contains("delimiter=%2F")
                && crate::storage::fake_s3::list_prefix(&lists[0]) == folder,
            "{key}: the check lists one level of its own folder: {}",
            lists[0]
        );
    }
}

/// A folder whose own level holds only sub-folders and reference.bin has
/// no object of its own: its reference.bin goes, even when the sub-folders
/// fill more than one page of the check's listing.
#[tokio::test]
async fn sub_folders_alone_do_not_keep_a_reference() {
    let (engine, _fake) = s3_engine().await;
    let keys = store_deltas(&engine, "run", 2).await;
    for i in 0..120 {
        fill(&engine, &format!("run/sub-{i:03}"), 1).await;
    }
    for key in &keys {
        engine.delete("b", key).await.unwrap();
    }
    assert!(!engine.storage().has_reference("b", "run").await.unwrap());
}

/// The reclaim takes out of stored_bytes what the store of reference.bin
/// added: its plaintext size. On an encrypted S3 backend it took the
/// size from the LIST, which is the ciphertext size (28 bytes more).
#[tokio::test]
async fn reclaiming_an_encrypted_reference_takes_out_the_bytes_its_store_added() {
    let (s3, _fake) = crate::storage::fake_s3_backend().await;
    let cfg = Arc::new(arc_swap::ArcSwap::new(Arc::new(
        crate::storage::EncryptionConfig {
            key: Some(crate::storage::EncryptionKey::from_hex(&"ab".repeat(32)).unwrap()),
            key_id: Some("kid".into()),
            ..Default::default()
        },
    )));
    let backend: Box<DynStorageBackend<'static>> =
        DynStorageBackend::new_box(crate::storage::EncryptingBackend::new(s3, cfg));
    let usage = Arc::new(BucketUsage::in_memory().unwrap());
    let engine = DeltaGliderEngine::new_with_backend(Arc::new(backend), &Config::default(), None)
        .with_bucket_usage(Some(usage.clone()));
    engine.create_bucket("b").await.unwrap();
    // An object that stays, so the counter cannot clamp at zero.
    engine
        .store("b", "keep/notes.txt", b"kept", None, Default::default())
        .await
        .unwrap();
    usage.flush_pending();
    let before = usage.read("b").unwrap().unwrap();

    for key in store_deltas(&engine, "run", 2).await {
        engine.delete("b", &key).await.unwrap();
    }

    assert!(!engine.storage().has_reference("b", "run").await.unwrap());
    usage.flush_pending();
    let after = usage.read("b").unwrap().unwrap();
    assert_eq!(
        (after.object_count, after.logical_bytes, after.stored_bytes),
        (
            before.object_count,
            before.logical_bytes,
            before.stored_bytes
        )
    );
}

/// The emptiness check after each delete reads the listing only. Before,
/// 20 deletes sent about 230 HEADs (190 of them from the check).
#[tokio::test]
async fn deleting_a_folder_key_by_key_heads_each_object_a_bounded_number_of_times() {
    const N: usize = 20;
    let (engine, fake) = s3_engine().await;
    let usage = Arc::new(BucketUsage::in_memory().unwrap());
    let engine = engine.with_bucket_usage(Some(usage.clone()));
    let keys = store_deltas(&engine, "reports/run-1", N).await;
    usage.flush_pending();
    let stored = usage.read("b").unwrap().unwrap();
    assert_eq!(stored.object_count, N as u64);
    fake.clear();

    for key in &keys {
        engine.delete("b", key).await.unwrap();
    }

    let heads = count(&fake, "HEAD ");
    eprintln!("{N} deletes: {heads} HEAD requests");
    assert!(
        heads <= 3 * N,
        "{N} deletes sent {heads} HEADs: the emptiness check HEADs the remaining objects"
    );
    assert!(
        !engine
            .storage()
            .has_reference("b", "reports/run-1")
            .await
            .unwrap(),
        "the last delete reclaims reference.bin"
    );
    // The reclaimed reference takes out of stored_bytes the bytes it added.
    usage.flush_pending();
    let row = usage.read("b").unwrap().unwrap();
    assert_eq!(
        (row.object_count, row.logical_bytes, row.stored_bytes),
        (0, 0, 0)
    );
}

/// A delta's baseline is the reference.bin of its own folder, never of a
/// parent. So a subfolder that still holds objects does not keep the
/// parent's reference.bin alive. On S3 the emptiness check listed the
/// whole subtree, and the parent's reference.bin stayed for ever.
#[tokio::test]
async fn a_subfolder_with_objects_does_not_keep_the_parent_reference() {
    let tmp = tempfile::tempdir().unwrap();
    let fs = FilesystemBackend::new(tmp.path().to_path_buf())
        .await
        .unwrap();
    let fs_engine = DeltaGliderEngine::new_with_backend(
        Arc::new(DynStorageBackend::new_box(fs)),
        &Config::default(),
        None,
    );
    fs_engine.create_bucket("b").await.unwrap();
    let (s3, _fake) = s3_engine().await;
    for (backend, engine) in [("filesystem", fs_engine), ("s3", s3)] {
        let parent = store_deltas(&engine, "run", 3).await;
        store_deltas(&engine, "run/trace", 3).await;
        for key in &parent {
            engine.delete("b", key).await.unwrap();
        }
        let s = engine.storage();
        assert!(
            !s.has_reference("b", "run").await.unwrap(),
            "{backend}: the parent's reference.bin is reclaimed"
        );
        assert!(
            s.has_reference("b", "run/trace").await.unwrap(),
            "{backend}: the subfolder keeps its reference.bin"
        );
    }
}

/// On S3 the full `scan_deltaspace` HEADs every delta of the folder. Only
/// the delta-efficiency "verify" endpoint needs that (an operator runs it
/// for one folder); every other caller outside the storage layer takes
/// `scan_deltaspace_lite`.
#[test]
fn only_the_efficiency_verify_endpoint_scans_with_heads() {
    let callers: Vec<String> = crate::source_scan::prod_sources("src")
        .into_iter()
        .filter(|(file, text)| {
            !file.starts_with("src/storage/")
                && crate::source_scan::prod_text(text).contains(".scan_deltaspace(")
        })
        .map(|(file, _)| file)
        .collect();
    assert_eq!(callers, ["src/api/admin/delta_efficiency.rs"]);
}

/// The counter forgets a deleted object as soon as the object is gone,
/// before the reclaim check. A request cut inside that check (a client
/// disconnect, the request timeout) left the object counted.
#[tokio::test]
async fn a_delete_cut_in_its_reclaim_check_is_still_counted() {
    let (engine, fake) = s3_engine().await;
    let usage = Arc::new(BucketUsage::in_memory().unwrap());
    let engine = engine.with_bucket_usage(Some(usage.clone()));
    let keys = store_deltas(&engine, "run", 1).await;
    fake.set_delay_ms("LIST", 5_000);

    let cut = tokio::time::timeout(
        std::time::Duration::from_millis(1_000),
        engine.delete("b", &keys[0]),
    )
    .await;

    assert!(cut.is_err(), "the delete was cut in its reclaim check");
    assert!(engine.head("b", &keys[0]).await.unwrap_err().is_not_found());
    usage.flush_pending();
    assert_eq!(usage.read("b").unwrap().unwrap().object_count, 0);
}

/// A folder left with its reference.bin alone (a delete cut before its
/// reclaim) is reclaimed by the next delete that finds a key of it gone.
/// The UI lists no key in such a folder and the S3 API refuses the key
/// `reference.bin`, so nothing else could remove it.
#[tokio::test]
async fn a_delete_that_finds_nothing_reclaims_a_reference_left_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let fs = FilesystemBackend::new(tmp.path().to_path_buf())
        .await
        .unwrap();
    let fs_engine = DeltaGliderEngine::new_with_backend(
        Arc::new(DynStorageBackend::new_box(fs)),
        &Config::default(),
        None,
    );
    fs_engine.create_bucket("b").await.unwrap();
    let (s3, _fake) = s3_engine().await;
    for (backend, engine) in [("filesystem", fs_engine), ("s3", s3)] {
        let keys = store_deltas(&engine, "run", 2).await;
        for key in &keys {
            engine.delete_in_sweep("b", key).await.unwrap();
        }
        let s = engine.storage();
        assert!(s.has_reference("b", "run").await.unwrap(), "{backend}");

        let gone = engine.delete("b", &keys[0]).await.unwrap_err();

        assert!(gone.is_not_found(), "{backend}: {gone}");
        assert!(
            !s.has_reference("b", "run").await.unwrap(),
            "{backend}: the reference left alone is reclaimed"
        );
    }
}

/// A delete sends a DELETE for the other stored variant of the key
/// (`k.delta` for a passthrough `k`, and back) only when its HEAD found
/// that variant. Each delete sent both DELETEs, and the absent one also
/// queued a background facts cleanup.
#[tokio::test]
async fn a_delete_skips_the_variant_its_head_did_not_find() {
    let (engine, fake) = s3_engine().await;
    let keys = store_deltas(&engine, "run", 2).await;
    engine
        .store("b", "run/notes.txt", b"hello", None, Default::default())
        .await
        .unwrap();
    fake.clear();

    engine.delete("b", &keys[0]).await.unwrap();
    engine.delete("b", "run/notes.txt").await.unwrap();

    let deletes: Vec<String> = fake
        .requests()
        .into_iter()
        // Not the background listing-facts cleanup under `.dg/`.
        .filter(|r| r.starts_with("DELETE ") && !r.contains("/.dg/"))
        .map(|r| r.split('?').next().unwrap_or_default().to_string())
        .collect();
    assert_eq!(
        deletes,
        [
            "DELETE /b/run/build-000.zip.delta",
            "DELETE /b/run/notes.txt"
        ]
    );
}
