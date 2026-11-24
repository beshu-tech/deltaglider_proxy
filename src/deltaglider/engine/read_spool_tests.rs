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
    engine_with(threshold, SPOOL_MIB, 0).await
}

/// [`engine`] with a spool of `spool_mib`, a spool acquire timeout of 5
/// seconds, and spool files cached for `range_ttl_secs` (`0`: none).
async fn engine_with(
    threshold: u64,
    spool_mib: u64,
    range_ttl_secs: u64,
) -> (tempfile::TempDir, Arc<DynEngine>, Faults) {
    engine_with_spool(threshold, range_ttl_secs, |dir| {
        crate::deltaglider::spool::SpoolDir::new(dir, spool_mib * MIB).unwrap()
    })
    .await
}

/// [`engine_with`] on the spool that `make` builds in the given directory.
async fn engine_with_spool(
    threshold: u64,
    range_ttl_secs: u64,
    make: impl FnOnce(std::path::PathBuf) -> crate::deltaglider::spool::SpoolDir,
) -> (tempfile::TempDir, Arc<DynEngine>, Faults) {
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
    config.tuning.spool_acquire_timeout_secs = 5;
    config.range_spool_ttl_secs = range_ttl_secs;
    let mut engine = DeltaGliderEngine::new_with_backend(Arc::new(backend), &config, None);
    engine.spool = Arc::new(make(dir.path().join("spool")));
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
    legacy_reference_in(engine, "rel", name, stored, recorded).await;
}

/// [`legacy_reference`] in the folder `folder`.
async fn legacy_reference_in(
    engine: &DynEngine,
    folder: &str,
    name: &str,
    stored: &[u8],
    recorded: &[u8],
) {
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
            folder,
            Bytes::copy_from_slice(stored),
            &meta,
            crate::deltaglider::RefWriteProof::for_tests(),
        )
        .await
        .unwrap();
    storage
        .put_passthrough("b", folder, name, Bytes::copy_from_slice(recorded), &meta)
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
    crate::deltaglider::spool::as_background(async {
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
        let (stream, len, _, _) = engine
            .retrieve_stream_range("b", "rel/a.zip", 100, 199, None)
            .await
            .unwrap()
            .expect("a large reference object serves its range");
        assert_eq!(len, 100);
        assert_eq!(collect(stream).await, body[100..200]);
        assert_eq!(faults.whole_reads(), (0, 0), "no whole read");
        budget_is_free(&engine);
    })
    .await
}

/// The SHA-256 gate runs before the first byte: a reference object whose
/// bytes are not the ones its metadata records fails the GET and the
/// range GET (no stream, so no truncated 200), and frees its spool.
#[tokio::test]
async fn a_tampered_large_reference_object_fails_before_the_first_byte() {
    crate::deltaglider::spool::as_background(async {
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
    })
    .await
}

/// A background copy of a large reference object streams the source into
/// the spool once: `spooled_copy` used to see a buffered answer, give up,
/// and the caller read the whole source into memory again.
#[tokio::test]
async fn a_large_reference_object_copies_through_the_spool() {
    crate::deltaglider::spool::as_background(async {
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
            commit_count: &crate::maintenance::gate::NO_GATE_FOR_TEST,
            origin: crate::types::WriteOrigin::Job,
        };
        crate::transfer::copy_object_with_retries(&engine, request)
            .await
            .unwrap();
        assert_eq!(faults.whole_reads(), (0, 0), "no whole read of the source");
        let (got, _) = engine.retrieve("dst", "rel/a.zip").await.unwrap();
        assert_eq!(got, body);
    })
    .await
}

/// A large delta GET reads its delta into a spool file (reserved with the
/// output in one permit) before the codec slot. Before, the delta (up to
/// `max_delta_ratio` x the object) was read whole into memory. A small
/// object still decodes in memory.
#[tokio::test]
async fn a_large_delta_get_reads_its_delta_into_a_spool_file() {
    crate::deltaglider::spool::as_background(async {
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
    })
    .await
}

