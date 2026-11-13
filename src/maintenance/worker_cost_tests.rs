// SPDX-License-Identifier: BUSL-1.1

//! The maintenance worker on a fake S3 that counts requests (jobs.md 13
//! and 17).

use super::*;
use crate::deltaglider::s3_engine;
use crate::storage::encrypting::ENCRYPTION_MARKER_KEY;
use crate::storage::{FakeS3, StorageBackendExt};

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
    db: Arc<Mutex<JobsDb>>,
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
    let db = JobsDb::in_memory("k").unwrap();
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
    let db = JobsDb::in_memory("k").unwrap();
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
/// newer form wins as on a client HEAD; then the older form once more, for
/// its own version (the job rewrites each form in place, conditional on
/// the version it read).
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
    assert_eq!(ops(&keys[1]).get("HEAD"), Some(&3), "{:?}", ops(&keys[1]));
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
            crate::storage::encrypting::PRESENTED_MARKER_VALUE.to_string(),
        )]);
        engine
            .store("b", &format!("k-{i:03}.txt"), b"x", None, meta)
            .await
            .unwrap();
    }
    engine.metadata_cache().invalidate_prefix("b", "");
}

/// P6.1 + P6.2 + N1: a re-encrypt that rewrites an object HEADs the
/// listed form, GETs it (the body and its version in one read), and PUTs
/// it conditional on that version: no prior read, no verify HEAD, no
/// DELETE of a form that the job had seen absent.
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
        // HEAD (the listed form), GET (the bytes), PUT (the rewrite).
        assert_eq!(object_requests(&fake, "b", &key), 3, "{key}: {ops:?}");
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
    use crate::storage::{StorageBackend, StorageBackendExt};
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
/// objects, the reference prefixes) one after the other. The walk lists it
/// once; without a trusted total, a count listing runs NEXT TO the walk
/// (maint M1: the bar stayed indeterminate to the end), and stops with it,
/// so it may not finish on a small bucket.
#[tokio::test]
async fn reencrypt_lists_the_bucket_once() {
    let (engine, fake) = counted_engine().await;
    store_marked(&engine, 5).await;
    crate::deltaglider::store_deltas(&engine, "d", 3).await;
    engine.metadata_cache().invalidate_prefix("b", "");
    let row = run_kind(engine, &fake, "reencrypt").await;
    assert_eq!(row.status, "completed", "{row:?}");
    assert_eq!(row.objects_total, Some(8), "{row:?}");
    assert!(
        (1..=2).contains(&bucket_lists(&fake, "b")),
        "{:#?}",
        fake.requests()
    );
}

/// P6.4: the same for a backfill.
#[tokio::test]
async fn backfill_lists_the_bucket_once() {
    let (engine, fake) = counted_engine().await;
    store_foreign(&fake, 5);
    let row = run_kind(engine, &fake, super::super::backfill::KIND).await;
    assert_eq!(row.status, "completed", "{row:?}");
    assert_eq!(row.objects_total, Some(5), "{row:?}");
    assert!(
        (1..=2).contains(&bucket_lists(&fake, "b")),
        "{:#?}",
        fake.requests()
    );
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
    let db = JobsDb::in_memory("k").unwrap();
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
    // The walk from the cursor, then the reference listing (and the count
    // listing next to the walk, which may not finish).
    assert!(
        (2..=3).contains(&bucket_lists(&fake, "b")),
        "{:#?}",
        fake.requests()
    );
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
async fn settled(db: &Arc<Mutex<JobsDb>>, id: i64, secs: u64) -> MaintenanceJob {
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
        e.db.lock()
            .await
            .maintenance_next_due_at(current_unix_seconds())
            .unwrap(),
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
    let worker = spawn_worker(
        e.mutator.clone(),
        e.db.clone(),
        crate::maintenance::delete::DeleteDeps {
            iam: std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(
                crate::iam::IamState::deny_all(),
            )),
            savings: Default::default(),
        },
    );
    let row = settled(&e.db, queued, 2).await;
    assert_eq!(row.status, "completed", "not drained at start: {row:?}");
    let row = settled(&e.db, id, 4).await;
    worker.abort();
    assert_eq!(row.status, "completed", "not run when due: {row:?}");
}

