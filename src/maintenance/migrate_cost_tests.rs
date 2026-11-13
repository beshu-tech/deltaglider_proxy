// SPDX-License-Identifier: BUSL-1.1

//! Migrate against a backend that times out now and then (jobs.md 3 and
//! 4, blast.md 5), on a fake S3 that counts requests: the "target" is the
//! bucket `t` of the same engine, reached through a staging route that
//! the config already holds (so no phase rebuilds the engine before the
//! unwind removes it).

use super::*;
use crate::deltaglider::s3_engine;
use crate::storage::FakeS3;

struct Env {
    state: Arc<AppState>,
    fake: Arc<FakeS3>,
    db: Arc<Mutex<JobsDb>>,
    job: MaintenanceJob,
    mutator: ConfigMutator,
    params: MigrateParams,
    _dir: tempfile::TempDir,
}

/// Objects `k-000.txt` … in `b` (the source) and the same objects in `t`
/// (the copies this job made), and a migrate job persisted in `phase`.
async fn env(phase: &str, objects: usize) -> Env {
    env_with(phase, objects, true).await
}

/// [`env`]; `in_target`: whether the target already holds the copies. The
/// usage counter is on, as in production.
async fn env_with(phase: &str, objects: usize, in_target: bool) -> Env {
    let (engine, fake) = s3_engine().await;
    let dir = tempfile::tempdir().unwrap();
    // A spool of its own: the process-wide spool (and its disk gate) must not
    // feel a test's full disk.
    let engine = engine
        .with_spool_dir(
            crate::deltaglider::spool::SpoolDir::new(dir.path().join("spool"), 64 * 1024 * 1024)
                .unwrap(),
        )
        .with_bucket_usage(Some(Arc::new(
            crate::bucket_usage::BucketUsage::in_memory().unwrap(),
        )));
    engine.create_bucket("t").await.unwrap();
    let params = MigrateParams {
        disk_wait: false,
        target_backend: "dst".into(),
        delete_source: false,
        transient_key: "t".into(),
        from_backend: "src".into(),
        target: MigrateTarget::Empty,
    };
    let db = JobsDb::in_memory("k").unwrap();
    let id = db
        .maintenance_create_job(
            "migrate",
            "b",
            phase,
            Some(&serde_json::to_string(&params).unwrap()),
            "admin",
            1,
        )
        .unwrap()
        .unwrap();
    db.maintenance_claim_next_job("inst", super::super::store::current_unix_seconds(), 60)
        .unwrap()
        .unwrap();
    let job = db.maintenance_job_by_id(id).unwrap().unwrap();
    let ours = provenance_value(&params, job.id);
    for i in 0..objects {
        let key = format!("run/k-{i:03}.txt");
        let body = format!("object {i}");
        engine
            .store("b", &key, body.as_bytes(), None, Default::default())
            .await
            .unwrap();
        if !in_target {
            continue;
        }
        let meta = std::collections::HashMap::from([("dg-migration".to_string(), ours.clone())]);
        engine
            .store("t", &key, body.as_bytes(), None, meta)
            .await
            .unwrap();
    }
    // Every HEAD of a phase reaches the fake (the stores above filled the
    // engine's metadata cache).
    engine.metadata_cache().invalidate_prefix("b", "");
    engine.metadata_cache().invalidate_prefix("t", "");
    let mut cfg = crate::config::Config {
        backend: crate::config::BackendConfig::Filesystem {
            path: dir.path().join("data"),
        },
        ..Default::default()
    };
    cfg.buckets.insert(
        "t".into(),
        crate::bucket_policy::BucketPolicyConfig {
            backend: Some("dst".into()),
            alias: Some("b".into()),
            ..Default::default()
        },
    );
    let state = AppState::for_tests(engine);
    let mutator = ConfigMutator {
        config: cfg.into_shared(),
        app: state.clone(),
        persist_path: dir.path().join("c.yaml").display().to_string(),
    };
    fake.clear();
    Env {
        state,
        fake,
        db: Arc::new(Mutex::new(db)),
        job,
        mutator,
        params,
        _dir: dir,
    }
}

