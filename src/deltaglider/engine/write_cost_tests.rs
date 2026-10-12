// SPDX-License-Identifier: BUSL-1.1

//! What a PUT costs in backend requests, on a fake S3 with a usage counter
//! (the counter is what makes a PUT read the key it replaces).

use super::*;
use crate::bucket_usage::BucketUsage;
use crate::storage::{DynStorageBackend, FakeS3};

/// An engine on a fake S3 with bucket `b` and a usage counter.
pub(super) async fn counted_s3_engine() -> (Arc<DynEngine>, Arc<FakeS3>) {
    let (engine, fake, _) = counted_s3_engine_and_endpoint().await;
    (engine, fake)
}

/// [`counted_s3_engine`], and the fake's endpoint (for a peer engine).
async fn counted_s3_engine_and_endpoint() -> (Arc<DynEngine>, Arc<FakeS3>, String) {
    let (endpoint, fake) = crate::storage::fake_s3::start().await;
    let engine = s3_engine_at(&endpoint)
        .with_bucket_usage(Some(Arc::new(BucketUsage::in_memory().unwrap())));
    engine.create_bucket("b").await.unwrap();
    (Arc::new(engine), fake, endpoint)
}

/// An engine with no counter on the fake S3 at `endpoint`.
fn s3_engine_at(endpoint: &str) -> DynEngine {
    let s3 = crate::storage::s3_test_support::for_test_endpoint(endpoint);
    let backend: Box<DynStorageBackend<'static>> = DynStorageBackend::new_box(s3);
    DeltaGliderEngine::new_with_backend(Arc::new(backend), &Config::default(), None)
}

/// The object requests (not the background listing-facts upkeep under
/// `.dg/facts`), by method.
pub(super) fn count(fake: &FakeS3, method: &str) -> usize {
    object_requests(fake)
        .iter()
        .filter(|r| r.starts_with(&format!("{method} ")))
        .count()
}

/// Every request except the background listing-facts upkeep.
pub(super) fn object_requests(fake: &FakeS3) -> Vec<String> {
    fake.requests()
        .into_iter()
        .filter(|r| !r.contains(".dg/facts") && !r.contains("prefix=.dg"))
        .collect()
}

/// A folder marker (`dir/`) never has a `.delta` form: its prior read
/// HEADs the marker only. 3 requests before (2 HEADs + PUT), 2 now; a
/// marker HEAD 2 -> 1.
#[tokio::test]
async fn a_folder_marker_put_reads_one_form() {
    let (engine, fake) = counted_s3_engine().await;
    fake.clear();
    engine
        .store("b", "dir/", b"", None, Default::default())
        .await
        .unwrap();
    let reqs = object_requests(&fake);
    assert_eq!(count(&fake, "HEAD"), 1, "{reqs:#?}");
    assert_eq!(reqs.len(), 2, "{reqs:#?}");
    // The overwrite of a marker reads it as present (counted once).
    fake.clear();
    let again = engine
        .store("b", "dir/", b"", None, Default::default())
        .await
        .unwrap();
    assert!(again.replaced.is_some(), "the overwrite saw the marker");
    assert_eq!(object_requests(&fake).len(), 2);
    // HEAD of a marker shares the lookup: one request.
    fake.clear();
    engine.head("b", "dir/").await.unwrap();
    assert_eq!(object_requests(&fake).len(), 1);
}

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

fn no_delta() -> HashMap<String, String> {
    HashMap::from([("dg-no-delta".to_string(), "true".to_string())])
}

