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
    db: Arc<Mutex<ConfigDb>>,
    job: MaintenanceJob,
    mutator: ConfigMutator,
    params: MigrateParams,
    _dir: tempfile::TempDir,
}

/// Objects `k-000.txt` … in `b` (the source) and the same objects in `t`
/// (the copies this job made), and a migrate job persisted in `phase`.
async fn env(phase: &str, objects: usize) -> Env {
    let (engine, fake) = s3_engine().await;
    engine.create_bucket("t").await.unwrap();
    let params = MigrateParams {
        target_backend: "dst".into(),
        delete_source: false,
        transient_key: "t".into(),
        from_backend: "src".into(),
        target: MigrateTarget::Empty,
    };
    let db = ConfigDb::in_memory("k").unwrap();
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
    let dir = tempfile::tempdir().unwrap();
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
    assert_eq!(result, Ok(()), "{result:?}");
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
    let db = ConfigDb::in_memory("k").unwrap();
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
    m.flip(&Counters::default()).await.unwrap();
    usage.flush_pending();
    let row = usage.read("b").unwrap().unwrap();
    assert_eq!(row.last_scan_at, None, "the row still reads as scanned");
}
