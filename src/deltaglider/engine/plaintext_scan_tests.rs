// SPDX-License-Identifier: BUSL-1.1

//! The boot scan of a bucket: its cost (one walk, bounded memory, baselines from the
//! pages), its retries of a transient error, and what a cut scan reads.

use super::*;
use crate::config::Config;
use crate::job_loop::Pager;
use crate::storage::{
    DynStorageBackend, Fault, FaultPoint, FaultyFs, FilesystemBackend, StorageBackend,
};
use crate::usage_scanner::test_support::{fake_s3_engine, put_raw_many, scope_lists};

const OBJECTS: usize = 2300;
const DELTASPACES: [&str; 3] = ["d1", "d2", "d3"];

/// A fake S3 bucket with `OBJECTS` plain keys and one baseline in each of `DELTASPACES`.
async fn seeded_s3() -> (DynEngine, std::sync::Arc<crate::storage::FakeS3>) {
    let (engine, fake, endpoint) = fake_s3_engine().await;
    let mut keys: Vec<String> = (0..OBJECTS).map(|i| format!("f/k-{i:05}.txt")).collect();
    keys.extend(DELTASPACES.iter().map(|d| format!("{d}/reference.bin")));
    put_raw_many(&endpoint, keys).await;
    fake.clear();
    (engine, fake)
}

fn reference_heads(fake: &crate::storage::FakeS3) -> usize {
    fake.requests()
        .iter()
        .filter(|r| r.starts_with("HEAD ") && r.contains("reference.bin"))
        .count()
}

/// The scan lists a bucket a page at a time only: the baselines come from the pages, not from
/// a second whole-bucket listing that holds every key in memory.
#[tokio::test]
async fn an_s3_scan_lists_the_bucket_once_and_reads_baselines_from_the_pages() {
    let (engine, fake) = seeded_s3().await;
    let scan = engine.plaintext_scan("b", ScanScope::Whole).await.unwrap();
    assert!(scan.complete, "{scan:?}");
    assert_eq!(scan.listed, OBJECTS as u64);
    assert_eq!(reference_heads(&fake), DELTASPACES.len());
    // The whole-bucket collector sends no `max-keys`: every listing here is one bounded page.
    let lists = scope_lists(&fake);
    let unbounded: Vec<_> = lists.iter().filter(|r| !r.contains("max-keys=")).collect();
    assert!(
        unbounded.is_empty(),
        "a listing outside the pages: {unbounded:#?}"
    );
}

/// The scan holds one page of keys, whatever the size of the bucket.
#[tokio::test]
async fn a_scan_holds_at_most_one_page_of_keys() {
    let (engine, _fake) = seeded_s3().await;
    let scan = engine.plaintext_scan("b", ScanScope::Whole).await.unwrap();
    assert!(scan.listed > 2 * u64::from(crate::maintenance::worker::PAGE_SIZE));
    assert!(scan.peak_held > 0);
    assert!(
        scan.peak_held <= u64::from(crate::maintenance::worker::PAGE_SIZE),
        "held {} keys at once",
        scan.peak_held
    );
}

/// A scan that the page budget cut cannot open a window: it reads no baseline.
#[tokio::test]
async fn a_scan_cut_by_the_page_budget_reads_no_baseline() {
    let (engine, fake) = seeded_s3().await;
    let scan = engine
        .plaintext_scan_paged("b", ScanScope::Whole, Pager::fresh().with_max_pages(1))
        .await
        .unwrap();
    assert!(!scan.complete, "{scan:?}");
    assert_eq!(reference_heads(&fake), 0);
}

async fn fs_engine_with_one_object() -> (tempfile::TempDir, DynEngine, crate::storage::Faults) {
    let dir = tempfile::tempdir().unwrap();
    let fs = FilesystemBackend::new(dir.path().to_path_buf())
        .await
        .unwrap();
    fs.create_bucket("b").await.unwrap();
    let body = bytes::Bytes::from_static(b"plain");
    let meta = crate::types::FileMetadata::new_passthrough(
        "a.txt".into(),
        "0".repeat(64),
        "0".repeat(32),
        body.len() as u64,
        None,
    );
    fs.put_passthrough("b", "p", "a.txt", body, &meta)
        .await
        .unwrap();
    let faulty = FaultyFs::new(fs);
    let faults = faulty.faults.clone();
    let backend: Box<DynStorageBackend<'static>> = DynStorageBackend::new_box(faulty);
    let engine = DeltaGliderEngine::new_with_backend(Arc::new(backend), &Config::default(), None);
    (dir, engine, faults)
}

/// A throttled HEAD is read again (the job loops' bounded retry), not a lost scan.
#[tokio::test]
async fn a_throttled_head_is_read_again() {
    let (_dir, engine, faults) = fs_engine_with_one_object().await;
    faults.arm_times(
        FaultPoint::GetPassthroughMetadata,
        "b/p/a.txt",
        Fault::Throttled,
        2,
    );
    let scan = engine.plaintext_scan("b", ScanScope::Whole).await.unwrap();
    assert!(scan.complete, "{scan:?}");
    assert_eq!(
        faults.fired(FaultPoint::GetPassthroughMetadata, "b/p/a.txt"),
        2
    );
}