/// An overwrite in the same stored form sends no DELETE for the other
/// form: the prior read saw it absent. The DELETE also queued a
/// listing-facts cleanup. Passthrough overwrite 4 -> 3 requests (2 HEADs +
/// PUT); delta overwrite 5 -> 4 (+ the facts traffic each).
#[tokio::test]
async fn a_same_form_overwrite_deletes_nothing() {
    let (engine, fake) = counted_s3_engine().await;
    let base = noise(7, 64 * 1024);
    let mut v2 = base.clone();
    v2[5000..5100].fill(1);
    let mut v3 = base.clone();
    v3[9000..9100].fill(2);

    engine
        .store("b", "img/a.jpg", &base, None, HashMap::new())
        .await
        .unwrap();
    fake.clear();
    engine
        .store("b", "img/a.jpg", &v2, None, HashMap::new())
        .await
        .unwrap();
    let reqs = object_requests(&fake);
    assert_eq!(count(&fake, "DELETE"), 0, "passthrough: {reqs:#?}");
    assert_eq!(reqs.len(), 3, "passthrough overwrite: {reqs:#?}");

    engine
        .store("b", "rel/a.zip", &base, None, HashMap::new())
        .await
        .unwrap();
    engine
        .store("b", "rel/b.zip", &v2, None, HashMap::new())
        .await
        .unwrap();
    fake.clear();
    let r = engine
        .store("b", "rel/b.zip", &v3, None, HashMap::new())
        .await
        .unwrap();
    assert_eq!(r.metadata.storage_info.label(), "delta");
    let reqs = object_requests(&fake);
    assert_eq!(count(&fake, "DELETE"), 0, "delta: {reqs:#?}");
    assert_eq!(reqs.len(), 4, "delta overwrite: {reqs:#?}");
}

/// An overwrite in the OTHER stored form still drops the old form, and
/// an unknown prior (a failed read) still sends the DELETE.
#[tokio::test]
async fn an_other_form_overwrite_or_an_unknown_prior_still_deletes() {
    let (engine, fake) = counted_s3_engine().await;
    let base = noise(3, 64 * 1024);
    engine
        .store("b", "rel/k.zip", &base, None, no_delta())
        .await
        .unwrap();
    fake.clear();
    let r = engine
        .store("b", "rel/k.zip", &base, None, HashMap::new())
        .await
        .unwrap();
    assert_eq!(r.metadata.storage_info.label(), "delta");
    assert!(
        object_requests(&fake)
            .iter()
            .any(|r| r.starts_with("DELETE /b/rel/k.zip?")),
        "{:#?}",
        object_requests(&fake)
    );

    // A prior read that fails: the write deletes as before.
    engine
        .store("b", "img/u.jpg", b"one", None, HashMap::new())
        .await
        .unwrap();
    fake.fail("HEAD", "u.jpg", 503, "SlowDown", u32::MAX);
    fake.clear();
    engine
        .store("b", "img/u.jpg", b"two", None, HashMap::new())
        .await
        .unwrap();
    fake.clear_faults();
    assert_eq!(count(&fake, "DELETE"), 1, "{:#?}", object_requests(&fake));
}

/// The fast-path delta write (`put_delta_raw`) drops the passthrough form
/// through the same helper: no DELETE when the prior read found the key
/// absent; a DELETE when the prior is unknown (the heal path).
#[tokio::test]
async fn a_raw_delta_write_drops_the_other_form_through_the_helper() {
    let (engine, fake) = counted_s3_engine().await;
    let keys = crate::deltaglider::store_deltas(&engine, "run", 1).await;
    let (_, filename) = keys[0].rsplit_once('/').unwrap();
    let bytes = engine.get_delta_raw("b", "run", filename).await.unwrap();
    let meta = engine.delta_meta("b", "run", filename).await.unwrap();
    for (prior, deletes) in [(CounterPrior::Absent, 0), (CounterPrior::Unknown, 1)] {
        fake.clear();
        let res: Result<(), EngineError> = engine
            .with_dest_prefix_lock("b", "run", || async {
                Ok(engine
                    .put_delta_raw(
                        "b",
                        "run",
                        "copy.zip",
                        bytes::Bytes::copy_from_slice(&bytes),
                        &meta,
                        &prior,
                    )
                    .await?)
            })
            .await;
        res.unwrap();
        assert_eq!(
            count(&fake, "DELETE"),
            deletes,
            "{prior:?}: {:#?}",
            object_requests(&fake)
        );
    }
}