impl Env {
    fn run(&self) -> MigrateRun<'_> {
        MigrateRun {
            mutator: &self.mutator,
            db: &self.db,
            state: &self.state,
            holder: Holder::test("inst"),
            job: &self.job,
            params: &self.params,
            provenance: provenance_value(&self.params, self.job.id),
        }
    }

    fn count(&self, method: &str) -> usize {
        self.fake
            .requests()
            .iter()
            .filter(|r| r.starts_with(method))
            .count()
    }

    async fn phases(&self) -> Result<(), PhaseStop> {
        execute_migrate_phases(
            &self.mutator,
            &self.db,
            &self.state,
            Holder::test("inst"),
            &self.job,
        )
        .await
    }
}

/// blast-5: one target HEAD that answers 503 in verify read as "missing",
/// failed the job, and the unwind deleted every staged copy. The HEAD is
/// sent again.
#[tokio::test]
async fn verify_retries_a_transient_target_head_error() {
    let e = env("verify", 50).await;
    e.fake.fail("HEAD", "t/run/k-037", 503, "SlowDown", 1);
    let c = Counters::default();
    let result = e.run().verify(&c).await;
    assert!(result.is_ok(), "{result:?}");
    assert!(
        e.fake
            .requests()
            .iter()
            .filter(|r| r.starts_with("HEAD /t/run/k-037"))
            .count()
            > 2,
        "the failed HEAD was sent again: {:#?}",
        e.fake.requests()
    );
}

/// blast-5: a target that keeps timing out stopped the job, and the
/// unwind deleted every staged copy. Such a stop says nothing about the
/// copies: the first ones queue the job again with its staging route (the
/// worker requeues it), and after the last retry the job fails, keeps the
/// copies (no DELETE) and says how to reuse them.
#[tokio::test]
async fn a_transient_failure_keeps_the_staged_copies() {
    let e = env("verify", 50).await;
    let engine = e.state.engine.load_full();
    e.fake
        .fail("HEAD", "t/run/k-037", 503, "SlowDown", u32::MAX);
    let err = e.phases().await.unwrap_err();
    assert!(matches!(err, PhaseStop::FailedTransient(_)), "{err:?}");
    assert!(
        e.fake
            .requests()
            .iter()
            .any(|r| r.starts_with("HEAD /t/run/k-037")),
        "the failing HEAD was sent"
    );
    assert_eq!(e.count("DELETE"), 0, "the staged copies were deleted");
    assert!(
        e.mutator.read().await.buckets.contains_key("t"),
        "a job with retries left keeps its staging route"
    );

    // The retries are used up: the job fails, the copies stay.
    let row = super::super::worker::current_row(&e.db, &e.job).await;
    super::super::worker::set_transient_stops(&row, super::super::worker::TRANSIENT_RETRIES);
    let err = e.phases().await.unwrap_err();
    assert_eq!(e.count("DELETE"), 0, "the staged copies were deleted");
    assert!(err.to_string().contains("mirror"), "{err}");
    assert!(!e.mutator.read().await.buckets.contains_key("t"));
    assert!(
        engine.head("b", "run/k-000.txt").await.is_ok(),
        "the source is untouched"
    );
}

/// jobs-3: a definitive failure still unwinds, through the batch: each
/// staged folder is checked once for its reference, and only this job's
/// copies go (a foreign object stays).
#[tokio::test]
async fn a_definitive_failure_unwinds_in_one_batch() {
    let e = env("verify", 30).await;
    let engine = e.state.engine.load().clone();
    // The target copy of k-010 differs from the source: verify fails for good.
    engine
        .store(
            "t",
            "run/k-010.txt",
            b"another version",
            None,
            Default::default(),
        )
        .await
        .unwrap();
    engine
        .store(
            "t",
            "run/foreign.txt",
            b"not ours",
            None,
            Default::default(),
        )
        .await
        .unwrap();
    e.fake.clear();
    let err = e.phases().await.unwrap_err();
    assert!(err.to_string().contains("differs"), "{err}");
    let checks: Vec<String> = e
        .fake
        .requests()
        .into_iter()
        // The check of a folder of plain files is one HEAD of its
        // reference.bin; a folder with a reference adds one LIST.
        .filter(|r| {
            r == "HEAD /t/run/reference.bin"
                || (r.starts_with("GET /t")
                    && r.contains("delimiter=%2F")
                    && crate::storage::fake_s3::list_prefix(r) == "run/")
        })
        .collect();
    assert_eq!(checks.len(), 1, "{checks:#?}");
    let left = engine
        .list_objects("t", "", None, 1000, None, false)
        .await
        .unwrap();
    let left: Vec<&str> = left.objects.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(left, ["run/foreign.txt", "run/k-010.txt"]);
}