/// Maint M1 / overview M1: without a scanned total the bar stayed
/// indeterminate until the walk ended. The count next to the walk gives it.
#[tokio::test]
async fn the_background_count_gives_the_total() {
    let (engine, _fake) = s3_engine().await;
    store_marked(&engine, 5).await;
    let count = BackgroundCount::spawn(Arc::new(engine), "b".into());
    for _ in 0..200 {
        if count.get().is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(count.get(), Some(5));
}

// ---- a job that waits for disk space ----

/// [`env`] with a spool of its own: the disk gate of the process-wide spool
/// must not feel a test's full disk.
async fn env_own_spool(objects: usize) -> Env {
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
    let engine = engine.with_spool_dir(
        crate::deltaglider::spool::SpoolDir::new(dir.path().join("spool"), 8 * 1024 * 1024)
            .unwrap(),
    );
    let config: SharedConfig = crate::config::Config::default().into_shared();
    let state = AppState::for_tests(engine);
    let mutator = ConfigMutator {
        config: config.clone(),
        app: state.clone(),
        persist_path: dir.path().join("c.yaml").display().to_string(),
    };
    let db = JobsDb::in_memory("k").unwrap();
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

/// Every path is one 100 GiB disk with `free` GiB free.
struct OneDisk(u64);

impl crate::disk_watch::DiskProbe for OneDisk {
    fn measure(&self, _path: &std::path::Path) -> Option<crate::disk_watch::PathFacts> {
        const GIB: u64 = 1024 * 1024 * 1024;
        Some(crate::disk_watch::PathFacts {
            free: self.0 * GIB,
            total: 100 * GIB,
            device: 1,
        })
    }
}

/// A monitor of this test's thread: the backend `default` (where a bucket
/// with no route lives) has `free` GiB of 100 free, and the spool is its own.
pub(crate) fn watch_with(state: &AppState, free: u64) -> Arc<crate::disk_watch::DiskWatch> {
    let gate = state.engine.load().spool_dir().gate().clone();
    let watch = Arc::new(crate::disk_watch::DiskWatch::new(gate));
    set_free(&watch, free);
    crate::disk_watch::install_for_this_test(watch.clone());
    watch
}

pub(crate) fn set_free(watch: &crate::disk_watch::DiskWatch, free: u64) {
    use crate::disk_watch::{Sizing, Targets};
    const GIB: u64 = 1024 * 1024 * 1024;
    let tuning = crate::config::tuning::RuntimeTuning::default().disk_monitor;
    let targets = Targets {
        spool_dir: "/spool".into(),
        backends: vec![("default".into(), "/data".into())],
        data_dir: "/state".into(),
        default_backend: "default".into(),
        spool_by_use: false,
    };
    let sizing = Sizing {
        budget: 8 * GIB,
        used: 0,
        largest_reservation: GIB,
    };
    watch.tick(
        &OneDisk(free),
        &targets,
        &tuning,
        sizing,
        std::time::Instant::now(),
        None,
    );
}

impl Env {
    /// [`Env::run`] with a wait of `defer_secs` after a deferral.
    async fn run_waiting(&self, job: MaintenanceJob, defer_secs: i64) {
        run_job(
            &self.mutator,
            &self.config,
            &self.db,
            &self.state,
            Holder {
                defer_secs: Some(defer_secs),
                ..Holder::test("inst")
            },
            job,
        )
        .await;
    }
}

/// A re-encrypt (legacy rows have no conditional writes: they gate their
/// bucket) that finds the disk of its bucket full waits, with its mark on the
/// gate: a delete into the bucket passes meanwhile; the wait goes on while the
/// disk stays full; and when it ends, ON THE REAL CLOCK (the row is claimed
/// after its wait has passed, so the mark has expired by then), a delete that
/// the wait admitted and that is still running holds the job: the job does not
/// run before it ends. No restart, no faked `now`.
#[tokio::test]
async fn a_reencrypt_that_waits_for_disk_resumes_on_the_real_clock_and_drains_the_deletes() {
    let e = env_own_spool(4).await;
    let watch = watch_with(&e.state, 3);
    e.state
        .engine
        .load()
        .spool_dir()
        .gate()
        .set(crate::disk_watch::Level::Ok, String::new());
    let gate = e.state.maintenance_gate.clone();
    let row = |id| {
        let db = e.db.clone();
        async move { db.lock().await.maintenance_job_by_id(id).unwrap().unwrap() }
    };

    let job = e.claim().await.unwrap();
    let id = job.id;
    gate.sync_from(&*e.db.lock().await);
    e.run_waiting(job, 1).await;
    let r = row(id).await;
    assert_eq!(r.status, "queued", "{r:?}");
    assert!(
        r.last_error
            .as_deref()
            .unwrap_or("")
            .contains("backend 'default'"),
        "{r:?}"
    );
    assert_eq!(
        r.objects_skipped + r.objects_done,
        0,
        "nothing was rewritten: {r:?}"
    );
    assert!(gate.is_disk_deferred(id), "the wait is on the gate");
    assert_eq!(
        gate.busy_kind_for("b", crate::maintenance::gate::WriteKind::FreesSpace),
        None,
        "deletes pass"
    );
    assert_eq!(gate.write_blocked("b"), None, "a delete job passes");

    // The wait ends on the real clock: the claim finds the row only now, and
    // the mark has expired. Still critical: the job waits on, the mark is
    // renewed.
    assert!(e.claim().await.is_none(), "claimed before its wait ended");
    tokio::time::sleep(std::time::Duration::from_millis(2200)).await;
    assert!(!gate.is_disk_deferred(id), "the mark expired with the wait");
    assert!(gate.has_disk_mark(id), "but the job's mark is still there");
    let job = e.claim().await.expect("claimable once its wait is over");
    gate.sync_from(&*e.db.lock().await);
    e.run_waiting(job, 1).await;
    assert_eq!(row(id).await.status, "queued");
    assert!(
        gate.is_disk_deferred(id),
        "the mark is renewed while the disk is full"
    );

    // Space again. A delete that the wait admitted is STILL RUNNING when the
    // wait ends: the job must not run before it is over.
    set_free(&watch, 50);
    tokio::time::sleep(std::time::Duration::from_millis(2200)).await;
    let in_flight_delete = gate.begin_write("b");
    let job = e.claim().await.expect("claimable");
    gate.sync_from(&*e.db.lock().await);
    let finished = std::sync::atomic::AtomicBool::new(false);
    let run = async {
        e.run_waiting(job, 1).await;
        finished.store(true, std::sync::atomic::Ordering::SeqCst);
    };
    let release = async {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert!(
            !finished.load(std::sync::atomic::Ordering::SeqCst),
            "the job ran while a delete of the wait was still in flight"
        );
        assert!(
            !gate.is_disk_deferred(id),
            "a delete is refused from now on"
        );
        drop(in_flight_delete);
    };
    tokio::join!(run, release);
    let r = row(id).await;
    assert_eq!(r.status, "completed", "{r:?}");
    assert!(!gate.has_disk_mark(id), "the mark ends with the drain");
}

/// A re-encrypt whose rewrites are conditional holds no write gate: client
/// writes go on, deletes need no pass, and a resume has nothing to drain. A
/// client write that is in flight at its resume must not hold it (the drain
/// would wait up to the request timeout and then fail the job).
#[tokio::test]
async fn a_reencrypt_without_the_write_gate_sets_no_mark_and_never_drains() {
    let e = env_own_spool(4).await;
    let watch = watch_with(&e.state, 3);
    e.state
        .engine
        .load()
        .spool_dir()
        .gate()
        .set(crate::disk_watch::Level::Ok, String::new());
    let gate = e.state.maintenance_gate.clone();
    let row = |id| {
        let db = e.db.clone();
        async move { db.lock().await.maintenance_job_by_id(id).unwrap().unwrap() }
    };
    let job = e.claim().await.unwrap();
    let id = job.id;
    {
        let db = e.db.lock().await;
        db.maintenance_set_params(id, Some("{\"conditional_writes\":true}"))
            .unwrap();
        gate.sync_from(&db);
    }
    assert!(!gate.arms_job(id), "a conditional job holds no bucket");
    e.run_waiting(job, 1).await;
    assert_eq!(row(id).await.status, "queued");
    assert!(!gate.arms_job(id), "the run kept the job conditional");
    assert!(!gate.has_disk_mark(id), "no gate, so no wait to mark");

    // Space again, and a client write in flight at the resume.
    set_free(&watch, 50);
    tokio::time::sleep(std::time::Duration::from_millis(2200)).await;
    let client_write = gate.begin_write("b");
    let job = e.claim().await.expect("claimable");
    gate.sync_from(&*e.db.lock().await);
    tokio::time::timeout(std::time::Duration::from_secs(5), e.run_waiting(job, 1))
        .await
        .expect("the resume waited for a client write that the job does not gate");
    drop(client_write);
    assert_eq!(row(id).await.status, "completed");
}

/// The resume order: the gate is whole FIRST (a delete is refused from that
/// moment), THEN the deletes that were admitted meanwhile are drained. A
/// delete still in flight holds the job: it waits for it and gives up after
/// its ceiling (the request timeout, so no request outlives it).
#[tokio::test]
async fn a_job_that_runs_again_closes_the_gate_before_it_drains_the_deletes() {
    use crate::maintenance::gate::{MaintenanceGate, WriteKind};
    let gate = Arc::new(MaintenanceGate::new());
    gate.set_busy("b");
    gate.mark_disk_deferred(5, current_unix_seconds() + 600);
    let _in_flight_delete = gate.begin_write("b");
    // `set_busy` has no job id: the closing is the mark's.
    let stop = resume_after_disk_wait(&gate, 5, "b", std::time::Duration::from_millis(60))
        .await
        .unwrap_err();
    assert!(
        matches!(stop, PhaseStop::Failed(_) | PhaseStop::Deferred(_)),
        "{stop:?}"
    );
    assert!(
        !gate.is_disk_deferred(5),
        "the wait is closed before the drain: a delete is refused"
    );
    assert!(
        gate.has_disk_mark(5),
        "a drain that gave up keeps the mark: the next claim drains again"
    );
    // Without the in-flight delete it drains at once, and only then is the
    // mark gone.
    drop(_in_flight_delete);
    gate.mark_disk_deferred(5, current_unix_seconds() + 600);
    resume_after_disk_wait(&gate, 5, "b", std::time::Duration::from_millis(60))
        .await
        .unwrap();
    assert!(!gate.is_disk_deferred(5));
    assert!(!gate.has_disk_mark(5), "the mark ends after the drain");
    let _ = WriteKind::Store;
}

/// A re-encrypt does not wait for the SPOOL disk as a whole: its objects need
/// a spool file only above the spool threshold (and an S3 bucket's never do).
/// With the spool disk full and the disk of its bucket fine, a job whose
/// objects need no spool runs to its end.
#[tokio::test]
async fn a_reencrypt_whose_objects_need_no_spool_runs_at_a_full_spool_disk() {
    use crate::disk_watch::Level;
    let e = env_own_spool(4).await;
    let _watch = watch_with(&e.state, 50);
    e.state
        .engine
        .load()
        .spool_dir()
        .gate()
        .set(Level::High, "the spool disk is full".into());
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
    assert_eq!(row.objects_failed, 0, "{row:?}");
    assert!(!e.state.maintenance_gate.is_disk_deferred(id));
}

/// An object that DOES need a spool file meets the spool gate in its own
/// rewrite: that is not its failure. The job waits and does it again; the
/// objects before it are settled and no failure is recorded.
#[tokio::test]
async fn an_object_that_needs_spool_at_a_full_spool_disk_defers_the_job() {
    struct DefersAt(&'static str);
    impl ObjectVisitor for DefersAt {
        async fn visit(
            &self,
            _engine: &Arc<crate::deltaglider::DynEngine>,
            key: &str,
            _listed: Option<crate::storage::ObjectVariant>,
        ) -> Visit {
            if key == self.0 {
                Visit::Deferred("the spool disk is full".into())
            } else {
                Visit::Done { bytes: 1 }
            }
        }
    }
    let e = env_own_spool(4).await;
    let job = e.claim().await.unwrap();
    let id = job.id;
    let stop = run_count_then_walk(
        &e.db,
        &e.state,
        Holder::test("inst"),
        &job,
        "test job",
        &mut DefersAt("k-002.txt"),
    )
    .await
    .err()
    .expect("the job waits");
    assert_eq!(stop, PhaseStop::Deferred("the spool disk is full".into()));
    let row =
        e.db.lock()
            .await
            .maintenance_job_by_id(id)
            .unwrap()
            .unwrap();
    assert_eq!(row.objects_failed, 0, "no failure is recorded: {row:?}");
}

/// A failed rewrite is the object's failure, except a retryable refusal of
/// the spool disk gate that is in force now: that defers the job.
#[test]
fn a_rewrite_error_defers_only_for_the_spool_gate() {
    use crate::deltaglider::EngineError;
    let failed = || Visit::Failed("rewrite failed".into());
    let overloaded = EngineError::Overloaded("busy".into());
    let other = EngineError::NotFound("k".into());
    let why = || Some("the spool disk is full".to_string());
    assert!(matches!(
        rewrite_error_visit(&overloaded, why(), failed),
        Visit::Deferred(m) if m == "the spool disk is full"
    ));
    // Overloaded with the gate open: a full budget, an object's failure.
    assert!(matches!(
        rewrite_error_visit(&overloaded, None, failed),
        Visit::Failed(_)
    ));
    // Another error, even with the gate shut: the object's failure.
    assert!(matches!(
        rewrite_error_visit(&other, why(), failed),
        Visit::Failed(_)
    ));
}

/// A wait for disk space opens the gate to deletes only where that is safe:
/// a re-encrypt, and a migrate in its COPY phase. A migrate deferred in the
/// stage, the verify, the flip or the cleanup while the disk is critical (a
/// backend that does not answer defers it too) is NOT marked: its verify would
/// not see a delete.
#[tokio::test]
async fn a_disk_wait_marks_the_gate_only_in_the_phases_where_it_is_safe() {
    use crate::coordination::health::HealthVerdict;
    for (kind, phase, marked) in [
        ("migrate", "stage", false),
        ("migrate", "copy", true),
        ("migrate", "verify", false),
        ("migrate", "flip", false),
        ("migrate", "cleanup", false),
        ("reencrypt", "objects", true),
    ] {
        let mut e = env_own_spool(0).await;
        let _watch = watch_with(&e.state, 3);
        e.state
            .engine
            .load()
            .spool_dir()
            .gate()
            .set(crate::disk_watch::Level::Ok, String::new());
        let params = (kind == "migrate").then(|| {
            serde_json::to_string(&super::super::migrate::MigrateParams {
                disk_wait: false,
                target_backend: "default".into(),
                delete_source: false,
                transient_key: "__dgmigrate_b_0".into(),
                from_backend: "default".into(),
                target: super::super::migrate::MigrateTarget::Empty,
            })
            .unwrap()
        });
        let db = JobsDb::in_memory("k").unwrap();
        db.maintenance_create_job(
            kind,
            "b",
            phase,
            params.as_deref(),
            "admin",
            unique_created_at(),
        )
        .unwrap()
        .unwrap();
        e.db = Arc::new(Mutex::new(db));
        // The backend does not answer: the job defers before any phase, with
        // the disk of its bucket critical as well.
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
        let row =
            e.db.lock()
                .await
                .maintenance_job_by_id(id)
                .unwrap()
                .unwrap();
        assert_eq!(row.status, "queued", "{kind} {phase}: {row:?}");
        assert_eq!(
            e.state.maintenance_gate.has_disk_mark(id),
            marked,
            "{kind} in phase {phase}"
        );
    }
}

/// A re-encrypt visit of an object ABOVE the spool threshold at a full spool
/// disk meets the spool gate in its rewrite: the job waits. The visit is not
/// the object's failure (nothing is recorded), and the object stays as it was.
#[tokio::test]
async fn a_large_reencrypt_visit_at_a_full_spool_disk_defers_and_leaves_the_object() {
    use crate::disk_watch::Level;
    let e = env_own_spool(0).await;
    let engine = e.state.engine.load_full();
    let large = vec![9u8; (engine.spool_threshold() + 1024 * 1024) as usize];
    engine
        .store("b", "large.jpg", &large, None, Default::default())
        .await
        .unwrap();
    engine
        .spool_dir()
        .gate()
        .set(Level::High, "the spool disk is full".into());
    let marker = crate::maintenance::conversion::begin(&engine, "b", None)
        .await
        .unwrap();
    let adoptions = crate::maintenance::conversion::Adoptions::default();
    let visitor = Reencrypt {
        config: &e.config,
        bucket: "b",
        desired: Some(DesiredEncryption::Proxy {
            key_id: "k1".into(),
        }),
        writes_plaintext: false,
        marker: &marker,
        adoptions: &adoptions,
        unconditional: crate::deltaglider::Unconditional::Allowed,
    };
    let visit = visitor.visit(&engine, "large.jpg", None).await;
    assert!(
        matches!(&visit, Visit::Deferred(m) if m == "the spool disk is full"),
        "a refusal of the spool gate defers the job: {}",
        match &visit {
            Visit::Failed(m) => format!("Failed({m})"),
            _ => "another outcome".into(),
        }
    );
    // The object is whole and unchanged (no marker was stamped).
    let meta = engine.head("b", "large.jpg").await.unwrap();
    assert!(
        needs_rewrite(
            &meta.user_metadata,
            &DesiredEncryption::Proxy {
                key_id: "k1".into()
            }
        ),
        "the object was not rewritten"
    );
    assert_eq!(engine.retrieve("b", "large.jpg").await.unwrap().0, large);
    // With room again the same visit rewrites it.
    engine.spool_dir().gate().set(Level::Ok, String::new());
    assert!(matches!(
        visitor.visit(&engine, "large.jpg", None).await,
        Visit::Done { .. } | Visit::Adopted { .. }
    ));
}

/// The same large re-encrypt visit at a spool that refuses by the USE of its
/// budget (the disk level is fine): it defers too, and the object stays as it
/// was. It used to fail, land in the failure ring and end the job
/// `completed_with_errors`.
#[tokio::test]
async fn a_large_reencrypt_visit_at_a_spool_refusing_by_use_defers() {
    let e = env_own_spool(0).await;
    let engine = e.state.engine.load_full();
    let large = vec![9u8; (engine.spool_threshold() + 1024 * 1024) as usize];
    engine
        .store("b", "large.jpg", &large, None, Default::default())
        .await
        .unwrap();
    let mut th = crate::disk_watch::Thresholds::new(
        &crate::config::tuning::RuntimeTuning::default().disk_monitor,
        9 * 1024 * 1024,
    );
    th.high_floor = 9 * 1024 * 1024 * 1024;
    engine.spool_dir().gate().set_by_use(Some(th));
    assert!(engine.spool_dir().write_refusal().is_some());
    let marker = crate::maintenance::conversion::begin(&engine, "b", None)
        .await
        .unwrap();
    let adoptions = crate::maintenance::conversion::Adoptions::default();
    let visitor = Reencrypt {
        config: &e.config,
        bucket: "b",
        desired: Some(DesiredEncryption::Proxy {
            key_id: "k1".into(),
        }),
        writes_plaintext: false,
        marker: &marker,
        adoptions: &adoptions,
        unconditional: crate::deltaglider::Unconditional::Allowed,
    };
    let visit = visitor.visit(&engine, "large.jpg", None).await;
    assert!(
        matches!(&visit, Visit::Deferred(_)),
        "a refusal by use defers the job: {}",
        match &visit {
            Visit::Failed(m) => format!("Failed({m})"),
            _ => "another outcome".into(),
        }
    );
    assert_eq!(engine.retrieve("b", "large.jpg").await.unwrap().0, large);
}

/// A visit that defers the job stops the page: no later object STARTS (the
/// phase's `stops`), so a job that waits for the disk does not go on rewriting.
#[tokio::test]
async fn a_deferral_stops_the_objects_after_it() {
    use std::sync::Mutex as StdMutex;
    struct Recorder(StdMutex<Vec<String>>);
    impl ObjectVisitor for Recorder {
        const CONCURRENCY: usize = 1;
        async fn visit(
            &self,
            _engine: &Arc<crate::deltaglider::DynEngine>,
            key: &str,
            _listed: Option<crate::storage::ObjectVariant>,
        ) -> Visit {
            self.0.lock().unwrap().push(key.to_string());
            if key == "k-001.txt" {
                Visit::Deferred("the spool disk is full".into())
            } else {
                Visit::Done { bytes: 1 }
            }
        }
    }
    let e = env_own_spool(4).await;
    let job = e.claim().await.unwrap();
    let mut visitor = Recorder(StdMutex::new(Vec::new()));
    let stop = run_count_then_walk(
        &e.db,
        &e.state,
        Holder::test("inst"),
        &job,
        "test job",
        &mut visitor,
    )
    .await
    .err()
    .expect("the job waits");
    assert!(matches!(stop, PhaseStop::Deferred(_)), "{stop:?}");
    assert_eq!(
        *visitor.0.lock().unwrap(),
        ["k-000.txt", "k-001.txt"],
        "nothing starts after the deferred object"
    );
}
