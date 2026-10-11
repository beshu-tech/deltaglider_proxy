// SPDX-License-Identifier: BUSL-1.1

//! The maintenance worker on a fake S3 that counts requests (jobs.md 13
//! and 17).

use super::*;
use crate::deltaglider::s3_engine;
use crate::storage::FakeS3;

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
    db.maintenance_create_job("reencrypt", "b", "counting", None, "admin", 1)
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
    // One engine HEAD of an object sends 2 requests at once (its two
    // stored forms): one object at a time peaks at 2.
    assert!(
        e.fake.peak_in_flight("HEAD") >= 2 * super::super::paged::PAGE_CONCURRENCY,
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