/// `reference_meta` reads reference.bin with one HEAD (it sent two:
/// `has_reference`, then the metadata).
#[tokio::test]
async fn reference_meta_heads_once() {
    let (engine, fake) = counted_s3_engine().await;
    crate::deltaglider::store_deltas(&engine, "run", 1).await;
    for (prefix, found) in [("run", true), ("none", false)] {
        fake.clear();
        assert_eq!(
            engine.reference_meta("b", prefix).await.unwrap().is_some(),
            found
        );
        assert_eq!(count(&fake, "HEAD"), 1, "{:#?}", object_requests(&fake));
    }
}

/// The race documented on `drop_other_form`: a peer (another instance, a
/// writer without this process's object write lock) adds the other form
/// after our prior read and before our write. Our write skips that form's
/// DELETE (the read saw it absent), so both forms stay. A read resolves
/// the newer (ours); the key's next write reads both forms and drops the
/// older one.
#[tokio::test]
async fn a_form_a_peer_adds_after_the_read_is_dropped_by_the_next_write() {
    let (engine, fake, endpoint) = counted_s3_engine_and_endpoint().await;
    // A peer engine on the same storage, with no counter (no prior read).
    let peer = s3_engine_at(&endpoint);
    let key = "rel/app.zip";
    let ours = noise(11, 64 * 1024);
    engine
        .store("b", key, &ours, None, no_delta())
        .await
        .unwrap();

    // Our read: the key holds the passthrough form only.
    let prior = engine.prior_for_counter("b", key).await;
    assert!(!prior.may_hold(crate::storage::ObjectVariant::Delta));
    // The peer writes the delta form now (and drops our passthrough).
    let theirs = noise(12, 64 * 1024);
    let stored = peer
        .store("b", key, &theirs, None, HashMap::new())
        .await
        .unwrap();
    assert_eq!(stored.metadata.storage_info.label(), "delta");
    // Our passthrough write, with the read from before the peer's write.
    fake.clear();
    engine
        .store_inner(
            "b",
            key,
            &Bytes::from(ours.clone()),
            None,
            no_delta(),
            None,
            Some(prior),
            None,
        )
        .await
        .unwrap();
    assert_eq!(count(&fake, "DELETE"), 0, "{:#?}", object_requests(&fake));

    // Both forms exist; GET resolves the newer one: ours.
    let (_, forms) = engine.head_with_forms("b", key).await.unwrap();
    assert!(forms.delta && forms.passthrough, "{forms:?}");
    let head = engine.head("b", key).await.unwrap();
    assert_eq!(head.storage_info.label(), "passthrough");
    assert_eq!(head.file_sha256, hex::encode(Sha256::digest(&ours)));

    // The next write reads both forms and drops the stale delta.
    fake.clear();
    engine
        .store("b", key, &ours, None, no_delta())
        .await
        .unwrap();
    assert!(
        object_requests(&fake)
            .iter()
            .any(|r| r.starts_with("DELETE /b/rel/app.zip.delta?")),
        "{:#?}",
        object_requests(&fake)
    );
    let (_, forms) = engine.head_with_forms("b", key).await.unwrap();
    assert!(!forms.delta && forms.passthrough, "{forms:?}");
}

/// A cross-instance lock that always grants, with no requests of its own.
struct GrantingLock;

impl crate::coordination::ReferenceLock for GrantingLock {
    async fn try_acquire(
        &self,
        _: &str,
        _: &str,
        _: i64,
    ) -> Result<bool, crate::coordination::LeaseError> {
        Ok(true)
    }
    async fn release(&self, _: &str, _: &str) -> Result<(), crate::coordination::LeaseError> {
        Ok(())
    }
    async fn renew(&self, _: &str, _: &str, _: i64) -> Result<(), crate::coordination::LeaseError> {
        Ok(())
    }
    fn ttl_secs(&self) -> i64 {
        120
    }
    fn renew_interval(&self) -> Duration {
        Duration::from_secs(30)
    }
    fn acquire_timeout(&self) -> Duration {
        Duration::from_secs(1)
    }
}