/// A large delta whose bytes are corrupt fails before the first byte and
/// gives back every spool file (reference, output, delta).
#[tokio::test]
async fn a_corrupt_large_delta_fails_and_frees_its_spool() {
    crate::deltaglider::spool::as_background(async {
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
    })
    .await
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

/// The disk gate (`disk_watch`) says no like a full budget: a 2 KiB PUT
/// against a reference above the threshold used to degrade to a whole store
/// when the budget was full, and still does when the spool DISK is at level
/// high. It is not refused: its body is in memory and the write needs no
/// spool.
#[tokio::test]
async fn a_small_put_against_a_large_reference_at_a_full_spool_disk_is_stored_whole() {
    use crate::disk_watch::Level;
    let (_dir, engine, _) = engine(64 * 1024).await;
    let base = noise(11, 200 * 1024);
    engine
        .store("b", "rel/a.zip", &base, None, HashMap::new())
        .await
        .unwrap();
    engine.spool.gate().set(Level::High);
    let mut small = base[..2048].to_vec();
    small[100] ^= 1;
    let r = engine
        .store("b", "rel/s.zip", &small, None, HashMap::new())
        .await
        .expect("a full spool disk does not refuse a write that needs no spool");
    assert_eq!(r.metadata.storage_info.label(), "passthrough");
    assert_eq!(
        r.metadata.passthrough_reason.as_deref(),
        Some("delta-unavailable")
    );
    assert_eq!(engine.retrieve("b", "rel/s.zip").await.unwrap().0, small);
    // With room again the same PUT is a delta.
    engine.spool.gate().set(Level::Ok);
    let mut again = base[..2048].to_vec();
    again[200] ^= 1;
    let r = engine
        .store("b", "rel/t.zip", &again, None, HashMap::new())
        .await
        .unwrap();
    assert_eq!(r.metadata.storage_info.label(), "delta");
    budget_is_free(&engine);
}

/// A GET of a large reference object reserves its spool file BEFORE it
/// takes the cache slot of the folder's reference. The streaming PUTs and
/// delta GETs of the folder join that slot while they hold spool budget;
/// before, a GET that waited for budget held the slot, so such a holder
/// waited behind it, and it for the holder, until the acquire timeout.
#[tokio::test]
async fn a_reference_fill_never_waits_for_budget_inside_the_slot() {
    crate::deltaglider::spool::as_background(async {
        let (_dir, engine, _) = engine_with(1024, 8, 60).await;
        let body = noise(11, 64 * 1024);
        legacy_reference(&engine, "a.zip", &body, &body).await;
        let sha = hex::encode(Sha256::digest(&body));
        // Every MiB of the budget is held: the GET waits for one.
        let full = engine.spool.try_acquire(8 * MIB).unwrap();
        let get = {
            let engine = engine.clone();
            tokio::spawn(crate::deltaglider::spool::as_background(async move {
                engine.retrieve("b", "rel/a.zip").await
            }))
        };
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        // A holder joins the folder's reference slot.
        let key = crate::deltaglider::range_spool::RangeSpoolKey::reference(
            engine.cache_key("b", "rel"),
            sha,
        );
        let joined = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            engine.range_spools.get_or_fill(key, || async {
                Err::<crate::deltaglider::spool::Spool, _>("the holder's own fill")
            }),
        )
        .await;
        assert!(
            joined.is_ok(),
            "the holder waited behind a fill that waits for spool budget"
        );
        drop(full);
        let (got, _) = get.await.unwrap().unwrap();
        assert_eq!(got, body);
    })
    .await
}

fn copy_request<'a>(source_key: &'a str) -> crate::transfer::ObjectTransferRequest<'a> {
    crate::transfer::ObjectTransferRequest {
        source_bucket: "b",
        source_key,
        destination_bucket: "dst",
        destination_key: source_key,
        provenance: None,
        strip_user_metadata_keys: &[],
        operation: "replication",
        upload_concurrency: None,
        keep_created_at: false,
        commit_count: &crate::maintenance::gate::NO_GATE_FOR_TEST,
        origin: crate::types::WriteOrigin::Job,
    }
}

