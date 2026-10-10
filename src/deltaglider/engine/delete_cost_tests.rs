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