/// accounting: the flip left the source's layout in the usage counter.
/// It marks the bucket's row for a recount.
#[tokio::test]
async fn the_flip_marks_the_usage_row_for_a_recount() {
    let (src, dst, dir) = (
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
        tempfile::tempdir().unwrap(),
    );
    let yaml = format!(
        "storage:\n  backends:\n  - name: src\n    type: filesystem\n    path: {}\n  - name: dst\n    type: filesystem\n    path: {}\n  default_backend: src\n  buckets:\n    b:\n      backend: src\n",
        src.path().display(),
        dst.path().display()
    );
    let mut cfg = crate::config::Config::from_yaml_str(&yaml).unwrap();
    let params = MigrateParams {
        disk_wait: false,
        target_backend: "dst".into(),
        delete_source: false,
        transient_key: "__dgmigrate_b_0".into(),
        from_backend: "src".into(),
        target: MigrateTarget::Empty,
    };
    cfg.buckets.insert(
        params.transient_key.clone(),
        crate::bucket_policy::BucketPolicyConfig {
            backend: Some("dst".into()),
            alias: Some("b".into()),
            ..Default::default()
        },
    );
    let engine = crate::deltaglider::DynEngine::new(&cfg, None)
        .await
        .unwrap();
    let usage = Arc::new(crate::bucket_usage::BucketUsage::in_memory().unwrap());
    usage
        .overwrite_from_scan(usage.begin_scan("b"), &Default::default(), 100)
        .unwrap();
    let state = Arc::new(AppState {
        bucket_usage: Some(usage.clone()),
        ..Arc::into_inner(AppState::for_tests(engine)).unwrap()
    });
    let db = JobsDb::in_memory("k").unwrap();
    let id = db
        .maintenance_create_job(
            "migrate",
            "b",
            "flip",
            Some(&serde_json::to_string(&params).unwrap()),
            "admin",
            1,
        )
        .unwrap()
        .unwrap();
    db.maintenance_claim_next_job("inst", super::super::store::current_unix_seconds(), 60)
        .unwrap()
        .unwrap();
    let job = db.maintenance_job_by_id(id).unwrap().unwrap();
    let db = Arc::new(Mutex::new(db));
    let mutator = ConfigMutator {
        config: cfg.into_shared(),
        app: state.clone(),
        persist_path: dir.path().join("c.yaml").display().to_string(),
    };
    let m = MigrateRun {
        mutator: &mutator,
        db: &db,
        state: &state,
        holder: Holder::test("inst"),
        job: &job,
        params: &params,
        provenance: provenance_value(&params, job.id),
    };
    m.flip(
        &Counters::default(),
        crate::maintenance::worker::Durable::for_tests(),
    )
    .await
    .unwrap();
    usage.flush_pending();
    let row = usage.read("b").unwrap().unwrap();
    assert_eq!(row.last_scan_at, None, "the row still reads as scanned");
}