/// A background copy whose source streams from a spool file holds that
/// budget, so it never waits for the budget of its destination copy: a
/// copy whose source and destination together are larger than the spool
/// fails at once (it never fits); one that finds the rest of the budget
/// taken by another holder fails at once with a retryable error. Before,
/// both waited for the acquire timeout (5 s here, 120 s by default), and
/// two such copies waited for each other.
#[tokio::test]
async fn a_copy_that_holds_its_source_spool_never_waits_for_more() {
    crate::deltaglider::spool::as_background(async {
        // Source 2 MiB + destination 2 MiB > a 3 MiB spool.
        let (_dir, engine, _) = engine_with(1024, 3, 0).await;
        let body = noise(12, 2 * MIB as usize);
        legacy_reference(&engine, "a.zip", &body, &body).await;
        let started = std::time::Instant::now();
        let err = crate::transfer::copy_object_with_retries(&engine, copy_request("rel/a.zip"))
            .await
            .expect_err("the copy cannot fit the spool");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "the copy waited {:?} for spool budget: {err}",
            started.elapsed()
        );
        assert!(err.to_string().contains("Object too large"), "{err}");
        assert_eq!(engine.spool.free_mib(), 3);

        // Another holder (the stream of a concurrent copy of another source)
        // holds 2 MiB of a 4 MiB spool: the copy holds its source's 2 MiB and
        // finds nothing left for its destination.
        let (_dir, engine, _) = engine_with(1024, 4, 0).await;
        legacy_reference(&engine, "a.zip", &body, &body).await;
        let other = noise(13, 2 * MIB as usize);
        let meta = crate::types::FileMetadata::new_reference(
            "o.zip".into(),
            "o.zip".into(),
            hex::encode(Sha256::digest(&other)),
            hex::encode(Md5::digest(&other)),
            other.len() as u64,
            None,
        );
        let storage = engine.storage();
        storage
            .put_reference(
                "b",
                "oth",
                Bytes::copy_from_slice(&other),
                &meta,
                crate::deltaglider::RefWriteProof::for_tests(),
            )
            .await
            .unwrap();
        storage
            .put_passthrough("b", "oth", "o.zip", Bytes::copy_from_slice(&other), &meta)
            .await
            .unwrap();
        let RetrieveResponse::Streamed { stream: held, .. } =
            engine.retrieve_stream("b", "oth/o.zip").await.unwrap()
        else {
            panic!("a large reference object streams")
        };
        assert_eq!(engine.spool.free_mib(), 2);
        let started = std::time::Instant::now();
        let err = crate::transfer::copy_object_with_retries(&engine, copy_request("rel/a.zip"))
            .await
            .expect_err("no budget for the destination while the other stream lives");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(4),
            "the copy waited {:?} for spool budget while it held its source: {err}",
            started.elapsed()
        );
        drop(held);
        assert_eq!(engine.spool.free_mib(), 4);
        crate::transfer::copy_object_with_retries(&engine, copy_request("rel/a.zip"))
            .await
            .unwrap();
        assert_eq!(engine.retrieve("dst", "rel/a.zip").await.unwrap().0, body);
    })
    .await
}

// ---- read shares count live response streams only ----

/// Four reference objects in four folders (4 different spool files of 1 MiB
/// each), for a spool of `spool_mib`.
async fn four_objects(
    spool_mib: u64,
    ttl: u64,
) -> (tempfile::TempDir, Arc<DynEngine>, Vec<Vec<u8>>) {
    let (dir, engine, _) = engine_with(1024, spool_mib, ttl).await;
    let mut bodies = Vec::new();
    for i in 0..4u64 {
        let body = noise(40 + i, 64 * 1024);
        legacy_reference_in(&engine, &format!("f{i}"), "a.zip", &body, &body).await;
        bodies.push(body);
    }
    (dir, engine, bodies)
}