/// A HEAD that keeps failing ends the scan after the bounded attempts.
#[tokio::test]
async fn a_head_that_keeps_failing_ends_the_scan_after_the_attempts() {
    let (_dir, engine, faults) = fs_engine_with_one_object().await;
    faults.arm(
        FaultPoint::GetPassthroughMetadata,
        "b/p/a.txt",
        Fault::Throttled,
    );
    assert!(engine.plaintext_scan("b", ScanScope::Whole).await.is_err());
    assert_eq!(
        faults.calls(FaultPoint::GetPassthroughMetadata, "b/p/a.txt"),
        crate::job_loop::LIST_ATTEMPTS as usize
    );
}

/// A throttled page listing is sent again with the same token.
#[tokio::test]
async fn a_throttled_page_listing_is_sent_again() {
    let (_dir, engine, faults) = fs_engine_with_one_object().await;
    faults.arm_times(FaultPoint::ListObjectsDelegated, "b//", Fault::Throttled, 1);
    let scan = engine.plaintext_scan("b", ScanScope::Whole).await.unwrap();
    assert!(scan.complete, "{scan:?}");
    assert_eq!(faults.fired(FaultPoint::ListObjectsDelegated, "b//"), 1);
}

/// A fake S3 bucket with a baseline and a plain key in each of `n` deltaspaces.
async fn s3_with_baselines(n: usize) -> (DynEngine, std::sync::Arc<crate::storage::FakeS3>) {
    let (engine, fake, endpoint) = fake_s3_engine().await;
    let mut keys: Vec<String> = (0..n).map(|i| format!("d{i:05}/reference.bin")).collect();
    keys.extend((0..n).map(|i| format!("d{i:05}/k.txt")));
    put_raw_many(&endpoint, keys).await;
    fake.clear();
    (engine, fake)
}

/// The baselines are read when their page is done and kept nowhere: the scan
/// holds one page of them, however many deltaspaces the bucket has. (The
/// whole set used to be held until the pass ended.)
#[tokio::test]
async fn an_s3_scan_holds_at_most_one_page_of_baselines() {
    let n = 2300;
    let (engine, fake) = s3_with_baselines(n).await;
    let scan = engine.plaintext_scan("b", ScanScope::Whole).await.unwrap();
    assert!(scan.complete, "{scan:?}");
    assert_eq!(reference_heads(&fake), n, "every baseline was read");
    assert!(scan.peak_baselines > 0);
    assert!(
        scan.peak_baselines <= u64::from(crate::maintenance::worker::PAGE_SIZE),
        "held {} baseline names at once",
        scan.peak_baselines
    );
}

/// A filesystem bucket reports no baselines in its pages: the scan streams
/// them in batches, so a bucket with a deltaspace for each object is bounded too.
#[tokio::test]
async fn a_filesystem_scan_streams_its_baselines_in_batches() {
    let dir = tempfile::tempdir().unwrap();
    let fs = FilesystemBackend::new(dir.path().to_path_buf())
        .await
        .unwrap();
    fs.create_bucket("b").await.unwrap();
    let body = bytes::Bytes::from_static(b"the baseline");
    let n = 2 * super::plaintext_scan::BASELINE_BATCH + 10;
    for i in 0..n {
        let meta = crate::types::FileMetadata::new_reference(
            "reference.bin".into(),
            "reference.bin".into(),
            "0".repeat(64),
            "0".repeat(32),
            body.len() as u64,
            None,
        );
        fs.put_reference(
            "b",
            &format!("g{}/f{i}", i % 7),
            body.clone(),
            &meta,
            crate::deltaglider::RefWriteProof::for_tests(),
        )
        .await
        .unwrap();
    }
    let streamed: std::collections::BTreeSet<String> = {
        use futures::TryStreamExt;
        fs.list_reference_prefix_stream("b", "")
            .await
            .unwrap()
            .try_collect()
            .await
            .unwrap()
    };
    let listed: std::collections::BTreeSet<String> = fs
        .list_reference_prefixes("b", "")
        .await
        .unwrap()
        .into_iter()
        .collect();
    assert_eq!(streamed.len(), n);
    assert_eq!(streamed, listed, "the walk names what the list names");
    let backend: Box<DynStorageBackend<'static>> = DynStorageBackend::new_box(fs);
    let engine = DeltaGliderEngine::new_with_backend(Arc::new(backend), &Config::default(), None);
    let scan = engine.plaintext_scan("b", ScanScope::Whole).await.unwrap();
    assert!(scan.complete, "{scan:?}");
    assert!(
        scan.peak_baselines > 0
            && scan.peak_baselines <= super::plaintext_scan::BASELINE_BATCH as u64,
        "held {} baseline names at once",
        scan.peak_baselines
    );
}