/// P6.1: the verify of one object HEADed both forms on the source and on
/// the target (4 requests). The source HEAD reads the listed form.
#[tokio::test]
async fn migrate_verify_heads_the_listed_source_form() {
    use super::super::worker::cost_tests::{object_ops, object_requests};
    let e = env("verify", 5).await;
    e.run().verify(&Counters::default()).await.unwrap();
    for i in 0..5 {
        let key = format!("run/k-{i:03}.txt");
        let (src, dst) = (
            object_ops(&e.fake, "b", &key),
            object_ops(&e.fake, "t", &key),
        );
        assert_eq!(
            object_requests(&e.fake, "b", &key),
            1,
            "{key}: source {src:?} target {dst:?}"
        );
    }
}
/// P6.1 + P6.2: the copy of one object into an empty target HEADed the
/// target (2 forms), HEADed the source (2), and the store read the
/// target's prior (2; on a `__dgmigrate_` staging bucket it reads none
/// but DELETEs the other form). The skip check's target HEAD is the
/// store's prior, and the source HEAD reads the one form that the listing
/// saw: 10 -> 7 requests; the verify HEADs only the written form: 6;
/// a server-side copy (one backend): 5, and no byte through the proxy.
#[tokio::test]
async fn migrate_copy_reuses_the_skip_check_as_the_prior() {
    use super::super::worker::cost_tests::{object_ops, object_requests};
    // Persisted in `stage`: a job resumed in `copy` re-copies its first
    // page without the skip check.
    let e = env_with("stage", 5, false).await;
    let mut c = Counters::default();
    e.run().copy(&mut c).await.unwrap();
    assert_eq!(c.done, 5, "{c:?}");
    for i in 0..5 {
        let key = format!("run/k-{i:03}.txt");
        let (src, dst) = (
            object_ops(&e.fake, "b", &key),
            object_ops(&e.fake, "t", &key),
        );
        let total = object_requests(&e.fake, "b", &key) + object_requests(&e.fake, "t", &key);
        // Source and target on one backend: a server-side copy. Target:
        // HEAD both forms (the check, then the prior), CopyObject. Source:
        // HEAD (the listed form), HEAD (the copy's generation pin); no
        // GET, no PUT: no byte passes through the proxy.
        assert_eq!(total, 5, "{key}: source {src:?} target {dst:?}");
        assert_eq!(src.get("HEAD"), Some(&2), "{key}: source {src:?}");
        assert_eq!(dst.get("COPY"), Some(&1), "{key}: target {dst:?}");
        assert_eq!(dst.get("DELETE"), None, "{key}: target {dst:?}");
    }
}

/// A durable save that fails before the flip stops the job there: the flip
/// does not run (the config never routes the bucket to the target) and the
/// copies stay on the target (a transient stop deletes nothing). The failure
/// is injected with an open transaction on the jobs DB, where the
/// `synchronous = FULL` commit refuses.
#[tokio::test]
async fn no_flip_runs_after_a_failed_durable_save() {
    let e = env("verify", 3).await;
    e.db.lock().await.conn.execute_batch("BEGIN").unwrap();
    let result = e.phases().await;
    assert!(
        matches!(result, Err(PhaseStop::FailedTransient(_))),
        "{result:?}"
    );
    let cfg = e.mutator.read().await;
    assert!(
        !routed_to_target(&cfg, "b", "dst"),
        "the bucket was flipped after a failed save"
    );
    drop(cfg);
    e.state
        .engine
        .load()
        .head("t", "run/k-000.txt")
        .await
        .expect("the copies stay on the target");
}

/// A resumed flip (the save after it failed) finds the config flipped. It
/// must not write the config again: here the config file cannot be written,
/// as on a full disk, and the flip still succeeds.
#[tokio::test]
async fn a_resumed_flip_writes_nothing_when_the_config_is_flipped() {
    let e = env("flip", 1).await;
    let durable = || crate::maintenance::worker::Durable::for_tests();
    // The flip happened (the save after it failed): the config routes the
    // bucket to the target.
    e.mutator.config.write().await.buckets.insert(
        "b".into(),
        crate::bucket_policy::BucketPolicyConfig {
            backend: Some("dst".into()),
            ..Default::default()
        },
    );
    assert!(routed_to_target(&*e.mutator.read().await, "b", "dst"));
    let broken = ConfigMutator {
        config: e.mutator.config.clone(),
        app: e.state.clone(),
        persist_path: "/nonexistent-dgp-dir/c.yaml".to_string(),
    };
    let resumed = MigrateRun {
        mutator: &broken,
        db: &e.db,
        state: &e.state,
        holder: Holder::test("inst"),
        job: &e.job,
        params: &e.params,
        provenance: provenance_value(&e.params, e.job.id),
    };
    resumed
        .flip(&Counters::default(), durable())
        .await
        .expect("a flip that already happened writes nothing");
}

