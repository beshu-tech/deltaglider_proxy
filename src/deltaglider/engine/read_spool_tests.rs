// SPDX-License-Identifier: BUSL-1.1

//! Large objects on the read side never pass through memory whole: a
//! reference object above the spool threshold is served from a verified
//! spool file, a large delta GET reads its delta into a spool file, and a
//! small PUT or GET against a large reference uses the reference FILE. The
//! `FaultyFs` counts whole-object reads (`get_object`), so each test pins
//! the structure, not the RSS.

use super::*;
use crate::config::Config;
use crate::storage::{DynStorageBackend, Faults, FaultyFs, FilesystemBackend, StorageBackendExt};
use md5::{Digest, Md5};
use sha2::Sha256;
use std::collections::HashMap;

const MIB: u64 = 1024 * 1024;
const SPOOL_MIB: u64 = 64;

fn noise(seed: u64, n: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

/// An engine on a counting filesystem backend with bucket `b`, its own
/// spool budget of [`SPOOL_MIB`], and no cached spool files (each read
/// fills its own, so the budget is free again after each test step).
async fn engine(threshold: u64) -> (tempfile::TempDir, Arc<DynEngine>, Faults) {
    let dir = tempfile::tempdir().unwrap();
    let faulty = FaultyFs::new(
        FilesystemBackend::new(dir.path().join("data"))
            .await
            .unwrap(),
    );
    let faults = faulty.faults.clone();
    let backend: Box<DynStorageBackend<'static>> = DynStorageBackend::new_box(faulty);
    let mut config = Config::default();
    config.tuning.spool_threshold_bytes = Some(threshold);
    config.range_spool_ttl_secs = 0;
    let mut engine = DeltaGliderEngine::new_with_backend(Arc::new(backend), &config, None);
    engine.spool = Arc::new(
        crate::deltaglider::spool::SpoolDir::new(dir.path().join("spool"), SPOOL_MIB * MIB)
            .unwrap(),
    );
    for b in ["b", "dst"] {
        engine.create_bucket(b).await.unwrap();
    }
    (dir, Arc::new(engine), faults)
}

/// A reference object `b/rel/<name>`: an object whose stored metadata is
/// the folder reference's (a copy of reference.bin with its metadata, the
/// layout of the first DeltaGlider releases). Its bytes are read from the
/// folder's reference.bin and checked against the SHA-256 it records.
/// `recorded`: the bytes whose hashes the metadata records (another body
/// in reference.bin = a tampered reference).
async fn legacy_reference(engine: &DynEngine, name: &str, stored: &[u8], recorded: &[u8]) {
    let meta = crate::types::FileMetadata::new_reference(
        name.into(),
        name.into(),
        hex::encode(Sha256::digest(recorded)),
        hex::encode(Md5::digest(recorded)),
        recorded.len() as u64,
        None,
    );
    let storage = engine.storage();
    storage
        .put_reference(
            "b",
            "rel",
            Bytes::copy_from_slice(stored),
            &meta,
            crate::deltaglider::RefWriteProof::for_tests(),
        )
        .await
        .unwrap();
    storage
        .put_passthrough("b", "rel", name, Bytes::copy_from_slice(recorded), &meta)
        .await
        .unwrap();
}

async fn collect(
    stream: futures::stream::BoxStream<'static, Result<Bytes, StorageError>>,
) -> Vec<u8> {
    use futures::TryStreamExt;
    let chunks: Vec<Bytes> = stream.try_collect().await.unwrap();
    chunks.concat()
}

fn budget_is_free(engine: &DynEngine) {
    assert_eq!(
        engine.spool.free_mib() as u64,
        SPOOL_MIB,
        "every spool file of the read is gone"
    );
}

/// A reference object above the spool threshold streams from a verified
/// spool file, whole and as a range. Before, it was read whole into memory
/// (`get_reference`), whatever its size.
#[tokio::test]
async fn a_large_reference_object_reads_from_a_verified_spool_file() {
    let (_dir, engine, faults) = engine(1024).await;
    let body = noise(1, 64 * 1024);
    legacy_reference(&engine, "a.zip", &body, &body).await;
    match engine.retrieve_stream("b", "rel/a.zip").await.unwrap() {
        RetrieveResponse::Streamed {
            stream, metadata, ..
        } => {
            assert!(metadata.is_reference());
            assert_eq!(collect(stream).await, body);
        }
        RetrieveResponse::Buffered { .. } => panic!("a large reference object was buffered"),
    }
    let (stream, len, _) = engine
        .retrieve_stream_range("b", "rel/a.zip", 100, 199, None)
        .await
        .unwrap()
        .expect("a large reference object serves its range");
    assert_eq!(len, 100);
    assert_eq!(collect(stream).await, body[100..200]);
    assert_eq!(faults.whole_reads(), (0, 0), "no whole read");
    budget_is_free(&engine);
}

/// The SHA-256 gate runs before the first byte: a reference object whose
/// bytes are not the ones its metadata records fails the GET and the
/// range GET (no stream, so no truncated 200), and frees its spool.
#[tokio::test]
async fn a_tampered_large_reference_object_fails_before_the_first_byte() {
    let (_dir, engine, _) = engine(1024).await;
    let body = noise(2, 64 * 1024);
    let mut tampered = body.clone();
    tampered[5000] ^= 1;
    legacy_reference(&engine, "a.zip", &tampered, &body).await;
    let err = engine.retrieve_stream("b", "rel/a.zip").await.err();
    assert!(
        matches!(err, Some(EngineError::ChecksumMismatch { .. })),
        "{:?}",
        err.map(|e| e.to_string())
    );
    let err = engine
        .retrieve_stream_range("b", "rel/a.zip", 0, 9, None)
        .await
        .err();
    assert!(matches!(err, Some(EngineError::ChecksumMismatch { .. })));
    budget_is_free(&engine);
}

/// A background copy of a large reference object streams the source into
/// the spool once: `spooled_copy` used to see a buffered answer, give up,
/// and the caller read the whole source into memory again.
#[tokio::test]
async fn a_large_reference_object_copies_through_the_spool() {
    let (_dir, engine, faults) = engine(1024).await;
    let body = noise(3, 64 * 1024);
    legacy_reference(&engine, "a.zip", &body, &body).await;
    let request = crate::transfer::ObjectTransferRequest {
        source_bucket: "b",
        source_key: "rel/a.zip",
        destination_bucket: "dst",
        destination_key: "rel/a.zip",
        provenance: None,
        strip_user_metadata_keys: &[],
        operation: "replication",
        upload_concurrency: None,
        keep_created_at: false,
    };
    crate::transfer::copy_object_with_retries(&engine, request)
        .await
        .unwrap();
    assert_eq!(faults.whole_reads(), (0, 0), "no whole read of the source");
    let (got, _) = engine.retrieve("dst", "rel/a.zip").await.unwrap();
    assert_eq!(got, body);
}

/// A large delta GET reads its delta into a spool file (reserved with the
/// output in one permit) before the codec slot. Before, the delta (up to
/// `max_delta_ratio` x the object) was read whole into memory. A small
/// object still decodes in memory.
#[tokio::test]
async fn a_large_delta_get_reads_its_delta_into_a_spool_file() {
    let (_dir, engine, faults) = engine(16 * 1024).await;
    let base = noise(4, 64 * 1024);
    let mut v = base.clone();
    v[3000..3100].fill(7);
    for (key, body) in [("rel/a.zip", &base), ("rel/b.zip", &v)] {
        engine
            .store("b", key, body, None, HashMap::new())
            .await
            .unwrap();
    }
    assert!(engine.head("b", "rel/b.zip").await.unwrap().is_delta());
    let before = faults.whole_reads();
    let (got, _) = engine.retrieve("b", "rel/b.zip").await.unwrap();
    assert_eq!(got, v);
    assert_eq!(faults.whole_reads(), before, "no whole read of the delta");
    budget_is_free(&engine);

    // The counter sees a whole read: a small delta decodes in memory.
    let small = noise(5, 4 * 1024);
    let mut small_v = small.clone();
    small_v[10] ^= 1;
    for (key, body) in [("s/a.zip", &small), ("s/b.zip", &small_v)] {
        engine
            .store("b", key, body, None, HashMap::new())
            .await
            .unwrap();
    }
    let before = faults.whole_reads().0;
    assert_eq!(engine.retrieve("b", "s/b.zip").await.unwrap().0, small_v);
    assert!(
        faults.whole_reads().0 > before,
        "the small delta is read whole"
    );
}

/// A large delta whose bytes are corrupt fails before the first byte and
/// gives back every spool file (reference, output, delta).
#[tokio::test]
async fn a_corrupt_large_delta_fails_and_frees_its_spool() {
    let (dir, engine, _) = engine(16 * 1024).await;
    let base = noise(6, 64 * 1024);
    let mut v = base.clone();
    v[3000..3100].fill(7);
    for (key, body) in [("rel/a.zip", &base), ("rel/b.zip", &v)] {
        engine
            .store("b", key, body, None, HashMap::new())
            .await
            .unwrap();
    }
    let delta = dir.path().join("data/b/deltaspaces/rel/b.zip.delta");
    let len = std::fs::metadata(&delta).unwrap().len() as usize;
    std::fs::write(&delta, noise(7, len)).unwrap();
    assert!(engine.retrieve_stream("b", "rel/b.zip").await.is_err());
    budget_is_free(&engine);
}

/// A PUT body in memory (at most the spool threshold) against a
/// reference above the threshold: the decision uses the reference size
/// from the lock's read, the body goes to a spool file, and the streaming
/// encode reads the reference FILE. Before, the whole reference was read
/// into the RAM reference cache. The GET of that small delta decodes
/// against the reference file too.
#[tokio::test]
async fn a_small_put_against_a_large_reference_encodes_from_files() {
    let (_dir, engine, faults) = engine(64 * 1024).await;
    let base = noise(8, 200 * 1024);
    engine
        .store("b", "rel/a.zip", &base, None, HashMap::new())
        .await
        .unwrap();
    let mut small = base[..32 * 1024].to_vec();
    small[100] ^= 1;
    let before = faults.delta_file_writes();
    let r = engine
        .store("b", "rel/s.zip", &small, None, HashMap::new())
        .await
        .unwrap();
    assert_eq!(r.metadata.storage_info.label(), "delta");
    assert_eq!(
        faults.delta_file_writes(),
        before + 1,
        "the spooled encode commits its delta from its file"
    );
    engine.metadata_cache.invalidate("b", "rel/s.zip");
    let (got, _) = engine.retrieve("b", "rel/s.zip").await.unwrap();
    assert_eq!(got, small);
    assert_eq!(faults.whole_reads().1, 0, "no whole reference read");
    budget_is_free(&engine);
}

/// The same PUT with no spool budget free: it never waits under the
/// deltaspace lock, and stores the body whole as `delta-unavailable`.
#[tokio::test]
async fn a_small_put_against_a_large_reference_without_spool_is_stored_whole() {
    let (_dir, engine, _) = engine(64 * 1024).await;
    let base = noise(9, 200 * 1024);
    engine
        .store("b", "rel/a.zip", &base, None, HashMap::new())
        .await
        .unwrap();
    let full = engine.spool.try_acquire(SPOOL_MIB * MIB).unwrap();
    let mut small = base[..32 * 1024].to_vec();
    small[100] ^= 1;
    let r = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        engine.store("b", "rel/s.zip", &small, None, HashMap::new()),
    )
    .await
    .expect("the PUT must not wait for spool under the lock")
    .unwrap();
    assert_eq!(r.metadata.storage_info.label(), "passthrough");
    assert_eq!(
        r.metadata.passthrough_reason.as_deref(),
        Some("delta-unavailable")
    );
    drop(full);
    assert_eq!(engine.retrieve("b", "rel/s.zip").await.unwrap().0, small);
}

/// The legacy-reference migration of a reference above the spool
/// threshold encodes it against itself from a spool file: the reference
/// cache refuses a reference this large, so the migration must not go
/// through it.
#[tokio::test]
async fn a_large_legacy_reference_migrates_from_a_spool_file() {
    let (_dir, engine, _) = engine(64 * 1024).await;
    let body = noise(10, 200 * 1024);
    let legacy = crate::types::FileMetadata::new_reference(
        "a.zip".into(),
        "a.zip".into(),
        hex::encode(Sha256::digest(&body)),
        hex::encode(Md5::digest(&body)),
        body.len() as u64,
        None,
    );
    engine
        .storage()
        .put_reference(
            "b",
            "rel",
            Bytes::copy_from_slice(&body),
            &legacy,
            crate::deltaglider::RefWriteProof::for_tests(),
        )
        .await
        .unwrap();
    let (migrated, _, errors) = engine.migrate_legacy_references("b").await.unwrap();
    assert_eq!((migrated, errors), (1, 0));
    let (got, meta) = engine.retrieve("b", "rel/a.zip").await.unwrap();
    assert!(meta.is_delta());
    assert_eq!(got, body);
    budget_is_free(&engine);
}
