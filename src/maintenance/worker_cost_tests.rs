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
    let (_, forms) = engine.list_job_page("b", 1000, None).await.unwrap();
    let form = forms.of("k.txt");
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