/// The config routes the bucket to the target already (the flip happened),
/// and the row names an earlier phase. A failure or a cancel of the resumed
/// job must not unwind: the staged copies ARE the live bucket, their route
/// stays, and the write gate is not released by an unwind.
async fn flipped_env(phase: &str) -> Env {
    let e = env(phase, 3).await;
    e.mutator.config.write().await.buckets.insert(
        "b".into(),
        crate::bucket_policy::BucketPolicyConfig {
            backend: Some("dst".into()),
            ..Default::default()
        },
    );
    e.state.maintenance_gate.sync_from(&*e.db.lock().await);
    assert!(
        e.state.maintenance_gate.is_busy("b"),
        "the job gates its bucket"
    );
    e
}

async fn assert_nothing_unwound(e: &Env, ctx: &str) {
    let engine = e.state.engine.load().clone();
    for key in ["run/k-000.txt", "run/k-002.txt"] {
        engine
            .head("t", key)
            .await
            .unwrap_or_else(|err| panic!("{ctx}: the target copy {key} is gone: {err}"));
    }
    assert!(
        e.mutator.read().await.buckets.contains_key("t"),
        "{ctx}: the route to the target was removed"
    );
    assert!(
        e.state.maintenance_gate.is_busy("b"),
        "{ctx}: the gate was released by an unwind"
    );
}

#[tokio::test]
async fn a_failure_over_a_flipped_config_unwinds_nothing() {
    let e = flipped_env("verify").await;
    // One copy is missing: verify fails for good (not a transient error).
    e.state
        .engine
        .load()
        .delete("t", "run/k-001.txt")
        .await
        .unwrap();
    let result = e.phases().await;
    assert!(
        matches!(result, Err(PhaseStop::Failed(_))),
        "the verify must fail: {result:?}"
    );
    assert_nothing_unwound(&e, "failure at verify").await;
}

#[tokio::test]
async fn a_cancel_over_a_flipped_config_unwinds_nothing() {
    let e = flipped_env("verify").await;
    e.db.lock()
        .await
        .maintenance_request_cancel(e.job.id)
        .unwrap();
    let result = e.phases().await;
    assert_eq!(result, Err(PhaseStop::Cancelled), "{result:?}");
    assert_nothing_unwound(&e, "cancel at verify").await;
}

#[tokio::test]
async fn a_save_failure_at_the_flip_over_a_flipped_config_unwinds_nothing() {
    let e = flipped_env("flip").await;
    e.db.lock().await.conn.execute_batch("BEGIN").unwrap();
    let result = e.phases().await;
    assert!(
        matches!(result, Err(PhaseStop::Deferred(_))),
        "a failed save after the flip waits: {result:?}"
    );
    assert_nothing_unwound(&e, "save failure at flip").await;
}

/// The copy of a migrate asks the DISK OF ITS TARGET BACKEND, not the spool
/// disk as a whole: with the spool disk at level high and the target's disk
/// fine, objects that need no spool file are copied (a 19 GiB host sits at
/// level high of the real thresholds, and the copy used to wait at once).
#[tokio::test]
async fn a_migrate_copy_ignores_a_full_spool_disk_when_no_object_needs_a_spool() {
    use super::super::worker::cost_tests::watch_with;
    let e = env_with("stage", 5, false).await;
    let _watch = watch_with(&e.state, 50);
    e.state.engine.load().spool_dir().gate().set(
        crate::disk_watch::Level::High,
        "the spool disk is full".into(),
    );
    let mut c = Counters::default();
    e.run().copy(&mut c).await.unwrap();
    assert_eq!(c.done, 5, "{c:?}");
}