/// Under the cross-instance lock, one HEAD of reference.bin gives the
/// fence and the metadata that the PUT encodes against. It sent two (the
/// fence, then the metadata). The third HEAD is the fence check after the
/// delta write. Delta PUT into a folder with a baseline: 3 -> 2 HEADs of
/// reference.bin.
#[tokio::test]
async fn a_locked_delta_put_heads_the_reference_for_fence_and_metadata_once() {
    let (endpoint, fake) = crate::storage::fake_s3::start().await;
    let engine = s3_engine_at(&endpoint).with_reference_lock(Some(
        crate::coordination::DynReferenceLock::from_arc(Arc::new(GrantingLock)),
    ));
    engine.create_bucket("b").await.unwrap();
    crate::deltaglider::store_deltas(&engine, "run", 1).await;
    fake.clear();
    crate::deltaglider::store_deltas(&engine, "run", 2).await;
    // The second call stores build-000 again and build-001: two PUTs.
    let heads = fake
        .requests()
        .iter()
        .filter(|r| r.starts_with("HEAD /b/run/reference.bin"))
        .count();
    assert_eq!(heads, 4, "two locked delta PUTs: {:#?}", fake.requests());
}

/// Fix the class: a buffered PUT body is one `Bytes` from the S3 adapter
/// to the storage write. The encode copied it (`data.to_vec()`), the S3
/// PUT copied it per attempt (`ByteStream::from(data.to_vec())`), and the
/// first delta copied it into the reference cache
/// (`Bytes::copy_from_slice`): up to 3 copies of a body of up to
/// `max_object_size`. The storage write methods take `Bytes`, so a copy
/// can come back only as an explicit one here.
#[test]
fn buffered_put_bodies_are_not_copied() {
    let mut sites = Vec::new();
    for file in [
        "src/deltaglider/engine/store.rs",
        "src/deltaglider/engine/conditional.rs",
        "src/storage/s3/objects.rs",
    ] {
        let text = std::fs::read_to_string(file).unwrap();
        let lines = crate::source_scan::prod_lines(&text);
        for (i, (n, line)) in lines.iter().enumerate() {
            // The slice API (`store`, `store_with_multipart_etag`) copies
            // once at its entry, and says so on the line above.
            if i > 0 && lines[i - 1].1.contains("One copy: the slice API") {
                continue;
            }
            if line.contains("data.to_vec()")
                || line.contains("Bytes::copy_from_slice(data)")
                || line.contains("ByteStream::from(data.to_vec())")
            {
                sites.push(format!("{file}:{n}: {}", line.trim()));
            }
        }
    }
    assert!(sites.is_empty(), "body copies: {sites:#?}");
}

/// The first delta PUT into a folder makes its body the baseline: the
/// reference cache holds the client's buffer, not a copy of it.
#[tokio::test]
async fn the_baseline_in_the_reference_cache_is_the_client_buffer() {
    let (engine, _fake) = counted_s3_engine().await;
    let body = Bytes::from(noise(21, 64 * 1024));
    let r = engine
        .store_conditional(
            "b",
            "rel/a.zip",
            body.clone(),
            None,
            None,
            HashMap::new(),
            &crate::deltaglider::Precondition::none(),
        )
        .await
        .unwrap();
    assert_eq!(r.metadata.storage_info.label(), "delta");
    let cached = engine
        .cache
        .get(&engine.cache_key("b", "rel"))
        .expect("the baseline is cached");
    assert_eq!(cached.as_ptr(), body.as_ptr(), "the cache copied the body");
}