/// A finished download leaves its spool file in the range cache. The cache
/// is evictable, so it is no charge: a principal that downloaded one file
/// is never locked out of the next by its own finished download.
#[tokio::test]
async fn a_finished_download_in_the_cache_is_no_charge_of_its_principal() {
    use crate::deltaglider::spool::as_principal;
    // 4 MiB spool: a 25 % share is 1 MiB, the same as one file.
    let (_dir, engine, bodies) = four_objects(4, 600).await;
    for (i, body) in bodies.iter().enumerate() {
        let key = format!("f{i}/a.zip");
        let got = as_principal("dana", async {
            match engine.retrieve_stream("b", &key).await.unwrap() {
                RetrieveResponse::Streamed { stream, .. } => collect(stream).await,
                RetrieveResponse::Buffered { .. } => panic!("buffered"),
            }
        })
        .await;
        assert_eq!(&got, body, "download {i} of the same principal");
    }
}

/// One principal alone (the single CI uploader) runs several large reads at
/// once: it may use 75 % of the read space, not 25 %.
#[tokio::test]
async fn one_principal_alone_runs_three_large_reads_at_once() {
    use crate::deltaglider::spool::as_principal;
    let (_dir, engine, bodies) = four_objects(8, 0).await;
    let mut open = Vec::new();
    for i in 0..3 {
        let key = format!("f{i}/a.zip");
        let response = as_principal("ci-uploader", engine.retrieve_stream("b", &key))
            .await
            .unwrap_or_else(|e| panic!("read {i} of a lone principal: {e}"));
        open.push(response);
    }
    for (response, body) in open.into_iter().zip(&bodies) {
        match response {
            RetrieveResponse::Streamed { stream, .. } => assert_eq!(&collect(stream).await, body),
            RetrieveResponse::Buffered { .. } => panic!("buffered"),
        }
    }
}