/// A critical disk of the target backend defers the copy before its next
/// object: no copy, no failure recorded, no byte on the target.
#[tokio::test]
async fn a_migrate_copy_waits_for_the_disk_of_its_target_backend() {
    use super::super::worker::cost_tests::watch_with;
    let e = env_with("stage", 5, false).await;
    let _watch = watch_with(&e.state, 3);
    e.state
        .engine
        .load()
        .spool_dir()
        .gate()
        .set(crate::disk_watch::Level::Ok, String::new());
    let mut c = Counters::default();
    let Err(stop) = e.run().copy(&mut c).await else {
        panic!("the copy waits");
    };
    assert!(
        matches!(&stop, PhaseStop::Deferred(m) if m.contains("backend 'default'")),
        "{stop:?}"
    );
    assert_eq!((c.done, c.failed), (0, 0), "{c:?}");
    assert_eq!(e.count("PUT") + e.count("COPY"), 0, "nothing was written");
}

/// The copy waits out a write that the gate admitted before it, every time
/// it starts (a resume after a disk wait included): one that deletes a source
/// key after the verify read it would bring the key back at the flip.
#[tokio::test]
async fn a_migrate_copy_starts_only_after_the_admitted_writes_end() {
    let e = env_with("stage", 5, false).await;
    let write = e.state.maintenance_gate.begin_detached_write("b");
    let run = e.run();
    let mut c = Counters::default();
    {
        let copy = run.copy(&mut c);
        tokio::pin!(copy);
        tokio::select! {
            done = &mut copy => panic!("the copy started with a write in flight: {done:?}"),
            _ = tokio::time::sleep(std::time::Duration::from_millis(600)) => {}
        }
        drop(write);
        copy.await.unwrap();
    }
    assert_eq!(c.done, 5, "{c:?}");
}

/// A cancel does not wait for the disk. A migrate that waits for a full
/// target disk (its mark is set) and is cancelled runs its cancel at the next
/// claim: the staged copies and the staging route go, the row ends
/// `cancelled`, the mark ends. It used to be deferred again and again while
/// the disk stayed full.
#[tokio::test]
async fn a_cancelled_migrate_unwinds_while_the_target_disk_is_still_full() {
    use super::super::worker::cost_tests::watch_with;
    let e = env_with("copy", 5, true).await;
    let _watch = watch_with(&e.state, 3);
    e.state
        .engine
        .load()
        .spool_dir()
        .gate()
        .set(crate::disk_watch::Level::Ok, String::new());
    let gate = e.state.maintenance_gate.clone();
    gate.sync_from(&*e.db.lock().await);
    gate.mark_disk_deferred(e.job.id, super::super::store::current_unix_seconds() - 1);
    assert!(
        e.db.lock()
            .await
            .maintenance_request_cancel(e.job.id)
            .unwrap()
            == super::super::store::CancelOutcome::CancelRequested
    );
    let config = e.mutator.config.clone();
    super::super::worker::run_job(
        &e.mutator,
        &config,
        &e.db,
        &e.state,
        Holder::test("inst"),
        e.job.clone(),
    )
    .await;
    let row =
        e.db.lock()
            .await
            .maintenance_job_by_id(e.job.id)
            .unwrap()
            .unwrap();
    assert_eq!(row.status, "cancelled", "{row:?}");
    let engine = e.state.engine.load().clone();
    assert!(
        engine.head("t", "run/k-000.txt").await.is_err(),
        "the staged copies are removed"
    );
    assert!(!e.mutator.read().await.buckets.contains_key("t"));
    assert!(!gate.has_disk_mark(e.job.id), "the mark ends with the run");
}

/// A copy that needs a spool file and meets the spool gate defers the job; a
/// busy proxy with the gate open, and every other error, is the object's.
#[test]
fn a_copy_error_defers_the_job_only_for_the_spool_gate() {
    let why = || Some("the spool disk is full".to_string());
    let failed = |d: CopyDone| {
        matches!(
            d,
            CopyDone::Failed {
                transient: true,
                ..
            }
        )
    };
    assert!(matches!(
        copy_error_done(true, why(), true, "busy".into()),
        CopyDone::Deferred(m) if m == "the spool disk is full"
    ));
    assert!(failed(copy_error_done(true, None, true, "busy".into())));
    assert!(failed(copy_error_done(false, why(), true, "x".into())));
}