/// A client body above the spool threshold is in RAM already: the store
/// hashes it from RAM while it writes the spool, and never reads the spool
/// back to hash it (it read the whole body back, once per PUT).
#[tokio::test]
async fn a_large_client_put_hashes_from_ram_not_from_the_spool() {
    let (endpoint, _fake) = crate::storage::fake_s3::start().await;
    let mut config = Config::default();
    config.tuning.spool_threshold_bytes = Some(1024);
    let s3 = crate::storage::s3_test_support::for_test_endpoint(&endpoint);
    let backend: Box<DynStorageBackend<'static>> = DynStorageBackend::new_box(s3);
    let engine = DeltaGliderEngine::new_with_backend(Arc::new(backend), &config, None)
        .with_bucket_usage(Some(Arc::new(BucketUsage::in_memory().unwrap())));
    engine.create_bucket("b").await.unwrap();
    super::store::SPOOL_HASH_READS.with(|n| n.set(0));
    let base = noise(31, 64 * 1024);
    let mut v2 = base.clone();
    v2[100..200].fill(9);
    for (key, body) in [("rel/a.zip", &base), ("rel/b.zip", &v2)] {
        let r = engine
            .store_conditional(
                "b",
                key,
                Bytes::from(body.clone()),
                None,
                None,
                HashMap::new(),
                &crate::deltaglider::Precondition::none(),
            )
            .await
            .unwrap();
        assert_eq!(r.metadata.storage_info.label(), "delta", "{key}");
        assert_eq!(
            r.metadata.file_sha256,
            hex::encode(Sha256::digest(body)),
            "{key}"
        );
        assert_eq!(r.metadata.file_size, body.len() as u64, "{key}");
    }
    assert_eq!(
        super::store::SPOOL_HASH_READS.with(|n| n.get()),
        0,
        "the spool was read back to hash"
    );
}

/// A client body whose SHA-256 the S3 adapter proved (s3s verified the
/// signed `x-amz-content-sha256` while the body streamed in) is not hashed
/// again for it: the engine stores the digest it was given. The test gives
/// the digest of OTHER bytes, so a re-hash would show. Buffered and spooled
/// stores, passthrough and delta-eligible.
#[tokio::test]
async fn a_proven_sha256_is_stored_not_recomputed() {
    let (endpoint, _fake) = crate::storage::fake_s3::start().await;
    let mut config = Config::default();
    config.tuning.spool_threshold_bytes = Some(1024);
    let s3 = crate::storage::s3_test_support::for_test_endpoint(&endpoint);
    let backend: Box<DynStorageBackend<'static>> = DynStorageBackend::new_box(s3);
    let engine = DeltaGliderEngine::new_with_backend(Arc::new(backend), &config, None);
    engine.create_bucket("b").await.unwrap();
    let proven = crate::deltaglider::KnownSha256::of(b"not the body");
    for (key, size) in [
        ("small/a.jpg", 512),             // buffered passthrough
        ("small/a.zip", 512),             // buffered, delta-eligible
        ("large/a.zip", 64 * 1024),       // spooled, delta-eligible
        ("large/a.jpg", 2 * 1024 * 1024), // above the blocking-hash threshold
    ] {
        let r = engine
            .store_conditional(
                "b",
                key,
                Bytes::from(noise(41, size)),
                Some(proven),
                None,
                HashMap::new(),
                &crate::deltaglider::Precondition::none(),
            )
            .await
            .unwrap();
        assert_eq!(r.metadata.file_sha256, proven.to_hex(), "{key}");
        // The MD5 (the ETag of a single-part PUT) is still the body's.
        assert_eq!(
            r.metadata.md5,
            hex::encode(md5::Md5::digest(noise(41, size))),
            "{key}"
        );
    }
}

/// An engine on the fake S3 at `endpoint` whose PUTs above 1 KiB take the
/// streaming (spooled) path.
fn spooling_engine_at(endpoint: &str) -> DynEngine {
    let mut config = Config::default();
    config.tuning.spool_threshold_bytes = Some(1024);
    let s3 = crate::storage::s3_test_support::for_test_endpoint(endpoint);
    let backend: Box<DynStorageBackend<'static>> = DynStorageBackend::new_box(s3);
    DeltaGliderEngine::new_with_backend(Arc::new(backend), &config, None)
}

/// The reference downloads of a run (GETs of `reference.bin`).
fn reference_gets(fake: &FakeS3) -> usize {
    fake.requests()
        .iter()
        .filter(|r| r.starts_with("GET ") && r.contains("reference.bin"))
        .count()
}

async fn put_large(engine: &DynEngine, key: &str, body: &[u8]) -> StoreResult {
    engine
        .store_conditional(
            "b",
            key,
            Bytes::from(body.to_vec()),
            None,
            None,
            HashMap::new(),
            &crate::deltaglider::Precondition::none(),
        )
        .await
        .unwrap()
}