/// The life of a read charge: a stream charges its own principal while it
/// lives; the file waits in the cache with no charge; a second principal that
/// streams the cached file pays for it itself, with no new reservation; the
/// TTL ends the file.
#[tokio::test]
async fn a_read_charge_lives_with_its_stream_not_with_the_cache() {
    use crate::deltaglider::spool::as_principal;
    let (_dir, engine, bodies) = four_objects(4, 1).await;
    let stream_of = |who: &'static str| {
        let engine = engine.clone();
        async move {
            as_principal(who, engine.retrieve_stream("b", "f0/a.zip"))
                .await
                .unwrap()
        }
    };
    // 1. Dana fills the file and streams it: dana is charged.
    let response = stream_of("dana").await;
    assert_eq!(engine.spool.read_held("dana"), (1, 1));
    // 2. The stream ends: the file stays in the cache, nobody is charged.
    match response {
        RetrieveResponse::Streamed { stream, .. } => assert_eq!(collect(stream).await, bodies[0]),
        RetrieveResponse::Buffered { .. } => panic!("buffered"),
    }
    assert_eq!(engine.spool.read_held("dana"), (0, 0));
    assert_eq!(engine.spool.free_mib(), 3, "the cache holds the file");
    // 3. Another principal streams the cached file: it pays, with no new reservation.
    let response = stream_of("ci-uploader").await;
    assert_eq!(engine.spool.read_held("ci-uploader"), (1, 1));
    assert_eq!(engine.spool.read_held("dana"), (0, 1));
    assert_eq!(engine.spool.free_mib(), 3, "a cache hit reserves nothing");
    // 4. Dropping the stream (a client that went away) ends the charge.
    drop(response);
    assert_eq!(engine.spool.read_held("ci-uploader"), (0, 0));
    // 5. The TTL ends the file and frees its budget.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while engine.spool.free_mib() < 4 {
        assert!(
            std::time::Instant::now() < deadline,
            "the file did not expire"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

// ---- the fill of a read is charged before it reserves anything ----

/// The files of the spool directory now.
fn spool_files(dir: &tempfile::TempDir) -> usize {
    std::fs::read_dir(dir.path().join("spool"))
        .map(|d| d.count())
        .unwrap_or(0)
}

/// `n` folders `f<i>`, each a reference `a.zip` and a delta `b.zip` of 64 KiB
/// (a spool file of 1 MiB each).
async fn delta_folders(engine: &DynEngine, n: usize) {
    for i in 0..n {
        let base = noise(60 + i as u64, 64 * 1024);
        let mut v = base.clone();
        v[3000..3100].fill(7);
        for (name, body) in [("a.zip", &base), ("b.zip", &v)] {
            engine
                .store("b", &format!("f{i}/{name}"), body, None, HashMap::new())
                .await
                .unwrap();
        }
        assert!(engine
            .head("b", &format!("f{i}/b.zip"))
            .await
            .unwrap()
            .is_delta());
    }
}

/// A principal over its reach is refused BEFORE its GET of a new delta
/// reserves, downloads or decodes anything: the share rule is asked with the
/// expected reservation (output + delta). Before, each such GET filled a file
/// and only the stream was refused, so a flood of distinct deltas made the
/// proxy decode them all.
#[tokio::test]
async fn a_principal_over_its_reach_is_refused_before_its_fill_reserves_anything() {
    use crate::deltaglider::spool::as_principal;
    // Reads in total: 25 % of 32 MiB = 8; a share is 4, the rest 4.
    let (dir, engine, _) = engine_with_spool(16 * 1024, 600, |d| {
        crate::deltaglider::spool::SpoolDir::new(d, 32 * MIB)
            .unwrap()
            .with_read_max(25)
            .with_read_share(50)
    })
    .await;
    delta_folders(&engine, 5).await;
    let mut open = Vec::new();
    for i in 0..3 {
        let key = format!("f{i}/b.zip");
        let response = as_principal("mallory", engine.retrieve_stream("b", &key))
            .await
            .unwrap_or_else(|e| panic!("fill {i} within the reach: {e}"));
        open.push(response);
    }
    let (free, files) = (engine.spool.free_mib(), spool_files(&dir));
    let refused = as_principal("mallory", engine.retrieve_stream("b", "f3/b.zip")).await;
    assert!(
        matches!(refused, Err(EngineError::Overloaded(_))),
        "{:?}",
        refused.err().map(|e| e.to_string())
    );
    assert_eq!(engine.spool.free_mib(), free, "the refused fill reserved");
    assert_eq!(spool_files(&dir), files, "the refused fill made a file");
    // Another principal has its own share.
    let dana = as_principal("dana", engine.retrieve_stream("b", "f4/b.zip")).await;
    assert!(dana.is_ok(), "{:?}", dana.err().map(|e| e.to_string()));
}

// ---- the fill is charged for everything it reserves ----

/// An engine with reads capped at 8 MiB of a 32 MiB spool (a share is 4, the
/// rest 4, so one principal reaches 4) and `ttl` seconds of cached files.
async fn capped_engine(ttl: u64) -> (tempfile::TempDir, Arc<DynEngine>) {
    let (dir, engine, _) = engine_with_spool(16 * 1024, ttl, |d| {
        crate::deltaglider::spool::SpoolDir::new(d, 32 * MIB)
            .unwrap()
            .with_read_max(25)
            .with_read_share(50)
    })
    .await;
    delta_folders(&engine, 3).await;
    (dir, engine)
}

/// A GET of the delta of folder `i` by `who`, as a task.
fn spawn_get(
    engine: &Arc<DynEngine>,
    who: &'static str,
    i: usize,
) -> tokio::task::JoinHandle<Result<RetrieveResponse, EngineError>> {
    let engine = engine.clone();
    tokio::spawn(async move {
        crate::deltaglider::spool::as_principal(who, async move {
            engine.retrieve_stream("b", &format!("f{i}/b.zip")).await
        })
        .await
    })
}

/// Wait until `who` is charged `mib` (or time runs out).
async fn wait_for_charge(engine: &DynEngine, who: &str, mib: usize) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while engine.spool.read_held(who).0 != mib {
        assert!(
            std::time::Instant::now() < deadline,
            "{who} is charged {} MiB, not {mib}",
            engine.spool.read_held(who).0
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// A fill with its own reference copy holds three files (output, delta,
/// reference) from before the codec slot until the decode ends. Its charge
/// covers all three, so the reads in flight never hold more than the read cap
/// allows: a second fill is refused. Before, only output and delta were
/// charged and the reference copy was in no ledger.
#[tokio::test]
async fn a_fill_is_charged_for_its_reference_copy_while_it_waits_for_the_codec() {
    let (_dir, engine) = capped_engine(0).await;
    // Every codec slot is taken: a fill waits for one after it reserved.
    let slots = engine.codec_semaphore.available_permits();
    let blocked = engine
        .codec_semaphore
        .acquire_many(slots as u32)
        .await
        .unwrap();
    let first = spawn_get(&engine, "mallory", 0);
    // Output, delta and the reference copy: 1 MiB each.
    wait_for_charge(&engine, "mallory", 3).await;
    // A second fill would hold 3 MiB more: over the reach of the principal.
    let second = as_principal_get(&engine, "mallory", 1).await;
    assert!(
        matches!(second, Err(EngineError::Overloaded(_))),
        "{:?}",
        second.err().map(|e| e.to_string())
    );
    // The decode runs; the reference and the delta end with it, and the
    // output is the charge of the stream.
    drop(blocked);
    let response = first.await.unwrap().expect("the first fill");
    assert_eq!(engine.spool.read_held("mallory"), (1, 1));
    drop(response);
    assert_eq!(engine.spool.read_held("mallory"), (0, 0));
}

async fn as_principal_get(
    engine: &Arc<DynEngine>,
    who: &'static str,
    i: usize,
) -> Result<RetrieveResponse, EngineError> {
    spawn_get(engine, who, i).await.unwrap()
}

/// A reference file that ends in the shared cache is evictable: it charges
/// nobody once it is cached, and the fill is charged for output and delta.
#[tokio::test]
async fn a_cached_reference_is_no_charge() {
    let (_dir, engine) = capped_engine(600).await;
    let slots = engine.codec_semaphore.available_permits();
    let blocked = engine
        .codec_semaphore
        .acquire_many(slots as u32)
        .await
        .unwrap();
    let first = spawn_get(&engine, "mallory", 0);
    // The reference is downloaded into the cache, then the fill waits for the codec.
    // Charged: output and delta. The reference is downloaded into the cache
    // first (charged while it downloads), then the fill waits for the codec.
    wait_for_charge(&engine, "mallory", 2).await;
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert_eq!(
        engine.spool.read_held("mallory").0,
        2,
        "the cached reference is no charge"
    );
    drop(blocked);
    let response = first.await.unwrap().expect("the fill");
    assert_eq!(engine.spool.read_held("mallory"), (1, 1));
    drop(response);
}

/// Two principals read one large cached file at the same time. The read cap
/// is about disk space and the file is on disk once, so the second principal
/// adds nothing to the total (it still counts the file for itself). Before,
/// the total counted the file once for each principal, so a file above half
/// of what one principal reaches could not be read by two principals at once.
#[tokio::test]
async fn two_principals_read_one_large_cached_file_at_once() {
    use crate::deltaglider::spool::as_principal;
    // Reads: 50 % of 16 MiB = 8; a share is 2, the rest 6. The file is 4 MiB.
    let (_dir, engine, _) = engine_with_spool(1024, 600, |d| {
        crate::deltaglider::spool::SpoolDir::new(d, 16 * MIB)
            .unwrap()
            .with_read_max(50)
            .with_read_share(25)
    })
    .await;
    let body = noise(77, 3 * MIB as usize + MIB as usize / 2);
    legacy_reference_in(&engine, "f0", "a.zip", &body, &body).await;
    let open = |who: &'static str| {
        let engine = engine.clone();
        async move {
            as_principal(who, engine.retrieve_stream("b", "f0/a.zip"))
                .await
                .unwrap_or_else(|e| panic!("{who} reads the file: {e}"))
        }
    };
    let first = open("dana").await;
    assert_eq!(engine.spool.read_held("dana"), (4, 4));
    let second = open("ci-uploader").await;
    assert_eq!(
        engine.spool.read_held("ci-uploader"),
        (4, 4),
        "its own count has the file, the total has it once"
    );
    for response in [first, second] {
        match response {
            RetrieveResponse::Streamed { stream, .. } => assert_eq!(collect(stream).await, body),
            RetrieveResponse::Buffered { .. } => panic!("buffered"),
        }
    }
    assert_eq!(engine.spool.read_held("dana"), (0, 0));
}
