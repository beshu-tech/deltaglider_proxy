// SPDX-License-Identifier: BUSL-1.1

//! The maintenance worker on a fake S3 that counts requests (jobs.md 13
//! and 17).

use super::*;
use crate::deltaglider::s3_engine;
use crate::storage::FakeS3;

/// A `created_at` no other test job has: the transient-stop count of a
/// job is process-global and matched on (id, kind, bucket, created_at),
/// and every in-memory DB starts its ids at 1.
fn unique_created_at() -> i64 {
    static NEXT: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(1_000);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

struct Env {
    state: Arc<AppState>,
    fake: Arc<FakeS3>,
    db: Arc<Mutex<ConfigDb>>,
    config: SharedConfig,
    mutator: ConfigMutator,
    _dir: tempfile::TempDir,
}

/// A re-encrypt job on bucket `b` holding `objects` plain objects (the
/// job reads each one and rewrites none).
async fn env(objects: usize) -> Env {
    let (engine, fake) = s3_engine().await;
    for i in 0..objects {
        engine
            .store(
                "b",
                &format!("k-{i:03}.txt"),
                b"x",
                None,
                Default::default(),
            )
            .await
            .unwrap();
    }
    let dir = tempfile::tempdir().unwrap();
    let config: SharedConfig = crate::config::Config::default().into_shared();
    let state = AppState::for_tests(engine);
    let mutator = ConfigMutator {
        config: config.clone(),
        app: state.clone(),
        persist_path: dir.path().join("c.yaml").display().to_string(),
    };
    let db = ConfigDb::in_memory("k").unwrap();
    db.maintenance_create_job(
        "reencrypt",
        "b",
        "counting",
        None,
        "admin",
        unique_created_at(),
    )
    .unwrap()
    .unwrap();
    fake.clear();
    Env {
        state,
        fake,
        db: Arc::new(Mutex::new(db)),
        config,
        mutator,
        _dir: dir,
    }
}

impl Env {
    async fn claim(&self) -> Option<MaintenanceJob> {
        self.db
            .lock()
            .await
            .maintenance_claim_next_job("inst", current_unix_seconds(), 60)
            .unwrap()
    }

    async fn run(&self, job: MaintenanceJob) {
        run_job(
            &self.mutator,
            &self.config,
            &self.db,
            &self.state,
            Holder::test("inst"),
            job,
        )
        .await;
    }
}

/// jobs-17: a maintenance job read and rewrote one object at a time, about
/// 12 round trips each; a 100,000-object bucket took days. A page's objects
/// run several at a time.
#[tokio::test]
async fn a_page_reads_several_objects_at_once() {
    let e = env(16).await;
    e.fake.set_delay_ms("HEAD", 30);
    let job = e.claim().await.unwrap();
    let id = job.id;
    e.run(job).await;
    let row =
        e.db.lock()
            .await
            .maintenance_job_by_id(id)
            .unwrap()
            .unwrap();
    assert_eq!(row.status, "completed", "{row:?}");
    assert_eq!(row.objects_skipped, 16, "{row:?}");
    // One object sends one HEAD (the form that the listing proved): one
    // object at a time peaks at 1.
    assert!(
        e.fake.peak_in_flight("HEAD") >= super::super::paged::PAGE_CONCURRENCY,
        "peak {} HEAD requests in flight",
        e.fake.peak_in_flight("HEAD")
    );
}

/// jobs-13: a job whose bucket's backend the health check closed sent its
/// whole volume to that backend (every request waiting out its timeout).
/// It waits instead: the row goes back to `queued` with the cause, no
/// worker claims it for a while, and no request reaches the backend.
#[tokio::test]
async fn a_job_on_a_gated_backend_waits_and_sends_nothing() {
    use crate::coordination::health::HealthVerdict;
    let e = env(3).await;
    {
        let cfg = e.config.read().await;
        let (name, def) = cfg.effective_backend_for_bucket("b").unwrap();
        e.state.backend_health.set(
            &name,
            def,
            HealthVerdict::Unreachable {
                detail: "connection refused".into(),
            },
        );
    }
    let job = e.claim().await.unwrap();
    let id = job.id;
    e.run(job).await;
    assert!(e.fake.requests().is_empty(), "{:#?}", e.fake.requests());
    let row =
        e.db.lock()
            .await
            .maintenance_job_by_id(id)
            .unwrap()
            .unwrap();
    assert_eq!(row.status, "queued", "{row:?}");
    assert!(
        row.last_error
            .as_deref()
            .unwrap_or("")
            .contains("unavailable"),
        "{row:?}"
    );
    assert!(e.claim().await.is_none(), "claimed again at once");
    assert!(
        e.state.maintenance_gate.is_busy("b") || {
            e.state.maintenance_gate.sync_from(&*e.db.lock().await);
            e.state.maintenance_gate.is_busy("b")
        },
        "the waiting job's write gate is open"
    );
}

/// blast-5 / jobs-14: a listing that kept timing out after the in-place
/// retries failed the job for good, and a migrate threw its copies away.
/// The job goes back to the queue with its cursor instead; it fails only
/// after [`TRANSIENT_RETRIES`] stops without progress in between.
#[tokio::test]
async fn a_job_that_a_timeout_stops_goes_back_to_the_queue() {
    let e = env(3).await;
    e.fake
        .fail("LIST", "list-type=2", 503, "SlowDown", u32::MAX);
    let job = e.claim().await.unwrap();
    let id = job.id;
    e.run(job).await;
    let row =
        e.db.lock()
            .await
            .maintenance_job_by_id(id)
            .unwrap()
            .unwrap();
    assert_eq!(row.status, "queued", "{row:?}");
    assert!(
        row.last_error
            .as_deref()
            .unwrap_or("")
            .contains("retry 1 of"),
        "{row:?}"
    );
    assert!(e.claim().await.is_none(), "claimed again at once");

    // The retries are used up: the next stop settles the job.
    set_transient_stops(&row, TRANSIENT_RETRIES);
    let later = current_unix_seconds() + 100_000;
    let job =
        e.db.lock()
            .await
            .maintenance_claim_next_job("inst", later, 60)
            .unwrap()
            .expect("claimable once the wait is over");
    e.run(job).await;
    let row =
        e.db.lock()
            .await
            .maintenance_job_by_id(id)
            .unwrap()
            .unwrap();
    assert_eq!(row.status, "failed", "{row:?}");
}

/// The requests for one object (`/{bucket}/{key}` and its `.delta` form),
/// by operation: `HEAD`, `GET`, `PUT`, `COPY` (CopyObject), `ACL` (object
/// ACL), `DELETE`. Listings are not object requests.
pub(crate) fn object_ops(
    fake: &FakeS3,
    bucket: &str,
    key: &str,
) -> std::collections::BTreeMap<String, usize> {
    let path = format!("/{bucket}/{key}");
    let mut ops = std::collections::BTreeMap::new();
    for r in fake.requests() {
        let (method, uri) = r.split_once(' ').unwrap_or((&r, ""));
        let Some(rest) = uri.strip_prefix(&path) else {
            continue;
        };
        if !(rest.is_empty() || rest.starts_with('?') || rest.starts_with(".delta")) {
            continue;
        }
        let op = if uri.contains("?acl") || uri.contains("&acl") {
            "ACL"
        } else if method == "PUT" && uri.contains("x-id=CopyObject") {
            "COPY"
        } else {
            method
        };
        *ops.entry(op.to_string()).or_insert(0) += 1;
    }
    ops
}

/// The total of [`object_ops`].
pub(crate) fn object_requests(fake: &FakeS3, bucket: &str, key: &str) -> usize {
    object_ops(fake, bucket, key).values().sum()
}

/// A job of `kind` on bucket `b` of `engine`, claimed and run to its end.
/// Returns the row and the fake's requests during the run.
async fn run_kind(
    engine: crate::deltaglider::DynEngine,
    fake: &FakeS3,
    kind: &str,
) -> MaintenanceJob {
    let dir = tempfile::tempdir().unwrap();
    let config: SharedConfig = crate::config::Config::default().into_shared();
    let state = AppState::for_tests(engine);
    let mutator = ConfigMutator {
        config: config.clone(),
        app: state.clone(),
        persist_path: dir.path().join("c.yaml").display().to_string(),
    };
    let db = ConfigDb::in_memory("k").unwrap();
    db.maintenance_create_job(kind, "b", "counting", None, "admin", unique_created_at())
        .unwrap()
        .unwrap();
    let db = Arc::new(Mutex::new(db));
    let job = db
        .lock()
        .await
        .maintenance_claim_next_job("inst", current_unix_seconds(), 60)
        .unwrap()
        .unwrap();
    let id = job.id;
    fake.clear();
    run_job(&mutator, &config, &db, &state, Holder::test("inst"), job).await;
    let row = db.lock().await.maintenance_job_by_id(id).unwrap().unwrap();
    row
}

/// An engine on a fake S3 with the usage counter on, as in production.
async fn counted_engine() -> (crate::deltaglider::DynEngine, Arc<FakeS3>) {
    let (engine, fake) = s3_engine().await;
    let engine = engine.with_bucket_usage(Some(Arc::new(
        crate::bucket_usage::BucketUsage::in_memory().unwrap(),
    )));
    (engine, fake)
}

/// P6.1: the listing of the job already saw which stored forms of each
/// object exist. A re-encrypt that rewrites nothing HEADed both forms of
/// every object (2 requests); it HEADs the one listed form.
#[tokio::test]
async fn reencrypt_heads_only_the_listed_form() {
    let e = env(5).await;
    let job = e.claim().await.unwrap();
    let id = job.id;
    e.run(job).await;
    let row =
        e.db.lock()
            .await
            .maintenance_job_by_id(id)
            .unwrap()
            .unwrap();
    assert_eq!(row.status, "completed", "{row:?}");
    assert_eq!(row.objects_skipped, 5, "{row:?}");
    for i in 0..5 {
        let key = format!("k-{i:03}.txt");
        let ops = object_ops(&e.fake, "b", &key);
        assert_eq!(object_requests(&e.fake, "b", &key), 1, "{key}: {ops:?}");
    }
}

/// P6.1 edge: a key that the listing saw in both stored forms (a crash
/// between a write and its other-form DELETE) is HEADed in both, so the
/// newer form wins as on a client HEAD.
#[tokio::test]
async fn a_key_listed_in_both_forms_is_headed_in_both() {
    let (engine, fake) = counted_engine().await;
    let keys = crate::deltaglider::store_deltas(&engine, "d", 2).await;
    fake.insert_object(&format!("b/{}", keys[1]), b"leftover", &[]);
    engine.metadata_cache().invalidate_prefix("b", "");
    let row = run_kind(engine, &fake, "reencrypt").await;
    assert_eq!(row.status, "completed", "{row:?}");
    assert_eq!(row.objects_failed, 0, "{row:?}");
    let ops = |k: &str| object_ops(&fake, "b", k);
    assert_eq!(ops(&keys[0]).get("HEAD"), Some(&1), "{:?}", ops(&keys[0]));
    assert_eq!(ops(&keys[1]).get("HEAD"), Some(&2), "{:?}", ops(&keys[1]));
}

/// P6.1 edge: a listed form that is gone when the job HEADs it (changed
/// outside the proxy) is read in both forms, as before.
#[tokio::test]
async fn a_listed_form_that_is_gone_is_read_in_both_forms() {
    let (engine, fake) = counted_engine().await;
    engine
        .store("b", "k.txt", b"x", None, Default::default())
        .await
        .unwrap();
    let page = engine.list_job_page("b", 1000, None).await.unwrap();
    let form = page.forms.of("k.txt");
    assert_eq!(form, Some(crate::storage::ObjectVariant::Passthrough));
    // Gone outside the proxy.
    engine
        .storage()
        .delete_passthrough("b", "", "k.txt")
        .await
        .unwrap();
    fake.clear();
    let err = engine.head_listed("b", "k.txt", form).await.unwrap_err();
    assert!(err.is_not_found(), "{err}");
    // The listed form, then both.
    assert_eq!(object_ops(&fake, "b", "k.txt").get("HEAD"), Some(&3));
}

/// Store `n` objects `k-NNN.txt` that carry a stale "encrypted" marker on
/// the plain backend: a re-encrypt rewrites each one.
async fn store_marked(engine: &crate::deltaglider::DynEngine, n: usize) {
    for i in 0..n {
        let meta = std::collections::HashMap::from([(
            ENCRYPTION_MARKER_KEY.to_string(),
            crate::storage::encrypting::ENCRYPTION_MARKER_VALUE.to_string(),
        )]);
        engine
            .store("b", &format!("k-{i:03}.txt"), b"x", None, meta)
            .await
            .unwrap();
    }
    engine.metadata_cache().invalidate_prefix("b", "");
}

/// P6.1 + P6.2: a re-encrypt that rewrites an object HEADed both forms,
/// then the copy HEADed them again, the store read the prior (2 more),
/// and the write DELETEd the other form that the job had seen absent.
/// Now the job's one HEAD is the copy's source and the store's prior.
#[tokio::test]
async fn reencrypt_rewrite_reuses_the_jobs_head() {
    let (engine, fake) = counted_engine().await;
    store_marked(&engine, 5).await;
    let row = run_kind(engine, &fake, "reencrypt").await;
    assert_eq!(row.status, "completed", "{row:?}");
    assert_eq!(row.objects_done, 5, "{row:?}");
    for i in 0..5 {
        let key = format!("k-{i:03}.txt");
        let ops = object_ops(&fake, "b", &key);
        // HEAD (the listed form), GET (the bytes), PUT (the rewrite), and
        // the copy's verify of the destination (a HEAD of both forms).
        assert_eq!(object_requests(&fake, "b", &key), 5, "{key}: {ops:?}");
        assert_eq!(ops.get("DELETE"), None, "{key}: {ops:?}");
    }
}

/// P6.2: with the job's HEAD as the prior, the counter still nets each
/// rewrite (an overwrite of the same object) to zero.
#[tokio::test]
async fn a_rewrite_with_the_jobs_prior_keeps_the_counter() {
    let (engine, fake) = s3_engine().await;
    let usage = Arc::new(crate::bucket_usage::BucketUsage::in_memory().unwrap());
    let engine = engine.with_bucket_usage(Some(usage.clone()));
    store_marked(&engine, 3).await;
    let before = usage.read("b").unwrap().unwrap();
    assert_eq!(before.object_count, 3, "{before:?}");
    let row = run_kind(engine, &fake, "reencrypt").await;
    assert_eq!(row.objects_done, 3, "{row:?}");
    let after = usage.read("b").unwrap().unwrap();
    assert_eq!(after.object_count, 3, "{after:?}");
    assert_eq!(after.logical_bytes, before.logical_bytes, "{after:?}");
}

/// P6.2 edge: without a usage counter the write reads no prior and sent a
/// DELETE of the other form on every rewrite; the job's HEAD found it
/// absent, so the rewrite sends none.
#[tokio::test]
async fn a_rewrite_without_a_counter_sends_no_delete() {
    let (engine, fake) = s3_engine().await;
    store_marked(&engine, 3).await;
    let row = run_kind(engine, &fake, "reencrypt").await;
    assert_eq!(row.objects_done, 3, "{row:?}");
    for i in 0..3 {
        let key = format!("k-{i:03}.txt");
        let ops = object_ops(&fake, "b", &key);
        assert_eq!(ops.get("DELETE"), None, "{key}: {ops:?}");
    }
}

/// Source guard (P6.1, P6.2): a maintenance job reads an object with
/// `head_listed` (one HEAD of the form that its listing proved) and hands
/// that read to the copy (`copy_object_known`). A plain engine HEAD or a
/// copy that reads the source and the prior again sends what the job
/// already knows.
#[test]
fn maintenance_jobs_reuse_their_reads() {
    let mut sites = Vec::new();
    for (file, text) in crate::source_scan::prod_sources("src/maintenance") {
        // `#[path]` test modules (this file) are not prod code.
        if file.ends_with("_tests.rs") {
            continue;
        }
        for (n, line) in crate::source_scan::prod_lines(&text) {
            let banned = [".head(", "copy_object_with_retries(", "copy_object_from("];
            if banned.iter().any(|b| line.contains(b)) {
                sites.push(format!("{file}:{n}: {}", line.trim()));
            }
        }
    }
    assert!(sites.is_empty(), "{sites:#?}");
}

/// Store `n` foreign objects `f-NNN.bin` (no DG metadata) in `b`.
fn store_foreign(fake: &FakeS3, n: usize) {
    for i in 0..n {
        fake.insert_object(&format!("b/f-{i:03}.bin"), &[b'z'; 100], &[]);
    }
}

/// P6.1 + P6.3: a backfill of a foreign object HEADed both forms, then
/// GOT the bytes, then the self-copy HEADed the object again and read its
/// ACL before the copy (6 requests). The job's one HEAD is the self-copy's
/// read: HEAD, GET, ACL, COPY.
#[tokio::test]
async fn backfill_costs_get_and_fenced_copy_per_object() {
    let (engine, fake) = counted_engine().await;
    store_foreign(&fake, 5);
    let row = run_kind(engine, &fake, super::super::backfill::KIND).await;
    assert_eq!(row.status, "completed", "{row:?}");
    assert_eq!(row.objects_done, 5, "{row:?}");
    for i in 0..5 {
        let key = format!("f-{i:03}.bin");
        let ops = object_ops(&fake, "b", &key);
        assert_eq!(object_requests(&fake, "b", &key), 4, "{key}: {ops:?}");
        assert_eq!(ops.get("HEAD"), Some(&1), "{key}: {ops:?}");
        assert_eq!(ops.get("COPY"), Some(&1), "{key}: {ops:?}");
    }
}

/// P6.3: a backend without object ACLs answers the ACL read with 501;
/// each self-copy asked again (one refused request per object). The
/// refusal is kept per bucket: only the objects already in flight when
/// it came ask.
#[tokio::test]
async fn backfill_asks_an_acl_less_backend_once() {
    let (engine, fake) = counted_engine().await;
    fake.set_acl_unsupported(true);
    store_foreign(&fake, 12);
    let row = run_kind(engine, &fake, super::super::backfill::KIND).await;
    assert_eq!(row.status, "completed", "{row:?}");
    assert_eq!(row.objects_done, 12, "{row:?}");
    let acl: usize = (0..12)
        .map(|i| {
            object_ops(&fake, "b", &format!("f-{i:03}.bin"))
                .get("ACL")
                .copied()
                .unwrap_or(0)
        })
        .sum();
    assert!(
        acl <= super::super::paged::PAGE_CONCURRENCY,
        "{acl} ACL requests: {:#?}",
        fake.requests()
    );
}

/// P6.3: the self-copy is fenced on the ETag of the job's read: an object
/// that changed after the read keeps its content and metadata, and the
/// rewrite fails (the job records it per object).
#[tokio::test]
async fn a_rewrite_is_fenced_on_the_jobs_read() {
    use crate::storage::StorageBackend;
    let (engine, fake) = counted_engine().await;
    store_foreign(&fake, 1);
    let storage = engine.storage();
    let (meta, basis) = storage
        .get_passthrough_metadata_for_rewrite("b", "", "f-000.bin")
        .await
        .unwrap();
    fake.insert_object("b/f-000.bin", b"changed", &[("owner", "bob")]);
    fake.clear();
    let err = storage
        .put_passthrough_metadata("b", "", "f-000.bin", &meta, &basis)
        .await
        .unwrap_err();
    assert!(
        matches!(err, crate::storage::StorageError::Throttled(_)),
        "{err:?}"
    );
    // The read is the rewrite's: the copy is sent without a HEAD before
    // it (a lost copy HEADs once after, to tell its own write from a
    // peer's).
    let requests = fake.requests();
    let copy = requests
        .iter()
        .position(|r| r.contains("x-id=CopyObject"))
        .expect("a copy");
    assert!(
        !requests[..copy].iter().any(|r| r.starts_with("HEAD")),
        "{requests:#?}"
    );
    let now = storage
        .get_passthrough_metadata("b", "", "f-000.bin")
        .await
        .unwrap();
    assert_eq!(now.file_size, 7, "{now:?}");
}

/// The ListObjectsV2 requests of the user objects of `bucket` (not the
/// background listing-facts upkeep under `.dg/`).
pub(crate) fn bucket_lists(fake: &FakeS3, bucket: &str) -> usize {
    fake.requests()
        .iter()
        .filter(|r| r.starts_with(&format!("GET /{bucket}/?")) && r.contains("list-type=2"))
        .filter(|r| !crate::storage::fake_s3::list_prefix(r).contains(".dg"))
        .count()
}

/// P6.4: a re-encrypt listed the bucket three times (the count, the
/// objects, the reference prefixes). It lists it once.
#[tokio::test]
async fn reencrypt_lists_the_bucket_once() {
    let (engine, fake) = counted_engine().await;
    store_marked(&engine, 5).await;
    crate::deltaglider::store_deltas(&engine, "d", 3).await;
    engine.metadata_cache().invalidate_prefix("b", "");
    let row = run_kind(engine, &fake, "reencrypt").await;
    assert_eq!(row.status, "completed", "{row:?}");
    assert_eq!(row.objects_total, Some(8), "{row:?}");
    assert_eq!(bucket_lists(&fake, "b"), 1, "{:#?}", fake.requests());
}

/// P6.4: the same for a backfill.
#[tokio::test]
async fn backfill_lists_the_bucket_once() {
    let (engine, fake) = counted_engine().await;
    store_foreign(&fake, 5);
    let row = run_kind(engine, &fake, super::super::backfill::KIND).await;
    assert_eq!(row.status, "completed", "{row:?}");
    assert_eq!(row.objects_total, Some(5), "{row:?}");
    assert_eq!(bucket_lists(&fake, "b"), 1, "{:#?}", fake.requests());
}

/// [`AppState`] for `engine` with the usage counter `usage`.
fn state_with_usage(
    engine: crate::deltaglider::DynEngine,
    usage: Arc<crate::bucket_usage::BucketUsage>,
) -> Arc<AppState> {
    let mut state = Arc::try_unwrap(AppState::for_tests(engine)).ok().unwrap();
    state.bucket_usage = Some(usage);
    Arc::new(state)
}

/// P6.4: the progress total comes from the usage counter only when a
/// full scan stamped its row (the quota gate's rule).
#[tokio::test]
async fn the_total_comes_from_a_scanned_counter_only() {
    let (engine, _fake) = s3_engine().await;
    let usage = Arc::new(crate::bucket_usage::BucketUsage::in_memory().unwrap());
    let engine = engine.with_bucket_usage(Some(usage.clone()));
    store_marked(&engine, 3).await;
    let state = state_with_usage(engine, usage.clone());
    // Writes alone: the row holds only what this counter saw.
    assert_eq!(trusted_object_count(&state, "b"), None);
    let ticket = usage.begin_scan("b");
    let totals = crate::deltaglider::SavingsTotals {
        passthrough_count: 3,
        ..Default::default()
    };
    usage.overwrite_from_scan(ticket, &totals, 1).unwrap();
    assert_eq!(trusted_object_count(&state, "b"), Some(3));
    // A write whose prior read failed marks the row for a recount.
    usage.mark_for_recount("b");
    assert_eq!(trusted_object_count(&state, "b"), None);
}

/// P6.4 edge: a job resumed mid-walk (or persisted mid-count by a
/// previous release) cannot vouch for the references of the pages before
/// its cursor: it lists them, and it still ends with the exact total.
#[tokio::test]
async fn a_resumed_walk_lists_the_references() {
    let (engine, fake) = counted_engine().await;
    store_marked(&engine, 3).await;
    crate::deltaglider::store_deltas(&engine, "d", 2).await;
    engine.metadata_cache().invalidate_prefix("b", "");
    let dir = tempfile::tempdir().unwrap();
    let config: SharedConfig = crate::config::Config::default().into_shared();
    let state = AppState::for_tests(engine);
    let mutator = ConfigMutator {
        config: config.clone(),
        app: state.clone(),
        persist_path: dir.path().join("c.yaml").display().to_string(),
    };
    let db = ConfigDb::in_memory("k").unwrap();
    let id = db
        .maintenance_create_job(
            "reencrypt",
            "b",
            "counting",
            None,
            "admin",
            unique_created_at(),
        )
        .unwrap()
        .unwrap();
    db.maintenance_claim_next_job("inst", current_unix_seconds(), 60)
        .unwrap()
        .unwrap();
    // The previous process walked up to `d/build-001.zip` (2 objects).
    db.maintenance_update_progress(id, "objects", None, 0, 2, 0, 0, Some("d/build-001.zip"))
        .unwrap();
    let job = db.maintenance_job_by_id(id).unwrap().unwrap();
    let db = Arc::new(Mutex::new(db));
    fake.clear();
    run_job(&mutator, &config, &db, &state, Holder::test("inst"), job).await;
    let row = db.lock().await.maintenance_job_by_id(id).unwrap().unwrap();
    assert_eq!(row.status, "completed", "{row:?}");
    assert_eq!(row.objects_total, Some(5), "{row:?}");
    // The walk from the cursor, then the reference listing.
    assert_eq!(bucket_lists(&fake, "b"), 2, "{:#?}", fake.requests());
}

/// P6.6: the idle worker waits for a notify, for the next due row, or at
/// most half a lease TTL; never less than a second.
#[test]
fn the_idle_wait_is_half_a_ttl_or_the_next_due_row() {
    assert_eq!(idle_wait_secs(60, None, 1_000), 30);
    assert_eq!(idle_wait_secs(60, Some(1_010), 1_000), 10);
    assert_eq!(idle_wait_secs(60, Some(5_000), 1_000), 30);
    // A row already due (or due this second): one second, never a spin.
    assert_eq!(idle_wait_secs(60, Some(900), 1_000), 1);
    assert_eq!(idle_wait_secs(60, Some(1_000), 1_000), 1);
    assert_eq!(idle_wait_secs(1, None, 1_000), 1);
}

/// Wait (up to `secs`) until job `id` settles; its row.
async fn settled(db: &Arc<Mutex<ConfigDb>>, id: i64, secs: u64) -> MaintenanceJob {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        let row = db.lock().await.maintenance_job_by_id(id).unwrap().unwrap();
        if row.status == "completed" || tokio::time::Instant::now() >= deadline {
            return row;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// P6.6: the worker drains the queue when it starts (a job that the
/// previous process left queued resumes at boot, with no notify), and a
/// deferred job runs when it comes due, not at the next poll.
#[tokio::test]
async fn the_worker_drains_at_start_and_wakes_when_a_row_comes_due() {
    let e = env(2).await;
    let id = e.claim().await.unwrap().id;
    // A deferred row (backend gated, transient retry): due in a second.
    let not_before = current_unix_seconds() + 1;
    assert!(e
        .db
        .lock()
        .await
        .maintenance_defer(id, "inst", not_before, "waiting")
        .unwrap());
    assert_eq!(
        e.db.lock().await.maintenance_next_due_at().unwrap(),
        Some(not_before)
    );
    // And one queued before the start, on another bucket.
    e.state.engine.load().create_bucket("c").await.unwrap();
    let queued =
        e.db.lock()
            .await
            .maintenance_create_job(
                "reencrypt",
                "c",
                "counting",
                None,
                "admin",
                unique_created_at(),
            )
            .unwrap()
            .unwrap();
    let worker = spawn_worker(e.mutator.clone(), e.db.clone());
    let row = settled(&e.db, queued, 2).await;
    assert_eq!(row.status, "completed", "not drained at start: {row:?}");
    let row = settled(&e.db, id, 4).await;
    worker.abort();
    assert_eq!(row.status, "completed", "not run when due: {row:?}");
}