/// Variant `n` of a 64 KiB base: a delta of it.
fn variant(base: &[u8], n: u8) -> Vec<u8> {
    let mut v = base.to_vec();
    v[100 * n as usize..100 * n as usize + 50].fill(n);
    v
}

/// P2.7: streaming delta PUTs into one folder share ONE verified download
/// of the reference (the GET path's `range_spools` entry). Each PUT
/// downloaded the whole reference.bin: 2 reference GETs for 2 PUTs, now 1.
#[tokio::test]
async fn back_to_back_large_puts_download_the_reference_once() {
    let (endpoint, fake) = crate::storage::fake_s3::start().await;
    let base = noise(51, 64 * 1024);
    let seeder = spooling_engine_at(&endpoint);
    seeder.create_bucket("b").await.unwrap();
    put_large(&seeder, "rel/a.zip", &base).await;
    // Another engine (another node, or after the cache TTL): nothing cached.
    let engine = spooling_engine_at(&endpoint);
    fake.clear();
    for n in 1..=2 {
        let r = put_large(&engine, &format!("rel/v{n}.zip"), &variant(&base, n)).await;
        assert_eq!(r.metadata.storage_info.label(), "delta");
    }
    assert_eq!(reference_gets(&fake), 1, "{:#?}", fake.requests());
    // The deltas decode against the reference they were encoded with.
    for n in 1..=2u8 {
        let (got, _) = engine
            .retrieve("b", &format!("rel/v{n}.zip"))
            .await
            .unwrap();
        assert_eq!(got, variant(&base, n));
    }
}

/// P2.7: the PUT that creates a reference keeps its body as the folder's
/// verified reference file, so the next large PUT downloads nothing: 1
/// reference GET before, 0 now.
#[tokio::test]
async fn the_put_that_creates_the_reference_seeds_the_spool_cache() {
    let (endpoint, fake) = crate::storage::fake_s3::start().await;
    let engine = spooling_engine_at(&endpoint);
    engine.create_bucket("b").await.unwrap();
    let base = noise(52, 64 * 1024);
    put_large(&engine, "rel/a.zip", &base).await;
    fake.clear();
    let r = put_large(&engine, "rel/v1.zip", &variant(&base, 1)).await;
    assert_eq!(r.metadata.storage_info.label(), "delta");
    assert_eq!(reference_gets(&fake), 0, "{:#?}", fake.requests());
}

/// P2.7 edge case: a reference whose bytes no longer match its sha256 is
/// not encoded against. The large PUT fails (500, as a GET of a delta of
/// it does) and stores nothing over the corrupt baseline.
#[tokio::test]
async fn a_large_put_refuses_a_corrupt_reference() {
    let (endpoint, fake) = crate::storage::fake_s3::start().await;
    let seeder = spooling_engine_at(&endpoint);
    seeder.create_bucket("b").await.unwrap();
    let base = noise(53, 64 * 1024);
    put_large(&seeder, "rel/a.zip", &base).await;
    let ref_path = fake
        .requests()
        .iter()
        .find(|r| r.starts_with("PUT ") && r.contains("reference.bin"))
        .map(|r| {
            let path = r.split_whitespace().nth(1).unwrap();
            path.split('?')
                .next()
                .unwrap()
                .trim_start_matches('/')
                .to_string()
        })
        .expect("the first PUT wrote the reference");
    fake.replace_body(&ref_path, &noise(54, 64 * 1024));
    let engine = spooling_engine_at(&endpoint);
    let err = engine
        .store_conditional(
            "b",
            "rel/v1.zip",
            Bytes::from(variant(&base, 1)),
            None,
            None,
            HashMap::new(),
            &crate::deltaglider::Precondition::none(),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            crate::deltaglider::ConditionalError::Engine(EngineError::ChecksumMismatch { .. })
        ),
        "{err:?}"
    );
    assert!(
        engine.head("b", "rel/v1.zip").await.is_err(),
        "nothing stored"
    );
}
